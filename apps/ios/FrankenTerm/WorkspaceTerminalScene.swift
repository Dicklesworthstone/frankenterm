import SwiftUI
import UIKit
import Observation
import FrankenTermKit
import FrankenTermSSHIntegration

/// One machine of a live workspace scene, in the group's order. `target` is
/// nil when none of its direct-SSH routes is ready on this device.
struct SceneMachine: Identifiable {
    let id: String, name: String
    let target: TerminalSessionTarget?
    let unavailable: String
    let theme: WorkspaceDocument.Theme?
    var autoConnect = false
}

/// Keeps the workspace's authenticated connections and terminals, giving its
/// single keyboard to exactly the selected machine. Only the active group's
/// opted-in machines prepare in the background. Selection may open a prepared
/// on-select route; a manual route waits for Open Terminal. A lost terminal's
/// replacement waits for New Session, even after bounded reauthentication.
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
    let machines: [SceneMachine]
    private(set) var sessions: [String: TerminalSessionController] = [:]
    /// Each machine's theme now; a pick in the scene changes it live.
    private(set) var themes: [String: WorkspaceDocument.Theme] = [:]
    /// The machine on screen; follows committed navigation only.
    private(set) var selectedMachineID: String?
    /// The committed group's order, not the full connection-owner inventory.
    /// Storing the IDs also makes same-machine group changes observable to UI.
    private(set) var navigationMachineIDs: [String] = []
    private(set) var problem: String?
    /// A read-only snapshot picker. Its dismissal joins one owned navigation
    /// operation before the terminal keyboard can be enabled again.
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
    @ObservationIgnored private var bindings: [String: TerminalKeyboardNavigationBinding] = [:]
    @ObservationIgnored private(set) var closed = false
    /// The app is active (not behind a Face ID sheet, alert or the switcher).
    @ObservationIgnored private var foreground = true
    /// The terminal screen is showing; sessions run whether or not it is.
    @ObservationIgnored private var onScreen = false
    /// A touch begun before a lifecycle or machine transition cannot become a
    /// fresh navigation request merely because the same screen returns later.
    @ObservationIgnored private var canvasGeneration = UUID()
    /// A retained scene can reopen at another machine. Its previous selection
    /// must not create a terminal before the requested focus handoff commits.
    @ObservationIgnored private var appearanceSelection: String?
    private struct GroupAppearance {
        let id: UUID
        let machineID: String
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
    /// False disables group warming and automatic retries; explicit actions
    /// still use the same bounded scheduler (also useful to hosted tests).
    @ObservationIgnored private let autoConnect: Bool

    init(workspaceID: UUID, revision: UUID, workspaceName: String, machines: [SceneMachine],
         initialMachineID: String, connections: MobileConnections,
         initialGroup: TerminalConnectionIntent.Group? = nil, autoConnect: Bool = true,
         maximumRetainedConnections: Int = 16) throws {
        self.workspaceID = workspaceID; self.revision = revision; self.autoConnect = autoConnect
        self.workspaceName = workspaceName; self.machines = machines; self.connections = connections
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
    }

    var selectedMachine: SceneMachine? { machines.first { $0.id == selectedMachineID } }
    var selectedSession: TerminalSessionController? { selectedMachineID.flatMap { sessions[$0] } }
    var navigationMachines: [SceneMachine] {
        let known = Dictionary(uniqueKeysWithValues: machines.map { ($0.id, $0) })
        return navigationMachineIDs.compactMap { known[$0] }
    }

    // MARK: Fleet overview

    func beginOverview() {
        guard !closed, foreground, onScreen, !overviewBlocksInput, !groupAppearanceBlocksInput,
              navigation.phase == .ready || navigation.phase == .inactive,
              selectedSession?.clipboardIsPresenting != true,
              selectedSession?.paneNavigationBusy != true else { return }
        // A deliberate picker must not discard an unfinished IME candidate or
        // an unadmitted local key. Previously admitted bytes keep their owner.
        if let keyboard = owner.keyboard,
           keyboard.document.isMarked || keyboard.accounting.pendingEvents > 0 || keyboard.phase == .blocked {
            problem = "Finish composing or let pending local input settle before opening the overview."
            return
        }
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
            let session = sessions[machine.id]
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
                panes = try state.panes.map { pane in
                    .init(plan: try control.inputRouting.navigation(pane: pane.paneId), title: pane.title,
                          tabTitle: pane.tabTitle, workingDirectory: pane.workingDir ?? "")
                }
                shown = state.shown; paneCount += panes.count
            }
            inventory.append(.init(id: machine.id, name: machine.name, endpoint: endpoint, status: status,
                sessionID: session?.id, panes: panes, selectedPaneID: shown))
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
                    if let plan = try self.navigation.navigation.plan(machineID: machineID) {
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
        overviewGeneration = UUID(); overviewPresented = false
        // Keep the closing sheet's slot until its exact dismissal arrives.
        // Otherwise an old onDismiss could consume a newly opened picker.
        if closed { overviewVisit = nil }
        overviewChoice = nil; overviewCatalogID = nil; overviewBuildID = nil
        overviewJob?.task.cancel()
    }

    func status(of machineID: String) -> Status {
        guard machines.first(where: { $0.id == machineID })?.target != nil else { return .unavailable }
        if disconnected.contains(machineID) { return .disconnected }
        guard let session = sessions[machineID] else {
            return connectionIntent.shouldPrepare(machineID) ? .connecting : .disconnected
        }
        if residency.wasReleased(machineID: machineID, ownerID: session.id) {
            if !session.settled { return session.cleanupFailure == nil ? .connecting : .attention }
            return connectionIntent.shouldPrepare(machineID) && permitsPreparation(machineID) ? .connecting : .disconnected
        }
        switch session.phase {
        case .idle, .connecting: return .connecting
        case .ready: return .ready
        case .live: return .live
        case .couldNotConnect: return .attention
        case .ended, .failed:
            return autoConnect && session.dropped && connectionIntent.permitsReconnect(machineID)
                && reconnectPolicies[machineID, default: .init()].nextDelay != nil
                ? .connecting : .attention
        }
    }

    /// A live machine off screen with output the phone has not shown since.
    func hasUnseenOutput(_ machineID: String) -> Bool {
        machineID != selectedMachineID && status(of: machineID) == .live && sessions[machineID]?.hasUnseenOutput == true
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
        pendingGroupAppearance = GroupAppearance(id: id, machineID: machineID, group: group,
                                                 navigationGroup: navigationGroup, connectOnOpen: connectOnOpen)
        startGroupAppearance()
        updatePresentationVisibility()
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
        cancelOverview()
        cancelGroupAppearance()
        onScreen = false
        canvasGeneration = UUID()
        appearanceSelection = nil
        navigation.suspend()
        updatePresentationVisibility()
    }

    func setForeground(_ active: Bool) {
        guard !closed else { return }
        if !active { cancelOverview(); cancelGroupAppearance() }
        if foreground != active { canvasGeneration = UUID() }
        foreground = active
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
        inBackground = true
        cancelGroupAppearance()
        // iOS may stop executing immediately after its grace period. Time we
        // cannot observe a session must not count as a stable live interval.
        for id in Array(reconnectPolicies.keys) { reconnectPolicies[id]?.recordEnd() }
        cancelReconnects()
        for controller in sessions.values { controller.suspendPendingTerminalCreation() }
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
                    guard let plan = try self.navigation.navigation.plan(machineID: machineID) else { return }
                    try await self.navigation.perform(plan)
                }
            } catch { report(error) }
        }
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
                    guard let plan = try self.navigation.navigation.plan(axis: .machine,
                                                                         direction: forward ? .next : .previous) else { return }
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
              let session = selectedSession, session.phase == .live,
              !session.paneNavigationBusy, !session.clipboardIsPresenting,
              let control = session.muxControl else { return }
        do {
            if let plan = try control.inputRouting.navigation(forward: forward) { session.showPane(plan.paneID) }
        } catch { report(error) }
    }

    /// A machine or pane other than the one on screen has output the phone
    /// has not shown, so Next New Output has somewhere to go.
    var hasUnseenOutputElsewhere: Bool {
        navigationMachineIDs.contains(where: hasUnseenOutput)
            || selectedSession?.muxState.map { state in state.unread.contains { $0 != state.shown } } == true
    }

    /// Shows the next machine or mux pane with output not yet shown, in ring
    /// order after the one on screen: the machine switch a chip tap makes,
    /// then the pane switch ⌘] makes. Output alone never moves the screen.
    func showNextUnseenOutput() {
        guard !closed, !overviewBlocksInput, !groupAppearanceBlocksInput,
              selectedSession?.clipboardIsPresenting != true, selectedSession?.paneNavigationBusy != true,
              let target = nextUnseenOutput() else { return }
        let appearance = appearanceGeneration
        Task {
            do {
                if target.machineID != self.selectedMachineID {
                    try await whileBusy {
                        guard self.appearanceGeneration == appearance, !self.groupAppearanceBlocksInput,
                              self.navigationMachineIDs.contains(target.machineID) else { throw TerminalNavigationError.stale }
                        guard let plan = try self.navigation.navigation.plan(machineID: target.machineID) else { return }
                        try await self.navigation.perform(plan)
                    }
                }
                guard let paneID = target.paneID else { return }
                // The machine's own view settles first, as after a chip tap.
                for _ in 0..<40 {
                    guard self.appearanceGeneration == appearance, !self.closed, self.selectedMachineID == target.machineID,
                          let session = self.selectedSession, session.phase == .live, let control = session.muxControl,
                          control.current.panes.contains(where: { $0.paneId == paneID }) else {
                        throw TerminalNavigationError.stale
                    }
                    if control.current.shown == paneID { return }
                    await session.waitForOverviewPresentation()
                    if !session.paneNavigationBusy {
                        try await session.navigatePane(control.inputRouting.navigation(pane: paneID))
                        return
                    }
                    try await Task.sleep(for: .milliseconds(50))
                }
            } catch { report(error) }
        }
    }

    private func nextUnseenOutput() -> TerminalUnseenOutput.Source? {
        var ring: [TerminalUnseenOutput.Source] = [], unseen: Set<TerminalUnseenOutput.Source> = []
        var current = selectedMachineID.map { TerminalUnseenOutput.Source(machineID: $0) }
        for id in navigationMachineIDs {
            let selected = id == selectedMachineID
            // A machine off screen keeps its mux state current without a view.
            if let session = sessions[id], session.phase == .live, let state = session.muxControl?.current,
               !state.panes.isEmpty {
                for pane in state.panes {
                    let source = TerminalUnseenOutput.Source(machineID: id, paneID: pane.paneId)
                    ring.append(source)
                    if selected, pane.paneId == state.shown { current = source }
                    else if status(of: id) == .live, state.unread.contains(pane.paneId) { unseen.insert(source) }
                }
            } else {
                let source = TerminalUnseenOutput.Source(machineID: id)
                ring.append(source)
                if hasUnseenOutput(id) { unseen.insert(source) }
            }
        }
        return TerminalUnseenOutput.next(after: current, ring: ring, unseen: unseen)
    }

    var permitsCanvasNavigation: Bool {
        !closed && foreground && onScreen && !overviewBlocksInput && !groupAppearanceBlocksInput && appearanceSelection == nil
            && selectedSession?.clipboardIsPresenting != true
            && selectedSession?.paneNavigationBusy != true
            && selectedSession?.terminal?.remoteMouseGrabbed != true
    }

    /// Called once per direction at touch-down by the native finger classifier.
    /// Never look up a new neighbour at touch-up or after keyboard settlement.
    func captureCanvasAction(_ axis: TerminalNavigationAxis, _ direction: TerminalNavigationDirection)
        -> NativeTerminalNavigationControls.NavigationAction? {
        guard permitsCanvasNavigation else { return nil }
        let generation = canvasGeneration, sourceMachine = selectedMachineID
        let sessionID = selectedSession?.id
        let sourcePane = selectedSession?.muxControl?.inputRouting.current.destination
        let stillCurrent: @MainActor () -> Bool = { [weak self] in
            guard let self else { return false }
            return self.permitsCanvasNavigation && self.canvasGeneration == generation
                && self.selectedMachineID == sourceMachine && self.selectedSession?.id == sessionID
                && self.selectedSession?.muxControl?.inputRouting.current.destination == sourcePane
        }
        switch axis {
        case .machine:
            guard let plan = try? navigation.navigation.plan(axis: .machine, direction: direction) else { return nil }
            return { [weak self] in
                guard let self, stillCurrent() else { throw TerminalNavigationError.stale }
                try await self.navigation.perform(plan)
            }
        case .pane:
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
        previous?.discardRetainedOutput()
        let controller = TerminalSessionController(target: target, connections: connections, focus: self,
            reconnectingFrom: preservingSuccessfulCandidate ? previous : nil)
        controller.onChange = { [weak self, weak controller] in
            guard let self, let controller, self.sessions[machineID] === controller else { return }
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
                try? self.connectionIntent.terminalEnded(machineID)
                self.reconnectPolicies[machineID, default: .init()].recordEnd()
            case .couldNotConnect:
                self.reconnectPolicies[machineID, default: .init()].recordEnd()
            case .idle, .connecting, .ready: break
            }
            self.reconcile()
        }
        sessions[machineID] = controller
        residency.started(machineID)
        controller.setPresentationVisible(isVisible(machineID))
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
                if opening == .explicit || (opening == .onSelect && self.isVisible(machineID)) {
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
        do { try connectionIntent.disconnect(machineID) }
        catch { report(error); return }
        requestedConnections.remove(machineID)
        explicitPreparations.remove(machineID)
        controllerJobs[machineID]?.task.cancel()
        cancelReconnect(machineID)
        reconnectPolicies[machineID] = nil
        if let session = sessions[machineID] {
            session.requestStop()
            Task { await session.stop() }
        }
    }

    func disconnectAll() {
        for machine in machines where machine.target != nil { disconnect(machine.id) }
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
        for id in readyOrder where controllerJobs.count < 3 && controllerJobs[id] == nil {
            guard let controller = sessions[id], !disconnected.contains(id) else { continue }
            if controller.phase == .idle {
                runController(controller, machineID: id, starting: true)
                continue
            }
            let opening = connectionIntent.opening(id, creation: controller.route.sessionCreation)
            if opening == .explicit || (opening == .onSelect && isVisible(id)) {
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
        var slots = max(0, 3 - controllerJobs.count)
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
            if sessions.values.filter({ !$0.settled }).count >= residency.limit {
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
        residency.permitsPreparation(machineID, selected: onScreen ? selectedMachineID : nil,
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
            let owned = sessions.compactMap { id, controller -> TerminalConnectionResidency.Owner? in
                guard !controller.settled else { return nil }
                let state: TerminalConnectionResidency.State
                switch controller.phase {
                case .ready:
                    state = controllerJobs[id] == nil && controller.canReleaseUnusedPreparation
                        && !explicitPreparations.contains(id) ? .automaticPreparation : .protectedPreparation
                case .live: state = .terminal
                case .idle, .connecting: state = .preparing
                case .ended, .failed, .couldNotConnect: state = .settling
                }
                return .init(machineID: id, id: controller.id, state: state)
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
    func activateKeyboard() async {
        guard !closed, onScreen, !overviewBlocksInput, !groupAppearanceBlocksInput,
              selectedSession?.keyboardActivationBlocked != true,
              navigation.phase == .inactive, let selection = navigation.navigation.selection,
              selection.paneID != nil, sessions[selection.machineID]?.phase == .live else { return }
        await owner.joinSealed()
        guard !closed, foreground, !overviewBlocksInput, !groupAppearanceBlocksInput,
              navigation.navigation.selection == selection,
              selectedSession?.keyboardActivationBlocked != true else { return }
        do { try await navigation.activateSelection() } catch { report(error) }
    }

    /// Shows a machine in another theme: its chrome at once, and its
    /// terminal from the next frame. A later session starts in it.
    func applyTheme(_ theme: WorkspaceDocument.Theme, to machineID: String) {
        guard !closed, machines.contains(where: { $0.id == machineID }) else { return }
        themes[machineID] = theme
        sessions[machineID]?.applyPalette(theme.terminalBasePalette)
    }

    /// Synchronously seal every scene-owned responder before scheduling joins.
    /// Used when the workspace itself goes away (replaced or removed).
    func requestClose() {
        guard !closed else { return }
        closed = true
        cancelOverview()
        cancelGroupAppearance()
        canvasGeneration = UUID()
        cancelReconnects()
        for job in controllerJobs.values { job.task.cancel() }
        navigation.suspend()
        for controller in sessions.values {
            controller.discardRetainedOutput()
            controller.requestStop()
        }
    }

    /// Concurrent callers join the same cleanup, not an early closed flag.
    /// A subsequent call retries failed settlements; it never reopens a shell.
    func close() async {
        requestClose()
        if let closing { await closing.value; return }
        let controllers = Array(sessions.values), navigation = self.navigation
        let reconnects = reconnectJobs.values.map(\.task)
        let operations = controllerJobs.values.map(\.task)
        let overview = overviewJob?.task
        let appearance = groupAppearanceJob?.task
        let release = capacityReleaseJob
        let job = Task {
            if let release { await release.value }
            if let appearance { await appearance.value }
            if let overview { await overview.value }
            for reconnect in reconnects { await reconnect.value }
            for controller in controllers { await controller.stop() }
            for operation in operations { await operation.value }
            await navigation.joinSuspension()
        }
        closing = job
        await job.value
        closing = nil
    }

    func report(_ error: any Error) {
        problem = TerminalSessionController.describe(error)
    }
    func dismissProblem() { problem = nil }

    private func inventory(_ values: [String: TerminalKeyboardNavigationBinding]) throws -> [TerminalNavigationMachine] {
        try machines.map { machine in
            let panes = try values[machine.id].map { [try TerminalNavigationPane(id: $0.paneID, title: "Direct SSH")] }
            return try TerminalNavigationMachine(id: machine.id, name: machine.name, panes: panes ?? [])
        }
    }

    /// Committed navigation: show its machine and, once the destination's focus
    /// is published, put the new keyboard into that machine's view. A machine
    /// that connected in the background gets its view now.
    private func navigationChanged() {
        let selection = navigation.navigation.selection
        navigationMachineIDs = navigation.navigation.machines.map(\.id)
        if selectedMachineID != selection?.machineID {
            canvasGeneration = UUID()
            selectedMachineID = selection?.machineID
            do { try connectionIntent.select(selectedMachineID) } catch { report(error) }
        }
        if !groupAppearanceBlocksInput, appearanceSelection == selectedMachineID { appearanceSelection = nil }
        updatePresentationVisibility()
        if onScreen, !overviewBlocksInput, !groupAppearanceBlocksInput,
           let session = selectedSession, session.phase == .live, session.terminal == nil { session.showView() }
        reconcile()
        guard !overviewBlocksInput, !groupAppearanceBlocksInput,
              navigation.phase == .ready, let selection, selection.paneID != nil,
              let keyboard = owner.keyboard else { return }
        sessions[selection.machineID]?.install(keyboard)
    }

    private func isVisible(_ machineID: String) -> Bool {
        !closed && foreground && !inBackground && onScreen && !groupAppearanceBlocksInput && appearanceSelection == nil
            && (!overviewBlocksInput || overviewAllowsTargetPresentation) && machineID == selectedMachineID
    }

    private func updatePresentationVisibility() {
        for (machineID, controller) in sessions {
            controller.setOverviewBlocking(overviewBlocksInput || groupAppearanceBlocksInput)
            controller.setPresentationVisible(isVisible(machineID))
        }
    }

    /// Navigation refuses overlapping transactions; a gesture in flight ends
    /// within its bounded handoff, so retry briefly rather than drop the change.
    private func whileBusy(_ body: () async throws -> Void) async throws {
        for attempt in 0..<100 {
            try Task.checkCancellation()
            do { return try await body() }
            catch TerminalNavigationError.busy where attempt < 99 { try await Task.sleep(for: .milliseconds(50)) }
        }
    }

    // MARK: TerminalSessionFocus

    var permitsTerminalCreation: Bool {
        !closed && foreground && !inBackground && !overviewBlocksInput
            && !groupAppearanceBlocksInput && appearanceSelection == nil
    }

    func shows(machineID: String) -> Bool { isVisible(machineID) }

    func attach(machineID: String, source: TerminalSource,
                encoder: any TerminalKeyboardEncoding) async throws -> TerminalKeyboardSession {
        guard !closed, machines.contains(where: { $0.id == machineID }),
              source.identity.machineID == machineID else { throw TerminalNavigationError.unavailable }
        let binding = TerminalKeyboardNavigationBinding(source: source, encoder: encoder)
        // Bindings are installed before the inventory offers the new pane.
        // Recompute after each busy wait, so another machine's concurrent
        // attachment/detachment is never overwritten by an old dictionary.
        try await whileBusy {
            guard !self.closed else { throw TerminalNavigationError.unavailable }
            var next = self.bindings; next[machineID] = binding
            let inventory = try self.inventory(next)
            try self.owner.replaceBindings(Array(next.values))
            self.bindings = next
            try self.navigation.replaceInventory(inventory)
        }
        // The sign-in that led here usually ends behind Face ID's sheet, which
        // leaves the scene inactive for a moment; the keyboard waits for it
        // rather than failing the new session. Background still refuses.
        for _ in 0..<200 where !foreground && !inBackground && !closed { try await Task.sleep(for: .milliseconds(50)) }
        try await whileBusy {
            guard !self.closed, self.bindings[machineID]?.source == source else { throw TerminalNavigationError.stale }
            guard self.isVisible(machineID) else { throw CancellationError() }
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
        // Removing the visible source seals its input before the list changes.
        // Preserve the old map on refused binding admission. A retry also
        // reconciles inventory if an earlier inventory update failed afterward.
        try await whileBusy {
            var next = self.bindings
            if next[machineID]?.source == source { next[machineID] = nil }
            let inventory = try self.inventory(next)
            try self.owner.replaceBindings(Array(next.values))
            self.bindings = next
            try self.navigation.replaceInventory(inventory)
        }
        await owner.joinSealed()
    }

    func fontSizeChanged(_ size: Double) {
        UserDefaults.standard.set(size, forKey: TerminalSessionController.fontSizeKey)
        for session in sessions.values { session.setFontSize(size) }
    }

    func inputRetired(machineID: String) {
        // A handoff retires the old keyboard itself and reports it synchronously;
        // sealing then would invalidate the switch in flight. Only a settled
        // selection's lost input (dismissal, resign, OS) is sealed here.
        guard navigation.phase == .ready, owner.currentSelection?.machineID == machineID,
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

    var body: some View {
        let machine = scene.selectedMachine
        let overviewSessionID = scene.overviewSessionID
        let backgroundHex = machine.flatMap { scene.themes[$0.id]?.background }
        let background = backgroundHex.map { Color(ftHex: $0) } ?? .black
        VStack(spacing: 0) {
            MachineStrip(scene: scene)
            if let session = scene.selectedSession, session.phase == .live, let mux = session.muxState, !mux.panes.isEmpty {
                PaneStrip(session: session, state: mux,
                          accent: Color(ftHex: machine.flatMap { scene.themes[$0.id]?.accent } ?? "#3B82F6"),
                          onKeyboard: { Task { await scene.activateKeyboard() } })
                    .transition(.move(edge: .top).combined(with: .opacity))
            }
            ZStack {
                WorkspaceCanvasHost(scene: scene, terminal: machine.flatMap { scene.sessions[$0.id]?.terminal })
                if let machine { card(for: machine).transition(.opacity.combined(with: .scale(scale: 0.97))) }
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
            } else if let session = scene.selectedSession, let problem = session.interactionProblem {
                problemBanner(problem) { session.dismissInteractionProblem() }
            } else if let problem = scene.problem {
                problemBanner(problem) { scene.dismissProblem() }
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
        .sheet(isPresented: Binding(get: { scene.overviewPresented }, set: { visible in
            if !visible { scene.dismissOverview() }
        }), onDismiss: { if let overviewSessionID { scene.finishOverviewDismissal(overviewSessionID) } }) {
            WorkspaceFleetOverviewScreen(scene: scene)
        }
        .toolbarBackground(.visible, for: .navigationBar)
        .toolbarColorScheme(TerminalSessionTarget.isDark(backgroundHex) ? .dark : .light, for: .navigationBar)
        .task {
            let connect = connectOnOpen && !initialConnectionConsumed
            initialConnectionConsumed = true
            scene.appear(selecting: initialMachineID, active: scenePhase == .active,
                         group: group, connectOnOpen: connect)
        }
        .onChange(of: returnableMachineID, initial: true) { _, id in onShowing?(id) }
        .onDisappear {
            historyInspection?.retire(); historyInspection = nil; retainedOutput = nil
            scene.disappear()
            if !scene.closed { onShowing?(nil) }
        }
        .onChange(of: scenePhase) { _, phase in
            // The app forwards its phase to the scene (MobileWorkspaceModel).
            if phase != .active { historyInspection?.retire(); historyInspection = nil; retainedOutput = nil }
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
                    .disabled(session.clipboardIsPresenting || session.localInputNeedsDecision)
            }
            Menu {
                if let session = scene.selectedSession, session.phase == .live, let terminal = session.terminal {
                    // ⇧⌘D is the canvas's key command: a SwiftUI shortcut on a
                    // button inside this menu never fired (UI test 3).
                    Button("Compose Command", systemImage: "square.and.pencil") { session.composeCommand() }
                        .disabled(session.clipboardIsPresenting || session.localInputNeedsDecision)
                        .accessibilityIdentifier("terminal.command.open")
                    Button("Select Output", systemImage: "doc.on.doc") { session.inspectOutput() }
                        .disabled(session.clipboardIsPresenting || session.localInputNeedsDecision)
                    // Pinching the terminal does the same.
                    Menu("Text Size", systemImage: "textformat.size") {
                        Button("Bigger", systemImage: "plus.magnifyingglass") { terminal.biggerText() }
                        Button("Smaller", systemImage: "minus.magnifyingglass") { terminal.smallerText() }
                        Button("Actual Size", systemImage: "1.magnifyingglass") { terminal.actualSize() }
                    }
                }
                if themes != nil, machine != nil {
                    Button("Theme…", systemImage: "paintpalette") { pickingTheme = true }
                        .accessibilityIdentifier("scene.theme")
                }
                if let machine, machine.target != nil {
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
        if scene.disconnected.contains(machine.id) {
            disconnectedCard(machine, accent: accent)
        } else if scene.capacityBlockedMachineID == machine.id {
            capacityCard(machine, accent: accent)
        } else if scene.preparationWasReleased(machine.id), let session = scene.sessions[machine.id] {
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
        } else if let session = scene.sessions[machine.id] {
            switch session.phase {
            case .idle, .connecting:
                connecting(machine, accent: accent, detail: Self.via(session.route))
            case .ready:
                readyCard(machine, session: session, accent: accent)
            case .live:
                if session.terminal == nil { ProgressView().controlSize(.large).tint(accent) }
            case let .couldNotConnect(help):
                connectionHelp(help, settled: session.settled) { scene.connect(machine.id) }
            case let .ended(message), let .failed(message):
                switch scene.status(of: machine.id) {
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
                    if let owned = scene.sessions[other.id], !owned.settled {
                        Button(role: .destructive) {
                            scene.disconnect(other.id, expectedControllerID: owned.id)
                        } label: {
                            Text(verbatim: "\(other.name) · \(owned.phase == .live ? "disconnect live terminal" : "close connection")")
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
        let ended = scene.requiresNewSession(machine.id)
        let message = session.route.mux
            ? "The connection is ready to attach to this machine's existing FrankenTerm panes."
            : (ended
                ? "The previous terminal has ended. Choose New Session to open a terminal; an ended shell cannot be restored."
                : "This machine is signed in. No terminal has been opened on this connection.")
        let action = session.route.mux ? "Open Existing Panes" : (ended ? "New Session" : "Open Terminal")
        return sceneCard(symbol: "checkmark.shield", title: "Signed in to \(machine.name)", message: message, accent: accent) {
            Button(action, systemImage: "terminal") { scene.openTerminal(machine.id) }
                .buttonStyle(.borderedProminent).controlSize(.large).tint(accent)
                .accessibilityIdentifier("terminal.ready.open")
        }
    }

    private func endedCard(_ machine: SceneMachine, message: String, session: TerminalSessionController,
                           accent: Color) -> some View {
        sceneCard(symbol: "moon.zzz", title: "Session ended", message: message, accent: accent) {
            Button(session.route.mux ? "Reconnect Existing Panes" : "New Session", systemImage: "arrow.clockwise") {
                scene.openTerminal(machine.id)
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

    /// Nothing ran, so there is no session to review: the reason, the one
    /// change that fixes it, and Try Again once the attempt has settled.
    private func connectionHelp(_ help: WorkspaceConnectionHelp, settled: Bool,
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
                Text("FrankenTerm tries again on its own when you come back with Tailscale on.")
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
        }
        .animation(.snappy, value: scene.hasUnseenOutputElsewhere)
        .background(.ultraThinMaterial)
    }

    private func chip(_ machine: SceneMachine) -> some View {
        let theme = scene.themes[machine.id]
        let accent = Color(ftHex: theme?.accent ?? "#8E8E93")
        let selected = machine.id == scene.selectedMachineID
        let status = scene.status(of: machine.id)
        let unseen = scene.hasUnseenOutput(machine.id)
        return Button { withAnimation(.snappy) { scene.select(machine.id) } } label: {
            HStack(spacing: 7) {
                StatusDot(status: status, accent: accent)
                Text(verbatim: machine.name).font(.subheadline.weight(selected ? .semibold : .medium)).lineLimit(1)
            }
            .padding(.horizontal, 12).padding(.vertical, 7)
            .foregroundStyle(selected ? Color(ftHex: theme?.foreground ?? "#FFFFFF") : Color.primary.opacity(status == .live ? 0.9 : 0.6))
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
            .overlay(alignment: .topTrailing) { if unseen { UnseenBadge() } }
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .accessibilityLabel("\(machine.name), \(Self.describe(status))\(unseen ? ", new output" : "")")
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

/// Hosts the selected machine's asynchronously created terminal. Other
/// machines' views leave the window, which pauses their rendering; their
/// sessions keep running. The navigation controls are not drawn (the machine
/// strip replaces their rail) but own both independent canvas gestures.
private struct WorkspaceCanvasHost: UIViewRepresentable {
    let scene: WorkspaceTerminalScene
    let terminal: NativeTerminalView?
    func makeUIView(context: Context) -> WorkspaceCanvasView { WorkspaceCanvasView(scene: scene) }
    func updateUIView(_ view: WorkspaceCanvasView, context: Context) { view.show(terminal) }
    static func dismantleUIView(_ view: WorkspaceCanvasView, coordinator: ()) { view.retireNavigation() }
}

private final class WorkspaceCanvasView: UIView {
    private let canvas = UIView()
    /// Keeps the keyboard up between machines, so switching resizes neither.
    private let keyboardHolder = TerminalKeyboardHolder()
    private var controls: NativeTerminalNavigationControls?
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
        do {
            // One recognizer owns the finger-count decision and the four
            // captured candidates. Mux panes are not flattened into machines.
            let controls = try NativeTerminalNavigationControls(coordinator: scene.navigation, terminalCanvas: canvas,
                permitsCanvasNavigation: { [weak scene] in
                    scene?.permitsCanvasNavigation == true
                }, canvasAction: { [weak scene] axis, direction in
                    scene?.captureCanvasAction(axis, direction)
                })
            controls.onFailure = { [weak scene] error in scene?.report(error) }
            self.controls = controls
        } catch { scene.report(error) }
        // A tap brings the keyboard back after its input generation ended.
        let tap = UITapGestureRecognizer(target: self, action: #selector(tapped))
        tap.cancelsTouchesInView = false
        canvas.addGestureRecognizer(tap)
    }
    @available(*, unavailable)
    required init?(coder: NSCoder) { nil }

    @objc private func tapped() {
        // A live terminal handles its own taps: the keyboard, a selection.
        guard scene?.selectedSession?.terminal == nil else { return }
        Task { [weak scene] in await scene?.activateKeyboard() }
    }

    func retireNavigation() { controls?.retire(suspendNavigation: false); controls = nil }

    /// A hardware keyboard switches as the strips and swipes do: ⇧⌘] ⇧⌘[ for
    /// machines, ⌘] ⌘[ for a mux's panes, ⌘J for the next new output;
    /// ⇧⌘D composes a command. UIKit asks
    /// the terminal's ancestors, so these work while a terminal has the
    /// keyboard. The untitled shifted spellings match a keyboard that reports
    /// ⇧] as "}".
    override var keyCommands: [UIKeyCommand]? {
        guard let scene else { return nil }
        var commands: [UIKeyCommand] = []
        if let session = scene.selectedSession, session.phase == .live, session.terminal != nil {
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
        return commands
    }
    @objc private func nextUnseenOutput() { scene?.showNextUnseenOutput() }
    @objc private func composeCommand() { scene?.selectedSession?.composeCommand() }
    @objc private func nextMachine() { scene?.stepMachine(forward: true) }
    @objc private func previousMachine() { scene?.stepMachine(forward: false) }
    @objc private func nextPane() { scene?.stepPane(forward: true) }
    @objc private func previousPane() { scene?.stepPane(forward: false) }

    /// Switching machines cross-fades between their terminals.
    func show(_ terminal: NativeTerminalView?) {
        terminal?.keyboardHolder = keyboardHolder
        // A machine whose terminal is on its way takes the keyboard when it
        // comes (or the holder's deadline lets it go); its page takes none.
        if terminal == nil, scene?.selectedSession.map({ $0.phase == .idle }) ?? true { keyboardHolder.release() }
        if let terminal, terminal.superview === canvas, canvas.subviews.count == 1 { return }
        let replacing = !canvas.subviews.isEmpty
        let swap = { [canvas] in
            canvas.subviews.forEach { $0.removeFromSuperview() }
            guard let terminal else { return }
            terminal.frame = canvas.bounds
            terminal.autoresizingMask = [.flexibleWidth, .flexibleHeight]
            canvas.addSubview(terminal)
        }
        if replacing, terminal != nil, !UIAccessibility.isReduceMotionEnabled {
            UIView.transition(with: canvas, duration: 0.18, options: [.transitionCrossDissolve, .allowUserInteraction],
                              animations: swap)
        } else { swap() }
    }
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
                    Picker("Status at refresh", selection: $query.filter) {
                        Text("Any status").tag(TerminalFleetOverview.Filter.all)
                        Text("Connected or signed in").tag(TerminalFleetOverview.Filter.connected)
                        Text("Needs attention").tag(TerminalFleetOverview.Filter.attention)
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
                        prompt: "Machine, host, pane, tab, or directory")
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
                Text(verbatim: row.status.label).font(.caption2).foregroundStyle(.secondary)
            }
            Spacer(minLength: 0)
            if row.selected { Image(systemName: "checkmark.circle.fill").accessibilityLabel("Selected at refresh") }
        }
        .padding(.vertical, 4)
        .contentShape(Rectangle())
        .accessibilityElement(children: .combine)
    }
}
