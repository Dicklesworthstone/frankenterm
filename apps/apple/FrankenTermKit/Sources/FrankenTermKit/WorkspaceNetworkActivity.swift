import Foundation

public enum WorkspaceNetworkError: Error, Equatable, Sendable, LocalizedError {
    case offlineRestoreActive, connectionsActive, capacity, confirmationRequired

    public var errorDescription: String? {
        switch self {
        case .offlineRestoreActive:
            return "Finish or dismiss Offline Restore before connecting to a machine."
        case .connectionsActive:
            return "Close the active connections before entering Offline Restore, or cancel restoration. Closing an ordinary SSH session may end its remote shell."
        case .capacity:
            return "The maximum number of active connection or restore operations has been reached."
        case .confirmationRequired:
            return "Choose Connect explicitly before using the network. Restoring a workspace keeps Connect Later enabled."
        }
    }
}

public struct WorkspaceNetworkAccounting: Equatable, Sendable {
    public let connections: Int
    public let offlineRestores: Int
}

/// Process-wide exclusion between application networking and strict restoration.
/// Admission happens before DNS or any socket creation. An offline request never
/// silently disconnects a shell: its caller must offer close-or-cancel explicitly.
///
/// Each owner retains its lease until all of its work has settled, including
/// cancelled native operations. Dropping a UI reference cannot release a lease
/// still retained by an asynchronous operation. This controls participating app
/// operations, not other processes or the device's radios.
public final class WorkspaceNetworkActivity: @unchecked Sendable {
    public static let shared = WorkspaceNetworkActivity()
    private let lock = NSLock()
    private var connections: Set<UUID> = []
    private var restores: Set<UUID> = []
    private let maximumConnections = 32
    private let maximumRestores = 8

    /// Separate instances support isolated tests. All production scenes and
    /// transports must share the default process-wide instance.
    public init() {}

    public func accounting() -> WorkspaceNetworkAccounting {
        lock.lock(); defer { lock.unlock() }
        return .init(connections: connections.count, offlineRestores: restores.count)
    }

    public func beginConnection() throws -> WorkspaceNetworkLease {
        try Task.checkCancellation()
        lock.lock(); defer { lock.unlock() }
        guard restores.isEmpty else { throw WorkspaceNetworkError.offlineRestoreActive }
        guard connections.count < maximumConnections else { throw WorkspaceNetworkError.capacity }
        let id = UUID()
        connections.insert(id)
        return WorkspaceNetworkLease(owner: self, id: id, offline: false)
    }

    public func beginOfflineRestore() throws -> WorkspaceNetworkLease {
        try Task.checkCancellation()
        lock.lock(); defer { lock.unlock() }
        guard connections.isEmpty else { throw WorkspaceNetworkError.connectionsActive }
        guard restores.count < maximumRestores else { throw WorkspaceNetworkError.capacity }
        let id = UUID()
        restores.insert(id)
        return WorkspaceNetworkLease(owner: self, id: id, offline: true)
    }

    fileprivate func release(_ id: UUID, offline: Bool) {
        lock.lock(); defer { lock.unlock() }
        if offline { restores.remove(id) } else { connections.remove(id) }
    }
}

/// Opaque shared lifetime. There is deliberately no early-release operation: an
/// in-flight import/connection holding the same reference keeps exclusion intact.
public final class WorkspaceNetworkLease: Sendable {
    private let owner: WorkspaceNetworkActivity
    private let id: UUID
    private let offline: Bool

    fileprivate init(owner: WorkspaceNetworkActivity, id: UUID, offline: Bool) {
        self.owner = owner; self.id = id; self.offline = offline
    }
    deinit { owner.release(id, offline: offline) }
}
