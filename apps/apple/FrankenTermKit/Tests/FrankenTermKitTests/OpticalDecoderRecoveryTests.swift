import Foundation
import XCTest
@testable import FrankenTermKit

private final class DecoderRecoveryDisk: WorkspacePersistence, @unchecked Sendable {
    enum Fault { case before, after }
    private let lock = NSRecursiveLock()
    private var slots: [String: Data] = [:]
    private var fault: Fault?
    private var cancelAfterPublication = false
    private var writeCount = 0, accessCount = 0
    var writes: Int { lock.withLock { writeCount } }
    var accesses: Int { lock.withLock { accessCount } }
    func arm(_ fault: Fault) { lock.withLock { self.fault = fault } }
    func cancelOnPublication() { lock.withLock { cancelAfterPublication = true } }
    func bytes(_ slot: WorkspaceStorageSlot) -> Data? { lock.withLock { slots[slot.rawValue] } }
    func replace(_ slot: WorkspaceStorageSlot, with bytes: Data?) { lock.withLock { slots[slot.rawValue] = bytes } }
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T {
        try lock.withLock { accessCount += 1; return try body() }
    }
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? {
        try lock.withLock {
            guard let data = slots[slot.rawValue] else { return nil }
            guard data.count <= limit else { throw FTError.limit("test.read") }
            return data
        }
    }
    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws {
        try lock.withLock {
            writeCount += 1
            let failure = slot == .catalog ? fault : nil
            if slot == .catalog { fault = nil }
            if failure == .before { throw FTError.storage }
            slots[slot.rawValue] = data
            if slot == .catalog, cancelAfterPublication {
                cancelAfterPublication = false
                withUnsafeCurrentTask { $0?.cancel() }
            }
            if failure == .after { throw FTError.storage }
        }
    }
    func removeJournal() throws { XCTFail("decoder recovery must retain its namespace"); throw FTError.storage }
}

@MainActor
final class OpticalDecoderRecoveryTests: XCTestCase {
    private let crypto = TestCryptography()
    private typealias Store = OpticalDecoderRecovery<TestCryptography>

    /// Structurally valid but opaque FTAG payload; no encryption/import claim.
    private func sender(tag: UInt8 = 14) throws -> OpticalSender {
        var bytes = Data("FTAG".utf8) + Data([1, 1, 0, 0]) + Data(repeating: tag, count: 16)
        bytes.appendBE(4112, bytes: 4); bytes.appendBE(28, bytes: 2)
        bytes += Data(repeating: 27, count: 28)
        bytes += Data((0..<4112).map { UInt8(truncatingIfNeeded: $0 * 13) })
        return try OpticalSender(envelope: bytes, using: crypto)
    }
    private func checkpoint(parts: Int = 4, tag: UInt8 = 14) throws -> Data {
        let sender = try sender(tag: tag)
        var receiver = OpticalReceiver(selecting: try OpticalFrame(text: sender.frame(page: 0, sequence: 1, using: crypto)))
        for sequence in 1...parts {
            try receiver.receive(OpticalFrame(text: sender.frame(page: 0, sequence: UInt32(sequence), using: crypto)), using: crypto)
        }
        return try receiver.decoderCheckpoint(using: crypto)
    }
    private func expect(_ expected: OpticalDecoderRecoveryError, _ operation: () async throws -> Void,
                        file: StaticString = #filePath, line: UInt = #line) async {
        do { try await operation(); XCTFail("operation unexpectedly succeeded", file: file, line: line) }
        catch { XCTAssertEqual(error as? OpticalDecoderRecoveryError, expected, file: file, line: line) }
    }
    private func expectStorage(_ operation: () async throws -> Void,
                               file: StaticString = #filePath, line: UInt = #line) async {
        do { try await operation(); XCTFail("operation unexpectedly succeeded", file: file, line: line) }
        catch { XCTAssertEqual(error as? FTError, .storage, file: file, line: line) }
    }

    func testUnusedReviewAndRefusedConsentNeverWrite() async throws {
        let disk = DecoderRecoveryDisk(), store = Store(persistence: disk, using: crypto)
        let review = try await store.review()
        XCTAssertNil(review); XCTAssertEqual(disk.writes, 0)
        let accesses = disk.accesses
        await expect(.consentRequired) { _ = try await store.beginSaving(self.checkpoint(), confirmedByUser: false) }
        XCTAssertEqual(disk.accesses, accesses); XCTAssertEqual(disk.writes, 0)
    }

    func testPartialProgressIsDistinctFromVerifiedDataAndReopensExactly() async throws {
        let disk = DecoderRecoveryDisk(), store = Store(persistence: disk, using: crypto)
        let bytes = try checkpoint(parts: 8)
        let session = try await store.beginSaving(bytes, confirmedByUser: true)
        XCTAssertEqual(session.review.verifiedPages, 0); XCTAssertEqual(session.review.verifiedBytes, 0)
        XCTAssertEqual(session.review.independentParts, 8); XCTAssertEqual(session.review.receivedParts, 8)
        let reopened = Store(persistence: disk, using: crypto)
        let review = try await reopened.review()
        let loaded = try await reopened.load(XCTUnwrap(review))
        XCTAssertEqual(loaded, bytes)
        XCTAssertEqual(disk.writes, 2)
    }

    func testFreshScanCannotReplaceSavedProgressEvenForSameTransfer() async throws {
        let disk = DecoderRecoveryDisk(), store = Store(persistence: disk, using: crypto)
        let session = try await store.beginSaving(checkpoint(), confirmedByUser: true), writes = disk.writes
        await expect(.savedProgressExists) { _ = try await store.beginSaving(self.checkpoint(parts: 2), confirmedByUser: true) }
        await expect(.savedProgressExists) { _ = try await store.beginSaving(self.checkpoint(tag: 15), confirmedByUser: true) }
        let saved = try await store.load(session.review)
        XCTAssertEqual(saved, try checkpoint()); XCTAssertEqual(disk.writes, writes)
    }

    func testCompareAndSwapRejectsStaleSaveButAllowsDeliberatePageRepair() async throws {
        let disk = DecoderRecoveryDisk(), store = Store(persistence: disk, using: crypto)
        let first = try await store.beginSaving(checkpoint(), confirmedByUser: true)
        let next = try await store.save(checkpoint(parts: 8), after: first)
        await expect(.staleWriter) { _ = try await store.save(self.checkpoint(parts: 6), after: first) }
        await expect(.staleReview) { _ = try await store.load(first.review) }
        let repaired = try await store.save(checkpoint(parts: 2), after: next)
        XCTAssertEqual(repaired.review.receivedParts, 2)
        await expect(.staleWriter) { _ = try await store.save(self.checkpoint(parts: 8), after: first) }
        let saved = try await store.load(repaired.review)
        XCTAssertEqual(saved, try checkpoint(parts: 2))
    }

    func testAnotherTransferCannotUseTheSaveSession() async throws {
        let disk = DecoderRecoveryDisk(), store = Store(persistence: disk, using: crypto)
        let first = try await store.beginSaving(checkpoint(), confirmedByUser: true), writes = disk.writes
        await expect(.differentTransfer) { _ = try await store.save(self.checkpoint(tag: 99), after: first) }
        XCTAssertEqual(disk.writes, writes)
    }

    func testResumeConsentRotatesWriterAcrossActorInstances() async throws {
        let disk = DecoderRecoveryDisk(), original = Store(persistence: disk, using: crypto)
        let first = try await original.beginSaving(checkpoint(), confirmedByUser: true)
        let reopened = Store(persistence: disk, using: crypto)
        await expect(.consentRequired) { _ = try await reopened.continueSaving(first.review, confirmedByUser: false) }
        let resumed = try await reopened.continueSaving(first.review, confirmedByUser: true)
        XCTAssertNotEqual(resumed.review.id, first.review.id)
        await expect(.staleWriter) { _ = try await original.save(self.checkpoint(parts: 5), after: first) }
        await expect(.staleReview) { _ = try await original.continueSaving(first.review, confirmedByUser: true) }
        let next = try await reopened.save(checkpoint(parts: 9), after: resumed)
        XCTAssertEqual(next.review.receivedParts, 9)
    }

    func testStopRevokesTheEpochWithoutLosingNewestSavedData() async throws {
        let disk = DecoderRecoveryDisk(), store = Store(persistence: disk, using: crypto)
        let first = try await store.beginSaving(checkpoint(), confirmedByUser: true)
        let second = try await store.save(checkpoint(parts: 8), after: first)
        try await store.stopSaving(first)
        await expect(.staleWriter) { _ = try await store.save(self.checkpoint(parts: 10), after: second) }
        let review = try await store.review()
        XCTAssertEqual(review?.receivedParts, 8)
        XCTAssertNotEqual(review?.id, second.review.id)
    }

    func testDiscardFencesAllOldWritersAndOldReviewsCannotDiscardANewScan() async throws {
        let disk = DecoderRecoveryDisk(), store = Store(persistence: disk, using: crypto)
        let first = try await store.beginSaving(checkpoint(), confirmedByUser: true)
        await expect(.consentRequired) { try await store.discard(first.review, confirmedByUser: false) }
        try await store.discard(first.review, confirmedByUser: true)
        let empty = try await store.review()
        XCTAssertNil(empty); XCTAssertEqual(disk.bytes(.catalog)?.count, 92)
        await expect(.staleWriter) { _ = try await store.save(self.checkpoint(parts: 8), after: first) }
        let second = try await store.beginSaving(checkpoint(tag: 15), confirmedByUser: true)
        await expect(.staleReview) { try await store.discard(first.review, confirmedByUser: true) }
        let saved = try await store.load(second.review)
        XCTAssertEqual(saved, try checkpoint(tag: 15))
    }

    func testFirstPublicationFailureBeforeAndAfterRenameIsRecoverable() async throws {
        for fault in [DecoderRecoveryDisk.Fault.before, .after] {
            let disk = DecoderRecoveryDisk(), store = Store(persistence: disk, using: crypto)
            disk.arm(fault)
            await expectStorage { _ = try await store.beginSaving(self.checkpoint(), confirmedByUser: true) }
            let review = try await store.review()
            if let review {
                let fresh = try await store.continueSaving(review, confirmedByUser: true)
                XCTAssertEqual(fresh.review.receivedParts, 4)
            } else {
                let fresh = try await store.beginSaving(checkpoint(), confirmedByUser: true)
                XCTAssertEqual(fresh.review.receivedParts, 4)
            }
        }
    }

    func testFailedSaveReaffirmsExactlyThePublishedSuccessor() async throws {
        for fault in [DecoderRecoveryDisk.Fault.before, .after] {
            let disk = DecoderRecoveryDisk(), store = Store(persistence: disk, using: crypto)
            let first = try await store.beginSaving(checkpoint(), confirmedByUser: true)
            disk.arm(fault)
            await expectStorage { _ = try await store.save(self.checkpoint(parts: 8), after: first) }
            let writes = disk.writes
            let retry = try await store.save(checkpoint(parts: 8), after: first)
            XCTAssertEqual(disk.writes, writes + 1, "readback alone cannot certify a failed fsync")
            XCTAssertEqual(retry.review.receivedParts, 8)
            _ = try await store.save(checkpoint(parts: 9), after: retry)
            await expect(.staleWriter) { _ = try await store.save(self.checkpoint(parts: 8), after: first) }
        }
    }

    func testAmbiguousDiscardCanBeReaffirmedWithoutResurrectingBytes() async throws {
        let disk = DecoderRecoveryDisk(), store = Store(persistence: disk, using: crypto)
        let first = try await store.beginSaving(checkpoint(), confirmedByUser: true)
        disk.arm(.after)
        await expectStorage { try await store.discard(first.review, confirmedByUser: true) }
        let writes = disk.writes
        try await store.discard(first.review, confirmedByUser: true)
        XCTAssertEqual(disk.writes, writes + 1)
        let review = try await store.review()
        XCTAssertNil(review)
        await expect(.staleWriter) { _ = try await store.save(self.checkpoint(parts: 8), after: first) }
    }

    func testTwoOwnersRacingFromOnePredecessorCannotBothPublish() async throws {
        let disk = DecoderRecoveryDisk(), a = Store(persistence: disk, using: crypto), b = Store(persistence: disk, using: crypto)
        let first = try await a.beginSaving(checkpoint(), confirmedByUser: true)
        let left = try checkpoint(parts: 6), right = try checkpoint(parts: 8)
        async let l = Self.result { try await a.save(left, after: first) }
        async let r = Self.result { try await b.save(right, after: first) }
        let results = await [l, r]
        XCTAssertEqual(results.filter { if case .success = $0 { true } else { false } }.count, 1)
        for value in results { if case let .failure(error) = value { XCTAssertEqual(error as? OpticalDecoderRecoveryError, .staleWriter) } }
        let review = try await a.review()
        let payload = try await a.load(XCTUnwrap(review))
        XCTAssertTrue(payload == left || payload == right)
    }
    nonisolated private static func result(_ operation: @Sendable () async throws -> OpticalDecoderSaveSession) async -> Result<OpticalDecoderSaveSession, any Error> {
        do { return .success(try await operation()) } catch { return .failure(error) }
    }

    func testCorruptionAndForeignNamespaceFailWithoutOverwriting() async throws {
        let disk = DecoderRecoveryDisk(), store = Store(persistence: disk, using: crypto)
        _ = try await store.beginSaving(checkpoint(), confirmedByUser: true)
        let catalog = try XCTUnwrap(disk.bytes(.catalog)), marker = try XCTUnwrap(disk.bytes(.journal))
        for position in [0, 4, 6, 16, 30, 59, 90, catalog.count - 1] {
            var corrupt = catalog; corrupt[position] ^= 1; disk.replace(.catalog, with: corrupt)
            let writes = disk.writes
            await expect(.corruptStore) { _ = try await store.review() }
            XCTAssertEqual(disk.writes, writes)
        }
        disk.replace(.catalog, with: catalog); disk.replace(.journal, with: nil)
        await expect(.wrongNamespace) { _ = try await store.review() }
        disk.replace(.journal, with: Data("foreign".utf8))
        await expect(.wrongNamespace) { _ = try await store.review() }
        disk.replace(.journal, with: marker)
        let review = try await store.review()
        XCTAssertEqual(review?.receivedParts, 4)
    }

    func testMalformedCheckpointIsRejectedBeforeStorageAccess() async throws {
        let disk = DecoderRecoveryDisk(), store = Store(persistence: disk, using: crypto)
        for payload in [Data(), Data("FTCODE1:NEVER-SAVE-THIS".utf8), Data(count: OpticalReceiver.decoderCheckpointByteLimit + 1)] {
            do { _ = try await store.beginSaving(payload, confirmedByUser: true); XCTFail("bad checkpoint saved") }
            catch { XCTAssertTrue(error is FTError) }
        }
        XCTAssertEqual(disk.accesses, 0); XCTAssertEqual(disk.writes, 0)
    }

    func testRehashedRecordStillRequiresExactFramingAndAValidDecoder() async throws {
        let disk = DecoderRecoveryDisk(), store = Store(persistence: disk, using: crypto)
        _ = try await store.beginSaving(checkpoint(), confirmedByUser: true)
        let original = try XCTUnwrap(disk.bytes(.catalog)), writes = disk.writes
        for position in [0, 5, 7, 56, 59, 60, original.count - 33] {
            var body = Data(original.dropLast(32)); body[position] ^= 1
            let altered = body + crypto.sha256(Data("FrankenTerm/FTDR/v1/recovery\0".utf8) + body)
            disk.replace(.catalog, with: altered)
            do { _ = try await store.review(); XCTFail("accepted rehashed malformed record at \(position)") }
            catch { XCTAssertTrue(error is OpticalDecoderRecoveryError || error is FTError) }
        }
        XCTAssertEqual(disk.writes, writes)
    }

    func testStorageReadCapAndMissingMarkerFailClosed() async throws {
        let disk = DecoderRecoveryDisk(), store = Store(persistence: disk, using: crypto)
        _ = try await store.beginSaving(checkpoint(), confirmedByUser: true)
        let writes = disk.writes
        disk.replace(.catalog, with: Data(count: Store.maximumBytes + 1))
        do { _ = try await store.review(); XCTFail("read oversized recovery record") }
        catch { XCTAssertEqual(error as? FTError, .limit("test.read")) }
        disk.replace(.catalog, with: Data("foreign workspace catalog".utf8))
        disk.replace(.journal, with: nil)
        await expect(.wrongNamespace) { _ = try await store.beginSaving(self.checkpoint(), confirmedByUser: true) }
        XCTAssertEqual(disk.writes, writes)
    }

    func testCancellationBeforeAdmissionDoesNotWriteButPublishedWorkReturnsItsSession() async throws {
        let disk = DecoderRecoveryDisk(), store = Store(persistence: disk, using: crypto), bytes = try checkpoint()
        let cancelled = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await store.beginSaving(bytes, confirmedByUser: true)
        }
        do { _ = try await cancelled.value; XCTFail("cancelled admission") } catch { XCTAssertTrue(error is CancellationError) }
        XCTAssertEqual(disk.writes, 0)
        disk.cancelOnPublication()
        let publishing = Task { try await store.beginSaving(bytes, confirmedByUser: true) }
        let session = try await publishing.value
        let saved = try await store.load(session.review)
        XCTAssertEqual(saved, bytes)
    }

    func testRealFileRestartRestoresPartialProgressThroughTheProductionActor() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("ft-partial-recovery-" + UUID().uuidString)
        let store = try Store(directory: directory, using: crypto)
        let first = try await store.beginSaving(checkpoint(parts: 8), confirmedByUser: true)
        let attributes = try FileManager.default.attributesOfItem(atPath: directory.appendingPathComponent("catalog.json").path)
        XCTAssertEqual((attributes[.posixPermissions] as? NSNumber)?.intValue, 0o600)
        let reopened = try Store(directory: directory, using: crypto)
        let reviewed = try await reopened.review()
        XCTAssertEqual(reviewed?.receivedParts, first.review.receivedParts)
        let bytes = try await reopened.load(XCTUnwrap(reviewed))
        let activity = WorkspaceNetworkActivity()
        let restore = OfflineRestore(using: crypto, store: WorkspaceStore(persistence: StoreTestPersistence(), vault: StoreTestVault()),
                                     networkActivity: activity)
        let initial = await restore.status()
        var state = try await restore.acceptDecoderCheckpoint(bytes, generation: initial.generation)
        XCTAssertEqual(state.receivedParts, 8); XCTAssertEqual(state.phase, .collecting)
        XCTAssertEqual(activity.accounting().offlineRestores, 1)
        let sender = try sender()
        for sequence in 9...(try sender.sourcePartCount(page: 0)) {
            state = try await restore.observe(sender.frame(page: 0, sequence: UInt32(sequence), using: crypto), generation: state.generation)
        }
        XCTAssertEqual(state.phase, .locked); XCTAssertNil(state.review); XCTAssertNil(state.receipt)
        _ = await restore.suspend(discardRecipientRequest: true)
        XCTAssertEqual(activity.accounting().offlineRestores, 0)
    }
}
