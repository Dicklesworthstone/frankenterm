import Foundation
@testable import FrankenTermKit

/// Fault-injected side-effect boundaries, not a second import implementation.
/// All accesses are serialized by this lock, including test fault configuration.
/// Its own file so the app-hosted test bundle can compile the Keychain-backed
/// session-history tests that use it.
final class StoreTestPersistence: WorkspacePersistence, @unchecked Sendable {
    enum Fault { case before(WorkspaceStorageSlot), after(WorkspaceStorageSlot), cleanup }
    private let lock = NSRecursiveLock()
    private var slots: [WorkspaceStorageSlot: Data] = [:]
    private var fault: Fault?
    private var followingFault: Fault?
    private var failedRead = false
    func configure(_ fault: Fault?, failedRead: Bool = false) {
        lock.lock(); defer { lock.unlock() }
        self.fault = fault; self.failedRead = failedRead
    }
    func configureWriteAndRecoveryFailure() {
        lock.lock(); defer { lock.unlock() }
        fault = .after(.catalog); followingFault = .before(.catalog)
    }
    func bytes(_ slot: WorkspaceStorageSlot) -> Data? {
        lock.lock(); defer { lock.unlock() }; return slots[slot]
    }
    func seed(_ data: Data, slot: WorkspaceStorageSlot) {
        lock.lock(); defer { lock.unlock() }; slots[slot] = data
    }
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T {
        lock.lock(); defer { lock.unlock() }; return try body()
    }
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? {
        lock.lock(); defer { lock.unlock() }
        if failedRead { throw FTError.storage }
        if let data = slots[slot], data.count > limit { throw FTError.limit("test.read") }
        return slots[slot]
    }
    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws {
        lock.lock(); defer { lock.unlock() }
        if case .before(let target) = fault, target == slot { fault = followingFault; followingFault = nil; throw FTError.storage }
        slots[slot] = data
        if case .after(let target) = fault, target == slot { fault = followingFault; followingFault = nil; throw FTError.storage }
    }
    func removeJournal() throws {
        lock.lock(); defer { lock.unlock() }
        if case .cleanup = fault { fault = nil; throw FTError.storage }
        slots[.journal] = nil
    }
}
