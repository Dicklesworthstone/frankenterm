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
/// Every switch, from the machine strip, a menu, a two-finger swipe or a new
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
    private(set) var problem: String?
    /// One source of truth for imported opt-ins, explicit actions and losses.
    private var connectionIntent: TerminalConnectionIntent
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
    /// A retained scene can reopen at another machine. Its previous selection
    /// must not create a terminal before the requested focus handoff commits.
    @ObservationIgnored private var appearanceSelection: String?
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
         initialGroup: TerminalConnectionIntent.Group? = nil, autoConnect: Bool = true) throws {
        self.workspaceID = workspaceID; self.revision = revision; self.autoConnect = autoConnect
        self.workspaceName = workspaceName; self.machines = machines; self.connections = connections
        connectionIntent = try TerminalConnectionIntent(
            machines: machines.map { .init(id: $0.id, autoConnect: $0.autoConnect) },
            selectedMachineID: initialMachineID, group: autoConnect ? initialGroup : nil)
        themes = Dictionary(uniqueKeysWithValues: machines.compactMap { machine in machine.theme.map { (machine.id, $0) } })
        owner = try TerminalKeyboardNavigationOwner(coordinator: connections.coordinator, sceneID: UUID(),
                                                    workspaceID: workspaceID.uuidString, bindings: [])
        let empty = try machines.map { try TerminalNavigationMachine(id: $0.id, name: $0.name, panes: []) }
        navigation = TerminalNavigationCoordinator(
            navigation: try TerminalNavigation(machines: empty, initialMachineID: initialMachineID), focusOwner: owner)
        selectedMachineID = navigation.navigation.selection?.machineID
        navigation.onChange = { [weak self] in self?.navigationChanged() }
    }

    var selectedMachine: SceneMachine? { machines.first { $0.id == selectedMachineID } }
    var selectedSession: TerminalSessionController? { selectedMachineID.flatMap { sessions[$0] } }

    func status(of machineID: String) -> Status {
        guard machines.first(where: { $0.id == machineID })?.target != nil else { return .unavailable }
        if disconnected.contains(machineID) { return .disconnected }
        guard let session = sessions[machineID] else {
            return connectionIntent.shouldPrepare(machineID) ? .connecting : .disconnected
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

    func requiresNewSession(_ machineID: String) -> Bool { connectionIntent.requiresNewSession.contains(machineID) }

    // MARK: Screen lifecycle

    /// Opening a group's scene scopes new warm-up work. A deliberate Connect
    /// link approves its selected machine, without upgrading a manual route.
    func appear(selecting machineID: String, active: Bool,
                group: TerminalConnectionIntent.Group? = nil, connectOnOpen: Bool = false) {
        guard !closed else { return }
        do {
            try connectionIntent.setGroup(autoConnect ? group : nil)
            if connectOnOpen {
                try connectionIntent.approveConnection(machineID)
                if let session = sessions[machineID] {
                    switch session.phase {
                    case .ended, .failed, .couldNotConnect: requestedConnections.insert(machineID)
                    case .idle, .connecting, .ready, .live: break
                    }
                } else { requestedConnections.insert(machineID) }
                cancelReconnect(machineID)
                reconnectPolicies[machineID, default: .init()].resetForUserRequest()
            }
        } catch { report(error); return }
        appearanceSelection = machineID == selectedMachineID ? nil : machineID
        onScreen = true
        setForeground(active)
        if machineID != selectedMachineID { select(machineID) }
        else if let session = selectedSession, session.phase == .live {
            // Back on a live terminal: its view, or its keyboard, right away.
            if session.terminal == nil { session.showView() } else { Task { await activateKeyboard() } }
        }
        reconcile()
    }

    /// Leaving the terminal screen keeps every session running; only the
    /// keyboard and rendering stop until it returns.
    func disappear() {
        guard !closed else { return }
        onScreen = false
        appearanceSelection = nil
        navigation.suspend()
        updatePresentationVisibility()
    }

    func setForeground(_ active: Bool) {
        guard !closed else { return }
        foreground = active
        if active {
            if inBackground {
                for (id, session) in sessions where session.phase == .live {
                    reconnectPolicies[id, default: .init()].recordLive()
                }
            }
            inBackground = false
        }
        if active && onScreen { navigation.resume() } else { navigation.suspend() }
        updatePresentationVisibility()
        guard active else { return }
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
        guard !closed, machineID != selectedMachineID else { return }
        Task {
            do {
                try await whileBusy {
                    guard let plan = try self.navigation.navigation.plan(machineID: machineID) else { return }
                    try await self.navigation.perform(plan)
                }
            } catch { report(error) }
        }
    }

    /// Steps through the selected machine's mux panes, and past either end
    /// on to the next or previous machine, so one swipe direction walks
    /// every pane of every machine in order.
    func stepPane(forward: Bool) {
        guard !closed else { return }
        if let session = selectedSession, session.phase == .live, let mux = session.muxState, let shown = mux.shown,
           let index = mux.panes.firstIndex(where: { $0.paneId == shown }) {
            let next = index + (forward ? 1 : -1)
            if mux.panes.indices.contains(next) { session.showPane(mux.panes[next].paneId); return }
        }
        let ids = machines.map(\.id)
        guard ids.count > 1, let current = selectedMachineID, let index = ids.firstIndex(of: current) else { return }
        select(ids[(index + (forward ? 1 : ids.count - 1)) % ids.count])
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
    func disconnect(_ machineID: String) {
        guard !closed else { return }
        do { try connectionIntent.disconnect(machineID) }
        catch { report(error); return }
        requestedConnections.remove(machineID)
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
            guard !disconnected.contains(id), controllerJobs[id] == nil else { continue }
            if let session = sessions[id] {
                guard session.settled else { continue }
                if !requestedConnections.contains(id) {
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
            // The manager owns the authoritative limits. Avoid turning a large
            // group's queued warm-up into failures when its 16 owners are full.
            guard slots > 0, sessions.values.filter({ !$0.settled }).count < 16 else { continue }
            let retry = sessions[id] != nil && !requestedConnections.contains(id)
            if retry {
                guard reconnectPolicies[id, default: .init()].beginRetry() else { continue }
                reconnectJobs[id] = nil
            }
            startConnection(id, preservingSuccessfulCandidate: retry); slots -= 1
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
        guard !closed, onScreen, selectedSession?.clipboardIsPresenting != true,
              navigation.phase == .inactive, let selection = navigation.navigation.selection,
              selection.paneID != nil, sessions[selection.machineID]?.phase == .live else { return }
        await owner.joinSealed()
        guard !closed, foreground, navigation.navigation.selection == selection,
              selectedSession?.clipboardIsPresenting != true else { return }
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
        let job = Task {
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
        if selectedMachineID != selection?.machineID {
            selectedMachineID = selection?.machineID
            do { try connectionIntent.select(selectedMachineID) } catch { report(error) }
        }
        if appearanceSelection == selectedMachineID { appearanceSelection = nil }
        updatePresentationVisibility()
        if onScreen, let session = selectedSession, session.phase == .live, session.terminal == nil { session.showView() }
        reconcile()
        guard navigation.phase == .ready, let selection, selection.paneID != nil,
              let keyboard = owner.keyboard else { return }
        sessions[selection.machineID]?.install(keyboard)
    }

    private func isVisible(_ machineID: String) -> Bool {
        permitsTerminalCreation && onScreen && appearanceSelection == nil && machineID == selectedMachineID
    }

    private func updatePresentationVisibility() {
        for (machineID, controller) in sessions { controller.setPresentationVisible(isVisible(machineID)) }
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

    var permitsTerminalCreation: Bool { !closed && foreground && !inBackground }

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
    let makeScene: @MainActor () throws -> WorkspaceTerminalScene
    @State private var scene: WorkspaceTerminalScene?
    @State private var failed = false

    var body: some View {
        Group {
            if let scene {
                WorkspaceTerminalScreen(scene: scene, initialMachineID: initialMachineID, themes: themes,
                                        group: group, connectOnOpen: connectOnOpen)
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
            if let session = scene.selectedSession, let problem = session.interactionProblem {
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
        .toolbarBackground(.visible, for: .navigationBar)
        .toolbarColorScheme(TerminalSessionTarget.isDark(backgroundHex) ? .dark : .light, for: .navigationBar)
        .task {
            let connect = connectOnOpen && !initialConnectionConsumed
            initialConnectionConsumed = true
            scene.appear(selecting: initialMachineID, active: scenePhase == .active,
                         group: group, connectOnOpen: connect)
        }
        .onDisappear {
            historyInspection?.retire(); historyInspection = nil; retainedOutput = nil
            scene.disappear()
        }
        .onChange(of: scenePhase) { _, phase in
            // The app forwards its phase to the scene (MobileWorkspaceModel).
            if phase != .active { historyInspection?.retire(); historyInspection = nil; retainedOutput = nil }
        }
    }

    @ToolbarContentBuilder private func toolbar(_ machine: SceneMachine?) -> some ToolbarContent {
        ToolbarItemGroup(placement: .topBarTrailing) {
            if let session = scene.selectedSession, session.phase == .live, session.terminal != nil {
                if session.historyReturnBookmark != nil {
                    Button("Return to Previous View", systemImage: "arrow.uturn.backward") { session.returnFromHistorySearch() }
                        .disabled(session.clipboardIsPresenting)
                }
                Button("Search Output", systemImage: "magnifyingglass") {
                    if session.muxControl != nil { muxSearch = session } else { historyInspection = session.inspectHistory() }
                }
                    .disabled(session.clipboardIsPresenting)
                    .keyboardShortcut("f", modifiers: .command)
                    .accessibilityIdentifier("terminal.history.open")
                Button("Paste", systemImage: "doc.on.clipboard") { session.paste() }
                    .disabled(session.clipboardIsPresenting)
            }
            Menu {
                if let session = scene.selectedSession, session.phase == .live, let terminal = session.terminal {
                    Button("Select Output", systemImage: "doc.on.doc") { session.inspectOutput() }
                        .disabled(session.clipboardIsPresenting)
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
/// raised. Tap a chip to switch; swipe the terminal with two fingers to step
/// through them in order.
private struct MachineStrip: View {
    let scene: WorkspaceTerminalScene
    @Namespace private var raised

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 8) {
                    ForEach(scene.machines) { machine in chip(machine).id(machine.id) }
                }
                .padding(.horizontal, 12).padding(.vertical, 8)
            }
            .onChange(of: scene.selectedMachineID, initial: true) { _, id in
                withAnimation(.snappy) { proxy.scrollTo(id, anchor: .center) }
            }
        }
        .background(.ultraThinMaterial)
    }

    private func chip(_ machine: SceneMachine) -> some View {
        let theme = scene.themes[machine.id]
        let accent = Color(ftHex: theme?.accent ?? "#8E8E93")
        let selected = machine.id == scene.selectedMachineID
        let status = scene.status(of: machine.id)
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
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .accessibilityLabel("\(machine.name), \(Self.describe(status))")
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
            .contentShape(RoundedRectangle(cornerRadius: 9))
        }
        .buttonStyle(.plain)
        .accessibilityLabel(names.detail.map { "\(names.place), \($0)" } ?? names.place)
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
/// strip replaces their rail) but keep the canvas's two-finger swipe.
private struct WorkspaceCanvasHost: UIViewRepresentable {
    let scene: WorkspaceTerminalScene
    let terminal: NativeTerminalView?
    func makeUIView(context: Context) -> WorkspaceCanvasView { WorkspaceCanvasView(scene: scene) }
    func updateUIView(_ view: WorkspaceCanvasView, context: Context) { view.show(terminal) }
}

private final class WorkspaceCanvasView: UIView, UIGestureRecognizerDelegate {
    private let canvas = UIView()
    private var controls: NativeTerminalNavigationControls?
    private weak var scene: WorkspaceTerminalScene?

    init(scene: WorkspaceTerminalScene) {
        self.scene = scene
        super.init(frame: .zero)
        canvas.translatesAutoresizingMaskIntoConstraints = false
        addSubview(canvas)
        NSLayoutConstraint.activate([
            canvas.topAnchor.constraint(equalTo: topAnchor), canvas.bottomAnchor.constraint(equalTo: bottomAnchor),
            canvas.leadingAnchor.constraint(equalTo: leadingAnchor), canvas.trailingAnchor.constraint(equalTo: trailingAnchor)
        ])
        do {
            // A machine attached to its mux steps through its panes instead
            // (paneSwipe below); the others, and one whose session ended,
            // keep Kit's two-finger navigation.
            let controls = try NativeTerminalNavigationControls(coordinator: scene.navigation, terminalCanvas: canvas,
                permitsCanvasNavigation: { [weak scene] in
                    guard let session = scene?.selectedSession else { return true }
                    return !session.clipboardIsPresenting && (session.muxState == nil || session.phase != .live)
                })
            controls.onFailure = { [weak scene] error in scene?.report(error) }
            self.controls = controls
        } catch { scene.report(error) }
        for direction in [UISwipeGestureRecognizer.Direction.left, .right] {
            let swipe = UISwipeGestureRecognizer(target: self, action: #selector(paneSwipe(_:)))
            swipe.direction = direction
            swipe.numberOfTouchesRequired = 2
            swipe.delegate = self
            canvas.addGestureRecognizer(swipe)
        }
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

    /// Swiping left shows the next pane, right the previous one.
    @objc private func paneSwipe(_ swipe: UISwipeGestureRecognizer) {
        scene?.stepPane(forward: swipe.direction == .left)
    }

    override func gestureRecognizerShouldBegin(_ gestureRecognizer: UIGestureRecognizer) -> Bool {
        guard gestureRecognizer is UISwipeGestureRecognizer else { return super.gestureRecognizerShouldBegin(gestureRecognizer) }
        guard let session = scene?.selectedSession else { return false }
        return session.phase == .live && session.muxState != nil && !session.clipboardIsPresenting
    }

    /// Two fingers sliding sideways rarely keep their spacing exactly: the
    /// terminal's pinch, which ignores small changes, must not block the swipe.
    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer,
                           shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool {
        gestureRecognizer is UISwipeGestureRecognizer && other is UIPinchGestureRecognizer
    }

    /// Switching machines cross-fades between their terminals.
    func show(_ terminal: NativeTerminalView?) {
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
