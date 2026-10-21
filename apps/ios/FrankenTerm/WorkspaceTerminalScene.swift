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
}

/// A frozen list of exact routes presented before any group DNS/authentication.
/// This review never grants permission to create a PTY or resolve old effects.
struct GroupConnectionReview: Identifiable {
    struct Request: Identifiable, Sendable {
        let target: TerminalSessionTarget
        let route: TerminalSessionRoute
        var id: String { target.machineID }
    }
    let id = UUID()
    let requests: [Request]
}

/// Keeps one live direct-SSH session per connected machine of a workspace and
/// gives the scene's single keyboard to exactly the selected machine. Every
/// switch, from the rail, a menu, a two-finger swipe or a new connection, goes
/// through Kit's navigation coordinator: the old source's composition and held
/// keys settle behind a focus barrier before the new source can accept input.
/// Machines without a session stay in the ring and show how to connect.
@MainActor
@Observable
final class WorkspaceTerminalScene: TerminalSessionFocus {
    let workspaceName: String
    let machines: [SceneMachine]
    private(set) var sessions: [String: TerminalSessionController] = [:]
    /// Each machine's theme now; a pick in the scene changes it live.
    private(set) var themes: [String: WorkspaceDocument.Theme] = [:]
    /// The machine on screen; follows committed navigation only.
    private(set) var selectedMachineID: String?
    private(set) var problem: String?
    private(set) var groupStates: [String: MobileConnectionBatch.State] = [:]
    private(set) var groupFailures: [String: String] = [:]
    private(set) var groupBusy = false
    private(set) var groupReady = 0
    private(set) var groupClosing = false

    @ObservationIgnored let connections: MobileConnections
    @ObservationIgnored let owner: TerminalKeyboardNavigationOwner
    @ObservationIgnored let navigation: TerminalNavigationCoordinator
    @ObservationIgnored private var bindings: [String: TerminalKeyboardNavigationBinding] = [:]
    @ObservationIgnored private var closed = false
    @ObservationIgnored private var foreground = true
    @ObservationIgnored private var closing: Task<Void, Never>?
    @ObservationIgnored private var group: MobileConnectionBatch?
    @ObservationIgnored private var groupClose: Task<Void, Never>?
    @ObservationIgnored private var groupApprovals: [String: GroupConnectionReview.Request] = [:]
    @ObservationIgnored private var pendingGroupReview: UUID?

    init(workspaceID: UUID, workspaceName: String, machines: [SceneMachine], initialMachineID: String,
         connections: MobileConnections) throws {
        self.workspaceName = workspaceName; self.machines = machines; self.connections = connections
        themes = Dictionary(uniqueKeysWithValues: machines.compactMap { machine in machine.theme.map { (machine.id, $0) } })
        owner = try TerminalKeyboardNavigationOwner(coordinator: connections.coordinator, sceneID: UUID(),
                                                    workspaceID: workspaceID.uuidString, bindings: [])
        let empty = try machines.map { try TerminalNavigationMachine(id: $0.id, name: $0.name, panes: []) }
        navigation = TerminalNavigationCoordinator(
            navigation: try TerminalNavigation(machines: empty, initialMachineID: initialMachineID), focusOwner: owner)
        selectedMachineID = navigation.navigation.selection?.machineID
        navigation.onChange = { [weak self] in self?.navigationChanged() }
        navigation.resume()
    }

    var selectedMachine: SceneMachine? { machines.first { $0.id == selectedMachineID } }
    var selectedSession: TerminalSessionController? { selectedMachineID.flatMap { sessions[$0] } }

    var canPrepareGroup: Bool {
        !closed && foreground && !groupClosing && group?.isRetired != true
            && pendingGroupReview == nil && (navigation.phase == .ready || navigation.phase == .inactive)
            && selectedSession?.clipboardIsPresenting != true
            && !sessions.values.contains(where: { $0.phase == .connecting || $0.phase == .idle })
            && machines.contains(where: { canPrepare($0.id) })
    }
    private func canPrepare(_ machineID: String) -> Bool {
        guard sessions[machineID] == nil,
              machines.first(where: { $0.id == machineID })?.target != nil else { return false }
        switch groupStates[machineID] {
        case nil, .failed, .cancelled: return true
        default: return false
        }
    }

    func reviewGroupConnections() -> GroupConnectionReview? {
        guard canPrepareGroup else { return nil }
        do {
            let eligible = Set(machines.filter { canPrepare($0.id) }.map(\.id))
            let order = try TerminalConnectionOrder.prioritize(machines.map(\.id),
                selected: selectedMachineID, including: eligible)
            let requests = order.compactMap { id -> GroupConnectionReview.Request? in
                guard let target = machines.first(where: { $0.id == id })?.target,
                      let route = target.attemptOrder().first else { return nil }
                return .init(target: target, route: route)
            }
            guard !requests.isEmpty else { return nil }
            let review = GroupConnectionReview(requests: requests)
            pendingGroupReview = review.id
            // No hardware typing behind the review sheet. Returning to the
            // terminal uses the existing explicit keyboard activation path.
            selectedSession?.terminal?.input.retire()
            return review
        } catch { report(error); return nil }
    }
    func cancelGroupReview(id: UUID? = nil) {
        if id == nil || pendingGroupReview == id { pendingGroupReview = nil }
    }

    func connectGroup(_ review: GroupConnectionReview) {
        guard !closed, foreground, pendingGroupReview == review.id,
              groupClose == nil, group?.isRetired != true else { return }
        pendingGroupReview = nil
        guard !review.requests.isEmpty, review.requests.count <= 128,
              Set(review.requests.map(\.id)).count == review.requests.count,
              review.requests.allSatisfy({ request in
                  canPrepare(request.id) && machines.first(where: { $0.id == request.id })?.target == request.target
                      && request.target.routes.contains(request.route)
              }) else {
            report(WorkspaceSSHConnectionError.stale); return
        }
        do {
            let batch: MobileConnectionBatch
            if let existing = group { batch = existing }
            else {
                let manager = connections.manager
                // Leave one of the manager's three attempt slots for an
                // individually selected terminal. Its global cap still applies.
                batch = try MobileConnectionBatch(maximumConcurrent: 2, connect: { [weak self] id in
                    guard let self, !self.closed, self.foreground,
                          self.sessions[id] == nil, let request = self.groupApprovals[id] else {
                        throw WorkspaceSSHConnectionError.stale
                    }
                    // Freeze before the first suspension. Only the reviewed
                    // endpoint is attempted; no fallback, shell or focus change.
                    let target = request.target, route = request.route
                    let connection = try await manager.connect(workspaceID: target.workspaceID,
                        expectedRevision: target.revision, machineID: target.machineID,
                        routeID: route.id, credentialID: route.credentialID, confirmedByUser: true)
                    // Return even when cancellation races success: the batch
                    // must receive ownership so it can join exact native cleanup.
                    return PreparedTerminalConnection(connection: connection, route: route)
                }, dispose: { prepared in
                    try await manager.close(connectionID: prepared.connection.id)
                })
                try connections.registerGroup(batch)
                group = batch
                batch.onChange = { [weak self, weak batch] in
                    guard let self, let batch, self.group === batch else { return }
                    self.groupStates = batch.states
                    self.groupBusy = batch.isBusy; self.groupReady = batch.readyCount
                }
                batch.onFailure = { [weak self] id, error in
                    self?.groupFailures[id] = String(TerminalSessionController.describe(error).prefix(512))
                }
            }
            for request in review.requests {
                groupApprovals[request.id] = request; groupFailures[request.id] = nil
            }
            try batch.enqueue(review.requests.map(\.id))
        } catch { report(error) }
    }

    func cancelQueuedConnections() { group?.cancelPending() }
    func dismissProblem() { problem = nil }

    /// Ready transports are not resurrected across deactivation. The app-owned
    /// registry retains failures even after this scene disappears.
    func closePreparedConnections() {
        guard let group else { return }
        group.retire()
        guard groupClose == nil else { return }
        groupClosing = true
        let connections = self.connections
        groupClose = Task { [weak self] in
            let finished = await connections.closeGroup(group)
            guard let self else { return }
            if self.group === group, finished {
                self.group = nil; self.groupApprovals.removeAll()
            }
            self.groupClose = nil
            self.groupClosing = false
            if !finished { self.problem = "Some prepared connections still need local cleanup. Retry closing prepared connections." }
        }
    }

    /// Shows a machine in another theme: its chrome at once, and its
    /// terminal from the next frame. A later session starts in it.
    func applyTheme(_ theme: WorkspaceDocument.Theme, to machineID: String) {
        guard !closed, machines.contains(where: { $0.id == machineID }) else { return }
        themes[machineID] = theme
        sessions[machineID]?.applyPalette(theme.terminalBasePalette)
    }

    /// Opens a session for a machine, replacing one that ended. Acknowledging
    /// records that the user read the earlier session's unknown outcome.
    func connect(_ machineID: String, acknowledging: Bool = false) {
        guard !closed, foreground, pendingGroupReview == nil,
              var target = machines.first(where: { $0.id == machineID })?.target else { return }
        switch groupStates[machineID] {
        case .queued, .connecting, .closing, .cleanupFailed: return
        default: break
        }
        if let theme = themes[machineID] { target = target.wearing(theme) }
        // A session that has not settled (starting, live, tearing down) blocks a second one.
        if let current = sessions[machineID], !current.settled { return }
        let reviewed: [TerminalSessionRecord]
        if acknowledging {
            guard let current = sessions[machineID], current.recoveryReviewReady else { return }
            reviewed = current.recoveryRecords
        } else { reviewed = [] }
        sessions[machineID]?.discardRetainedOutput()
        let prepared = group?.take(machineID)
        let controller = TerminalSessionController(target: target, connections: connections, focus: self, prepared: prepared)
        sessions[machineID] = controller
        controller.setPresentationVisible(foreground && machineID == selectedMachineID)
        Task { await controller.start(reviewingEarlierSessions: reviewed) }
    }

    /// Re-establishes the keyboard after its input generation ended (dismissed
    /// keyboard, an alert, scene deactivation). Never reconnects anything.
    func activateKeyboard() async {
        guard !closed, pendingGroupReview == nil, selectedSession?.clipboardIsPresenting != true,
              navigation.phase == .inactive, let selection = navigation.navigation.selection,
              selection.paneID != nil, sessions[selection.machineID]?.phase == .live else { return }
        await owner.joinSealed()
        guard !closed, foreground, pendingGroupReview == nil, navigation.navigation.selection == selection,
              selectedSession?.clipboardIsPresenting != true else { return }
        do { try await navigation.activateSelection() } catch { report(error) }
    }

    func setForeground(_ active: Bool) {
        guard !closed else { return }
        foreground = active
        if !active { pendingGroupReview = nil; closePreparedConnections() }
        else if group?.isRetired == true { closePreparedConnections() }
        if active { navigation.resume() } else { navigation.suspend() }
        updatePresentationVisibility()
        // Back from turning Tailscale on: try the machine that needed it, once
        // the app's own resume has let connections through again.
        if active, let id = selectedMachineID, needsTailscale(id), WorkspaceRouteNetwork.deviceTailnetAddress() != nil {
            let manager = connections.manager
            Task { [weak self] in
                for _ in 0..<20 where await !manager.permitsConnections { try? await Task.sleep(for: .milliseconds(100)) }
                guard let self, !self.closed, self.foreground, self.needsTailscale(id) else { return }
                self.connect(id)
            }
        }
    }

    private func needsTailscale(_ machineID: String) -> Bool {
        guard case .couldNotConnect(let help) = sessions[machineID]?.phase else { return false }
        return help.remedy == .turnOnTailscale
    }

    /// Synchronously seal every scene-owned responder before scheduling joins.
    func requestClose() {
        guard !closed else { return }
        closed = true
        pendingGroupReview = nil
        closePreparedConnections()
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
        let job = Task {
            for controller in controllers { await controller.stop() }
            await navigation.joinSuspension()
            if let groupClose = self.groupClose { await groupClose.value }
        }
        closing = job
        await job.value
        closing = nil
    }

    func report(_ error: any Error) {
        problem = TerminalSessionController.describe(error)
    }

    private func inventory(_ values: [String: TerminalKeyboardNavigationBinding]) throws -> [TerminalNavigationMachine] {
        try machines.map { machine in
            let panes = try values[machine.id].map { [try TerminalNavigationPane(id: $0.paneID, title: "Direct SSH")] }
            return try TerminalNavigationMachine(id: machine.id, name: machine.name, panes: panes ?? [])
        }
    }

    /// Committed navigation: show its machine and, once the destination's focus
    /// is published, put the new keyboard into that machine's view.
    private func navigationChanged() {
        let selection = navigation.navigation.selection
        if selectedMachineID != selection?.machineID { selectedMachineID = selection?.machineID }
        if let selectedMachineID { group?.promote(selectedMachineID) }
        updatePresentationVisibility()
        guard navigation.phase == .ready, let selection, selection.paneID != nil,
              let keyboard = owner.keyboard else { return }
        guard pendingGroupReview == nil else { owner.seal(); return }
        sessions[selection.machineID]?.install(keyboard)
    }

    private func updatePresentationVisibility() {
        for (machineID, controller) in sessions {
            controller.setPresentationVisible(!closed && foreground && machineID == selectedMachineID)
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

    func inputRetired(machineID: String) {
        // A handoff retires the old keyboard itself and reports it synchronously;
        // sealing then would invalidate the switch in flight. Only a settled
        // selection's lost input (dismissal, resign, OS) is sealed here.
        guard navigation.phase == .ready, owner.currentSelection?.machineID == machineID,
              owner.keyboard?.acceptsInput != true else { return }
        owner.seal()
    }
}

/// How a live scene reads and records theme picks; the workspace owns them.
struct SceneThemeSource {
    let themes: @MainActor () -> MachineThemes?
    let choose: @MainActor (_ machineID: String, _ choice: MachineThemeChoice) -> Void
}

/// Builds the live scene once, when its screen is actually shown. A
/// NavigationLink evaluates its destination on every redraw of the page that
/// holds it, so constructing the scene there built and dropped one per redraw.
struct LiveWorkspaceView: View {
    let initialMachineID: String
    var themes: SceneThemeSource? = nil
    let makeScene: @MainActor () throws -> WorkspaceTerminalScene
    @State private var scene: WorkspaceTerminalScene?
    @State private var failed = false

    var body: some View {
        Group {
            if let scene { WorkspaceTerminalScreen(scene: scene, initialMachineID: initialMachineID, themes: themes) }
            else if failed { Text("These machines could not be opened together.").padding() }
            else { Color.black.ignoresSafeArea() }
        }
        .onAppear {
            guard scene == nil, !failed else { return }
            do { scene = try makeScene() } catch { failed = true }
        }
    }
}

/// The live workspace: the selected machine's terminal under a machine/pane
/// rail, or that machine's connect, progress, review or ended card.
struct WorkspaceTerminalScreen: View {
    @State var scene: WorkspaceTerminalScene
    let initialMachineID: String
    var themes: SceneThemeSource? = nil
    @Environment(\.scenePhase) private var scenePhase
    @State private var pickingTheme = false
    @State private var historyInspection: TerminalHistoryInspection?
    @State private var retainedOutput: TerminalRetainedOutput?
    @State private var groupReview: GroupConnectionReview?

    var body: some View {
        let machine = scene.selectedMachine
        let backgroundHex = machine.flatMap { scene.themes[$0.id]?.background }
        let background = backgroundHex.map { Color(ftHex: $0) } ?? .black
        ZStack {
            background.ignoresSafeArea()
            WorkspaceCanvasHost(scene: scene, terminal: machine.flatMap { scene.sessions[$0.id]?.terminal })
            if let machine { card(for: machine) }
        }
        .overlay(alignment: .bottom) {
            if let session = scene.selectedSession, let problem = session.interactionProblem {
                VStack(spacing: 8) {
                    Text(verbatim: problem).font(.callout)
                    Button("Dismiss") { session.dismissInteractionProblem() }
                }.padding().background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12)).padding()
            } else if let problem = scene.problem {
                VStack(spacing: 8) {
                    Text(verbatim: problem).font(.callout)
                    Button("Dismiss") { scene.dismissProblem() }
                }.padding().background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12)).padding()
            }
        }
        .navigationTitle(machine?.name ?? scene.workspaceName)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItemGroup(placement: .topBarTrailing) {
                Menu {
                    Button("Authenticate Remaining Machines…", systemImage: "network") {
                        groupReview = scene.reviewGroupConnections()
                    }.disabled(!scene.canPrepareGroup)
                    if scene.groupBusy {
                        Button("Cancel Pending Authentication", systemImage: "stop.circle") { scene.cancelQueuedConnections() }
                    }
                    if scene.groupReady > 0 || scene.groupStates.values.contains(.cleanupFailed) {
                        Button("Close Unopened Connections", systemImage: "xmark.circle") { scene.closePreparedConnections() }
                    }
                    Text("\(scene.groupReady) authenticated without a terminal")
                    if scene.groupBusy { Text("Group connection work is in progress") }
                } label: {
                    Label("Group Connections", systemImage: "network")
                }
                .accessibilityIdentifier("terminal.group.connections")
                if let session = scene.selectedSession, session.phase == .live {
                    if session.historyReturnBookmark != nil {
                        Button("Return to Previous View", systemImage: "arrow.uturn.backward") { session.returnFromHistorySearch() }
                            .disabled(session.clipboardIsPresenting)
                    }
                    Button("Search Output", systemImage: "magnifyingglass") { historyInspection = session.inspectHistory() }
                        .disabled(session.clipboardIsPresenting)
                        .accessibilityIdentifier("terminal.history.open")
                    Button("Select Output", systemImage: "doc.on.doc") { session.inspectOutput() }
                        .disabled(session.clipboardIsPresenting)
                    Button("Paste", systemImage: "doc.on.clipboard") { session.paste() }
                        .disabled(session.clipboardIsPresenting)
                }
                if themes != nil, machine != nil {
                    Button("Theme", systemImage: "paintpalette") { pickingTheme = true }
                        .accessibilityIdentifier("scene.theme")
                }
            }
        }
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
        .sheet(item: $retainedOutput) { output in TerminalRetainedOutputScreen(output: output) }
        .sheet(item: $groupReview) { review in
            NavigationStack {
                List {
                    Section {
                        Text("Authenticate to these machines using their imported SSH identities. Keychain approval may be requested for each connection.")
                        Text("No shells or commands will be started. Choose Open Terminal on a machine afterwards. Existing terminals and keyboard destinations stay unchanged.")
                        Text("Two group attempts run at once, leaving room for a foreground connection. The app's existing 16-connection limit still applies. A rejected route is reported, not retried automatically.")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    Section("Reviewed Routes") {
                        ForEach(review.requests) { request in
                            VStack(alignment: .leading, spacing: 5) {
                                Text(verbatim: request.target.machineName).font(.headline)
                                Text(verbatim: "\(request.route.host):\(request.route.port)").font(.callout)
                                Text(verbatim: "Machine \(request.id) · Route \(request.route.id) · Identity \(request.route.credentialID)")
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                        }
                    }
                }
                .navigationTitle("Connect Group")
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Cancel") { scene.cancelGroupReview(id: review.id); groupReview = nil }
                    }
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Authenticate \(review.requests.count)") {
                            scene.connectGroup(review); groupReview = nil
                        }.accessibilityIdentifier("terminal.group.confirm")
                    }
                }
                .onDisappear { scene.cancelGroupReview(id: review.id) }
            }
        }
        .toolbarBackground(.visible, for: .navigationBar)
        .toolbarColorScheme(TerminalSessionTarget.isDark(backgroundHex) ? .dark : .light, for: .navigationBar)
        .task {
            scene.setForeground(scenePhase == .active)
            scene.connect(initialMachineID)
        }
        .onDisappear {
            historyInspection?.retire(); historyInspection = nil; retainedOutput = nil
            groupReview = nil; scene.cancelGroupReview()
            let retiring = scene
            retiring.requestClose()
            Task { await retiring.close() }
        }
        .onChange(of: scenePhase) { _, phase in
            if phase != .active {
                historyInspection?.retire(); historyInspection = nil; retainedOutput = nil
                groupReview = nil; scene.cancelGroupReview()
            }
            scene.setForeground(phase == .active)
        }
    }

    @ViewBuilder private func card(for machine: SceneMachine) -> some View {
        if let session = scene.sessions[machine.id] {
            switch session.phase {
            case .idle, .connecting:
                ProgressView {
                    Text("Connecting to \(machine.name)…")
                    Text(verbatim: Self.via(session.route)).font(.caption).foregroundStyle(.white.opacity(0.7))
                }.tint(.white).foregroundStyle(.white)
            case .live:
                EmptyView()
            case let .couldNotConnect(help):
                connectionHelp(help, settled: session.settled) { scene.connect(machine.id) }
            case .needsReview:
                notice(symbol: "questionmark.circle", title: "Earlier session on this machine",
                       message: "A previous session may have left remote effects whose outcome is unknown.",
                       uncertain: true, enabled: session.settled, cleanup: session) { scene.connect(machine.id, acknowledging: true) }
            case let .ended(message):
                notice(symbol: "checkmark.circle", title: "Session ended", message: message, uncertain: true,
                       enabled: session.settled, cleanup: session) { scene.connect(machine.id, acknowledging: true) }
            case let .failed(message):
                notice(symbol: "exclamationmark.triangle", title: "Session unavailable", message: message, uncertain: true,
                       enabled: session.settled, cleanup: session) { scene.connect(machine.id, acknowledging: true) }
            }
        } else if machine.target != nil {
            switch scene.groupStates[machine.id] {
            case .queued, .connecting:
                notice(symbol: "network", title: scene.groupStates[machine.id] == .queued ? "Queued for authentication" : "Authenticating…",
                       message: "No terminal will be opened until you request one. Group Connections can cancel pending attempts.",
                       uncertain: false, enabled: false, action: nil) {}
            case .ready:
                notice(symbol: "checkmark.shield", title: "Authenticated — no terminal opened",
                       message: "Open a new terminal using this prepared connection. Current host and credential policy will be checked again.",
                       uncertain: false, enabled: true, action: "Open Terminal") { scene.connect(machine.id) }
            case .closing, .cleanupFailed:
                notice(symbol: "hourglass", title: "Closing prepared connection",
                       message: scene.groupFailures[machine.id] ?? "Native connection work must settle before another attempt.",
                       uncertain: false, enabled: false, action: nil) {}
            default:
                notice(symbol: "terminal", title: "Not connected",
                       message: scene.groupFailures[machine.id] ?? "Swipe the rail to return to a connected machine.",
                       uncertain: false, enabled: true, action: "Connect") { scene.connect(machine.id) }
            }
        } else {
            notice(symbol: "network.slash", title: "Not available here", message: machine.unavailable,
                   uncertain: false, enabled: false, action: nil) {}
        }
    }

    /// "over Tailscale · 100.91.120.17", naming how the route travels.
    static func via(_ route: TerminalSessionRoute) -> String {
        switch WorkspaceRouteNetwork(host: route.host) {
        case .tailnet: "over Tailscale · \(route.host)"
        case .localNetwork: "on the local network · \(route.host)"
        case .internet: route.host
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
            .background(.black.opacity(0.8), in: RoundedRectangle(cornerRadius: 20)).padding()
            .accessibilityElement(children: .contain)
    }

    /// The Tailscale app when it is installed, otherwise its App Store page.
    static func openTailscale() {
        guard let app = URL(string: "tailscale://"),
              let store = URL(string: "https://apps.apple.com/app/id1470499037") else { return }
        UIApplication.shared.open(app) { opened in if !opened { UIApplication.shared.open(store) } }
    }

    /// Every way out of a session states that its last input may or may not
    /// have run. Inspecting retained output never creates a new session.
    private func notice(symbol: String, title: String, message: String, uncertain: Bool, enabled: Bool,
                        action: String? = "Start New Session", cleanup: TerminalSessionController? = nil,
                        perform: @escaping () -> Void) -> some View {
        VStack(spacing: 12) {
            Image(systemName: symbol).font(.largeTitle)
            Text(title).font(.headline)
            Text(verbatim: message).multilineTextAlignment(.center).font(.callout)
            if let output = cleanup?.retainedOutput {
                Button("View Retained Output", systemImage: "doc.text.magnifyingglass") { retainedOutput = output }
                    .buttonStyle(.bordered)
                    .accessibilityIdentifier("terminal.retained.open")
            } else if let failure = cleanup?.retainedOutputFailure {
                Text("A complete received viewport could not be retained: \(failure)")
                    .multilineTextAlignment(.center).font(.caption)
            }
            if uncertain {
                Text("The session marker does not prove that a particular command was sent or executed. Nothing is sent again automatically; check the machine before repeating a command.")
                    .multilineTextAlignment(.center).font(.caption)
            }
            if uncertain, let cleanup {
                if cleanup.recoveryLoading {
                    ProgressView("Reading saved session history…")
                } else if let failure = cleanup.recoveryFailure {
                    Text("Saved session history is unavailable: \(failure)")
                        .multilineTextAlignment(.center).font(.caption)
                    Button("Retry Session Review") { Task { await cleanup.refreshRecoveryReview() } }
                        .buttonStyle(.bordered)
                } else if !cleanup.recoveryRecords.isEmpty {
                    DisclosureGroup("Review \(cleanup.recoveryRecords.count) saved session records") {
                        ScrollView {
                            LazyVStack(alignment: .leading, spacing: 14) {
                                ForEach(cleanup.recoveryRecords) { record in
                                    VStack(alignment: .leading, spacing: 6) {
                                        Text(verbatim: "Route \(record.scope.routeID) · Session \(record.id.uuidString.prefix(8))")
                                            .font(.caption.bold())
                                        Text(verbatim: "Workspace revision \(record.scope.workspaceRevision.uuidString)")
                                            .font(.caption2)
                                        if record.inputEvidence.isEmpty {
                                            Text("Only the session-start marker is available. It does not establish whether any input was sent.")
                                        }
                                        if record.inputEvidence.contains(.transportAccepted) {
                                            Text("Some input crossed the SSH transport. This does not confirm that the remote program processed it.")
                                        }
                                        if record.inputEvidence.contains(.acceptedPrefix) {
                                            Text("Some input or cleanup bytes were accepted. Their remote effects remain unconfirmed.")
                                        }
                                        if record.inputEvidence.contains(.unknownTransportOutcome) {
                                            Text("Some in-flight input has an unknown transport outcome.")
                                        }
                                        if record.inputEvidence.contains(.knownUnsent) {
                                            Text("Some input was confirmed unsent. Other operations may still have had an effect.")
                                        }
                                        if !record.canResolve {
                                            Text("An active owner must finish this session's local cleanup before it can be acknowledged.")
                                        }
                                    }.font(.caption)
                                }
                            }
                        }.frame(maxHeight: 220)
                    }.font(.caption)
                    Text("Starting a new session acknowledges these saved records. It does not confirm or repeat their remote effects.")
                        .multilineTextAlignment(.center).font(.caption)
                }
                if cleanup.settled, !cleanup.recoveryReviewReady, !cleanup.recoveryLoading,
                   cleanup.recoveryFailure == nil {
                    Button("Refresh Session Review") { Task { await cleanup.refreshRecoveryReview() } }
                        .buttonStyle(.bordered)
                }
            }
            if cleanup?.interruptedInput == true {
                Text("Some input was interrupted. Nothing will be replayed automatically.")
                    .multilineTextAlignment(.center).font(.caption)
            }
            if let cleanup, let failure = cleanup.cleanupFailure {
                Text("Local cleanup is incomplete: \(failure)").font(.caption).multilineTextAlignment(.center)
                Button("Retry Cleanup") { Task { await cleanup.retryCleanup() } }
                    .buttonStyle(.bordered)
            }
            if let action {
                Button(action, systemImage: action == "Connect" ? "terminal" : "arrow.clockwise", action: perform)
                    .buttonStyle(.borderedProminent)
                    .disabled(!enabled || (uncertain && cleanup?.recoveryReviewReady != true))
            }
        }.padding(24).foregroundStyle(.white)
            .background(.black.opacity(0.8), in: RoundedRectangle(cornerRadius: 16)).padding()
    }
}

/// Hosts the navigation rail above a canvas showing the selected machine's
/// asynchronously created terminal. Other machines' views leave the window,
/// which pauses their rendering; their sessions keep running.
private struct WorkspaceCanvasHost: UIViewRepresentable {
    let scene: WorkspaceTerminalScene
    let terminal: NativeTerminalView?
    func makeUIView(context: Context) -> WorkspaceCanvasView { WorkspaceCanvasView(scene: scene) }
    func updateUIView(_ view: WorkspaceCanvasView, context: Context) { view.show(terminal) }
}

private final class WorkspaceCanvasView: UIView {
    private let canvas = UIView()
    private var controls: NativeTerminalNavigationControls?
    private weak var scene: WorkspaceTerminalScene?

    init(scene: WorkspaceTerminalScene) {
        self.scene = scene
        super.init(frame: .zero)
        canvas.translatesAutoresizingMaskIntoConstraints = false
        addSubview(canvas)
        var top = safeAreaLayoutGuide.topAnchor
        do {
            let controls = try NativeTerminalNavigationControls(coordinator: scene.navigation, terminalCanvas: canvas,
                permitsCanvasNavigation: { [weak scene] in scene?.selectedSession?.clipboardIsPresenting != true })
            controls.onFailure = { [weak scene] error in scene?.report(error) }
            controls.translatesAutoresizingMaskIntoConstraints = false
            addSubview(controls)
            NSLayoutConstraint.activate([
                controls.topAnchor.constraint(equalTo: safeAreaLayoutGuide.topAnchor),
                controls.leadingAnchor.constraint(equalTo: leadingAnchor),
                controls.trailingAnchor.constraint(equalTo: trailingAnchor)
            ])
            top = controls.bottomAnchor
            self.controls = controls
        } catch { scene.report(error) }
        NSLayoutConstraint.activate([
            canvas.topAnchor.constraint(equalTo: top), canvas.bottomAnchor.constraint(equalTo: bottomAnchor),
            canvas.leadingAnchor.constraint(equalTo: leadingAnchor), canvas.trailingAnchor.constraint(equalTo: trailingAnchor)
        ])
        // A tap brings the keyboard back after its input generation ended.
        let tap = UITapGestureRecognizer(target: self, action: #selector(tapped))
        tap.cancelsTouchesInView = false
        canvas.addGestureRecognizer(tap)
    }
    @available(*, unavailable)
    required init?(coder: NSCoder) { nil }

    @objc private func tapped() {
        Task { [weak scene] in await scene?.activateKeyboard() }
    }

    func show(_ terminal: NativeTerminalView?) {
        if let terminal, terminal.superview === canvas, canvas.subviews.count == 1 { return }
        canvas.subviews.forEach { $0.removeFromSuperview() }
        guard let terminal else { return }
        terminal.frame = canvas.bounds
        terminal.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        canvas.addSubview(terminal)
    }
}
