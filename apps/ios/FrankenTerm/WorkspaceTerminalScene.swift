import SwiftUI
import UIKit
import UniformTypeIdentifiers
import Network
import Observation
import Darwin
import FrankenTermKit
import FrankenTermSSH
import FrankenTermSSHIntegration

/// One machine of a live workspace scene, in the group's order. `target` is
/// nil when none of its direct-SSH routes is ready on this device.
struct SceneMachine: Identifiable {
    let id: String, name: String
    let target: TerminalSessionTarget?
    let unavailable: String
    let theme: WorkspaceDocument.Theme?
    var autoConnect = false
    var tags: [String] = []
}

/// Keeps the workspace's authenticated connections and terminals, giving its
/// single keyboard to exactly the selected machine. Only the active group's
/// opted-in machines prepare in the background. Selection may open a prepared
/// on-select route; a manual route waits for Open Terminal. A lost terminal's
/// replacement waits for New Session, even after bounded reauthentication; a
/// lost attachment to existing mux panes reattaches to them on its own.
///
/// Every switch, from the machine strip, a menu, a one-finger swipe or a new
/// connection, goes through Kit's navigation coordinator: the old source's
/// composition and held keys settle behind a focus barrier before the new
/// source can accept input.
@MainActor
@Observable
final class WorkspaceTerminalScene: TerminalSessionFocus {
    /// What a machine is doing, for its chip and card.
    enum Status: Equatable { case live, ready, connecting, attention, disconnected, unavailable }

    let workspaceID: UUID, revision: UUID
    let workspaceName: String
    private(set) var machines: [SceneMachine]
    private(set) var sessions: [String: TerminalSessionController] = [:]
    /// The nil-slot primary keeps the existing group/mux scheduling contract.
    /// Extra slots are deliberate direct-SSH terminals and never auto-respawn.
    struct DirectTerminalSlot: Identifiable {
        let id: UUID
        let number: Int
        var controller: TerminalSessionController
    }
    private(set) var additionalSessions: [String: [DirectTerminalSlot]] = [:]
    private(set) var selectedTerminalSessionIDs: [String: UUID] = [:]
    private(set) var invalidatedTerminalSlots: Set<UUID> = []
    @ObservationIgnored private var additionalJobs: [UUID: Task<Void, Never>] = [:]
    @ObservationIgnored private var installedAuthenticationOwnerID: UUID?
    private(set) var orphanedTerminalRecords: [TerminalSessionRecord] = []
    private(set) var terminalHistoryFailure: String?
    private(set) var reviewingTerminalHistory = false
    @ObservationIgnored private var terminalHistoryGeneration = UUID()
    /// Each machine's theme now; a pick in the scene changes it live.
    private(set) var themes: [String: WorkspaceDocument.Theme] = [:]
    /// The machine on screen; follows committed navigation only.
    private(set) var selectedMachineID: String?
    /// Explicitly chosen local slots. Only selectedMachineID owns input; the
    /// other slot observes its existing session at the shared remote size.
    private(set) var splitLayout = TerminalSplitLayout()
    private(set) var splitAvailable = false
    private(set) var openingSplit = false
    @ObservationIgnored private var splitGeneration = UUID()
    @ObservationIgnored private var splitJob: Task<Void, Never>?
    /// The committed group's order, not the full connection-owner inventory.
    /// Storing the IDs also makes same-machine group changes observable to UI.
    private(set) var navigationMachineIDs: [String] = []
    private(set) var problem: String?
    /// Native panels share one input fence. Their dismissal joins one owned
    /// navigation operation before the terminal keyboard can be enabled again.
    enum Panel: Equatable { case fleet, hardwareKeyboard, remoteFiles }
    private(set) var remoteFiles: TerminalRemoteFilesController?
    @ObservationIgnored private var fileTransferNotices: [String: (routeID: String, message: String)] = [:]
    private(set) var presentedPanel: Panel = .fleet
    private(set) var overviewPresented = false
    @ObservationIgnored private var overviewVisit: UUID?
    @ObservationIgnored private var overviewGeneration = UUID()
    @ObservationIgnored private var overviewBuildID: UUID?
    @ObservationIgnored private var overviewCatalogID: UUID?
    @ObservationIgnored private var overviewChoice: (TerminalFleetOverview, TerminalFleetOverview.Choice)?
    @ObservationIgnored private var overviewJob: (id: UUID, task: Task<Void, Never>)?
    @ObservationIgnored private var overviewAllowsTargetPresentation = false
    private var overviewBlocksInput: Bool { overviewVisit != nil || overviewJob != nil }
    var overviewSessionID: UUID? { overviewVisit }
    /// One source of truth for imported opt-ins, explicit actions and losses.
    private var connectionIntent: TerminalConnectionIntent
    /// Cold automatically prepared connections may yield to actual demand.
    /// The connection manager remains the app-wide admission authority.
    private var residency: TerminalConnectionResidency
    private(set) var capacityBlockedMachineID: String?
    private(set) var capacityReleaseMachineID: String?
    @ObservationIgnored private var explicitPreparations: Set<String> = []
    @ObservationIgnored private var capacityRelease: TerminalConnectionResidency.Owner?
    @ObservationIgnored private var capacityReleaseJob: Task<Void, Never>?
    private(set) var failedCapacityRelease: TerminalConnectionResidency.Owner?
    /// Machines the user disconnected: navigation never revives them.
    var disconnected: Set<String> { connectionIntent.disconnected }

    @ObservationIgnored let connections: MobileConnections
    @ObservationIgnored let owner: TerminalKeyboardNavigationOwner
    @ObservationIgnored let navigation: TerminalNavigationCoordinator
    @ObservationIgnored private var bindings: [UUID: TerminalKeyboardNavigationBinding] = [:]
    @ObservationIgnored private(set) var closed = false
    /// The app is active (not behind a Face ID sheet, alert or the switcher).
    @ObservationIgnored private var foreground = true
    /// The terminal screen is showing; sessions run whether or not it is.
    @ObservationIgnored private var onScreen = false
    /// A touch begun before a lifecycle or machine transition cannot become a
    /// fresh navigation request merely because the same screen returns later.
    @ObservationIgnored private var canvasGeneration = UUID()
    @ObservationIgnored private var keyboardPresentation: (id: UUID, selection: TerminalNavigationSelection, visible: Bool)?
    /// A retained scene can reopen at another machine. Its previous selection
    /// must not create a terminal before the requested focus handoff commits.
    @ObservationIgnored private var appearanceSelection: String?
    private struct GroupAppearance {
        let id: UUID
        let machineID: String
        let terminalSessionID: UUID?
        let group: TerminalConnectionIntent.Group?
        let navigationGroup: TerminalNavigationGroup?
        let connectOnOpen: Bool
    }
    @ObservationIgnored private var appearanceGeneration = UUID()
    @ObservationIgnored private var pendingGroupAppearance: GroupAppearance?
    @ObservationIgnored private var groupAppearanceJob: (id: UUID, task: Task<Void, Never>)?
    private var groupAppearanceBlocksInput: Bool { pendingGroupAppearance != nil || groupAppearanceJob != nil }
    /// Set by enteredBackground until the next activation.
    @ObservationIgnored private var inBackground = false
    @ObservationIgnored private var closing: Task<Void, Never>?
    /// A short-lived successful shell does not replenish its retry budget.
    @ObservationIgnored private var reconnectPolicies: [String: TerminalReconnectPolicy] = [:]
    private struct ReconnectJob {
        let id: UUID, sessionID: UUID
        var ready = false, cancelled = false
        let task: Task<Void, Never>
    }
    /// One retained timer per machine. Cancelled timers stay owned until their
    /// callback joins; a ready timer stays queued behind normal attempt limits.
    @ObservationIgnored private var reconnectJobs: [String: ReconnectJob] = [:]
    /// The device's network path as last reported, and the monitor reporting it.
    @ObservationIgnored private var networkPath: TerminalNetworkPath?
    @ObservationIgnored private var pathMonitor: NWPathMonitor?
    private struct ControllerJob {
        let id: UUID, controllerID: UUID
        let task: Task<Void, Never>
    }
    /// Startup and Ready activation share one exact-controller owner. Phase
    /// callbacks cannot start a second activation while its wrapper settles.
    @ObservationIgnored private var controllerJobs: [String: ControllerJob] = [:]
    /// A user retry may replace an ended owner once its cleanup has joined;
    /// imported warm-up intent alone never restarts an authentication failure.
    @ObservationIgnored private var requestedConnections: Set<String> = []
    /// A deliberate path change carries input uncertainty, but never the old
    /// route's successful candidate, retry plan, shell or mux attachment mode.
    @ObservationIgnored private var changedRouteMachines: Set<String> = []
    /// Local choices survive reconnects in this scene. A fresh scene begins
    /// shared panes in View Only; these are not restorable server leases.
    @ObservationIgnored private var muxAccessSelections: [String: TerminalMuxAccessMode] = [:]
    @ObservationIgnored private let initialMuxAccessMode: TerminalMuxAccessMode
    @ObservationIgnored private var existingMuxRequired: Set<String> = []
    private(set) var changingMuxModes: Set<String> = []
    @ObservationIgnored private var muxModeJobs: [String: (id: UUID, task: Task<Void, Never>)] = [:]
    /// False disables group warming and automatic retries; explicit actions
    /// still use the same bounded scheduler (also useful to hosted tests).
    @ObservationIgnored private let autoConnect: Bool

    /// `watchesNetworkPath` false leaves network hints to the caller (hosted
    /// tests deliver their own).
    init(workspaceID: UUID, revision: UUID, workspaceName: String, machines: [SceneMachine],
         initialMachineID: String, connections: MobileConnections,
         initialGroup: TerminalConnectionIntent.Group? = nil, autoConnect: Bool = true,
         maximumRetainedConnections: Int = 16, watchesNetworkPath: Bool = true,
         initialMuxAccessMode: TerminalMuxAccessMode = .viewOnly) throws {
        self.workspaceID = workspaceID; self.revision = revision; self.autoConnect = autoConnect
        self.workspaceName = workspaceName; self.machines = machines; self.connections = connections
        self.initialMuxAccessMode = initialMuxAccessMode
        residency = try .init(limit: maximumRetainedConnections)
        connectionIntent = try TerminalConnectionIntent(
            machines: machines.map { .init(id: $0.id, autoConnect: $0.autoConnect) },
            selectedMachineID: initialMachineID, group: autoConnect ? initialGroup : nil)
        themes = Dictionary(uniqueKeysWithValues: machines.compactMap { machine in machine.theme.map { (machine.id, $0) } })
        owner = try TerminalKeyboardNavigationOwner(coordinator: connections.coordinator, sceneID: UUID(),
                                                    workspaceID: workspaceID.uuidString, bindings: [])
        let empty = try machines.map { try TerminalNavigationMachine(id: $0.id, name: $0.name, panes: []) }
        let initialNavigationGroup = try initialGroup.map {
            try TerminalNavigationGroup(id: $0.id, machineIDs: $0.machineIDs)
        }
        navigation = TerminalNavigationCoordinator(
            navigation: try TerminalNavigation(machines: empty, initialMachineID: initialMachineID,
                                               group: initialNavigationGroup), focusOwner: owner)
        selectedMachineID = navigation.navigation.selection?.machineID
        navigationMachineIDs = navigation.navigation.machines.map(\.id)
        navigation.onChange = { [weak self] in self?.navigationChanged() }
        if watchesNetworkPath {
            let monitor = NWPathMonitor()
            monitor.pathUpdateHandler = { [weak self] path in
                let update = TerminalNetworkPath(usable: path.status == .satisfied,
                                                 interfaces: path.availableInterfaces.map(\.name))
                MainActor.assumeIsolated { self?.networkPathChanged(update) }
            }
            monitor.start(queue: .main)
            pathMonitor = monitor
        }
    }

    var selectedMachine: SceneMachine? { machines.first { $0.id == selectedMachineID } }
    var selectedSession: TerminalSessionController? { selectedMachineID.flatMap { displayedSession($0) } }

    func displayedSession(_ machineID: String) -> TerminalSessionController? {
        session(machineID: machineID, terminalSessionID: selectedTerminalSessionIDs[machineID])
    }

    private func session(machineID: String, terminalSessionID: UUID?) -> TerminalSessionController? {
        guard let terminalSessionID else { return sessions[machineID] }
        return additionalSessions[machineID]?.first { $0.id == terminalSessionID }?.controller
    }

    func terminalControllers(_ machineID: String) -> [TerminalSessionController] {
        [sessions[machineID]].compactMap { $0 } + (additionalSessions[machineID] ?? []).map(\.controller)
    }

    private var allControllers: [TerminalSessionController] {
        machines.flatMap { terminalControllers($0.id) }
    }

    private func owns(_ controller: TerminalSessionController) -> Bool {
        session(machineID: controller.target.machineID, terminalSessionID: controller.terminalSessionID) === controller
    }

    func terminalTitle(_ controller: TerminalSessionController) -> String {
        let number = additionalSessions[controller.target.machineID]?.first { $0.controller === controller }?.number ?? 1
        return "Terminal \(number)"
    }

    private func selection(for controller: TerminalSessionController,
                           bindings values: [UUID: TerminalKeyboardNavigationBinding]? = nil) -> TerminalNavigationSelection {
        let binding = (values ?? bindings)[controller.id]
        return .init(machineID: controller.target.machineID,
            paneID: binding?.paneID ?? .init(id: controller.id, incarnation: controller.id))
    }

    private func controller(for selection: TerminalNavigationSelection) -> TerminalSessionController? {
        terminalControllers(selection.machineID).first { self.selection(for: $0) == selection }
    }

    private var selectedBinding: TerminalKeyboardNavigationBinding? {
        selectedSession.flatMap { bindings[$0.id] }
    }

    private func updateTerminalAuthenticationOwner() {
        if !closed, onScreen, let controller = selectedSession {
            installedAuthenticationOwnerID = controller.id
            connections.selectTerminalAuthenticationOwner(controller)
        } else if let previous = installedAuthenticationOwnerID {
            connections.releaseTerminalAuthenticationOwner(previous)
            installedAuthenticationOwnerID = nil
        }
    }

    var canCreateAdditionalTerminal: Bool {
        guard permitsTerminalCreation, onScreen, let machineID = selectedMachineID,
              let primary = sessions[machineID], primary.phase == .live,
              primary.additionalTerminalTarget != nil,
              !disconnected.contains(machineID), additionalJobs.isEmpty,
              !openingSplit, selectedSession?.clipboardIsPresenting != true,
              navigation.phase == .ready || navigation.phase == .inactive else { return false }
        return terminalControllers(machineID).count < WorkspaceSSHConnectionManager.maximumTerminalSessionsPerMachine
            && additionalSessions.values.reduce(0, { $0 + $1.count }) < residency.limit
            && allControllers.filter({ !$0.settled }).count < residency.limit
    }

    /// A new local slot is admitted only by this deliberate action. Its tab
    /// selection must finish the old keyboard handoff before sign-in starts.
    func newTerminal() {
        guard canCreateAdditionalTerminal, let machineID = selectedMachineID,
              var target = sessions[machineID]?.additionalTerminalTarget else { return }
        if let theme = themes[machineID] { target = target.wearing(theme) }
        let slotID = UUID()
        let controller = TerminalSessionController(target: target, connections: connections, focus: self,
            terminalSessionID: slotID)
        let number = max(1, additionalSessions[machineID]?.map(\.number).max() ?? 1) + 1
        additionalSessions[machineID, default: []].append(.init(id: slotID, number: number, controller: controller))
        observeAdditional(controller)
        startAdditional(controller)
    }

    private func observeAdditional(_ controller: TerminalSessionController) {
        controller.onChange = { [weak self, weak controller] in
            guard let self, let controller, self.owns(controller) else { return }
            self.reconcileRemoteFiles()
            // No reconnect scheduler or machine-level terminal-ended intent:
            // a sibling ending cannot replace another tab's shell.
            self.reconcile()
        }
    }

    private func startAdditional(_ controller: TerminalSessionController) {
        guard let slotID = controller.terminalSessionID, additionalJobs[slotID] == nil else { return }
        let generation = appearanceGeneration
        additionalJobs[slotID] = Task { [weak self, controller] in
            guard let self else { return }
            defer { self.additionalJobs[slotID] = nil; self.reconcile() }
            do {
                try await self.whileBusy {
                    guard self.owns(controller), self.permitsTerminalCreation, self.onScreen,
                          self.appearanceGeneration == generation else { throw CancellationError() }
                    try self.publishBindings(self.bindings)
                    guard let paneID = self.selection(for: controller).paneID,
                          let plan = try self.navigation.navigation.plan(paneID: paneID)
                            ?? self.navigation.navigation.activationPlan() else { throw TerminalNavigationError.unavailable }
                    try await self.navigation.perform(plan)
                }
                guard !Task.isCancelled, self.owns(controller), self.selectedSession === controller,
                      self.permitsTerminalCreation, self.appearanceGeneration == generation else { throw CancellationError() }
                await controller.activateTerminal(explicit: true)
            } catch {
                controller.requestStop()
                await controller.stop()
                if !Task.isCancelled, !self.closed { self.report(error) }
            }
        }
    }

    func selectTerminal(_ controller: TerminalSessionController) {
        guard owns(controller), permitsCanvasNavigation, !openingSplit else { return }
        let generation = canvasGeneration, expected = controller.id
        Task { [weak self, weak controller] in
            guard let self, let controller else { return }
            do {
                try await self.whileBusy {
                    guard self.permitsCanvasNavigation, self.canvasGeneration == generation,
                          self.owns(controller), controller.id == expected else { throw TerminalNavigationError.stale }
                    guard let paneID = self.selection(for: controller).paneID,
                          let plan = try self.navigation.navigation.plan(paneID: paneID)
                            ?? self.navigation.navigation.activationPlan() else { return }
                    try await self.navigation.perform(plan)
                }
            } catch { if !Task.isCancelled { self.report(error) } }
        }
    }

    /// Replaces only the selected logical slot. Its own journal continuation
    /// and consent are preserved; sibling slots never participate.
    func restartTerminal(_ controller: TerminalSessionController) {
        guard permitsTerminalCreation, onScreen, owns(controller), selectedSession === controller,
              controller.settled, let slotID = controller.terminalSessionID,
              !invalidatedTerminalSlots.contains(slotID),
              additionalJobs[slotID] == nil,
              allControllers.filter({ !$0.settled }).count < residency.limit,
              let index = additionalSessions[controller.target.machineID]?.firstIndex(where: { $0.id == slotID }) else { return }
        var target = controller.target
        do { try connectionIntent.approveConnection(target.machineID) }
        catch { report(error); return }
        if let theme = themes[target.machineID] { target = target.wearing(theme) }
        let replacement = TerminalSessionController(target: target, connections: connections, focus: self,
            inputDelivery: controller.inputDelivery, terminalSessionID: slotID)
        controller.discardRetainedOutput(); controller.discardHostKeyMismatch()
        additionalSessions[target.machineID]?[index].controller = replacement
        observeAdditional(replacement)
        startAdditional(replacement)
    }

    func endTerminal(_ controller: TerminalSessionController) {
        guard !closed, owns(controller) else { return }
        if let slotID = controller.terminalSessionID { additionalJobs[slotID]?.cancel() }
        else {
            controllerJobs[controller.target.machineID]?.task.cancel()
            requestedConnections.remove(controller.target.machineID)
            cancelReconnect(controller.target.machineID)
        }
        controller.requestStop()
        Task { await controller.stop() }
    }

    func removeTerminal(_ controller: TerminalSessionController) {
        guard permitsTerminalCreation, onScreen, owns(controller), controller.settled,
              let slotID = controller.terminalSessionID, additionalJobs[slotID] == nil,
              navigation.phase == .ready || navigation.phase == .inactive else { return }
        let machineID = controller.target.machineID
        let wasSelected = displayedSession(machineID) === controller
        controller.discardRetainedOutput(); controller.discardHostKeyMismatch()
        additionalSessions[machineID]?.removeAll { $0.id == slotID }
        invalidatedTerminalSlots.remove(slotID)
        if selectedTerminalSessionIDs[machineID] == slotID { selectedTerminalSessionIDs[machineID] = nil }
        do { try publishBindings(bindings) } catch { report(error) }
        updateTerminalAuthenticationOwner()
        updatePresentationVisibility()
        if wasSelected, let primary = sessions[machineID], selectedMachineID == machineID,
           bindings[primary.id] != nil || additionalSessions[machineID]?.isEmpty == false {
            selectTerminal(primary)
        }
        refreshTerminalHistory()
    }

    /// Orphaned tabs survive only as bounded durable metadata. Loading them
    /// cannot reconnect, execute input, or grant permission to a new shell.
    func refreshTerminalHistory() {
        let generation = UUID(); terminalHistoryGeneration = generation
        Task { [weak self] in
            guard let self else { return }
            do {
                let records = try await self.connections.journal.records()
                guard !self.closed, self.terminalHistoryGeneration == generation else { return }
                self.orphanedTerminalRecords = self.orphanedRecords(in: records)
            } catch {
                guard !self.closed, self.terminalHistoryGeneration == generation else { return }
                self.terminalHistoryFailure = "Earlier terminal history could not be read: \(TerminalSessionController.describe(error))"
            }
        }
    }

    private func orphanedRecords(in records: [TerminalSessionRecord]) -> [TerminalSessionRecord] {
        records.filter { record in
            guard record.scope.workspaceID == workspaceID, let slotID = record.scope.terminalSessionID,
                  record.canResolve, record.hasRecoveryNotice || record.blocksNewSession else { return false }
            return additionalSessions[record.scope.machineID]?.contains(where: { $0.id == slotID }) != true
        }
    }

    func earlierTerminals(_ machineID: String) -> [TerminalSessionRecord] {
        orphanedTerminalRecords.filter { $0.scope.machineID == machineID }
    }

    /// The button supplies the exact snapshots it displayed. A later record
    /// stays visible; ambiguous writes require a successful explicit retry.
    func acknowledgeEarlierTerminals(_ reviewed: [TerminalSessionRecord], machineID: String) {
        guard permitsTerminalCreation, onScreen, selectedMachineID == machineID, !reviewingTerminalHistory,
              reviewed.allSatisfy({ $0.scope.machineID == machineID && orphanedTerminalRecords.contains($0) }) else { return }
        reviewingTerminalHistory = true
        terminalHistoryGeneration = UUID()
        Task { [weak self] in
            guard let self else { return }
            defer { self.reviewingTerminalHistory = false }
            do {
                try await self.connections.journal.acknowledgeRecovery(reviewed.filter(\.hasRecoveryNotice))
                for record in reviewed where !record.hasRecoveryNotice { try await self.connections.journal.resolve(record) }
                let current = try await self.connections.journal.records()
                self.clearReviewedTerminalHints(reviewed, current: current)
                self.orphanedTerminalRecords = self.orphanedRecords(in: current)
                self.terminalHistoryFailure = nil
            } catch {
                if let current = try? await self.connections.journal.records() {
                    self.clearReviewedTerminalHints(reviewed, current: current)
                    self.orphanedTerminalRecords = self.orphanedRecords(in: current)
                }
                self.terminalHistoryFailure = "Earlier terminal history could not be acknowledged: \(TerminalSessionController.describe(error)) Review again to retry."
            }
        }
    }

    /// Crash hints are non-authoritative and may outlive a removed slot. Only
    /// exact user-reviewed, now-resolved slots can consume an earlier launch's
    /// hint; a fresh record or current controller always keeps it. Readback
    /// after an ambiguous write may clear this hint but never the failure UI.
    private func clearReviewedTerminalHints(_ reviewed: [TerminalSessionRecord], current: [TerminalSessionRecord]) {
        for record in reviewed {
            guard record.scope.workspaceID == workspaceID, let slotID = record.scope.terminalSessionID,
                  session(machineID: record.scope.machineID, terminalSessionID: slotID) == nil,
                  !current.contains(where: {
                      $0.belongsTo(workspaceID: workspaceID, machineID: record.scope.machineID, terminalSessionID: slotID)
                          && ($0.blocksNewSession || $0.hasRecoveryNotice)
                  }) else { continue }
            _ = TerminalUnansweredInput(workspace: workspaceID.uuidString, machine: record.scope.machineID,
                terminalSessionID: slotID).takeFromEarlierLaunch()
        }
    }
    /// Review and copy actions must still belong to the visible machine's
    /// exact failed controller and workspace revision. A captured sheet is
    /// not authority after navigation or replacement, and cannot expose or
    /// copy evidence while the app is inactive.
    func hostKeyMismatch(for controller: TerminalSessionController) -> WorkspaceSSHConnectionHostKeyMismatch? {
        guard !closed, foreground, !inBackground, onScreen,
              selectedMachineID == controller.target.machineID,
              selectedSession === controller,
              controller.target.workspaceID == workspaceID, controller.target.revision == revision,
              case .couldNotConnect = controller.phase,
              let mismatch = controller.hostKeyMismatch,
              mismatch.workspaceID == workspaceID, mismatch.expectedRevision == revision,
              mismatch.machineID == controller.target.machineID,
              mismatch.destinationRouteID == controller.route.id else { return nil }
        return mismatch
    }

    @discardableResult
    func copyHostKeyFingerprint(_ key: WorkspaceDocument.HostKey, from controller: TerminalSessionController,
                               expected mismatch: WorkspaceSSHConnectionHostKeyMismatch) -> Bool {
        let certificateIdentity = mismatch.hostCertificate.map { certificate in
            [certificate.subject, certificate.authority].contains {
                $0.algorithm == key.algorithm && $0.fingerprint == key.fingerprint
            }
        } ?? false
        guard hostKeyMismatch(for: controller) == mismatch,
              key == mismatch.presentedKey || mismatch.expectedKeys.contains(key)
                || mismatch.expectedCertificateAuthorities.contains(key) || certificateIdentity else { return false }
        UIPasteboard.general.setItems([[UTType.utf8PlainText.identifier: key.fingerprint]],
            options: [.localOnly: true, .expirationDate: Date().addingTimeInterval(120)])
        return true
    }

    var displayedMachineIDs: [String] {
        splitLayout.visibleMachineIDs(selected: selectedMachineID, expanded: splitAvailable)
    }
    var splitExpanded: Bool { displayedMachineIDs.count == 2 }
    var navigationMachines: [SceneMachine] {
        let known = Dictionary(uniqueKeysWithValues: machines.map { ($0.id, $0) })
        return navigationMachineIDs.compactMap { known[$0] }
    }

    /// Group warm-up follows the phone's saved choice, including in a scene
    /// kept alive behind its workspace page. Existing owners and deliberate
    /// Connect actions keep their authority; this setting never disconnects.
    func setAutomaticConnection(_ enabled: Bool, machineID: String) {
        setAutomaticConnections([machineID: enabled])
    }

    func setAutomaticConnections(_ values: [String: Bool]) {
        guard !closed else { return }
        do { try connectionIntent.setAutomaticConnections(values) }
        catch { report(error); return }
        reconcile()
    }

    // MARK: Fleet overview

    func beginRemoteFiles() {
        guard remoteFiles == nil, let session = selectedSession, session.remoteFilesTarget != nil else { return }
        beginOverview(panel: .remoteFiles)
        guard overviewPresented, presentedPanel == .remoteFiles else { return }
        do {
            let files = try session.makeRemoteFiles(scene: self)
            remoteFiles = files
            files.start()
        } catch { report(error); dismissOverview() }
    }

    func remoteFilesAreCurrent(_ files: TerminalRemoteFilesController, requireForeground: Bool = true) -> Bool {
        !closed && onScreen && !inBackground && (!requireForeground || foreground)
            && overviewPresented && presentedPanel == .remoteFiles && remoteFiles === files
            && selectedMachineID == files.target.machineID
            && workspaceID == files.target.workspaceID && revision == files.target.revision
            && selectedSession === files.source
            && files.source.remoteFilesTarget == files.target
    }

    func reconnectRemoteFiles(_ previous: TerminalRemoteFilesController) {
        guard remoteFilesAreCurrent(previous), previous.settled else { return }
        do {
            let replacement = try previous.source.makeRemoteFiles(scene: self)
            remoteFiles = replacement
            replacement.start()
        } catch { report(error) }
    }

    func fileTransferNotice(for target: TerminalRemoteFilesTarget) -> String? {
        let notice = fileTransferNotices[target.machineID]
        return notice?.routeID == target.route.id ? notice?.message : nil
    }

    func recordFileTransferNotice(_ message: String?, for target: TerminalRemoteFilesTarget) {
        guard target.workspaceID == workspaceID, target.revision == revision else { return }
        fileTransferNotices[target.machineID] = message.map { (target.route.id, $0) }
    }

    private func reconcileRemoteFiles() {
        if let files = remoteFiles, !remoteFilesAreCurrent(files, requireForeground: false) {
            files.retire()
            dismissOverview()
        }
    }

    func beginOverview(panel: Panel = .fleet) {
        guard !closed, foreground, onScreen, !overviewBlocksInput, !groupAppearanceBlocksInput, !openingSplit,
              navigation.phase == .ready || navigation.phase == .inactive,
              selectedSession?.clipboardIsPresenting != true,
              selectedSession?.paneNavigationBusy != true else { return }
        // A deliberate picker must not discard an unfinished IME candidate or
        // an unadmitted local key. Previously admitted bytes keep their owner.
        if let keyboard = owner.keyboard,
           keyboard.document.isMarked || keyboard.accounting.pendingEvents > 0 || keyboard.phase == .blocked {
            problem = "Finish composing or let pending local input settle before opening this panel."
            return
        }
        presentedPanel = panel
        overviewGeneration = UUID(); overviewVisit = overviewGeneration
        overviewChoice = nil; overviewCatalogID = nil; overviewPresented = true
        canvasGeneration = UUID()
        navigation.suspend()
        updatePresentationVisibility()
    }

    /// Reads already-discovered metadata only. Refresh never signs in to a
    /// machine or asks a server for a new inventory. Indexing leaves MainActor.
    func makeOverview() async throws -> TerminalFleetOverview {
        guard overviewPresented, let generation = overviewVisit,
              foreground, onScreen, !closed else { throw CancellationError() }
        let buildID = UUID()
        overviewBuildID = buildID; overviewCatalogID = nil
        var inventory: [TerminalFleetOverview.Machine] = []
        var paneCount = 0
        guard machines.count <= TerminalFleetOverview.maximumMachines else { throw TerminalFleetOverview.Failure.capacity }
        for machine in navigationMachines {
            let session = displayedSession(machine.id)
            let status: TerminalFleetOverview.Status
            switch self.status(of: machine.id) {
            case .live: status = .live
            case .ready: status = .ready
            case .connecting: status = .connecting
            case .attention: status = .attention
            case .disconnected: status = .disconnected
            case .unavailable: status = .unavailable
            }
            let route = session?.route ?? machine.target?.routes.first
            let endpoint = route.map(WorkspaceTerminalScreen.via) ?? ""
            var panes: [TerminalFleetOverview.Pane] = []
            var shown: Int?
            if session?.phase == .live, let control = session?.muxControl {
                let state = control.current
                guard state.panes.count <= TerminalFleetOverview.maximumPanes - paneCount else {
                    throw TerminalFleetOverview.Failure.capacity
                }
                let onScreen = machine.id == selectedMachineID ? state.shown : nil
                panes = try state.panes.map { pane in
                    .init(plan: try control.inputRouting.navigation(pane: pane.paneId), title: pane.title,
                          tabTitle: pane.tabTitle, workingDirectory: pane.workingDir ?? "",
                          newOutput: pane.paneId != onScreen && state.unread.contains(pane.paneId))
                }
                shown = state.shown; paneCount += panes.count
            }
            inventory.append(.init(id: machine.id, name: machine.name, endpoint: endpoint, status: status,
                sessionID: session?.id, panes: panes, selectedPaneID: shown,
                newOutput: hasUnseenOutput(machine.id), tags: machine.tags, attention: attentionReasons(machine.id)))
        }
        let selected = selectedMachineID
        let frozenInventory = inventory
        let catalog = try await withThrowingTaskGroup(of: TerminalFleetOverview.self) { group in
            group.addTask { try TerminalFleetOverview(machines: frozenInventory, selectedMachineID: selected) }
            guard let catalog = try await group.next() else { throw CancellationError() }
            return catalog
        }
        try Task.checkCancellation()
        guard overviewPresented, overviewVisit == generation, overviewBuildID == buildID,
              foreground, onScreen, !closed else { throw CancellationError() }
        overviewCatalogID = catalog.id
        return catalog
    }

    /// Selection first records a frozen candidate. Actual navigation waits for
    /// the sheet's dismissal; a delayed row callback cannot replace the choice.
    func chooseOverview(_ choice: TerminalFleetOverview.Choice, in catalog: TerminalFleetOverview) throws {
        guard overviewPresented, overviewVisit != nil, overviewCatalogID == catalog.id,
              foreground, onScreen, !closed else { throw TerminalFleetOverview.Failure.stale }
        try validateOverviewChoice(choice, in: catalog)
        overviewChoice = (catalog, choice)
        overviewPresented = false
    }

    func dismissOverview() {
        guard overviewVisit != nil else { return }
        remoteFiles?.retire()
        overviewChoice = nil; overviewPresented = false
    }

    private func validateOverviewChoice(_ choice: TerminalFleetOverview.Choice,
                                        in catalog: TerminalFleetOverview) throws {
        switch try catalog.resolve(choice) {
        case .machine(let machineID):
            guard navigationMachineIDs.contains(machineID) else { throw TerminalFleetOverview.Failure.stale }
        case let .pane(machineID, sessionID, _):
            guard navigationMachineIDs.contains(machineID),
                  let session = sessions[machineID], session.id == sessionID, session.phase == .live,
                  let control = session.muxControl else { throw TerminalFleetOverview.Failure.stale }
            _ = try catalog.validatePane(choice, sessionID: sessionID, routing: control.inputRouting)
        }
    }

    /// One retained operation owns dismissal, machine focus, and an optional
    /// pane handoff. Backgrounding/closing invalidates it and never replays it.
    func finishOverviewDismissal(_ expectedVisit: UUID) {
        guard let generation = overviewVisit, generation == expectedVisit, overviewJob == nil else { return }
        let files = remoteFiles
        files?.retire(); remoteFiles = nil
        let choice = overviewChoice
        overviewVisit = nil; overviewChoice = nil; overviewCatalogID = nil; overviewBuildID = nil
        let id = UUID()
        let task = Task { [weak self] in
            guard let self else { return }
            defer {
                if self.overviewJob?.id == id {
                    self.overviewJob = nil; self.overviewAllowsTargetPresentation = false
                    if !self.closed, self.foreground, self.onScreen { self.navigation.resume() }
                    self.startGroupAppearance()
                    self.updatePresentationVisibility()
                    self.reconcile()
                }
            }
            do {
                await files?.retryCleanup()
                try self.checkOverviewNavigation(generation)
                await self.navigation.joinSuspension()
                try self.checkOverviewNavigation(generation)
                self.navigation.resume()
                if let (catalog, selection) = choice {
                    try self.validateOverviewChoice(selection, in: catalog)
                    let target = try catalog.resolve(selection)
                    let machineID: String
                    switch target {
                    case .machine(let id): machineID = id
                    case .pane(let id, _, _): machineID = id
                    }
                    if let plan = try self.machinePlan(machineID) {
                        try await self.navigation.perform(plan)
                    }
                    try self.checkOverviewNavigation(generation)
                    try self.validateOverviewChoice(selection, in: catalog)
                    guard self.selectedMachineID == machineID else { throw TerminalFleetOverview.Failure.stale }
                    if case let .pane(_, sessionID, plan) = target {
                        // A machine handoff may have prepared its old pane's
                        // keyboard. It was never installed behind the picker;
                        // retire it before selecting the requested pane.
                        self.navigation.suspend()
                        await self.navigation.joinSuspension()
                        try self.checkOverviewNavigation(generation)
                        try self.validateOverviewChoice(selection, in: catalog)
                        guard let session = self.sessions[machineID], session.id == sessionID else {
                            throw TerminalFleetOverview.Failure.stale
                        }
                        await session.waitForOverviewPresentation()
                        try self.checkOverviewNavigation(generation)
                        try self.validateOverviewChoice(selection, in: catalog)
                        self.navigation.resume()
                        self.overviewAllowsTargetPresentation = true
                        self.updatePresentationVisibility()
                        try await session.navigatePane(plan)
                        try self.checkOverviewNavigation(generation)
                    }
                }
                // Cancellation of the sheet is also a fresh responder visit.
                // Install happens only after this owned task clears its fence.
                if self.navigation.phase == .inactive, self.selectedSession?.terminal != nil,
                   self.selectedSession?.paneNavigationBusy != true {
                    try await self.navigation.activateSelection()
                }
            } catch {
                if !Task.isCancelled, self.overviewGeneration == generation, !self.closed {
                    self.report(error)
                }
            }
        }
        overviewJob = (id, task)
    }

    private func checkOverviewNavigation(_ generation: UUID) throws {
        try Task.checkCancellation()
        guard overviewGeneration == generation, !closed, foreground, onScreen else { throw CancellationError() }
    }

    private func cancelOverview() {
        remoteFiles?.retire()
        overviewGeneration = UUID(); overviewPresented = false
        // Keep the closing sheet's slot until its exact dismissal arrives.
        // Otherwise an old onDismiss could consume a newly opened picker.
        if closed { overviewVisit = nil }
        overviewChoice = nil; overviewCatalogID = nil; overviewBuildID = nil
        overviewJob?.task.cancel()
    }

    func status(of machineID: String) -> Status {
        guard machines.first(where: { $0.id == machineID })?.target != nil else { return .unavailable }
        // Switching how the phone uses a shared pane reconnects it.
        if changingMuxModes.contains(machineID) { return .connecting }
        if disconnected.contains(machineID) { return .disconnected }
        guard let session = displayedSession(machineID) else {
            return connectionIntent.shouldPrepare(machineID) ? .connecting : .disconnected
        }
        if residency.wasReleased(machineID: machineID, ownerID: session.id) {
            if !session.settled { return session.cleanupFailure == nil ? .connecting : .attention }
            return connectionIntent.shouldPrepare(machineID) && permitsPreparation(machineID) ? .connecting : .disconnected
        }
        switch session.phase {
        case .idle, .connecting: return session.settled && session.terminalSessionID != nil ? .attention : .connecting
        case .ready: return .ready
        case .live: return .live
        case .ended, .failed, .couldNotConnect:
            return session.terminalSessionID == nil && autoConnect && session.dropped && connectionIntent.permitsReconnect(machineID)
                && reconnectPolicies[machineID, default: .init()].nextDelay != nil
                ? .connecting : .attention
        }
    }

    /// A live machine off screen with output the phone has not shown since.
    func hasUnseenOutput(_ machineID: String) -> Bool {
        terminalControllers(machineID).contains {
            $0.phase == .live && $0.hasUnseenOutput && (machineID != selectedMachineID || selectedSession !== $0)
        }
    }

    /// Input notices remain meaningful after reconnect, independently of the
    /// machine's current connected state. An intentional Disconnect is not a
    /// failure. Unavailable routes still need their key or trust requirement
    /// resolved before the machine can connect.
    func attentionReasons(_ machineID: String) -> Set<TerminalAttentionReason> {
        var reasons = TerminalAttentionReason.observed(input: .init(),
            keyboardNeedsDecision: false,
            connectionNeedsAttention: status(of: machineID) == .attention || status(of: machineID) == .unavailable
                || !earlierTerminals(machineID).isEmpty,
            newOutput: hasUnseenOutput(machineID))
        for controller in terminalControllers(machineID) {
            reasons.formUnion(TerminalAttentionReason.observed(input: controller.inputDelivery,
                keyboardNeedsDecision: controller.localInputNeedsDecision,
                connectionNeedsAttention: false, newOutput: false))
        }
        return reasons
    }

    func requiresNewSession(_ machineID: String) -> Bool { connectionIntent.requiresNewSession.contains(machineID) }

    // MARK: Screen lifecycle

    /// Opening a group's scene scopes new warm-up work. A deliberate Connect
    /// link approves its selected machine, without upgrading a manual route.
    func appear(selecting machineID: String, active: Bool,
                group: TerminalConnectionIntent.Group? = nil, connectOnOpen: Bool = false) {
        guard !closed else { return }
        if let keyboard = owner.keyboard,
           keyboard.document.isMarked || keyboard.accounting.pendingEvents > 0 || keyboard.phase == .blocked {
            problem = "Finish composing or let pending local input settle before changing groups."
            return
        }
        let navigationGroup: TerminalNavigationGroup?
        do {
            navigationGroup = try group.map { try .init(id: $0.id, machineIDs: $0.machineIDs) }
            _ = try navigation.navigation.plan(group: navigationGroup, selecting: machineID)
            // Validate networking intent without publishing it before navigation.
            var checked = connectionIntent
            try checked.setGroup(autoConnect ? group : nil)
        } catch { report(error); return }
        cancelOverview()
        cancelGroupAppearance()
        canvasGeneration = UUID()
        appearanceSelection = machineID
        onScreen = true
        setForeground(active)
        let id = UUID(); appearanceGeneration = id
        pendingGroupAppearance = GroupAppearance(id: id, machineID: machineID,
                                                 terminalSessionID: selectedTerminalSessionIDs[machineID], group: group,
                                                 navigationGroup: navigationGroup, connectOnOpen: connectOnOpen)
        startGroupAppearance()
        updatePresentationVisibility()
        refreshTerminalHistory()
    }

    /// One retained worker and one latest request, even when navigation is busy.
    /// Reappearing cancels and joins the old handoff before starting its successor.
    /// Group membership never starts or closes a connection by itself.
    private func startGroupAppearance() {
        guard groupAppearanceJob == nil, let request = pendingGroupAppearance,
              !closed, foreground, onScreen, !overviewBlocksInput else { return }
        pendingGroupAppearance = nil
        let id = UUID()
        let task = Task { [weak self] in
            guard let self else { return }
            let previousGroup = self.navigation.navigation.group
            var completed = false
            defer {
                // Cancellation may follow the core's synchronous commit. Keep
                // imported warming intent consistent with the actual ring, but
                // never manufacture an explicit Connect approval on that path.
                if self.navigation.navigation.group == request.navigationGroup,
                   completed || previousGroup != request.navigationGroup {
                    do { try self.connectionIntent.setGroup(self.autoConnect ? request.group : nil) }
                    catch { self.report(error) }
                }
                if self.groupAppearanceJob?.id == id {
                    self.groupAppearanceJob = nil
                    if self.pendingGroupAppearance == nil { self.appearanceSelection = nil }
                    self.startGroupAppearance()
                    self.navigationChanged()
                }
            }
            do {
                await self.navigation.joinSuspension()
                try self.checkGroupAppearance(request)
                self.navigation.resume()
                try await self.whileBusy {
                    try self.checkGroupAppearance(request)
                    if let plan = try self.navigation.navigation.plan(group: request.navigationGroup,
                                                                     selecting: request.machineID)
                        ?? self.navigation.navigation.activationPlan() {
                        try await self.navigation.perform(plan)
                    }
                }
                try self.checkGroupAppearance(request)
                if let desired = self.session(machineID: request.machineID, terminalSessionID: request.terminalSessionID),
                   let paneID = self.selection(for: desired).paneID,
                   self.navigation.navigation.machines.first(where: { $0.id == request.machineID })?.panes.contains(where: { $0.id == paneID }) == true,
                   let plan = try self.navigation.navigation.plan(paneID: paneID) {
                    try await self.navigation.perform(plan)
                    try self.checkGroupAppearance(request)
                }
                guard self.navigation.navigation.group == request.navigationGroup,
                      self.selectedMachineID == request.machineID else { throw TerminalNavigationError.stale }
                if request.connectOnOpen {
                    try self.connectionIntent.approveConnection(request.machineID)
                    self.explicitPreparations.insert(request.machineID)
                    if let session = self.sessions[request.machineID] {
                        switch session.phase {
                        case .ended, .failed, .couldNotConnect: self.requestedConnections.insert(request.machineID)
                        case .idle, .connecting, .ready, .live: break
                        }
                    } else { self.requestedConnections.insert(request.machineID) }
                    self.cancelReconnect(request.machineID)
                    self.reconnectPolicies[request.machineID, default: .init()].resetForUserRequest()
                }
                completed = true
            } catch {
                if !Task.isCancelled, self.appearanceGeneration == request.id, !self.closed { self.report(error) }
            }
        }
        groupAppearanceJob = (id, task)
    }

    private func checkGroupAppearance(_ request: GroupAppearance) throws {
        try Task.checkCancellation()
        guard appearanceGeneration == request.id, !closed, foreground, onScreen,
              !overviewBlocksInput else { throw CancellationError() }
    }

    private func cancelGroupAppearance() {
        appearanceGeneration = UUID(); pendingGroupAppearance = nil
        groupAppearanceJob?.task.cancel()
        if groupAppearanceJob == nil { appearanceSelection = nil }
    }

    /// Leaving the terminal screen keeps every session running; only the
    /// keyboard and rendering stop until it returns.
    func disappear() {
        guard !closed else { return }
        for controller in allControllers { controller.discardHostKeyMismatch() }
        splitGeneration = UUID(); splitJob?.cancel()
        cancelOverview()
        cancelGroupAppearance()
        onScreen = false
        updateTerminalAuthenticationOwner()
        canvasGeneration = UUID()
        appearanceSelection = nil
        navigation.suspend()
        updatePresentationVisibility()
    }

    func setForeground(_ active: Bool) {
        guard !closed else { return }
        if !active {
            // Keychain authentication can make the app briefly inactive.
            // Hide identity reviews and deny copying, but keep their evidence
            // and an admitted handshake's capture generation until background.
            if presentedPanel != .remoteFiles || remoteFiles == nil || !overviewPresented { cancelOverview() }
            cancelGroupAppearance()
            splitGeneration = UUID(); splitJob?.cancel()
        }
        if foreground != active { canvasGeneration = UUID() }
        foreground = active
        remoteFiles?.foregroundChanged(active)
        if active {
            if inBackground {
                for (id, session) in sessions where session.phase == .live {
                    reconnectPolicies[id, default: .init()].recordLive()
                }
            }
            inBackground = false
        }
        if active && onScreen && !overviewBlocksInput { navigation.resume() } else { navigation.suspend() }
        updatePresentationVisibility()
        guard active else { return }
        startGroupAppearance()
        reconcile()
        // Back from turning Tailscale on: try the machine that needed it, once
        // the app's own resume has let connections through again.
        if let id = selectedMachineID, let controllerID = sessions[id]?.id,
           needsTailscale(id), WorkspaceRouteNetwork.deviceTailnetAddress() != nil {
            let manager = connections.manager
            Task { [weak self] in
                for _ in 0..<20 where await !manager.permitsConnections { try? await Task.sleep(for: .milliseconds(100)) }
                guard let self, !self.closed, self.foreground, self.sessions[id]?.id == controllerID,
                      self.needsTailscale(id) else { return }
                self.connect(id)
            }
        }
    }

    /// Sessions keep running in the background for as long as iOS lets the
    /// app run; a connection that drops meanwhile reconnects on return.
    func enteredBackground() {
        guard !closed else { return }
        cancelOverview()
        for controller in allControllers { controller.discardHostKeyMismatch() }
        inBackground = true
        cancelGroupAppearance()
        // iOS may stop executing immediately after its grace period. Time we
        // cannot observe a session must not count as a stable live interval.
        for id in Array(reconnectPolicies.keys) { reconnectPolicies[id]?.recordEnd() }
        cancelReconnects()
        for controller in allControllers { controller.suspendPendingTerminalCreation() }
    }

    /// A hint from the device's network path, never proof that a transport
    /// survived or died. When the network comes back or changes interfaces,
    /// each machine that may reconnect gets a new retry budget for its current
    /// or next drop (every retry still verifies the host and signs in again),
    /// and each live one probes its server, so a connection stranded on the
    /// old route is found within one liveness interval.
    func networkPathChanged(_ path: TerminalNetworkPath) {
        let previous = networkPath
        networkPath = path
        guard !closed, path.isNewRoute(after: previous) else { return }
        // Dropped, still retrying, or live and about to find out.
        for id in sessions.keys where connectionIntent.permitsReconnect(id) {
            reconnectPolicies[id, default: .init()].networkChanged()
        }
        for session in allControllers { session.networkRouteChanged() }
        reconcile()
    }

    private func needsTailscale(_ machineID: String) -> Bool {
        guard !disconnected.contains(machineID),
              case .couldNotConnect(let help) = sessions[machineID]?.phase else { return false }
        return help.remedy == .turnOnTailscale
    }

    // MARK: Connections

    /// Switches the screen to a machine, through the same focus barrier as
    /// every other navigation.
    func select(_ machineID: String) {
        guard !closed, !overviewBlocksInput, !groupAppearanceBlocksInput,
              navigationMachineIDs.contains(machineID), machineID != selectedMachineID else { return }
        let appearance = appearanceGeneration
        Task {
            do {
                try await whileBusy {
                    guard self.appearanceGeneration == appearance, !self.groupAppearanceBlocksInput,
                          self.navigationMachineIDs.contains(machineID) else { throw TerminalNavigationError.stale }
                    guard let plan = try self.machinePlan(machineID) else { return }
                    try await self.navigation.perform(plan)
                }
            } catch { report(error) }
        }
    }

    /// The second machine is selected through the normal composition/input
    /// barrier before the pair becomes visible. Selection retains the route's
    /// normal creation policy; a manual or lost shell still needs Open Terminal.
    func split(with machineID: String) {
        guard splitAvailable, splitJob == nil, permitsTerminalCreation, onScreen,
              !overviewBlocksInput, !groupAppearanceBlocksInput,
              let first = selectedMachineID, first != machineID,
              let original = selectedSession, original.phase == .live, original.terminal != nil,
              !original.clipboardIsPresenting, !original.localInputNeedsDecision, !original.paneNavigationBusy,
              navigationMachineIDs.contains(machineID) else { return }
        let generation = UUID(), appearance = appearanceGeneration
        splitGeneration = generation; openingSplit = true
        splitJob = Task { [weak self] in
            guard let self else { return }
            defer { self.splitJob = nil; self.openingSplit = false }
            do {
                try await self.whileBusy {
                    try Task.checkCancellation()
                    guard self.splitGeneration == generation, self.appearanceGeneration == appearance,
                          !self.closed, self.onScreen, self.foreground, self.splitAvailable,
                          self.selectedMachineID == first, self.displayedSession(first) === original,
                          !self.overviewBlocksInput, !self.groupAppearanceBlocksInput,
                          self.navigationMachineIDs.contains(machineID) else { throw CancellationError() }
                    guard let plan = try self.machinePlan(machineID) else {
                        throw TerminalNavigationError.unavailable
                    }
                    try await self.navigation.perform(plan)
                }
                try Task.checkCancellation()
                guard self.splitGeneration == generation, !self.closed, self.onScreen,
                      self.selectedMachineID == machineID, self.displayedSession(first) === original else {
                    throw CancellationError()
                }
                try self.splitLayout.open(first: first, second: machineID, available: self.navigationMachineIDs)
                self.canvasGeneration = UUID()
                self.updatePresentationVisibility()
            } catch {
                if !(error is CancellationError), self.splitGeneration == generation { self.report(error) }
            }
        }
    }

    func closeSplit() {
        splitGeneration = UUID(); splitJob?.cancel()
        splitLayout.close(); canvasGeneration = UUID()
        updatePresentationVisibility()
    }

    /// Window geometry only changes local residency, never connection intent.
    func setSplitViewport(width: Double, isPad: Bool) {
        let allowed = TerminalSplitLayout.supports(width: width, isPad: isPad)
        guard splitAvailable != allowed else { return }
        splitAvailable = allowed; canvasGeneration = UUID()
        updatePresentationVisibility()
    }

    /// The strip's next or previous machine, wrapping at either end, as a
    /// swipe across the strip does. Hardware-keyboard shortcuts use it.
    func stepMachine(forward: Bool) {
        guard !closed, !overviewBlocksInput, !groupAppearanceBlocksInput else { return }
        let appearance = appearanceGeneration
        Task {
            do {
                try await whileBusy {
                    guard self.appearanceGeneration == appearance, !self.groupAppearanceBlocksInput else {
                        throw TerminalNavigationError.stale
                    }
                    guard let adjacent = try self.navigation.navigation.plan(axis: .machine,
                                                                         direction: forward ? .next : .previous),
                          let plan = try self.machinePlan(adjacent.destination.machineID) else { return }
                    try await self.navigation.perform(plan)
                }
            } catch { report(error) }
        }
    }

    /// Pane traversal is a separate ring. At either end it wraps within this
    /// machine; zero or one pane never falls through to another machine.
    /// A press while the last switch is still settling, or over a paste review,
    /// is ignored, as a swipe is.
    func stepPane(forward: Bool) {
        guard !closed, !overviewBlocksInput, !groupAppearanceBlocksInput,
              let session = selectedSession,
              !session.paneNavigationBusy, !session.clipboardIsPresenting else { return }
        do {
            if let control = session.muxControl, session.phase == .live {
                if let plan = try control.inputRouting.navigation(forward: forward) { session.showPane(plan.paneID) }
            } else if let action = captureCanvasAction(.pane, forward ? .next : .previous) {
                Task { do { try await action() } catch { report(error) } }
            }
        } catch { report(error) }
    }

    private func machinePlan(_ machineID: String) throws -> TerminalNavigationPlan? {
        if let controller = displayedSession(machineID),
           let paneID = selection(for: controller).paneID,
           navigation.navigation.machines.first(where: { $0.id == machineID })?.panes.contains(where: { $0.id == paneID }) == true {
            return try navigation.navigation.plan(paneID: paneID)
        }
        return try navigation.navigation.plan(machineID: machineID)
    }

    private func localPanePlan(_ direction: TerminalNavigationDirection) throws -> TerminalNavigationPlan? {
        guard let machineID = selectedMachineID, let selectedSession else { return nil }
        let controllers = terminalControllers(machineID)
        guard controllers.count > 1, let index = controllers.firstIndex(where: { $0 === selectedSession }) else { return nil }
        let next = (index + (direction == .next ? 1 : controllers.count - 1)) % controllers.count
        guard let paneID = selection(for: controllers[next]).paneID else { return nil }
        return try navigation.navigation.plan(paneID: paneID)
    }

    /// A machine or pane other than the one on screen has output the phone
    /// has not shown, so Next New Output has somewhere to go. The marks are
    /// the state SwiftUI redraws on; where the button would go decides, so a
    /// machine that is gone, with its last pane list, never leaves it shown.
    var hasUnseenOutputElsewhere: Bool {
        (navigationMachineIDs.contains(where: hasUnseenOutput)
            || selectedSession?.muxState.map { state in state.unread.contains { $0 != state.shown } } == true)
            && nextUnseenOutput() != nil
    }

    /// Shows the next machine or mux pane with output not yet shown, in ring
    /// order after the one on screen: the machine switch a chip tap makes,
    /// then the pane switch ⌘] makes. Output alone never moves the screen.
    func showNextUnseenOutput() {
        guard !closed, foreground, onScreen, !overviewBlocksInput, !groupAppearanceBlocksInput,
              selectedSession?.clipboardIsPresenting != true, selectedSession?.paneNavigationBusy != true,
              let target = nextUnseenOutput() else { return }
        visitFleetSource(target)
    }

    /// Next Attention includes connection/key requirements and unresolved
    /// input as well as unread output. It never changes either ring's order,
    /// dismisses a notice, resends input, or runs merely because state changed.
    var hasAttentionElsewhere: Bool { nextAttention() != nil }
    var hasActionableAttentionElsewhere: Bool {
        navigationMachineIDs.contains { $0 != selectedMachineID && !attentionReasons($0).subtracting([.newOutput]).isEmpty }
    }

    func showNextAttention() {
        guard !closed, foreground, onScreen, !overviewBlocksInput, !groupAppearanceBlocksInput,
              selectedSession?.clipboardIsPresenting != true, selectedSession?.paneNavigationBusy != true,
              let target = nextAttention() else { return }
        visitFleetSource(target)
    }

    private func nextAttention() -> TerminalUnseenOutput.Source? {
        var ring: [TerminalUnseenOutput.Source] = [], attention: Set<TerminalUnseenOutput.Source> = []
        var current = selectedMachineID.map {
            TerminalUnseenOutput.Source(machineID: $0, terminalSessionID: selectedSession?.terminalSessionID)
        }
        for id in navigationMachineIDs {
            let controllers = terminalControllers(id)
            if controllers.isEmpty {
                let source = TerminalUnseenOutput.Source(machineID: id)
                ring.append(source)
                if id != selectedMachineID, !attentionReasons(id).isEmpty { attention.insert(source) }
            }
            for controller in controllers {
                let source = TerminalUnseenOutput.Source(machineID: id, terminalSessionID: controller.terminalSessionID)
                ring.append(source)
                if id != selectedMachineID, controller.terminalSessionID == nil, !earlierTerminals(id).isEmpty {
                    attention.insert(source)
                }
                let selected = selectedSession === controller
                let needsConnection: Bool
                switch controller.phase {
                case .failed, .couldNotConnect: needsConnection = true
                case .ended: needsConnection = controller.dropped
                case .idle, .connecting, .ready, .live: needsConnection = false
                }
                let reasons = TerminalAttentionReason.observed(input: controller.inputDelivery,
                    keyboardNeedsDecision: controller.localInputNeedsDecision,
                    connectionNeedsAttention: needsConnection,
                    newOutput: controller.phase == .live && controller.hasUnseenOutput)
                if !selected, !reasons.isEmpty { attention.insert(source) }
                if controller.phase == .live, let state = controller.muxControl?.current {
                    for pane in state.panes {
                        let paneSource = TerminalUnseenOutput.Source(machineID: id, paneID: pane.paneId)
                        ring.append(paneSource)
                        if selected, pane.paneId == state.shown { current = paneSource }
                        else if state.unread.contains(pane.paneId) { attention.insert(paneSource) }
                    }
                }
            }
        }
        return TerminalUnseenOutput.next(after: current, ring: ring, unseen: attention)
    }

    /// Both shortcuts capture the original controller and exact pane plan
    /// before yielding. Reconnecting, inventory replacement, and A -> B -> A
    /// cannot turn the click into a visit to a replacement pane with its number.
    private func visitFleetSource(_ source: TerminalUnseenOutput.Source) {
        let session = session(machineID: source.machineID, terminalSessionID: source.terminalSessionID)
        let target: TerminalFleetVisit
        do {
            var pane: TerminalMuxInputRouting.NavigationPlan?
            if let paneID = source.paneID {
                guard session?.phase == .live, let control = session?.muxControl else {
                    throw TerminalNavigationError.stale
                }
                pane = try control.inputRouting.navigation(pane: paneID)
            }
            target = try .init(machineID: source.machineID, sessionID: session?.id, pane: pane)
        } catch { report(error); return }
        let appearance = appearanceGeneration
        Task {
            do {
                if target.machineID != self.selectedMachineID || self.selectedSession !== session {
                    try await whileBusy {
                        guard self.appearanceGeneration == appearance, !self.groupAppearanceBlocksInput,
                              self.foreground, self.onScreen, !self.overviewBlocksInput,
                              self.navigationMachineIDs.contains(target.machineID) else { throw TerminalNavigationError.stale }
                        try target.validate(sessionID: self.session(machineID: target.machineID, terminalSessionID: source.terminalSessionID)?.id,
                                            routing: self.session(machineID: target.machineID, terminalSessionID: source.terminalSessionID)?.muxControl?.inputRouting)
                        let plan: TerminalNavigationPlan?
                        if let session, let paneID = self.selection(for: session).paneID,
                           self.navigation.navigation.machines.first(where: { $0.id == target.machineID })?.panes.contains(where: { $0.id == paneID }) == true {
                            plan = try self.navigation.navigation.plan(paneID: paneID)
                        } else { plan = try self.navigation.navigation.plan(machineID: target.machineID) }
                        if let plan { try await self.navigation.perform(plan) }
                    }
                }
                guard self.appearanceGeneration == appearance, !self.closed, self.foreground, self.onScreen,
                      !self.overviewBlocksInput, !self.groupAppearanceBlocksInput,
                      self.selectedMachineID == target.machineID else { throw TerminalNavigationError.stale }
                try target.validate(sessionID: self.session(machineID: target.machineID, terminalSessionID: source.terminalSessionID)?.id,
                                    routing: self.session(machineID: target.machineID, terminalSessionID: source.terminalSessionID)?.muxControl?.inputRouting)
                guard let pane = target.pane else { return }
                // The machine's own view settles first, as after a chip tap.
                for _ in 0..<40 {
                    guard self.appearanceGeneration == appearance, !self.closed, self.selectedMachineID == target.machineID,
                          let session = self.selectedSession, session.phase == .live, let control = session.muxControl else {
                        throw TerminalNavigationError.stale
                    }
                    try target.validate(sessionID: session.id, routing: control.inputRouting)
                    if control.current.shown == pane.paneID { return }
                    await session.waitForOverviewPresentation()
                    guard self.appearanceGeneration == appearance, !self.closed, self.foreground, self.onScreen,
                          !self.overviewBlocksInput, !self.groupAppearanceBlocksInput,
                          self.selectedMachineID == target.machineID,
                          self.selectedSession === session else { throw TerminalNavigationError.stale }
                    try target.validate(sessionID: session.id, routing: control.inputRouting)
                    if !session.paneNavigationBusy {
                        try await session.navigatePane(pane)
                        return
                    }
                    try await Task.sleep(for: .milliseconds(50))
                }
            } catch { report(error) }
        }
    }

    private func nextUnseenOutput() -> TerminalUnseenOutput.Source? {
        var ring: [TerminalUnseenOutput.Source] = [], unseen: Set<TerminalUnseenOutput.Source> = []
        var current = selectedMachineID.map {
            TerminalUnseenOutput.Source(machineID: $0, terminalSessionID: selectedSession?.terminalSessionID)
        }
        for id in navigationMachineIDs {
            for session in terminalControllers(id) {
                let selected = selectedSession === session
                if session.phase == .live, let state = session.muxControl?.current, !state.panes.isEmpty {
                    for pane in state.panes {
                        let source = TerminalUnseenOutput.Source(machineID: id, paneID: pane.paneId)
                        ring.append(source)
                        if selected, pane.paneId == state.shown { current = source }
                        else if state.unread.contains(pane.paneId) { unseen.insert(source) }
                    }
                } else {
                    let source = TerminalUnseenOutput.Source(machineID: id, terminalSessionID: session.terminalSessionID)
                    ring.append(source)
                    if !selected, session.phase == .live, session.hasUnseenOutput { unseen.insert(source) }
                }
            }
        }
        return TerminalUnseenOutput.next(after: current, ring: ring, unseen: unseen)
    }

    var permitsCanvasNavigation: Bool {
        !closed && foreground && onScreen && !overviewBlocksInput && !groupAppearanceBlocksInput && appearanceSelection == nil
            && selectedSession?.clipboardIsPresenting != true
            && selectedSession?.paneNavigationBusy != true
            && selectedSession?.terminal?.remoteMouseGrabbed != true
            && selectedSession?.ownsViewportPan != true
    }

    /// Called once per direction at touch-down by the native finger classifier.
    /// Never look up a new neighbour at touch-up or after keyboard settlement.
    func captureCanvasAction(_ axis: TerminalNavigationAxis, _ direction: TerminalNavigationDirection)
        -> NativeTerminalNavigationControls.NavigationAction? {
        guard permitsCanvasNavigation else { return nil }
        let generation = canvasGeneration, sourceMachine = selectedMachineID
        let sessionID = selectedSession?.id
        let viewportGeneration = selectedSession?.viewportModeGeneration
        let sourcePane = selectedSession?.muxControl?.inputRouting.current.destination
        let stillCurrent: @MainActor () -> Bool = { [weak self] in
            guard let self else { return false }
            return self.permitsCanvasNavigation && self.canvasGeneration == generation
                && self.selectedMachineID == sourceMachine && self.selectedSession?.id == sessionID
                && self.selectedSession?.viewportModeGeneration == viewportGeneration
                && self.selectedSession?.muxControl?.inputRouting.current.destination == sourcePane
        }
        switch axis {
        case .machine:
            guard let adjacent = try? navigation.navigation.plan(axis: .machine, direction: direction),
                  let plan = try? machinePlan(adjacent.destination.machineID) else { return nil }
            return { [weak self] in
                guard let self, stillCurrent() else { throw TerminalNavigationError.stale }
                try await self.navigation.perform(plan)
            }
        case .pane:
            if selectedSession?.muxControl == nil {
                guard let plan = try? localPanePlan(direction) else { return nil }
                return { [weak self] in
                    guard let self, stillCurrent() else { throw TerminalNavigationError.stale }
                    try await self.navigation.perform(plan)
                }
            }
            guard let session = selectedSession, session.phase == .live, let control = session.muxControl,
                  let plan = try? control.inputRouting.navigation(forward: direction == .next) else { return nil }
            return { [weak session, weak control] in
                guard let session, let control, stillCurrent(), session.muxControl === control else {
                    throw TerminalNavigationError.stale
                }
                try await session.navigatePane(plan)
            }
        }
    }

    /// Connect approves authentication and normal on-select behavior. It does
    /// not grant a manual route, or a lost session, permission for a new PTY.
    func connect(_ machineID: String) {
        requestConnection(machineID, terminal: false)
    }

    /// The explicitly labelled Open Terminal / New Session action approves
    /// this exact machine even when its route is manual or its old PTY ended.
    func openTerminal(_ machineID: String) {
        requestConnection(machineID, terminal: true)
    }

    func openTerminal(_ controller: TerminalSessionController) {
        guard selectedSession === controller, owns(controller) else { return }
        if controller.terminalSessionID == nil { openTerminal(controller.target.machineID) }
        else if controller.phase == .ready, permitsTerminalCreation, onScreen,
                let slotID = controller.terminalSessionID, !invalidatedTerminalSlots.contains(slotID) {
            startAdditional(controller)
        }
        else { restartTerminal(controller) }
    }

    func retryConnection(_ controller: TerminalSessionController) {
        guard selectedSession === controller, owns(controller) else { return }
        if controller.terminalSessionID == nil { connect(controller.target.machineID) }
        else { restartTerminal(controller) }
    }

    private func requestConnection(_ machineID: String, terminal: Bool) {
        guard permitsTerminalCreation,
              navigationMachineIDs.contains(machineID),
              machines.first(where: { $0.id == machineID })?.target != nil else { return }
        if let session = sessions[machineID], !session.settled {
            switch session.phase {
            case .idle, .connecting, .ready: break
            case .live: return
            case .ended, .failed, .couldNotConnect: return
            }
        }
        do {
            if terminal { try connectionIntent.requestTerminal(machineID) }
            else { try connectionIntent.approveConnection(machineID) }
        } catch { report(error); return }
        explicitPreparations.insert(machineID)
        cancelReconnect(machineID)
        reconnectPolicies[machineID, default: .init()].resetForUserRequest()
        if sessions[machineID] == nil || sessions[machineID]?.settled == true {
            requestedConnections.insert(machineID)
        }
        reconcile()
    }

    /// Automatic callers have already reserved their retry budget. Keeping this
    /// separate from Connect prevents a retry from resetting its own limit.
    private func startConnection(_ machineID: String, preservingSuccessfulCandidate: Bool = false) {
        guard permitsTerminalCreation, controllerJobs[machineID] == nil,
              var target = machines.first(where: { $0.id == machineID })?.target else { return }
        if let theme = themes[machineID] { target = target.wearing(theme) }
        // A session that has not settled (starting, live, tearing down) blocks a second one.
        if let current = sessions[machineID], !current.settled { return }
        do { try connectionIntent.connectionStarted(machineID) }
        catch { report(error); return }
        requestedConnections.remove(machineID)
        let previous = sessions[machineID]
        let continuity = changedRouteMachines.remove(machineID) == nil ? previous : nil
        previous?.discardRetainedOutput()
        previous?.discardHostKeyMismatch()
        let controller = TerminalSessionController(target: target, connections: connections, focus: self,
            reconnectingFrom: preservingSuccessfulCandidate ? continuity : nil,
            retryingAfterPasswordHop: preservingSuccessfulCandidate ? nil : continuity,
            inputDelivery: previous?.inputDelivery ?? .init(),
            persistentShellNotice: previous?.persistentShellNotice,
            muxAccessMode: muxAccessSelections[machineID] ?? continuity?.muxAccessMode ?? initialMuxAccessMode,
            requiresExistingMux: existingMuxRequired.contains(machineID) || continuity?.requiresExistingMux == true)
        controller.onChange = { [weak self, weak controller] in
            guard let self, let controller, self.sessions[machineID] === controller else { return }
            self.reconcileRemoteFiles()
            // Releasing an unused sign-in did not end a remote terminal and
            // must not add the New Session gate or replenish reconnect intent.
            if self.residency.wasReleased(machineID: machineID, ownerID: controller.id) {
                self.reconcile(); return
            }
            switch controller.phase {
            case .live:
                try? self.connectionIntent.terminalOpened(machineID)
                if !self.inBackground { self.reconnectPolicies[machineID, default: .init()].recordLive() }
            case .ended, .failed:
                try? self.connectionIntent.terminalEnded(machineID,
                    reattachesExistingPanes: controller.dropped && controller.attachedToMux)
                self.reconnectPolicies[machineID, default: .init()].recordEnd()
            case .couldNotConnect:
                self.reconnectPolicies[machineID, default: .init()].recordEnd()
            case .idle, .connecting, .ready: break
            }
            self.reconcile()
        }
        sessions[machineID] = controller
        updateTerminalAuthenticationOwner()
        residency.started(machineID)
        controller.setPresentationVisible(isPresented(machineID) && displayedSession(machineID) === controller)
        runController(controller, machineID: machineID, starting: true)
    }

    /// Background preparation first reaches Ready. A deliberate terminal action
    /// enters the controller's reviewed creation path from the start, so New
    /// Session can acknowledge an earlier terminal's journal before sign-in.
    /// The retained wrapper also covers the controller's task-publication gap.
    private func runController(_ controller: TerminalSessionController, machineID: String, starting: Bool) {
        guard controllerJobs[machineID] == nil else { return }
        let id = UUID()
        let task = Task { [weak self, weak controller] in
            guard let self, let controller else { return }
            if starting, !Task.isCancelled, self.permitsTerminalCreation,
               self.sessions[machineID] === controller, !self.disconnected.contains(machineID) {
                let opening = self.connectionIntent.opening(machineID, creation: controller.route.sessionCreation)
                if opening == .explicit { await controller.activateTerminal(explicit: true) }
                else { await controller.start(openTerminal: false) }
            }
            if !Task.isCancelled, self.permitsTerminalCreation,
               self.sessions[machineID] === controller, controller.phase == .ready {
                let opening = self.connectionIntent.opening(machineID, creation: controller.route.sessionCreation)
                if opening == .explicit || (opening == .onSelect && self.shows(controllerID: controller.id, machineID: machineID)) {
                    await controller.activateTerminal(explicit: opening == .explicit)
                }
            }
            self.controllerWorkFinished(machineID, id: id, controllerID: controller.id)
        }
        controllerJobs[machineID] = .init(id: id, controllerID: controller.id, task: task)
    }

    private func controllerWorkFinished(_ machineID: String, id: UUID, controllerID: UUID) {
        guard let job = controllerJobs[machineID], job.id == id, job.controllerID == controllerID else { return }
        controllerJobs[machineID] = nil
        reconcile()
    }

    /// Ends a machine's session; it stays disconnected until Connect.
    func disconnect(_ machineID: String, expectedControllerID: UUID? = nil) {
        guard !closed, expectedControllerID == nil || sessions[machineID]?.id == expectedControllerID else { return }
        muxModeJobs.removeValue(forKey: machineID)?.task.cancel()
        changingMuxModes.remove(machineID)
        do { try connectionIntent.disconnect(machineID) }
        catch { report(error); return }
        requestedConnections.remove(machineID)
        explicitPreparations.remove(machineID)
        controllerJobs[machineID]?.task.cancel()
        cancelReconnect(machineID)
        reconnectPolicies[machineID] = nil
        for session in terminalControllers(machineID) {
            if let slotID = session.terminalSessionID { additionalJobs[slotID]?.cancel() }
            session.requestStop()
            Task { await session.stop() }
        }
    }

    func disconnectAll() {
        for machine in machines where machine.target != nil { disconnect(machine.id) }
    }

    /// The review names the exact controller, including its absence. Retire
    /// only that machine and join every task that could still publish its old
    /// connection. Failure leaves it disconnected with its cleanup owner.
    func prepareRouteChange(machineID: String, expectedControllerID: UUID?) async throws {
        guard permitsTerminalCreation,
              machines.contains(where: { $0.id == machineID }),
              sessions[machineID]?.id == expectedControllerID else { throw WorkspaceSSHConnectionError.stale }
        let machineControllers = terminalControllers(machineID)
        let extraStartups = machineControllers.compactMap { $0.terminalSessionID.flatMap { additionalJobs[$0] } }
        let startup = controllerJobs[machineID]?.task
        let muxChange = muxModeJobs[machineID]?.task
        let reconnect = reconnectJobs[machineID]?.task
        for controller in machineControllers { controller.requestRouteChange() }
        disconnect(machineID, expectedControllerID: expectedControllerID)
        for controller in machineControllers { await controller.stop() }
        for startup in extraStartups { await startup.value }
        await startup?.value
        await muxChange?.value
        await reconnect?.value
        guard !Task.isCancelled, permitsTerminalCreation,
              sessions[machineID]?.id == expectedControllerID,
              disconnected.contains(machineID), controllerJobs[machineID] == nil else {
            throw WorkspaceSSHConnectionError.stale
        }
        guard machineControllers.allSatisfy(\.settled) else { throw WorkspaceSSHConnectionError.busy }
    }

    /// Publish only after current local policy was rechecked and the old owner
    /// settled. Group preferences cannot reopen this machine; the following
    /// explicit Connect action releases its disconnected intent.
    func installRouteChange(_ target: TerminalSessionTarget, expectedControllerID: UUID?) throws {
        guard permitsTerminalCreation, target.workspaceID == workspaceID, target.revision == revision,
              let index = machines.firstIndex(where: { $0.id == target.machineID }),
              sessions[target.machineID]?.id == expectedControllerID,
              terminalControllers(target.machineID).allSatisfy(\.settled),
              controllerJobs[target.machineID] == nil, disconnected.contains(target.machineID) else {
            throw WorkspaceSSHConnectionError.stale
        }
        let old = machines[index]
        machines[index] = .init(id: old.id, name: old.name, target: target, unavailable: old.unavailable,
            theme: old.theme, autoConnect: old.autoConnect, tags: old.tags)
        changedRouteMachines.insert(target.machineID)
        muxAccessSelections[target.machineID] = nil
        existingMuxRequired.remove(target.machineID)
        invalidatedTerminalSlots.formUnion((additionalSessions[target.machineID] ?? []).map(\.id))
        for controller in terminalControllers(target.machineID) { controller.discardRetainedOutput() }
    }

    /// The mode a machine's shared pane is reconnecting in, while it is.
    func changingMuxMode(_ machineID: String) -> TerminalMuxAccessMode? {
        changingMuxModes.contains(machineID) ? muxAccessSelections[machineID] : nil
    }

    /// A mode change never changes authority under already admitted input.
    /// The old connection settles first; its uncertainty remains visible on
    /// the replacement, and an absent mux cannot become a new SSH shell.
    func reconnectMux(_ mode: TerminalMuxAccessMode, machineID: String, controllerID: UUID) {
        guard permitsTerminalCreation, let session = sessions[machineID], session.id == controllerID,
              session.phase == .live, session.attachedToMux, session.muxAccessMode != mode,
              !session.clipboardIsPresenting, !session.localInputNeedsDecision,
              muxModeJobs[machineID] == nil else { return }
        disconnect(machineID, expectedControllerID: controllerID)
        muxAccessSelections[machineID] = mode
        existingMuxRequired.insert(machineID)
        changingMuxModes.insert(machineID)
        let id = UUID()
        let task = Task { [weak self, session] in
            await session.stop()
            guard let self, self.muxModeJobs[machineID]?.id == id else { return }
            self.muxModeJobs[machineID] = nil; self.changingMuxModes.remove(machineID)
            guard !Task.isCancelled, self.permitsTerminalCreation,
                  self.sessions[machineID] === session, session.settled else { return }
            self.requestConnection(machineID, terminal: true)
        }
        muxModeJobs[machineID] = (id, task)
    }

    /// A shared pane larger than the phone can draw needs explicit permission
    /// to change its size. The prior owner must settle first; an absent mux
    /// still cannot become a new SSH shell.
    func takePaneAtPhoneSize(_ machineID: String, from session: TerminalSessionController) {
        guard permitsTerminalCreation, sessions[machineID] === session, session.paneTooLargeToView,
              !session.muxAccessMode.controlsSize, muxModeJobs[machineID] == nil else { return }
        muxAccessSelections[machineID] = .interactive
        existingMuxRequired.insert(machineID)
        changingMuxModes.insert(machineID)
        let id = UUID()
        let task = Task { [weak self, session] in
            await session.stop()
            guard let self, self.muxModeJobs[machineID]?.id == id else { return }
            self.muxModeJobs[machineID] = nil; self.changingMuxModes.remove(machineID)
            guard !Task.isCancelled, self.permitsTerminalCreation,
                  self.sessions[machineID] === session, session.settled else { return }
            self.requestConnection(machineID, terminal: true)
        }
        muxModeJobs[machineID] = (id, task)
    }

    /// Three operations at most prepare approved machines, selected first.
    /// Existing owners survive group changes; their bounded reconnect policy
    /// prepares a replacement transport without authorizing a replacement PTY.
    private func reconcile() {
        guard permitsTerminalCreation else { return }
        // Reservation precedes requestStop's synchronous callbacks. Cleanup
        // completion, not an ended phase, is what makes the slot available.
        guard capacityRelease == nil else { return }
        capacityBlockedMachineID = nil
        if let failed = failedCapacityRelease,
           sessions[failed.machineID]?.id != failed.id || sessions[failed.machineID]?.settled == true {
            failedCapacityRelease = nil
        }
        let ready = Set(sessions.filter { $0.value.phase == .idle || $0.value.phase == .ready }.map(\.key))
        let readyOrder = (try? TerminalConnectionOrder.prioritize(machines.map(\.id), selected: selectedMachineID,
                                                                 including: ready)) ?? []
        for id in readyOrder where controllerJobs.count + additionalJobs.count < 3 && controllerJobs[id] == nil {
            guard let controller = sessions[id], !disconnected.contains(id) else { continue }
            if controller.phase == .idle {
                runController(controller, machineID: id, starting: true)
                continue
            }
            let opening = connectionIntent.opening(id, creation: controller.route.sessionCreation)
            if opening == .explicit || (opening == .onSelect && shows(controllerID: controller.id, machineID: id)) {
                runController(controller, machineID: id, starting: false)
            }
        }
        let available = Set(machines.filter { $0.target != nil }.map(\.id))
        let preparedOrder = (try? connectionIntent.connectionOrder(available: available)) ?? []
        let retries: Set<String> = autoConnect ? available.filter(connectionIntent.permitsReconnect) : []
        let included = Set(preparedOrder).union(retries)
        let first = Set(preparedOrder)
        let inventory = preparedOrder + machines.map(\.id).filter { !first.contains($0) }
        let order = (try? TerminalConnectionOrder.prioritize(inventory, selected: selectedMachineID, including: included)) ?? preparedOrder
        var slots = max(0, 3 - controllerJobs.count - additionalJobs.count)
        for id in order {
            guard !disconnected.contains(id), controllerJobs[id] == nil, permitsPreparation(id) else { continue }
            if let session = sessions[id] {
                guard session.settled else { continue }
                if !requestedConnections.contains(id), !residency.wasReleased(machineID: id, ownerID: session.id) {
                    guard autoConnect, connectionIntent.permitsReconnect(id), session.dropped,
                          let delay = reconnectPolicies[id, default: .init()].nextDelay else { continue }
                    if let job = reconnectJobs[id] {
                        guard job.sessionID == session.id, job.ready, !job.cancelled else { continue }
                    } else {
                        scheduleReconnect(id, sessionID: session.id, delay: delay)
                        continue
                    }
                }
            }
            guard slots > 0 else { continue }
            if allControllers.filter({ !$0.settled }).count >= residency.limit {
                // Background warming alone must never evict another warm-up.
                // Only visible approved demand or an explicit action may do so.
                if (onScreen && id == selectedMachineID) || requestedConnections.contains(id) {
                    if capacityBlockedMachineID == nil || id == selectedMachineID { capacityBlockedMachineID = id }
                    if failedCapacityRelease == nil { releaseUnusedPreparation(for: id) }
                }
                if capacityRelease != nil { return }
                continue
            }
            let retry = sessions[id].map { !residency.wasReleased(machineID: id, ownerID: $0.id) } == true
                && !requestedConnections.contains(id)
            if retry {
                guard reconnectPolicies[id, default: .init()].beginRetry() else { continue }
                reconnectJobs[id] = nil
            }
            startConnection(id, preservingSuccessfulCandidate: retry); slots -= 1
        }
    }

    private func permitsPreparation(_ machineID: String) -> Bool {
        // A password is a visible user action. Do not warm password machines
        // in other tabs or automatically prompt again after a connection drops.
        if machines.first(where: { $0.id == machineID })?.target?.requiresAuthenticationPrompt == true,
           !requestedConnections.contains(machineID) {
            guard onScreen, selectedMachineID == machineID, sessions[machineID] == nil else { return false }
        }
        return residency.permitsPreparation(machineID, selected: onScreen ? selectedMachineID : nil,
                                           explicitlyRequested: requestedConnections.contains(machineID))
    }

    func preparationWasReleased(_ machineID: String) -> Bool {
        sessions[machineID].map { residency.wasReleased(machineID: machineID, ownerID: $0.id) } ?? false
    }

    var connectionLimit: Int { residency.limit }

    func retryCapacityCleanup(expectedControllerID: UUID) {
        guard !closed, foreground, capacityRelease == nil,
              let failed = failedCapacityRelease, failed.id == expectedControllerID,
              let controller = sessions[failed.machineID], controller.id == failed.id else { return }
        capacityRelease = failed; capacityReleaseMachineID = failed.machineID
        joinCapacityRelease(failed, controller: controller)
    }

    /// Reclaim only exact, idle, automatic preparers. Neither a live remote
    /// session nor a user's explicit/manual sign-in is an eviction candidate.
    private func releaseUnusedPreparation(for machineID: String) {
        guard capacityRelease == nil, failedCapacityRelease == nil else { return }
        let group = navigationMachineIDs, members = Set(group)
        let inventory = group + machines.map(\.id).filter { !members.contains($0) }
        do {
            let priority = try TerminalConnectionOrder.prioritize(inventory, selected: selectedMachineID,
                                                                  including: Set(inventory))
            let owned = allControllers.compactMap { controller -> TerminalConnectionResidency.Owner? in
                guard !controller.settled else { return nil }
                let id = controller.target.machineID
                let state: TerminalConnectionResidency.State
                switch controller.phase {
                case .ready:
                    state = controller.terminalSessionID == nil && controllerJobs[id] == nil && controller.canReleaseUnusedPreparation
                        && !explicitPreparations.contains(id) ? .automaticPreparation : .protectedPreparation
                case .live: state = .terminal
                case .idle, .connecting: state = .preparing
                case .ended, .failed, .couldNotConnect: state = .settling
                }
                return .init(machineID: id, id: controller.id, state: state, terminalSessionID: controller.terminalSessionID)
            }
            let protected = Set([selectedMachineID].compactMap { $0 }).union(explicitPreparations)
            guard case let .release(candidate) = try residency.decision(for: machineID, priority: priority,
                    owners: owned, protecting: protected),
                  let controller = sessions[candidate.machineID], controller.id == candidate.id,
                  controllerJobs[candidate.machineID] == nil else { return }
            try residency.willRelease(candidate)
            capacityRelease = candidate; capacityReleaseMachineID = candidate.machineID
            // No await between exact-owner validation, reservation and sealing.
            guard controller.requestReleaseUnusedPreparation() else {
                residency.releaseRefused(candidate); capacityRelease = nil; capacityReleaseMachineID = nil
                return
            }
            joinCapacityRelease(candidate, controller: controller)
        } catch { report(error) }
    }

    private func joinCapacityRelease(_ candidate: TerminalConnectionResidency.Owner,
                                     controller: TerminalSessionController) {
        // Cleanup is deliberately not cancelled by a navigation/foreground
        // change. It already owns a sealed connection and must finish settling.
        capacityReleaseJob = Task { [weak self, controller] in
            await controller.stop()
            guard let self, self.capacityRelease == candidate else { return }
            if !controller.settled { self.failedCapacityRelease = candidate }
            self.capacityRelease = nil; self.capacityReleaseMachineID = nil; self.capacityReleaseJob = nil
            self.reconcile()
        }
    }

    private func scheduleReconnect(_ machineID: String, sessionID: UUID, delay: Duration) {
        guard reconnectJobs[machineID] == nil else { return }
        let id = UUID()
        let task = Task { [weak self] in
            do {
                try await Task.sleep(for: delay)
                self?.reconnectWaitFinished(machineID, id: id, sessionID: sessionID, elapsed: true)
            } catch {
                self?.reconnectWaitFinished(machineID, id: id, sessionID: sessionID, elapsed: false)
            }
        }
        reconnectJobs[machineID] = .init(id: id, sessionID: sessionID, task: task)
    }

    private func reconnectWaitFinished(_ machineID: String, id: UUID, sessionID: UUID, elapsed: Bool) {
        guard var job = reconnectJobs[machineID], job.id == id else { return }
        if elapsed, !job.cancelled, !closed, !inBackground,
           !disconnected.contains(machineID), sessions[machineID]?.id == sessionID {
            job.ready = true
            reconnectJobs[machineID] = job
        } else { reconnectJobs[machineID] = nil }
        reconcile()
    }

    private func cancelReconnect(_ machineID: String) {
        guard var job = reconnectJobs[machineID] else { return }
        if job.ready { reconnectJobs[machineID] = nil; return }
        job.cancelled = true
        reconnectJobs[machineID] = job
        job.task.cancel()
    }

    private func cancelReconnects() {
        for id in Array(reconnectJobs.keys) { cancelReconnect(id) }
    }

    /// Re-establishes the keyboard after its input generation ended (dismissed
    /// keyboard, an alert, scene deactivation). Never reconnects anything.
    func activateKeyboard(showKeyboard: Bool = true, source: TerminalSource? = nil) async {
        guard !Task.isCancelled, !closed, onScreen, foreground, !overviewBlocksInput, !groupAppearanceBlocksInput,
              selectedSession?.keyboardActivationBlocked != true,
              let selection = navigation.navigation.selection, selection.paneID != nil,
              selectedSession?.phase == .live,
              source == nil || selectedBinding?.source == source,
              showKeyboard || selectedSession?.terminal?.isSceneActive == true else { return }
        let displayKeyboard = showKeyboard && selectedSession?.isViewingOnly != true
        if navigation.phase == .ready, owner.keyboard?.acceptsInput == true {
            if displayKeyboard { selectedSession?.terminal?.requestKeyboard(true) }
            return
        }
        guard navigation.phase == .inactive else { return }
        let generation = canvasGeneration, activity = selectedSession?.terminal?.sceneActivityGeneration
        await owner.joinSealed()
        guard !Task.isCancelled, !closed, onScreen, foreground, canvasGeneration == generation,
              !overviewBlocksInput, !groupAppearanceBlocksInput,
              navigation.navigation.selection == selection,
              selectedSession?.keyboardActivationBlocked != true,
              source == nil || selectedBinding?.source == source,
              showKeyboard || (selectedSession?.terminal?.isSceneActive == true
                  && selectedSession?.terminal?.sceneActivityGeneration == activity) else { return }
        let request = UUID()
        keyboardPresentation = (request, selection, displayKeyboard)
        defer { if keyboardPresentation?.id == request { keyboardPresentation = nil } }
        do { try await navigation.activateSelection() } catch { report(error) }
    }

    /// Shows a machine in another theme: its chrome at once, and its
    /// terminal from the next frame. A later session starts in it.
    func applyTheme(_ theme: WorkspaceDocument.Theme, to machineID: String) {
        guard !closed, machines.contains(where: { $0.id == machineID }) else { return }
        themes[machineID] = theme
        for controller in terminalControllers(machineID) { controller.applyTheme(theme) }
    }

    /// Synchronously seal every scene-owned responder before scheduling joins.
    /// Used when the workspace itself goes away (replaced or removed).
    func requestClose() {
        guard !closed else { return }
        closed = true
        splitGeneration = UUID(); splitJob?.cancel()
        cancelOverview()
        cancelGroupAppearance()
        canvasGeneration = UUID()
        pathMonitor?.cancel(); pathMonitor = nil
        cancelReconnects()
        for job in muxModeJobs.values { job.task.cancel() }
        for job in controllerJobs.values { job.task.cancel() }
        for job in additionalJobs.values { job.cancel() }
        updateTerminalAuthenticationOwner()
        navigation.suspend()
        for controller in allControllers {
            controller.discardRetainedOutput()
            controller.requestStop()
        }
    }

    /// Concurrent callers join the same cleanup, not an early closed flag.
    /// A subsequent call retries failed settlements; it never reopens a shell.
    func close() async {
        requestClose()
        if let closing { await closing.value; return }
        let controllers = allControllers, navigation = self.navigation, files = remoteFiles
        let reconnects = reconnectJobs.values.map(\.task)
        let operations = controllerJobs.values.map(\.task) + Array(additionalJobs.values)
        let modeChanges = muxModeJobs.values.map(\.task)
        let overview = overviewJob?.task
        let appearance = groupAppearanceJob?.task
        let release = capacityReleaseJob
        let split = splitJob
        let job = Task {
            await files?.retryCleanup()
            if let split { await split.value }
            if let release { await release.value }
            if let appearance { await appearance.value }
            if let overview { await overview.value }
            for reconnect in reconnects { await reconnect.value }
            for controller in controllers { await controller.stop() }
            for operation in operations { await operation.value }
            for change in modeChanges { await change.value }
            await navigation.joinSuspension()
        }
        closing = job
        await job.value
        remoteFiles = nil
        closing = nil
    }

    func report(_ error: any Error) {
        problem = TerminalSessionController.describe(error)
    }
    func dismissProblem() { problem = nil }

    private func inventory(_ values: [UUID: TerminalKeyboardNavigationBinding]) throws -> [TerminalNavigationMachine] {
        try machines.map { machine in
            let hasTabs = additionalSessions[machine.id]?.isEmpty == false
            let panes = try terminalControllers(machine.id).compactMap { controller -> TerminalNavigationPane? in
                guard hasTabs || values[controller.id] != nil else { return nil }
                return try TerminalNavigationPane(id: selection(for: controller, bindings: values).paneID!,
                    title: hasTabs ? terminalTitle(controller) : "Direct SSH")
            }
            return try TerminalNavigationMachine(id: machine.id, name: machine.name, panes: panes)
        }
    }

    private func inactiveSelections(_ values: [UUID: TerminalKeyboardNavigationBinding]) -> [TerminalNavigationSelection] {
        machines.flatMap { machine -> [TerminalNavigationSelection] in
            guard additionalSessions[machine.id]?.isEmpty == false else { return [] }
            return terminalControllers(machine.id).filter { values[$0.id] == nil }
                .map { selection(for: $0, bindings: values) }
        }
    }

    /// Publish the same exact inventory to the input owner and navigation.
    /// Ended/connecting slots are inert placeholders, never fabricated input.
    private func publishBindings(_ values: [UUID: TerminalKeyboardNavigationBinding]) throws {
        let inventory = try inventory(values)
        try owner.replaceBindings(Array(values.values), inactiveSelections: inactiveSelections(values))
        bindings = values
        try navigation.replaceInventory(inventory)
    }

    /// Committed navigation: show its machine and, once the destination's focus
    /// is published, put the new keyboard into that machine's view. A machine
    /// that connected in the background gets its view now.
    private func navigationChanged() {
        let selection = navigation.navigation.selection
        let oldControllerID = selectedSession?.id
        navigationMachineIDs = navigation.navigation.machines.map(\.id)
        splitLayout.reconcile(previous: selectedMachineID, selected: selection?.machineID,
                              available: navigationMachineIDs)
        if selectedMachineID != selection?.machineID {
            canvasGeneration = UUID()
            selectedMachineID = selection?.machineID
            reconcileRemoteFiles()
            do { try connectionIntent.select(selectedMachineID) } catch { report(error) }
        }
        if let selection, let controller = controller(for: selection) {
            selectedTerminalSessionIDs[selection.machineID] = controller.terminalSessionID
        }
        if oldControllerID != selectedSession?.id {
            canvasGeneration = UUID()
            reconcileRemoteFiles()
        }
        updateTerminalAuthenticationOwner()
        if !groupAppearanceBlocksInput, appearanceSelection == selectedMachineID { appearanceSelection = nil }
        updatePresentationVisibility()
        if onScreen, !overviewBlocksInput, !groupAppearanceBlocksInput,
           let session = selectedSession, session.phase == .live, session.terminal == nil { session.showView() }
        reconcile()
        guard !overviewBlocksInput, !groupAppearanceBlocksInput,
              navigation.phase == .ready, let selection, selection.paneID != nil,
              let keyboard = owner.keyboard else { return }
        let visible = keyboardPresentation.flatMap { $0.selection == selection ? $0.visible : nil } ?? true
        controller(for: selection)?.install(keyboard, showKeyboard: visible)
    }

    private func isVisible(_ machineID: String) -> Bool {
        isPresented(machineID) && machineID == selectedMachineID
    }

    private func isPresented(_ machineID: String) -> Bool {
        !closed && foreground && !inBackground && onScreen && !groupAppearanceBlocksInput && appearanceSelection == nil
            && (!overviewBlocksInput || (overviewAllowsTargetPresentation && machineID == selectedMachineID))
            && displayedMachineIDs.contains(machineID)
    }

    private func updatePresentationVisibility() {
        for controller in allControllers {
            let machineID = controller.target.machineID
            controller.setOverviewBlocking(overviewBlocksInput || groupAppearanceBlocksInput)
            controller.setPresentationVisible(isPresented(machineID) && displayedSession(machineID) === controller)
        }
    }

    /// Navigation refuses overlapping transactions; a gesture in flight ends
    /// within its bounded handoff, so retry briefly rather than drop the change.
    private func whileBusy(_ body: () async throws -> Void) async throws {
        for attempt in 0..<100 {
            try Task.checkCancellation()
            do { return try await body() }
            catch TerminalNavigationError.busy where attempt < 99 { try await Task.sleep(for: .milliseconds(50)) }
            // A mux snapshot can temporarily fence the old keyboard's model
            // transaction before its focus handoff is admitted. The keyboard
            // retains its original focus on this refusal, so retry the same
            // validated navigation rather than dropping the user's gesture.
            catch TerminalModelError.busy where attempt < 99 { try await Task.sleep(for: .milliseconds(50)) }
        }
    }

    // MARK: TerminalSessionFocus

    var permitsHostKeyMismatchCapture: Bool { !closed && !inBackground && onScreen }

    var permitsTerminalCreation: Bool {
        !closed && foreground && !inBackground && !overviewBlocksInput
            && !groupAppearanceBlocksInput && appearanceSelection == nil
    }

    func shows(machineID: String) -> Bool { isVisible(machineID) }

    func shows(controllerID: UUID, machineID: String) -> Bool {
        isVisible(machineID) && displayedSession(machineID)?.id == controllerID
    }

    func attach(machineID: String, source: TerminalSource,
                encoder: any TerminalKeyboardEncoding) async throws -> TerminalKeyboardSession {
        guard let controller = sessions[machineID] else { throw TerminalNavigationError.unavailable }
        return try await attach(controllerID: controller.id, machineID: machineID, source: source, encoder: encoder)
    }

    func attach(controllerID: UUID, machineID: String, source: TerminalSource,
                encoder: any TerminalKeyboardEncoding) async throws -> TerminalKeyboardSession {
        guard !closed, terminalControllers(machineID).contains(where: { $0.id == controllerID }),
              source.identity.machineID == machineID else { throw TerminalNavigationError.unavailable }
        let binding = TerminalKeyboardNavigationBinding(source: source, encoder: encoder)
        // Bindings are installed before the inventory offers the new pane.
        // Recompute after each busy wait, so another machine's concurrent
        // attachment/detachment is never overwritten by an old dictionary.
        try await whileBusy {
            guard !self.closed, self.terminalControllers(machineID).contains(where: { $0.id == controllerID }) else {
                throw TerminalNavigationError.unavailable
            }
            var next = self.bindings; next[controllerID] = binding
            try self.publishBindings(next)
        }
        // The sign-in that led here usually ends behind Face ID's sheet, which
        // leaves the scene inactive for a moment; the keyboard waits for it
        // rather than failing the new session. Background still refuses.
        for _ in 0..<200 where !foreground && !inBackground && !closed { try await Task.sleep(for: .milliseconds(50)) }
        try await whileBusy {
            guard !self.closed, self.bindings[controllerID]?.source == source else { throw TerminalNavigationError.stale }
            guard self.shows(controllerID: controllerID, machineID: machineID) else { throw CancellationError() }
            guard let plan = try self.navigation.navigation.plan(paneID: binding.paneID)
                    ?? self.navigation.navigation.activationPlan() else { throw TerminalNavigationError.unavailable }
            try await self.navigation.perform(plan)
        }
        guard !closed, owner.currentSelection?.paneID == binding.paneID, let keyboard = owner.keyboard else {
            throw TerminalNavigationError.stale
        }
        return keyboard
    }

    func detach(machineID: String, source: TerminalSource) async throws {
        let departing = terminalControllers(machineID).first { bindings[$0.id]?.source == source }
        // Removing the visible source seals its input before the list changes.
        // Preserve the old map on refused binding admission. A retry also
        // reconciles inventory if an earlier inventory update failed afterward.
        try await whileBusy {
            var next = self.bindings
            for (id, binding) in next where binding.source == source { next[id] = nil }
            try self.publishBindings(next)
        }
        await owner.joinSealed()
        // Preserve this exact ended tab as an input-free ring position. This
        // never selects a sibling or creates a replacement shell, and a newer
        // user selection wins while the old source's cleanup is joining.
        if let departing, owns(departing), selectedSession === departing,
           additionalSessions[machineID]?.isEmpty == false, permitsCanvasNavigation,
           navigation.navigation.selection?.paneID == nil {
            try await whileBusy {
                guard self.owns(departing), self.selectedSession === departing, self.permitsCanvasNavigation,
                      self.bindings[departing.id] == nil,
                      self.navigation.navigation.selection?.paneID == nil else { return }
                guard let paneID = self.selection(for: departing).paneID,
                      let plan = try self.navigation.navigation.plan(paneID: paneID) else { return }
                try await self.navigation.perform(plan)
            }
        }
    }

    func fontSizeChanged(_ size: Double) {
        UserDefaults.standard.set(size, forKey: TerminalSessionController.fontSizeKey)
        for session in allControllers { session.setFontSize(size) }
    }

    func inputRetired(machineID: String) {
        guard let controller = displayedSession(machineID) else { return }
        inputRetired(controllerID: controller.id, machineID: machineID)
    }

    func inputRetired(controllerID: UUID, machineID: String) {
        // A handoff retires the old keyboard itself and reports it synchronously;
        // sealing then would invalidate the switch in flight. Only a settled
        // selection's lost input (dismissal, resign, OS) is sealed here.
        guard navigation.phase == .ready, owner.currentSelection?.machineID == machineID,
              selectedSession?.id == controllerID,
              owner.keyboard?.acceptsInput != true else { return }
        owner.seal()
    }
}

extension TerminalSessionController: Identifiable {}

/// How a live scene reads and records theme picks; the workspace owns them.
struct SceneThemeSource {
    let themes: @MainActor () -> MachineThemes?
    let choose: @MainActor (_ machineID: String, _ choice: MachineThemeChoice) -> Void
}

/// Shows the workspace's live scene, which the app keeps across visits. A
/// NavigationLink evaluates its destination on every redraw of the page that
/// holds it, so the scene is fetched once, when the screen is actually shown.
struct LiveWorkspaceView: View {
    let initialMachineID: String
    var themes: SceneThemeSource? = nil
    var group: TerminalConnectionIntent.Group? = nil
    var connectOnOpen = false
    var onShowing: (@MainActor (_ machineID: String?) -> Void)? = nil
    let makeScene: @MainActor () throws -> WorkspaceTerminalScene
    @State private var scene: WorkspaceTerminalScene?
    @State private var failed = false

    var body: some View {
        Group {
            if let scene {
                WorkspaceTerminalScreen(scene: scene, initialMachineID: initialMachineID, themes: themes,
                                        group: group, connectOnOpen: connectOnOpen, onShowing: onShowing)
            }
            else if failed { Text("These machines could not be opened together.").padding() }
            else { Color.black.ignoresSafeArea() }
        }
        .onAppear {
            guard scene == nil, !failed else { return }
            do { scene = try makeScene() } catch { failed = true }
        }
    }
}

/// The live workspace: a strip of every machine above the selected machine's
/// terminal, or that machine's connecting, help or ended card.
struct WorkspaceTerminalScreen: View {
    @State var scene: WorkspaceTerminalScene
    let initialMachineID: String
    var themes: SceneThemeSource? = nil
    var group: TerminalConnectionIntent.Group? = nil
    var connectOnOpen = false
    /// Told the machine on screen, unless the user disconnected it, and nil
    /// on leaving: a relaunch returns there (MobileWorkspaceModel.showing).
    /// A screen whose scene a restore closed keeps the record on leaving: its
    /// page opens that terminal again when the machine can connect.
    var onShowing: (@MainActor (_ machineID: String?) -> Void)? = nil
    private var returnableMachineID: String? {
        scene.selectedMachineID.flatMap { scene.disconnected.contains($0) ? nil : $0 }
    }
    @Environment(\.scenePhase) private var scenePhase
    /// A presentation returning to this screen is not another Connect action.
    @State private var initialConnectionConsumed = false
    @State private var pickingTheme = false
    @State private var historyInspection: TerminalHistoryInspection?
    @State private var retainedOutput: TerminalRetainedOutput?
    /// A machine attached to its mux searches its panes' history on the server.
    @State private var muxSearch: TerminalSessionController?
    @State private var useTerminalReview: TerminalSessionController?
    @State private var hostKeyReview: TerminalSessionController?
    private struct PersistentShellReview: Identifiable {
        var id: UUID { request.id }
        let session: TerminalSessionController
        let request: TerminalSessionController.PersistentShellReview
    }
    @State private var persistentShellReview: PersistentShellReview?
    private struct EarlierTerminalsReview: Identifiable {
        let id = UUID()
        let machineID: String, machineName: String
        let records: [TerminalSessionRecord]
    }
    @State private var earlierTerminalsReview: EarlierTerminalsReview?

    var body: some View {
        let machine = scene.selectedMachine
        let canvasMachines = scene.displayedMachineIDs.compactMap { id in scene.machines.first { $0.id == id } }
        let overviewSessionID = scene.overviewSessionID
        let backgroundHex = machine.flatMap { scene.themes[$0.id]?.background }
        let background = backgroundHex.map { Color(ftHex: $0) } ?? .black
        // The navigation bar and the strips' material sit on the theme's
        // background, so their text follows it.
        let chrome: ColorScheme = TerminalSessionTarget.isDark(backgroundHex) ? .dark : .light
        VStack(spacing: 0) {
            MachineStrip(scene: scene).environment(\.colorScheme, chrome)
            if let machine, scene.additionalSessions[machine.id]?.isEmpty == false
                || scene.sessions[machine.id]?.additionalTerminalTarget != nil {
                directTerminalStrip(machine).environment(\.colorScheme, chrome)
            }
            if let machine, !scene.earlierTerminals(machine.id).isEmpty || scene.terminalHistoryFailure != nil {
                earlierTerminalBar(machine).environment(\.colorScheme, chrome)
            }
            if let session = scene.selectedSession, session.phase == .live, let mux = session.muxState {
                muxAccessBar(session).environment(\.colorScheme, chrome)
                if !mux.panes.isEmpty {
                    PaneStrip(session: session, state: mux,
                              accent: Color(ftHex: machine.flatMap { scene.themes[$0.id]?.accent } ?? "#3B82F6"),
                              onKeyboard: { Task { await scene.activateKeyboard() } })
                        .environment(\.colorScheme, chrome)
                        .transition(.move(edge: .top).combined(with: .opacity))
                }
            }
            if let notice = scene.selectedSession?.persistentShellNotice {
                HStack(spacing: 10) {
                    if scene.selectedSession?.muxState?.shellCreation?.pending == true { ProgressView() }
                    else { Image(systemName: "terminal") }
                    Text(notice).font(.caption).frame(maxWidth: .infinity, alignment: .leading)
                }
                .padding(.horizontal, 14).padding(.vertical, 8).background(.regularMaterial)
                .environment(\.colorScheme, chrome)
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("terminal.mux.creation.result")
            }
            if scene.splitExpanded {
                HStack(spacing: 1) {
                    ForEach(canvasMachines) { splitHeading($0) }
                }
                .environment(\.colorScheme, chrome)
            }
            ZStack {
                WorkspaceCanvasHost(scene: scene, panes: canvasMachines.map {
                    WorkspaceCanvasPane(machineID: $0.id, terminal: scene.displayedSession($0.id)?.terminal)
                })
                .background {
                    GeometryReader { geometry in
                        Color.clear.onChange(of: geometry.size.width, initial: true) { _, width in
                            scene.setSplitViewport(width: Double(width), isPad: UIDevice.current.userInterfaceIdiom == .pad)
                        }
                    }
                }
                HStack(spacing: 1) {
                    ForEach(canvasMachines) { machine in
                        ZStack {
                            if machine.id == scene.selectedMachineID {
                                card(for: machine).transition(.opacity)
                            } else if scene.displayedSession(machine.id)?.terminal == nil {
                                Button { scene.select(machine.id) } label: {
                                    VStack(spacing: 10) {
                                        Image(systemName: "desktopcomputer").font(.title)
                                        Text(verbatim: machine.name).font(.headline)
                                        Text("Select this machine to continue").font(.callout)
                                    }.padding().background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16))
                                }.buttonStyle(.plain)
                            }
                        }.frame(maxWidth: .infinity, maxHeight: .infinity)
                    }
                }
            }
        }
        .background(background.ignoresSafeArea())
        .animation(.smooth(duration: 0.25), value: scene.selectedMachineID)
        .sensoryFeedback(.selection, trigger: scene.selectedMachineID)
        .overlay(alignment: .bottom) {
            if let session = scene.selectedSession, let recovery = session.localInputRecovery {
                inputRecoveryBanner(recovery, session: session)
            } else if scene.selectedSession?.resolvingLocalInput == true {
                ProgressView("Resetting keyboard…")
                    .padding(14).background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16)).padding()
            } else if let session = scene.selectedSession,
                      let delivery = session.inputDeliveryMessage {
                problemBanner(delivery) { session.dismissInputDelivery() }
                    .accessibilityElement(children: .contain)
                    .accessibilityIdentifier("terminal.input.delivery")
            } else if let session = scene.selectedSession, let problem = session.interactionProblem {
                problemBanner(problem) { session.dismissInteractionProblem() }
            } else if let problem = scene.problem {
                problemBanner(problem) { scene.dismissProblem() }
            } else if scene.selectedSession?.shownMuxPaneWaiting == true {
                // Not dismissable: it says what the pane is doing and leaves
                // when the machine sends the pane's screen.
                ProgressView("The machine is busy and is not sending this pane's screen. Still asking…")
                    .padding(14).background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16)).padding()
                    .accessibilityIdentifier("terminal.mux.waiting")
            }
        }
        .navigationTitle(scene.workspaceName)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar { toolbar(machine) }
        .sheet(isPresented: $pickingTheme) {
            if let themes, let id = machine?.id, let current = themes.themes(),
               let documentMachine = current.document.machines.first(where: { $0.id == id }) {
                MachineThemePicker(machine: documentMachine, themes: current) { choice in
                    themes.choose(id, choice)
                    if let theme = themes.themes()?.theme(for: documentMachine) { scene.applyTheme(theme, to: id) }
                }
            }
        }
        .toolbarBackground(background, for: .navigationBar)
        .sheet(item: $historyInspection) { inspected in TerminalHistoryScreen(inspection: inspected) }
        .sheet(item: $muxSearch) { session in MuxSearchScreen(session: session) }
        .sheet(item: $retainedOutput) { output in TerminalRetainedOutputScreen(output: output) }
        .sheet(item: $hostKeyReview) { session in
            TerminalHostKeyMismatchScreen(scene: scene, session: session)
        }
        .sheet(item: $earlierTerminalsReview) { review in earlierTerminalReview(review) }
        .sheet(item: Binding(get: { persistentShellReview }, set: { review in
            if let review { persistentShellReview = review }
            else { dismissPersistentShellReview() }
        })) { review in persistentShellConfirmation(review) }
        .confirmationDialog("How should this phone use the shared terminal?",
            isPresented: Binding(get: { useTerminalReview != nil }, set: { if !$0 { useTerminalReview = nil } }),
            titleVisibility: .visible, presenting: useTerminalReview) { session in
                if session.muxAccessMode != .inputOnly {
                    Button("Type at Shared Size") {
                        scene.reconnectMux(.inputOnly, machineID: session.target.machineID, controllerID: session.id)
                        useTerminalReview = nil
                    }
                }
                if session.muxAccessMode != .interactive {
                    Button("Type and Fit to Phone") {
                        scene.reconnectMux(.interactive, machineID: session.target.machineID, controllerID: session.id)
                        useTerminalReview = nil
                    }
                }
                if session.muxAccessMode != .viewOnly {
                    Button("View Only") {
                        scene.reconnectMux(.viewOnly, machineID: session.target.machineID, controllerID: session.id)
                        useTerminalReview = nil
                    }
                }
                Button("Cancel", role: .cancel) { useTerminalReview = nil }
            } message: { session in
                Text("Reconnect to \(session.target.machineName) with the selected permissions. Type at Shared Size keeps the pane's current dimensions. Type and Fit to Phone changes them for every client and permits separately reviewed new persistent shells. Other connected clients can still type and resize it.")
            }
        .sheet(isPresented: Binding(get: { scene.overviewPresented }, set: { visible in
            if !visible { scene.dismissOverview() }
        }), onDismiss: { if let overviewSessionID { scene.finishOverviewDismissal(overviewSessionID) } }) {
            switch scene.presentedPanel {
            case .fleet: WorkspaceFleetOverviewScreen(scene: scene)
            case .hardwareKeyboard: TerminalHardwareSettingsScreen()
            case .remoteFiles:
                if let files = scene.remoteFiles {
                    TerminalRemoteFilesScreen(scene: scene, files: files).id(files.id)
                } else {
                    ContentUnavailableView("Remote Files Closed", systemImage: "folder")
                }
            }
        }
        .toolbarBackground(.visible, for: .navigationBar)
        .toolbarColorScheme(chrome, for: .navigationBar)
        .task {
            let connect = connectOnOpen && !initialConnectionConsumed
            initialConnectionConsumed = true
            scene.appear(selecting: initialMachineID, active: scenePhase == .active,
                         group: group, connectOnOpen: connect)
        }
        .onChange(of: returnableMachineID, initial: true) { _, id in onShowing?(id) }
        .onDisappear {
            historyInspection?.retire(); historyInspection = nil; retainedOutput = nil
            hostKeyReview = nil
            earlierTerminalsReview = nil
            dismissPersistentShellReview(resumingView: false)
            scene.disappear()
            if !scene.closed { onShowing?(nil) }
        }
        .onChange(of: scenePhase) { _, phase in
            // The app forwards its phase to the scene (MobileWorkspaceModel).
            if phase != .active {
                historyInspection?.retire(); historyInspection = nil; retainedOutput = nil
                hostKeyReview = nil
                earlierTerminalsReview = nil
                dismissPersistentShellReview(resumingView: false)
            }
        }
        .onChange(of: scene.selectedSession?.id) { _, _ in
            hostKeyReview = nil; earlierTerminalsReview = nil
            dismissPersistentShellReview(resumingView: false)
        }
    }

    private func dismissPersistentShellReview(resumingView: Bool = true) {
        // A review can be cancelled before SwiftUI ever mounts its sheet.
        // Retire the captured controller now; onDisappear is only a backstop.
        guard let review = persistentShellReview else { return }
        persistentShellReview = nil
        review.session.endPersistentShellReview(review.request, resumingView: resumingView)
    }

    private func beginPersistentShell(_ session: TerminalSessionController) {
        do { persistentShellReview = .init(session: session, request: try session.preparePersistentShell()) }
        catch { session.reportPersistentShellProblem(error) }
    }

    private func persistentShellConfirmation(_ review: PersistentShellReview) -> some View {
        NavigationStack {
            Form {
                Section("Connected machine") {
                    Text(verbatim: review.request.machineName).font(.headline)
                    Text(verbatim: "\(review.request.user)@\(review.request.host):\(review.request.port)")
                        .font(.system(.body, design: .monospaced)).textSelection(.enabled)
                    Text(verbatim: Self.via(review.request.route)).font(.caption)
                }
                Section("New persistent shell") {
                    LabeledContent("Domain", value: "Server's default domain")
                    LabeledContent("Workspace", value: review.request.plan.workspace)
                    LabeledContent("Initial size", value: "\(review.request.plan.columns) columns × \(review.request.plan.rows) rows")
                    Text("Creates a new window and tab using the default domain's configured shell and working directory. That domain may be local or remote. The shell remains in the mux when this phone disconnects.")
                    Text("After confirmation, select the new pane from the pane list. This phone keeps its current pane selected.")
                }
                if let previous = review.session.persistentShellNotice {
                    Section("Previous request") { Text(previous) }
                }
            }
            .navigationTitle("New Persistent Shell").navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismissPersistentShellReview() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Create Shell") {
                        do { try review.session.createPersistentShell(review.request) }
                        catch { review.session.reportPersistentShellProblem(error) }
                        dismissPersistentShellReview()
                    }
                    .disabled(scene.selectedSession !== review.session || !review.session.canConfirmPersistentShell(review.request))
                    .accessibilityIdentifier("terminal.mux.creation.confirm")
                }
            }
        }
        .onDisappear { review.session.endPersistentShellReview(review.request) }
    }

    private func directTerminalStrip(_ machine: SceneMachine) -> some View {
        ScrollViewReader { reader in
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 8) {
                    ForEach(scene.terminalControllers(machine.id)) { controller in
                        let selected = scene.selectedSession === controller
                        Button { scene.selectTerminal(controller) } label: {
                            HStack(spacing: 6) {
                                Image(systemName: controller.phase == .live ? "terminal" : controller.settled ? "stop.circle" : "hourglass")
                                Text(scene.terminalTitle(controller)).lineLimit(1)
                                if !selected, controller.hasUnseenOutput {
                                    Circle().fill(.orange).frame(width: 6, height: 6)
                                }
                                if controller.inputDeliveryMessage != nil || controller.localInputNeedsDecision {
                                    Image(systemName: "exclamationmark.circle.fill").foregroundStyle(.orange)
                                }
                            }
                            .font(.caption.weight(selected ? .semibold : .regular))
                            .padding(.horizontal, 12).frame(minHeight: 40)
                            .background(selected ? Color.accentColor.opacity(0.18) : Color.primary.opacity(0.06),
                                        in: RoundedRectangle(cornerRadius: 10))
                        }
                        .buttonStyle(.plain).id(controller.id)
                        .accessibilityLabel("\(scene.terminalTitle(controller)), \(controller.phase == .live ? "live" : controller.settled ? "ended" : "connecting")\(controller.hasUnseenOutput && !selected ? ", new output" : "")")
                        .accessibilityAddTraits(selected ? .isSelected : [])
                        .accessibilityIdentifier("terminal.tab.\(controller.terminalSessionID?.uuidString ?? "primary")")
                    }
                    Button("New Terminal", systemImage: "plus") { scene.newTerminal() }
                        .font(.caption.weight(.semibold)).buttonStyle(.bordered)
                        .disabled(!scene.canCreateAdditionalTerminal)
                        .accessibilityHint("Opens a separate SSH shell using Terminal 1's route and sign-in choices. Up to four terminals per machine.")
                        .accessibilityIdentifier("terminal.tab.new")
                }
                .padding(.horizontal, 12).padding(.vertical, 6)
            }
            .background(.regularMaterial)
            .onChange(of: scene.selectedSession?.id) { _, id in
                if let id { withAnimation { reader.scrollTo(id, anchor: .center) } }
            }
        }
    }

    private func earlierTerminalBar(_ machine: SceneMachine) -> some View {
        let records = scene.earlierTerminals(machine.id)
        return HStack(spacing: 10) {
            Image(systemName: "clock.badge.exclamationmark").foregroundStyle(.orange)
            VStack(alignment: .leading, spacing: 2) {
                Text("Earlier Terminals").font(.caption.weight(.semibold))
                Text(scene.terminalHistoryFailure ?? (records.contains(where: \.hasRecoveryNotice)
                    ? "Earlier input may have unknown remote effects."
                    : "Closed tabs have local history to review."))
                    .font(.caption2).foregroundStyle(.secondary)
            }
            Spacer(minLength: 4)
            if scene.reviewingTerminalHistory { ProgressView() }
            else {
                Button("Review") {
                    earlierTerminalsReview = .init(machineID: machine.id, machineName: machine.name, records: records)
                }
                .font(.caption.weight(.semibold)).buttonStyle(.bordered)
                .accessibilityIdentifier("terminal.tabs.history")
            }
        }
        .padding(.horizontal, 12).padding(.vertical, 8).background(.regularMaterial)
    }

    private func earlierTerminalReview(_ review: EarlierTerminalsReview) -> some View {
        NavigationStack {
            List {
                Section {
                    Text("These terminals are no longer open in this app. Reviewing their saved metadata does not reconnect or replay input.")
                    if review.records.contains(where: \.hasRecoveryNotice) {
                        Text("Some earlier input may have reached the remote machine. Its effects are unknown; check the machine before repeating a command.")
                    }
                    if let failure = scene.terminalHistoryFailure { Text(failure).foregroundStyle(.secondary) }
                } header: { Text(verbatim: review.machineName) }
                ForEach(review.records) { record in
                    VStack(alignment: .leading, spacing: 6) {
                        Text("Earlier terminal \(record.scope.terminalSessionID?.uuidString.prefix(8) ?? "")")
                            .font(.headline)
                        Text(verbatim: "Route: \(record.scope.routeID)").font(.caption).foregroundStyle(.secondary)
                        Text(record.hasRecoveryNotice ? "Input outcome is uncertain." : "The local connection was closed.")
                            .font(.callout)
                    }
                }
                Section {
                    Button("Acknowledge Reviewed History") {
                        scene.acknowledgeEarlierTerminals(review.records, machineID: review.machineID)
                        earlierTerminalsReview = nil
                    }
                    .disabled(scene.reviewingTerminalHistory || scene.selectedMachineID != review.machineID)
                    .accessibilityIdentifier("terminal.tabs.history.acknowledge")
                } footer: {
                    Text("This dismisses only the displayed local records. It does not claim that a remote command ran or that the remote shell exited.")
                }
            }
            .navigationTitle("Earlier Terminals").navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Done") { earlierTerminalsReview = nil } } }
        }
    }

    @ToolbarContentBuilder private func toolbar(_ machine: SceneMachine?) -> some ToolbarContent {
        ToolbarItemGroup(placement: .topBarTrailing) {
            Button("Fleet Overview", systemImage: "square.grid.2x2") { scene.beginOverview() }
                .keyboardShortcut("k", modifiers: .command)
                .accessibilityIdentifier("terminal.fleet.open")
            if let session = scene.selectedSession, session.phase == .live, session.terminal != nil {
                if session.historyReturnBookmark != nil {
                    Button("Return to Previous View", systemImage: "arrow.uturn.backward") { session.returnFromHistorySearch() }
                        .disabled(session.clipboardIsPresenting || session.localInputNeedsDecision)
                }
                Button("Search Output", systemImage: "magnifyingglass") {
                    if session.muxControl != nil { muxSearch = session } else { historyInspection = session.inspectHistory() }
                }
                    .disabled(session.clipboardIsPresenting || session.localInputNeedsDecision)
                    .keyboardShortcut("f", modifiers: .command)
                    .accessibilityIdentifier("terminal.history.open")
                Button("Paste", systemImage: "doc.on.clipboard") { session.paste() }
                    .disabled(session.isViewingOnly || session.clipboardIsPresenting || session.localInputNeedsDecision)
            }
            Menu {
                if let session = scene.selectedSession, session.phase == .live, session.attachedToMux {
                    Button("New Persistent Shell", systemImage: "plus.rectangle") { beginPersistentShell(session) }
                        .disabled(!session.canCreatePersistentShell)
                        .accessibilityIdentifier("terminal.mux.creation.new")
                    Divider()
                }
                if let session = scene.selectedSession, !session.attachedToMux,
                   scene.additionalSessions[session.target.machineID]?.isEmpty == false
                    || session.additionalTerminalTarget != nil {
                    Button("New Terminal", systemImage: "plus.rectangle") { scene.newTerminal() }
                        .disabled(!scene.canCreateAdditionalTerminal)
                    if !session.settled {
                        Button("End This Terminal", systemImage: "stop.circle", role: .destructive) { scene.endTerminal(session) }
                    }
                    if session.terminalSessionID != nil, session.settled {
                        if !scene.invalidatedTerminalSlots.contains(session.terminalSessionID!) {
                            Button("New Session in This Tab", systemImage: "arrow.clockwise") { scene.restartTerminal(session) }
                        }
                        Button("Remove This Tab", systemImage: "xmark.rectangle", role: .destructive) { scene.removeTerminal(session) }
                            .accessibilityIdentifier("terminal.tab.remove")
                    }
                    Divider()
                }
                if let session = scene.selectedSession, session.remoteFilesTarget != nil {
                    Button("Remote Files", systemImage: "folder") { scene.beginRemoteFiles() }
                        .disabled(scene.openingSplit || session.clipboardIsPresenting || session.localInputNeedsDecision)
                        .accessibilityIdentifier("terminal.files.open")
                }
                Button("Next Attention", systemImage: "exclamationmark.circle") { scene.showNextAttention() }
                    .disabled(!scene.hasAttentionElsewhere)
                    .accessibilityIdentifier("terminal.attention.menu")
                Button("Hardware Keyboard…", systemImage: "keyboard") {
                    scene.beginOverview(panel: .hardwareKeyboard)
                }
                .disabled(scene.openingSplit)
                .accessibilityIdentifier("terminal.keyboard.settings")
                if scene.splitLayout.isSplit {
                    Button("Close Split", systemImage: "rectangle") { scene.closeSplit() }
                        .accessibilityIdentifier("terminal.split.close")
                }
                if scene.splitAvailable, scene.selectedSession?.terminal != nil {
                    Menu("Split with Another Machine", systemImage: "rectangle.split.2x1") {
                        ForEach(scene.navigationMachines.filter { $0.id != scene.selectedMachineID }) { other in
                            Button(other.name) { scene.split(with: other.id) }
                                .accessibilityIdentifier("terminal.split.choose.\(other.id)")
                        }
                    }
                    .disabled(scene.openingSplit || scene.selectedSession?.clipboardIsPresenting == true
                        || scene.selectedSession?.paneNavigationBusy == true)
                    .accessibilityIdentifier("terminal.split.open")
                }
                if let session = scene.selectedSession, session.phase == .live, let terminal = session.terminal {
                    // ⇧⌘D is the canvas's key command: a SwiftUI shortcut on a
                    // button inside this menu never fired (UI test 3).
                    Button("Compose Command", systemImage: "square.and.pencil") { session.composeCommand() }
                        .disabled(session.isViewingOnly || session.clipboardIsPresenting || session.localInputNeedsDecision)
                        .accessibilityIdentifier("terminal.command.open")
                    Button("Select Output", systemImage: "doc.on.doc") { session.inspectOutput() }
                        .disabled(session.clipboardIsPresenting || session.localInputNeedsDecision)
                    // Pinching the terminal does the same.
                    Menu("Text Size", systemImage: "textformat.size") {
                        if session.keepsSharedMuxSize {
                            Button("Fit Entire Pane", systemImage: session.prefersReadableViewport ? "arrow.down.right.and.arrow.up.left" : "checkmark") {
                                session.setReadableViewport(false)
                            }
                            .disabled(!session.canAdjustLocalViewport)
                            .accessibilityIdentifier("terminal.viewport.fit")
                            Button("Readable Size", systemImage: session.prefersReadableViewport ? "checkmark" : "text.magnifyingglass") {
                                session.setReadableViewport(true)
                            }
                            .disabled(!session.canAdjustLocalViewport)
                            .accessibilityIdentifier("terminal.viewport.readable")
                            Button("Reveal Cursor", systemImage: "scope") { session.revealViewportCursor() }
                                .disabled(!session.canAdjustLocalViewport || !session.prefersReadableViewport)
                                .accessibilityIdentifier("terminal.viewport.cursor")
                            Divider()
                        }
                        Button("Bigger", systemImage: "plus.magnifyingglass") { terminal.biggerText() }
                        Button("Smaller", systemImage: "minus.magnifyingglass") { terminal.smallerText() }
                        Button("Actual Size", systemImage: "1.magnifyingglass") { terminal.actualSize() }
                    }
                }
                if themes != nil, machine != nil {
                    Button("Theme…", systemImage: "paintpalette") { pickingTheme = true }
                        .accessibilityIdentifier("scene.theme")
                }
                if let machine, machine.target != nil, scene.selectedSession?.terminalSessionID != nil {
                    Button("Disconnect \(machine.name)", systemImage: "xmark.circle", role: .destructive) {
                        scene.disconnect(machine.id)
                    }
                } else if let machine, machine.target != nil {
                    switch scene.status(of: machine.id) {
                    case .disconnected, .attention:
                        if scene.requiresNewSession(machine.id), !scene.disconnected.contains(machine.id) {
                            Button(scene.sessions[machine.id]?.route.mux == true ? "Reconnect Existing Panes" : "New Session",
                                   systemImage: "plus.rectangle") { scene.openTerminal(machine.id) }
                        } else {
                            Button("Connect \(machine.name)", systemImage: "bolt.horizontal") { scene.connect(machine.id) }
                        }
                    case .ready:
                        Button(scene.sessions[machine.id]?.route.mux == true ? "Open Existing Panes"
                               : (scene.requiresNewSession(machine.id) ? "New Session" : "Open Terminal"),
                               systemImage: "terminal") { scene.openTerminal(machine.id) }
                        Button("Disconnect \(machine.name)", systemImage: "xmark.circle", role: .destructive) {
                            scene.disconnect(machine.id)
                        }
                    default:
                        Button("Disconnect \(machine.name)", systemImage: "xmark.circle", role: .destructive) {
                            scene.disconnect(machine.id)
                        }
                    }
                }
                Button("Disconnect All", systemImage: "power", role: .destructive) { scene.disconnectAll() }
            } label: { Label("More", systemImage: "ellipsis.circle") }
                .accessibilityIdentifier("terminal.more")
        }
    }

    private func splitHeading(_ machine: SceneMachine) -> some View {
        let focused = machine.id == scene.selectedMachineID
        return Button {
            if focused { Task { await scene.activateKeyboard() } }
            else { scene.select(machine.id) }
        } label: {
            HStack(spacing: 8) {
                Image(systemName: focused ? "scope" : "eye")
                Text(verbatim: machine.name).fontWeight(.semibold).lineLimit(1)
                Spacer(minLength: 0)
                Text(focused ? "Focused" : "Tap to focus").font(.caption2)
            }
            .font(.caption).padding(.horizontal, 12).frame(height: 36)
            .background(focused ? Color.accentColor.opacity(0.18) : Color.primary.opacity(0.06))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain).frame(maxWidth: .infinity)
        .accessibilityLabel("\(machine.name), \(focused ? "focused terminal" : "observing, activate to type")")
        .accessibilityIdentifier("terminal.split.focus.\(machine.id)")
    }

    private func muxAccessBar(_ session: TerminalSessionController) -> some View {
        HStack(spacing: 12) {
            VStack(alignment: .leading, spacing: 2) {
                Label(session.isViewingOnly ? "View Only" : "Using Shared Terminal",
                      systemImage: session.isViewingOnly ? "eye" : "keyboard").font(.caption.weight(.semibold))
                Text(session.isViewingOnly ? "Shared size · Input is off"
                    : session.muxAccessMode.controlsSize ? "Phone size · Input is on" : "Shared size · Input is on")
                    .font(.caption2).foregroundStyle(.secondary)
                if session.keepsSharedMuxSize && session.prefersReadableViewport {
                    Text("Readable view · Drag to pan; pinch to zoom")
                        .font(.caption2).foregroundStyle(.secondary)
                        .accessibilityIdentifier("terminal.viewport.status")
                }
            }
            Spacer(minLength: 4)
            Button(session.isViewingOnly ? "Use Terminal" : "Change…") {
                useTerminalReview = session
            }
            .font(.caption.weight(.semibold)).buttonStyle(.bordered)
            .disabled(session.clipboardIsPresenting || session.localInputNeedsDecision
                || scene.changingMuxModes.contains(session.target.machineID))
            .accessibilityIdentifier("terminal.mux.access.change")
        }
        .padding(.horizontal, 12).padding(.vertical, 6).background(.thinMaterial)
        .accessibilityElement(children: .contain).accessibilityIdentifier("terminal.mux.access")
    }

    private func inputRecoveryBanner(_ recovery: TerminalKeyboardRecovery, session: TerminalSessionController) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Label("Keyboard paused", systemImage: "keyboard.badge.ellipsis").font(.headline)
            Text(verbatim: "\(session.target.machineName) · \(session.route.id)").font(.caption).foregroundStyle(.secondary)
            Text("\(recovery.accounting.pendingEvents) pending local events · \(recovery.accounting.pendingUTF8Bytes) UTF-8 bytes")
                .font(.caption).monospacedDigit()
            Text(recovery.canRetry
                ? "Retry sends only the pending local input to this terminal. Earlier input may already have been sent; it will not be replayed."
                : "This input cannot be safely retried. Reset the keyboard by discarding pending local input. Earlier admitted input is unchanged.")
                .font(.caption)
            if recovery.accounting.compositionUTF8Bytes > 0 {
                Text("Discard also removes the unfinished local composition.").font(.caption)
            }
            if let problem = session.interactionProblem { Text(verbatim: problem).font(.caption).foregroundStyle(.orange) }
            ViewThatFits(in: .horizontal) {
                HStack {
                    inputRecoveryActions(recovery, session: session)
                }
                VStack(alignment: .leading) {
                    inputRecoveryActions(recovery, session: session)
                }
            }
        }
        .padding(14).frame(maxWidth: 520, alignment: .leading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16)).padding()
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("terminal.input.recovery")
    }

    @ViewBuilder private func inputRecoveryActions(_ recovery: TerminalKeyboardRecovery, session: TerminalSessionController) -> some View {
        if recovery.canRetry {
            Button("Retry Pending Input") { session.retryLocalInput(recovery) }
                .buttonStyle(.borderedProminent).disabled(session.resolvingLocalInput)
                .accessibilityIdentifier("terminal.input.retry")
        }
        Button(recovery.accounting.pendingEvents == 0 ? "Reset Keyboard" : "Discard Pending Input", role: .destructive) {
            session.discardLocalInput(recovery)
        }
        .buttonStyle(.bordered).disabled(session.resolvingLocalInput)
        .accessibilityIdentifier("terminal.input.discard")
    }

    private func problemBanner(_ text: String, dismiss: @escaping () -> Void) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
            Text(verbatim: text).font(.callout).frame(maxWidth: .infinity, alignment: .leading)
            Button("Dismiss", systemImage: "xmark", action: dismiss).labelStyle(.iconOnly)
        }
        .padding(14).background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16)).padding()
    }

    @ViewBuilder private func card(for machine: SceneMachine) -> some View {
        let accent = Color(ftHex: scene.themes[machine.id]?.accent ?? "#3B82F6")
        if let mode = scene.changingMuxMode(machine.id) {
            connecting(machine, accent: accent, detail: mode == .interactive
                ? "Fitting the shared pane to this phone…"
                : mode == .inputOnly ? "Enabling input at the shared size…" : "Reconnecting to view only…")
        } else if scene.disconnected.contains(machine.id), scene.displayedSession(machine.id)?.terminalSessionID == nil {
            disconnectedCard(machine, accent: accent)
        } else if scene.capacityBlockedMachineID == machine.id {
            capacityCard(machine, accent: accent)
        } else if scene.preparationWasReleased(machine.id), let session = scene.displayedSession(machine.id), session.terminalSessionID == nil {
            if !session.settled, let failure = session.cleanupFailure {
                sceneCard(symbol: "exclamationmark.triangle", title: "Sign-in cleanup needs attention",
                          message: failure, accent: accent) {
                    Button("Retry Cleanup") { scene.retryCapacityCleanup(expectedControllerID: session.id) }
                }
            } else if scene.status(of: machine.id) == .connecting {
                connecting(machine, accent: accent, detail: session.settled ? "Waiting for a connection slot" : "Closing an unused sign-in…")
            } else {
                sceneCard(symbol: "bolt.horizontal.circle", title: "Unused sign-in released",
                          message: "This automatically prepared connection made room for another machine. No terminal was opened or closed.",
                          accent: accent) {
                    Button("Connect") { scene.connect(machine.id) }.buttonStyle(.borderedProminent)
                }
            }
        } else if let session = scene.displayedSession(machine.id) {
            switch session.phase {
            case .idle, .connecting:
                if session.settled, session.terminalSessionID != nil {
                    endedCard(machine, message: "This terminal was not opened. Choose New Session to try again.",
                              session: session, accent: accent)
                } else { connecting(machine, accent: accent, detail: Self.via(session.route)) }
            case .ready:
                readyCard(machine, session: session, accent: accent)
            case .live:
                if session.terminal == nil {
                    if let mux = session.muxState, mux.shown == nil {
                        if mux.panes.isEmpty {
                            sceneCard(symbol: "terminal", title: "No Persistent Shells",
                                      message: "Connected to this machine's mux. Create its first shell, or wait for a pane opened by another client.", accent: accent) {
                                Button("New Persistent Shell") { beginPersistentShell(session) }
                                    .buttonStyle(.borderedProminent).disabled(!session.canCreatePersistentShell)
                                    .accessibilityIdentifier("terminal.mux.creation.empty")
                                if !session.muxAccessMode.permitsShellCreation {
                                    Text("Choose Type and Fit to Phone in the access menu to enable shell creation.")
                                        .font(.caption)
                                }
                            }
                        } else {
                            sceneCard(symbol: "rectangle.split.2x1", title: "Select a Pane",
                                      message: mux.notice ?? "Choose a pane above to continue on this machine.", accent: accent) {}
                        }
                    } else { ProgressView().controlSize(.large).tint(accent) }
                }
            case let .couldNotConnect(help):
                if scene.status(of: machine.id) == .connecting {
                    connecting(machine, accent: accent, detail: "Reconnecting…")
                } else if scene.hostKeyMismatch(for: session) != nil {
                    hostKeyChangedCard(session, accent: accent)
                } else {
                    connectionHelp(help, settled: session.settled, automaticallyRetriesTailscale: session.terminalSessionID == nil) {
                        scene.retryConnection(session)
                    }
                }
            case let .ended(message), let .failed(message):
                switch session.terminalSessionID == nil ? scene.status(of: machine.id) : .attention {
                case .disconnected:
                    disconnectedCard(machine, accent: accent)
                case .connecting:
                    connecting(machine, accent: accent, detail: "Reconnecting…")
                default:
                    endedCard(machine, message: message, session: session, accent: accent)
                }
            }
        } else if machine.target != nil {
            if scene.status(of: machine.id) == .disconnected { disconnectedCard(machine, accent: accent) }
            else { connecting(machine, accent: accent, detail: "Waiting to connect") }
        } else {
            sceneCard(symbol: "network.slash", title: "Not available here", message: machine.unavailable, accent: accent) { EmptyView() }
        }
    }

    private func connecting(_ machine: SceneMachine, accent: Color, detail: String) -> some View {
        VStack(spacing: 14) {
            ProgressView().controlSize(.large).tint(accent)
            Text("Connecting to \(machine.name)").font(.title3.weight(.semibold))
            Text(verbatim: detail).font(.caption).foregroundStyle(.white.opacity(0.7))
        }
        .foregroundStyle(.white).padding(28)
        .background(.black.opacity(0.55), in: RoundedRectangle(cornerRadius: 22))
    }

    private func capacityCard(_ machine: SceneMachine, accent: Color) -> some View {
        let releasing = scene.capacityReleaseMachineID
        let failed = scene.failedCapacityRelease
        return sceneCard(symbol: "network", title: releasing == nil ? "Connection slots are full" : "Making room for \(machine.name)",
            message: releasing == nil
                ? "This workspace is using its \(scene.connectionLimit) retained connection slots. Live terminals and explicit sign-ins are never closed automatically. Choose an existing connection to close; sessions still settling keep their slots."
                : "Closing one unused automatic sign-in. \(machine.name) will connect only after cleanup finishes.", accent: accent) {
            if releasing != nil { ProgressView().tint(accent) }
            if let failed, scene.sessions[failed.machineID]?.cleanupFailure != nil {
                Text("An unused sign-in could not finish cleanup. No further connections will be released automatically.")
                    .font(.caption)
                Button("Retry Cleanup") { scene.retryCapacityCleanup(expectedControllerID: failed.id) }
                    .buttonStyle(.bordered)
                    .disabled(releasing != nil)
            }
            Menu("Close a Connection…") {
                ForEach(scene.machines) { other in
                    ForEach(scene.terminalControllers(other.id).filter { !$0.settled }) { owned in
                        Button(role: .destructive) { scene.endTerminal(owned) } label: {
                            Text(verbatim: "\(other.name) · \(scene.terminalTitle(owned)) · \(owned.phase == .live ? "disconnect terminal" : "close connection")")
                        }
                    }
                }
            }.buttonStyle(.bordered).disabled(releasing != nil)
                .accessibilityIdentifier("terminal.capacity.manage")
        }
    }

    private func disconnectedCard(_ machine: SceneMachine, accent: Color) -> some View {
        sceneCard(symbol: "bolt.horizontal.circle", title: "\(machine.name) is disconnected",
                  message: "Connect to sign in to this machine. Its route decides when to open a terminal.", accent: accent) {
            Button("Connect", systemImage: "bolt.horizontal") { scene.connect(machine.id) }
                .buttonStyle(.borderedProminent).controlSize(.large).tint(accent)
        }
    }

    private func readyCard(_ machine: SceneMachine, session: TerminalSessionController, accent: Color) -> some View {
        let ended = session.terminalSessionID == nil && scene.requiresNewSession(machine.id)
        let message = session.route.mux
            ? "The connection is ready to attach to this machine's existing FrankenTerm panes."
            : (ended
                ? "The previous terminal has ended. Choose New Session to open a terminal; an ended shell cannot be restored."
                : "This machine is signed in. No terminal has been opened on this connection.")
        let action = session.route.mux ? "Open Existing Panes" : (ended ? "New Session" : "Open Terminal")
        return sceneCard(symbol: "checkmark.shield", title: "Signed in to \(machine.name)", message: message, accent: accent) {
            Button(action, systemImage: "terminal") { scene.openTerminal(session) }
                .buttonStyle(.borderedProminent).controlSize(.large).tint(accent)
                .accessibilityIdentifier("terminal.ready.open")
        }
    }

    private func endedCard(_ machine: SceneMachine, message: String, session: TerminalSessionController,
                           accent: Color) -> some View {
        // Viewing a pane too large to view would end the same way again;
        // only the phone's own size can show it.
        let fits = session.paneTooLargeToView
        let routeChanged = session.terminalSessionID.map { scene.invalidatedTerminalSlots.contains($0) } == true
        return sceneCard(symbol: "moon.zzz", title: "Session ended", message: fits
            ? message + " Use at Phone Size fits it to this phone while you use it; the desktop gets its size back when you leave."
            : routeChanged ? "This tab used the earlier connection choice. Remove it and open a new terminal after connecting with the new route."
            : message, accent: accent) {
            Group {
                if routeChanged {
                    Button("Remove This Tab", systemImage: "xmark.rectangle") { scene.removeTerminal(session) }
                } else if fits {
                    Button("Use at Phone Size", systemImage: "arrow.down.right.and.arrow.up.left") {
                        scene.takePaneAtPhoneSize(machine.id, from: session)
                    }
                    .accessibilityIdentifier("terminal.ended.fit")
                } else {
                    Button(session.route.mux ? "Reconnect Existing Panes" : "New Session", systemImage: "arrow.clockwise") {
                        scene.openTerminal(session)
                    }
                }
            }
            .buttonStyle(.borderedProminent).controlSize(.large).tint(accent)
            .disabled(!session.settled)
            if let output = session.retainedOutput {
                Button("View Last Output", systemImage: "doc.text.magnifyingglass") { retainedOutput = output }
                    .buttonStyle(.bordered)
                    .accessibilityIdentifier("terminal.retained.open")
            }
            if let failure = session.cleanupFailure {
                Text("Local cleanup is incomplete: \(failure)").font(.caption).multilineTextAlignment(.center)
                Button("Retry Cleanup") { Task { await session.retryCleanup() } }.buttonStyle(.bordered)
            }
        }
    }

    private func sceneCard<Actions: View>(symbol: String, title: String, message: String, accent: Color,
                                          @ViewBuilder actions: () -> Actions) -> some View {
        VStack(spacing: 14) {
            Image(systemName: symbol).font(.system(size: 40)).foregroundStyle(accent).symbolRenderingMode(.hierarchical)
            Text(verbatim: title).font(.title3.bold()).multilineTextAlignment(.center)
            Text(verbatim: message).font(.callout).multilineTextAlignment(.center).foregroundStyle(.white.opacity(0.8))
            actions()
        }
        .padding(24).frame(maxWidth: 420).foregroundStyle(.white)
        .background(.black.opacity(0.72), in: RoundedRectangle(cornerRadius: 22))
        .overlay(RoundedRectangle(cornerRadius: 22).stroke(accent.opacity(0.35)))
        .padding()
    }

    /// Phone connectivity describes the first hop. A private destination behind
    /// a public jump host does not require this phone to share its local network.
    static func via(_ route: TerminalSessionRoute) -> String {
        func endpoint(_ host: String, _ port: UInt16) -> String {
            let address = host.contains(":") && !host.hasPrefix("[") ? "[\(host)]" : host
            return "\(address):\(port)"
        }
        let path: String
        if let jump = route.jumpHost {
            path = "\(endpoint(route.host, route.port)) via \(endpoint(jump, route.networkPort))"
        } else {
            path = route.port == 22 ? route.host : endpoint(route.host, route.port)
        }
        switch WorkspaceRouteNetwork(host: route.networkHost) {
        case .tailnet: return "over Tailscale · \(path)"
        case .localNetwork: return "on the local network · \(path)"
        case .internet: return path
        }
    }

    private func hostKeyChangedCard(_ session: TerminalSessionController, accent: Color) -> some View {
        sceneCard(symbol: "exclamationmark.shield", title: "Host Key Changed",
            message: "A peer on this machine's connection path presented a different host key. Review the expected and presented fingerprints before updating the workspace.", accent: accent) {
            Button("Review Host Identity", systemImage: "checkmark.shield") {
                guard scene.hostKeyMismatch(for: session) != nil else { return }
                hostKeyReview = session
            }.buttonStyle(.borderedProminent)
                .accessibilityIdentifier("terminal.host-key.review")
            Button("Try Again", systemImage: "arrow.clockwise") {
                guard scene.hostKeyMismatch(for: session) != nil else { return }
                scene.connect(session.target.machineID)
            }.buttonStyle(.bordered).disabled(!session.settled)
            Text("Retry checks the same permitted keys. To change paths, return to the machine page and choose Choose Route and Sign-In.")
                .font(.caption).multilineTextAlignment(.center)
        }
    }

    /// Nothing ran, so there is no session to review: the reason, the one
    /// change that fixes it, and Try Again once the attempt has settled.
    private func connectionHelp(_ help: WorkspaceConnectionHelp, settled: Bool,
                                automaticallyRetriesTailscale: Bool,
                                retry: @escaping () -> Void) -> some View {
        let retry = Button("Try Again", systemImage: "arrow.clockwise", action: retry).disabled(!settled)
        return VStack(spacing: 14) {
            Image(systemName: help.remedy == .turnOnTailscale ? "network.badge.shield.half.filled" : "wifi.exclamationmark")
                .font(.system(size: 40)).symbolRenderingMode(.hierarchical)
            Text(verbatim: help.title).font(.title3.bold()).multilineTextAlignment(.center)
            Text(verbatim: help.message).font(.callout).multilineTextAlignment(.center)
                .foregroundStyle(.white.opacity(0.85))
            switch help.remedy {
            case .turnOnTailscale:
                Button("Open Tailscale", systemImage: "arrow.up.forward.app") { Self.openTailscale() }
                    .buttonStyle(.borderedProminent).controlSize(.large)
                retry.buttonStyle(.bordered)
                Text(automaticallyRetriesTailscale
                    ? "FrankenTerm tries again on its own when you come back with Tailscale on."
                    : "Return here and choose Try Again after turning Tailscale on.")
                    .font(.caption).multilineTextAlignment(.center).foregroundStyle(.white.opacity(0.7))
            case .allowLocalNetwork:
                retry.buttonStyle(.borderedProminent).controlSize(.large)
                Button("Open Settings", systemImage: "gear") {
                    if let url = URL(string: UIApplication.openSettingsURLString) { UIApplication.shared.open(url) }
                }.buttonStyle(.bordered)
            case .none:
                retry.buttonStyle(.borderedProminent).controlSize(.large)
            }
        }.padding(24).frame(maxWidth: 420).foregroundStyle(.white)
            .background(.black.opacity(0.72), in: RoundedRectangle(cornerRadius: 22)).padding()
            .accessibilityElement(children: .contain)
    }

    /// The Tailscale app when it is installed, otherwise its App Store page.
    static func openTailscale() {
        guard let app = URL(string: "tailscale://"),
              let store = URL(string: "https://apps.apple.com/app/id1470499037") else { return }
        UIApplication.shared.open(app) { opened in if !opened { UIApplication.shared.open(store) } }
    }
}

/// Remote paths are byte identities. Swift String equality intentionally
/// folds canonical Unicode spellings, which a remote filesystem need not do.
enum TerminalRemoteFilesPath {
    static func same(_ lhs: String, _ rhs: String) -> Bool { lhs.utf8.elementsEqual(rhs.utf8) }
    static func validName(_ name: String) -> Bool {
        !name.isEmpty && name != "." && name != ".." && name.utf8.count <= 255
            && !name.utf8.contains(0) && !name.utf8.contains(47)
    }
    static func joining(_ directory: String, name: String) throws -> String {
        guard validName(name), directory.utf8.first == 47 else { throw NativeSFTPError.invalidPath }
        let path = directory + (directory.utf8.last == 47 ? "" : "/") + name
        guard path.utf8.count <= 4_096, !path.utf8.contains(0) else { throw NativeSFTPError.invalidPath }
        return path
    }
    static func parent(of path: String) -> String {
        var bytes = Array(path.utf8)
        while bytes.count > 1, bytes.last == 47 { bytes.removeLast() }
        guard let slash = bytes.lastIndex(of: 47), slash > 0 else { return "/" }
        return String(decoding: bytes[..<slash], as: UTF8.self)
    }
}

/// A lease on one app-owned, protected staging directory. Export controllers
/// retain the lease until their system UI has finished reading the file.
final class TerminalFileStorage: @unchecked Sendable {
    let directory: URL
    let url: URL
    init(name: String) throws {
        guard TerminalRemoteFilesPath.validName(name) else { throw TerminalFileStaging.Failure.invalidName }
        let cache = try FileManager.default.url(for: .cachesDirectory, in: .userDomainMask,
                                               appropriateFor: nil, create: true)
        var directory = cache.appendingPathComponent("RemoteFiles-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false,
            attributes: [.posixPermissions: 0o700, .protectionKey: FileProtectionType.complete])
        do {
            var values = URLResourceValues(); values.isExcludedFromBackup = true
            try directory.setResourceValues(values)
        } catch {
            try? FileManager.default.removeItem(at: directory)
            throw error
        }
        self.directory = directory
        url = directory.appendingPathComponent(name, isDirectory: false)
    }
    deinit { try? FileManager.default.removeItem(at: directory) }
}

struct TerminalLocalFile: Identifiable, Sendable {
    let id = UUID()
    let storage: TerminalFileStorage
    let name: String
    let bytes: UInt64
}

/// Freeze a Files-provider selection before review. No provider URL, symlink,
/// changing source or unbounded Data buffer reaches the upload operation.
enum TerminalFileStaging {
    enum Failure: Error, LocalizedError {
        case invalidSource, invalidName, tooLarge, changed, storage
        var errorDescription: String? {
            switch self {
            case .invalidSource: "Choose a regular file from Files. Folders and symbolic links cannot be uploaded."
            case .invalidName: "Use a filename of at most 255 UTF-8 bytes without a slash or NUL."
            case .tooLarge: "Remote Files supports files up to 256 MiB."
            case .changed: "The selected file changed while it was being prepared. Choose it again."
            case .storage: "The selected file could not be prepared in protected local storage."
            }
        }
    }

    private final class ReadAccess: @unchecked Sendable {
        let coordinator = NSFileCoordinator(filePresenter: nil)
        private let lock = NSLock()
        private var cancelled = false
        func cancel() {
            lock.lock(); cancelled = true; lock.unlock()
            coordinator.cancel()
        }
        func check() throws {
            lock.lock(); let cancelled = cancelled; lock.unlock()
            if cancelled { throw CancellationError() }
            try Task.checkCancellation()
        }
    }

    static func copy(_ source: URL,
                     onProgress: @escaping @Sendable (UInt64, UInt64?) async -> Void) async throws -> TerminalLocalFile {
        let scoped = source.startAccessingSecurityScopedResource()
        defer { if scoped { source.stopAccessingSecurityScopedResource() } }
        guard source.isFileURL, !source.path.utf8.contains(0) else { throw Failure.invalidSource }
        try Task.checkCancellation()
        let access = ReadAccess()
        return try await withTaskCancellationHandler {
            var coordinationError: NSError?
            var result: Result<TerminalLocalFile, any Error>?
            // A Files provider may materialize or move its representation.
            // Keep the complete bounded read inside its coordinated accessor.
            access.coordinator.coordinate(readingItemAt: source, options: [], error: &coordinationError) { coordinatedURL in
                result = Result {
                    try copyCoordinated(coordinatedURL, access: access) { bytes, total in
                        Task { await onProgress(bytes, total) }
                    }
                }
            }
            try access.check()
            if coordinationError != nil { throw Failure.invalidSource }
            guard let result else { throw Failure.invalidSource }
            let file = try result.get()
            await onProgress(file.bytes, file.bytes)
            return file
        } onCancel: { access.cancel() }
    }

    private static func copyCoordinated(_ source: URL, access: ReadAccess,
        onProgress: @Sendable (UInt64, UInt64?) -> Void) throws -> TerminalLocalFile {
        try access.check()
        guard source.isFileURL, !source.path.utf8.contains(0) else { throw Failure.invalidSource }
        let descriptor = source.withUnsafeFileSystemRepresentation { path -> Int32 in
            guard let path else { return -1 }
            return open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK)
        }
        guard descriptor >= 0 else { throw Failure.invalidSource }
        let input = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
        defer { try? input.close() }
        var before = stat()
        guard fstat(descriptor, &before) == 0, before.st_size >= 0,
              before.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG) else { throw Failure.invalidSource }
        let expected = UInt64(before.st_size)
        guard expected <= WorkspaceSFTPClient.maximumFileBytes else { throw Failure.tooLarge }
        let name = source.lastPathComponent
        let storage = try TerminalFileStorage(name: name)
        let outputDescriptor = storage.url.withUnsafeFileSystemRepresentation { path -> Int32 in
            guard let path else { return -1 }
            return open(path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, mode_t(0o600))
        }
        guard outputDescriptor >= 0 else { throw Failure.storage }
        let output = FileHandle(fileDescriptor: outputDescriptor, closeOnDealloc: true)
        defer { try? output.close() }
        try FileManager.default.setAttributes([.protectionKey: FileProtectionType.complete],
                                              ofItemAtPath: storage.url.path)
        var count: UInt64 = 0, reported: UInt64 = 0
        onProgress(0, expected)
        while true {
            try access.check()
            let bytes: Data
            do { bytes = try input.read(upToCount: 64 * 1_024) ?? Data() }
            catch { throw Failure.storage }
            if bytes.isEmpty { break }
            guard UInt64(bytes.count) <= expected - min(count, expected),
                  UInt64(bytes.count) <= WorkspaceSFTPClient.maximumFileBytes - count else { throw Failure.changed }
            do { try output.write(contentsOf: bytes) }
            catch { throw Failure.storage }
            count += UInt64(bytes.count)
            // Bound cross-actor updates even when local reads are much faster
            // than screen rendering. At most 256 chunk updates per file.
            if count - reported >= 1_024 * 1_024 {
                reported = count; onProgress(count, expected)
            }
        }
        try access.check()
        var after = stat()
        guard fstat(descriptor, &after) == 0, count == expected,
              before.st_dev == after.st_dev, before.st_ino == after.st_ino,
              before.st_mode == after.st_mode, before.st_size == after.st_size,
              before.st_mtimespec.tv_sec == after.st_mtimespec.tv_sec,
              before.st_mtimespec.tv_nsec == after.st_mtimespec.tv_nsec,
              before.st_ctimespec.tv_sec == after.st_ctimespec.tv_sec,
              before.st_ctimespec.tv_nsec == after.st_ctimespec.tv_nsec else { throw Failure.changed }
        guard fsync(outputDescriptor) == 0 else { throw Failure.storage }
        try output.close()
        return .init(storage: storage, name: name, bytes: count)
    }
}

/// Owns a dedicated file connection and its exact reviewed terminal source.
/// Cancellation retires that owner; no retry can replay a remote create.
@MainActor
@Observable
final class TerminalRemoteFilesController {
    enum Phase: Equatable { case connecting, listing, browsing, staging, downloading, uploading, closing, closed }
    struct UploadReview: Identifiable {
        let id = UUID()
        let file: TerminalLocalFile
        let directory: String
        let listingID: UUID
    }
    let id = UUID()
    let target: TerminalRemoteFilesTarget
    let source: TerminalSessionController
    let connections: MobileConnections
    private(set) var phase: Phase = .connecting
    private(set) var listing: NativeSFTPDirectoryListing?
    private(set) var listingID = UUID()
    private(set) var uploadReview: UploadReview?
    private(set) var completedDownload: TerminalLocalFile?
    private(set) var transferredBytes: UInt64 = 0
    private(set) var totalBytes: UInt64?
    private(set) var transferPath: String?
    private(set) var message: String?
    private(set) var cleanupFailure: String?
    private(set) var foreground = true
    private(set) var settled = false
    @ObservationIgnored private weak var scene: WorkspaceTerminalScene?
    @ObservationIgnored private var connection: WorkspaceSSHConnection?
    @ObservationIgnored private var client: WorkspaceSFTPClient?
    @ObservationIgnored private var work: (id: UUID, task: Task<Void, Never>)?
    @ObservationIgnored private var cleanup: (id: UUID, task: Task<Void, Never>)?
    @ObservationIgnored private var retired = false
    @ObservationIgnored private var openingPending = false
    @ObservationIgnored private var importRequest: (id: UUID, directory: String, listingID: UUID)?
    @ObservationIgnored private var activeUploadPath: String?

    init(target: TerminalRemoteFilesTarget, source: TerminalSessionController,
         scene: WorkspaceTerminalScene, connections: MobileConnections) {
        self.target = target; self.source = source; self.scene = scene; self.connections = connections
        message = scene.fileTransferNotice(for: target)
    }

    var visible: Bool { foreground && scene?.remoteFilesAreCurrent(self) == true }
    var canBrowse: Bool { visible && !retired && phase == .browsing && work == nil && uploadReview == nil }
    var canReview: Bool { visible && !retired && phase == .browsing && work == nil && uploadReview != nil }
    var canReconnect: Bool { visible && retired && settled }
    var authenticationPrompt: MobileSSHAuthenticationSheetRequest? {
        visible ? connections.fileAuthenticationPrompt(for: id, target: target) : nil
    }

    func start() {
        guard visible, !retired, work == nil, connection == nil else { return }
        perform(.connecting) { owner, _ in
            let connection = try await owner.connections.connectFiles(owner.target, ownerID: owner.id)
            // Retain before any cancellation/lifecycle check: a late successful
            // handshake still occupies the manager and must be closed by ID.
            owner.connection = connection
            try owner.checkCurrent()
            guard TerminalRemoteFilesPath.same(connection.host, owner.target.host),
                  TerminalRemoteFilesPath.same(connection.user, owner.target.user),
                  connection.port == owner.target.port else { throw WorkspaceSSHConnectionError.stale }
            if !owner.visible { owner.openingPending = true; return }
            try await owner.openAndList(connection)
        }
    }

    func foregroundChanged(_ active: Bool) {
        foreground = active
        if !active { importRequest = nil }
        guard active, openingPending, !retired, work == nil, let connection, visible else { return }
        openingPending = false
        perform(.connecting) { owner, _ in try await owner.openAndList(connection) }
    }

    private func openAndList(_ connection: WorkspaceSSHConnection) async throws {
        try checkCurrent(requireForeground: true)
        let files: WorkspaceSFTPClient
        if let client { files = client }
        else {
            files = try await connection.openFiles()
            client = files
        }
        try checkCurrent()
        // A Face ID interruption may finish the admitted handshake. Only
        // foreground demand starts a new SFTP operation.
        if !visible { openingPending = true; return }
        phase = .listing
        try await receiveListing(files, path: ".")
    }

    private func receiveListing(_ client: WorkspaceSFTPClient, path: String) async throws {
        try checkCurrent(requireForeground: true)
        let result = try await client.listDirectory(path: path, limit: 512)
        try checkCurrent()
        listing = result; listingID = UUID(); phase = .browsing
    }

    func browse(_ path: String, listingID expected: UUID? = nil) {
        guard canBrowse, let client else { return }
        guard expected == nil || expected == listingID else { return }
        guard !path.isEmpty, path.utf8.count <= 4_096, !path.utf8.contains(0) else {
            message = NativeSFTPError.invalidPath.localizedDescription; return
        }
        perform(.listing) { owner, _ in try await owner.receiveListing(client, path: path) }
    }

    func enter(_ entry: NativeSFTPDirectoryEntry, listingID expected: UUID) {
        guard canBrowse, listingID == expected, let listing,
              listing.entries.contains(where: { $0.id == entry.id && $0.attributes.kind == .directory }) else { return }
        do { browse(try TerminalRemoteFilesPath.joining(listing.path, name: entry.name)) }
        catch { message = error.localizedDescription }
    }

    func download(_ entry: NativeSFTPDirectoryEntry, listingID expected: UUID) {
        guard canBrowse, listingID == expected, let listing, let client,
              listing.entries.contains(where: { $0.id == entry.id && $0.attributes.kind == .regularFile }) else { return }
        if let size = entry.attributes.size, size > WorkspaceSFTPClient.maximumFileBytes {
            message = TerminalFileStaging.Failure.tooLarge.localizedDescription; return
        }
        do {
            let path = try TerminalRemoteFilesPath.joining(listing.path, name: entry.name)
            let storage = try TerminalFileStorage(name: entry.name)
            completedDownload = nil; transferPath = path
            perform(.downloading) { owner, workID in
                let receipt = try await client.download(path: path, to: storage.url) { [weak owner] bytes, total in
                    await owner?.progress(bytes, total: total, workID: workID)
                }
                try owner.checkCurrent()
                owner.completedDownload = .init(storage: storage, name: entry.name, bytes: receipt.transferredBytes)
                owner.message = "Download complete. Save the file to Files or share it before closing this browser."
                owner.phase = .browsing
            }
        } catch { message = error.localizedDescription }
    }

    func beginImport() -> UUID? {
        guard canBrowse, let listing else { return nil }
        let request = UUID()
        importRequest = (request, listing.path, listingID)
        return request
    }

    func importFailed(_ error: any Error, request: UUID) {
        guard visible, importRequest?.id == request else { return }
        importRequest = nil
        if (error as NSError).code != NSUserCancelledError { message = "The selected file could not be opened from Files." }
    }

    func prepareUpload(_ url: URL, request: UUID) {
        guard canBrowse, let selected = importRequest, selected.id == request,
              selected.listingID == listingID, let listing,
              TerminalRemoteFilesPath.same(selected.directory, listing.path) else { return }
        importRequest = nil; transferPath = nil
        perform(.staging, retireOnFailure: false) { owner, workID in
            let task = Task.detached(priority: .userInitiated) { [weak owner] in
                try await TerminalFileStaging.copy(url) { [weak owner] bytes, total in
                    await owner?.progress(bytes, total: total, workID: workID)
                }
            }
            let file = try await withTaskCancellationHandler { try await task.value } onCancel: { task.cancel() }
            try owner.checkCurrent()
            guard owner.listingID == selected.listingID,
                  owner.listing.map({ TerminalRemoteFilesPath.same($0.path, selected.directory) }) == true else {
                throw WorkspaceSSHConnectionError.stale
            }
            owner.uploadReview = .init(file: file, directory: selected.directory, listingID: selected.listingID)
            owner.phase = .browsing
        }
    }

    func cancelUploadReview(_ id: UUID) {
        guard canReview, uploadReview?.id == id else { return }
        uploadReview = nil
    }

    func createFile(_ name: String, reviewID: UUID) {
        guard canReview, let review = uploadReview, review.id == reviewID,
              review.listingID == listingID, let listing, let client,
              TerminalRemoteFilesPath.same(review.directory, listing.path) else { return }
        do {
            let path = try TerminalRemoteFilesPath.joining(review.directory, name: name)
            guard !listing.entries.contains(where: { $0.id == Data(name.utf8) }) else {
                message = "That filename is already listed. Choose a new name; Remote Files never overwrites an existing file."
                return
            }
            uploadReview = nil; transferPath = path
            perform(.uploading) { owner, workID in
                // Retain uncertainty before the first remote create, including
                // an OPEN whose acknowledgement never reached the phone.
                owner.activeUploadPath = path
                owner.recordUploadUncertainty(path: path, bytes: 0)
                let receipt = try await client.upload(from: review.file.storage.url, to: path) { [weak owner] bytes, total in
                    await owner?.progress(bytes, total: total, workID: workID)
                }
                try owner.checkCurrent()
                owner.activeUploadPath = nil
                owner.scene?.recordFileTransferNotice(nil, for: owner.target)
                owner.message = "Created \(receipt.remotePath) · \(Self.formatBytes(receipt.transferredBytes))."
                owner.phase = .browsing
                // Refresh is a separate deliberate action; a successful create
                // must never be reported as failed because a later list failed.
            }
        } catch { message = error.localizedDescription }
    }

    func fileForExport(_ id: UUID) -> TerminalLocalFile? {
        guard canBrowse, let file = completedDownload, file.id == id else { return nil }
        return file
    }

    func cancel() {
        guard visible, !retired else { return }
        message = "Cancelled. Reconnect to browse or transfer another file."
        retire()
    }

    func retire() {
        guard !retired else { return }
        retired = true; settled = false; openingPending = false; importRequest = nil
        uploadReview = nil; completedDownload = nil
        phase = .closing
        work?.task.cancel()
        beginCleanup()
    }

    func retryCleanup() async {
        if !retired { retire() }
        if let cleanup { await cleanup.task.value }
        if connection != nil || work != nil {
            beginCleanup()
            await cleanup?.task.value
        }
    }

    private func checkCurrent(requireForeground: Bool = false) throws {
        try Task.checkCancellation()
        guard !retired, scene?.remoteFilesAreCurrent(self, requireForeground: requireForeground) == true else {
            throw WorkspaceSSHConnectionError.stale
        }
    }

    private func perform(_ phase: Phase, retireOnFailure: Bool = true,
                         operation: @escaping @MainActor (TerminalRemoteFilesController, UUID) async throws -> Void) {
        guard !retired, work == nil, visible else { return }
        self.phase = phase; transferredBytes = 0; totalBytes = nil
        let workID = UUID()
        let task = Task { [self] in
            do {
                // The scene can change after the button callback schedules
                // this task, before it starts any file or network operation.
                try checkCurrent(requireForeground: true)
                try await operation(self, workID)
            }
            catch {
                if let failure = error as? WorkspaceFileTransferFailure, let path = activeUploadPath {
                    if failure.remoteFileMayRemain {
                        recordUploadUncertainty(path: path, bytes: failure.transferredBytes)
                    } else { scene?.recordFileTransferNotice(nil, for: target) }
                    activeUploadPath = nil
                }
                if !retired {
                    if (error as? WorkspaceFileTransferFailure)?.remoteFileMayRemain != true {
                        message = error.localizedDescription
                    }
                    if retireOnFailure || error is CancellationError
                        || scene?.remoteFilesAreCurrent(self, requireForeground: false) != true { retire() }
                    else { self.phase = .browsing }
                }
            }
            if work?.id == workID { work = nil }
            if retired { beginCleanup() }
            else if openingPending, visible, let connection {
                openingPending = false
                perform(.connecting) { owner, _ in try await owner.openAndList(connection) }
            }
        }
        work = (workID, task)
    }

    private func progress(_ bytes: UInt64, total: UInt64?, workID: UUID) {
        guard !retired, work?.id == workID,
              scene?.remoteFilesAreCurrent(self, requireForeground: false) == true else { return }
        transferredBytes = max(transferredBytes, bytes); totalBytes = total
    }

    private func recordUploadUncertainty(path: String, bytes: UInt64) {
        let notice = "Upload to \(path) has no confirmed completion (\(Self.formatBytes(bytes)) acknowledged). "
            + "A partial or complete remote file may remain. Inspect that path before choosing a new filename; this app will not overwrite it."
        scene?.recordFileTransferNotice(notice, for: target)
        message = notice
    }

    private func beginCleanup() {
        guard retired, cleanup == nil else { return }
        let cleanupID = UUID(), pending = work?.task
        connections.retainFileCleanup(self)
        let task = Task { [self] in
            // Close promptly to interrupt a blocked native call, then join its
            // owner and close any connection returned by a late handshake.
            await closeManagedConnection()
            await pending?.value
            await closeManagedConnection()
            guard cleanup?.id == cleanupID else { return }
            cleanup = nil; phase = .closed
            settled = connection == nil && work == nil
            if settled { connections.releaseFileCleanup(id) }
        }
        cleanup = (cleanupID, task)
    }

    private func closeManagedConnection() async {
        guard let connection else { return }
        do {
            try await connections.manager.close(connectionID: connection.id)
            if self.connection?.id == connection.id { self.connection = nil; client = nil }
            cleanupFailure = nil
        } catch {
            cleanupFailure = "The file connection is still closing. Retry closing it before reconnecting."
        }
    }

    static func formatBytes(_ value: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(clamping: value), countStyle: .file)
    }
}

@MainActor
private struct TerminalRemoteFilesScreen: View {
    @Bindable var scene: WorkspaceTerminalScene
    @Bindable var files: TerminalRemoteFilesController
    @State private var path = ""
    @State private var uploadName = ""
    @State private var importRequestID: UUID?
    @State private var saveFile: TerminalLocalFile?
    @State private var shareFile: TerminalLocalFile?
    @Environment(\.scenePhase) private var scenePhase

    private var visible: Bool { scenePhase == .active && files.visible }

    var body: some View {
        NavigationStack {
            Group {
                if visible { browser }
                else {
                    ContentUnavailableView("Remote Files Paused", systemImage: "lock",
                        description: Text("Return to FrankenTerm to view files."))
                }
            }
            .navigationTitle("Remote Files")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Done") { scene.dismissOverview() }
                        .accessibilityIdentifier("terminal.files.done")
                }
            }
        }
        .onAppear {
            path = files.listing?.path ?? ""
            uploadName = files.uploadReview?.file.name ?? ""
        }
        .onChange(of: files.listingID) { _, _ in path = files.listing?.path ?? "" }
        .onChange(of: files.uploadReview?.id) { _, _ in uploadName = files.uploadReview?.file.name ?? "" }
        .background {
            if let request = importRequestID {
                RemoteFilesImportPicker(requestID: request, selected: { id, result in
                    guard importRequestID == id else { return }
                    importRequestID = nil
                    switch result {
                    case .success(let urls):
                        if urls.count == 1, let url = urls.first { files.prepareUpload(url, request: id) }
                    case .failure(let error): files.importFailed(error, request: id)
                    }
                }, cancelled: { id in
                    guard importRequestID == id else { return }
                    importRequestID = nil
                    files.importFailed(NSError(domain: NSCocoaErrorDomain, code: NSUserCancelledError), request: id)
                }).id(request)
            }
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .background { importRequestID = nil }
        }
        .onDisappear { importRequestID = nil }
        .sheet(item: $saveFile) { file in TerminalFileSavePicker(file: file) }
        .sheet(item: Binding(get: { visible ? files.authenticationPrompt : nil }, set: { _ in })) { request in
            switch request {
            case .password(let value):
                MobileSSHPasswordSheet(prompt: files.connections.passwordPrompt, request: value)
            case .keyboardInteractive(let value):
                MobileSSHKeyboardInteractiveSheet(prompt: files.connections.keyboardInteractivePrompt, request: value)
            }
        }
    }

    private var browser: some View {
        List {
            Section("Connected Account") {
                LabeledContent("Machine", value: files.target.machineName)
                LabeledContent("Host", value: files.target.host)
                LabeledContent("Port", value: String(files.target.port))
                LabeledContent("Account", value: files.target.user)
                LabeledContent("Route", value: files.target.route.id)
                Text("Files use this account through a separate SSH connection. Transfers stop when you close this browser or leave the app.")
                    .font(.footnote).foregroundStyle(.secondary)
            }
            if let message = files.message {
                Section { Text(message).font(.callout).accessibilityIdentifier("terminal.files.message") }
            }
            switch files.phase {
            case .connecting, .listing:
                Section {
                    ProgressView(files.phase == .connecting ? "Connecting to Remote Files…" : "Reading directory…")
                    Button("Cancel", role: .cancel) { files.cancel() }
                }
            case .staging, .downloading, .uploading:
                transferProgress
            case .closing:
                Section { ProgressView("Closing file connection…") }
            case .closed:
                Section {
                    if let failure = files.cleanupFailure {
                        Text(failure)
                        Button("Retry Closing Connection") { Task { await files.retryCleanup() } }
                    } else {
                        Text("The file connection is closed. Your terminal session has its own connection.")
                        Button("Reconnect") { scene.reconnectRemoteFiles(files) }
                            .disabled(!files.canReconnect)
                            .accessibilityIdentifier("terminal.files.reconnect")
                    }
                }
            case .browsing:
                if let review = files.uploadReview { uploadReview(review) }
                else if let listing = files.listing { directory(listing, listingID: files.listingID) }
            }
            if let file = files.completedDownload {
                Section("Completed Download") {
                    Text(file.name).font(.body.monospaced())
                    Text(TerminalRemoteFilesController.formatBytes(file.bytes)).foregroundStyle(.secondary)
                    Button("Save to Files", systemImage: "folder") {
                        saveFile = files.fileForExport(file.id)
                    }
                        .disabled(!files.canBrowse)
                        .accessibilityIdentifier("terminal.files.save")
                    Button("Share File", systemImage: "square.and.arrow.up") {
                        shareFile = files.fileForExport(file.id)
                    }
                        .disabled(!files.canBrowse)
                        .popover(item: $shareFile, attachmentAnchor: .rect(.bounds), arrowEdge: .bottom) { file in
                            TerminalFileShareSheet(file: file).presentationCompactAdaptation(.sheet)
                        }
                }
            }
        }
        .accessibilityIdentifier("terminal.files.browser")
    }

    @ViewBuilder
    private func directory(_ listing: NativeSFTPDirectoryListing, listingID: UUID) -> some View {
        Section("Directory") {
            Text(listing.path).font(.body.monospaced())
            TextField("Remote directory path", text: $path)
                .font(.body.monospaced()).textInputAutocapitalization(.never).autocorrectionDisabled()
                .submitLabel(.go).onSubmit { files.browse(path) }
                .accessibilityIdentifier("terminal.files.path")
            HStack {
                Button("Up", systemImage: "arrow.up") {
                    files.browse(TerminalRemoteFilesPath.parent(of: listing.path), listingID: listingID)
                }.disabled(listing.path.utf8.elementsEqual("/".utf8))
                Spacer()
                Button("Go") { files.browse(path) }
                Button("Refresh", systemImage: "arrow.clockwise") { files.browse(listing.path, listingID: listingID) }
            }.buttonStyle(.borderless).disabled(!files.canBrowse)
            Button("Upload from Files", systemImage: "square.and.arrow.up") {
                guard let request = files.beginImport() else { return }
                importRequestID = request
            }.disabled(!files.canBrowse).accessibilityIdentifier("terminal.files.upload.pick")
            Text("Regular files up to 256 MiB. Uploads create a new filename and never overwrite. Symbolic links are not opened.")
                .font(.footnote).foregroundStyle(.secondary)
        }
        Section("Contents") {
            if listing.entries.isEmpty { Text("This directory is empty.").foregroundStyle(.secondary) }
            ForEach(listing.entries, id: \.id) { entry in
                switch entry.attributes.kind {
                case .directory:
                    Button { files.enter(entry, listingID: listingID) } label: {
                        Label(entry.name, systemImage: "folder")
                    }.disabled(!files.canBrowse)
                case .regularFile:
                    Button { files.download(entry, listingID: listingID) } label: {
                        HStack {
                            Label(entry.name, systemImage: "doc")
                            Spacer(minLength: 8)
                            VStack(alignment: .trailing) {
                                if let size = entry.attributes.size {
                                    Text(TerminalRemoteFilesController.formatBytes(size)).font(.caption)
                                }
                                Text("Download").font(.caption)
                            }.foregroundStyle(.secondary)
                        }
                    }.disabled(!files.canBrowse)
                        .accessibilityLabel("Download \(entry.name)")
                case .symbolicLink, .other, .unknown:
                    VStack(alignment: .leading, spacing: 3) {
                        Label(entry.name, systemImage: entry.attributes.kind == .symbolicLink ? "link" : "doc.questionmark")
                        Text(entry.attributes.kind == .symbolicLink ? "Symbolic link · not opened" : "Not a regular file")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
            if listing.isTruncated {
                Text("Showing the first 512 entries. Other entries may exist. Enter a directory path above to navigate directly.")
                    .font(.footnote).accessibilityIdentifier("terminal.files.truncated")
            }
        }
    }

    private func uploadReview(_ review: TerminalRemoteFilesController.UploadReview) -> some View {
        Section("Review Upload") {
            LabeledContent("Selected file", value: review.file.name)
            LabeledContent("Size", value: "\(review.file.bytes.formatted()) bytes")
            Text("Destination directory").font(.caption).foregroundStyle(.secondary)
            Text(review.directory).font(.body.monospaced())
            TextField("New remote filename", text: $uploadName)
                .font(.body.monospaced()).textInputAutocapitalization(.never).autocorrectionDisabled()
                .accessibilityIdentifier("terminal.files.upload.name")
            if let destination = try? TerminalRemoteFilesPath.joining(review.directory, name: uploadName) {
                Text("Create at").font(.caption).foregroundStyle(.secondary)
                Text(destination).font(.body.monospaced())
            }
            Text("The reviewed file is a protected copy of the selected bytes. If this transfer stops, a remote file may remain; inspect it before trying a new filename.")
                .font(.footnote).foregroundStyle(.secondary)
            Button("Create New File", systemImage: "plus") { files.createFile(uploadName, reviewID: review.id) }
                .disabled(!files.canReview || !TerminalRemoteFilesPath.validName(uploadName))
                .accessibilityIdentifier("terminal.files.upload.create")
            Button("Discard Upload", role: .cancel) { files.cancelUploadReview(review.id) }
                .disabled(!files.canReview)
        }
    }

    private var transferProgress: some View {
        Section(files.phase == .staging ? "Preparing Review" : files.phase == .uploading ? "Uploading" : "Downloading") {
            if let path = files.transferPath { Text(path).font(.body.monospaced()) }
            if let total = files.totalBytes, total > 0 {
                ProgressView(value: Double(min(files.transferredBytes, total)), total: Double(total))
            } else { ProgressView() }
            Text(progressText).monospacedDigit()
                .accessibilityIdentifier("terminal.files.progress")
            Button("Cancel Transfer", role: .cancel) { files.cancel() }
                .accessibilityIdentifier("terminal.files.cancel")
        }
    }

    private var progressText: String {
        let done = TerminalRemoteFilesController.formatBytes(files.transferredBytes)
        if let total = files.totalBytes { return "\(done) of \(TerminalRemoteFilesController.formatBytes(total))" }
        return done
    }

}

/// Both entry points retain the exact completed file on the system controller.
/// Closing its SSH owner cannot remove bytes a Files provider is still reading.
struct TerminalFileExportLease: Identifiable, Sendable {
    let id = UUID()
    let url: URL
    private let terminalFile: TerminalLocalFile?
    private let workspaceFile: WorkspaceFilesCompletion?

    init(file: TerminalLocalFile) {
        url = file.storage.url; terminalFile = file; workspaceFile = nil
    }
    init?(completion: WorkspaceFilesCompletion) {
        guard let url = completion.downloadURL else { return nil }
        self.url = url; terminalFile = nil; workspaceFile = completion
    }
}

final class TerminalFileExportPicker: UIDocumentPickerViewController {
    var fileLease: TerminalFileExportLease?
}

struct TerminalFileSavePicker: UIViewControllerRepresentable {
    let file: TerminalFileExportLease
    init(file: TerminalLocalFile) { self.file = .init(file: file) }
    init(file: TerminalFileExportLease) { self.file = file }
    func makeUIViewController(context: Context) -> TerminalFileExportPicker {
        let picker = TerminalFileExportPicker(forExporting: [file.url], asCopy: true)
        picker.fileLease = file
        return picker
    }
    func updateUIViewController(_ uiViewController: TerminalFileExportPicker, context: Context) {}
}

final class TerminalFileActivityController: UIActivityViewController {
    var fileLease: TerminalFileExportLease?
}

struct TerminalFileShareSheet: UIViewControllerRepresentable {
    let file: TerminalFileExportLease
    init(file: TerminalLocalFile) { self.file = .init(file: file) }
    init(file: TerminalFileExportLease) { self.file = file }
    func makeUIViewController(context: Context) -> TerminalFileActivityController {
        let controller = TerminalFileActivityController(activityItems: [file.url], applicationActivities: nil)
        controller.fileLease = file
        return controller
    }
    func updateUIViewController(_ uiViewController: TerminalFileActivityController, context: Context) {}
}

/// The failed handshake already supplied this evidence. Opening this review
/// never probes the peer again, changes trust, or offers an acceptance action.
@MainActor
private struct TerminalHostKeyMismatchScreen: View {
    @Bindable var scene: WorkspaceTerminalScene
    @Bindable var session: TerminalSessionController
    @Environment(\.dismiss) private var dismiss
    @Environment(\.scenePhase) private var scenePhase
    @State private var copiedKey: WorkspaceDocument.HostKey?

    private var currentMismatch: WorkspaceSSHConnectionHostKeyMismatch? {
        guard scenePhase == .active else { return nil }
        return scene.hostKeyMismatch(for: session)
    }

    var body: some View {
        NavigationStack {
            Group {
                if let mismatch = currentMismatch {
                    Form {
                        Section {
                            Label("Connection Stopped", systemImage: "exclamationmark.shield")
                                .font(.headline)
                            Text("This peer presented an identity outside this workspace's permitted keys and certificate authorities. FrankenTerm stopped before signing into it. A planned server change, a different server, or interception can cause this; verify the identity independently.")
                        }
                        Section("Affected Connection") {
                            detail("Workspace", scene.workspaceName)
                            detail("Machine", session.target.machineName)
                            detail("Machine ID", mismatch.machineID)
                            detail("Destination Route", mismatch.destinationRouteID)
                            detail("Peer", mismatch.hopIndex.map { "Jump host \($0 + 1)" } ?? "Destination")
                            detail("Peer Route", mismatch.routeID)
                            detail("Host", mismatch.host)
                            detail("Port", String(mismatch.port))
                            detail("SSH User", mismatch.user)
                            detail("Host-Key Alias", mismatch.hostKeyAlias ?? "None — uses the host name")
                            if !mismatch.expectedCertificateAuthorities.isEmpty || mismatch.hostCertificate != nil {
                                detail("Certificate Principal", mismatch.certificatePrincipal)
                            }
                        }
                        if !mismatch.expectedKeys.isEmpty {
                            Section {
                                ForEach(Array(mismatch.expectedKeys.enumerated()), id: \.offset) { index, key in
                                    fingerprint(key, mismatch: mismatch, presented: false)
                                        .accessibilityIdentifier("terminal.host-key.expected.\(index)")
                                }
                            } header: {
                                Text("Expected Host Keys")
                            } footer: {
                                Text("These were the permitted raw host keys for this exact peer when the connection was checked.")
                            }
                        }
                        if !mismatch.expectedCertificateAuthorities.isEmpty {
                            Section {
                                ForEach(Array(mismatch.expectedCertificateAuthorities.enumerated()), id: \.offset) { index, authority in
                                    fingerprint(authority, mismatch: mismatch, presented: false)
                                        .accessibilityIdentifier("terminal.host-key.expected-authority.\(index)")
                                }
                            } header: {
                                Text("Expected Certificate Authorities")
                            } footer: {
                                Text("These authorities may certify this peer's host key. An authority fingerprint is different from the host key and certificate fingerprints.")
                            }
                        }
                        Section {
                            Text(mismatch.hostCertificate == nil ? "Host key" : "Exact host certificate")
                                .font(.caption).foregroundStyle(.secondary)
                            fingerprint(mismatch.presentedKey, mismatch: mismatch, presented: true)
                                .accessibilityIdentifier("terminal.host-key.presented")
                        } header: {
                            Text("Presented by This Peer")
                        } footer: {
                            Text("Copying a fingerprint does not trust it. Copied text stays on this device and expires from the clipboard after 2 minutes.")
                        }
                        if let certificate = mismatch.hostCertificate,
                           let authority = try? WorkspaceDocument.HostKey(algorithm: certificate.authority.algorithm,
                                fingerprint: certificate.authority.fingerprint),
                           let subject = try? WorkspaceDocument.HostKey(algorithm: certificate.subject.algorithm,
                                fingerprint: certificate.subject.fingerprint) {
                            Section("Presented Certificate's Signing Authority") {
                                fingerprint(authority, mismatch: mismatch, presented: true)
                            }
                            Section("Presented Certificate's Host Key") {
                                fingerprint(subject, mismatch: mismatch, presented: true)
                            }
                        }
                        Section("Verify Before Updating") {
                            Text("Compare the complete new fingerprint with the machine's administrator or a trusted console using a separate channel. A fresh connection or ssh-keyscan alone does not verify identity.")
                            Text("For trust imported from your Mac, verify this peer's host key or certificate authority on the Mac, then export the workspace again with FrankenTerm Export.")
                            Text("Return to the workspace list, tap Add machines (+), then choose Scan My Mac or Open a Workspace or Results File. Review the workspace update and its host-key changes before restoring.")
                            Text("For a key you verified on this phone, return to the machine page, open Keys, and choose Check for a Changed Host Key… for this peer. Independently compare the new fingerprint before replacing the phone-verified key.")
                            Text("Keys blocked on this phone remain blocked after a workspace update.")
                        }
                        Section("Check the Connection Path") {
                            Text("If this is the wrong path, return to the machine page and choose Choose Route and Sign-In. Every jump host and the destination must still match their permitted keys or certificate authorities.")
                        }
                    }
                } else {
                    ContentUnavailableView("Host Identity Review Unavailable", systemImage: "lock.shield",
                        description: Text("This review is available only while FrankenTerm is in the foreground. Entering the background or replacing the connection clears its fingerprint details."))
                }
            }
            .navigationTitle("Host Identity Changed").navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
            }
        }
        .onChange(of: currentMismatch) { _, mismatch in
            copiedKey = nil
            if mismatch == nil { dismiss() }
        }
    }

    private func detail(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(verbatim: label).font(.caption).foregroundStyle(.secondary)
            Text(verbatim: value).fixedSize(horizontal: false, vertical: true)
        }.frame(maxWidth: .infinity, alignment: .leading)
    }

    private func fingerprint(_ key: WorkspaceDocument.HostKey,
                             mismatch: WorkspaceSSHConnectionHostKeyMismatch, presented: Bool) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(verbatim: key.algorithm).font(.caption).foregroundStyle(.secondary)
            Text(verbatim: key.fingerprint).font(.system(.body, design: .monospaced))
                .fixedSize(horizontal: false, vertical: true)
            Button(copiedKey == key ? "Fingerprint Copied" : presented ? "Copy Fingerprint" : "Copy Expected Fingerprint",
                   systemImage: "doc.on.doc") {
                guard scenePhase == .active,
                      scene.copyHostKeyFingerprint(key, from: session, expected: mismatch) else { return }
                copiedKey = key
            }
            .accessibilityLabel(presented ? "Copy presented fingerprint" : "Copy expected fingerprint for \(key.algorithm)")
        }.padding(.vertical, 4)
    }
}

/// These are device preferences, so a workspace import cannot change what a
/// physical key does. The scene's existing panel fence owns held-key cleanup
/// and creates a fresh input generation after dismissal, without reconnecting.
private struct TerminalHardwareSettingsScreen: View {
    @Environment(\.dismiss) private var dismiss
    @State private var preferences = TerminalHardwarePreferencesStore.shared.current
    @State private var saveFailed = false

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Picker("Control and Option", selection: $preferences.modifierMapping) {
                        ForEach(TerminalHardwarePreferences.ModifierMapping.allCases, id: \.self) { mapping in
                            Text(mapping.title).tag(mapping)
                        }
                    }
                    .accessibilityIdentifier("terminal.keyboard.modifiers")
                } footer: {
                    Text(preferences.modifierMapping.explanation)
                }
                Section {
                    Picker("Backspace", selection: $preferences.backspace) {
                        ForEach(TerminalHardwarePreferences.Backspace.allCases, id: \.self) { backspace in
                            Text(backspace.title).tag(backspace)
                        }
                    }
                    .accessibilityIdentifier("terminal.keyboard.backspace")
                } footer: {
                    Text("Use Control-Backspace for a shell that expects Ctrl-H. Extended keyboard protocols keep their negotiated key encoding.")
                }
                Section {
                    Button("Use Default Keys") { preferences = .init() }
                        .accessibilityIdentifier("terminal.keyboard.defaults")
                } footer: {
                    Text("Applies to hardware keyboards across this device. Command shortcuts, Control-Space, composed text, paste, and the on-screen key row keep their existing behavior.")
                }
                if saveFailed {
                    Text("Keyboard settings could not be saved. Please try again.").foregroundStyle(.red)
                }
            }
            .navigationTitle("Hardware Keyboard")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        do { try TerminalHardwarePreferencesStore.shared.save(preferences); dismiss() }
                        catch { saveFailed = true }
                    }
                    .accessibilityIdentifier("terminal.keyboard.save")
                }
            }
        }
    }
}

/// Every machine as a chip in its own colors, with its state (live,
/// connecting, needs attention, disconnected) at a glance and the one on screen
/// raised. Tap a chip to switch; swipe the terminal with one finger to step
/// through them in order. While a machine or pane off screen has new output,
/// the strip ends in Next New Output (⌘J), which goes to each in turn.
private struct MachineStrip: View {
    let scene: WorkspaceTerminalScene
    @Namespace private var raised

    var body: some View {
        HStack(spacing: 0) {
            ScrollViewReader { proxy in
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 8) {
                        ForEach(scene.navigationMachines) { machine in chip(machine).id(machine.id) }
                    }
                    .padding(.horizontal, 12).padding(.vertical, 8)
                }
                .onChange(of: scene.selectedMachineID, initial: true) { _, id in
                    withAnimation(.snappy) { proxy.scrollTo(id, anchor: .center) }
                }
            }
            if scene.hasUnseenOutputElsewhere {
                Button { withAnimation(.snappy) { scene.showNextUnseenOutput() } } label: {
                    Image(systemName: "arrow.forward.circle.fill").font(.title3)
                        .foregroundStyle(Color.accentColor)
                        .overlay(alignment: .topTrailing) { UnseenBadge() }
                        .frame(width: 44, height: 40)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .transition(.scale.combined(with: .opacity))
                .accessibilityLabel("Next New Output")
                .accessibilityHint("Shows the next machine or pane with output you have not seen.")
                .accessibilityIdentifier("terminal.unseen.next")
            }
            if scene.hasActionableAttentionElsewhere {
                Button { withAnimation(.snappy) { scene.showNextAttention() } } label: {
                    Image(systemName: "exclamationmark.circle.fill").font(.title3)
                        .foregroundStyle(.orange)
                        .frame(width: 44, height: 40)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .transition(.scale.combined(with: .opacity))
                .accessibilityLabel("Next Attention")
                .accessibilityHint("Visits input warnings, connection problems, and unseen output in machine and pane order.")
                .accessibilityIdentifier("terminal.attention.next")
            }
        }
        .animation(.snappy, value: scene.hasUnseenOutputElsewhere)
        .animation(.snappy, value: scene.hasActionableAttentionElsewhere)
        .background(.ultraThinMaterial)
    }

    private func chip(_ machine: SceneMachine) -> some View {
        let theme = scene.themes[machine.id]
        let accent = Color(ftHex: theme?.accent ?? "#8E8E93")
        let selected = machine.id == scene.selectedMachineID
        let status = scene.status(of: machine.id)
        let unseen = scene.hasUnseenOutput(machine.id)
        let reasons = scene.attentionReasons(machine.id)
        let attention = TerminalAttentionReason.allCases.filter { reasons.contains($0) }
        let needsAction = attention.contains { $0 != .newOutput }
        return Button { withAnimation(.snappy) { scene.select(machine.id) } } label: {
            HStack(spacing: 7) {
                StatusDot(status: status, accent: accent)
                Text(verbatim: machine.name).font(.subheadline.weight(selected ? .semibold : .medium)).lineLimit(1)
            }
            .padding(.horizontal, 12).padding(.vertical, 7)
            .foregroundStyle(selected ? Color(ftHex: theme?.foreground ?? "#FFFFFF") : Color.primary.opacity(status == .live ? 0.9 : 0.75))
            .background {
                if selected {
                    Capsule().fill(Color(ftHex: theme?.background ?? "#1C1C1E"))
                        .overlay(Capsule().stroke(accent, lineWidth: 1.5))
                        .shadow(color: accent.opacity(0.45), radius: 6)
                        .matchedGeometryEffect(id: "raised", in: raised)
                } else {
                    Capsule().fill(Color.primary.opacity(0.07))
                }
            }
            .overlay(alignment: .topTrailing) {
                if needsAction { Image(systemName: "exclamationmark.circle.fill").foregroundStyle(.orange).font(.caption) }
                else if unseen { UnseenBadge() }
            }
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(([machine.name, Self.describe(status)] + attention.map(\.label)).joined(separator: ", "))
        .accessibilityHint("Changes machines. One-finger horizontal swipes on the terminal do the same when mouse interaction is inactive.")
        .accessibilityAddTraits(selected ? .isSelected : [])
    }

    static func describe(_ status: WorkspaceTerminalScene.Status) -> String {
        switch status {
        case .live: "connected"
        case .ready: "signed in, terminal not open"
        case .connecting: "connecting"
        case .attention: "needs attention"
        case .disconnected: "disconnected"
        case .unavailable: "not available on this device"
        }
    }
}

/// The selected machine's FrankenTerm panes, the ones the desktop app shows,
/// each named by its project directory and its program's title. Tap one to
/// put it on screen; a two-finger swipe on the terminal steps through them.
private struct PaneStrip: View {
    let session: TerminalSessionController
    let state: TerminalMuxControl.State
    let accent: Color
    /// Brings up the keyboard: needed while the pane's program has the mouse,
    /// since a tap on the terminal is then a click.
    let onKeyboard: () -> Void

    var body: some View {
        HStack(spacing: 0) {
            ScrollViewReader { proxy in
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 6) {
                        Image(systemName: "square.split.2x1").font(.caption).foregroundStyle(.secondary)
                            .accessibilityHidden(true)
                        ForEach(state.panes) { pane in chip(pane).id(pane.paneId) }
                    }
                    .padding(.horizontal, 12).padding(.vertical, 6)
                }
                .onChange(of: state.shown, initial: true) { _, shown in
                    guard let shown else { return }
                    withAnimation(.snappy) { proxy.scrollTo(shown, anchor: .center) }
                }
            }
            if !session.isViewingOnly {
                Button(action: onKeyboard) {
                    Image(systemName: state.mouseGrabbed ? "keyboard.badge.ellipsis" : "keyboard")
                        .font(.callout)
                        .foregroundStyle(state.mouseGrabbed ? accent : Color.primary.opacity(0.7))
                        .frame(width: 44, height: 36)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Show keyboard")
                .accessibilityHint(state.mouseGrabbed ? "Taps on this program are clicks." : "")
            }
        }
        .background(.ultraThinMaterial.opacity(0.85))
        .sensoryFeedback(.selection, trigger: state.shown)
    }

    private func chip(_ pane: TerminalMuxPane) -> some View {
        let shown = pane.paneId == state.shown
        let unseen = !shown && state.unread.contains(pane.paneId)
        let names = Self.names(pane)
        return Button { withAnimation(.snappy) { session.showPane(pane.paneId) } } label: {
            VStack(alignment: .leading, spacing: 1) {
                Text(verbatim: names.place).font(.caption.weight(shown ? .semibold : .medium))
                if let detail = names.detail {
                    Text(verbatim: detail).font(.caption2).opacity(0.7)
                }
            }
            .lineLimit(1).frame(maxWidth: 170, alignment: .leading)
            .padding(.horizontal, 10).padding(.vertical, 5)
            .foregroundStyle(shown ? Color.primary : Color.primary.opacity(0.7))
            .background {
                RoundedRectangle(cornerRadius: 9)
                    .fill(shown ? accent.opacity(0.22) : Color.primary.opacity(0.06))
                    .overlay(RoundedRectangle(cornerRadius: 9).stroke(shown ? accent : .clear, lineWidth: 1.2))
            }
            .overlay(alignment: .topTrailing) { if unseen { UnseenBadge() } }
            .contentShape(RoundedRectangle(cornerRadius: 9))
        }
        .buttonStyle(.plain)
        .accessibilityLabel((names.detail.map { "\(names.place), \($0)" } ?? names.place) + (unseen ? ", new output" : ""))
        .accessibilityHint("Changes panes within this machine. Two-finger horizontal swipes wrap through this machine's panes.")
        .accessibilityAddTraits(shown ? .isSelected : [])
    }

    /// The pane's project directory first (agents' panes are told apart by
    /// project), then its program's title when that says something more.
    static func names(_ pane: TerminalMuxPane) -> (place: String, detail: String?) {
        let title = pane.title.trimmingCharacters(in: .whitespacesAndNewlines)
        let directory = pane.workingDir.flatMap { URL(string: $0)?.lastPathComponent }.flatMap { $0.isEmpty || $0 == "/" ? nil : $0 }
        guard let directory else { return (title.isEmpty ? "Pane \(pane.paneId)" : title, nil) }
        return (directory, title.isEmpty || title == directory ? nil : title)
    }
}

/// Marks a machine or pane with output the phone has not shown since it
/// arrived, in the system's tint so it reads apart from the machine's accent.
private struct UnseenBadge: View {
    var body: some View {
        Circle().fill(Color.accentColor).frame(width: 7, height: 7)
            .overlay(Circle().stroke(.background, lineWidth: 1.5))
            .offset(x: 2, y: -2)
            .transition(.scale.combined(with: .opacity))
            .accessibilityHidden(true)
    }
}

/// A machine's state in one glyph: a lit dot in its accent when live, a
/// breathing ring while connecting, an alert when it needs a look.
private struct StatusDot: View {
    let status: WorkspaceTerminalScene.Status
    let accent: Color

    var body: some View {
        Group {
            switch status {
            case .live:
                Circle().fill(accent).frame(width: 8, height: 8).shadow(color: accent, radius: 3)
            case .ready:
                Image(systemName: "checkmark.circle").font(.system(size: 10)).foregroundStyle(accent)
            case .connecting:
                Circle().stroke(accent, lineWidth: 1.5).frame(width: 8, height: 8)
                    .phaseAnimator([false, true]) { ring, out in
                        ring.scaleEffect(out ? 1.25 : 0.8).opacity(out ? 0.4 : 1)
                    } animation: { _ in .easeInOut(duration: 0.8) }
            case .attention:
                Image(systemName: "exclamationmark.circle.fill").font(.system(size: 10)).foregroundStyle(.orange)
            case .disconnected:
                Circle().stroke(Color.secondary, lineWidth: 1.5).frame(width: 8, height: 8)
            case .unavailable:
                Image(systemName: "slash.circle").font(.system(size: 10)).foregroundStyle(.secondary)
            }
        }
        .frame(width: 12, height: 12)
        .animation(.smooth, value: status)
    }
}

struct WorkspaceCanvasPane {
    let machineID: String
    let terminal: NativeTerminalView?
}

/// Hosts one focused terminal and at most one explicit companion. Views keep
/// their machine slot through focus transfers; removing a slot releases only
/// its presentation, while its authenticated session remains owned by the scene.
private struct WorkspaceCanvasHost: UIViewRepresentable {
    let scene: WorkspaceTerminalScene
    let panes: [WorkspaceCanvasPane]
    func makeUIView(context: Context) -> WorkspaceCanvasView { WorkspaceCanvasView(scene: scene) }
    func updateUIView(_ view: WorkspaceCanvasView, context: Context) { view.show(panes) }
    static func dismantleUIView(_ view: WorkspaceCanvasView, coordinator: ()) { view.retireNavigation() }
}

final class WorkspaceCanvasView: UIView {
    private let canvas = UIView()
    /// Keeps the keyboard up between machines, so switching resizes neither.
    private let keyboardHolder = TerminalKeyboardHolder()
    private var controls: NativeTerminalNavigationControls?
    private weak var navigationSurface: UIView?
    private var hosts: [String: WorkspaceCanvasPaneView] = [:]
    private var panes: [WorkspaceCanvasPane] = []
    private weak var scene: WorkspaceTerminalScene?

    init(scene: WorkspaceTerminalScene) {
        self.scene = scene
        super.init(frame: .zero)
        canvas.translatesAutoresizingMaskIntoConstraints = false
        addSubview(keyboardHolder)
        addSubview(canvas)
        NSLayoutConstraint.activate([
            canvas.topAnchor.constraint(equalTo: topAnchor), canvas.bottomAnchor.constraint(equalTo: bottomAnchor),
            canvas.leadingAnchor.constraint(equalTo: leadingAnchor), canvas.trailingAnchor.constraint(equalTo: trailingAnchor)
        ])
    }
    @available(*, unavailable)
    required init?(coder: NSCoder) { nil }
    override var canBecomeFirstResponder: Bool { scene?.selectedSession?.isViewingOnly == true }

    private func installNavigation(on surface: UIView?) {
        guard navigationSurface !== surface else { return }
        controls?.retire(suspendNavigation: false); controls = nil
        navigationSurface = surface
        guard let scene, let surface else { return }
        do {
            // One recognizer owns the finger-count decision and the four
            // captured candidates. Mux panes are not flattened into machines.
            let controls = try NativeTerminalNavigationControls(coordinator: scene.navigation, terminalCanvas: surface,
                permitsCanvasNavigation: { [weak scene] in
                    scene?.permitsCanvasNavigation == true
                }, canvasAction: { [weak scene] axis, direction in
                    scene?.captureCanvasAction(axis, direction)
                })
            controls.onFailure = { [weak scene] error in scene?.report(error) }
            self.controls = controls
        } catch { scene.report(error) }
    }

    @objc private func tapped(_ gesture: UITapGestureRecognizer) {
        guard let host = gesture.view as? WorkspaceCanvasPaneView, let scene else { return }
        if host.machineID != scene.selectedMachineID { scene.select(host.machineID); return }
        // A live terminal handles its own taps: the keyboard, a selection.
        guard scene.selectedSession?.terminal == nil else { return }
        Task { [weak scene] in await scene?.activateKeyboard() }
    }

    func retireNavigation() {
        controls?.retire(suspendNavigation: false); controls = nil; navigationSurface = nil
        keyboardHolder.release()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        layoutPanes()
    }

    private func layoutPanes() {
        guard !panes.isEmpty else { return }
        let width = max(0, (canvas.bounds.width - CGFloat(panes.count - 1)) / CGFloat(panes.count))
        for (index, pane) in panes.enumerated() {
            let host = hosts[pane.machineID]
            host?.frame = CGRect(x: CGFloat(index) * (width + 1), y: 0, width: width, height: canvas.bounds.height)
            if let host { pane.terminal?.frame = host.bounds }
        }
    }

    /// A hardware keyboard switches as the strips and swipes do: ⇧⌘] ⇧⌘[ for
    /// machines, ⌘] ⌘[ for a mux's panes, ⌘J for the next new output;
    /// ⇧⌘D composes a command. UIKit asks
    /// the terminal's ancestors, so these work while a terminal has the
    /// keyboard. The untitled shifted spellings match a keyboard that reports
    /// ⇧] as "}".
    override var keyCommands: [UIKeyCommand]? {
        guard let scene else { return nil }
        var commands: [UIKeyCommand] = []
        if let session = scene.selectedSession, session.phase == .live, session.terminal != nil, !session.isViewingOnly {
            commands.append(UIKeyCommand(title: "Compose Command", action: #selector(composeCommand),
                                         input: "d", modifierFlags: [.command, .shift]))
        }
        if scene.navigationMachineIDs.count > 1 {
            commands += [
                UIKeyCommand(title: "Next Machine", action: #selector(nextMachine), input: "]", modifierFlags: [.command, .shift]),
                UIKeyCommand(title: "Previous Machine", action: #selector(previousMachine), input: "[", modifierFlags: [.command, .shift]),
                UIKeyCommand(input: "}", modifierFlags: [.command, .shift], action: #selector(nextMachine)),
                UIKeyCommand(input: "{", modifierFlags: [.command, .shift], action: #selector(previousMachine))
            ]
        }
        if let session = scene.selectedSession, session.phase == .live, (session.muxState?.panes.count ?? 0) > 1 {
            commands += [
                UIKeyCommand(title: "Next Pane", action: #selector(nextPane), input: "]", modifierFlags: .command),
                UIKeyCommand(title: "Previous Pane", action: #selector(previousPane), input: "[", modifierFlags: .command)
            ]
        }
        commands.append(UIKeyCommand(title: "Next New Output", action: #selector(nextUnseenOutput),
                                     input: "j", modifierFlags: .command))
        commands.append(UIKeyCommand(title: "Next Attention", action: #selector(nextAttention),
                                     input: "j", modifierFlags: [.command, .shift]))
        return commands
    }
    @objc private func nextUnseenOutput() { scene?.showNextUnseenOutput() }
    @objc private func nextAttention() { scene?.showNextAttention() }
    @objc private func composeCommand() { scene?.selectedSession?.composeCommand() }
    @objc private func nextMachine() { scene?.stepMachine(forward: true) }
    @objc private func previousMachine() { scene?.stepMachine(forward: false) }
    @objc private func nextPane() { scene?.stepPane(forward: true) }
    @objc private func previousPane() { scene?.stepPane(forward: false) }

    func show(_ panes: [WorkspaceCanvasPane]) {
        guard panes.count <= 2, Set(panes.map(\.machineID)).count == panes.count else { return }
        self.panes = panes
        let selected = scene?.selectedMachineID
        let terminal = panes.first { $0.machineID == selected }?.terminal
        for id in Array(hosts.keys) where !panes.contains(where: { $0.machineID == id }) {
            hosts.removeValue(forKey: id)?.removeFromSuperview()
        }
        for pane in panes {
            let host: WorkspaceCanvasPaneView
            if let existing = hosts[pane.machineID] { host = existing }
            else {
                host = WorkspaceCanvasPaneView(machineID: pane.machineID)
                hosts[pane.machineID] = host; canvas.addSubview(host)
                let tap = UITapGestureRecognizer(target: self, action: #selector(tapped(_:)))
                tap.cancelsTouchesInView = false; host.addGestureRecognizer(tap)
            }
            pane.terminal?.keyboardHolder = keyboardHolder
            pane.terminal?.isUserInteractionEnabled = pane.machineID == selected
            if pane.terminal?.superview !== host || host.subviews.count != 1 {
                host.subviews.forEach { $0.removeFromSuperview() }
                if let terminal = pane.terminal {
                    terminal.frame = host.bounds
                    terminal.autoresizingMask = [.flexibleWidth, .flexibleHeight]
                    host.addSubview(terminal)
                }
            }
        }
        installNavigation(on: selected.flatMap { hosts[$0] })
        layoutPanes()
        if terminal?.input.isReadOnly == true {
            keyboardHolder.release()
            if !isFirstResponder { _ = becomeFirstResponder() }
        } else if isFirstResponder { _ = resignFirstResponder() }
        // A machine whose terminal is on its way takes the keyboard when it
        // comes (or the holder's deadline lets it go); its page takes none.
        if terminal == nil, scene?.selectedSession.map({ $0.phase == .idle }) ?? true { keyboardHolder.release() }
    }
}

private final class WorkspaceCanvasPaneView: UIView {
    let machineID: String
    init(machineID: String) {
        self.machineID = machineID
        super.init(frame: .zero)
        clipsToBounds = true
        accessibilityIdentifier = "terminal.canvas.\(machineID)"
    }
    @available(*, unavailable)
    required init?(coder: NSCoder) { nil }
}

/// Search the discovered fleet, not terminal contents. The immutable snapshot
/// does not stream previews or silently open connections. A refresh is explicit;
/// its source-bound choices are checked again when the sheet has dismissed.
private struct WorkspaceFleetOverviewScreen: View {
    let scene: WorkspaceTerminalScene
    @State private var catalog: TerminalFleetOverview?
    @State private var query = TerminalFleetOverview.Query()
    @State private var results: TerminalFleetOverview.Results?
    @State private var resultsKey: SearchKey?
    @State private var refresh = UUID()
    @State private var loading = false
    @State private var loadFailure: String?
    @State private var searchFailure: String?
    @State private var selectionFailure: String?
    private struct SearchKey: Hashable {
        let catalogID: UUID?
        let query: TerminalFleetOverview.Query
    }
    private var searchKey: SearchKey { .init(catalogID: catalog?.id, query: query) }

    var body: some View {
        NavigationStack {
            List {
                Section {
                    Text("Browse machines in the current navigation group and their discovered panes. This snapshot does not connect to machines or search terminal output.")
                        .font(.caption).foregroundStyle(.secondary)
                    if let catalog {
                        Text("\(catalog.machineCount) machines · \(catalog.paneCount) discovered panes")
                            .font(.caption).monospacedDigit()
                    }
                    Picker("Show", selection: $query.scope) {
                        Text("All").tag(TerminalFleetOverview.Scope.all)
                        Text("Machines").tag(TerminalFleetOverview.Scope.machines)
                        Text("Panes").tag(TerminalFleetOverview.Scope.panes)
                    }.pickerStyle(.segmented)
                    Picker("Show at refresh", selection: $query.filter) {
                        Text("Any status").tag(TerminalFleetOverview.Filter.all)
                        Text("Connected or signed in").tag(TerminalFleetOverview.Filter.connected)
                        Text("Needs attention").tag(TerminalFleetOverview.Filter.attention)
                        Text("New output").tag(TerminalFleetOverview.Filter.newOutput)
                    }
                }
                if let selectionFailure {
                    Text(verbatim: selectionFailure).font(.callout).foregroundStyle(.orange)
                }
                if loading {
                    ProgressView("Reading known inventory…")
                } else if let loadFailure {
                    Text(verbatim: loadFailure).foregroundStyle(.orange)
                } else if catalog != nil {
                    if let searchFailure {
                        Text(verbatim: searchFailure).foregroundStyle(.orange)
                    } else if let results, resultsKey == searchKey {
                        Section {
                            if results.rows.isEmpty {
                                Text("No matches in this snapshot. Change the filters or refresh the inventory.")
                                    .foregroundStyle(.secondary)
                            }
                            ForEach(results.rows) { row in
                                Button { choose(row.id) } label: { resultRow(row) }
                                    .buttonStyle(.plain)
                                    .accessibilityHint(row.paneID == nil ? "Show this machine" : "Show this exact pane")
                            }
                        } header: {
                            Text(results.limited
                                ? "Showing \(results.rows.count) of \(results.matchingCount) matches. Narrow the search for more."
                                : "\(results.matchingCount) matches")
                        }
                    } else { ProgressView("Filtering inventory…") }
                }
            }
            .navigationTitle("Fleet Overview")
            .navigationBarTitleDisplayMode(.inline)
            .searchable(text: $query.text, placement: .navigationBarDrawer(displayMode: .always),
                        prompt: "Machine, tag, host, pane, tab, or directory")
            .textInputAutocapitalization(.never).autocorrectionDisabled()
            .onSubmit(of: .search) {
                // Return is an explicit shortcut only for an unambiguous result.
                if let results, resultsKey == searchKey, results.matchingCount == 1, let row = results.rows.first {
                    choose(row.id)
                }
            }
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Done") { scene.dismissOverview() }.keyboardShortcut(.cancelAction)
                }
                ToolbarItem(placement: .primaryAction) {
                    Button("Refresh", systemImage: "arrow.clockwise") {
                        catalog = nil; results = nil; resultsKey = nil
                        selectionFailure = nil; searchFailure = nil; loadFailure = nil
                        refresh = UUID()
                    }.disabled(loading).accessibilityIdentifier("terminal.fleet.refresh")
                }
            }
        }
        .presentationDetents([.large])
        .task(id: refresh) { await reload() }
        .task(id: searchKey) { await filter() }
    }

    private func reload() async {
        let request = refresh
        loading = true; loadFailure = nil
        defer { if refresh == request { loading = false } }
        do {
            let snapshot = try await scene.makeOverview()
            try Task.checkCancellation()
            guard refresh == request, scene.overviewPresented else { return }
            catalog = snapshot
        } catch {
            if !Task.isCancelled, refresh == request, scene.overviewPresented {
                loadFailure = TerminalSessionController.describe(error)
            }
        }
    }

    private func filter() async {
        let key = searchKey
        results = nil; resultsKey = nil; searchFailure = nil
        guard let catalog else { return }
        do {
            try await Task.sleep(for: .milliseconds(120))
            let query = key.query
            let filtered = try await withThrowingTaskGroup(of: TerminalFleetOverview.Results.self) { group in
                group.addTask { try catalog.search(query) }
                guard let result = try await group.next() else { throw CancellationError() }
                return result
            }
            try Task.checkCancellation()
            guard key == searchKey, scene.overviewPresented else { return }
            results = filtered; resultsKey = key
        } catch {
            if !Task.isCancelled, key == searchKey, scene.overviewPresented {
                searchFailure = TerminalSessionController.describe(error)
            }
        }
    }

    private func choose(_ choice: TerminalFleetOverview.Choice) {
        guard let catalog, resultsKey == searchKey, results?.rows.contains(where: { $0.id == choice }) == true else { return }
        do { try scene.chooseOverview(choice, in: catalog) }
        catch { selectionFailure = TerminalSessionController.describe(error) }
    }

    private func resultRow(_ row: TerminalFleetOverview.Row) -> some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: row.paneID == nil ? "desktopcomputer" : "rectangle.split.2x1")
                .foregroundStyle(Color(ftHex: scene.themes[row.machineID]?.accent ?? "#3B82F6"))
                .frame(width: 24).accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 4) {
                if let pane = row.paneID {
                    Text(verbatim: "\(row.machineName) · Pane \(pane)").font(.caption).foregroundStyle(.secondary)
                }
                Text(verbatim: row.title).font(.body.weight(row.selected ? .semibold : .regular)).lineLimit(2)
                if !row.detail.isEmpty { Text(verbatim: row.detail).font(.caption).foregroundStyle(.secondary).lineLimit(2) }
                HStack(spacing: 6) {
                    Text(verbatim: row.status.label).foregroundStyle(.secondary)
                }.font(.caption2)
                ForEach(TerminalAttentionReason.allCases.filter { row.attention.contains($0) }, id: \.self) { reason in
                    Text(verbatim: reason.label).font(.caption2)
                        .foregroundStyle(reason == .newOutput ? Color.accentColor : .orange)
                }
            }
            Spacer(minLength: 0)
            if row.selected { Image(systemName: "checkmark.circle.fill").accessibilityLabel("Selected at refresh") }
        }
        .padding(.vertical, 4)
        .contentShape(Rectangle())
        .accessibilityElement(children: .combine)
    }
}
