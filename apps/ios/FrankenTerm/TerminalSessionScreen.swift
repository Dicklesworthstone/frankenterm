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

    /// `signer` defaults to the protected Keychain signer; tests inject one.
    init(store: WorkspaceStore, journalDirectory: URL,
         signer: any WorkspaceSSHSigning = AppleSSHCredentialSigner()) throws {
        let broker = SSHAuthenticationBroker(store: store, signer: signer,
                                             support: WorkspaceNativeSSHAuthentication.routeSupport)
        coordinator = try TerminalInputCoordinator()
        manager = WorkspaceSSHConnectionManager(store: store, broker: broker,
            journal: try TerminalSessionJournal(directory: journalDirectory), coordinator: coordinator)
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
        case ended(String)
        case failed(String)
    }
    private(set) var phase: Phase = .idle
    private(set) var terminal: NativeTerminalView?
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

    func start() async {
        guard phase == .idle else { return }
        phase = .connecting
        do {
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
            let presenter = TerminalFramePresenter()
            presenter.onUnavailable = { [weak self] reason in self?.phase = .failed("Display unavailable: \(reason)") }
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
            view.onFailure = { [weak self] error in self?.phase = .failed(String(describing: error)) }
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
        } catch {
            phase = .failed(String(describing: error))
            await teardown()
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

    private func pumpEnded(_ end: Result<TerminalSessionEnd, Error>) async {
        guard !stopped else { return }
        switch end {
        case .success(.peerOutputEnded): phase = .ended("The remote shell ended. Open the machine again to start a new session.")
        case let .success(other): phase = .ended("Session ended (\(other)).")
        case let .failure(error): phase = .failed(String(describing: error))
        }
        await teardown()
    }

    /// Idempotent. Retires input, stops the pump, settles the connection through
    /// the manager, joins the pump task and releases model and source.
    func stop() async {
        guard !stopped else { return }
        stopped = true
        await teardown()
    }

    private func teardown() async {
        terminal?.retire()
        pump?.requestStop()
        if let connection { try? await connections.manager.close(connectionID: connection.id) }
        if let run, Task.isCancelled == false { await run.value }
        try? await runtime?.close()
        let coordinator = connections.coordinator
        if let focus { _ = try? await coordinator.suspend(focus) }
        if let source = pump?.source { try? await coordinator.retire(source) }
        connection = nil; runtime = nil; focus = nil; run = nil
    }
}

/// Full-screen terminal for one chosen machine route.
struct TerminalSessionScreen: View {
    @State var controller: TerminalSessionController

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            TerminalHost(view: controller.terminal)
            switch controller.phase {
            case .idle, .connecting:
                ProgressView("Connecting to \(controller.target.machineName)…").tint(.white).foregroundStyle(.white)
            case .live:
                EmptyView()
            case let .ended(message), let .failed(message):
                VStack(spacing: 12) {
                    Image(systemName: controller.phase == .ended(message) ? "checkmark.circle" : "exclamationmark.triangle")
                        .font(.largeTitle)
                    Text(verbatim: message).multilineTextAlignment(.center).font(.callout)
                }.padding(24).foregroundStyle(.white)
                    .background(.black.opacity(0.75), in: RoundedRectangle(cornerRadius: 16)).padding()
            }
        }
        .navigationTitle(controller.target.machineName)
        .navigationBarTitleDisplayMode(.inline)
        .task { await controller.start() }
        .onDisappear { Task { await controller.stop() } }
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
