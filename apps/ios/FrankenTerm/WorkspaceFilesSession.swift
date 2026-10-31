import SwiftUI
import Observation
import FrankenTermKit
import FrankenTermSSH
import FrankenTermSSHIntegration

/// One explicit machine-level Files visit. Its admission and teardown are
/// independent of terminal creation, reconnect, input and background grace.
@MainActor
@Observable
final class WorkspaceFilesSession: Identifiable, Hashable {
    enum Phase { case idle, connecting, ready, failed, closed }

    nonisolated let id: UUID
    let target: TerminalSessionTarget
    let connections: MobileConnections
    private(set) var phase: Phase = .idle
    private(set) var controller: WorkspaceFilesController?
    private(set) var message: String?
    private(set) var cleanupFailure: String?
    @ObservationIgnored private let isCurrent: @MainActor () -> Bool
    @ObservationIgnored private let onRetired: @MainActor () -> Void
    @ObservationIgnored private var connection: WorkspaceSSHConnection?
    @ObservationIgnored private var operation: Task<Void, Never>?
    @ObservationIgnored private var retirement: Task<Void, Never>?
    @ObservationIgnored private var cleanup: Task<Bool, Never>?
    @ObservationIgnored private var retired = false

    init(id: UUID, target: TerminalSessionTarget, connections: MobileConnections,
         isCurrent: @escaping @MainActor () -> Bool, onRetired: @escaping @MainActor () -> Void) {
        self.id = id; self.target = target; self.connections = connections
        self.isCurrent = isCurrent; self.onRetired = onRetired
    }

    nonisolated static func == (lhs: WorkspaceFilesSession, rhs: WorkspaceFilesSession) -> Bool { lhs.id == rhs.id }
    nonisolated func hash(into hasher: inout Hasher) { hasher.combine(id) }

    var authenticationPrompt: MobileSSHAuthenticationSheetRequest? {
        guard phase == .connecting, let selection = target.routes.first?.selectedConnection else { return nil }
        return connections.fileAuthenticationPrompt(for: id, selection: selection)
    }

    func start() {
        guard phase == .idle, operation == nil, !retired else { return }
        phase = .connecting
        operation = Task { [self] in
            defer { operation = nil }
            do {
                try validate()
                guard target.routes.count == 1, let selection = target.routes.first?.selectedConnection else {
                    throw WorkspaceSSHConnectionSelectionError.stale
                }
                await connections.activate()
                try validate()
                let connected = try await connections.connectFiles(selection: selection, ownerID: id)
                // Retain every published owner before cancellation can throw.
                connection = connected
                try validate()
                let client = try await connected.openFiles()
                try validate()
                let files = WorkspaceFilesController(client: client, scope: connected.scope,
                    machineName: target.machineName, account: connected.user, host: connected.host, port: connected.port,
                    onClose: { [weak self] in await self?.releaseConnection() })
                controller = files
                phase = .ready
                await files.refresh()
            } catch {
                if phase != .closed {
                    phase = .failed
                    message = "Remote Files could not connect. " + error.localizedDescription
                }
                await releaseConnection()
            }
        }
    }

    /// Closes admission synchronously, then joins any in-flight sign-in or
    /// transfer. A retired visit is never silently reopened on foregrounding.
    func requestClose() {
        guard retirement == nil else { return }
        phase = .closed
        operation?.cancel()
        let pending = operation, files = controller
        retirement = Task { [self] in
            await files?.close()
            await pending?.value
            await releaseConnection()
            retirement = nil
        }
    }

    func retryCleanup() async {
        guard phase == .closed || phase == .failed || controller?.isClosed == true, operation == nil else { return }
        await releaseConnection()
    }

    func close() async {
        requestClose()
        await retirement?.value
    }

    private func validate() throws {
        try Task.checkCancellation()
        guard !retired, phase != .closed, isCurrent() else { throw WorkspaceSSHConnectionError.stale }
    }

    private func releaseConnection() async {
        if controller?.isClosed == true, phase != .failed { phase = .closed }
        if let cleanup { _ = await cleanup.value; return }
        guard let connection else {
            retire()
            return
        }
        let manager = connections.manager
        let task = Task { () -> Bool in
            do { try await manager.close(connectionID: connection.id); return true }
            catch { return false }
        }
        cleanup = task
        let settled = await task.value
        cleanup = nil
        if settled {
            self.connection = nil
            cleanupFailure = nil
            retire()
        } else {
            cleanupFailure = "The file connection is closed, but its cleanup has not finished. Retry cleanup before opening another Files connection."
        }
    }

    private func retire() {
        guard !retired else { return }
        retired = true
        onRetired()
    }
}

@MainActor
struct WorkspaceFilesSessionScreen: View {
    @Bindable var session: WorkspaceFilesSession
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        Group {
            if let controller = session.controller {
                WorkspaceFilesScreen(controller: controller)
            } else if session.phase == .idle || session.phase == .connecting {
                VStack(spacing: 16) {
                    ProgressView("Connecting to Remote Files…")
                    Text(verbatim: session.target.machineName).font(.headline)
                    Button("Cancel", role: .cancel) { session.requestClose(); dismiss() }
                }.padding()
            } else {
                ContentUnavailableView("Remote Files Closed", systemImage: "folder.badge.questionmark",
                    description: Text(session.message ?? "Choose Remote Files on the machine to review its route and connect again."))
            }
        }
        .navigationTitle("Remote Files").navigationBarTitleDisplayMode(.inline)
        .safeAreaInset(edge: .bottom) {
            if let failure = session.cleanupFailure {
                VStack(spacing: 8) {
                    Text(verbatim: failure).font(.footnote)
                    Button("Retry Cleanup") { Task { await session.retryCleanup() } }
                }.padding().background(.regularMaterial)
            }
        }
        .sheet(item: Binding(get: { session.authenticationPrompt }, set: { _ in })) { request in
            switch request {
            case .password(let password):
                MobileSSHPasswordSheet(prompt: session.connections.passwordPrompt, request: password)
            case .keyboardInteractive(let challenge):
                MobileSSHKeyboardInteractiveSheet(prompt: session.connections.keyboardInteractivePrompt, request: challenge)
            }
        }
        .task { session.start() }
        .onChange(of: scenePhase) { _, phase in if phase == .background { session.requestClose() } }
        .onDisappear { session.requestClose() }
    }
}
