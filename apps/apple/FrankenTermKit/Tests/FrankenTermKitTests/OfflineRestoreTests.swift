import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class OfflineRestoreTests: XCTestCase {
    let crypto = TestCryptography()
    func export(keys: Bool = true, bundleID: String? = nil) throws -> FTAG.Export {
        let (key, fingerprint) = try sampleKey(using: crypto)
        var document = try WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint))
        if let bundleID { document = try document.reissuingBundle(id: bundleID) }
        return try FTAG.seal(AirGapArchive(workspace: document, keys: keys ? [key] : [], using: crypto), using: crypto)
    }
    func environment() -> (OfflineRestore<TestCryptography>, WorkspaceStore, StoreTestPersistence, StoreTestVault) {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        return (OfflineRestore(using: crypto, store: store), store, persistence, vault)
    }

    private func pagedScan() throws -> (envelope: Data, code: TransferCode, sender: OpticalSender) {
        let (key, fingerprint) = try sampleKey(using: crypto)
        let findings = (0..<10).map {
            AirGapArchive.Portability(entityID: "route", setting: "saved_scan_\($0)", disposition: 0,
                                       reason: String(repeating: "x", count: 4000))
        }
        let archive = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint)),
                                       keys: [key], portability: findings, using: crypto)
        let code = try TransferCode(secret: Data(repeating: 27, count: 16))
        let envelope = try FTAG.sealArchiveBytes(archive.encoded(), code: code,
            transferID: Data(repeating: 28, count: 16), salt: Data(repeating: 29, count: 16),
            nonce: Data(repeating: 30, count: 12), compress: false, using: crypto)
        let sender = try OpticalSender(envelope: envelope, using: crypto)
        XCTAssertEqual(sender.identity.pageCount, 2)
        return (envelope, code, sender)
    }

    private func completedScan(_ envelope: Data) throws -> OpticalScanCheckpoint {
        let sender = try OpticalSender(envelope: envelope, using: crypto)
        var receiver = OpticalReceiver(selecting: try OpticalFrame(text: sender.frame(page: 0, sequence: 1, using: crypto)))
        for page in 0..<sender.identity.pageCount {
            for sequence in 1...(try sender.sourcePartCount(page: page)) {
                try receiver.receive(OpticalFrame(text: sender.frame(page: page, sequence: UInt32(sequence), using: crypto)), using: crypto)
            }
        }
        return try XCTUnwrap(receiver.checkpoint(using: crypto))
    }

    func testSavedScanResumesVerifiedPagesAndUsesNormalAuthenticationAndImport() async throws {
        let (restore, store, persistence, vault) = environment(), exported = try pagedScan()
        let sender = exported.sender
        var state = await restore.status()
        state = try await restore.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        let candidateCheckpoint = try await restore.scanCheckpoint(generation: state.generation)
        XCTAssertNil(candidateCheckpoint)
        state = try await restore.selectCandidate(generation: state.generation, digest: sender.identity.envelopeDigest)
        let partialCheckpoint = try await restore.scanCheckpoint(generation: state.generation)
        XCTAssertNil(partialCheckpoint)
        for sequence in 1...(try sender.sourcePartCount(page: 1)) {
            state = try await restore.observe(sender.frame(page: 1, sequence: UInt32(sequence), using: crypto), generation: state.generation)
        }
        XCTAssertEqual(state.verifiedPages, 1); XCTAssertEqual(state.independentParts, 1)
        let captured = try await restore.scanCheckpoint(generation: state.generation)
        let checkpoint = try XCTUnwrap(captured)
        let savedDisk = StoreTestPersistence()
        let recovery = OpticalScanRecovery(persistence: savedDisk, using: crypto)
        do { _ = try await recovery.beginSaving(checkpoint, confirmedByUser: false); XCTFail("saved without consent") }
        catch { XCTAssertEqual(error as? OpticalScanRecoveryError, .consentRequired) }
        _ = try await recovery.beginSaving(checkpoint, confirmedByUser: true)
        _ = await restore.suspend(discardRecipientRequest: true)

        // A fresh recovery owner and restore actor stand in for process restart.
        let reopened = OpticalScanRecovery(persistence: savedDisk, using: crypto)
        let savedReview = try await reopened.review()
        let saved = try await reopened.load(XCTUnwrap(savedReview))
        let restarted = OfflineRestore(using: crypto, store: store)
        let initial = await restarted.status()
        state = try await restarted.resumeScan(saved, generation: initial.generation)
        XCTAssertNotEqual(state.generation, initial.generation)
        XCTAssertEqual(state.source, .optical); XCTAssertEqual(state.identity, sender.identity)
        XCTAssertEqual(state.phase, .collecting); XCTAssertEqual(state.verifiedPages, 1)
        XCTAssertEqual(state.independentParts, 0); XCTAssertEqual(state.incompletePageIndices, [0])
        XCTAssertEqual(state.receivedParts, try sender.sourcePartCount(page: 1))
        do {
            _ = try await restarted.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: initial.generation)
            XCTFail("a pre-adoption camera callback entered the resumed scan")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        for sequence in 1...(try sender.sourcePartCount(page: 0)) {
            state = try await restarted.observe(sender.frame(page: 0, sequence: UInt32(sequence), using: crypto), generation: state.generation)
        }
        XCTAssertEqual(state.phase, .locked); XCTAssertEqual(state.verifiedBytes, exported.envelope.count)
        XCTAssertNil(state.review); XCTAssertNil(state.receipt)
        XCTAssertEqual(vault.creates, 0); XCTAssertNil(persistence.bytes(.catalog))
        let wrong = try TransferCode(secret: Data(repeating: 31, count: 16)).reveal(using: crypto)
        do { _ = try await restarted.unlock(enteredCode: wrong, generation: state.generation); XCTFail("wrong code accepted") }
        catch { XCTAssertEqual(error as? FTError, .authentication) }
        XCTAssertEqual(vault.creates, 0); XCTAssertNil(persistence.bytes(.catalog))
        state = try await restarted.unlock(enteredCode: exported.code.reveal(using: crypto), generation: state.generation)
        XCTAssertEqual(state.phase, .review); XCTAssertEqual(state.review?.findings.count, 10)
        state = try await restarted.commit(reviewToken: XCTUnwrap(state.review).token, generation: state.generation,
                                          sourceAndContentsConfirmed: true)
        XCTAssertEqual(state.phase, .complete); XCTAssertEqual(vault.creates, 1)
        XCTAssertTrue(try XCTUnwrap(state.receipt).workspace.connectLater)
    }

    func testCompleteOnePageAndReviewedScanSnapshotsNeverRestoreApproval() async throws {
        let (restore, store, persistence, vault) = environment(), exported = try export()
        let complete = try completedScan(exported.envelope)
        XCTAssertEqual(complete.identity.pageCount, 1)
        var state = await restore.status()
        state = try await restore.resumeScan(complete, generation: state.generation)
        XCTAssertEqual(state.phase, .locked); XCTAssertEqual(state.encryptionMode, .code)
        let lockedSnapshot = try await restore.scanCheckpoint(generation: state.generation)
        let locked = try XCTUnwrap(lockedSnapshot)
        XCTAssertEqual(locked.verifiedPages, 1); XCTAssertTrue(locked.incompletePageIndices.isEmpty)
        XCTAssertEqual(try OpticalReceiver(resuming: locked, using: crypto).envelope(using: crypto), exported.envelope)
        state = try await restore.unlock(enteredCode: exported.code.reveal(using: crypto), generation: state.generation)
        let previousApproval = try XCTUnwrap(state.review).token
        let reviewedSnapshot = try await restore.scanCheckpoint(generation: state.generation)
        let reviewed = try XCTUnwrap(reviewedSnapshot)
        XCTAssertEqual(try reviewed.encoded(using: crypto), try locked.encoded(using: crypto),
                       "decrypting and reviewing must not add a secret or approval to the cache")
        _ = await restore.suspend(discardRecipientRequest: true)
        let reopened = OfflineRestore(using: crypto, store: store)
        state = await reopened.status()
        state = try await reopened.resumeScan(reviewed, generation: state.generation)
        XCTAssertEqual(state.phase, .locked); XCTAssertNil(state.review); XCTAssertNil(state.receipt)
        do {
            _ = try await reopened.commit(reviewToken: previousApproval, generation: state.generation, sourceAndContentsConfirmed: true)
            XCTFail("a saved checkpoint restored an earlier import approval")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        XCTAssertEqual(vault.creates, 0); XCTAssertNil(persistence.bytes(.catalog))
        state = try await reopened.unlock(enteredCode: exported.code.reveal(using: crypto), generation: state.generation)
        XCTAssertNotEqual(state.review?.token, previousApproval)
    }

    func testSavedScanAdoptionRequiresIdleForegroundGenerationAndOfflineExclusion() async throws {
        let network = WorkspaceNetworkActivity(), disk = StoreTestPersistence(), vault = StoreTestVault()
        let restore = OfflineRestore(using: crypto, store: WorkspaceStore(persistence: disk, vault: vault), networkActivity: network)
        let exported = try export(), checkpoint = try completedScan(exported.envelope)
        var connection: WorkspaceNetworkLease? = try network.beginConnection()
        let initial = await restore.status()
        do { _ = try await restore.resumeScan(checkpoint, generation: initial.generation); XCTFail("adopted beside a live connection") }
        catch { XCTAssertEqual(error as? WorkspaceNetworkError, .connectionsActive) }
        XCTAssertEqual(network.accounting().connections, 1); XCTAssertEqual(network.accounting().offlineRestores, 0)
        var state = await restore.status()
        XCTAssertEqual(state.phase, .idle); XCTAssertNil(state.identity)
        withExtendedLifetime(connection) {}; connection = nil
        state = await restore.suspend(discardRecipientRequest: true)
        do { _ = try await restore.resumeScan(checkpoint, generation: state.generation); XCTFail("adopted while suspended") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        XCTAssertEqual(network.accounting().offlineRestores, 0)
        state = await restore.resume()
        do { _ = try await restore.resumeScan(checkpoint, generation: initial.generation); XCTFail("adopted with retired generation") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        let sender = try OpticalSender(envelope: exported.envelope, using: crypto)
        state = try await restore.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        do { _ = try await restore.resumeScan(checkpoint, generation: state.generation); XCTFail("replaced an active candidate") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .busy) }
        let unchanged = await restore.status()
        XCTAssertEqual(unchanged.phase, .candidate); XCTAssertEqual(unchanged.identity, sender.identity)
        state = try await restore.reset()
        state = try await restore.acceptEncryptedFile(exported.envelope, generation: state.generation)
        let fileSnapshot = try await restore.scanCheckpoint(generation: state.generation)
        XCTAssertNil(fileSnapshot, "ordinary imported files are not opted-in camera progress")
        let oldGeneration = state.generation
        _ = await restore.suspend()
        do { _ = try await restore.scanCheckpoint(generation: oldGeneration); XCTFail("captured with a retired token") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        XCTAssertEqual(vault.creates, 0); XCTAssertNil(disk.bytes(.catalog))
    }

    func testCompleteSavedScanRevalidatesWholeEnvelopeBeforeAdoptingSource() async throws {
        let (restore, _, disk, vault) = environment(), exported = try export()
        let complete = try completedScan(exported.envelope)
        let identity = OpticalFrame.Identity(transferID: complete.identity.transferID,
            envelopeDigest: Data(repeating: 0, count: 32), envelopeLength: complete.identity.envelopeLength)
        let wrongDigest = try OpticalScanCheckpoint(identity: identity, pages: complete.pages,
            pageDigests: complete.pageDigests, using: crypto)
        var badHeader = exported.envelope; badHeader[4] = 99
        let malformed = try OpticalScanCheckpoint(identity: .init(transferID: complete.identity.transferID,
            envelopeDigest: crypto.sha256(badHeader), envelopeLength: badHeader.count),
            pages: [0: badHeader], pageDigests: [0: crypto.sha256(badHeader)], using: crypto)
        let initial = await restore.status()
        for checkpoint in [wrongDigest, malformed] {
            do { _ = try await restore.resumeScan(checkpoint, generation: initial.generation); XCTFail("accepted forged whole-envelope metadata") }
            catch { XCTAssertNotNil(error as? FTError) }
            let state = await restore.status()
            XCTAssertEqual(state.phase, .idle); XCTAssertNil(state.source); XCTAssertNil(state.identity)
            XCTAssertEqual(state.generation, initial.generation)
        }
        XCTAssertEqual(vault.creates, 0); XCTAssertNil(disk.bytes(.catalog)); XCTAssertNil(disk.bytes(.journal))
    }

    func testSavedRecipientScanNeedsTheOriginalPrivateRequestAndRespectsItsBounds() async throws {
        let (original, _, _, _) = environment()
        var state = await original.status()
        state = try await original.createRecipientRequest(generation: state.generation)
        let request = try XCTUnwrap(state.recipientRequest)
        let archive = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [], using: crypto)
        let envelope = try FTAG.seal(archive, recipient: request, confirmedRequestDigest: request.digest(using: crypto), using: crypto)
        let checkpoint = try completedScan(envelope)
        state = try await original.resumeScan(checkpoint, generation: state.generation)
        XCTAssertTrue(state.canUnlockRecipient)
        state = try await original.unlockRecipient(generation: state.generation)
        XCTAssertEqual(state.phase, .review)
        _ = await original.suspend(discardRecipientRequest: true)

        let (restarted, _, disk, vault) = environment()
        state = await restarted.status()
        state = try await restarted.resumeScan(checkpoint, generation: state.generation)
        XCTAssertEqual(state.phase, .locked); XCTAssertEqual(state.encryptionMode, .recipient)
        XCTAssertFalse(state.canUnlockRecipient); XCTAssertNil(state.recipientRequest)
        do { _ = try await restarted.unlockRecipient(generation: state.generation); XCTFail("resurrected a lost recipient key") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        let code = try TransferCode(secret: Data(repeating: 32, count: 16)).reveal(using: crypto)
        do { _ = try await restarted.unlock(enteredCode: code, generation: state.generation); XCTFail("downgraded recipient mode") }
        catch { XCTAssertEqual(error as? FTError, .unsupported("envelope.requires_recipient_key")) }
        state = try await restarted.reset()
        state = try await restarted.createRecipientRequest(generation: state.generation)
        let replacementRequest = state.recipientRequest
        do { _ = try await restarted.resumeScan(checkpoint, generation: state.generation); XCTFail("accepted another recipient request") }
        catch { XCTAssertEqual(error as? FTError, .authentication) }
        state = await restarted.status()
        XCTAssertEqual(state.phase, .idle); XCTAssertNil(state.identity); XCTAssertEqual(state.recipientRequest, replacementRequest)
        state = try await restarted.reset()
        state = try await restarted.createRecipientRequest(generation: state.generation, maximumEnvelopeBytes: 4222)
        let oversized = try completedScan(pagedScan().envelope)
        do { _ = try await restarted.resumeScan(oversized, generation: state.generation); XCTFail("ignored the recipient allocation ceiling") }
        catch { XCTAssertEqual(error as? FTError, .limit("recipient.envelope_limit")) }
        state = await restarted.status()
        XCTAssertEqual(state.phase, .idle); XCTAssertNil(state.identity); XCTAssertEqual(state.verifiedPages, 0)
        XCTAssertEqual(vault.creates, 0); XCTAssertNil(disk.bytes(.catalog))
    }

    func testOpticalSourceSelectionRepairDecodeReviewAndCommit() async throws {
        let (restore, store, _, vault) = environment(), exported = try export()
        let sender = try OpticalSender(envelope: exported.envelope, using: crypto)
        var state = await restore.status()
        state = try await restore.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        XCTAssertEqual(state.phase, .candidate); XCTAssertEqual(state.independentParts, 0)
        XCTAssertEqual(state.verifiedPages, 0); XCTAssertEqual(vault.count, 0)
        state = try await restore.selectCandidate(generation: state.generation, digest: sender.identity.envelopeDigest)
        // Deliberately miss almost all systematic symbols. Recovery uses repair data.
        for sequence in UInt32(100)..<500 {
            state = try await restore.observe(sender.frame(page: 0, sequence: sequence, using: crypto), generation: state.generation)
            if state.phase == .locked { break }
        }
        XCTAssertEqual(state.phase, .locked)
        XCTAssertEqual(state.verifiedBytes, exported.envelope.count)
        XCTAssertNil(state.review); XCTAssertEqual(vault.count, 0)
        state = try await restore.unlock(enteredCode: exported.code.reveal(using: crypto), generation: state.generation)
        XCTAssertEqual(state.phase, .review); XCTAssertEqual(vault.count, 0)
        let review = try XCTUnwrap(state.review)
        XCTAssertEqual(review.document.groups[0].machineOrder, ["build", "gpu"])
        XCTAssertEqual(review.includedFingerprints.count, 1)
        state = try await restore.commit(reviewToken: review.token, generation: state.generation, sourceAndContentsConfirmed: true)
        XCTAssertEqual(state.phase, .complete); XCTAssertEqual(vault.count, 1)
        XCTAssertTrue(try XCTUnwrap(state.receipt).workspace.connectLater)
        XCTAssertTrue(try XCTUnwrap(state.receipt).missingCredentialIDs.isEmpty)
        let saved = try await store.load()
        XCTAssertEqual(saved.count, 1)
    }
    /// After the import commits there is nothing left to read: exclusion ends
    /// (the phone may tell its Mac, or connect) and reset takes it back. The
    /// arrival proof key is the digest of the very manifest the Mac sealed.
    func testACommittedTransferEndsOfflineExclusionAndCarriesItsArrival() async throws {
        let activity = WorkspaceNetworkActivity()
        let restore = OfflineRestore(using: crypto, store: WorkspaceStore(persistence: StoreTestPersistence(), vault: StoreTestVault()),
                                     networkActivity: activity)
        let (key, fingerprint) = try sampleKey(using: crypto)
        let archive = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint)), keys: [key], using: crypto)
        let exported = try FTAG.seal(archive, using: crypto)
        let sender = try OpticalSender(envelope: exported.envelope, using: crypto)
        try await restore.enterOfflineMode()
        var state = await restore.status()
        state = try await restore.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        state = try await restore.selectCandidate(generation: state.generation, digest: sender.identity.envelopeDigest)
        for sequence in UInt32(2)..<500 where state.phase != .locked {
            state = try await restore.observe(sender.frame(page: 0, sequence: sequence, using: crypto), generation: state.generation)
        }
        state = try await restore.unlock(enteredCode: exported.code.reveal(using: crypto), generation: state.generation)
        XCTAssertNil(state.arrival)
        XCTAssertEqual(activity.accounting().offlineRestores, 1)
        XCTAssertThrowsError(try activity.beginConnection())
        state = try await restore.commit(reviewToken: XCTUnwrap(state.review).token, generation: state.generation,
                                         sourceAndContentsConfirmed: true)
        XCTAssertEqual(state.phase, .complete)
        XCTAssertEqual(state.arrival, TransferArrival(envelopeDigest: sender.identity.envelopeDigest,
                                                      workspaceDigest: crypto.sha256(archive.workspace.raw)))
        XCTAssertEqual(activity.accounting().offlineRestores, 0)
        try await restore.enterOfflineMode()
        XCTAssertEqual(activity.accounting().offlineRestores, 0, "a finished restore stays online")
        var connection: WorkspaceNetworkLease? = try activity.beginConnection()
        state = try await restore.reset()
        XCTAssertNil(state.arrival); XCTAssertEqual(state.phase, .idle)
        do { try await restore.enterOfflineMode(); XCTFail("entered offline mode beside a connection") }
        catch { XCTAssertEqual(error as? WorkspaceNetworkError, .connectionsActive) }
        withExtendedLifetime(connection) {}; connection = nil
        try await restore.enterOfflineMode()
        XCTAssertEqual(activity.accounting().offlineRestores, 1)
    }
    func testWrongCodeDoesNotDestroyCollectedEnvelopeOrTouchStorage() async throws {
        let (restore, _, persistence, vault) = environment(), exported = try export()
        let initial = await restore.status()
        let locked = try await restore.acceptEncryptedFile(exported.envelope, generation: initial.generation)
        let wrong = try TransferCode(secret: Data(repeating: 88, count: 16)).reveal(using: crypto)
        do { _ = try await restore.unlock(enteredCode: wrong, generation: locked.generation); XCTFail("wrong code accepted") }
        catch { XCTAssertEqual(error as? FTError, .authentication) }
        let state = await restore.status()
        XCTAssertEqual(state.phase, .locked); XCTAssertEqual(state.verifiedBytes, exported.envelope.count)
        XCTAssertNil(persistence.bytes(.catalog)); XCTAssertEqual(vault.count, 0)
        let ready = try await restore.unlock(enteredCode: exported.code.reveal(using: crypto), generation: state.generation)
        XCTAssertEqual(ready.phase, .review)
    }
    func testResetAndSuspendFenceLateCameraAndReviewCallbacks() async throws {
        let (restore, _, _, vault) = environment(), exported = try export()
        let sender = try OpticalSender(envelope: exported.envelope, using: crypto)
        let initial = await restore.status()
        _ = try await restore.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: initial.generation)
        let reset = try await restore.reset()
        do { _ = try await restore.observe(sender.frame(page: 0, sequence: 2, using: crypto), generation: initial.generation); XCTFail("retired callback") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        _ = try await restore.acceptEncryptedFile(exported.envelope, generation: reset.generation)
        let ready = try await restore.unlock(enteredCode: exported.code.reveal(using: crypto), generation: reset.generation)
        let review = try XCTUnwrap(ready.review)
        let suspended = await restore.suspend()
        XCTAssertNil(suspended.review); XCTAssertTrue(suspended.isSuspended)
        let resumed = await restore.resume()
        XCTAssertEqual(resumed.phase, .locked)
        do { _ = try await restore.commit(reviewToken: review.token, generation: ready.generation, sourceAndContentsConfirmed: true); XCTFail("stale review") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        XCTAssertEqual(vault.count, 0)
        let rereviewed = try await restore.unlock(enteredCode: exported.code.reveal(using: crypto), generation: resumed.generation)
        XCTAssertNotEqual(rereviewed.review?.token, review.token)
    }
    func testUnrelatedCodeCannotReplaceSelectedSource() async throws {
        let (restore, _, _, _) = environment(), a = try export(), b = try export()
        let first = try OpticalSender(envelope: a.envelope, using: crypto)
        let second = try OpticalSender(envelope: b.envelope, using: crypto)
        var state = await restore.status()
        state = try await restore.observe(first.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        state = try await restore.selectCandidate(generation: state.generation, digest: first.identity.envelopeDigest)
        let rank = state.independentParts
        do { _ = try await restore.observe(second.frame(page: 0, sequence: 1, using: crypto), generation: state.generation); XCTFail("mixed source") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        let after = await restore.status()
        XCTAssertEqual(after.identity, first.identity); XCTAssertEqual(after.independentParts, rank)
        do { _ = try await restore.acceptEncryptedFile(b.envelope, generation: state.generation); XCTFail("replaced scan") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .busy) }
    }
    func testConfirmationAndExactReviewTokenRequired() async throws {
        let (restore, _, persistence, vault) = environment(), exported = try export()
        let initial = await restore.status()
        _ = try await restore.acceptEncryptedFile(exported.envelope, generation: initial.generation)
        let ready = try await restore.unlock(enteredCode: exported.code.reveal(using: crypto), generation: initial.generation)
        let review = try XCTUnwrap(ready.review)
        do { _ = try await restore.commit(reviewToken: review.token, generation: ready.generation, sourceAndContentsConfirmed: false); XCTFail("no confirmation") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .reviewRequired) }
        do { _ = try await restore.commit(reviewToken: UUID(), generation: ready.generation, sourceAndContentsConfirmed: true); XCTFail("wrong review") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        XCTAssertNil(persistence.bytes(.journal)); XCTAssertEqual(vault.count, 0)
    }
    func testPublicationErrorReconcilesInsteadOfReinsertingKeys() async throws {
        let (restore, _, persistence, vault) = environment(), exported = try export()
        let initial = await restore.status()
        _ = try await restore.acceptEncryptedFile(exported.envelope, generation: initial.generation)
        let ready = try await restore.unlock(enteredCode: exported.code.reveal(using: crypto), generation: initial.generation)
        persistence.configure(.after(.catalog))
        do { _ = try await restore.commit(reviewToken: XCTUnwrap(ready.review).token, generation: ready.generation, sourceAndContentsConfirmed: true); XCTFail("fault ignored") }
        catch {}
        let uncertain = await restore.status()
        XCTAssertEqual(uncertain.phase, .recoveryRequired)
        do { _ = try await restore.reset(); XCTFail("erased uncertain publication") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .recoveryRequired) }
        let result = try await restore.reconcile(generation: uncertain.generation)
        XCTAssertEqual(result.phase, .complete); XCTAssertEqual(vault.creates, 1)
        XCTAssertEqual(result.receipt?.missingCredentialIDs, [])
    }
    func testPrepublicationFailureReconcilesToFreshReview() async throws {
        let (restore, _, persistence, vault) = environment(), exported = try export()
        let initial = await restore.status()
        _ = try await restore.acceptEncryptedFile(exported.envelope, generation: initial.generation)
        let ready = try await restore.unlock(enteredCode: exported.code.reveal(using: crypto), generation: initial.generation)
        persistence.configure(.before(.catalog))
        do { _ = try await restore.commit(reviewToken: XCTUnwrap(ready.review).token, generation: ready.generation, sourceAndContentsConfirmed: true); XCTFail("fault ignored") }
        catch {}
        let state = try await restore.reconcile(generation: ready.generation)
        XCTAssertEqual(state.phase, .locked); XCTAssertNil(state.review); XCTAssertNil(state.receipt)
        XCTAssertEqual(vault.count, 0)
    }
    func testSuspensionPreservesOnlyCiphertextProgress() async throws {
        let (restore, _, _, _) = environment(), exported = try export()
        let sender = try OpticalSender(envelope: exported.envelope, using: crypto)
        var state = await restore.status()
        state = try await restore.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        state = try await restore.selectCandidate(generation: state.generation, digest: sender.identity.envelopeDigest)
        let before = state.independentParts
        _ = await restore.suspend(); state = await restore.resume()
        XCTAssertEqual(state.phase, .collecting); XCTAssertEqual(state.independentParts, before)
        for sequence in UInt32(2)...UInt32(try sender.sourcePartCount(page: 0)) {
            state = try await restore.observe(sender.frame(page: 0, sequence: sequence, using: crypto), generation: state.generation)
        }
        XCTAssertEqual(state.phase, .locked)
    }
    func testPublicWorkspaceFileReviewsAndCommitsWithoutCodeOrKeyStaging() async throws {
        let (restore, store, persistence, vault) = environment()
        let document = try sampleManifest(), initial = await restore.status()
        let ready = try await restore.acceptWorkspaceDocument(document, generation: initial.generation)
        XCTAssertEqual(ready.phase, .review); XCTAssertEqual(ready.source, .workspaceDocument)
        XCTAssertNil(ready.identity); XCTAssertEqual(ready.verifiedPages, 0); XCTAssertEqual(ready.verifiedBytes, 0)
        XCTAssertEqual(ready.comparison, try PublicWorkspaceIdentity.comparison(data: document, using: crypto))
        let expectedDigest = crypto.sha256(Data("FrankenTerm/ftworkspace/v1/document\0".utf8) + document)
        XCTAssertEqual(try PublicWorkspaceIdentity.digest(data: document, using: crypto), expectedDigest)
        XCTAssertNotEqual(expectedDigest, crypto.sha256(document), "public imports keep their established domain-separated history identity")
        let equivalentBytes = document + Data("\n".utf8)
        XCTAssertEqual(try WorkspaceDocument(data: equivalentBytes).id, try WorkspaceDocument(data: document).id)
        XCTAssertNotEqual(try PublicWorkspaceIdentity.comparison(data: equivalentBytes, using: crypto), ready.comparison,
                          "comparison covers exact file bytes, including whitespace")
        let review = try XCTUnwrap(ready.review)
        XCTAssertEqual(review.document.groups[0].machineOrder, ["build", "gpu"])
        XCTAssertTrue(review.includedFingerprints.isEmpty)
        XCTAssertNil(review.expectedRevision); XCTAssertFalse(review.isRollback)
        XCTAssertEqual(vault.creates, 0); XCTAssertNil(persistence.bytes(.catalog))
        do {
            _ = try await restore.commit(reviewToken: review.token, generation: ready.generation, sourceAndContentsConfirmed: false)
            XCTFail("a public document must still receive source and contents review")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .reviewRequired) }
        let committed = try await restore.commit(reviewToken: review.token, generation: ready.generation, sourceAndContentsConfirmed: true)
        let receipt = try XCTUnwrap(committed.receipt)
        XCTAssertEqual(receipt.workspace.envelopeDigest, expectedDigest)
        XCTAssertEqual(committed.phase, .complete)
        XCTAssertEqual(receipt.missingCredentialIDs, ["key"])
        XCTAssertTrue(receipt.workspace.connectLater)
        XCTAssertEqual(vault.count, 0); XCTAssertEqual(vault.creates, 0)
        XCTAssertNil(persistence.bytes(.journal))
        let saved = try await store.load()
        XCTAssertEqual(saved.count, 1); XCTAssertEqual(saved[0].manifest, document)
        let relaunched = WorkspaceStore(persistence: persistence, vault: vault)
        let reopened = try await relaunched.load()
        XCTAssertEqual(reopened[0].revision, receipt.workspace.revision)
        XCTAssertTrue(reopened[0].connectLater)
        XCTAssertTrue(try reopened[0].document().machines.allSatisfy(\.autoConnect), "preferences survive without becoming permission to connect")
    }
    func testPublicDocumentSuspensionRequiresFreshReviewWithoutADecryptStep() async throws {
        let (restore, _, persistence, vault) = environment(), initial = await restore.status()
        let ready = try await restore.acceptWorkspaceDocument(sampleManifest(), generation: initial.generation)
        let originalReview = try XCTUnwrap(ready.review)
        let paused = await restore.suspend()
        XCTAssertEqual(paused.phase, .documentReady); XCTAssertTrue(paused.isSuspended); XCTAssertNil(paused.review)
        do { _ = try await restore.reviewWorkspaceDocument(generation: paused.generation); XCTFail("review while suspended") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        let resumed = await restore.resume()
        XCTAssertEqual(resumed.phase, .documentReady); XCTAssertEqual(resumed.source, .workspaceDocument)
        XCTAssertNotEqual(resumed.generation, ready.generation)
        XCTAssertEqual(resumed.comparison, ready.comparison)
        do {
            _ = try await restore.commit(reviewToken: originalReview.token, generation: ready.generation, sourceAndContentsConfirmed: true)
            XCTFail("retired generation committed a public document")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        let rereviewed = try await restore.reviewWorkspaceDocument(generation: resumed.generation)
        let newReview = try XCTUnwrap(rereviewed.review)
        XCTAssertEqual(rereviewed.phase, .review); XCTAssertNotEqual(newReview.token, originalReview.token)
        XCTAssertEqual(newReview.document.raw, originalReview.document.raw)
        do {
            _ = try await restore.commit(reviewToken: originalReview.token, generation: rereviewed.generation, sourceAndContentsConfirmed: true)
            XCTFail("old review token was accepted with the current generation")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        XCTAssertNil(persistence.bytes(.catalog)); XCTAssertEqual(vault.creates, 0)
        let committed = try await restore.commit(reviewToken: newReview.token, generation: rereviewed.generation, sourceAndContentsConfirmed: true)
        XCTAssertEqual(committed.phase, .complete); XCTAssertEqual(vault.creates, 0)
    }
    func testPublicDocumentPublicationFailuresReconcileToCommittedOrReviewableState() async throws {
        for (fault, published) in [(StoreTestPersistence.Fault.before(.catalog), false), (.after(.catalog), true)] {
            let (restore, store, persistence, vault) = environment(), initial = await restore.status()
            let ready = try await restore.acceptWorkspaceDocument(sampleManifest(), generation: initial.generation)
            let originalReview = try XCTUnwrap(ready.review)
            persistence.configure(fault)
            do {
                _ = try await restore.commit(reviewToken: originalReview.token, generation: ready.generation, sourceAndContentsConfirmed: true)
                XCTFail("the injected public-file publication fault was not exercised")
            } catch { XCTAssertEqual(error as? FTError, .storage) }
            let uncertain = await restore.status()
            XCTAssertEqual(uncertain.phase, .recoveryRequired); XCTAssertEqual(uncertain.source, .workspaceDocument)
            do { _ = try await restore.reset(); XCTFail("lost the unsettled public-file transaction") }
            catch { XCTAssertEqual(error as? WorkspaceStoreError, .recoveryRequired) }
            var reconciled = try await restore.reconcile(generation: uncertain.generation)
            XCTAssertEqual(reconciled.phase, published ? .complete : .documentReady)
            XCTAssertEqual(vault.count, 0); XCTAssertEqual(vault.creates, 0)
            let saved = try await store.load()
            XCTAssertEqual(saved.count, published ? 1 : 0)
            if !published {
                XCTAssertNil(reconciled.review); XCTAssertNil(reconciled.receipt)
                reconciled = try await restore.reviewWorkspaceDocument(generation: reconciled.generation)
                let retryReview = try XCTUnwrap(reconciled.review)
                XCTAssertNotEqual(retryReview.token, originalReview.token)
                XCTAssertNil(retryReview.expectedRevision)
                reconciled = try await restore.commit(reviewToken: retryReview.token, generation: reconciled.generation, sourceAndContentsConfirmed: true)
            }
            XCTAssertEqual(reconciled.phase, .complete)
            XCTAssertEqual(reconciled.receipt?.missingCredentialIDs, ["key"])
            XCTAssertTrue(try XCTUnwrap(reconciled.receipt).workspace.connectLater)
            XCTAssertNil(persistence.bytes(.journal)); XCTAssertEqual(vault.creates, 0)
        }
    }
    func testPreviouslyImportedEncryptedExportRequiresExplicitRollbackApprovalAfterRelaunch() async throws {
        let (restore, _, persistence, vault) = environment(), original = try export()
        let originalArchive = try FTAG.open(original.envelope, code: original.code, using: crypto)
        let replacementArchive = try AirGapArchive(workspace: originalArchive.workspace.reissuingBundle(id: "replacement"),
                                                   keys: originalArchive.keys, using: crypto)
        let replacement = try FTAG.seal(replacementArchive, using: crypto)
        var state = await restore.status()
        for exported in [original, replacement] {
            _ = try await restore.acceptEncryptedFile(exported.envelope, generation: state.generation)
            state = try await restore.unlock(enteredCode: exported.code.reveal(using: crypto), generation: state.generation)
            let review = try XCTUnwrap(state.review)
            XCTAssertFalse(review.isRollback)
            _ = try await restore.commit(reviewToken: review.token, generation: state.generation, sourceAndContentsConfirmed: true,
                                         associateWithExistingWorkspace: true)
            state = try await restore.reset()
        }
        XCTAssertEqual(vault.creates, 1, "reissuing a bundle reuses the existing canonical identity")
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let currentWorkspaces = try await store.load()
        let current = try XCTUnwrap(currentWorkspaces.first)
        XCTAssertEqual(try current.document().bundleID, "replacement")
        let reopened = OfflineRestore(using: crypto, store: store)
        state = await reopened.status()
        _ = try await reopened.acceptEncryptedFile(original.envelope, generation: state.generation)
        state = try await reopened.unlock(enteredCode: original.code.reveal(using: crypto), generation: state.generation)
        let rollbackReview = try XCTUnwrap(state.review)
        XCTAssertTrue(rollbackReview.isRollback); XCTAssertEqual(rollbackReview.expectedRevision, current.revision)
        do {
            _ = try await reopened.commit(reviewToken: rollbackReview.token, generation: state.generation, sourceAndContentsConfirmed: true)
            XCTFail("a previously superseded encrypted bundle bypassed rollback approval")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .rollbackNeedsApproval) }
        let denied = await reopened.status()
        XCTAssertEqual(denied.phase, .review); XCTAssertEqual(denied.review?.token, rollbackReview.token)
        XCTAssertNil(persistence.bytes(.journal)); XCTAssertEqual(vault.creates, 1)
        let unchanged = try await store.load()
        XCTAssertEqual(unchanged[0].revision, current.revision)
        state = try await reopened.commit(reviewToken: rollbackReview.token, generation: state.generation,
                                         sourceAndContentsConfirmed: true, allowRollback: true)
        let receipt = try XCTUnwrap(state.receipt)
        XCTAssertEqual(state.phase, .complete)
        XCTAssertEqual(try receipt.workspace.document().bundleID, originalArchive.workspace.bundleID)
        XCTAssertTrue(receipt.workspace.connectLater); XCTAssertEqual(vault.creates, 1)
    }
    func testPageRepairPreservesVerifiedPagesAndFencesQueuedPreResetObservations() async throws {
        let (restore, _, persistence, vault) = environment()
        // A valid archive large enough to span pages, sealed without compression
        // so its transport shape is independent of the zlib implementation.
        let findings = (0..<10).map {
            AirGapArchive.Portability(entityID: "route", setting: "desktop_setting_\($0)", disposition: 0,
                                       reason: String(repeating: "x", count: 4000))
        }
        let archive = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [], portability: findings, using: crypto)
        let code = try TransferCode(secret: Data(repeating: 7, count: 16))
        let envelope = try FTAG.sealArchiveBytes(archive.encoded(), code: code, transferID: Data(repeating: 8, count: 16),
                                                salt: Data(repeating: 9, count: 16), nonce: Data(repeating: 10, count: 12), compress: false, using: crypto)
        let sender = try OpticalSender(envelope: envelope, using: crypto)
        XCTAssertEqual(sender.identity.pageCount, 2)
        var state = await restore.status()
        state = try await restore.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        state = try await restore.selectCandidate(generation: state.generation, digest: sender.identity.envelopeDigest)
        // Progress counts parts, the same total the sender splits the pages into.
        let required = try sender.sourcePartCount(page: 0) + sender.sourcePartCount(page: 1)
        XCTAssertEqual(state.requiredParts, required); XCTAssertEqual(state.receivedParts, 1)
        for sequence in 2...(try sender.sourcePartCount(page: 0)) {
            state = try await restore.observe(sender.frame(page: 0, sequence: UInt32(sequence), using: crypto), generation: state.generation)
            XCTAssertEqual(state.receivedParts, sequence)
        }
        state = try await restore.observe(sender.frame(page: 1, sequence: 1, using: crypto), generation: state.generation)
        XCTAssertEqual(state.verifiedPages, 1); XCTAssertEqual(state.verifiedBytes, FTLimit.page)
        XCTAssertEqual(state.independentParts, 1); XCTAssertEqual(state.incompletePageIndices, [1])
        XCTAssertEqual(state.receivedParts, try sender.sourcePartCount(page: 0) + 1)
        let oldGeneration = state.generation
        state = try await restore.resetIncompletePage(1, generation: oldGeneration)
        XCTAssertEqual(state.phase, .collecting); XCTAssertNotEqual(state.generation, oldGeneration)
        XCTAssertEqual(state.identity, sender.identity); XCTAssertEqual(state.independentParts, 0)
        XCTAssertEqual(state.verifiedPages, 1); XCTAssertEqual(state.verifiedBytes, FTLimit.page)
        XCTAssertEqual(state.incompletePageIndices, [1])
        do {
            _ = try await restore.observe(sender.frame(page: 1, sequence: 2, using: crypto), generation: oldGeneration)
            XCTFail("queued observation repopulated a deliberately reset decoder")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        do { _ = try await restore.resetIncompletePage(0, generation: state.generation); XCTFail("verified page was reset") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        for sequence in 1...(try sender.sourcePartCount(page: 1)) {
            state = try await restore.observe(sender.frame(page: 1, sequence: UInt32(sequence), using: crypto), generation: state.generation)
        }
        XCTAssertEqual(state.phase, .locked); XCTAssertEqual(state.verifiedBytes, envelope.count)
        XCTAssertTrue(state.incompletePageIndices.isEmpty)
        XCTAssertEqual(vault.creates, 0); XCTAssertNil(persistence.bytes(.catalog))
        let reviewed = try await restore.unlock(enteredCode: code.reveal(using: crypto), generation: state.generation)
        XCTAssertEqual(reviewed.phase, .review); XCTAssertEqual(reviewed.review?.findings.count, 10)
    }

    func review(_ exported: FTAG.Export, using restore: OfflineRestore<TestCryptography>) async throws -> OfflineRestoreStatus {
        let reset = try await restore.reset()
        _ = try await restore.acceptEncryptedFile(exported.envelope, generation: reset.generation)
        return try await restore.unlock(enteredCode: exported.code.reveal(using: crypto), generation: reset.generation)
    }

    func testHistoricalEncryptedRecordingRequiresReviewBoundRollbackApproval() async throws {
        let (restore, store, persistence, vault) = environment()
        let original = try export(bundleID: "original"), replacement = try export(bundleID: "replacement")
        var ready = try await review(original, using: restore)
        let first = try await restore.commit(reviewToken: XCTUnwrap(ready.review).token, generation: ready.generation,
                                             sourceAndContentsConfirmed: true)
        let firstRevision = try XCTUnwrap(first.receipt).workspace.revision
        ready = try await review(replacement, using: restore)
        XCTAssertFalse(try XCTUnwrap(ready.review).requiresRollbackApproval)
        let second = try await restore.commit(reviewToken: XCTUnwrap(ready.review).token, generation: ready.generation,
                                              sourceAndContentsConfirmed: true, associateWithExistingWorkspace: true)
        let secondRevision = try XCTUnwrap(second.receipt).workspace.revision
        ready = try await review(original, using: restore)
        let rollback = try XCTUnwrap(ready.review)
        XCTAssertTrue(rollback.requiresRollbackApproval)
        XCTAssertEqual(rollback.firstAcceptedRevision, firstRevision)
        XCTAssertEqual(rollback.expectedRevision, secondRevision)
        let before = persistence.bytes(.catalog)
        do {
            _ = try await restore.commit(reviewToken: rollback.token, generation: ready.generation,
                sourceAndContentsConfirmed: true, allowMachineRemoval: true)
            XCTFail("generic source/removal review silently rolled back an accepted version")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .rollbackNeedsApproval) }
        let unchanged = await restore.status()
        XCTAssertEqual(unchanged.phase, .review); XCTAssertEqual(unchanged.review?.token, rollback.token)
        XCTAssertEqual(persistence.bytes(.catalog), before)
        let restored = try await restore.commit(reviewToken: rollback.token, generation: ready.generation,
            sourceAndContentsConfirmed: true, allowHistoricalRollback: true)
        let restoredRevision = try XCTUnwrap(restored.receipt).workspace.revision
        XCTAssertNotEqual(restoredRevision, secondRevision); XCTAssertNotEqual(restoredRevision, firstRevision)
        ready = try await review(original, using: restore)
        XCTAssertFalse(try XCTUnwrap(ready.review).requiresRollbackApproval)
        let repeated = try await restore.commit(reviewToken: XCTUnwrap(ready.review).token, generation: ready.generation,
                                                sourceAndContentsConfirmed: true)
        XCTAssertEqual(repeated.receipt?.workspace.revision, restoredRevision); XCTAssertEqual(vault.creates, 1)
        let saved = try await store.load()
        XCTAssertEqual(try saved[0].document().bundleID, "original")
    }

    func testAuthenticatedResealCannotReuseAcceptedBundleIdentity() async throws {
        let (restore, _, persistence, vault) = environment()
        let first = try export(), resealed = try export()
        let ready = try await review(first, using: restore)
        _ = try await restore.commit(reviewToken: XCTUnwrap(ready.review).token, generation: ready.generation,
                                     sourceAndContentsConfirmed: true)
        let before = persistence.bytes(.catalog)
        do { _ = try await review(resealed, using: restore); XCTFail("same bundle resealed to different authenticated bytes") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .conflictingExport) }
        let state = await restore.status()
        XCTAssertEqual(state.phase, .locked); XCTAssertNil(state.review)
        XCTAssertEqual(state.verifiedBytes, resealed.envelope.count)
        XCTAssertEqual(persistence.bytes(.catalog), before); XCTAssertEqual(vault.creates, 1)
        XCTAssertNil(persistence.bytes(.journal))
    }

    func testAuthenticatedNewBundleCannotReuseAcceptedTransferIdentity() async throws {
        let (restore, _, persistence, vault) = environment()
        let first = try export(bundleID: "first"), second = try export(bundleID: "second")
        let ready = try await review(first, using: restore)
        _ = try await restore.commit(reviewToken: XCTUnwrap(ready.review).token, generation: ready.generation,
                                     sourceAndContentsConfirmed: true)
        let secondArchive = try FTAG.open(second.envelope, code: second.code, using: crypto)
        let rebound = try FTAG.sealArchiveBytes(secondArchive.encoded(), code: second.code,
            transferID: FTAGHeader(envelope: first.envelope).transferID,
            salt: Data(repeating: 3, count: 16), nonce: Data(repeating: 4, count: 12), using: crypto)
        let reset = try await restore.reset()
        _ = try await restore.acceptEncryptedFile(rebound, generation: reset.generation)
        let before = persistence.bytes(.catalog)
        do {
            _ = try await restore.unlock(enteredCode: second.code.reveal(using: crypto), generation: reset.generation)
            XCTFail("authenticated transfer identifier rebinding")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .conflictingExport) }
        XCTAssertEqual(persistence.bytes(.catalog), before); XCTAssertEqual(vault.creates, 1)
    }

    func testPreflightRejectionReturnsToSourceSpecificReviewWithoutAmbiguousSave() async throws {
        for publicDocument in [false, true] {
            let (restore, store, persistence, vault) = environment()
            let exported = try export(), archive = try FTAG.open(exported.envelope, code: exported.code, using: crypto)
            let ready: OfflineRestoreStatus
            if publicDocument {
                let initial = await restore.status()
                ready = try await restore.acceptWorkspaceDocument(archive.workspace.raw, generation: initial.generation)
            } else { ready = try await review(exported, using: restore) }
            let originalReview = try XCTUnwrap(ready.review)
            try await store.revokeLocally(fingerprint: archive.workspace.credentials[0].fingerprint)
            let before = persistence.bytes(.catalog)
            do {
                _ = try await restore.commit(reviewToken: originalReview.token, generation: ready.generation,
                                              sourceAndContentsConfirmed: true)
                XCTFail("revocation after review was not rechecked")
            } catch { XCTAssertEqual(error as? WorkspaceStoreError, .revokedIdentity) }
            let rejected = await restore.status()
            XCTAssertEqual(rejected.phase, publicDocument ? .documentReady : .locked)
            XCTAssertNil(rejected.review); XCTAssertNil(rejected.receipt)
            XCTAssertEqual(persistence.bytes(.catalog), before)
            XCTAssertNil(persistence.bytes(.journal)); XCTAssertEqual(vault.creates, 0)
            let reset = try await restore.reset()
            XCTAssertEqual(reset.phase, .idle)
        }
    }

    private func changedDocument(bundleID: String, user: String) throws -> Data {
        var root = try StrictJSON.decode(sampleManifest(bundleID: bundleID)).object()
        var workspace = try root.required("workspace").object()
        var routes = try workspace.required("routes").array()
        var route = try routes[0].object()
        route["user"] = .string(user); routes[0] = .object(route)
        workspace["routes"] = .array(routes); root["workspace"] = .object(workspace)
        return try StrictJSON.encode(.object(root))
    }

    func testReimportShowsExactOldAndNewValuesAndRequiresIndependentReplacementConsents() async throws {
        let (restore, store, disk, vault) = environment()
        var state = await restore.status()
        state = try await restore.acceptWorkspaceDocument(sampleManifest(), generation: state.generation)
        state = try await restore.commit(reviewToken: XCTUnwrap(state.review).token, generation: state.generation,
                                        sourceAndContentsConfirmed: true)
        let first = try XCTUnwrap(state.receipt).workspace
        state = try await restore.reset()
        state = try await restore.acceptWorkspaceDocument(changedDocument(bundleID: "replacement", user: "reviewed-account"),
                                                         generation: state.generation)
        let review = try XCTUnwrap(state.review)
        XCTAssertEqual(review.existingWorkspaceName, "Machines")
        XCTAssertTrue(review.requiresSourceAssociation); XCTAssertTrue(review.requiresSecurityApproval)
        let change = try XCTUnwrap(review.changeReview?.changes.first { $0.entity == .route && $0.field == "user" })
        XCTAssertEqual(change.previous, .string("operator")); XCTAssertEqual(change.incoming, .string("reviewed-account"))
        let before = disk.bytes(.catalog)
        do {
            _ = try await restore.commit(reviewToken: review.token, generation: state.generation,
                sourceAndContentsConfirmed: true, securityChangesConfirmed: true)
            XCTFail("general source consent replaced a claimed existing namespace")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .sourceReplacementNeedsApproval) }
        do {
            _ = try await restore.commit(reviewToken: review.token, generation: state.generation,
                sourceAndContentsConfirmed: true, associateWithExistingWorkspace: true)
            XCTFail("namespace association approved changed authentication details")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .securityChangesNeedApproval) }
        let waiting = await restore.status()
        XCTAssertEqual(waiting.phase, .review); XCTAssertEqual(waiting.review?.token, review.token)
        XCTAssertEqual(disk.bytes(.catalog), before); XCTAssertNil(disk.bytes(.journal)); XCTAssertEqual(vault.creates, 0)
        let committed = try await restore.commit(reviewToken: review.token, generation: state.generation,
            sourceAndContentsConfirmed: true, associateWithExistingWorkspace: true, securityChangesConfirmed: true)
        XCTAssertEqual(committed.phase, .complete)
        let loaded = try await store.load(), saved = try XCTUnwrap(loaded.first)
        XCTAssertEqual(saved.id, first.id); XCTAssertNotEqual(saved.revision, first.revision)
        XCTAssertEqual(try saved.document().routes[0].user, "reviewed-account"); XCTAssertTrue(saved.connectLater)
    }

    func testReplacementConsentCannotFollowAnotherDocumentOrAChangedSavedRevision() async throws {
        let (restore, store, disk, _) = environment()
        var state = await restore.status()
        state = try await restore.acceptWorkspaceDocument(sampleManifest(), generation: state.generation)
        state = try await restore.commit(reviewToken: XCTUnwrap(state.review).token, generation: state.generation,
                                        sourceAndContentsConfirmed: true)
        let first = try XCTUnwrap(state.receipt).workspace
        state = try await restore.reset()
        state = try await restore.acceptWorkspaceDocument(changedDocument(bundleID: "first-candidate", user: "reviewed-account"),
                                                         generation: state.generation)
        let oldReview = try XCTUnwrap(state.review), oldGeneration = state.generation
        state = try await restore.reset()
        state = try await restore.acceptWorkspaceDocument(changedDocument(bundleID: "different-candidate", user: "different-account"),
                                                         generation: state.generation)
        let currentReview = try XCTUnwrap(state.review), before = disk.bytes(.catalog)
        do {
            _ = try await restore.commit(reviewToken: oldReview.token, generation: state.generation,
                sourceAndContentsConfirmed: true, associateWithExistingWorkspace: true, securityChangesConfirmed: true)
            XCTFail("a confirmation for other bytes authorized the current candidate")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        do {
            _ = try await restore.commit(reviewToken: currentReview.token, generation: oldGeneration,
                sourceAndContentsConfirmed: true, associateWithExistingWorkspace: true, securityChangesConfirmed: true)
            XCTFail("an earlier generation authorized replacement")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        XCTAssertEqual(disk.bytes(.catalog), before)

        // Another scene completes its own reviewed replacement while this sheet
        // remains open. The old→new diff is now stale even though its token fits.
        let parallel = try WorkspaceDocument(data: changedDocument(bundleID: "parallel-export", user: "parallel-account"))
        let parallelArchive = try AirGapArchive(workspace: parallel, keys: [], using: crypto)
        let replacement = try await store.install(parallelArchive,
            envelopeDigest: PublicWorkspaceIdentity.digest(data: parallel.raw, using: crypto),
            expectedRevision: first.revision, sourceAndContentsConfirmed: true,
            securityChangesConfirmed: true, associateWithExistingWorkspace: true)
        let afterParallel = disk.bytes(.catalog)
        do {
            _ = try await restore.commit(reviewToken: currentReview.token, generation: state.generation,
                sourceAndContentsConfirmed: true, associateWithExistingWorkspace: true, securityChangesConfirmed: true)
            XCTFail("approval based on an old saved revision replaced another scene's work")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
        XCTAssertEqual(disk.bytes(.catalog), afterParallel); XCTAssertNil(disk.bytes(.journal))
        state = await restore.status()
        XCTAssertEqual(state.phase, .documentReady); XCTAssertNil(state.review)
        state = try await restore.reviewWorkspaceDocument(generation: state.generation)
        XCTAssertEqual(state.review?.expectedRevision, replacement.workspace.revision)
        let refreshedChange = try XCTUnwrap(state.review?.changeReview?.changes.first { $0.entity == .route && $0.field == "user" })
        XCTAssertEqual(refreshedChange.previous, .string("parallel-account"))
        XCTAssertEqual(refreshedChange.incoming, .string("different-account"))
    }
}
