import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class WorkspaceCredentialImportTests: XCTestCase {
    private let crypto = TestCryptography()
    private let digest = Data(repeating: 91, count: 32)
    private let transferID = Data(repeating: 92, count: 16)
    private struct Fixture: Sendable {
        let store: WorkspaceStore, disk: StoreTestPersistence, vault: StoreTestVault
        let key: PrivateKeyRecord, archive: AirGapArchive, saved: InstalledWorkspace
    }

    private func document(fingerprint: String, algorithm: String = "ssh-ed25519",
                          edit: (inout [String: Any]) -> Void = { _ in }) throws -> WorkspaceDocument {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest(fingerprint: fingerprint)) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        var credentials = try XCTUnwrap(workspace["credentials"] as? [[String: Any]])
        credentials[0]["algorithm"] = algorithm
        workspace["credentials"] = credentials
        edit(&workspace)
        root["workspace"] = workspace
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }

    private func fixture(includeKey: Bool = false) async throws -> Fixture {
        let (key, fingerprint) = try sampleKey(using: crypto)
        let archive = try AirGapArchive(workspace: document(fingerprint: fingerprint),
            keys: includeKey ? [key] : [], using: crypto)
        let disk = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: disk, vault: vault)
        let installed = try await store.install(archive, envelopeDigest: digest, transferID: transferID,
            expectedRevision: nil, sourceAndContentsConfirmed: true)
        return .init(store: store, disk: disk, vault: vault, key: key, archive: archive, saved: installed.workspace)
    }

    private func review(_ f: Fixture) async throws -> WorkspaceCredentialImportReview {
        try await f.store.reviewCredentialImport(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, credentialID: "key")
    }

    private func catalogPolicy(_ disk: StoreTestPersistence) throws -> Data {
        var object = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(disk.bytes(.catalog))) as? [String: Any])
        object.removeValue(forKey: "workspaces")
        return try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
    }

    func testRepairPublishesOnlySelectedIdentityAliasesAndPreservesAcceptedPolicy() async throws {
        let (key, fingerprint) = try sampleKey(using: crypto)
        let document = try document(fingerprint: fingerprint) { workspace in
            var references = workspace["credentials"] as! [[String: Any]]
            var alias = references[0]; alias["id"] = "alias"
            var missing = references[0]; missing["id"] = "other"
            missing["public_fingerprint"] = "SHA256:ERERERERERERERERERERERERERERERERERERERERERE"
            references += [alias, missing]; workspace["credentials"] = references
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["required_capabilities"] = ["future-host-trust-policy"]
            workspace["host_trust"] = trusts
        }
        let archive = try AirGapArchive(workspace: document, keys: [],
            portability: [.init(entityID: "route", setting: "proxy_command", disposition: 1,
                                reason: "This route requires an unavailable proxy.")], using: crypto)
        let disk = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: disk, vault: vault)
        let installed = try await store.install(archive, envelopeDigest: digest, transferID: transferID,
            expectedRevision: nil, sourceAndContentsConfirmed: true)
        let saved = installed.workspace, trust = document.hostTrust[0]
        try await store.revokeHostLocally(workspaceID: saved.id, trustID: trust.id, key: XCTUnwrap(trust.pins.first))
        let policy = try catalogPolicy(disk)
        let review = try await store.reviewCredentialImport(workspaceID: saved.id,
            expectedRevision: saved.revision, credentialID: "key")
        XCTAssertEqual(review.workspaceName, document.name)
        XCTAssertEqual(review.credential.fingerprint, fingerprint)
        XCTAssertEqual(review.credentialIDs, ["key", "alias"])
        let result = try await store.importCredential(key, review: review, confirmedByUser: true)
        XCTAssertEqual(result.workspace.id, saved.id)
        XCTAssertNotEqual(result.workspace.revision, saved.revision)
        XCTAssertEqual(result.workspace.manifest, saved.manifest)
        XCTAssertEqual(result.workspace.envelopeDigest, saved.envelopeDigest)
        XCTAssertEqual(result.workspace.findings.map(\.entityID), saved.findings.map(\.entityID))
        XCTAssertEqual(result.workspace.findings.map(\.setting), saved.findings.map(\.setting))
        XCTAssertEqual(result.workspace.findings.map(\.disposition), saved.findings.map(\.disposition))
        XCTAssertEqual(result.workspace.findings.map(\.reason), saved.findings.map(\.reason))
        XCTAssertEqual(result.workspace.inactiveSettings.map(\.original), saved.inactiveSettings.map(\.original))
        XCTAssertTrue(result.workspace.connectLater)
        XCTAssertFalse(result.cleanupPending)
        XCTAssertEqual(result.credentialIDs, ["key", "alias"])
        XCTAssertEqual(Set(result.workspace.bindings.map(\.credentialID)), ["key", "alias"])
        XCTAssertEqual(Set(result.workspace.bindings.map(\.account)).count, 1)
        XCTAssertEqual(vault.creates, 1)
        XCTAssertEqual(try catalogPolicy(disk), policy)
        XCTAssertNil(disk.bytes(.journal))
        let reopened = WorkspaceStore(persistence: disk, vault: vault)
        let available = try await reopened.availableCredentialIDs(workspaceID: saved.id)
        let revoked = try await reopened.revokedHostKeys(workspaceID: saved.id)
        let originalAcceptance = try await reopened.preview(document, envelopeDigest: digest, transferID: transferID)
        XCTAssertEqual(available, ["key", "alias"])
        XCTAssertEqual(revoked[trust.canonicalTrustScope], Set(trust.pins))
        XCTAssertEqual(originalAcceptance.firstAcceptedRevision, saved.revision)
        XCTAssertEqual(originalAcceptance.expectedRevision, result.workspace.revision)
        XCTAssertFalse(originalAcceptance.isRollback)
        let catalog = try XCTUnwrap(disk.bytes(.catalog))
        XCTAssertNil(catalog.range(of: key.container))
        XCTAssertNil(catalog.range(of: Data(key.container.base64EncodedString().utf8)))
    }

    func testReviewConsentIssuerAndCancellationPrecedeAllImportSideEffects() async throws {
        let f = try await fixture(), selected = try await review(f)
        let before = f.disk.bytes(.catalog)
        do {
            _ = try await f.store.importCredential(f.key, review: selected, confirmedByUser: false)
            XCTFail("unapproved key installation")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .reviewRequired) }
        let anotherStore = WorkspaceStore(persistence: f.disk, vault: f.vault)
        do {
            _ = try await anotherStore.importCredential(f.key, review: selected, confirmedByUser: true)
            XCTFail("review escaped its issuing store")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
        do {
            _ = try await f.store.reviewCredentialImport(workspaceID: f.saved.id,
                expectedRevision: f.saved.revision, credentialID: "unlisted")
            XCTFail("unlisted reference was selectable")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
        let cancelled = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            do {
                _ = try await f.store.importCredential(f.key, review: selected, confirmedByUser: true)
                return false
            } catch is CancellationError { return true }
            catch { return false }
        }
        let stopped = await cancelled.value
        XCTAssertTrue(stopped)
        XCTAssertEqual(f.disk.bytes(.catalog), before)
        XCTAssertNil(f.disk.bytes(.journal)); XCTAssertEqual(f.vault.creates, 0)
    }

    func testReplacementAndRevocationInvalidateAlreadyReviewedKey() async throws {
        let f = try await fixture(), selected = try await review(f)
        let next = try AirGapArchive(workspace: f.archive.workspace.reissuingBundle(id: "new-export"), keys: [], using: crypto)
        let replaced = try await f.store.install(next, envelopeDigest: Data(repeating: 93, count: 32),
            expectedRevision: f.saved.revision, sourceAndContentsConfirmed: true, associateWithExistingWorkspace: true)
        let replacementBytes = f.disk.bytes(.catalog)
        do {
            _ = try await f.store.importCredential(f.key, review: selected, confirmedByUser: true)
            XCTFail("old reviewed revision installed a key into the replacement")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
        XCTAssertEqual(f.disk.bytes(.catalog), replacementBytes)
        let current = try await f.store.reviewCredentialImport(workspaceID: replaced.workspace.id,
            expectedRevision: replaced.workspace.revision, credentialID: "key")
        try await f.store.revokeLocally(fingerprint: current.credential.fingerprint)
        let deniedBytes = f.disk.bytes(.catalog)
        do {
            _ = try await f.store.importCredential(f.key, review: current, confirmedByUser: true)
            XCTFail("revocation was undone by a pending key repair")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .revokedIdentity) }
        do {
            _ = try await f.store.reviewCredentialImport(workspaceID: replaced.workspace.id,
                expectedRevision: replaced.workspace.revision, credentialID: "key")
            XCTFail("a revoked identity could start a new key-file prompt")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .revokedIdentity) }
        XCTAssertEqual(f.disk.bytes(.catalog), deniedBytes)
        XCTAssertNil(f.disk.bytes(.journal)); XCTAssertEqual(f.vault.creates, 0)
    }

    func testProductionPrivateProofRejectsSubstitutedBlobPrivateKeyAndReference() async throws {
        let f = try await fixture(), selected = try await review(f)
        let unrelated = try KeyFiles.load().pairs[0]
        let other = try PrivateKeyRecord.fromPEM(Data(unrelated.clearPEM.utf8), credentialID: "key", using: crypto)
        let before = f.disk.bytes(.catalog)
        for bad in [
            PrivateKeyRecord(credentialID: "key", container: f.key.container, publicBlob: other.publicBlob),
            PrivateKeyRecord(credentialID: "key", container: other.container, publicBlob: f.key.publicBlob),
            PrivateKeyRecord(credentialID: "another", container: f.key.container, publicBlob: f.key.publicBlob),
            PrivateKeyRecord(credentialID: "key", container: f.key.container, publicBlob: f.key.publicBlob, certificates: [Data([1])])
        ] {
            do {
                _ = try await f.store.importCredential(bad, review: selected, confirmedByUser: true)
                XCTFail("a nonmatching or unsupported key record was installed")
            } catch {}
        }
        XCTAssertEqual(f.disk.bytes(.catalog), before)
        XCTAssertNil(f.disk.bytes(.journal)); XCTAssertEqual(f.vault.creates, 0)
    }

    func testEncryptedOpenSSHFilesRepairEverySupportedIdentityWithoutAnArchiveImport() async throws {
        let files = try KeyFiles.load()
        XCTAssertEqual(Set(files.pairs.map(\.algorithm)), Set(["ssh-ed25519"] + SSHECDSACurve.allCases.map(\.algorithm)))
        for file in files.pairs {
            let document = try document(fingerprint: file.fingerprint, algorithm: file.algorithm)
            let disk = StoreTestPersistence(), vault = StoreTestVault()
            let store = WorkspaceStore(persistence: disk, vault: vault)
            let archive = try AirGapArchive(workspace: document, keys: [], using: crypto)
            let installed = try await store.install(archive, envelopeDigest: digest, transferID: transferID,
                expectedRevision: nil, sourceAndContentsConfirmed: true)
            let review = try await store.reviewCredentialImport(workspaceID: installed.workspace.id,
                expectedRevision: installed.workspace.revision, credentialID: "key")
            let before = disk.bytes(.catalog), policy = try catalogPolicy(disk)
            var unlocker = OpenSSHSourceKeyUnlocker(using: crypto)
            guard case let .passphraseRequired(request) = try unlocker.select(pem: Data(file.encryptedPEM.utf8), reference: review.credential) else {
                return XCTFail("encrypted OpenSSH file did not request its passphrase")
            }
            XCTAssertThrowsError(try unlocker.unlock(requestID: request.id, passphrase: Data("wrong-test-passphrase".utf8)))
            XCTAssertEqual(unlocker.request?.attemptsRemaining, 4)
            let key = try unlocker.unlock(requestID: request.id, passphrase: Data(files.passphrase.utf8))
            XCTAssertNil(unlocker.request)
            XCTAssertEqual(disk.bytes(.catalog), before)
            XCTAssertEqual(vault.creates, 0, "unlocking alone must not install the key")
            let result = try await store.importCredential(key, review: review, confirmedByUser: true)
            XCTAssertEqual(result.workspace.manifest, installed.workspace.manifest)
            XCTAssertEqual(result.workspace.envelopeDigest, installed.workspace.envelopeDigest)
            XCTAssertEqual(result.credentialIDs, ["key"])
            XCTAssertEqual(try catalogPolicy(disk), policy)
            let identity = try SSHUserAuthenticationSigner.publicIdentity(privateContainer: key.container)
            XCTAssertEqual(identity.algorithm, file.algorithm)
            XCTAssertEqual(identity.fingerprint, file.fingerprint)
            let available = try await store.availableCredentialIDs(workspaceID: result.workspace.id)
            XCTAssertEqual(available, ["key"])
            XCTAssertEqual(vault.creates, 1)
        }
    }

    func testMatchingItemFromAnotherWorkspaceIsReusedAndRepeatedRepairIsIdempotent() async throws {
        let f = try await fixture(), selected = try await review(f)
        let sourceDocument = try document(fingerprint: selected.credential.fingerprint) { $0["id"] = "source" }
            .reissuingBundle(id: "source-bundle")
        let sourceArchive = try AirGapArchive(workspace: sourceDocument, keys: [f.key], using: crypto)
        let source = try await f.store.install(sourceArchive, envelopeDigest: Data(repeating: 94, count: 32),
            expectedRevision: nil, sourceAndContentsConfirmed: true)
        let result = try await f.store.importCredential(f.key, review: selected, confirmedByUser: true)
        XCTAssertEqual(result.workspace.bindings[0].account, source.workspace.bindings[0].account)
        XCTAssertEqual(f.vault.creates, 1); XCTAssertEqual(f.vault.count, 1)
        let current = try await f.store.reviewCredentialImport(workspaceID: result.workspace.id,
            expectedRevision: result.workspace.revision, credentialID: "key")
        let before = f.disk.bytes(.catalog)
        let repeated = try await f.store.importCredential(f.key, review: current, confirmedByUser: true)
        XCTAssertEqual(repeated.workspace.revision, result.workspace.revision)
        XCTAssertEqual(f.disk.bytes(.catalog), before); XCTAssertEqual(f.vault.creates, 1)
        let saved = try await f.store.load()
        XCTAssertEqual(saved.first(where: { $0.id == source.workspace.id })?.revision, source.workspace.revision)
    }

    func testRepairMissingProtectedItemChangesOnlyItsBindingAndPreservesImportHistory() async throws {
        let f = try await fixture(includeKey: true)
        let previousAccount = f.saved.bindings[0].account
        try f.vault.remove(account: previousAccount)
        let selected = try await review(f), policy = try catalogPolicy(f.disk)
        let result = try await f.store.importCredential(f.key, review: selected, confirmedByUser: true)
        XCTAssertNotEqual(result.workspace.bindings[0].account, previousAccount)
        XCTAssertNotEqual(result.workspace.revision, f.saved.revision)
        XCTAssertEqual(result.workspace.manifest, f.saved.manifest)
        XCTAssertEqual(try catalogPolicy(f.disk), policy)
        XCTAssertEqual(f.vault.count, 1); XCTAssertEqual(f.vault.creates, 2)
        XCTAssertNil(f.disk.bytes(.journal))
    }

    func testEveryJournalAndPublicationFaultRecoversOldOrNewWithNoOrphanedKey() async throws {
        for (fault, published) in [(StoreTestPersistence.Fault.before(.journal), false), (.after(.journal), false),
                                   (.before(.catalog), false), (.after(.catalog), true)] {
            let f = try await fixture(), selected = try await review(f)
            let policy = try catalogPolicy(f.disk)
            f.disk.configure(fault)
            do {
                _ = try await f.store.importCredential(f.key, review: selected, confirmedByUser: true)
                XCTFail("configured persistence fault was not exercised")
            } catch {}
            let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault)
            let states = try await reopened.loadWithCredentialAvailability()
            let recovered = try XCTUnwrap(states.first)
            XCTAssertEqual(recovered.workspace.id, f.saved.id)
            XCTAssertEqual(recovered.workspace.manifest, f.saved.manifest)
            XCTAssertEqual(recovered.workspace.envelopeDigest, f.saved.envelopeDigest)
            XCTAssertEqual(recovered.credentialIDs, published ? ["key"] : [])
            XCTAssertEqual(recovered.workspace.revision == f.saved.revision, !published)
            XCTAssertEqual(try catalogPolicy(f.disk), policy)
            XCTAssertEqual(f.vault.count, published ? 1 : 0)
            XCTAssertNil(f.disk.bytes(.journal))
        }
    }

    func testKeychainInsertionFailuresKeepExistingWorkspaceAndUnrelatedProtectedItems() async throws {
        for fault in [StoreTestVault.Fault.beforeInsert, .afterInsert] {
            let f = try await fixture(), selected = try await review(f)
            try f.vault.create(account: "unrelated-item", privateContainer: Data([88]), fingerprint: "unrelated")
            let before = try StrictJSON.decode(XCTUnwrap(f.disk.bytes(.catalog)))
            f.vault.configure(fault)
            do {
                _ = try await f.store.importCredential(f.key, review: selected, confirmedByUser: true)
                XCTFail("configured key insertion fault was not exercised")
            } catch {}
            let saved = try await f.store.load()
            XCTAssertEqual(saved.first?.revision, f.saved.revision)
            // Recovery durably republishes the unchanged catalog. Object-key
            // order may change; every decoded field and array must stay equal.
            XCTAssertEqual(try StrictJSON.decode(XCTUnwrap(f.disk.bytes(.catalog))), before)
            XCTAssertEqual(f.vault.count, 1)
            XCTAssertEqual(try f.vault.fingerprint(account: "unrelated-item"), "unrelated")
            XCTAssertNil(f.disk.bytes(.journal))
        }
    }

    func testUncertainPublicationRetainsRecoveryAuthorityAndNeverDuplicatesPublishedKey() async throws {
        let f = try await fixture(), selected = try await review(f)
        f.disk.configureWriteAndRecoveryFailure()
        do {
            _ = try await f.store.importCredential(f.key, review: selected, confirmedByUser: true)
            XCTFail("indeterminate publication was reported as ordinary success")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .recoveryRequired) }
        XCTAssertNotNil(f.disk.bytes(.journal)); XCTAssertEqual(f.vault.count, 1)
        let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault)
        let states = try await reopened.loadWithCredentialAvailability()
        let recovered = try XCTUnwrap(states.first)
        XCTAssertEqual(recovered.credentialIDs, ["key"])
        XCTAssertNotEqual(recovered.workspace.revision, f.saved.revision)
        XCTAssertNil(f.disk.bytes(.journal))
        let current = try await reopened.reviewCredentialImport(workspaceID: recovered.workspace.id,
            expectedRevision: recovered.workspace.revision, credentialID: "key")
        let repeated = try await reopened.importCredential(f.key, review: current, confirmedByUser: true)
        XCTAssertEqual(repeated.workspace.revision, recovered.workspace.revision)
        XCTAssertEqual(f.vault.creates, 1)
    }

    func testCommittedKeyRemainsUsableWhenJournalCleanupNeedsRetry() async throws {
        let f = try await fixture(), selected = try await review(f)
        f.disk.configure(.cleanup)
        let result = try await f.store.importCredential(f.key, review: selected, confirmedByUser: true)
        XCTAssertTrue(result.cleanupPending)
        XCTAssertNotNil(f.disk.bytes(.journal))
        let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault)
        let states = try await reopened.loadWithCredentialAvailability()
        XCTAssertEqual(states.first?.workspace.revision, result.workspace.revision)
        XCTAssertEqual(states.first?.credentialIDs, ["key"])
        XCTAssertNil(f.disk.bytes(.journal)); XCTAssertEqual(f.vault.creates, 1)
    }
}

/// Independent OpenSSH-generated public test fixtures; production code does not
/// create the encrypted input used to qualify this import path.
private struct KeyFiles: Decodable {
    struct Pair: Decodable {
        let algorithm: String, fingerprint: String, encryptedPEM: String, clearPEM: String
    }
    let passphrase: String
    let pairs: [Pair]
    static func load() throws -> Self {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        return try JSONDecoder().decode(Self.self,
            from: Data(contentsOf: root.appendingPathComponent("fixtures/ios-airgap/openssh-source-keys.json")))
    }
}
