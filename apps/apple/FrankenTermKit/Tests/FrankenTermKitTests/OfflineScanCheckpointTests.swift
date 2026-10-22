import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class OfflineScanCheckpointTests: XCTestCase {
    private let crypto = TestCryptography()

    private func fixture(activity: WorkspaceNetworkActivity = WorkspaceNetworkActivity()) -> OfflineRestore<TestCryptography> {
        OfflineRestore(using: crypto, store: WorkspaceStore(persistence: StoreTestPersistence(), vault: StoreTestVault()),
                       networkActivity: activity)
    }
    /// Opaque transport data with a valid outer header, not a decrypted archive.
    private func sender(mode: FTAGMode = .code) throws -> OpticalSender {
        var bytes = Data("FTAG".utf8) + Data([1, mode.rawValue, 0, 0]) + Data(repeating: 19, count: 16)
        bytes.appendBE(4112, bytes: 4); bytes.appendBE(UInt64(mode.modeHeaderLength), bytes: 2)
        bytes += Data(repeating: 27, count: mode.modeHeaderLength)
        bytes += Data((0..<4112).map { UInt8(truncatingIfNeeded: $0 * 13) })
        return try OpticalSender(envelope: bytes, using: crypto)
    }
    private func checkpoint(_ sender: OpticalSender, completed: Bool = false) throws -> Data {
        let first = try OpticalFrame(text: sender.frame(page: 0, sequence: 1, using: crypto))
        var receiver = OpticalReceiver(selecting: first)
        let count = completed ? try sender.sourcePartCount(page: 0) : 4
        for sequence in 1...count {
            try receiver.receive(OpticalFrame(text: sender.frame(page: 0, sequence: UInt32(sequence), using: crypto)), using: crypto)
        }
        return try receiver.decoderCheckpoint(using: crypto)
    }

    private func encryptedTransfer() throws -> FTAG.Export {
        let (key, fingerprint) = try sampleKey(using: crypto)
        let document = try WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint))
        return try FTAG.seal(AirGapArchive(workspace: document, keys: [key], using: crypto), using: crypto)
    }

    func testPartialDecoderRecoveryReopensBeforeFirstPageAndAuthenticatesBeforeImport() async throws {
        let exported = try encryptedTransfer(), sender = try OpticalSender(envelope: exported.envelope, using: crypto)
        XCTAssertEqual(sender.identity.pageCount, 1)
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("ft-authenticated-scan-" + UUID().uuidString)
        let recovery = try OpticalDecoderRecovery(directory: directory, using: crypto)
        let disk = StoreTestPersistence(), vault = StoreTestVault(), activity = WorkspaceNetworkActivity()
        let store = WorkspaceStore(persistence: disk, vault: vault)
        let original = OfflineRestore(using: crypto, store: store, networkActivity: activity)
        var state = await original.status()
        state = try await original.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        state = try await original.selectCandidate(generation: state.generation, digest: sender.identity.envelopeDigest)
        for sequence in 2...4 {
            state = try await original.observe(sender.frame(page: 0, sequence: UInt32(sequence), using: crypto), generation: state.generation)
        }
        XCTAssertEqual(state.verifiedPages, 0); XCTAssertEqual(state.receivedParts, 4)
        let partial = try await original.exportDecoderCheckpoint(generation: state.generation)
        XCTAssertLessThanOrEqual(partial.count, OpticalReceiver.decoderCheckpointByteLimit)
        do { _ = try await recovery.beginSaving(partial, confirmedByUser: false); XCTFail("saved without approval") }
        catch { XCTAssertEqual(error as? OpticalDecoderRecoveryError, .consentRequired) }
        XCTAssertFalse(FileManager.default.fileExists(atPath: directory.appendingPathComponent("catalog.json").path))
        let first = try await recovery.beginSaving(partial, confirmedByUser: true)
        XCTAssertEqual(first.review.verifiedBytes, 0); XCTAssertEqual(first.review.independentParts, 4)
        let attributes = try FileManager.default.attributesOfItem(atPath: directory.appendingPathComponent("catalog.json").path)
        XCTAssertEqual((attributes[.posixPermissions] as? NSNumber)?.intValue, 0o600)
        try await recovery.stopSaving(first)
        _ = await original.suspend(discardRecipientRequest: true)
        XCTAssertEqual(activity.accounting().offlineRestores, 0)

        // Fresh file and restore owners represent a process restart. Loading
        // precedes the new saving decision, which rotates the stored witness.
        let reopened = try OpticalDecoderRecovery(directory: directory, using: crypto)
        let reviewed = try await reopened.review()
        let saved = try await reopened.load(XCTUnwrap(reviewed))
        let continued = try await reopened.continueSaving(XCTUnwrap(reviewed), confirmedByUser: true)
        do { _ = try await recovery.save(partial, after: first); XCTFail("retired writer saved after reopen") }
        catch { XCTAssertEqual(error as? OpticalDecoderRecoveryError, .staleWriter) }
        let resumed = OfflineRestore(using: crypto, store: store, networkActivity: activity)
        let idle = await resumed.status()
        state = try await resumed.acceptDecoderCheckpoint(saved, generation: idle.generation)
        XCTAssertNotEqual(state.generation, idle.generation)
        XCTAssertEqual(state.phase, .collecting); XCTAssertEqual(state.verifiedPages, 0)
        XCTAssertEqual(state.receivedParts, 4); XCTAssertEqual(state.identity, sender.identity)
        XCTAssertNil(state.review); XCTAssertNil(state.receipt)
        state = try await resumed.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        XCTAssertEqual(state.receivedParts, 4, "a duplicate after reopen is not recovered data")
        for sequence in 5...(try sender.sourcePartCount(page: 0)) {
            state = try await resumed.observe(sender.frame(page: 0, sequence: UInt32(sequence), using: crypto), generation: state.generation)
        }
        XCTAssertEqual(state.phase, .locked); XCTAssertEqual(state.encryptionMode, .code)
        XCTAssertEqual(state.verifiedBytes, exported.envelope.count)
        let complete = try await resumed.exportDecoderCheckpoint(generation: state.generation)
        XCTAssertLessThanOrEqual(complete.count, OpticalReceiver.decoderCheckpointByteLimit)
        let decoded = try OpticalReceiver(decoderCheckpoint: complete, using: crypto)
        XCTAssertTrue(decoded.isComplete); XCTAssertEqual(decoded.independentParts, 0)
        XCTAssertEqual(decoded.identity.envelopeLength, exported.envelope.count)
        XCTAssertEqual(try decoded.envelope(using: crypto), exported.envelope)
        XCTAssertEqual(try FTAGHeader(envelope: decoded.envelope(using: crypto)).mode, .code)
        let completedSave = try await reopened.save(complete, after: continued)
        XCTAssertEqual(completedSave.review.verifiedPages, 1)
        XCTAssertEqual(completedSave.review.receivedParts, sender.identity.sourcePartCount)
        XCTAssertEqual(vault.creates, 0); XCTAssertNil(disk.bytes(.catalog)); XCTAssertNil(disk.bytes(.journal))
        let wrong = try TransferCode(secret: Data(repeating: 71, count: 16)).reveal(using: crypto)
        do { _ = try await resumed.unlock(enteredCode: wrong, generation: state.generation); XCTFail("wrong code unlocked saved ciphertext") }
        catch { XCTAssertEqual(error as? FTError, .authentication) }
        state = try await resumed.unlock(enteredCode: exported.code.reveal(using: crypto), generation: state.generation)
        let previousApproval = try XCTUnwrap(state.review).token
        let reviewedBytes = try await resumed.exportDecoderCheckpoint(generation: state.generation)
        XCTAssertEqual(reviewedBytes, complete, "decryption must not add a key, archive or approval to saved progress")
        _ = await resumed.suspend(discardRecipientRequest: true)

        let finalRecovery = try OpticalDecoderRecovery(directory: directory, using: crypto)
        let finalReview = try await finalRecovery.review()
        let finalBytes = try await finalRecovery.load(XCTUnwrap(finalReview))
        let finalRestore = OfflineRestore(using: crypto, store: store, networkActivity: activity)
        state = await finalRestore.status()
        state = try await finalRestore.acceptDecoderCheckpoint(finalBytes, generation: state.generation)
        XCTAssertEqual(state.phase, .locked); XCTAssertNil(state.review); XCTAssertNil(state.receipt)
        do {
            _ = try await finalRestore.commit(reviewToken: previousApproval, generation: state.generation, sourceAndContentsConfirmed: true)
            XCTFail("saved progress restored earlier import authority")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        XCTAssertEqual(vault.creates, 0); XCTAssertNil(disk.bytes(.catalog))
        state = try await finalRestore.unlock(enteredCode: exported.code.reveal(using: crypto), generation: state.generation)
        XCTAssertNotEqual(state.review?.token, previousApproval)
        state = try await finalRestore.commit(reviewToken: XCTUnwrap(state.review).token, generation: state.generation,
                                              sourceAndContentsConfirmed: true)
        XCTAssertEqual(state.phase, .complete); XCTAssertEqual(vault.creates, 1)
        XCTAssertTrue(try XCTUnwrap(state.receipt).workspace.connectLater)
        XCTAssertEqual(activity.accounting().offlineRestores, 0)
        do { _ = try await finalRestore.exportDecoderCheckpoint(generation: state.generation); XCTFail("retained a completed import's scan") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
    }

    func testPageRepairPersistsLowerRankAndRejectsEarlierQueuedSnapshot() async throws {
        let exported = try encryptedTransfer(), sender = try OpticalSender(envelope: exported.envelope, using: crypto)
        let restore = fixture(), recovery = OpticalDecoderRecovery(persistence: StoreTestPersistence(), using: crypto)
        var state = await restore.status()
        state = try await restore.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        state = try await restore.selectCandidate(generation: state.generation, digest: sender.identity.envelopeDigest)
        for sequence in 2...4 {
            state = try await restore.observe(sender.frame(page: 0, sequence: UInt32(sequence), using: crypto), generation: state.generation)
        }
        let initialBytes = try await restore.exportDecoderCheckpoint(generation: state.generation)
        let first = try await recovery.beginSaving(initialBytes, confirmedByUser: true)
        for sequence in 5...8 {
            state = try await restore.observe(sender.frame(page: 0, sequence: UInt32(sequence), using: crypto), generation: state.generation)
        }
        let queued = try await restore.exportDecoderCheckpoint(generation: state.generation)
        let prior = try await recovery.save(queued, after: first)
        XCTAssertEqual(prior.review.independentParts, 8)
        let oldGeneration = state.generation
        state = try await restore.resetIncompletePage(0, generation: state.generation)
        XCTAssertNotEqual(state.generation, oldGeneration); XCTAssertEqual(state.receivedParts, 0)
        do { _ = try await restore.exportDecoderCheckpoint(generation: oldGeneration); XCTFail("exported a retired page repair") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        let reset = try await restore.exportDecoderCheckpoint(generation: state.generation)
        let repaired = try await recovery.save(reset, after: prior)
        XCTAssertEqual(repaired.review.independentParts, 0); XCTAssertEqual(repaired.review.verifiedPages, 0)
        do { _ = try await recovery.save(queued, after: prior); XCTFail("queued snapshot resurrected discarded equations") }
        catch { XCTAssertEqual(error as? OpticalDecoderRecoveryError, .staleWriter) }
        let saved = try await recovery.load(repaired.review)
        let restarted = fixture(), idle = await restarted.status()
        var reopened = try await restarted.acceptDecoderCheckpoint(saved, generation: idle.generation)
        XCTAssertEqual(reopened.receivedParts, 0); XCTAssertEqual(reopened.incompletePageIndices, [0])
        reopened = try await restarted.observe(sender.frame(page: 0, sequence: 9, using: crypto), generation: reopened.generation)
        XCTAssertEqual(reopened.receivedParts, 1)
        let fresh = try await restarted.exportDecoderCheckpoint(generation: reopened.generation)
        let savedFresh = try await recovery.save(fresh, after: repaired)
        XCTAssertEqual(savedFresh.review.independentParts, 1)
    }

    func testCompletedDecoderChecksExactEnvelopeExtentAndHeaderBeforeSourceAdoption() async throws {
        let exported = try encryptedTransfer(), sender = try OpticalSender(envelope: exported.envelope, using: crypto)
        let restore = fixture(), initial = await restore.status()
        // Construct checksum-consistent hostile cache files. Every contained
        // page hash and the FTDC checksum match, leaving the real envelope
        // extent/header/transfer checks as the required rejection boundary.
        func completeCheckpoint(_ envelope: Data, transferID: Data) -> Data {
            let digest = crypto.sha256(envelope)
            var body = Data("FTDC".utf8) + CBORWriter.array(5) + CBORWriter.uint(1)
                + CBORWriter.bytes(transferID) + CBORWriter.bytes(digest)
                + CBORWriter.uint(UInt64(envelope.count)) + CBORWriter.array(1)
            body += CBORWriter.array(4) + CBORWriter.uint(0) + CBORWriter.bytes(digest)
                + CBORWriter.uint(0) + CBORWriter.bytes(envelope)
            return body + crypto.sha256(Data("FrankenTerm/FTDC/v1/checksum\0".utf8) + body)
        }
        var badVersion = exported.envelope; badVersion[4] = 99
        var badModeHeader = exported.envelope; badModeHeader[29] = 27
        let invalid = [
            completeCheckpoint(exported.envelope + Data([0]), transferID: sender.identity.transferID),
            completeCheckpoint(badVersion, transferID: sender.identity.transferID),
            completeCheckpoint(badModeHeader, transferID: sender.identity.transferID),
            completeCheckpoint(exported.envelope, transferID: Data(repeating: 0, count: 16)),
            Data(count: OpticalReceiver.decoderCheckpointByteLimit + 1)
        ]
        for data in invalid {
            do { _ = try await restore.acceptDecoderCheckpoint(data, generation: initial.generation); XCTFail("accepted a malformed complete checkpoint") }
            catch { XCTAssertNotNil(error as? FTError) }
            let unchanged = await restore.status()
            XCTAssertEqual(unchanged.generation, initial.generation); XCTAssertEqual(unchanged.phase, .idle)
            XCTAssertNil(unchanged.source); XCTAssertNil(unchanged.identity); XCTAssertNil(unchanged.review)
        }
        let completed = try await restore.acceptDecoderCheckpoint(checkpoint(sender, completed: true), generation: initial.generation)
        XCTAssertEqual(completed.phase, .locked); XCTAssertEqual(completed.encryptionMode, .code)
    }

    func testExportFromOneActorResumesInAnotherWithoutDecrypting() async throws {
        let sender = try sender(), original = fixture()
        var state = await original.status()
        state = try await original.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        state = try await original.selectCandidate(generation: state.generation, digest: sender.identity.envelopeDigest)
        for sequence in 2...4 {
            state = try await original.observe(sender.frame(page: 0, sequence: UInt32(sequence), using: crypto), generation: state.generation)
        }
        let saved = try await original.exportDecoderCheckpoint(generation: state.generation)
        _ = await original.suspend(discardRecipientRequest: true)
        let resumed = fixture()
        let empty = await resumed.status()
        state = try await resumed.acceptDecoderCheckpoint(saved, generation: empty.generation)
        XCTAssertNotEqual(state.generation, empty.generation)
        XCTAssertEqual(state.phase, .collecting)
        XCTAssertEqual(state.source, .optical)
        XCTAssertEqual(state.identity, sender.identity)
        XCTAssertEqual(state.receivedParts, 4)
        XCTAssertNil(state.review); XCTAssertNil(state.receipt)
        for sequence in 5...(try sender.sourcePartCount(page: 0)) {
            state = try await resumed.observe(sender.frame(page: 0, sequence: UInt32(sequence), using: crypto), generation: state.generation)
        }
        XCTAssertEqual(state.phase, .locked)
        XCTAssertEqual(state.encryptionMode, .code)
        XCTAssertEqual(state.verifiedBytes, sender.identity.envelopeLength)
        XCTAssertNil(state.review); XCTAssertNil(state.receipt); XCTAssertNil(state.arrival)
    }
    func testResumeRetiresPreImportCameraAndExportTokens() async throws {
        let sender = try sender(), restore = fixture()
        let before = await restore.status()
        let after = try await restore.acceptDecoderCheckpoint(checkpoint(sender), generation: before.generation)
        do {
            _ = try await restore.observe(sender.frame(page: 0, sequence: 5, using: crypto), generation: before.generation)
            XCTFail("old callback reached a newly selected scan")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        do {
            _ = try await restore.exportDecoderCheckpoint(generation: before.generation)
            XCTFail("old export token accepted")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        let current = await restore.status()
        XCTAssertEqual(current.generation, after.generation)
        XCTAssertEqual(current.receivedParts, 4)
    }
    func testBadCheckpointDoesNotChangeIdleGenerationOrSelectASource() async throws {
        let sender = try sender(), restore = fixture()
        let before = await restore.status()
        do {
            _ = try await restore.acceptDecoderCheckpoint(Data(checkpoint(sender).dropLast()), generation: before.generation)
            XCTFail("truncated checkpoint accepted")
        } catch {}
        let current = await restore.status()
        XCTAssertEqual(current.generation, before.generation)
        XCTAssertEqual(current.phase, .idle)
        XCTAssertNil(current.identity); XCTAssertNil(current.review)
        let valid = try await restore.acceptDecoderCheckpoint(checkpoint(sender), generation: before.generation)
        XCTAssertEqual(valid.receivedParts, 4)
    }
    func testSelectedScanCannotBeReplacedByCheckpointFile() async throws {
        let sender = try sender(), restore = fixture()
        let empty = await restore.status()
        let collecting = try await restore.acceptDecoderCheckpoint(checkpoint(sender), generation: empty.generation)
        do {
            _ = try await restore.acceptDecoderCheckpoint(checkpoint(sender, completed: true), generation: collecting.generation)
            XCTFail("file replaced an active scan")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .busy) }
        let state = await restore.status()
        XCTAssertEqual(state.phase, .collecting)
        XCTAssertEqual(state.receivedParts, 4)
    }
    func testExportRequiresActiveSelectedSourceAndDoesNotReviveSuspension() async throws {
        let sender = try sender(), restore = fixture()
        let empty = await restore.status()
        do { _ = try await restore.exportDecoderCheckpoint(generation: empty.generation); XCTFail("exported idle") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        let collecting = try await restore.acceptDecoderCheckpoint(checkpoint(sender), generation: empty.generation)
        let suspended = await restore.suspend()
        do { _ = try await restore.exportDecoderCheckpoint(generation: suspended.generation); XCTFail("exported suspended") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        let resumed = await restore.resume()
        XCTAssertNotEqual(resumed.generation, collecting.generation)
        let bytes = try await restore.exportDecoderCheckpoint(generation: resumed.generation)
        XCTAssertEqual(try OpticalReceiver(decoderCheckpoint: bytes, using: crypto).receivedParts, 4)
    }
    func testCompletedCheckpointIsLockedNotApprovedForInstallation() async throws {
        let sender = try sender(), restore = fixture()
        let empty = await restore.status()
        let locked = try await restore.acceptDecoderCheckpoint(checkpoint(sender, completed: true), generation: empty.generation)
        XCTAssertEqual(locked.phase, .locked)
        XCTAssertNil(locked.review); XCTAssertNil(locked.receipt)
        do {
            _ = try await restore.commit(reviewToken: UUID(), generation: locked.generation, sourceAndContentsConfirmed: true)
            XCTFail("checkpoint granted installation authority")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        let exported = try await restore.exportDecoderCheckpoint(generation: locked.generation)
        let receiver = try OpticalReceiver(decoderCheckpoint: exported, using: crypto)
        XCTAssertTrue(receiver.isComplete); XCTAssertEqual(receiver.identity, sender.identity)
        XCTAssertEqual(exported, try checkpoint(sender, completed: true))
    }
    func testRecipientCheckpointDoesNotRestoreASecretRequestKey() async throws {
        let sender = try sender(mode: .recipient), restore = fixture()
        let empty = await restore.status()
        let locked = try await restore.acceptDecoderCheckpoint(checkpoint(sender, completed: true), generation: empty.generation)
        XCTAssertEqual(locked.phase, .locked)
        XCTAssertEqual(locked.encryptionMode, .recipient)
        XCTAssertFalse(locked.canUnlockRecipient)
        XCTAssertNil(locked.recipientRequest)
        XCTAssertNil(locked.review); XCTAssertNil(locked.receipt)
    }
    func testExistingConnectionExcludesResumeAndRestoredScanExcludesNetworking() async throws {
        let sender = try sender(), activity = WorkspaceNetworkActivity(), restore = fixture(activity: activity)
        let before = await restore.status()
        var lease: WorkspaceNetworkLease? = try activity.beginConnection()
        do { _ = try await restore.acceptDecoderCheckpoint(checkpoint(sender), generation: before.generation); XCTFail("resumed online") }
        catch { XCTAssertEqual(error as? WorkspaceNetworkError, .connectionsActive) }
        let rejected = await restore.status()
        XCTAssertEqual(rejected.phase, .idle); XCTAssertEqual(rejected.generation, before.generation)
        withExtendedLifetime(lease) {}; lease = nil
        _ = try await restore.acceptDecoderCheckpoint(checkpoint(sender), generation: before.generation)
        XCTAssertEqual(activity.accounting().offlineRestores, 1)
        XCTAssertThrowsError(try activity.beginConnection()) {
            XCTAssertEqual($0 as? WorkspaceNetworkError, .offlineRestoreActive)
        }
        _ = await restore.suspend(discardRecipientRequest: true)
        XCTAssertEqual(activity.accounting().offlineRestores, 0)
    }
    func testCancelledResumePublishesNothing() async throws {
        let sender = try sender(), restore = fixture()
        let before = await restore.status(), bytes = try checkpoint(sender)
        let task = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await restore.acceptDecoderCheckpoint(bytes, generation: before.generation)
        }
        do { _ = try await task.value; XCTFail("cancelled resume published state") }
        catch { XCTAssertTrue(error is CancellationError) }
        let after = await restore.status()
        XCTAssertEqual(after.phase, .idle); XCTAssertEqual(after.generation, before.generation)
    }
}
