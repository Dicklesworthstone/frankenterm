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
        if active { await manager.resume() } else { await manager.suspend() }
    }
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
    weak var view: NativeTerminalView?
    var onUnavailable: ((TerminalRenderError) -> Void)?
    func present(_ presentation: TerminalModelPresentation, source: TerminalSource) async throws {
        switch presentation {
        case let .frame(bytes): try view?.submit(bytes, source: source)
        case let .unavailable(reason): onUnavailable?(reason)
        }
    }
    // Input outcomes are owned by the coordinator; this M0 screen has no
    // delivery-state UI yet, and accepting a batch acknowledges it exactly once.
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) async throws {}
}

/// Owns one direct-SSH terminal: connection, PTY shell, canonical Rust model,
/// keyboard session and Metal/UIKit view, in the order the components require
/// (apps/apple/FrankenTermTerminal/README.md, "Connecting the components").
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
    private(set) var phase: Phase = .idle
    private(set) var terminal: NativeTerminalView?
    /// Teardown finished: the connection, model, focus and source are released
    /// and the session's journal record can be acknowledged.
    private(set) var settled = false
    let target: TerminalSessionTarget

    @ObservationIgnored private let connections: MobileConnections
    @ObservationIgnored private var connection: WorkspaceSSHConnection?
    @ObservationIgnored private var pump: TerminalSessionPump?
    @ObservationIgnored private var runtime: TerminalModelRuntime?
    @ObservationIgnored private var focus: TerminalFocus?
    @ObservationIgnored private var run: Task<Void, Never>?
    @ObservationIgnored private var presenter: TerminalFramePresenter?
    @ObservationIgnored private var wantedGeometry: TerminalSessionGeometry?
    @ObservationIgnored private var resizing = false
    @ObservationIgnored private var stopped = false

    init(target: TerminalSessionTarget, connections: MobileConnections) {
        self.target = target; self.connections = connections
    }

    /// A fresh controller for the same route once this one ended or failed.
    func replacement() -> TerminalSessionController { .init(target: target, connections: connections) }

    /// `acknowledgingEarlierSessions` records the user's explicit acceptance
    /// that earlier sessions on this machine may have had unknown effects.
    /// Nothing from them is replayed; it only permits a new shell.
    func start(acknowledgingEarlierSessions: Bool = false) async {
        guard phase == .idle else { return }
        phase = .connecting
        do {
            if acknowledgingEarlierSessions {
                let journal = connections.journal
                for record in try await journal.records() where record.requiresReview && record.canResolve
                    && record.scope.workspaceID == target.workspaceID && record.scope.machineID == target.machineID {
                    try await journal.resolve(record)
                }
            }
            let connection = try await connections.manager.connect(workspaceID: target.workspaceID,
                expectedRevision: target.revision, machineID: target.machineID, routeID: target.routeID,
                credentialID: target.credentialID, confirmedByUser: true)
            self.connection = connection
            guard !stopped else { return await teardown() }
            // The first viewport proposal replaces this provisional geometry.
            let geometry = try TerminalSessionGeometry(columns: 80, rows: 24)
            let pump = try await connection.openTerminal(geometry: geometry)
            self.pump = pump
            let model = try NativeTerminalModel(source: pump.source, geometry: geometry)
            if let palette = target.basePalette { try model.setBasePalette(palette) }
            let presenter = TerminalFramePresenter()
            presenter.onUnavailable = { [weak self] reason in
                self?.fail("The display is unavailable (\(Self.describe(reason))).")
            }
            self.presenter = presenter
            let coordinator = connections.coordinator
            let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: presenter)
            self.runtime = runtime
            let stream = try await coordinator.makeStream(sceneID: UUID())
            let focus = try await coordinator.changeFocus(stream, to: pump.source)
            self.focus = focus
            let keyboard = try TerminalKeyboardSession(focus: focus, coordinator: coordinator, encoder: runtime)
            let view = try await NativeTerminalView(session: keyboard)
            presenter.view = view
            view.onViewportChange = { [weak self] viewport in self?.propose(viewport) }
            view.onFailure = { [weak self] error in self?.fail(Self.describe(error)) }
            terminal = view
            guard !stopped else { return await teardown() }
            phase = .live
            view.setSceneActive(true)
            view.requestKeyboard(true)
            run = Task { [weak self] in
                let end: Result<TerminalSessionEnd, Error>
                do { end = .success(try await pump.run(consumer: runtime)) } catch { end = .failure(error) }
                await self?.pumpEnded(end)
            }
        } catch TerminalSessionJournalError.reviewRequired {
            phase = .needsReview
            await teardown()
        } catch {
            phase = .failed(Self.describe(error))
            await teardown()
        }
    }

    /// User-facing text: an error's own description when it has one.
    static func describe(_ error: any Error) -> String {
        (error as? LocalizedError)?.errorDescription ?? String(describing: error)
    }

    /// Output EOF is not proof that the remote process exited, so no reason
    /// claims a shell's fate.
    static func describe(_ end: TerminalSessionEnd) -> String {
        switch end {
        case .peerOutputEnded: "The machine closed this session."
        case .cancelled: "This device closed the session, for example when FrankenTerm left the foreground."
        case .disconnected: "The connection to the machine was lost."
        case .transportFailed: "The connection to the machine failed."
        case .consumerFailed: "The terminal could not process the session's output."
        case .ownershipConflict: "Another owner took over this session's input."
        }
    }

    /// Applies the renderer's measured grid to the remote PTY. One native resize
    /// is in flight at a time; only the newest proposal is retained meanwhile.
    /// Kit's TerminalViewportResize is the intended owner of this once
    /// TerminalModelRuntime reports applied geometry (its didApply input); until
    /// then a refused (busy) request is retried briefly with the newest value.
    private func propose(_ viewport: NativeTerminalViewport) {
        guard let geometry = try? TerminalSessionGeometry(columns: UInt32(viewport.columns), rows: UInt32(viewport.rows),
            pixelWidth: UInt32(min(viewport.pixelWidth, 65_535)), pixelHeight: UInt32(min(viewport.pixelHeight, 65_535)))
        else { return }
        wantedGeometry = geometry
        guard !resizing else { return }
        resizing = true
        Task { [weak self] in await self?.drainResizes() }
    }

    private func drainResizes() async {
        defer { resizing = false }
        while let geometry = wantedGeometry, !stopped, let connection, let source = pump?.source {
            wantedGeometry = nil
            do { try await connection.requestResize(geometry, source: source) }
            catch NativeSSHError.busy {
                // The previous request still owns the native slot; retry the
                // newest geometry shortly instead of dropping it.
                if wantedGeometry == nil { wantedGeometry = geometry }
                try? await Task.sleep(for: .milliseconds(50))
            } catch { return }
        }
    }

    /// A display that cannot show this session's output must not keep sending
    /// it input: retire input now; the pump's end completes the teardown.
    private func fail(_ message: String) {
        guard !stopped, phase == .live else { return }
        phase = .failed(message)
        terminal?.retire(); pump?.requestStop()
    }

    /// Runs inside `run`, so it must not join that task.
    private func pumpEnded(_ end: Result<TerminalSessionEnd, Error>) async {
        guard !stopped else { return }
        if phase == .live { // keep an earlier failure's explanation
            switch end {
            case let .success(reason): phase = .ended(Self.describe(reason))
            case let .failure(error): phase = .failed(Self.describe(error))
            }
        }
        await teardown(joiningRun: false)
    }

    /// Idempotent. Retires input, stops the pump, settles the connection through
    /// the manager, joins the pump task and releases model and source.
    func stop() async {
        guard !stopped else { return }
        stopped = true
        await teardown()
    }

    private func teardown(joiningRun: Bool = true) async {
        terminal?.retire()
        pump?.requestStop()
        if let connection { try? await connections.manager.close(connectionID: connection.id) }
        if joiningRun, let run { await run.value }
        try? await runtime?.close()
        let coordinator = connections.coordinator
        if let focus { _ = try? await coordinator.suspend(focus) }
        if let source = pump?.source { try? await coordinator.retire(source) }
        connection = nil; runtime = nil; focus = nil; run = nil
        settled = true
    }
}

/// Full-screen terminal for one chosen machine route.
struct TerminalSessionScreen: View {
    @State var controller: TerminalSessionController

    var body: some View {
        let background = controller.target.background.map { Color(ftHex: $0) } ?? .black
        ZStack {
            background.ignoresSafeArea()
            TerminalHost(view: controller.terminal)
            switch controller.phase {
            case .idle, .connecting:
                ProgressView("Connecting to \(controller.target.machineName)…").tint(.white).foregroundStyle(.white)
            case .live:
                EmptyView()
            case .needsReview:
                notice(symbol: "questionmark.circle", title: "Earlier session on this machine",
                       message: "A previous session here ended before everything typed in it had a known outcome.")
            case let .ended(message):
                notice(symbol: "checkmark.circle", title: "Session ended", message: message)
            case let .failed(message):
                notice(symbol: "exclamationmark.triangle", title: "Session unavailable", message: message)
            }
        }
        .navigationTitle(controller.target.machineName)
        .navigationBarTitleDisplayMode(.inline)
        .toolbarBackground(background, for: .navigationBar)
        .toolbarBackground(.visible, for: .navigationBar)
        .toolbarColorScheme(TerminalSessionTarget.isDark(controller.target.background) ? .dark : .light,
                            for: .navigationBar)
        .task { await controller.start() }
        .onDisappear { Task { await controller.stop() } }
    }

    /// Every way out of a session states that its last input may or may not
    /// have run; the only action is an informed, explicit new session.
    private func notice(symbol: String, title: String, message: String) -> some View {
        VStack(spacing: 12) {
            Image(systemName: symbol).font(.largeTitle)
            Text(title).font(.headline)
            Text(verbatim: message).multilineTextAlignment(.center).font(.callout)
            Text("Commands typed just before it ended may or may not have run. Nothing is sent again automatically; check the machine before repeating one.")
                .multilineTextAlignment(.center).font(.caption)
            Button("Start New Session", systemImage: "arrow.clockwise", action: startNewSession)
                .buttonStyle(.borderedProminent).disabled(!controller.settled)
        }.padding(24).foregroundStyle(.white)
            .background(.black.opacity(0.8), in: RoundedRectangle(cornerRadius: 16)).padding()
    }

    /// The ended controller already tore down; its successor acknowledges the
    /// unknown outcome the user just read and connects anew on the same route
    /// through the same authorization and host checks.
    private func startNewSession() {
        let next = controller.replacement()
        controller = next
        Task { await next.start(acknowledgingEarlierSessions: true) }
    }
}

/// Hosts the asynchronously created NativeTerminalView.
private struct TerminalHost: UIViewRepresentable {
    let view: NativeTerminalView?
    func makeUIView(context: Context) -> UIView { UIView() }
    func updateUIView(_ container: UIView, context: Context) {
        guard let view, view.superview !== container else { return }
        container.subviews.forEach { $0.removeFromSuperview() }
        view.frame = container.bounds
        view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        container.addSubview(view)
    }
}
