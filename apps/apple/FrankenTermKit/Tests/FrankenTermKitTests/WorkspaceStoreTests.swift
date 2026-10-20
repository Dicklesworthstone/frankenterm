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
    func archive(keys: Bool = true) throws -> AirGapArchive {
        let (key, fingerprint) = try sampleKey(using: crypto)
        return try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint)),
                                 keys: keys ? [key] : [], using: crypto)
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
            let replacement = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [], using: crypto)
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
        let replacement = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [], using: crypto)
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
}
