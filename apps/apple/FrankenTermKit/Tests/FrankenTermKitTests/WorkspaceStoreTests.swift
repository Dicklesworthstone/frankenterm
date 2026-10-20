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
    func archive(keys: Bool = true, bundleID: String? = nil) throws -> AirGapArchive {
        let (key, fingerprint) = try sampleKey(using: crypto)
        var document = try WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint))
        if let bundleID { document = try document.reissuingBundle(id: bundleID) }
        return try AirGapArchive(workspace: document, keys: keys ? [key] : [], using: crypto)
    }
    /// The catalog shape published before host-revocation history was added.
    /// Keep these fields explicit so migration is tested against that real format.
    func versionTwoCatalog(workspaces: [InstalledWorkspace], revokedFingerprints: [String] = [],
                           acceptedImports: [[String: Any]]) throws -> Data {
        let saved = try JSONSerialization.jsonObject(with: JSONEncoder().encode(workspaces))
        return try JSONSerialization.data(withJSONObject: ["version": 2, "workspaces": saved,
            "revokedFingerprints": revokedFingerprints, "acceptedImports": acceptedImports])
    }
    func versionTwoAcceptance(_ document: WorkspaceDocument, digest: Data, transferID: Data?, revision: UUID) -> [String: Any] {
        var fields: [String: Any] = ["exporterID": document.exporterID, "workspaceID": document.id,
            "bundleID": document.bundleID, "envelopeDigest": digest.base64EncodedString(), "firstAcceptedRevision": revision.uuidString]
        if let transferID { fields["transferID"] = transferID.base64EncodedString() }
        return fields
    }
    func replacingMetadata(of saved: InstalledWorkspace, annotations: [InstalledWorkspace.InactiveSetting],
                           findings: [InstalledWorkspace.PortabilityFinding] = []) -> InstalledWorkspace {
        .init(id: saved.id, revision: saved.revision, manifest: saved.manifest, envelopeDigest: saved.envelopeDigest,
              bindings: saved.bindings, inactiveSettings: annotations, findings: findings, connectLater: saved.connectLater)
    }
    func testImportAndRelaunchKeepKeysOutOfCatalogAndGateConnections() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), archive = try archive()
        let receipt = try await store.install(archive, envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
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
            _ = try await store.install(archive, envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: false)
            XCTFail("unreviewed import")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .reviewRequired) }
        XCTAssertNil(persistence.bytes(.journal)); XCTAssertEqual(vault.count, 0)
        let first = try await store.install(archive, envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
        do {
            _ = try await store.install(archive, envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
            XCTFail("stale preview")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
        let repeated = try await store.install(archive, envelopeDigest: digest, expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        XCTAssertEqual(repeated.workspace.revision, first.workspace.revision)
        XCTAssertEqual(vault.creates, 1)
    }
    func testSameBackupCanRestoreAMissingKeyWithoutDuplicatingLiveKeys() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), archive = try archive()
        let first = try await store.install(archive, envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
        try vault.remove(account: first.workspace.bindings[0].account)
        let second = try await store.install(archive, envelopeDigest: digest, expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
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
                _ = try await store.install(archive(), envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
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
                _ = try await store.install(archive(), envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
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
        let first = try await store.install(archive(), envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
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
        let first = try await store.install(archive, envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
        try await store.revokeLocally(fingerprint: archive.workspace.credentials[0].fingerprint)
        let available = try await store.availableCredentialIDs(workspaceID: first.workspace.id)
        XCTAssertTrue(available.isEmpty)
        do {
            _ = try await store.install(archive, envelopeDigest: digest, expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
            XCTFail("revoked key resurrected")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .revokedIdentity) }
        XCTAssertEqual(vault.creates, 1)
    }
    func testManifestOnlyRestoreReportsMissingKeys() async throws {
        let store = WorkspaceStore(persistence: StoreTestPersistence(), vault: StoreTestVault())
        let receipt = try await store.install(archive(keys: false), envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
        XCTAssertEqual(receipt.missingCredentialIDs, ["key"])
        XCTAssertTrue(receipt.workspace.bindings.isEmpty)
    }
    func testUnreferencedOldKeyIsRetiredOnlyAfterReplacementPublishes() async throws {
        for (fault, oldSurvives) in [(StoreTestPersistence.Fault.before(.catalog), true), (.after(.catalog), false)] {
            let persistence = StoreTestPersistence(), vault = StoreTestVault()
            let store = WorkspaceStore(persistence: persistence, vault: vault)
            let first = try await store.install(archive(), envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
            let replacement = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()).reissuingBundle(id: "replacement"), keys: [], using: crypto)
            persistence.configure(fault)
            do {
                _ = try await store.install(replacement, envelopeDigest: Data(repeating: 8, count: 32),
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
        let receipt = try await store.install(archive(), envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
        let reopened = WorkspaceStore(persistence: other, vault: vault)
        let saved = try await reopened.load()
        XCTAssertEqual(saved.first?.revision, receipt.workspace.revision)
        XCTAssertThrowsError(try disk.read(.catalog, limit: 10))
    }
    func testFailedDurabilityReaffirmationKeepsOldKeysUntilRecovery() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let first = try await store.install(archive(), envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
        let replacement = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()).reissuingBundle(id: "replacement"), keys: [], using: crypto)
        persistence.configureWriteAndRecoveryFailure()
        do {
            _ = try await store.install(replacement, envelopeDigest: Data(repeating: 9, count: 32),
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

    func testImmutableBundleAndTransferConflictsPrecedeKeyStaging() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive()
        let transfer = Data(repeating: 12, count: 16)
        let first = try await store.install(original, envelopeDigest: digest, expectedRevision: nil,
                                            sourceAndContentsConfirmed: true, transferID: transfer)
        let before = persistence.bytes(.catalog)
        let otherDigest = Data(repeating: 8, count: 32)
        do {
            _ = try await store.preview(original.workspace, envelopeDigest: otherDigest)
            XCTFail("same bundle ID accepted with different bytes")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .immutableExportConflict) }
        let changed = try AirGapArchive(workspace: original.workspace.reissuingBundle(id: "changed"), keys: original.keys, using: crypto)
        do {
            _ = try await store.install(changed, envelopeDigest: otherDigest, expectedRevision: first.workspace.revision,
                                        sourceAndContentsConfirmed: true, transferID: transfer)
            XCTFail("same authenticated transfer ID rebound to a new export")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .immutableExportConflict) }
        XCTAssertEqual(persistence.bytes(.catalog), before)
        XCTAssertNil(persistence.bytes(.journal)); XCTAssertEqual(vault.creates, 1)
    }

    func testOlderBundleRemainsKnownAfterRelaunchAndRequiresRollbackReview() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive()
        let first = try await store.install(original, envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
        let changed = try AirGapArchive(workspace: original.workspace.reissuingBundle(id: "newer"), keys: original.keys, using: crypto)
        let newer = try await store.install(changed, envelopeDigest: Data(repeating: 9, count: 32),
                                            expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        let preview = try await reopened.preview(original.workspace, envelopeDigest: digest)
        XCTAssertTrue(preview.isRollback); XCTAssertEqual(preview.expectedRevision, newer.workspace.revision)
        do {
            _ = try await reopened.install(original, envelopeDigest: digest, expectedRevision: newer.workspace.revision, sourceAndContentsConfirmed: true)
            XCTFail("rollback bypassed review")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .rollbackNeedsApproval) }
        do {
            _ = try await reopened.preview(original.workspace, envelopeDigest: Data(repeating: 10, count: 32))
            XCTFail("historical bundle ID was forgotten")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .immutableExportConflict) }
        let restored = try await reopened.install(original, envelopeDigest: digest, expectedRevision: newer.workspace.revision,
                                                  sourceAndContentsConfirmed: true, allowRollback: true)
        XCTAssertEqual(restored.workspace.id, first.workspace.id)
        XCTAssertEqual(restored.workspace.manifest, original.workspace.raw)
        XCTAssertEqual(vault.creates, 1, "reused protected identity must survive both revisions")
        XCTAssertTrue(restored.workspace.connectLater)
    }

    func testMissingTransferIdentityIsDurablyBoundWithoutDuplicatingKeys() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive()
        let first = try await store.install(original, envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
        let transfer = Data(repeating: 17, count: 16)
        let repeated = try await store.install(original, envelopeDigest: digest, expectedRevision: first.workspace.revision,
                                               sourceAndContentsConfirmed: true, transferID: transfer)
        XCTAssertEqual(repeated.workspace.id, first.workspace.id)
        XCTAssertEqual(vault.creates, 1); XCTAssertNil(persistence.bytes(.journal))
        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        let recovered = try await reopened.recoverImport(exporterID: original.workspace.exporterID,
            workspaceID: original.workspace.id, envelopeDigest: digest, transferID: transfer)
        XCTAssertEqual(recovered?.workspace.revision, repeated.workspace.revision)
        do {
            _ = try await reopened.preview(original.workspace, envelopeDigest: digest, transferID: Data(repeating: 18, count: 16))
            XCTFail("migrated transfer identity was not reserved")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .immutableExportConflict) }
    }

    func testTransferIdentityMigrationRecoversActualPublicationAtEveryWriteBoundary() async throws {
        for (fault, published) in [(StoreTestPersistence.Fault.before(.journal), false), (.after(.journal), false),
                                    (.before(.catalog), false), (.after(.catalog), true)] {
            let persistence = StoreTestPersistence(), vault = StoreTestVault()
            let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive()
            let first = try await store.install(original, envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
            let transfer = Data(repeating: 19, count: 16)
            persistence.configure(fault)
            do {
                _ = try await store.install(original, envelopeDigest: digest, expectedRevision: first.workspace.revision,
                                            sourceAndContentsConfirmed: true, transferID: transfer)
                XCTFail("migration write fault was not exercised")
            } catch {}
            let reopened = WorkspaceStore(persistence: persistence, vault: vault)
            let recovered = try await reopened.recoverImport(exporterID: original.workspace.exporterID,
                workspaceID: original.workspace.id, envelopeDigest: digest, transferID: transfer)
            XCTAssertEqual(recovered != nil, published)
            let installed = try await reopened.load()
            XCTAssertEqual(installed.count, 1); XCTAssertEqual(vault.creates, 1)
            XCTAssertNil(persistence.bytes(.journal))
            if !published { XCTAssertEqual(installed.first?.revision, first.workspace.revision) }
        }
    }

    func testLocalHostRejectionSurvivesNewExportAndApprovedRollback() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive()
        let first = try await store.install(original, envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
        let trust = try XCTUnwrap(original.workspace.hostTrust.first), key = try XCTUnwrap(trust.pins.first)
        try await store.revokeHostLocally(workspaceID: first.workspace.id, trustID: trust.id, key: key)
        let changed = try AirGapArchive(workspace: original.workspace.reissuingBundle(id: "cosmetic-update"), keys: [], using: crypto)
        let next = try await store.install(changed, envelopeDigest: Data(repeating: 9, count: 32),
                                           expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        let preview = try await reopened.preview(original.workspace, envelopeDigest: digest)
        XCTAssertEqual(preview.preservedHostRevocationCount, 1)
        let restored = try await reopened.install(original, envelopeDigest: digest, expectedRevision: next.workspace.revision,
                                                  sourceAndContentsConfirmed: true, allowRollback: true)
        let revocations = try await reopened.revokedHostKeys(workspaceID: restored.workspace.id)
        XCTAssertEqual(revocations[trust.canonicalTrustScope], [key])
    }

    func testAcceptedNegativeTrustCannotDisappearWhenIncomingListIsEmpty() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive()
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: original.workspace.raw) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        var trust = try XCTUnwrap(workspace["host_trust"] as? [[String: Any]])
        trust[0]["revoked_keys"] = trust[0]["pins"]
        workspace["host_trust"] = trust; root["workspace"] = workspace
        let denied = try AirGapArchive(workspace: WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root)), keys: [], using: crypto)
        let first = try await store.install(denied, envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
        let changed = try AirGapArchive(workspace: original.workspace.reissuingBundle(id: "empty-revocations"), keys: [], using: crypto)
        let next = try await store.install(changed, envelopeDigest: Data(repeating: 8, count: 32),
                                           expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        let revoked = try await store.revokedHostKeys(workspaceID: next.workspace.id)
        let scope = original.workspace.hostTrust[0].canonicalTrustScope
        XCTAssertEqual(revoked[scope], Set(original.workspace.hostTrust[0].pins))
    }

    func testVersionOneCatalogMigratesSingleCandidateWithoutWeakeningPolicy() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive()
        let first = try await store.install(original, envelopeDigest: digest, expectedRevision: nil, sourceAndContentsConfirmed: true)
        var legacyManifest = try XCTUnwrap(JSONSerialization.jsonObject(with: original.workspace.raw) as? [String: Any])
        var workspace = try XCTUnwrap(legacyManifest["workspace"] as? [String: Any])
        var routes = try XCTUnwrap(workspace["routes"] as? [[String: Any]])
        routes[0].removeValue(forKey: "credential_refs"); routes[0].removeValue(forKey: "authentication_methods")
        routes[0]["credential_ref"] = "key"; workspace["routes"] = routes; legacyManifest["workspace"] = workspace
        var catalog = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(persistence.bytes(.catalog))) as? [String: Any])
        var saved = try XCTUnwrap(catalog["workspaces"] as? [[String: Any]])
        saved[0]["manifest"] = try JSONSerialization.data(withJSONObject: legacyManifest).base64EncodedString()
        catalog["workspaces"] = saved; catalog["version"] = 1
        catalog.removeValue(forKey: "acceptedImports"); catalog.removeValue(forKey: "hostRevocations")
        catalog.removeValue(forKey: "hostRequirements"); catalog.removeValue(forKey: "scopedHostRevocations")
        persistence.seed(try JSONSerialization.data(withJSONObject: catalog), slot: .catalog)
        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        let migrated = try await reopened.load()
        XCTAssertEqual(migrated.first?.revision, first.workspace.revision)
        let document = try XCTUnwrap(migrated.first).document()
        XCTAssertEqual(document.routes[0].credentialIDs, ["key"])
        XCTAssertEqual(document.routes[0].authenticationMethods.map(\.rawValue), ["publickey"])
        let available = try await reopened.availableCredentialIDs(workspaceID: first.workspace.id)
        XCTAssertEqual(available, ["key"]); XCTAssertEqual(vault.creates, 1)
        let preview = try await reopened.preview(document, envelopeDigest: digest)
        XCTAssertFalse(preview.isRollback)
    }

    func testBundleAndTransferBindingsSurviveReplacementAndRelaunch() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive(keys: false)
        let first = try await store.install(original, envelopeDigest: digest, transferID: transferID,
                                            expectedRevision: nil, sourceAndContentsConfirmed: true)
        let second = try await store.install(archive(keys: false, bundleID: "next-bundle"),
            envelopeDigest: Data(repeating: 8, count: 32), transferID: Data(repeating: 2, count: 16),
            expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        let reopened = WorkspaceStore(persistence: persistence, vault: vault), before = persistence.bytes(.catalog)
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
            XCTAssertEqual(vault.creates, 0); XCTAssertNil(persistence.bytes(.journal))
            XCTAssertEqual(persistence.bytes(.catalog), before)
        }
        let assessment = try await reopened.assessImport(original.workspace, envelopeDigest: digest, transferID: transferID)
        XCTAssertTrue(assessment.requiresRollbackApproval)
        XCTAssertEqual(assessment.firstAcceptedRevision, first.workspace.revision)
        XCTAssertEqual(assessment.existingWorkspace?.revision, second.workspace.revision)
    }

    func testBundleAndEnvelopeBindingsCannotMoveToAnotherWorkspaceOrExporter() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive()
        _ = try await store.install(archive(keys: false), envelopeDigest: digest, transferID: transferID,
                                    expectedRevision: nil, sourceAndContentsConfirmed: true)
        let before = persistence.bytes(.catalog)
        for (exporter, bundle, candidateDigest) in [("mac", "bundle", Data(repeating: 10, count: 32)),
                                                   ("another-mac", "another-bundle", digest)] {
            var fields = try XCTUnwrap(JSONSerialization.jsonObject(with: original.workspace.raw) as? [String: Any])
            var workspace = try XCTUnwrap(fields["workspace"] as? [String: Any])
            workspace["id"] = "another-workspace"; fields["workspace"] = workspace
            fields["source"] = ["exporter_id": exporter]; fields["bundle_id"] = bundle
            let candidate = try AirGapArchive(workspace: WorkspaceDocument(data: JSONSerialization.data(withJSONObject: fields)),
                                              keys: original.keys, using: crypto)
            do {
                _ = try await store.install(candidate, envelopeDigest: candidateDigest, transferID: Data(repeating: 4, count: 16),
                    expectedRevision: nil, sourceAndContentsConfirmed: true)
                XCTFail("immutable package identity moved to a different namespace")
            } catch { XCTAssertEqual(error as? WorkspaceStoreError, .conflictingExport) }
            XCTAssertEqual(vault.creates, 0); XCTAssertNil(persistence.bytes(.journal))
            XCTAssertEqual(persistence.bytes(.catalog), before)
        }
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
            let reopened = WorkspaceStore(persistence: persistence, vault: vault), saved = try await reopened.load()
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
        var legacy = try XCTUnwrap(JSONSerialization.jsonObject(with: versionTwoCatalog(workspaces: [first.workspace], acceptedImports: [])) as? [String: Any])
        legacy["version"] = 1; legacy.removeValue(forKey: "acceptedImports")
        persistence.seed(try JSONSerialization.data(withJSONObject: legacy), slot: .catalog)
        let reopened = WorkspaceStore(persistence: persistence, vault: vault), loaded = try await reopened.load()
        XCTAssertEqual(loaded.first?.revision, first.workspace.revision)
        do {
            _ = try await reopened.install(original, envelopeDigest: Data(repeating: 8, count: 32), transferID: Data(repeating: 2, count: 16),
                expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
            XCTFail("migration forgot the saved bundle binding")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .conflictingExport) }
        let repeated = try await reopened.install(original, envelopeDigest: digest, transferID: transferID,
            expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        XCTAssertEqual(repeated.workspace.id, first.workspace.id)
        XCTAssertEqual(vault.creates, 1); XCTAssertNil(persistence.bytes(.journal))
        let upgraded = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(persistence.bytes(.catalog))) as? [String: Any])
        XCTAssertEqual(upgraded["version"] as? Int, 4)
        let accepted = try XCTUnwrap((upgraded["acceptedImports"] as? [[String: Any]])?.first)
        XCTAssertEqual(accepted["transferID"] as? String, transferID.base64EncodedString())
        XCTAssertEqual(accepted["envelopeDigest"] as? String, digest.base64EncodedString())
        let assessment = try await reopened.assessImport(original.workspace, envelopeDigest: digest, transferID: transferID)
        XCTAssertEqual(assessment.firstAcceptedRevision, first.workspace.revision)
    }

    func testFullHistoryRefusesNewExportsWithoutEvictingPreviouslyAcceptedIdentities() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive(keys: false)
        let first = try await store.install(original, envelopeDigest: digest, transferID: transferID,
                                            expectedRevision: nil, sourceAndContentsConfirmed: true)
        var accepted = [versionTwoAcceptance(original.workspace, digest: digest, transferID: transferID, revision: first.workspace.revision)]
        for index in 1..<WorkspaceStore.importHistoryLimit {
            var otherDigest = Data(repeating: 0, count: 24), otherTransfer = Data(repeating: 0, count: 8)
            otherDigest.appendBE(UInt64(index), bytes: 8); otherTransfer.appendBE(UInt64(index), bytes: 8)
            accepted.append(["exporterID": "mac", "workspaceID": "work", "bundleID": "history-\(index)",
                "transferID": otherTransfer.base64EncodedString(), "envelopeDigest": otherDigest.base64EncodedString(),
                "firstAcceptedRevision": UUID().uuidString])
        }
        persistence.seed(try versionTwoCatalog(workspaces: [first.workspace], acceptedImports: accepted), slot: .catalog)
        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        _ = try await reopened.load()
        let before = persistence.bytes(.catalog)
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
        let corrupt = versionTwoAcceptance(original.workspace, digest: Data(repeating: 8, count: 32),
                                            transferID: transferID, revision: first.workspace.revision)
        persistence.seed(try versionTwoCatalog(workspaces: [first.workspace], acceptedImports: [corrupt]), slot: .catalog)
        let transaction = UUID(), account = "ft-import:\(transaction.uuidString):0"
        try vault.create(account: account, privateContainer: Data([99]), fingerprint: "keep-pending")
        persistence.seed(try JSONSerialization.data(withJSONObject: ["version": 1, "transaction": transaction.uuidString,
            "targetRevision": first.workspace.revision.uuidString, "newAccounts": [account], "retiredAccounts": []]), slot: .journal)
        do { _ = try await store.load(); XCTFail("current revision has no matching accepted envelope") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .corruptStore) }
        XCTAssertEqual(vault.count, 2); XCTAssertNotNil(persistence.bytes(.journal))
    }

    func testVersionTwoMigrationPreservesEveryAcceptedRevisionAndCredentialRevocation() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let original = try archive(), replacement = try archive(bundleID: "version-two-current")
        let first = try await store.install(original, envelopeDigest: digest, transferID: transferID,
                                            expectedRevision: nil, sourceAndContentsConfirmed: true)
        let nextDigest = Data(repeating: 20, count: 32), nextTransfer = Data(repeating: 21, count: 16)
        let second = try await store.install(replacement, envelopeDigest: nextDigest, transferID: nextTransfer,
                                             expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        let fingerprint = original.workspace.credentials[0].fingerprint
        let accepted = [
            versionTwoAcceptance(original.workspace, digest: digest, transferID: transferID, revision: first.workspace.revision),
            versionTwoAcceptance(replacement.workspace, digest: nextDigest, transferID: nextTransfer, revision: second.workspace.revision)
        ]
        persistence.seed(try versionTwoCatalog(workspaces: [second.workspace], revokedFingerprints: [fingerprint],
                                               acceptedImports: accepted), slot: .catalog)
        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        let snapshot = try await reopened.loadWithCredentialAvailability()
        XCTAssertEqual(snapshot.count, 1); XCTAssertEqual(snapshot[0].workspace.revision, second.workspace.revision)
        XCTAssertTrue(snapshot[0].credentialIDs.isEmpty); XCTAssertEqual(vault.count, 1)
        // Publish the migrated catalog without changing the already-local denial.
        try await reopened.revokeLocally(fingerprint: fingerprint)
        let upgraded = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(persistence.bytes(.catalog))) as? [String: Any])
        XCTAssertEqual(upgraded["version"] as? Int, 4)
        let preserved = try XCTUnwrap(upgraded["acceptedImports"] as? [[String: Any]])
        XCTAssertEqual(preserved.count, 2)
        XCTAssertEqual(preserved.map { $0["firstAcceptedRevision"] as? String },
                       [first.workspace.revision.uuidString, second.workspace.revision.uuidString])
        XCTAssertEqual(preserved.map { $0["transferID"] as? String },
                       [transferID.base64EncodedString(), nextTransfer.base64EncodedString()])
        let relaunched = WorkspaceStore(persistence: persistence, vault: vault)
        // The immutable acceptance history survives, but a local denial still
        // refuses both preview and installation of the historical private key.
        do {
            _ = try await relaunched.assessImport(original.workspace, envelopeDigest: digest, transferID: transferID)
            XCTFail("a migrated credential denial was bypassed during historical preview")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .revokedIdentity) }
        do {
            _ = try await relaunched.install(original, envelopeDigest: digest, transferID: transferID,
                expectedRevision: second.workspace.revision, sourceAndContentsConfirmed: true, allowHistoricalRollback: true)
            XCTFail("version-two credential denial was lost during migration")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .revokedIdentity) }
        XCTAssertEqual(vault.creates, 1)
    }

    func testVersionTwoHostDenialsAndRequirementsSurviveNewExportsAndRollback() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive()
        var fields = try XCTUnwrap(JSONSerialization.jsonObject(with: original.workspace.raw) as? [String: Any])
        var workspace = try XCTUnwrap(fields["workspace"] as? [String: Any])
        var trustFields = try XCTUnwrap(workspace["host_trust"] as? [[String: Any]])
        trustFields[0]["revoked_keys"] = trustFields[0]["pins"]
        trustFields[0]["required_capabilities"] = ["future-host-trust-policy"]
        trustFields[0]["extensions"] = ["org.example.required-host-policy": ["required": true]]
        workspace["host_trust"] = trustFields; fields["workspace"] = workspace
        let denied = try AirGapArchive(workspace: WorkspaceDocument(data: JSONSerialization.data(withJSONObject: fields)),
                                       keys: original.keys, using: crypto)
        let first = try await store.install(denied, envelopeDigest: digest, transferID: transferID,
                                            expectedRevision: nil, sourceAndContentsConfirmed: true)
        persistence.seed(try versionTwoCatalog(workspaces: [first.workspace], acceptedImports: [
            versionTwoAcceptance(denied.workspace, digest: digest, transferID: transferID, revision: first.workspace.revision)
        ]), slot: .catalog)
        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        let trust = denied.workspace.hostTrust[0], scope = trust.canonicalTrustScope
        let requirements: Set<String> = ["future-host-trust-policy", "extension:org.example.required-host-policy"]
        let migratedSnapshots = try await reopened.loadWithLocalPolicy()
        let migrated = try XCTUnwrap(migratedSnapshots.first)
        XCTAssertEqual(migrated.revokedHostKeysByTrustScope[scope], Set(trust.revokedKeys))
        XCTAssertEqual(migrated.requiredHostCapabilitiesByTrustScope[scope], requirements)
        XCTAssertEqual(migrated.credentialIDs, ["key"])
        let removedPolicy = try archive(keys: false, bundleID: "removed-policy")
        let next = try await reopened.install(removedPolicy, envelopeDigest: Data(repeating: 22, count: 32),
            transferID: Data(repeating: 23, count: 16), expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        let relaunched = WorkspaceStore(persistence: persistence, vault: vault)
        let snapshots = try await relaunched.loadWithLocalPolicy()
        let current = try XCTUnwrap(snapshots.first), currentDocument = try current.workspace.document()
        XCTAssertEqual(current.workspace.revision, next.workspace.revision)
        XCTAssertTrue(currentDocument.hostTrust[0].revokedKeys.isEmpty)
        XCTAssertTrue(currentDocument.hostTrust[0].requiredCapabilities.isEmpty)
        XCTAssertTrue(currentDocument.hostTrust[0].requiredExtensions.isEmpty)
        XCTAssertEqual(current.revokedHostKeysByTrustScope[scope], Set(trust.revokedKeys))
        XCTAssertEqual(current.requiredHostCapabilitiesByTrustScope[scope], requirements)
        let readiness = try currentDocument.readiness(for: "route", availableCredentialIDs: current.credentialIDs,
            acceptedTrustIDs: ["trust"], supportedAuthenticationMethods: [.publicKey],
            supportedCredentialAlgorithms: ["ssh-ed25519"], supportedHostKeyAlgorithms: ["ssh-ed25519"],
            supportedCapabilities: requirements,
            revokedHostKeysByTrustScope: current.revokedHostKeysByTrustScope,
            requiredHostCapabilitiesByTrustScope: current.requiredHostCapabilitiesByTrustScope)
        XCTAssertFalse(readiness.canAttemptConnection)
        XCTAssertTrue(readiness.issues.contains { $0.code == .requiredCapabilityUnavailable })
        XCTAssertTrue(readiness.issues.contains { $0.code == .hostIdentityRevoked })
        let preview = try await relaunched.preview(denied.workspace, envelopeDigest: digest, transferID: transferID)
        XCTAssertEqual(preview.preservedHostRevocationCount, 1); XCTAssertEqual(preview.preservedHostRequirementCount, 2)
        let restored = try await relaunched.install(denied, envelopeDigest: digest, transferID: transferID,
            expectedRevision: next.workspace.revision, sourceAndContentsConfirmed: true, allowHistoricalRollback: true)
        let required = try await relaunched.requiredHostCapabilities(workspaceID: restored.workspace.id)
        XCTAssertEqual(required[scope], requirements)
        XCTAssertEqual(vault.creates, 1)
    }

    func testCredentialSnapshotsReflectKeyLossAndLocalRevocationForTheirRevision() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive()
        let first = try await store.install(original, envelopeDigest: digest, transferID: transferID,
                                            expectedRevision: nil, sourceAndContentsConfirmed: true)
        let initial = try await store.loadWithCredentialAvailability()
        XCTAssertEqual(initial[0].workspace.revision, first.workspace.revision); XCTAssertEqual(initial[0].credentialIDs, ["key"])
        try vault.remove(account: first.workspace.bindings[0].account)
        let missing = try await store.loadWithLocalPolicy()
        XCTAssertEqual(missing[0].workspace.revision, first.workspace.revision)
        XCTAssertTrue(missing[0].credentialIDs.isEmpty); XCTAssertEqual(missing[0].workspace.bindings.count, 1)
        let restored = try await store.install(original, envelopeDigest: digest, transferID: transferID,
            expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        let available = try await store.loadWithLocalPolicy()
        XCTAssertEqual(available[0].workspace.revision, restored.workspace.revision); XCTAssertEqual(available[0].credentialIDs, ["key"])
        try await store.revokeLocally(fingerprint: original.workspace.credentials[0].fingerprint)
        let revoked = try await store.loadWithCredentialAvailability()
        XCTAssertEqual(revoked[0].workspace.revision, restored.workspace.revision)
        XCTAssertTrue(revoked[0].credentialIDs.isEmpty); XCTAssertEqual(vault.count, 1)
    }

    func testLegacyMetadataSanitizationPreservesReplayIdentityAndCanonicalizesPublicValues() async throws {
        for version in [1, 2] {
            let persistence = StoreTestPersistence(), vault = StoreTestVault()
            let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive()
            let first = try await store.install(original, envelopeDigest: digest, transferID: transferID,
                                                expectedRevision: nil, sourceAndContentsConfirmed: true)
            let secret = Data("SYNTHETIC_OPAQUE_PRIVATE_VALUE_MUST_NOT_SURVIVE_MIGRATION".utf8)
            let saved = replacingMetadata(of: first.workspace, annotations: [
                .init(entityID: "theme", setting: "unknown_private_setting", original: secret),
                .init(entityID: "work", setting: "enable_tab_bar", original: Data(" \ntrue\t".utf8)),
                .init(entityID: "theme", setting: "window_background_opacity", original: Data("0.75000e0".utf8))
            ], findings: [
                .init(entityID: "route", setting: "desktop.policy", disposition: 1, reason: "Required desktop policy remains unresolved."),
                .init(entityID: "theme", setting: "window_background_opacity", disposition: 1, reason: "Required desktop appearance remains unresolved.")
            ])
            var fields = try XCTUnwrap(JSONSerialization.jsonObject(with: versionTwoCatalog(workspaces: [saved], acceptedImports: [
                versionTwoAcceptance(original.workspace, digest: digest, transferID: transferID, revision: first.workspace.revision)
            ])) as? [String: Any])
            fields["version"] = version
            if version == 1 { fields.removeValue(forKey: "acceptedImports") }
            persistence.seed(try JSONSerialization.data(withJSONObject: fields), slot: .catalog)
            let reopened = WorkspaceStore(persistence: persistence, vault: vault), loaded = try await reopened.load()
            let migrated = try XCTUnwrap(loaded.first)
            XCTAssertEqual(migrated.id, first.workspace.id); XCTAssertEqual(migrated.revision, first.workspace.revision)
            XCTAssertEqual(migrated.manifest, original.workspace.raw); XCTAssertEqual(migrated.envelopeDigest, digest)
            XCTAssertEqual(migrated.bindings, first.workspace.bindings); XCTAssertTrue(migrated.connectLater)
            XCTAssertEqual(migrated.inactiveSettings.map(\.setting), ["enable_tab_bar", "window_background_opacity"])
            XCTAssertEqual(migrated.inactiveSettings.map { String(decoding: $0.original, as: UTF8.self) }, ["true", "0.75"])
            XCTAssertEqual(migrated.findings.filter { $0.disposition == 1 }.map(\.setting), ["desktop.policy", "window_background_opacity"])
            XCTAssertEqual(migrated.findings.filter { $0.setting == "frankenterm.discarded_annotations" }.count, 1)
            let published = try XCTUnwrap(persistence.bytes(.catalog))
            XCTAssertNil(published.range(of: secret))
            XCTAssertNil(published.range(of: Data(secret.base64EncodedString().utf8)))
            XCTAssertNil(published.range(of: Data("unknown_private_setting".utf8)))
            let catalog = try XCTUnwrap(JSONSerialization.jsonObject(with: published) as? [String: Any])
            XCTAssertEqual(catalog["version"] as? Int, 4); XCTAssertNil(catalog["needsSanitizedPublication"])
            let history = try XCTUnwrap((catalog["acceptedImports"] as? [[String: Any]])?.first)
            XCTAssertEqual(history["envelopeDigest"] as? String, digest.base64EncodedString())
            XCTAssertEqual(history["firstAcceptedRevision"] as? String, first.workspace.revision.uuidString)
            let assessment = try await reopened.assessImport(original.workspace, envelopeDigest: digest, transferID: transferID)
            XCTAssertFalse(assessment.requiresRollbackApproval); XCTAssertEqual(assessment.firstAcceptedRevision, first.workspace.revision)
            let relaunched = WorkspaceStore(persistence: persistence, vault: vault), repeated = try await relaunched.load()
            XCTAssertEqual(repeated[0].inactiveSettings.map(\.original), migrated.inactiveSettings.map(\.original))
            XCTAssertEqual(repeated[0].findings.count, migrated.findings.count, "the omission diagnostic must not accumulate")
            XCTAssertEqual(persistence.bytes(.catalog), published, "canonical data should not need another migration write")
            XCTAssertEqual(vault.creates, 1); XCTAssertEqual(vault.count, 1)
        }
    }

    func testLegacyRequiredOpaqueAnnotationRejectsBeforePublicationOrJournalCleanup() async throws {
        for version in [1, 2] {
            let persistence = StoreTestPersistence(), vault = StoreTestVault()
            let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive()
            let first = try await store.install(original, envelopeDigest: digest, transferID: transferID,
                                                expectedRevision: nil, sourceAndContentsConfirmed: true)
            let saved = replacingMetadata(of: first.workspace, annotations: [
                .init(entityID: "route", setting: "desktop.unknown_policy", original: Data("unrecognized required policy payload".utf8))
            ], findings: [.init(entityID: "route", setting: "desktop.unknown_policy", disposition: 1,
                                 reason: "This policy must be satisfied before connecting.")])
            var fields = try XCTUnwrap(JSONSerialization.jsonObject(with: versionTwoCatalog(workspaces: [saved], acceptedImports: [
                versionTwoAcceptance(original.workspace, digest: digest, transferID: transferID, revision: first.workspace.revision)
            ])) as? [String: Any])
            fields["version"] = version
            if version == 1 { fields.removeValue(forKey: "acceptedImports") }
            let legacy = try JSONSerialization.data(withJSONObject: fields)
            persistence.seed(legacy, slot: .catalog)
            let transaction = UUID(), pending = "ft-import:\(transaction.uuidString):0"
            try vault.create(account: pending, privateContainer: Data([99]), fingerprint: "pending-owned-key")
            persistence.seed(try JSONSerialization.data(withJSONObject: ["version": 1, "transaction": transaction.uuidString,
                "targetRevision": UUID().uuidString, "newAccounts": [pending], "retiredAccounts": []]), slot: .journal)
            let reopened = WorkspaceStore(persistence: persistence, vault: vault)
            do { _ = try await reopened.load(); XCTFail("unknown required annotation was erased during migration") }
            catch { XCTAssertEqual(error as? WorkspaceStoreError, .corruptStore) }
            XCTAssertEqual(persistence.bytes(.catalog), legacy)
            XCTAssertNotNil(persistence.bytes(.journal)); XCTAssertEqual(vault.count, 2)
            XCTAssertEqual(try vault.fingerprint(account: pending), "pending-owned-key")
        }
    }

    func testLegacyMetadataPublicationFailuresRetainJournalOwnedKeysUntilRecovery() async throws {
        for fault in [StoreTestPersistence.Fault.before(.catalog), .after(.catalog)] {
            let persistence = StoreTestPersistence(), vault = StoreTestVault()
            let store = WorkspaceStore(persistence: persistence, vault: vault), original = try archive()
            let first = try await store.install(original, envelopeDigest: digest, transferID: transferID,
                                                expectedRevision: nil, sourceAndContentsConfirmed: true)
            let saved = replacingMetadata(of: first.workspace, annotations: [
                .init(entityID: "work", setting: "opaque_optional_setting", original: Data("SYNTHETIC_PRIVATE_PAYLOAD".utf8))
            ])
            persistence.seed(try versionTwoCatalog(workspaces: [saved], acceptedImports: [
                versionTwoAcceptance(original.workspace, digest: digest, transferID: transferID, revision: first.workspace.revision)
            ]), slot: .catalog)
            let transaction = UUID(), pending = "ft-import:\(transaction.uuidString):0"
            try vault.create(account: pending, privateContainer: Data([99]), fingerprint: "pending-owned-key")
            persistence.seed(try JSONSerialization.data(withJSONObject: ["version": 1, "transaction": transaction.uuidString,
                "targetRevision": UUID().uuidString, "newAccounts": [pending], "retiredAccounts": []]), slot: .journal)
            persistence.configure(fault)
            let reopened = WorkspaceStore(persistence: persistence, vault: vault)
            do { _ = try await reopened.load(); XCTFail("migration publication fault was not exercised") }
            catch { XCTAssertEqual(error as? FTError, .storage) }
            XCTAssertEqual(vault.count, 2); XCTAssertNotNil(persistence.bytes(.journal))
            let recovered = try await reopened.load()
            XCTAssertEqual(recovered[0].revision, first.workspace.revision)
            XCTAssertTrue(recovered[0].inactiveSettings.isEmpty)
            XCTAssertEqual(vault.count, 1); XCTAssertNil(persistence.bytes(.journal))
            XCTAssertEqual(try vault.fingerprint(account: first.workspace.bindings[0].account), original.workspace.credentials[0].fingerprint)
        }
    }
}
