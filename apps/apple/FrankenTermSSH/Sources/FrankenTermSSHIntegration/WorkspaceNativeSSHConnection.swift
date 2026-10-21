import Foundation
import FrankenTermKit
import FrankenTermSSH

/// One explicitly requested route, from local admission through authenticated
/// SSH and the existing terminal pump. No imported auto-connect preference calls
/// this factory. No raw native session escapes its network-scope owner.
///
/// The app cancels connect and closes its owned connections on deactivation. The
/// broker's foreground witness also rejects a result crossing suspend/resume.
/// A direct-SSH close has no remote-shell survival guarantee and never reconnects
/// or repeats session creation automatically.
public actor WorkspaceNativeSSHConnection {
    public nonisolated let identity: TerminalConnectionIdentity
    public nonisolated let peer: NativeSSHPeer
    public nonisolated let workspaceRevision: UUID
    private let transport: WorkspaceNativeSSHTransport
    private var attached: TerminalSessionPump?
    private var opening = false, closed = false
    private var shutdown: Task<Void, Never>?

    private init(transport: WorkspaceNativeSSHTransport, peer: NativeSSHPeer, workspaceRevision: UUID) {
        self.transport = transport; self.peer = peer; identity = transport.identity
        self.workspaceRevision = workspaceRevision
    }

    /// The explicit confirmation is independent of imported preferences and is
    /// required on every new connection. The durable Connect Later bit remains
    /// intact, including across relaunch. A stale/disabled/unsupported route or
    /// missing selected identity is refused before hostname resolution begins.
    public static func connect(workspaceID: UUID, expectedRevision: UUID, machineID: String,
                               routeID: String, credentialID: String, confirmedByUser: Bool,
                               store: WorkspaceStore, broker: SSHAuthenticationBroker,
                               networkActivity: WorkspaceNetworkActivity = .shared,
                               connectionTimeout: Duration = .seconds(15),
                               authenticationTimeout: Duration = .seconds(60)) async throws -> WorkspaceNativeSSHConnection {
        guard confirmedByUser else { throw WorkspaceNetworkError.confirmationRequired }
        try Task.checkCancellation()
        let foreground = try await broker.foregroundLease()
        let lease = try networkActivity.beginConnection()
        let entries = try await store.loadWithReadiness(support: WorkspaceNativeSSHAuthentication.routeSupport)
        guard let entry = entries.first(where: { $0.workspace.id == workspaceID }),
              entry.workspace.revision == expectedRevision else { throw SSHAuthenticationError.stale }
        let document = try entry.workspace.document()
        guard let machine = document.machines.first(where: { $0.id == machineID }),
              machine.routeIDs.contains(routeID),
              let route = document.routes.first(where: { $0.id == routeID }),
              route.mode == "direct_ssh", route.jumpRoutes.isEmpty,
              route.sessionCreation == "on_select" || route.sessionCreation == "manual",
              let ready = entry.routes.first(where: { $0.routeID == routeID }),
              ready.isLocallyReady, ready.usableCredentialIDs.contains(credentialID) else {
            throw SSHAuthenticationError.denied
        }
        let endpoint = try NativeSSHEndpoint(host: route.host, port: route.port)
        try await broker.validateForeground(foreground)
        try Task.checkCancellation()
        let session = try await NativeSSHConnector.connect(to: endpoint, timeout: connectionTimeout, networkLifetime: lease)
        do {
            try await broker.validateForeground(foreground)
            try Task.checkCancellation()
            let authority = WorkspaceNativeSSHAuthentication(broker: broker, workspaceID: workspaceID,
                revision: expectedRevision, routeID: routeID, credentialID: credentialID,
                transportID: session.transportID, endpoint: endpoint)
            let peer = try await session.authenticate(using: authority, timeout: authenticationTimeout)
            try await broker.validateForeground(foreground)
            try Task.checkCancellation()
            // This source never changes route revision in place. The complete
            // durable workspace revision is retained separately, and every new
            // source has an independent transport UUID and channel UUID.
            let identity = try TerminalConnectionIdentity(workspaceID: workspaceID.uuidString,
                machineID: machineID, routeID: routeID, routeRevision: 1,
                transportID: session.transportID, channelID: session.channelID)
            let transport = WorkspaceNativeSSHTransport(session: session, identity: identity, broker: broker,
                foreground: foreground, networkLease: lease, authority: authority)
            return WorkspaceNativeSSHConnection(transport: transport, peer: peer, workspaceRevision: expectedRevision)
        } catch {
            await session.close()
            // Keep exclusion until cancellation/readiness teardown has joined.
            withExtendedLifetime(lease) {}
            throw error
        }
    }

    /// Separate deliberate remote side effect. prepareForInput must durably
    /// record this exact source's uncertainty marker before returning. Failure
    /// creates no shell. The caller supplies the canonical terminal consumer to
    /// the returned pump; this owner adds no ANSI parser or local echo.
    public func openTerminal(geometry: NativeSSHGeometry, coordinator: TerminalInputCoordinator,
                             prepareForInput: @Sendable (TerminalConnectionIdentity) async throws -> Void) async throws -> TerminalSessionPump {
        guard !closed, !opening, attached == nil else { throw NativeSSHError.busy }
        opening = true
        defer { opening = false }
        do {
            try await validateCurrent()
            try await prepareForInput(identity)
            try await validateCurrent()
            try await transport.openShell(geometry: geometry)
            try await validateCurrent()
            let pump = try await TerminalSessionPump.attach(transport, coordinator: coordinator)
            attached = pump
            do { try await validateCurrent() }
            catch {
                try? await pump.disconnect()
                try? await coordinator.retire(pump.source)
                throw error
            }
            return pump
        } catch { await close(); throw error }
    }

    public func requestResize(_ geometry: NativeSSHGeometry) async throws {
        do {
            try await validateCurrent()
            guard attached != nil else { throw NativeSSHError.closed }
            try await transport.requestResize(geometry)
        } catch { await close(); throw error }
    }

    /// Stop coordinator admission even when pump.run has not started. The
    /// private transport is never returned, so a second coordinator cannot
    /// claim this connection. Closing joins the actual native teardown.
    public func close() async {
        if let shutdown { await shutdown.value; return }
        closed = true
        let pump = attached, transport = self.transport
        attached = nil
        let task = Task<Void, Never> {
            if let pump { try? await pump.disconnect() }
            await transport.close()
        }
        shutdown = task
        await task.value
    }

    private func validateCurrent() async throws {
        try Task.checkCancellation()
        guard !closed else { throw NativeSSHError.closed }
        try await transport.validateForeground()
        guard !closed else { throw NativeSSHError.closed }
        try Task.checkCancellation()
    }

    deinit {
        let pump = attached, transport = self.transport
        Task {
            if let pump { try? await pump.disconnect() }
            await transport.close()
        }
    }
}

/// The sole byte-service capability stays inside the connection and its pump.
/// No cycle points back to the public owner, and every native operation repeats
/// the current foreground and durable workspace authorization checks.
private actor WorkspaceNativeSSHTransport: TerminalSessionTransport {
    nonisolated let identity: TerminalConnectionIdentity
    nonisolated let inputSignal = TerminalSessionSignal()
    private let session: NativeSSHSession
    private let broker: SSHAuthenticationBroker
    private let foreground: SSHForegroundLease
    private let authority: WorkspaceNativeSSHAuthentication
    private var driver: NativeSSHDriver?
    private var networkLease: WorkspaceNetworkLease?
    private var closed = false
    private var shutdown: Task<Void, Never>?

    init(session: NativeSSHSession, identity: TerminalConnectionIdentity, broker: SSHAuthenticationBroker,
         foreground: SSHForegroundLease, networkLease: WorkspaceNetworkLease,
         authority: WorkspaceNativeSSHAuthentication) {
        self.session = session; self.identity = identity; self.broker = broker
        self.foreground = foreground; self.networkLease = networkLease; self.authority = authority
    }

    func openShell(geometry: NativeSSHGeometry) async throws {
        do {
            try await validateForeground()
            guard driver == nil else { throw NativeSSHError.busy }
            try await session.openShell(geometry: geometry)
            try await validateForeground()
            driver = try await session.claimTerminalDriver(transportID: identity.transportID, channelID: identity.channelID)
            try await validateForeground()
        } catch { await close(); throw error }
    }

    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        do {
            try await validateForeground()
            guard let driver else { throw NativeSSHError.closed }
            guard !bytes.isEmpty, bytes.count <= 65536 else { throw TerminalInputError.invalidWriteSize }
            switch try await session.admit(Data(bytes.prefix(NativeSSHSession.maximumWriteBytes)), driver: driver) {
            case .accepted(let count): return .accepted(count)
            case .wouldBlock: return .wouldBlock
            }
        } catch { await close(); throw error }
    }

    func service() async throws -> TerminalSessionUpdate {
        do {
            try await validateForeground()
            guard let driver else { throw NativeSSHError.closed }
            let event = try await session.service(driver: driver)
            try await validateForeground()
            let geometry = try event.resized.map {
                try TerminalSessionGeometry(columns: $0.columns, rows: $0.rows,
                                            pixelWidth: $0.pixelWidth, pixelHeight: $0.pixelHeight)
            }
            return .init(output: event.output, endOfOutput: event.endOfOutput, resized: geometry)
        } catch { await close(); throw error }
    }

    func requestResize(_ geometry: NativeSSHGeometry) async throws {
        do {
            try await validateForeground()
            guard let driver else { throw NativeSSHError.closed }
            try await session.requestResize(geometry, driver: driver)
        } catch { await close(); throw error }
    }

    func wakeService() async {
        guard !closed, let driver else { return }
        do { try await session.wakeService(driver: driver) }
        catch { await close() }
    }

    /// Join the native close before releasing offline exclusion. Concurrent
    /// callers await the same teardown, and no subsequent call can reopen it.
    func close() async {
        if let shutdown { await shutdown.value; return }
        closed = true; inputSignal.finish()
        let session = self.session, lease = networkLease
        let task = Task<Void, Never> {
            await session.close()
            withExtendedLifetime(lease) {}
        }
        shutdown = task
        await task.value
        networkLease = nil
    }

    func validateForeground() async throws {
        try Task.checkCancellation()
        guard !closed else { throw NativeSSHError.closed }
        try await broker.validateForeground(foreground)
        try await authority.validateCurrent()
        guard !closed else { throw NativeSSHError.closed }
        try Task.checkCancellation()
    }

    deinit {
        inputSignal.finish()
        let session = self.session, lease = networkLease
        Task {
            await session.close()
            withExtendedLifetime(lease) {}
        }
    }
}
