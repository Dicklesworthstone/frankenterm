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
    func testPublicWorkspaceFileReviewsAndCommitsWithoutCodeOrKeyStaging() async throws {
        let (restore, store, persistence, vault) = environment()
        let document = try sampleManifest(), initial = await restore.status()
        let ready = try await restore.acceptWorkspaceDocument(document, generation: initial.generation)
        XCTAssertEqual(ready.phase, .review); XCTAssertEqual(ready.source, .workspaceDocument)
        XCTAssertNil(ready.identity); XCTAssertEqual(ready.verifiedPages, 0); XCTAssertEqual(ready.verifiedBytes, 0)
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
            _ = try await restore.commit(reviewToken: review.token, generation: state.generation, sourceAndContentsConfirmed: true)
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
        for sequence in 2...(try sender.sourcePartCount(page: 0)) {
            state = try await restore.observe(sender.frame(page: 0, sequence: UInt32(sequence), using: crypto), generation: state.generation)
        }
        state = try await restore.observe(sender.frame(page: 1, sequence: 1, using: crypto), generation: state.generation)
        XCTAssertEqual(state.verifiedPages, 1); XCTAssertEqual(state.verifiedBytes, FTLimit.page)
        XCTAssertEqual(state.independentParts, 1); XCTAssertEqual(state.incompletePageIndices, [1])
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
}
