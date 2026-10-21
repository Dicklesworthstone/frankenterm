import Foundation
import FrankenTermKit
import FrankenTermSSH

public enum WorkspaceSSHConnectionError: Error, Equatable, Sendable, LocalizedError {
    case offline, busy, capacity, stale
    public var errorDescription: String? {
        switch self {
        case .offline: return "Connections are paused. Resume while the app is active to connect."
        case .busy: return "This machine already has a connection or an attempt in progress."
        case .capacity: return "The connection limit has been reached. Close a session before connecting another machine."
        case .stale: return "This connection changed or closed. Select the machine again."
        }
    }
}

/// One app-owned manager shared by scenes. It admits at most three attempts and
/// sixteen retained owners, with one owner per installed workspace and machine.
/// Networking starts disabled. Imported preferences and row selection do not
/// call connect; each attempt requires explicit user confirmation. All native
/// work uses WorkspaceNativeSSHConnection and its process-wide offline exclusion.
public actor WorkspaceSSHConnectionManager {
    private struct Machine: Hashable, Sendable { let workspaceID: UUID; let machineID: String }
    private struct Pending {
        let id: UUID
        let task: Task<WorkspaceSSHConnection, Error>
    }
    private let store: WorkspaceStore, broker: SSHAuthenticationBroker
    private let journal: TerminalSessionJournal, coordinator: TerminalInputCoordinator
    private let networkActivity: WorkspaceNetworkActivity
    private var foreground = false, requestedForeground = false, mustSuspend = false
    private var lifecycle = UUID()
    private var transition: Task<Void, Never>?
    private var pending: [Machine: Pending] = [:]
    private var live: [Machine: WorkspaceSSHConnection] = [:]
    private var settlements: [UUID: Task<Void, Error>] = [:]
    private var retiringWorkspaces: [UUID: Int] = [:]

    public init(store: WorkspaceStore, broker: SSHAuthenticationBroker,
                journal: TerminalSessionJournal, coordinator: TerminalInputCoordinator,
                networkActivity: WorkspaceNetworkActivity = .shared) {
        self.store = store; self.broker = broker; self.journal = journal
        self.coordinator = coordinator; self.networkActivity = networkActivity
    }

    public func resume() async {
        requestedForeground = true
        await synchronizeForeground().value
    }

    /// One retained driver coalesces lifecycle requests. A suspend must cross
    /// the broker even if resume arrives before its actor hop completes, so old
    /// foreground witnesses never revive. The latest requested state wins.
    private func synchronizeForeground() -> Task<Void, Never> {
        if let transition { return transition }
        let task = Task { await driveForeground() }
        transition = task
        return task
    }

    private func driveForeground() async {
        defer { transition = nil }
        while true {
            if mustSuspend {
                mustSuspend = false
                await broker.suspend()
                foreground = false
                continue
            }
            guard requestedForeground else { return }
            let generation = lifecycle
            await broker.resume()
            guard !mustSuspend, requestedForeground, lifecycle == generation else { continue }
            foreground = true
            return
        }
    }

    /// Seal every source before joining any prompt, creation write or native
    /// teardown. A failed journal settlement retains its closed owner for retry.
    public func suspend() async {
        foreground = false; requestedForeground = false; mustSuspend = true; lifecycle = UUID()
        let stateChange = synchronizeForeground()
        let attempts = Array(pending.values), connections = Array(live)
        for attempt in attempts { attempt.task.cancel() }
        for (_, connection) in connections { await connection.sealAdmission() }
        await stateChange.value
        await settle(attempts: attempts, connections: connections)
    }

    /// Call after reimport, revocation or a local policy edit. The native owner
    /// independently revalidates current durable authority before every I/O.
    public func invalidate(workspaceID: UUID) async {
        retiringWorkspaces[workspaceID, default: 0] += 1
        defer {
            if retiringWorkspaces[workspaceID] == 1 { retiringWorkspaces.removeValue(forKey: workspaceID) }
            else { retiringWorkspaces[workspaceID, default: 1] -= 1 }
        }
        let attempts = pending.filter { $0.key.workspaceID == workspaceID }.map(\.value)
        let connections = live.filter { $0.key.workspaceID == workspaceID }.map { ($0.key, $0.value) }
        for attempt in attempts { attempt.task.cancel() }
        for (_, connection) in connections { await connection.sealAdmission() }
        await settle(attempts: attempts, connections: connections)
    }

    public func connections() -> [WorkspaceSSHConnection] {
        live.values.sorted {
            if $0.scope.workspaceID != $1.scope.workspaceID {
                return $0.scope.workspaceID.uuidString < $1.scope.workspaceID.uuidString
            }
            return $0.identity.machineID < $1.identity.machineID
        }
    }
    public var pendingCount: Int { pending.count }
    public var permitsConnections: Bool { foreground }

    public func connect(workspaceID: UUID, expectedRevision: UUID, machineID: String,
                        routeID: String, credentialID: String, confirmedByUser: Bool,
                        connectionTimeout: Duration = .seconds(15),
                        authenticationTimeout: Duration = .seconds(60)) async throws -> WorkspaceSSHConnection {
        guard confirmedByUser else { throw WorkspaceNetworkError.confirmationRequired }
        guard foreground else { throw WorkspaceSSHConnectionError.offline }
        guard retiringWorkspaces[workspaceID] == nil else { throw WorkspaceSSHConnectionError.stale }
        guard connectionTimeout > .zero, connectionTimeout <= .seconds(120),
              authenticationTimeout > .zero, authenticationTimeout <= .seconds(120) else {
            throw NativeSSHError.invalidInput
        }
        try Task.checkCancellation()
        let machine = Machine(workspaceID: workspaceID, machineID: machineID)
        guard pending[machine] == nil, live[machine] == nil else { throw WorkspaceSSHConnectionError.busy }
        guard pending.count < 3, pending.count + live.count + settlements.count < 16 else {
            throw WorkspaceSSHConnectionError.capacity
        }
        let generation = lifecycle, attemptID = UUID()
        let store = self.store, broker = self.broker, journal = self.journal
        let coordinator = self.coordinator, networkActivity = self.networkActivity
        let task = Task<WorkspaceSSHConnection, Error> {
            let native = try await WorkspaceNativeSSHConnection.connect(workspaceID: workspaceID,
                expectedRevision: expectedRevision, machineID: machineID, routeID: routeID,
                credentialID: credentialID, confirmedByUser: confirmedByUser,
                store: store, broker: broker, journal: journal, networkActivity: networkActivity,
                connectionTimeout: connectionTimeout, authenticationTimeout: authenticationTimeout)
            do {
                try Task.checkCancellation()
                return WorkspaceSSHConnection(native: native, journal: journal, coordinator: coordinator)
            } catch {
                await native.close()
                throw error
            }
        }
        pending[machine] = .init(id: attemptID, task: task)
        defer { if pending[machine]?.id == attemptID { pending.removeValue(forKey: machine) } }
        let connection = try await withTaskCancellationHandler {
            try await task.value
        } onCancel: { task.cancel() }
        do {
            try Task.checkCancellation()
            guard foreground, lifecycle == generation, retiringWorkspaces[workspaceID] == nil,
                  pending[machine]?.id == attemptID else { throw WorkspaceSSHConnectionError.stale }
            try await connection.validateForPublication()
            try Task.checkCancellation()
            guard foreground, lifecycle == generation, retiringWorkspaces[workspaceID] == nil,
                  pending[machine]?.id == attemptID else { throw WorkspaceSSHConnectionError.stale }
            live[machine] = connection
            return connection
        } catch {
            try? await connection.close()
            throw error
        }
    }

    /// Remove only the exact owner after its durable settlement succeeds. A
    /// delayed close cannot remove a replacement connection for the same machine.
    public func close(connectionID: UUID) async throws {
        guard let (machine, connection) = live.first(where: { $0.value.id == connectionID }) else { return }
        try await settleConnection(machine, connection).value
    }

    private func settle(attempts: [Pending], connections: [(Machine, WorkspaceSSHConnection)]) async {
        // Repeated lifecycle signals share one job per exact owned attempt or
        // connection. Start all bounded jobs before waiting on any of them.
        let jobs = attempts.map(settleAttempt) + connections.map { settleConnection($0.0, $0.1) }
        for job in jobs { try? await job.value }
    }

    private func settleAttempt(_ attempt: Pending) -> Task<Void, Error> {
        if let existing = settlements[attempt.id] { return existing }
        let id = attempt.id, operation = attempt.task
        let job = Task<Void, Error> {
            defer { settlements.removeValue(forKey: id) }
            if let connection = try? await operation.value { try await connection.close() }
        }
        settlements[id] = job
        return job
    }

    private func settleConnection(_ machine: Machine, _ connection: WorkspaceSSHConnection) -> Task<Void, Error> {
        let id = connection.id
        if let existing = settlements[id] { return existing }
        let job = Task<Void, Error> {
            defer { settlements.removeValue(forKey: id) }
            try await connection.close()
            if live[machine]?.id == id { live.removeValue(forKey: machine) }
        }
        settlements[id] = job
        return job
    }
}

/// The managed connection exposes the existing native owner through a durable
/// creation boundary. There is no alternate signer, transport or native session
/// here. The journal and coordinator cannot be replaced after admission.
public final class WorkspaceSSHConnection: Sendable {
    public let id = UUID()
    public let identity: TerminalConnectionIdentity
    public let peer: NativeSSHPeer
    public let scope: TerminalSessionScope
    private let native: WorkspaceNativeSSHConnection
    private let journal: TerminalSessionJournal, coordinator: TerminalInputCoordinator

    fileprivate init(native: WorkspaceNativeSSHConnection, journal: TerminalSessionJournal,
                     coordinator: TerminalInputCoordinator) {
        self.native = native; self.journal = journal; self.coordinator = coordinator
        identity = native.identity; peer = native.peer; scope = native.scope
    }
    public var isClosed: Bool { get async { await native.isClosed } }

    public func openTerminal(geometry: TerminalSessionGeometry) async throws -> TerminalSessionPump {
        try await native.openTerminal(geometry: NativeSSHGeometry(columns: geometry.columns, rows: geometry.rows,
            pixelWidth: geometry.pixelWidth, pixelHeight: geometry.pixelHeight),
            coordinator: coordinator, journal: journal)
    }

    public func requestResize(_ geometry: TerminalSessionGeometry, source: TerminalSource) async throws {
        try await native.requestResize(NativeSSHGeometry(columns: geometry.columns, rows: geometry.rows,
            pixelWidth: geometry.pixelWidth, pixelHeight: geometry.pixelHeight), source: source)
    }

    /// Call when the pump ends, including EOF/error. Closing preserves unresolved
    /// remote state. The scene separately joins its consumer and reconciles the
    /// published source's outcomes; manager.close also releases its admission slot.
    public func close() async throws { try await native.closeAndRecordInterruption() }
    fileprivate func sealAdmission() async { await native.sealAdmission() }
    fileprivate func validateForPublication() async throws { try await native.validateForPublication() }
}
