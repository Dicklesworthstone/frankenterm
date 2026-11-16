import Foundation
import XCTest
@testable import FrankenTermKit

private final class ScanRecoveryPersistence: WorkspacePersistence, @unchecked Sendable {
    enum Failure { case none, before, after }
    private let lock = NSRecursiveLock()
    var values: [String: Data] = [:]
    var failure = Failure.none
    var failingSlot = WorkspaceStorageSlot.catalog
    var writes = 0, reads = 0
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T { try lock.withLock(body) }
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? {
        try lock.withLock {
            reads += 1
            guard let data = values[slot.rawValue] else { return nil }
            guard data.count <= limit else { throw FTError.limit("test.read") }
            return data
        }
    }
    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws {
        try lock.withLock {
            writes += 1
            if slot == failingSlot, failure == .before { failure = .none; throw FTError.storage }
            values[slot.rawValue] = data
            if slot == failingSlot, failure == .after { failure = .none; throw FTError.storage }
        }
    }
    func removeJournal() throws { XCTFail("recovery must retain its namespace"); throw FTError.storage }
}

final class OpticalScanRecoveryTests: XCTestCase, @unchecked Sendable {
    let crypto = ScanCheckpointCryptography()
    private func checkpoint(_ indexes: [Int], seed: UInt8 = 1, damagedPage: Int? = nil) throws -> OpticalScanCheckpoint {
        let identity = OpticalFrame.Identity(transferID: Data(repeating: seed, count: 16),
            envelopeDigest: Data(repeating: seed, count: 32), envelopeLength: FTLimit.page * 4)
        var pages: [Int: Data] = [:], digests: [Int: Data] = [:]
        for index in indexes {
            let page = Data(repeating: damagedPage == index ? 250 : UInt8(index), count: FTLimit.page)
            pages[index] = page; digests[index] = crypto.sha256(page)
        }
        return try OpticalScanCheckpoint(identity: identity, pages: pages, pageDigests: digests, using: crypto)
    }
    private func requireReview(_ store: OpticalScanRecovery<ScanCheckpointCryptography>) async throws -> OpticalScanRecoveryReview {
        let review = try await store.review()
        return try XCTUnwrap(review)
    }
    private func expect(_ expected: OpticalScanRecoveryError,
                        _ operation: () async throws -> Void, file: StaticString = #filePath, line: UInt = #line) async {
        do { try await operation(); XCTFail("operation unexpectedly succeeded", file: file, line: line) }
        catch { XCTAssertEqual(error as? OpticalScanRecoveryError, expected, file: file, line: line) }
    }

    func testDefaultIsReadOnlyAndConsentPrecedesStorageAccess() async throws {
        let storage = ScanRecoveryPersistence(), store = OpticalScanRecovery(persistence: ScanRecoveryPersistence(), using: crypto)
        let empty = try await store.review()
        XCTAssertNil(empty)
        let subject = OpticalScanRecovery(persistence: storage, using: crypto)
        await expect(.consentRequired) { _ = try await subject.beginSaving(self.checkpoint([0]), confirmedByUser: false) }
        XCTAssertEqual(storage.writes, 0); XCTAssertEqual(storage.reads, 0); XCTAssertTrue(storage.values.isEmpty)
        let absent = try await subject.review()
        XCTAssertNil(absent); XCTAssertEqual(storage.writes, 0)
    }

    func testIndependentWritersMergeWithoutRegressingVerifiedProgress() async throws {
        let storage = ScanRecoveryPersistence()
        let first = OpticalScanRecovery(persistence: storage, using: crypto)
        let second = OpticalScanRecovery(persistence: storage, using: crypto)
        let permit = try await first.beginSaving(checkpoint([0]), confirmedByUser: true)
        async let a = first.save(checkpoint([1]), permit: permit)
        async let b = second.save(checkpoint([2]), permit: permit)
        _ = try await (a, b)
        let review = try await requireReview(second)
        XCTAssertEqual(review.verifiedPages, 3)
        XCTAssertEqual(review.incompletePageIndices, [3])
        let loaded = try await second.load(review)
        XCTAssertEqual(Set(loaded.pages.keys), [0, 1, 2])
        let smaller = try await first.save(checkpoint([0]), permit: permit)
        XCTAssertEqual(smaller.verifiedPages, 3)
    }

    func testOtherTransferCannotOverwriteSavedScanEvenWithConsent() async throws {
        let storage = ScanRecoveryPersistence(), saved = try checkpoint([0])
        let store = OpticalScanRecovery(persistence: storage, using: crypto)
        _ = try await store.beginSaving(saved, confirmedByUser: true)
        let before = storage.values
        await expect(.differentTransfer) { _ = try await store.beginSaving(self.checkpoint([1], seed: 2), confirmedByUser: true) }
        XCTAssertEqual(storage.values, before)
    }

    func testConflictingSameIdentityPageDoesNotReplaceEitherSource() async throws {
        let storage = ScanRecoveryPersistence()
        let subject = OpticalScanRecovery(persistence: storage, using: crypto)
        let permit = try await subject.beginSaving(checkpoint([0]), confirmedByUser: true)
        let before = storage.values
        await expect(.conflictingPage) { _ = try await subject.save(self.checkpoint([0], damagedPage: 0), permit: permit) }
        XCTAssertEqual(storage.values, before)
    }

    func testStaleReviewCannotDiscardOrLoadNewerProgress() async throws {
        let store = OpticalScanRecovery(persistence: ScanRecoveryPersistence(), using: crypto)
        let permit = try await store.beginSaving(checkpoint([0]), confirmedByUser: true)
        let old = try await requireReview(store)
        _ = try await store.save(checkpoint([1]), permit: permit)
        await expect(.staleReview) { try await store.discard(old) }
        await expect(.staleReview) { _ = try await store.load(old) }
        let current = try await requireReview(store)
        XCTAssertEqual(current.verifiedPages, 2)
    }

    func testDiscardTombstonePreventsDelayedWriterResurrection() async throws {
        let storage = ScanRecoveryPersistence()
        let store = OpticalScanRecovery(persistence: storage, using: crypto)
        let permit = try await store.beginSaving(checkpoint([0]), confirmedByUser: true)
        let reviewed = try await requireReview(store)
        try await store.discard(reviewed)
        let tombstone = try XCTUnwrap(storage.values["catalog"])
        XCTAssertEqual(tombstone.count, 60)
        await expect(.stalePermit) { _ = try await store.save(self.checkpoint([1]), permit: permit) }
        let restarted = OpticalScanRecovery(persistence: storage, using: crypto)
        let absent = try await restarted.review()
        XCTAssertNil(absent)
        _ = try await restarted.beginSaving(checkpoint([2], seed: 2), confirmedByUser: true)
        await expect(.stalePermit) { _ = try await restarted.save(self.checkpoint([1]), permit: permit) }
    }

    func testNewConsentFencesOldPermitButPreservesEarlierPages() async throws {
        let store = OpticalScanRecovery(persistence: ScanRecoveryPersistence(), using: crypto)
        let first = try await store.beginSaving(checkpoint([0]), confirmedByUser: true)
        let oldReview = try await requireReview(store)
        let second = try await store.beginSaving(checkpoint([1]), confirmedByUser: true)
        await expect(.stalePermit) { _ = try await store.save(self.checkpoint([2]), permit: first) }
        await expect(.staleReview) { try await store.discard(oldReview) }
        let review = try await store.save(checkpoint([2]), permit: second)
        XCTAssertEqual(review.verifiedPages, 3)
    }

    func testBeforePublicationFailurePreservesPriorProgressAndCanRetry() async throws {
        let storage = ScanRecoveryPersistence(), subject = try checkpoint([1])
        let store = OpticalScanRecovery(persistence: storage, using: crypto)
        let permit = try await store.beginSaving(checkpoint([0]), confirmedByUser: true)
        let old = storage.values
        storage.failure = .before
        do { _ = try await store.save(subject, permit: permit); XCTFail("injected failure") }
        catch { XCTAssertEqual(error as? FTError, .storage) }
        XCTAssertEqual(storage.values, old)
        let saved = try await store.save(subject, permit: permit)
        XCTAssertEqual(saved.verifiedPages, 2)
    }

    func testAfterPublicationErrorReconcilesActualPagesAndReaffirmsWrite() async throws {
        let storage = ScanRecoveryPersistence()
        let store = OpticalScanRecovery(persistence: storage, using: crypto)
        let permit = try await store.beginSaving(checkpoint([0]), confirmedByUser: true)
        storage.failure = .after
        do { _ = try await store.save(checkpoint([1]), permit: permit); XCTFail("injected failure") }
        catch { XCTAssertEqual(error as? FTError, .storage) }
        let observed = try await requireReview(store)
        XCTAssertEqual(observed.verifiedPages, 2)
        let writes = storage.writes
        let saved = try await store.save(checkpoint([0]), permit: permit)
        XCTAssertEqual(saved.verifiedPages, 2)
        XCTAssertEqual(storage.writes, writes + 1, "readback alone is not a durability claim")
    }

    func testInterruptedFirstNamespaceWriteCanBeRetriedWithoutNewDataLoss() async throws {
        for failure in [ScanRecoveryPersistence.Failure.before, .after] {
            let storage = ScanRecoveryPersistence()
            storage.failingSlot = .journal; storage.failure = failure
            let store = OpticalScanRecovery(persistence: storage, using: crypto)
            do { _ = try await store.beginSaving(checkpoint([0]), confirmedByUser: true); XCTFail("injected failure") }
            catch { XCTAssertEqual(error as? FTError, .storage) }
            XCTAssertNil(storage.values["catalog"])
            let permit = try await store.beginSaving(checkpoint([0]), confirmedByUser: true)
            let saved = try await store.save(checkpoint([1]), permit: permit)
            XCTAssertEqual(saved.verifiedPages, 2)
        }
    }

    func testWrongNamespaceOrCorruptionNeverOverwritesExistingStorage() async throws {
        for slot in ["catalog", "journal"] {
            let storage = ScanRecoveryPersistence()
            storage.values[slot] = Data("{\"workspace\":1}".utf8)
            let before = storage.values
            let store = OpticalScanRecovery(persistence: storage, using: crypto)
            do { _ = try await store.beginSaving(checkpoint([0]), confirmedByUser: true); XCTFail("wrong namespace") }
            catch { XCTAssertEqual(error as? OpticalScanRecoveryError, .wrongNamespace) }
            XCTAssertEqual(storage.values, before); XCTAssertEqual(storage.writes, 0)
        }
        let storage = ScanRecoveryPersistence()
        let subject = OpticalScanRecovery(persistence: storage, using: crypto)
        _ = try await subject.beginSaving(checkpoint([0]), confirmedByUser: true)
        storage.values["catalog"]![100] ^= 1
        let before = storage.values
        do { _ = try await subject.review(); XCTFail("corrupt snapshot") }
        catch { XCTAssertEqual(error as? OpticalScanRecoveryError, .corruptStore) }
        do { _ = try await subject.beginSaving(checkpoint([1]), confirmedByUser: true); XCTFail("corrupt snapshot replaced") }
        catch { XCTAssertEqual(error as? OpticalScanRecoveryError, .corruptStore) }
        XCTAssertEqual(storage.values, before)
    }

    func testActualFileBackedRestartPreservesExactBytesAndPrivatePermissions() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("ft-scan-test-" + UUID().uuidString)
        let store = try OpticalScanRecovery(directory: directory, using: crypto)
        let expected = try checkpoint([0, 2])
        _ = try await store.beginSaving(expected, confirmedByUser: true)
        let restarted = try OpticalScanRecovery(directory: directory, using: crypto)
        let review = try await requireReview(restarted)
        let actual = try await restarted.load(review)
        XCTAssertEqual(try actual.encoded(using: crypto), try expected.encoded(using: crypto))
        for name in ["catalog.json", "journal.json"] {
            let attributes = try FileManager.default.attributesOfItem(atPath: directory.appendingPathComponent(name).path)
            XCTAssertEqual((attributes[.posixPermissions] as? NSNumber)?.intValue, 0o600)
        }
        try await restarted.discard(review)
        let again = try OpticalScanRecovery(directory: directory, using: crypto)
        let empty = try await again.review()
        XCTAssertNil(empty)
    }
}
