import SwiftUI
import UIKit
import Observation
import FrankenTermKit
import FrankenTermSSH
import FrankenTermSSHIntegration
import FrankenTermTerminal

/// App-wide live-connection services over the app's single WorkspaceStore: one
/// input coordinator, one durable shell-creation journal and one connection
/// manager (which owns the authentication broker's foreground state).
@MainActor
final class MobileConnections {
    let coordinator: TerminalInputCoordinator
    let manager: WorkspaceSSHConnectionManager
    /// Durable record of sessions whose input may have unknown remote effects.
    let journal: TerminalSessionJournal
    // Failed cleanup retains its existing bounded connection/model owner even
    // after SwiftUI removes the screen. Never manufacture a replacement owner
    // or forget an unresolved protected-storage write to make room for a shell.
    private var pendingCleanup: [UUID: TerminalSessionController] = [:]
    private var activityGeneration = UUID()

    /// `signer` defaults to the protected Keychain signer; tests inject one,
    /// and may shorten how long an unanswering server takes to be reported.
    init(store: WorkspaceStore, journalDirectory: URL,
         journalIdentifier: String = "primary",
         signer: any WorkspaceSSHSigning = AppleSSHCredentialSigner(),
         liveness: WorkspaceSSHLiveness = .standard) throws {
        let broker = SSHAuthenticationBroker(store: store, signer: signer,
                                             support: WorkspaceNativeSSHAuthentication.routeSupport)
        coordinator = try TerminalInputCoordinator()
        journal = try TerminalSessionJournal(directory: journalDirectory, identifier: journalIdentifier)
        manager = WorkspaceSSHConnectionManager(store: store, broker: broker, journal: journal,
                                                coordinator: coordinator, liveness: liveness)
    }

    /// Foreground admits explicit connections; background seals every source and
    /// settles native teardown (the remote shell may keep running).
    func setActive(_ active: Bool) async {
        let generation = UUID(); activityGeneration = generation
        if active {
            let retained = Array(pendingCleanup.values)
            for controller in retained { await controller.retryCleanup() }
            guard activityGeneration == generation else { return }
            await manager.resume()
        } else { await manager.suspend() }
    }

    fileprivate func retainForCleanup(_ controller: TerminalSessionController) {
        pendingCleanup[controller.id] = controller
    }
    fileprivate func releaseCleanup(_ id: UUID) { pendingCleanup[id] = nil }
}

/// One ready direct-SSH route to a machine, with the key it uses: the first of
/// the route's ordered references available on this device.
struct TerminalSessionRoute: Hashable {
    let id: String, credentialID: String, host: String, port: UInt16
}

/// A machine to open and its ready routes, the chosen or preferred one first.
struct TerminalSessionTarget: Hashable {
    let workspaceID: UUID, revision: UUID
    let machineID: String, machineName: String
    /// Never empty.
    let routes: [TerminalSessionRoute]
    /// The machine theme as the terminal's base palette, and its background.
    let basePalette: [UInt8]?, background: String?

    init(workspaceID: UUID, revision: UUID, machineID: String, machineName: String,
         routes: [TerminalSessionRoute], theme: WorkspaceDocument.Theme? = nil) {
        precondition(!routes.isEmpty, "a session target needs a route")
        self.workspaceID = workspaceID; self.revision = revision; self.machineID = machineID
        self.machineName = machineName; self.routes = routes
        basePalette = theme?.terminalBasePalette; background = theme?.background
    }

    /// The same routes under another theme.
    func wearing(_ theme: WorkspaceDocument.Theme) -> TerminalSessionTarget {
        .init(workspaceID: workspaceID, revision: revision, machineID: machineID, machineName: machineName,
              routes: routes, theme: theme)
    }

    /// The order to try the routes in: those this device can reach now first
    /// (see WorkspaceRouteNetwork.isLikelyReachable), each group in the
    /// machine's own order.
    func attemptOrder() -> [TerminalSessionRoute] {
        let reachable = routes.map { WorkspaceRouteNetwork.isLikelyReachable($0.host) }
        return routes.indices.filter { reachable[$0] }.map { routes[$0] }
            + routes.indices.filter { !reachable[$0] }.map { routes[$0] }
    }

    /// Whether light chrome reads better over this background: its relative
    /// luminance is below 0.179, where black and white text contrast equally.
    /// No theme means the black default.
    static func isDark(_ hex: String?) -> Bool {
        guard let hex, let value = UInt32(hex.dropFirst().prefix(6), radix: 16) else { return true }
        func linear(_ shift: UInt32) -> Double {
            let component = Double(value >> shift & 0xFF) / 255
            return component <= 0.04045 ? component / 12.92 : pow((component + 0.055) / 1.055, 2.4)
        }
        return 0.2126 * linear(16) + 0.7152 * linear(8) + 0.0722 * linear(0) < 0.179
    }
}

/// Publishes each owned FTSF frame from the Rust model to the native view.
@MainActor
private final class TerminalFramePresenter: TerminalModelDelivery {
    let source: TerminalSource
    private let connection: WorkspaceSSHConnection
    weak var view: NativeTerminalView?
    weak var clipboard: NativeTerminalClipboardController?
    // The scene fences delivery synchronously, before the model actor applies
    // its hide. An already-captured obsolete frame cannot restart hidden work.
    var acceptsFrames = false
    var onUnavailable: ((TerminalRenderError) -> Void)?
    var onInterruptedInput: (() -> Void)?
    private var reportedInterruptedInput = false
    init(source: TerminalSource, connection: WorkspaceSSHConnection) {
        self.source = source; self.connection = connection
    }

    func present(_ presentation: TerminalModelPresentation, source: TerminalSource) async throws {
        guard source == self.source else { throw TerminalInputError.stale }
        guard acceptsFrames else { return }
        switch presentation {
        case let .frame(bytes):
            try view?.submit(bytes, source: source)
            try clipboard?.receiveFrame(bytes, source: source)
        case let .unavailable(reason): onUnavailable?(reason)
        }
    }
    /// Metadata-only, idempotent acceptance of normal AND final outcome batches.
    /// No input bytes or unbounded receipt history survive here. The durable
    /// journal's conservative uncertainty is not resolved by local acceptance.
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) async throws {
        guard source == self.source, outcomes.count <= 64,
              outcomes.allSatisfy({ $0.receipt.source == source }) else { throw TerminalInputError.stale }
        // The exact journal epoch outlives native close. Publish content-free
        // evidence before the pump can acknowledge this batch, including final
        // cleanup outcomes delivered after the view has retired.
        try await connection.recordInput(outcomes, source: source)
        if !reportedInterruptedInput, outcomes.contains(where: {
            switch $0.disposition {
            case .connectionLost, .definitelyNotWritten, .cancelledAfterPrefix: true
            case .transportAccepted, .cancelledBeforeWrite: false
            }
        }) {
            reportedInterruptedInput = true
            onInterruptedInput?()
        }
    }
}

/// The workspace scene owns its one keyboard and all navigation bindings.
/// Detachment must succeed before the source and its canonical model retire.
@MainActor
protocol TerminalSessionFocus: AnyObject {
    func attach(machineID: String, source: TerminalSource,
                encoder: any TerminalKeyboardEncoding) async throws -> TerminalKeyboardSession
    func detach(machineID: String, source: TerminalSource) async throws
    func inputRetired(machineID: String)
}

/// Owns one direct-SSH connection, PTY, model and native display. Keyboard
/// generations come from the workspace scene's existing navigation owner.
@MainActor
@Observable
final class TerminalSessionController {
    enum Phase: Equatable {
        case idle, connecting, live
        /// An earlier session on this machine left input with an unknown
        /// outcome; a new shell waits for the user's acknowledgement (plan 11.8).
        case needsReview
        /// No route reached a signed-in connection, so no shell started and
        /// nothing was sent.
        case couldNotConnect(WorkspaceConnectionHelp)
        case ended(String)
        case failed(String)
    }
    let id = UUID()
    private(set) var phase: Phase = .idle
    /// The route being tried, then the one connected.
    private(set) var route: TerminalSessionRoute
    private(set) var terminal: NativeTerminalView?
    /// True only after every owned cleanup operation actually succeeds.
    private(set) var settled = false
    private(set) var cleanupFailure: String?
    private(set) var interruptedInput = false
    private(set) var interactionProblem: String?
    private(set) var recoveryRecords: [TerminalSessionRecord] = []
    private(set) var recoveryFailure: String?
    private(set) var recoveryLoading = false
    private(set) var recoveryLoaded = false
    private(set) var historyInspection: TerminalHistoryInspection?
    private(set) var historyReturnBookmark: TerminalHistoryBookmark?
    private(set) var historyReturning = false
    private(set) var retainedOutput: TerminalRetainedOutput?
    private(set) var retainedOutputFailure: String?
    let target: TerminalSessionTarget

    var recoveryReviewReady: Bool {
        settled && recoveryLoaded && !recoveryLoading && recoveryRecords.allSatisfy(\.canResolve)
    }

    /// These exact immutable snapshots are shown before a new-session action.
    /// A late outcome or another owner changing history invalidates resolution;
    /// a click never acknowledges newly discovered, unseen records.
    func refreshRecoveryReview() async {
        guard !recoveryLoading else { return }
        recoveryLoading = true; recoveryLoaded = false; recoveryFailure = nil
        defer { recoveryLoading = false }
        do {
            recoveryRecords = try await connections.journal.records().filter {
                $0.requiresReview && $0.scope.workspaceID == target.workspaceID
                    && $0.scope.machineID == target.machineID
            }
            recoveryLoaded = true
        } catch {
            recoveryRecords = []
            recoveryFailure = Self.describe(error)
        }
    }

    @ObservationIgnored private let connections: MobileConnections
    @ObservationIgnored private var connection: WorkspaceSSHConnection?
    @ObservationIgnored private var pump: TerminalSessionPump?
    @ObservationIgnored private var runtime: TerminalModelRuntime?
    @ObservationIgnored private var unadoptedModel: NativeTerminalModel?
    @ObservationIgnored private var keyboard: TerminalKeyboardSession?
    @ObservationIgnored private weak var focus: (any TerminalSessionFocus)?
    @ObservationIgnored private var attached = false
    @ObservationIgnored private var startup: Task<Void, Never>?
    @ObservationIgnored private var run: Task<TerminalSessionEnd, Error>?
    @ObservationIgnored private var runObserver: Task<Void, Never>?
    @ObservationIgnored private var cleanup: (id: UUID, task: Task<Void, Never>)?
    @ObservationIgnored private var presenter: TerminalFramePresenter?
    @ObservationIgnored private var clipboard: NativeTerminalClipboardController?
    @ObservationIgnored private var presentation: TerminalPresentationVisibility<TerminalSource>?
    @ObservationIgnored private var presentationVisible = true
    @ObservationIgnored private var resize: TerminalViewportResize<TerminalSource>?
    @ObservationIgnored private var history: TerminalHistorySelection<TerminalSource>?
    @ObservationIgnored private var lastViewportRevision: UInt64 = 0
    @ObservationIgnored private var stopped = false
    /// The palette the terminal should show, and the one its model has.
    @ObservationIgnored private var basePalette: [UInt8]?
    @ObservationIgnored private var appliedPalette: [UInt8]?
    @ObservationIgnored private var paletteChange: Task<Void, Never>?
    @ObservationIgnored private var historyReturn: Task<Void, Never>?
    @ObservationIgnored private var retainedCaptureAttempted = false
    @ObservationIgnored private var retainEndedOutput = true

    init(target: TerminalSessionTarget, connections: MobileConnections, focus: any TerminalSessionFocus) {
        self.target = target; self.connections = connections; self.focus = focus
        basePalette = target.basePalette
        route = target.routes[0]
    }

    /// "Machine · Route", for labels that leave the session.
    private var destination: String { "\(target.machineName) · Machine \(target.machineID) · Route \(route.id)" }

    /// Re-themes the terminal. Before the model exists, startup applies the
    /// latest palette; after, one change at a time reaches the runtime and
    /// the last pick wins.
    func applyPalette(_ palette: [UInt8]) {
        basePalette = palette
        guard paletteChange == nil else { return }
        paletteChange = Task { [weak self] in
            while let self, !self.stopped, let runtime = self.runtime, let source = self.pump?.source,
                  let wanted = self.basePalette, wanted != self.appliedPalette {
                do { try await runtime.setBasePalette(wanted, source: source); self.appliedPalette = wanted }
                catch { break }
            }
            self?.paletteChange = nil
        }
    }

    var clipboardIsPresenting: Bool {
        clipboard?.isPresenting == true || historyInspection != nil || historyReturning
    }
    func inspectOutput() {
        guard !stopped, phase == .live, presentationVisible, !clipboardIsPresenting else { return }
        interactionProblem = nil; clipboard?.requestCopy()
    }
    func paste() {
        guard !stopped, phase == .live, presentationVisible, !clipboardIsPresenting else { return }
        interactionProblem = nil; clipboard?.requestPaste()
    }
    func dismissInteractionProblem() { interactionProblem = nil }

    /// The owner is captured by the sheet, never looked up again through a
    /// possibly different selected machine when that sheet is dismissed.
    func inspectHistory() -> TerminalHistoryInspection? {
        guard !stopped, phase == .live, presentationVisible, !clipboardIsPresenting,
              let runtime, let source = pump?.source else { return nil }
        interactionProblem = nil
        terminal?.input.retire()
        let history = history
        let inspection = TerminalHistoryInspection(runtime: runtime, source: source,
            destination: destination,
            prepare: { await history?.waitUntilSettled() }) { [weak self] id, bookmark, failure in
                guard let self, self.historyInspection?.id == id else { return }
                self.historyInspection = nil
                if !self.stopped, self.presentationVisible {
                    self.historyReturnBookmark = bookmark
                    if let failure { self.interactionProblem = "The earlier viewing position could not be restored: \(failure)" }
                }
            }
        historyInspection = inspection
        return inspection
    }

    func returnFromHistorySearch() {
        guard !stopped, phase == .live, presentationVisible, !clipboardIsPresenting,
              historyReturn == nil, let bookmark = historyReturnBookmark, let runtime,
              bookmark.source == pump?.source else { return }
        historyReturnBookmark = nil; historyReturning = true
        terminal?.input.retire()
        let history = history
        historyReturn = Task { [weak self] in
            guard let self else { return }
            await history?.waitUntilSettled()
            do { try await runtime.restoreHistoryBookmark(bookmark, source: bookmark.source) }
            catch {
                if !self.stopped, self.presentationVisible {
                    self.interactionProblem = "The earlier viewing position could not be restored: \(Self.describe(error))"
                }
            }
            self.historyReturning = false; self.historyReturn = nil
        }
    }

    /// Closing a workspace or replacing its ended controller releases plaintext
    /// and prevents an in-flight teardown from capturing it again afterwards.
    func discardRetainedOutput() {
        retainEndedOutput = false; retainedOutput = nil; retainedOutputFailure = nil
    }

    private func inputFailed(_ error: Error) {
        // Rejected local text is not a transport failure. In particular an
        // oversized/unsafe clipboard must not tear down a working SSH session.
        switch error as? TerminalInputError {
        case .capacity:
            interactionProblem = "Input was not accepted because a local text or buffer limit was reached. No rejected input will be sent automatically."
        case .unsafePaste:
            interactionProblem = "The clipboard contains terminal control characters and was not pasted."
        case .emptyInput:
            interactionProblem = "There is no text to paste."
        default: fail(Self.describe(error))
        }
    }

    /// Keep consuming this source's bytes/replies while it is offscreen, but
    /// stop allocating snapshots and preparing GPU frames. The scene owns this
    /// policy; changing presentation never changes the keyboard destination.
    func setPresentationVisible(_ visible: Bool) {
        let visible = visible && !stopped
        presentationVisible = visible
        clipboard?.setActive(visible)
        if !visible {
            interactionProblem = nil
            historyInspection?.retire()
            historyReturn?.cancel(); historyReturnBookmark = nil
        }
        presenter?.acceptsFrames = visible
        terminal?.renderer.setSceneActive(visible)
        if let presentation, !presentation.isRetired {
            do { try presentation.request(visible, source: presentation.source) }
            catch { fail(Self.describe(error)) }
        }
    }

    /// Installs only the source-matching keyboard published by navigation.
    func install(_ keyboard: TerminalKeyboardSession) {
        guard !stopped, phase == .live, let terminal,
              terminal.input.session !== keyboard else { return }
        guard keyboard.originalFocus.source == pump?.source else {
            fail("The selected keyboard belongs to another terminal."); return
        }
        // An activation may have started before a read-only sheet opened and
        // completed its handoff afterwards. Seal that newly published responder
        // before it can acquire hardware input behind the inspection surface.
        guard !clipboardIsPresenting else {
            keyboard.suspend()
            focus?.inputRetired(machineID: target.machineID)
            return
        }
        do {
            try terminal.replaceInputSession(keyboard)
            self.keyboard = keyboard
            terminal.requestKeyboard(true)
        } catch { keyboard.suspend(); fail(Self.describe(error)) }
    }

    /// The retained startup task owns partially constructed resources. Teardown
    /// cancels and joins it; a late connection/view cannot outlive a settled UI.
    func start(reviewingEarlierSessions: [TerminalSessionRecord] = []) async {
        guard phase == .idle, !stopped else { return }
        phase = .connecting
        let job = Task { await self.performStart(reviewingEarlierSessions: reviewingEarlierSessions) }
        startup = job
        await withTaskCancellationHandler { await job.value } onCancel: { job.cancel() }
        if Task.isCancelled { requestStop() }
        if stopped || phase != .live { await teardown() }
    }

    private func checkStarting() throws {
        try Task.checkCancellation()
        guard !stopped else { throw CancellationError() }
    }

    /// Never calls teardown: teardown must be able to join this task without a
    /// self-join. Retain each resource BEFORE the next cancellation check/await.
    private func performStart(reviewingEarlierSessions: [TerminalSessionRecord]) async {
        do {
            try checkStarting()
            for record in reviewingEarlierSessions {
                guard record.requiresReview, record.canResolve,
                      record.scope.workspaceID == target.workspaceID,
                      record.scope.machineID == target.machineID else { throw TerminalSessionJournalError.staleReview }
                try checkStarting()
                try await connections.journal.resolve(record)
            }
            try checkStarting()
            let connection = try await connectFirstReachable()
            self.connection = connection
            try checkStarting()
            let geometry = try TerminalSessionGeometry(columns: 80, rows: 24)
            let pump = try await connection.openTerminal(geometry: geometry)
            self.pump = pump
            try checkStarting()
            let presenter = TerminalFramePresenter(source: pump.source, connection: connection)
            presenter.onUnavailable = { [weak self] reason in
                self?.fail("The display is unavailable (\(Self.describe(reason))).")
            }
            presenter.onInterruptedInput = { [weak self] in self?.interruptedInput = true }
            self.presenter = presenter
            let model = try NativeTerminalModel(source: pump.source, geometry: geometry)
            unadoptedModel = model
            if let palette = basePalette { try model.setBasePalette(palette); appliedPalette = palette }
            let coordinator = connections.coordinator
            let runtime = TerminalModelRuntime(model: model, coordinator: coordinator,
                                               delivery: presenter, initiallyVisible: false)
            self.runtime = runtime; unadoptedModel = nil
            guard let focus else { throw TerminalKeyboardError.unavailable }
            attached = true
            let keyboard = try await focus.attach(machineID: target.machineID, source: pump.source, encoder: runtime)
            self.keyboard = keyboard
            try checkStarting()
            let view = try await NativeTerminalView(session: keyboard, configuration: Self.renderConfiguration)
            terminal = view
            try checkStarting()
            presenter.view = view
            let clipboard = try NativeTerminalClipboardController(terminal: view,
                destinationLabel: "\(target.machineName)\nMachine \(target.machineID) · Route \(route.id)")
            self.clipboard = clipboard; presenter.clipboard = clipboard
            clipboard.onFailure = { [weak self] error in self?.interactionProblem = Self.describe(error) }
            let source = pump.source
            let resize = TerminalViewportResize(source: source, initial: geometry) { geometry in
                do {
                    try await connection.requestResize(geometry, source: source)
                    return .queued
                } catch NativeSSHError.busy { return .busy }
            }
            self.resize = resize
            resize.onFailure = { [weak self] error in self?.fail(Self.describe(error)) }
            // The model keeps the selected history row while output arrives and
            // while this machine is hidden, so returning finds the same place.
            let history = TerminalHistorySelection(source: source) { top, source in
                try await runtime.setViewport(top: top, source: source)
            }
            self.history = history
            history.onFailure = { [weak self] error in self?.fail(Self.describe(error)) }
            view.onHistoryViewportRequest = { [weak self] top in self?.selectHistory(top) }
            view.onViewportChange = { [weak self] viewport in self?.propose(viewport) }
            view.onFailure = { [weak self] error in self?.inputFailed(error) }
            view.onInputRetired = { [weak self] in
                guard let self else { return }
                self.clipboard?.inputRetired()
                self.focus?.inputRetired(machineID: self.target.machineID)
            }
            phase = .live
            let presentation = TerminalPresentationVisibility(source: source, initiallyVisible: false) { visible, source in
                try await runtime.setVisible(visible, source: source)
            }
            self.presentation = presentation
            presentation.onFailure = { [weak self] error in self?.fail(Self.describe(error)) }
            setPresentationVisible(presentationVisible)
            view.requestKeyboard(presentationVisible)
            let run = Task {
                try await pump.run(consumer: runtime) { geometry, appliedSource in
                    // Retirement can race the MainActor hop after the pump's
                    // stop check. Ignore only this retired owner, never a source
                    // mismatch or an invalid model-completion notification.
                    try await MainActor.run {
                        guard !resize.isRetired else { return }
                        try resize.didApply(geometry, source: appliedSource)
                    }
                }
            }
            self.run = run
            // The observer is NOT the task joined by teardown. A pump ending
            // naturally can therefore request cleanup without awaiting itself.
            runObserver = Task { [weak self] in
                let result = await run.result
                await self?.pumpEnded(result)
            }
        } catch TerminalSessionJournalError.reviewRequired {
            if !stopped { phase = .needsReview }
        } catch let failure as ConnectFailure {
            if !stopped { phase = .couldNotConnect(failure.help) }
        } catch {
            if !stopped { phase = .failed(Self.describe(error)) }
        }
    }

    private struct ConnectFailure: Error { let help: WorkspaceConnectionHelp }

    /// Tries the machine's routes in attempt order and moves on only while an
    /// address never answers. A refused key or a changed host key ends the
    /// attempt with its own explanation; no other route is tried after it.
    private func connectFirstReachable() async throws -> WorkspaceSSHConnection {
        let onTailnet = WorkspaceRouteNetwork.deviceTailnetAddress() != nil
        let device = "this \(UIDevice.current.model)"
        var unreachable: [WorkspaceConnectionHelp] = []
        for candidate in target.attemptOrder() {
            try checkStarting()
            route = candidate
            do {
                return try await connections.manager.connect(workspaceID: target.workspaceID,
                    expectedRevision: target.revision, machineID: target.machineID, routeID: candidate.id,
                    credentialID: candidate.credentialID, confirmedByUser: true)
            } catch let error as NativeSSHError where error.isUnreachable {
                if let help = WorkspaceConnectionHelp(failure: error, machine: target.machineName, host: candidate.host,
                                                      port: candidate.port, onTailnet: onTailnet, device: device) {
                    unreachable.append(help)
                }
            } catch is CancellationError {
                throw CancellationError()
            } catch TerminalSessionJournalError.reviewRequired {
                throw TerminalSessionJournalError.reviewRequired
            } catch {
                throw ConnectFailure(help: .init(title: "Couldn't connect to \(target.machineName)",
                                                 message: Self.describe(error), remedy: .none))
            }
        }
        throw ConnectFailure(help: WorkspaceConnectionHelp.best(unreachable)
            ?? .init(title: "Couldn't connect to \(target.machineName)", message: "No route answered.", remedy: .none))
    }

    /// The desktop's default terminal font, bundled with its license, when it
    /// is registered; otherwise Kit's Menlo default, because Core Text would
    /// replace an unknown name with a proportional system font.
    static var renderConfiguration: NativeTerminalRenderConfiguration {
        var configuration = NativeTerminalRenderConfiguration()
        if UIFont(name: "PragmasevkaNF", size: 12) != nil { configuration.fontName = "PragmasevkaNF" }
        return configuration
    }

    /// User-facing text: an error's own description when it has one.
    static func describe(_ error: any Error) -> String {
        (error as? LocalizedError)?.errorDescription ?? String(describing: error)
    }

    /// Only a reported nonzero exit code or signal describes remote termination.
    /// libssh2 cannot distinguish a reported zero from an absent exit status.
    static func describe(_ end: TerminalSessionEnd) -> String {
        switch end {
        case let .completed(completion):
            switch completion.remoteExit {
            case let .exitStatus(status): "The remote program reported exit status \(status)."
            case let .signal(signal): "The remote program reported termination by signal \(signal)."
            case nil:
                completion.channelClosed
                    ? "The machine closed this session's channel; a usable remote exit status was not reported."
                    : "The machine ended its output; channel closure and remote process status remain unconfirmed."
            }
        case .peerOutputEnded: "The machine closed this session's output; remote process status is unknown."
        case .cancelled: "This device closed the session, for example when FrankenTerm left the foreground."
        case .disconnected: "The connection to the machine was lost."
        case .transportFailed: "The connection to the machine failed."
        case .consumerFailed: "The terminal could not process the session's output."
        case .ownershipConflict: "Another owner took over this session's input."
        }
    }

    /// One latest-value owner coalesces layouts until the pump reports actual
    /// canonical application. There is no timer, optimistic model resize, or
    /// generation-less retry that can target a replacement terminal.
    private func propose(_ viewport: NativeTerminalViewport) {
        guard !stopped, phase == .live, let resize, viewport.source == resize.source,
              viewport.revision > lastViewportRevision,
              let columns = UInt32(exactly: viewport.columns), let rows = UInt32(exactly: viewport.rows),
              viewport.pixelWidth >= 0, viewport.pixelHeight >= 0,
              let geometry = try? TerminalSessionGeometry(columns: columns, rows: rows,
                  pixelWidth: UInt32(min(viewport.pixelWidth, 65_535)),
                  pixelHeight: UInt32(min(viewport.pixelHeight, 65_535))) else { return }
        do {
            try resize.propose(geometry, source: viewport.source)
            lastViewportRevision = viewport.revision
        } catch { fail(Self.describe(error)) }
    }

    private func selectHistory(_ top: Int64?) {
        guard !stopped, phase == .live, !clipboardIsPresenting, let history else { return }
        do { try history.request(top: top, source: history.source) }
        catch { fail(Self.describe(error)) }
    }

    private func sealLocalOwnership() {
        presentationVisible = false
        clipboard?.retire()
        historyInspection?.retire()
        historyReturn?.cancel(); historyReturnBookmark = nil
        paletteChange?.cancel()
        interactionProblem = nil
        presenter?.acceptsFrames = false
        presentation?.retire()
        terminal?.retire()
        keyboard?.suspend()
        resize?.retire()
        history?.retire()
        pump?.requestStop()
    }

    /// Synchronous disappearance/close barrier, before SwiftUI schedules cleanup.
    func requestStop() {
        if !stopped, phase == .connecting || phase == .live {
            phase = .ended(Self.describe(TerminalSessionEnd.cancelled))
        }
        stopped = true
        startup?.cancel()
        sealLocalOwnership()
    }

    /// A display that cannot show this session's output must not keep sending
    /// it input. The pump's observer joins cleanup outside its own run task.
    private func fail(_ message: String) {
        guard !stopped, phase == .live else { return }
        phase = .failed(message)
        sealLocalOwnership()
    }

    private func pumpEnded(_ end: Result<TerminalSessionEnd, Error>) async {
        guard !stopped else { return }
        if phase == .live {
            switch end {
            case let .success(reason): phase = .ended(Self.describe(reason))
            case let .failure(error): phase = .failed(Self.describe(error))
            }
        }
        await teardown()
    }

    func stop() async { requestStop(); await teardown() }
    func retryCleanup() async { guard !settled else { return }; await teardown() }

    /// All callers join one cleanup job. Failure retains the actual owners, and
    /// a later explicit retry resumes cleanup, not connection or shell creation.
    private func teardown() async {
        guard !settled else { return }
        requestStop()
        let owned: (id: UUID, task: Task<Void, Never>)
        if let cleanup { owned = cleanup }
        else {
            let id = UUID()
            let task = Task { await self.performTeardown() }
            owned = (id, task); cleanup = owned
        }
        await owned.task.value
        if cleanup?.id == owned.id { cleanup = nil }
    }

    private func performTeardown() async {
        cleanupFailure = nil
        recoveryLoaded = false
        // Interrupt an already-published native connection promptly. Startup may
        // still be in openTerminal/view creation; it never joins this cleanup.
        if let connection { try? await connections.manager.close(connectionID: connection.id) }
        if let startup { await startup.value }
        startup = nil
        sealLocalOwnership()
        var failure: Error?
        if let connection {
            // Native I/O stops now; a bound epoch deliberately refuses final
            // publication until responders and outcome delivery have joined.
            try? await connections.manager.close(connectionID: connection.id)
        }
        if let pump {
            do { try await pump.disconnect() } catch { failure = failure ?? error }
        }
        if let run { _ = await run.result }
        run = nil; runObserver = nil
        if let clipboard {
            await clipboard.waitUntilSettled()
            presenter?.clipboard = nil; self.clipboard = nil
        }
        if let presentation {
            await presentation.waitUntilSettled()
            self.presentation = nil
        }
        if let resize { await resize.waitUntilSettled(); self.resize = nil }
        if let history { await history.waitUntilSettled(); self.history = nil }
        if let inspection = historyInspection { await inspection.waitUntilSettled(); historyInspection = nil }
        if let historyReturn { await historyReturn.value; self.historyReturn = nil; historyReturning = false }
        if let paletteChange { await paletteChange.value; self.paletteChange = nil }
        if attached, let source = pump?.source {
            do {
                // A live scene removes the exact binding and joins its sealed
                // responder. If the scene is gone, the captured keyboard below
                // still owns cleanup; coordinator.retire checks for any orphan.
                try await focus?.detach(machineID: target.machineID, source: source)
                attached = false
            } catch { failure = failure ?? error }
        }
        // Keep the model available while keyboard cleanup finishes its canonical
        // transaction. Native-view retirement alone does not join that work.
        if let keyboard {
            await keyboard.waitUntilSuspended()
            self.keyboard = nil
        }
        let coordinator = connections.coordinator
        if let pump, !attached {
            let presenter = self.presenter
            do {
                try await pump.reconcileStoppedOutcomes { records, source in
                    guard let presenter else { throw TerminalInputError.unavailable }
                    try await presenter.receive(records, source: source)
                }
                guard let connection else { throw TerminalInputError.unavailable }
                try await connection.prepareInputSettlement(source: pump.source)
                try await coordinator.retire(pump.source)
                self.pump = nil
            } catch { failure = failure ?? error }
        }
        if let runtime, !attached {
            if retainEndedOutput, !retainedCaptureAttempted {
                retainedCaptureAttempted = true
                do {
                    let document = try await runtime.captureOutputDocument(source: runtime.source)
                    if retainEndedOutput {
                        retainedOutput = TerminalRetainedOutput(source: runtime.source,
                            destination: destination, document: document)
                    }
                } catch {
                    if retainEndedOutput { retainedOutputFailure = Self.describe(error) }
                }
            }
            do { try await runtime.close(); self.runtime = nil }
            catch { failure = failure ?? error }
        }
        if let model = unadoptedModel {
            do { try model.close(); unadoptedModel = nil }
            catch { failure = failure ?? error }
        }
        if let connection {
            // A cached seal survives source retirement. A protected-storage
            // error keeps this exact owner for retry, without recreating a shell
            // or retaining an otherwise settled coordinator source.
            do { try await connections.manager.close(connectionID: connection.id); self.connection = nil }
            catch { failure = failure ?? error }
        }
        if let failure {
            cleanupFailure = Self.describe(failure)
            connections.retainForCleanup(self)
        } else {
            presenter?.view = nil; presenter?.onUnavailable = nil; presenter?.onInterruptedInput = nil
            presenter = nil
            settled = true
            connections.releaseCleanup(id)
        }
        await refreshRecoveryReview()
    }
}

