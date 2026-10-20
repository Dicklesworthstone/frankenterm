import Foundation
import XCTest
@testable import FrankenTermKit

/// Fault-injected side-effect boundaries, not a second import implementation.
/// All accesses are serialized by this lock, including test fault configuration.
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

final class StoreTestVault: WorkspaceCredentialVault, @unchecked Sendable {
    enum Fault { case beforeInsert, afterInsert, remove }
    private let lock = NSLock()
    private var items: [String: (Data, String)] = [:]
    private var fault: Fault?
    private var insertions = 0
    var count: Int { lock.lock(); defer { lock.unlock() }; return items.count }
    var creates: Int { lock.lock(); defer { lock.unlock() }; return insertions }
    func configure(_ value: Fault?) { lock.lock(); defer { lock.unlock() }; fault = value }
    func create(account: String, privateContainer: Data, fingerprint: String) throws {
        lock.lock(); defer { lock.unlock() }
        if case .beforeInsert = fault { fault = nil; throw FTError.storage }
        guard items[account] == nil else { throw FTError.storage }
        items[account] = (privateContainer, fingerprint); insertions += 1
        if case .afterInsert = fault { fault = nil; throw FTError.storage }
    }
    func fingerprint(account: String) throws -> String? {
        lock.lock(); defer { lock.unlock() }; return items[account]?.1
    }
    func remove(account: String) throws {
        lock.lock(); defer { lock.unlock() }
        if case .remove = fault { fault = nil; throw FTError.storage }
        items[account] = nil
    }
}

@MainActor
final class WorkspaceStoreTests: XCTestCase {
    let crypto = TestCryptography()
    let digest = Data(repeating: 7, count: 32)
    let transferID = Data(repeating: 1, count: 16)
    func archive(keys: Bool = true, bundleID: String = "bundle", credentialFingerprint: String? = nil) throws -> AirGapArchive {
        let (key, fingerprint) = try sampleKey(using: crypto)
        var manifest = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest(fingerprint: credentialFingerprint ?? fingerprint)) as? [String: Any])
        manifest["bundle_id"] = bundleID
        return try AirGapArchive(workspace: WorkspaceDocument(data: JSONSerialization.data(withJSONObject: manifest, options: [.sortedKeys])),
                                 keys: keys ? [key] : [], using: crypto)
    }
    func testImportAndRelaunchKeepKeysOutOfCatalogAndGateConnections() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), archive = try archive()
        let receipt = try await store.install(archive, envelopeDigest: digest, transferID: transferID, expectedRevision: nil, sourceAndContentsConfirmed: true)
        XCTAssertTrue(receipt.workspace.connectLater)
        XCTAssertTrue(receipt.missingCredentialIDs.isEmpty)
        XCTAssertEqual(vault.count, 1)
        XCTAssertNil(persistence.bytes(.journal))
        let disk = try XCTUnwrap(persistence.bytes(.catalog))
        XCTAssertNil(disk.range(of: archive.keys[0].container))
        XCTAssertNil(disk.range(of: Data(archive.keys[0].container.base64EncodedString().utf8)))
        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        let saved = try await reopened.load()
        XCTAssertEqual(saved.count, 1)
        XCTAssertEqual(saved[0].manifest, archive.workspace.raw)
        XCTAssertTrue(try saved[0].document().machines.allSatisfy(\.autoConnect), "preserve preferences without executing them")
        let available = try await reopened.availableCredentialIDs(workspaceID: saved[0].id)
        XCTAssertEqual(available, ["key"])
    }
    func testReviewAndRevisionFencesPrecedeAllSideEffects() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), archive = try archive()
        do {
            _ = try await store.install(archive, envelopeDigest: digest, transferID: transferID, expectedRevision: nil, sourceAndContentsConfirmed: false)
            XCTFail("unreviewed import")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .reviewRequired) }
        XCTAssertNil(persistence.bytes(.journal)); XCTAssertEqual(vault.count, 0)
        let first = try await store.install(archive, envelopeDigest: digest, transferID: transferID, expectedRevision: nil, sourceAndContentsConfirmed: true)
        do {
            _ = try await store.install(archive, envelopeDigest: digest, transferID: transferID, expectedRevision: nil, sourceAndContentsConfirmed: true)
            XCTFail("stale preview")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
        let repeated = try await store.install(archive, envelopeDigest: digest, transferID: transferID, expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        XCTAssertEqual(repeated.workspace.revision, first.workspace.revision)
        XCTAssertEqual(vault.creates, 1)
    }
    func testSameBackupCanRestoreAMissingKeyWithoutDuplicatingLiveKeys() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), archive = try archive()
        let first = try await store.install(archive, envelopeDigest: digest, transferID: transferID, expectedRevision: nil, sourceAndContentsConfirmed: true)
        try vault.remove(account: first.workspace.bindings[0].account)
        let second = try await store.install(archive, envelopeDigest: digest, transferID: transferID, expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        XCTAssertEqual(second.workspace.id, first.workspace.id)
        XCTAssertNotEqual(second.workspace.revision, first.workspace.revision)
        XCTAssertTrue(second.missingCredentialIDs.isEmpty); XCTAssertEqual(vault.count, 1)
    }
    func testEveryJournalAndCatalogWriteBoundaryRecoversOldOrNew() async throws {
        for (fault, published) in [(StoreTestPersistence.Fault.before(.journal), false), (.after(.journal), false),
                                    (.before(.catalog), false), (.after(.catalog), true)] {
            let persistence = StoreTestPersistence(), vault = StoreTestVault()
            persistence.configure(fault)
            let store = WorkspaceStore(persistence: persistence, vault: vault)
            do {
                _ = try await store.install(archive(), envelopeDigest: digest, transferID: transferID, expectedRevision: nil, sourceAndContentsConfirmed: true)
                XCTFail("fault was not exercised")
            } catch {}
            let reopened = WorkspaceStore(persistence: persistence, vault: vault)
            let saved = try await reopened.load()
            XCTAssertEqual(saved.count, published ? 1 : 0)
            XCTAssertEqual(vault.count, published ? 1 : 0)
            XCTAssertNil(persistence.bytes(.journal))
        }
    }
    func testInsertFailureBeforeAndAfterEffectCleansOnlyOwnedItems() async throws {
        for fault in [StoreTestVault.Fault.beforeInsert, .afterInsert] {
            let persistence = StoreTestPersistence(), vault = StoreTestVault()
            try vault.create(account: "unrelated", privateContainer: Data([99]), fingerprint: "unrelated")
            vault.configure(fault)
            let store = WorkspaceStore(persistence: persistence, vault: vault)
            do {
                _ = try await store.install(archive(), envelopeDigest: digest, transferID: transferID, expectedRevision: nil, sourceAndContentsConfirmed: true)
                XCTFail("insertion fault not exercised")
            } catch {}
            let saved = try await store.load()
            XCTAssertTrue(saved.isEmpty); XCTAssertEqual(vault.count, 1)
            XCTAssertEqual(try vault.fingerprint(account: "unrelated"), "unrelated")
        }
    }
    func testFailedPostCommitCleanupDoesNotTurnSuccessIntoRetry() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        persistence.configure(.cleanup)
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let first = try await store.install(archive(), envelopeDigest: digest, transferID: transferID, expectedRevision: nil, sourceAndContentsConfirmed: true)
        XCTAssertTrue(first.cleanupPending); XCTAssertEqual(vault.count, 1)
        XCTAssertNotNil(persistence.bytes(.journal))
        let saved = try await store.load()
        XCTAssertEqual(saved.first?.revision, first.workspace.revision)
        XCTAssertEqual(vault.count, 1); XCTAssertNil(persistence.bytes(.journal))
    }
    func testCorruptCatalogNeverAuthorizesJournalKeyDeletion() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let transaction = UUID(), account = "ft-import:\(transaction.uuidString):0"
        try vault.create(account: account, privateContainer: Data([1]), fingerprint: "preserve")
        persistence.seed(Data("not a catalog".utf8), slot: .catalog)
        persistence.seed(try JSONSerialization.data(withJSONObject: ["version": 1, "transaction": transaction.uuidString,
            "targetRevision": UUID().uuidString, "newAccounts": [account], "retiredAccounts": []]), slot: .journal)
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        do { _ = try await store.load(); XCTFail("corrupt catalog accepted") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .corruptStore) }
        XCTAssertEqual(vault.count, 1); XCTAssertNotNil(persistence.bytes(.journal))
    }
    func testJournalCannotDeleteArbitraryVaultAccounts() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        try vault.create(account: "some-other-application", privateContainer: Data([1]), fingerprint: "preserve")
        persistence.seed(try JSONSerialization.data(withJSONObject: ["version": 1, "transaction": UUID().uuidString,
            "targetRevision": UUID().uuidString, "newAccounts": ["some-other-application"], "retiredAccounts": []]), slot: .journal)
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        do { _ = try await store.load(); XCTFail("malformed ownership accepted") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .corruptStore) }
        XCTAssertEqual(vault.count, 1)
    }
    func testRevocationCannotBeUndoneByReimportingOldVideo() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), archive = try archive()
        let first = try await store.install(archive, envelopeDigest: digest, transferID: transferID, expectedRevision: nil, sourceAndContentsConfirmed: true)
        try await store.revokeLocally(fingerprint: archive.workspace.credentials[0].fingerprint)
        let available = try await store.availableCredentialIDs(workspaceID: first.workspace.id)
        XCTAssertTrue(available.isEmpty)
        do {
            _ = try await store.install(archive, envelopeDigest: digest, transferID: transferID, expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
            XCTFail("revoked key resurrected")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .revokedIdentity) }
        XCTAssertEqual(vault.creates, 1)
    }
    func testManifestOnlyRestoreReportsMissingKeys() async throws {
        let store = WorkspaceStore(persistence: StoreTestPersistence(), vault: StoreTestVault())
        let receipt = try await store.install(archive(keys: false), envelopeDigest: digest, transferID: transferID, expectedRevision: nil, sourceAndContentsConfirmed: true)
        XCTAssertEqual(receipt.missingCredentialIDs, ["key"])
        XCTAssertTrue(receipt.workspace.bindings.isEmpty)
    }
    func testUnreferencedOldKeyIsRetiredOnlyAfterReplacementPublishes() async throws {
        for (fault, oldSurvives) in [(StoreTestPersistence.Fault.before(.catalog), true), (.after(.catalog), false)] {
            let persistence = StoreTestPersistence(), vault = StoreTestVault()
            let store = WorkspaceStore(persistence: persistence, vault: vault)
            let first = try await store.install(archive(), envelopeDigest: digest, transferID: transferID, expectedRevision: nil, sourceAndContentsConfirmed: true)
            let replacement = try archive(keys: false, bundleID: "replacement", credentialFingerprint: "SHA256:" + String(repeating: "A", count: 43))
            persistence.configure(fault)
            do {
                _ = try await store.install(replacement, envelopeDigest: Data(repeating: 8, count: 32),
                                            transferID: Data(repeating: 2, count: 16),
                                            expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
                XCTFail("write fault not exercised")
            } catch {}
            let saved = try await store.load()
            XCTAssertEqual(saved.count, 1)
            XCTAssertEqual(vault.count, oldSurvives ? 1 : 0)
        }
    }
    func testRealFilePersistenceLockBoundsAndRelaunch() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("ft-store-test-" + UUID().uuidString)
        let disk = try FileWorkspacePersistence(directory: directory), other = try FileWorkspacePersistence(directory: directory)
        try disk.withExclusiveAccess {
            XCTAssertThrowsError(try other.withExclusiveAccess { XCTFail("overlapping file leases") })
        }
        let vault = StoreTestVault(), store = WorkspaceStore(persistence: disk, vault: vault)
        let receipt = try await store.install(archive(), envelopeDigest: digest, transferID: transferID, expectedRevision: nil, sourceAndContentsConfirmed: true)
        let reopened = WorkspaceStore(persistence: other, vault: vault)
        let saved = try await reopened.load()
        XCTAssertEqual(saved.first?.revision, receipt.workspace.revision)
        XCTAssertThrowsError(try disk.read(.catalog, limit: 10))
    }
    func testFailedDurabilityReaffirmationKeepsOldKeysUntilRecovery() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let first = try await store.install(archive(), envelopeDigest: digest, transferID: transferID, expectedRevision: nil, sourceAndContentsConfirmed: true)
        let replacement = try archive(keys: false, bundleID: "replacement", credentialFingerprint: "SHA256:" + String(repeating: "A", count: 43))
        persistence.configureWriteAndRecoveryFailure()
        do {
            _ = try await store.install(replacement, envelopeDigest: Data(repeating: 9, count: 32),
                                        transferID: Data(repeating: 3, count: 16),
                                        expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
            XCTFail("failed durability reaffirmation was ignored")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .recoveryRequired) }
        XCTAssertEqual(vault.count, 1, "readable new bytes do not authorize deleting the old key")
        XCTAssertNotNil(persistence.bytes(.journal))
        let recovered = try await store.load()
        XCTAssertEqual(recovered.count, 1)
        XCTAssertEqual(recovered[0].manifest, replacement.workspace.raw)
        XCTAssertEqual(vault.count, 0)
        XCTAssertNil(persistence.bytes(.journal))
    }

    func testBundleAndTransferBindingsSurviveReplacementAndRelaunch() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let original = try archive(keys: false)
        let first = try await store.install(original, envelopeDigest: digest, transferID: transferID,
                                            expectedRevision: nil, sourceAndContentsConfirmed: true)
        let replacement = try archive(keys: false, bundleID: "next-bundle")
        let nextDigest = Data(repeating: 8, count: 32), nextTransfer = Data(repeating: 2, count: 16)
        let second = try await store.install(replacement, envelopeDigest: nextDigest, transferID: nextTransfer,
                                             expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        let before = persistence.bytes(.catalog)
        // Both attacks supply key material which would be staged if admission
        // forgot the first export after replacement. Neither may touch the vault.
        for (candidate, candidateDigest, candidateTransfer) in [
            (try archive(), Data(repeating: 9, count: 32), Data(repeating: 3, count: 16)),
            (try archive(bundleID: "another-bundle"), Data(repeating: 9, count: 32), transferID),
            (try archive(), digest, Data(repeating: 3, count: 16))
        ] {
            do {
                _ = try await reopened.install(candidate, envelopeDigest: candidateDigest, transferID: candidateTransfer,
                    expectedRevision: second.workspace.revision, sourceAndContentsConfirmed: true, allowHistoricalRollback: true)
                XCTFail("accepted identifier rebinding")
            } catch { XCTAssertEqual(error as? WorkspaceStoreError, .conflictingExport) }
            XCTAssertEqual(vault.creates, 0)
            XCTAssertNil(persistence.bytes(.journal))
            XCTAssertEqual(persistence.bytes(.catalog), before)
        }
        let assessment = try await reopened.assessImport(original.workspace, envelopeDigest: digest, transferID: transferID)
        XCTAssertTrue(assessment.requiresRollbackApproval)
        XCTAssertEqual(assessment.firstAcceptedRevision, first.workspace.revision)
        XCTAssertEqual(assessment.existingWorkspace?.revision, second.workspace.revision)
    }

    func testHistoricalRecordingNeedsSeparateApprovalAndThenBecomesIdempotent() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let original = try archive(), replacement = try archive(bundleID: "next-bundle")
        let first = try await store.install(original, envelopeDigest: digest, transferID: transferID,
                                            expectedRevision: nil, sourceAndContentsConfirmed: true)
        let second = try await store.install(replacement, envelopeDigest: Data(repeating: 8, count: 32),
            transferID: Data(repeating: 2, count: 16), expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        let before = persistence.bytes(.catalog)
        do {
            _ = try await store.install(original, envelopeDigest: digest, transferID: transferID,
                expectedRevision: second.workspace.revision, sourceAndContentsConfirmed: true, allowMachineRemoval: true)
            XCTFail("machine-removal consent authorized historical rollback")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .rollbackNeedsApproval) }
        XCTAssertEqual(persistence.bytes(.catalog), before)
        let restored = try await store.install(original, envelopeDigest: digest, transferID: transferID,
            expectedRevision: second.workspace.revision, sourceAndContentsConfirmed: true, allowHistoricalRollback: true)
        XCTAssertEqual(restored.workspace.id, first.workspace.id)
        XCTAssertNotEqual(restored.workspace.revision, first.workspace.revision)
        XCTAssertNotEqual(restored.workspace.revision, second.workspace.revision)
        let current = try await store.assessImport(original.workspace, envelopeDigest: digest, transferID: transferID)
        XCTAssertFalse(current.requiresRollbackApproval)
        XCTAssertEqual(current.firstAcceptedRevision, first.workspace.revision)
        let repeated = try await store.install(original, envelopeDigest: digest, transferID: transferID,
            expectedRevision: restored.workspace.revision, sourceAndContentsConfirmed: true)
        XCTAssertEqual(repeated.workspace.revision, restored.workspace.revision)
        XCTAssertEqual(vault.creates, 1)
        try await store.revokeLocally(fingerprint: original.workspace.credentials[0].fingerprint)
        do {
            _ = try await store.install(replacement, envelopeDigest: Data(repeating: 8, count: 32),
                transferID: Data(repeating: 2, count: 16), expectedRevision: restored.workspace.revision,
                sourceAndContentsConfirmed: true, allowHistoricalRollback: true)
            XCTFail("rollback consent bypassed local revocation")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .revokedIdentity) }
        XCTAssertEqual(vault.creates, 1)
    }

    func testReplayHistoryAndWorkspacePublishAtTheSameFaultBoundary() async throws {
        for (fault, published) in [(StoreTestPersistence.Fault.before(.catalog), false), (.after(.catalog), true)] {
            let persistence = StoreTestPersistence(), vault = StoreTestVault()
            let store = WorkspaceStore(persistence: persistence, vault: vault)
            let original = try archive(keys: false), replacement = try archive(keys: false, bundleID: "next-bundle")
            let first = try await store.install(original, envelopeDigest: digest, transferID: transferID,
                expectedRevision: nil, sourceAndContentsConfirmed: true)
            persistence.configure(fault)
            do {
                _ = try await store.install(replacement, envelopeDigest: Data(repeating: 8, count: 32),
                    transferID: Data(repeating: 2, count: 16), expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
                XCTFail("publication fault was not exercised")
            } catch {}
            let reopened = WorkspaceStore(persistence: persistence, vault: vault)
            let saved = try await reopened.load()
            let fields = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(persistence.bytes(.catalog))) as? [String: Any])
            let accepted = try XCTUnwrap(fields["acceptedImports"] as? [[String: Any]])
            XCTAssertEqual(accepted.count, published ? 2 : 1)
            XCTAssertEqual(try saved[0].document().bundleID, published ? "next-bundle" : "bundle")
            let assessment = try await reopened.assessImport(replacement.workspace, envelopeDigest: Data(repeating: 8, count: 32),
                                                             transferID: Data(repeating: 2, count: 16))
            XCTAssertEqual(assessment.firstAcceptedRevision != nil, published)
            XCTAssertFalse(assessment.requiresRollbackApproval)
        }
    }

    func testLegacyCatalogPreservesKnownBundleBindingAndLearnsTransferOnExactRescan() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive()
        let first = try await store.install(original, envelopeDigest: digest, transferID: transferID,
                                            expectedRevision: nil, sourceAndContentsConfirmed: true)
        var legacy = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(persistence.bytes(.catalog))) as? [String: Any])
        legacy["version"] = 1; legacy.removeValue(forKey: "acceptedImports")
        persistence.seed(try JSONSerialization.data(withJSONObject: legacy), slot: .catalog)
        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        let loaded = try await reopened.load()
        XCTAssertEqual(loaded.first?.revision, first.workspace.revision)
        do {
            _ = try await reopened.install(original, envelopeDigest: Data(repeating: 8, count: 32), transferID: Data(repeating: 2, count: 16),
                expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
            XCTFail("migration forgot the saved bundle binding")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .conflictingExport) }
        let repeated = try await reopened.install(original, envelopeDigest: digest, transferID: transferID,
            expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        XCTAssertEqual(repeated.workspace.revision, first.workspace.revision)
        XCTAssertEqual(vault.creates, 1)
        let upgraded = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(persistence.bytes(.catalog))) as? [String: Any])
        XCTAssertEqual(upgraded["version"] as? Int, 2)
        let accepted = try XCTUnwrap((upgraded["acceptedImports"] as? [[String: Any]])?.first)
        XCTAssertEqual(accepted["transferID"] as? String, transferID.base64EncodedString())
        XCTAssertEqual(accepted["envelopeDigest"] as? String, digest.base64EncodedString())
    }

    func testFullHistoryRefusesNewExportsWithoutEvictingPreviouslyAcceptedIdentities() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive(keys: false)
        let first = try await store.install(original, envelopeDigest: digest, transferID: transferID,
                                            expectedRevision: nil, sourceAndContentsConfirmed: true)
        var full = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(persistence.bytes(.catalog))) as? [String: Any])
        var accepted = try XCTUnwrap(full["acceptedImports"] as? [[String: Any]])
        for index in 1..<WorkspaceStore.importHistoryLimit {
            var otherDigest = Data(repeating: 0, count: 24), otherTransfer = Data(repeating: 0, count: 8)
            otherDigest.appendBE(UInt64(index), bytes: 8); otherTransfer.appendBE(UInt64(index), bytes: 8)
            accepted.append(["exporterID": "mac", "workspaceID": "work", "bundleID": "history-\(index)",
                "transferID": otherTransfer.base64EncodedString(), "envelopeDigest": otherDigest.base64EncodedString(),
                "firstAcceptedRevision": UUID().uuidString])
        }
        full["acceptedImports"] = accepted
        persistence.seed(try JSONSerialization.data(withJSONObject: full), slot: .catalog)
        let before = persistence.bytes(.catalog)
        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        do {
            _ = try await reopened.install(archive(bundleID: "new-bundle"), envelopeDigest: Data(repeating: 9, count: 32),
                transferID: Data(repeating: 3, count: 16), expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
            XCTFail("full history was silently evicted")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .importHistoryFull) }
        XCTAssertEqual(vault.creates, 0); XCTAssertNil(persistence.bytes(.journal))
        XCTAssertEqual(persistence.bytes(.catalog), before)
        let repeated = try await reopened.install(original, envelopeDigest: digest, transferID: transferID,
            expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        XCTAssertEqual(repeated.workspace.revision, first.workspace.revision)
    }

    func testCorruptReplayBindingsFailBeforePendingKeyCleanup() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive()
        let first = try await store.install(original, envelopeDigest: digest, transferID: transferID,
                                            expectedRevision: nil, sourceAndContentsConfirmed: true)
        var corrupt = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(persistence.bytes(.catalog))) as? [String: Any])
        var accepted = try XCTUnwrap(corrupt["acceptedImports"] as? [[String: Any]])
        accepted[0]["envelopeDigest"] = Data(repeating: 8, count: 32).base64EncodedString()
        corrupt["acceptedImports"] = accepted
        persistence.seed(try JSONSerialization.data(withJSONObject: corrupt), slot: .catalog)
        let transaction = UUID(), account = "ft-import:\(transaction.uuidString):0"
        try vault.create(account: account, privateContainer: Data([99]), fingerprint: "keep-pending")
        persistence.seed(try JSONSerialization.data(withJSONObject: ["version": 1, "transaction": transaction.uuidString,
            "targetRevision": first.workspace.revision.uuidString, "newAccounts": [account], "retiredAccounts": []]), slot: .journal)
        do { _ = try await store.load(); XCTFail("current revision has no matching accepted envelope") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .corruptStore) }
        XCTAssertEqual(vault.count, 2)
        XCTAssertNotNil(persistence.bytes(.journal))
    }
}
