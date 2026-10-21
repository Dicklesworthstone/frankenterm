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

    /// `signer` defaults to the protected Keychain signer; tests inject one.
    init(store: WorkspaceStore, journalDirectory: URL,
         signer: any WorkspaceSSHSigning = AppleSSHCredentialSigner()) throws {
        let broker = SSHAuthenticationBroker(store: store, signer: signer,
                                             support: WorkspaceNativeSSHAuthentication.routeSupport)
        coordinator = try TerminalInputCoordinator()
        journal = try TerminalSessionJournal(directory: journalDirectory)
        manager = WorkspaceSSHConnectionManager(store: store, broker: broker, journal: journal, coordinator: coordinator)
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

/// The exact route a user chose to open. Credential choice follows the route's
/// ordered references that are available on this device.
struct TerminalSessionTarget: Hashable {
    let workspaceID: UUID, revision: UUID
    let machineID: String, machineName: String, routeID: String, credentialID: String
    /// The machine theme as the terminal's base palette, and its background.
    let basePalette: [UInt8]?, background: String?

    init(workspaceID: UUID, revision: UUID, machineID: String, machineName: String, routeID: String,
         credentialID: String, theme: WorkspaceDocument.Theme? = nil) {
        self.workspaceID = workspaceID; self.revision = revision; self.machineID = machineID
        self.machineName = machineName; self.routeID = routeID; self.credentialID = credentialID
        basePalette = theme?.terminalBasePalette; background = theme?.background
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
    weak var view: NativeTerminalView?
    var onUnavailable: ((TerminalRenderError) -> Void)?
    var onInterruptedInput: (() -> Void)?
    private var reportedInterruptedInput = false
    init(source: TerminalSource) { self.source = source }

    func present(_ presentation: TerminalModelPresentation, source: TerminalSource) async throws {
        guard source == self.source else { throw TerminalInputError.stale }
        switch presentation {
        case let .frame(bytes): try view?.submit(bytes, source: source)
        case let .unavailable(reason): onUnavailable?(reason)
        }
    }
    /// Metadata-only, idempotent acceptance of normal AND final outcome batches.
    /// No input bytes or unbounded receipt history survive here. The durable
    /// journal's conservative uncertainty is not resolved by local acceptance.
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) async throws {
        guard source == self.source, outcomes.count <= 64,
              outcomes.allSatisfy({ $0.receipt.source == source }) else { throw TerminalInputError.stale }
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
        case ended(String)
        case failed(String)
    }
    let id = UUID()
    private(set) var phase: Phase = .idle
    private(set) var terminal: NativeTerminalView?
    /// True only after every owned cleanup operation actually succeeds.
    private(set) var settled = false
    private(set) var cleanupFailure: String?
    private(set) var interruptedInput = false
    let target: TerminalSessionTarget

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
    @ObservationIgnored private var resize: TerminalViewportResize<TerminalSource>?
    @ObservationIgnored private var lastViewportRevision: UInt64 = 0
    @ObservationIgnored private var stopped = false

    init(target: TerminalSessionTarget, connections: MobileConnections, focus: any TerminalSessionFocus) {
        self.target = target; self.connections = connections; self.focus = focus
    }

    /// Installs only the source-matching keyboard published by navigation.
    func install(_ keyboard: TerminalKeyboardSession) {
        guard !stopped, phase == .live, let terminal,
              terminal.input.session !== keyboard else { return }
        guard keyboard.originalFocus.source == pump?.source else {
            fail("The selected keyboard belongs to another terminal."); return
        }
        do {
            try terminal.replaceInputSession(keyboard)
            self.keyboard = keyboard
            terminal.requestKeyboard(true)
        } catch { keyboard.suspend(); fail(Self.describe(error)) }
    }

    /// The retained startup task owns partially constructed resources. Teardown
    /// cancels and joins it; a late connection/view cannot outlive a settled UI.
    func start(acknowledgingEarlierSessions: Bool = false) async {
        guard phase == .idle, !stopped else { return }
        phase = .connecting
        let job = Task { await self.performStart(acknowledgingEarlierSessions: acknowledgingEarlierSessions) }
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
    private func performStart(acknowledgingEarlierSessions: Bool) async {
        do {
            try checkStarting()
            if acknowledgingEarlierSessions {
                let journal = connections.journal
                for record in try await journal.records() where record.requiresReview && record.canResolve
                    && record.scope.workspaceID == target.workspaceID && record.scope.machineID == target.machineID {
                    try checkStarting()
                    try await journal.resolve(record)
                }
            }
            try checkStarting()
            let connection = try await connections.manager.connect(workspaceID: target.workspaceID,
                expectedRevision: target.revision, machineID: target.machineID, routeID: target.routeID,
                credentialID: target.credentialID, confirmedByUser: true)
            self.connection = connection
            try checkStarting()
            let geometry = try TerminalSessionGeometry(columns: 80, rows: 24)
            let pump = try await connection.openTerminal(geometry: geometry)
            self.pump = pump
            try checkStarting()
            let presenter = TerminalFramePresenter(source: pump.source)
            presenter.onUnavailable = { [weak self] reason in
                self?.fail("The display is unavailable (\(Self.describe(reason))).")
            }
            presenter.onInterruptedInput = { [weak self] in self?.interruptedInput = true }
            self.presenter = presenter
            let model = try NativeTerminalModel(source: pump.source, geometry: geometry)
            unadoptedModel = model
            if let palette = target.basePalette { try model.setBasePalette(palette) }
            let coordinator = connections.coordinator
            let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: presenter)
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
            let source = pump.source
            let resize = TerminalViewportResize(source: source, initial: geometry) { geometry in
                do {
                    try await connection.requestResize(geometry, source: source)
                    return .queued
                } catch NativeSSHError.busy { return .busy }
            }
            self.resize = resize
            resize.onFailure = { [weak self] error in self?.fail(Self.describe(error)) }
            view.onViewportChange = { [weak self] viewport in self?.propose(viewport) }
            view.onFailure = { [weak self] error in self?.fail(Self.describe(error)) }
            view.onInputRetired = { [weak self] in
                guard let self else { return }
                self.focus?.inputRetired(machineID: self.target.machineID)
            }
            phase = .live
            view.setSceneActive(true)
            view.requestKeyboard(true)
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
        } catch {
            if !stopped { phase = .failed(Self.describe(error)) }
        }
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

    /// Output EOF is not proof that the remote process exited, so no reason
    /// claims a shell's fate.
    static func describe(_ end: TerminalSessionEnd) -> String {
        switch end {
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

    private func sealLocalOwnership() {
        terminal?.retire()
        keyboard?.suspend()
        resize?.retire()
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
        // Interrupt an already-published native connection promptly. Startup may
        // still be in openTerminal/view creation; it never joins this cleanup.
        if let connection { try? await connections.manager.close(connectionID: connection.id) }
        if let startup { await startup.value }
        startup = nil
        sealLocalOwnership()
        var failure: Error?
        if let connection {
            do { try await connections.manager.close(connectionID: connection.id); self.connection = nil }
            catch { failure = error }
        }
        if let pump {
            do { try await pump.disconnect() } catch { failure = failure ?? error }
        }
        if let run { _ = await run.result }
        run = nil; runObserver = nil
        if let resize { await resize.waitUntilSettled(); self.resize = nil }
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
                try await coordinator.retire(pump.source)
                self.pump = nil
            } catch { failure = failure ?? error }
        }
        if let runtime, !attached {
            do { try await runtime.close(); self.runtime = nil }
            catch { failure = failure ?? error }
        }
        if let model = unadoptedModel {
            do { try model.close(); unadoptedModel = nil }
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
    }
}

