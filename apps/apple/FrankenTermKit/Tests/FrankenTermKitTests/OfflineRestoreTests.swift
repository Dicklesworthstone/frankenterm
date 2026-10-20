import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class OfflineRestoreTests: XCTestCase {
    let crypto = TestCryptography()
    func export(keys: Bool = true, bundleID: String = "bundle") throws -> FTAG.Export {
        let (key, fingerprint) = try sampleKey(using: crypto)
        var manifest = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest(fingerprint: fingerprint)) as? [String: Any])
        manifest["bundle_id"] = bundleID
        return try FTAG.seal(AirGapArchive(workspace: WorkspaceDocument(data: JSONSerialization.data(withJSONObject: manifest, options: [.sortedKeys])),
                                          keys: keys ? [key] : [], using: crypto), using: crypto)
    }
    func environment() -> (OfflineRestore<TestCryptography>, WorkspaceStore, StoreTestPersistence, StoreTestVault) {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        return (OfflineRestore(using: crypto, store: store), store, persistence, vault)
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
                                              sourceAndContentsConfirmed: true)
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
        XCTAssertEqual(unchanged.phase, .review)
        XCTAssertEqual(unchanged.review?.token, rollback.token)
        XCTAssertEqual(persistence.bytes(.catalog), before)
        let restored = try await restore.commit(reviewToken: rollback.token, generation: ready.generation,
            sourceAndContentsConfirmed: true, allowHistoricalRollback: true)
        let restoredRevision = try XCTUnwrap(restored.receipt).workspace.revision
        XCTAssertNotEqual(restoredRevision, secondRevision)
        XCTAssertNotEqual(restoredRevision, firstRevision)
        ready = try await review(original, using: restore)
        XCTAssertFalse(try XCTUnwrap(ready.review).requiresRollbackApproval)
        let repeated = try await restore.commit(reviewToken: XCTUnwrap(ready.review).token, generation: ready.generation,
                                                sourceAndContentsConfirmed: true)
        XCTAssertEqual(repeated.receipt?.workspace.revision, restoredRevision)
        XCTAssertEqual(vault.creates, 1)
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
        XCTAssertEqual(state.phase, .locked)
        XCTAssertNil(state.review)
        XCTAssertEqual(state.verifiedBytes, resealed.envelope.count)
        XCTAssertEqual(persistence.bytes(.catalog), before)
        XCTAssertEqual(vault.creates, 1)
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
        XCTAssertEqual(persistence.bytes(.catalog), before)
        XCTAssertEqual(vault.creates, 1)
    }
}
