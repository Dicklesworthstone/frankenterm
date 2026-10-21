import Foundation
import XCTest
@testable import FrankenTermKit

/// A real store transaction pauses at its persistence boundary while the restore
/// actor processes a scene dismissal. No timing-dependent sleeps are required.
private final class RestoreNetworkPersistence: WorkspacePersistence, @unchecked Sendable {
    enum Boundary { case catalogRead, catalogWrite }
    let base = StoreTestPersistence()
    private let condition = NSCondition()
    private var boundary: Boundary?
    private var reached = false
    private var released = false
    private var waiter: CheckedContinuation<Void, any Error>?

    func arm(_ boundary: Boundary) {
        condition.lock(); defer { condition.unlock() }
        self.boundary = boundary; reached = false; released = false
    }
    func waitUntilReached() async throws {
        try await withCheckedThrowingContinuation { continuation in
            condition.lock()
            if reached {
                condition.unlock(); continuation.resume(); return
            }
            waiter = continuation
            condition.unlock()
            DispatchQueue.global().asyncAfter(deadline: .now() + 5) { [self] in
                condition.lock()
                let expired = waiter; waiter = nil
                condition.unlock()
                expired?.resume(throwing: FTError.storage)
            }
        }
    }
    func unblock() {
        condition.lock(); defer { condition.unlock() }
        released = true; condition.broadcast()
    }
    private func pause(at point: Boundary) throws {
        condition.lock()
        guard boundary == point else { condition.unlock(); return }
        boundary = nil; reached = true
        let observer = waiter; waiter = nil
        condition.unlock()
        observer?.resume()
        condition.lock(); defer { condition.unlock() }
        let deadline = Date().addingTimeInterval(10)
        while !released {
            guard condition.wait(until: deadline) else { throw FTError.storage }
        }
    }
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T {
        try base.withExclusiveAccess(body)
    }
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? {
        if slot == .catalog { try pause(at: .catalogRead) }
        return try base.read(slot, limit: limit)
    }
    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws {
        if slot == .catalog { try pause(at: .catalogWrite) }
        try base.write(data, to: slot)
    }
    func removeJournal() throws { try base.removeJournal() }
}

@MainActor
final class OfflineRestoreNetworkTests: XCTestCase {
    private let crypto = TestCryptography()

    private func environment(network: WorkspaceNetworkActivity) -> (OfflineRestore<TestCryptography>, StoreTestPersistence) {
        let disk = StoreTestPersistence()
        let store = WorkspaceStore(persistence: disk, vault: StoreTestVault())
        return (OfflineRestore(using: crypto, store: store, networkActivity: network), disk)
    }

    func testActiveConnectionBlocksEntryAndImportBeforeAnyRestoreSideEffects() async throws {
        let network = WorkspaceNetworkActivity(), (restore, disk) = environment(network: network)
        let connection = try network.beginConnection()
        defer { withExtendedLifetime(connection) {} }
        let initial = await restore.status()
        do { try await restore.enterOfflineMode(); XCTFail("entered restore with a live connection") }
        catch { XCTAssertEqual(error as? WorkspaceNetworkError, .connectionsActive) }
        do {
            _ = try await restore.acceptWorkspaceDocument(sampleManifest(), generation: initial.generation)
            XCTFail("admitted a file with a live connection")
        } catch { XCTAssertEqual(error as? WorkspaceNetworkError, .connectionsActive) }
        do {
            _ = try await restore.observe("FTQR1/invalid", generation: initial.generation)
            XCTFail("processed a camera observation with a live connection")
        } catch { XCTAssertEqual(error as? WorkspaceNetworkError, .connectionsActive) }
        let after = await restore.status()
        XCTAssertEqual(after.phase, .idle); XCTAssertNil(after.source)
        XCTAssertEqual(network.accounting().connections, 1, "restoration must not silently close a shell")
        XCTAssertEqual(network.accounting().offlineRestores, 0)
        XCTAssertNil(disk.bytes(.catalog)); XCTAssertNil(disk.bytes(.journal))
    }

    func testStaleCallbacksCannotAcquireAnOfflineLease() async throws {
        let network = WorkspaceNetworkActivity(), (restore, _) = environment(network: network)
        let old = await restore.status()
        _ = try await restore.reset()
        do {
            _ = try await restore.observe("FTQR1/invalid", generation: old.generation)
            XCTFail("retired callback acquired offline admission")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        XCTAssertEqual(network.accounting().offlineRestores, 0)
        let connection = try network.beginConnection()
        withExtendedLifetime(connection) { XCTAssertEqual(network.accounting().connections, 1) }
    }

    func testBackgroundAndResetRetainOfflineExclusionUntilExplicitDismissal() async throws {
        let network = WorkspaceNetworkActivity(), (restore, _) = environment(network: network)
        try await restore.enterOfflineMode()
        try await restore.enterOfflineMode()
        XCTAssertEqual(network.accounting().offlineRestores, 1, "entry is idempotent for one restore")
        XCTAssertThrowsError(try network.beginConnection()) { XCTAssertEqual($0 as? WorkspaceNetworkError, .offlineRestoreActive) }
        _ = await restore.suspend()
        XCTAssertEqual(network.accounting().offlineRestores, 1)
        _ = await restore.resume()
        _ = try await restore.reset()
        XCTAssertThrowsError(try network.beginConnection()) { XCTAssertEqual($0 as? WorkspaceNetworkError, .offlineRestoreActive) }
        let dismissed = await restore.suspend(discardRecipientRequest: true)
        XCTAssertTrue(dismissed.isSuspended)
        XCTAssertEqual(network.accounting().offlineRestores, 0)
        let connection = try network.beginConnection()
        withExtendedLifetime(connection) { XCTAssertEqual(network.accounting().connections, 1) }
    }

    func testMultipleRestoreScenesKeepIndependentOfflineLeases() async throws {
        let network = WorkspaceNetworkActivity()
        let (first, _) = environment(network: network), (second, _) = environment(network: network)
        try await first.enterOfflineMode(); try await second.enterOfflineMode()
        XCTAssertEqual(network.accounting().offlineRestores, 2)
        _ = await first.suspend(discardRecipientRequest: true)
        XCTAssertEqual(network.accounting().offlineRestores, 1)
        XCTAssertThrowsError(try network.beginConnection()) { XCTAssertEqual($0 as? WorkspaceNetworkError, .offlineRestoreActive) }
        _ = await second.suspend(discardRecipientRequest: true)
        XCTAssertEqual(network.accounting().offlineRestores, 0)
    }

    func testDismissalCannotReleaseOfflineExclusionDuringReviewPreview() async throws {
        let network = WorkspaceNetworkActivity(), disk = RestoreNetworkPersistence()
        let restore = OfflineRestore(using: crypto, store: WorkspaceStore(persistence: disk, vault: StoreTestVault()), networkActivity: network)
        let initial = await restore.status(), document = try sampleManifest()
        disk.arm(.catalogRead)
        defer { disk.unblock() }
        let operation = Task { try await restore.acceptWorkspaceDocument(document, generation: initial.generation) }
        try await disk.waitUntilReached()
        _ = await restore.suspend(discardRecipientRequest: true)
        XCTAssertEqual(network.accounting().offlineRestores, 1)
        XCTAssertThrowsError(try network.beginConnection()) { XCTAssertEqual($0 as? WorkspaceNetworkError, .offlineRestoreActive) }
        disk.unblock()
        do { _ = try await operation.value; XCTFail("dismissed review was published") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        XCTAssertEqual(network.accounting().offlineRestores, 0)
        let after = await restore.status()
        XCTAssertTrue(after.isSuspended); XCTAssertNil(after.review)
        XCTAssertNil(disk.base.bytes(.catalog))
    }

    func testDismissalCannotReleaseOfflineExclusionDuringPublication() async throws {
        let network = WorkspaceNetworkActivity(), disk = RestoreNetworkPersistence()
        let store = WorkspaceStore(persistence: disk, vault: StoreTestVault())
        let restore = OfflineRestore(using: crypto, store: store, networkActivity: network)
        let initial = await restore.status()
        let ready = try await restore.acceptWorkspaceDocument(sampleManifest(), generation: initial.generation)
        let review = try XCTUnwrap(ready.review)
        disk.arm(.catalogWrite)
        defer { disk.unblock() }
        let operation = Task {
            try await restore.commit(reviewToken: review.token, generation: ready.generation, sourceAndContentsConfirmed: true)
        }
        try await disk.waitUntilReached()
        _ = await restore.suspend(discardRecipientRequest: true)
        XCTAssertEqual(network.accounting().offlineRestores, 1)
        XCTAssertThrowsError(try network.beginConnection()) { XCTAssertEqual($0 as? WorkspaceNetworkError, .offlineRestoreActive) }
        disk.unblock()
        let result = try await operation.value
        XCTAssertEqual(result.phase, .complete); XCTAssertTrue(result.isSuspended); XCTAssertNil(result.receipt)
        XCTAssertEqual(network.accounting().offlineRestores, 0)
        let saved = try await store.load()
        XCTAssertEqual(saved.count, 1); XCTAssertTrue(saved[0].connectLater)
    }

    func testDismissalCannotReleaseOfflineExclusionDuringPublicationRecovery() async throws {
        let network = WorkspaceNetworkActivity(), disk = RestoreNetworkPersistence()
        let store = WorkspaceStore(persistence: disk, vault: StoreTestVault())
        let restore = OfflineRestore(using: crypto, store: store, networkActivity: network)
        let initial = await restore.status()
        let ready = try await restore.acceptWorkspaceDocument(sampleManifest(), generation: initial.generation)
        let review = try XCTUnwrap(ready.review)
        disk.base.configure(.after(.catalog))
        do {
            _ = try await restore.commit(reviewToken: review.token, generation: ready.generation, sourceAndContentsConfirmed: true)
            XCTFail("publication fault was ignored")
        } catch { XCTAssertEqual(error as? FTError, .storage) }
        let uncertain = await restore.status()
        XCTAssertEqual(uncertain.phase, .recoveryRequired)
        disk.arm(.catalogRead)
        defer { disk.unblock() }
        let operation = Task { try await restore.reconcile(generation: uncertain.generation) }
        try await disk.waitUntilReached()
        _ = await restore.suspend(discardRecipientRequest: true)
        XCTAssertEqual(network.accounting().offlineRestores, 1)
        XCTAssertThrowsError(try network.beginConnection()) { XCTAssertEqual($0 as? WorkspaceNetworkError, .offlineRestoreActive) }
        disk.unblock()
        let result = try await operation.value
        XCTAssertEqual(result.phase, .complete); XCTAssertTrue(result.isSuspended)
        XCTAssertEqual(network.accounting().offlineRestores, 0)
        XCTAssertNil(disk.base.bytes(.journal))
        let saved = try await store.load()
        XCTAssertEqual(saved.count, 1)
    }
}

#if canImport(MultipeerConnectivity) && canImport(CryptoKit)
@MainActor
final class TransferCompletionTests: XCTestCase {
    let arrival = TransferArrival(envelopeDigest: Data(repeating: 0xA5, count: 32), workspaceDigest: Data(repeating: 7, count: 32))

    /// The proof binds both the transfer and its decrypted manifest; the tag
    /// only finds the Mac and is what the animation already shows.
    func testTheProofNeedsTheDecryptedManifestAndThisTransfer() {
        XCTAssertEqual(TransferCompletion.tag(arrival), "A5A5A5A5A5A5A5A5")
        let proof = TransferCompletion.proof(arrival)
        XCTAssertEqual(proof.count, 32)
        XCTAssertEqual(TransferCompletion.proof(arrival), proof)
        XCTAssertNotEqual(TransferCompletion.proof(.init(envelopeDigest: arrival.envelopeDigest,
                                                         workspaceDigest: Data(repeating: 8, count: 32))), proof)
        XCTAssertNotEqual(TransferCompletion.proof(.init(envelopeDigest: Data(repeating: 0xA4, count: 32),
                                                         workspaceDigest: arrival.workspaceDigest)), proof)
    }

    /// A real Multipeer exchange on this machine: the listener hears the
    /// matching notice and nothing from a phone that holds another manifest.
    func testTheMacHearsOnlyThePhoneThatOpenedItsTransfer() async throws {
        var heard = 0
        let listener = TransferCompletionListener(arrival: arrival) { heard += 1 }
        defer { listener.stop() }
        let forged = TransferArrival(envelopeDigest: arrival.envelopeDigest, workspaceDigest: Data(repeating: 9, count: 32))
        let forgedAnswered = await TransferCompletionNotice.send(forged, timeout: .seconds(10))
        XCTAssertTrue(forgedAnswered, "the advertised Mac was not found")
        XCTAssertEqual(heard, 0)
        let answered = await TransferCompletionNotice.send(arrival, timeout: .seconds(10))
        XCTAssertTrue(answered)
        for _ in 0..<50 where heard == 0 { try await Task.sleep(for: .milliseconds(100)) }
        XCTAssertEqual(heard, 1)
    }
}
#endif
