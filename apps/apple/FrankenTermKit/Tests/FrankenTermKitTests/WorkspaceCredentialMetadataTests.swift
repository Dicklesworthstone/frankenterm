import Foundation
import XCTest
@testable import FrankenTermKit

/// Fault-injected protected-item boundary. Its immutable protection token
/// models preservation for portable assertions; it is not native Keychain ACL
/// proof. Public attributes use the production codec without a second parser.
private final class MetadataTestVault: WorkspaceCredentialVault, @unchecked Sendable {
    enum Fault { case publicRead, beforeEnrichment, afterEnrichment }
    struct Item: Equatable {
        let privateContainer: Data, protection: UUID, fingerprintBytes: Data
        var publicComment: String?
    }
    private let lock = NSLock()
    private let legacy: Bool, supportsEnrichment: Bool
    private var items: [String: Item] = [:]
    private var fault: Fault?
    private var identityOverride: SSHPublicIdentity?
    private var inserted = 0, enriched = 0
    init(legacy: Bool = false, supportsEnrichment: Bool = true) {
        self.legacy = legacy; self.supportsEnrichment = supportsEnrichment
    }
    var creates: Int { lock.lock(); defer { lock.unlock() }; return inserted }
    var enrichments: Int { lock.lock(); defer { lock.unlock() }; return enriched }
    func configure(_ fault: Fault?) { lock.lock(); defer { lock.unlock() }; self.fault = fault }
    func overrideIdentity(_ identity: SSHPublicIdentity?) {
        lock.lock(); defer { lock.unlock() }; identityOverride = identity
    }
    func snapshot(_ account: String) -> Item? { lock.lock(); defer { lock.unlock() }; return items[account] }
    private func metadata(_ item: Item) throws -> WorkspaceCredentialMetadata {
        try .init(fingerprintBytes: item.fingerprintBytes, publicComment: item.publicComment)
    }
    func create(account: String, privateContainer: Data, fingerprint: String) throws {
        lock.lock(); defer { lock.unlock() }
        guard items[account] == nil else { throw FTError.storage }
        let identity = try SSHUserAuthenticationSigner.publicIdentity(privateContainer: privateContainer)
        guard identity.fingerprint == fingerprint else { throw SSHAuthenticationError.identityMismatch }
        let metadata = WorkspaceCredentialMetadata(identity: identity)
        items[account] = .init(privateContainer: privateContainer, protection: UUID(),
            fingerprintBytes: metadata.fingerprintBytes, publicComment: legacy ? nil : metadata.publicComment)
        inserted += 1
    }
    func fingerprint(account: String) throws -> String? {
        lock.lock(); defer { lock.unlock() }
        return try items[account].map { try metadata($0).fingerprint }
    }
    func publicIdentity(account: String) throws -> SSHPublicIdentity? {
        lock.lock(); defer { lock.unlock() }
        if case .publicRead = fault { fault = nil; throw FTError.storage }
        if let identityOverride { return identityOverride }
        return try items[account].flatMap { try metadata($0).identity }
    }
    func enrichPublicIdentity(account: String, identity: SSHPublicIdentity) throws -> Bool {
        lock.lock(); defer { lock.unlock() }
        guard supportsEnrichment else { return false }
        guard var item = items[account] else { throw FTError.storage }
        let old = try metadata(item)
        guard old.fingerprint == identity.fingerprint else { throw SSHAuthenticationError.identityMismatch }
        if let known = old.identity {
            guard known == identity else { throw SSHAuthenticationError.identityMismatch }
            return true
        }
        if case .beforeEnrichment = fault { fault = nil; throw FTError.storage }
        item.publicComment = WorkspaceCredentialMetadata(identity: identity).publicComment
        items[account] = item; enriched += 1
        if case .afterEnrichment = fault { fault = nil; throw FTError.storage }
        return true
    }
    func remove(account: String) throws { lock.lock(); defer { lock.unlock() }; items[account] = nil }
}

/// Deliberately incorrect provider used only to prove that a caller-created
/// archive cannot substitute for the store's independent production validation.
private struct MisreportingPointCryptography: AirGapCryptography {
    let point: Data
    private let base = TestCryptography()
    func sha256(_ bytes: Data) -> Data { base.sha256(bytes) }
    func ed25519PublicKey(seed: Data) throws -> Data { try base.ed25519PublicKey(seed: seed) }
    func ecdsaPublicKey(scalar: Data, curve: SSHECDSACurve) throws -> Data { point }
    func hmacSHA1(key: Data, message: Data) throws -> Data { try base.hmacSHA1(key: key, message: message) }
    func randomBytes(count: Int) throws -> Data { try base.randomBytes(count: count) }
    func seal(_ plaintext: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data {
        try base.seal(plaintext, secret: secret, salt: salt, nonce: nonce, aad: aad)
    }
    func open(_ ciphertextAndTag: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data {
        try base.open(ciphertextAndTag, secret: secret, salt: salt, nonce: nonce, aad: aad)
    }
    func bcryptPBKDF(passphrase: Data, salt: Data, rounds: UInt32, outputByteCount: Int) throws -> Data {
        try base.bcryptPBKDF(passphrase: passphrase, salt: salt, rounds: rounds, outputByteCount: outputByteCount)
    }
    func decryptAESCTR(_ ciphertext: Data, key: Data, iv: Data) throws -> Data {
        try base.decryptAESCTR(ciphertext, key: key, iv: iv)
    }
}

@MainActor
final class WorkspaceCredentialMetadataTests: XCTestCase {
    private let crypto = TestCryptography()
    private struct Key {
        let record: PrivateKeyRecord, algorithm: String, fingerprint: String
    }
    private struct Fixture {
        let store: WorkspaceStore, persistence: StoreTestPersistence, vault: MetadataTestVault
        let archive: AirGapArchive, installed: InstalledWorkspace
        var account: String { installed.bindings[0].account }
    }
    private func ssh(_ data: Data) -> Data {
        var result = Data(); result.appendBE(UInt64(data.count), bytes: 4); return result + data
    }
    private func keys() throws -> [Key] {
        let ed25519 = try sampleKey(using: crypto)
        var result = [Key(record: ed25519.0, algorithm: "ssh-ed25519", fingerprint: ed25519.1)]
        // Synthetic d=1 inputs must never authorize a real host. On Linux,
        // OpenSSL derives these points independently of production Swift Crypto.
        for curve in SSHECDSACurve.allCases {
            let scalar = Data(repeating: 0, count: curve.scalarBytes - 1) + Data([1])
            let point = try crypto.ecdsaPublicKey(scalar: scalar, curve: curve)
            let algorithm = Data(curve.algorithm.utf8), name = Data(curve.rawValue.utf8)
            let blob = ssh(algorithm) + ssh(name) + ssh(point)
            var part = Data([1, 2, 3, 4, 1, 2, 3, 4]) + ssh(algorithm) + ssh(name) + ssh(point)
                + ssh(Data([1])) + ssh(Data("PUBLIC_SYNTHETIC_METADATA_NEVER_USE".utf8))
            var padding: UInt8 = 1
            while !part.count.isMultiple(of: 8) { part.append(padding); padding += 1 }
            let container = Data("openssh-key-v1\0".utf8) + ssh(Data("none".utf8)) + ssh(Data("none".utf8))
                + ssh(Data()) + Data([0, 0, 0, 1]) + ssh(blob) + ssh(part)
            result.append(.init(record: .init(credentialID: "key", container: container, publicBlob: blob),
                algorithm: curve.algorithm,
                fingerprint: "SHA256:" + crypto.sha256(blob).base64EncodedString().replacingOccurrences(of: "=", with: "")))
        }
        return result
    }
    private func document(_ data: Data, change: (inout [String: Any], inout [String: Any]) -> Void) throws -> WorkspaceDocument {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        change(&root, &workspace); root["workspace"] = workspace
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }
    private func fixture(key: Key? = nil, legacy: Bool = true, supportsEnrichment: Bool = true) async throws -> Fixture {
        let key = try key ?? keys()[0]
        let workspace = try document(sampleManifest(fingerprint: key.fingerprint)) { _, workspace in
            workspace["credentials"] = [["id": "key", "algorithm": key.algorithm, "public_fingerprint": key.fingerprint,
                "requirement": "matching_private_key", "private_material": "not_included"]]
        }
        let archive = try AirGapArchive(workspace: workspace, keys: [key.record], using: crypto)
        let persistence = StoreTestPersistence(), vault = MetadataTestVault(legacy: legacy, supportsEnrichment: supportsEnrichment)
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let installed = try await store.install(archive, envelopeDigest: Data(repeating: 1, count: 32),
            expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        return .init(store: store, persistence: persistence, vault: vault, archive: archive, installed: installed)
    }
    private func reimport(_ f: Fixture) async throws -> WorkspaceImportReceipt {
        try await f.store.install(f.archive, envelopeDigest: Data(repeating: 1, count: 32),
            expectedRevision: f.installed.revision, sourceAndContentsConfirmed: true)
    }

    func testPublishedMetadataFormatRoundtripsEveryIndependentlyDerivedKeyFamily() throws {
        for key in try keys() {
            let identity = try SSHUserAuthenticationSigner.publicIdentity(privateContainer: key.record.container)
            XCTAssertEqual(identity.blob, key.record.publicBlob); XCTAssertEqual(identity.algorithm, key.algorithm)
            XCTAssertEqual(identity.fingerprint, key.fingerprint)
            let metadata = WorkspaceCredentialMetadata(identity: identity)
            XCTAssertEqual(metadata.fingerprintBytes, Data(key.fingerprint.utf8))
            XCTAssertEqual(metadata.publicComment, "ft-ssh-public-v1:" + key.record.publicBlob.base64EncodedString())
            XCTAssertEqual(try WorkspaceCredentialMetadata(fingerprintBytes: metadata.fingerprintBytes,
                publicComment: metadata.publicComment), metadata)
            XCTAssertLessThanOrEqual(try XCTUnwrap(metadata.publicComment).utf8.count, WorkspaceCredentialMetadata.maximumCommentBytes)
            let legacy = try WorkspaceCredentialMetadata(fingerprintBytes: metadata.fingerprintBytes, publicComment: nil)
            XCTAssertEqual(legacy.fingerprint, identity.fingerprint); XCTAssertNil(legacy.identity)
            XCTAssertNil(legacy.publicComment)
        }
    }

    func testMalformedPresentMetadataNeverBecomesLegacyOrChangesIdentity() throws {
        let key = try keys()[0], prefix = WorkspaceCredentialMetadata.commentPrefix
        let encoded = key.record.publicBlob.base64EncodedString()
        for comment in ["", "ft-ssh-public-v2:" + encoded, prefix, prefix + encoded + "=",
                        prefix + encoded + "\n", prefix + String(encoded.dropLast()),
                        prefix + Data(repeating: 0, count: 513).base64EncodedString(),
                        prefix + (key.record.publicBlob + Data([0])).base64EncodedString()] {
            XCTAssertThrowsError(try WorkspaceCredentialMetadata(fingerprintBytes: Data(key.fingerprint.utf8), publicComment: comment))
        }
        let other = try keys()[1]
        XCTAssertThrowsError(try WorkspaceCredentialMetadata(fingerprintBytes: Data(key.fingerprint.utf8),
            publicComment: prefix + other.record.publicBlob.base64EncodedString()))
        for fingerprint in [Data(), Data((key.fingerprint + "=").utf8), Data(String(repeating: "x", count: 50).utf8),
                            Data("SHA256:///////////////////////////////////////////".utf8)] {
            XCTAssertThrowsError(try WorkspaceCredentialMetadata(fingerprintBytes: fingerprint, publicComment: nil))
        }
    }

    func testExactReviewedReimportEnrichesAllFamiliesWithoutReplacingProtectedItemsOrRevision() async throws {
        for key in try keys() {
            let f = try await fixture(key: key), catalog = f.persistence.bytes(.catalog)
            let original = try XCTUnwrap(f.vault.snapshot(f.account))
            XCTAssertNil(try f.vault.publicIdentity(account: f.account))
            let receipt = try await reimport(f), repeated = try await reimport(f)
            XCTAssertEqual(receipt.workspace.revision, f.installed.revision)
            XCTAssertEqual(repeated.workspace.revision, f.installed.revision)
            XCTAssertEqual(receipt.workspace.bindings, f.installed.bindings)
            let upgraded = try XCTUnwrap(f.vault.snapshot(f.account))
            XCTAssertEqual(upgraded.privateContainer, original.privateContainer)
            XCTAssertEqual(upgraded.protection, original.protection)
            XCTAssertEqual(upgraded.fingerprintBytes, original.fingerprintBytes)
            XCTAssertEqual(try f.vault.publicIdentity(account: f.account), try SSHPublicIdentity(blob: key.record.publicBlob))
            XCTAssertEqual(f.vault.creates, 1); XCTAssertEqual(f.vault.enrichments, 1)
            XCTAssertEqual(f.persistence.bytes(.catalog), catalog); XCTAssertNil(f.persistence.bytes(.journal))
        }
    }

    func testEveryRequiredReviewAndLocalRevocationPrecedesMetadataEnrichment() async throws {
        for scenario in 0..<5 {
            let f = try await fixture(), original = f.vault.snapshot(f.account)
            var archive = f.archive, digest = Data(repeating: 1, count: 32)
            let expected: WorkspaceStoreError
            if scenario == 2 || scenario == 3 {
                let changed = try document(f.archive.workspace.raw) { root, workspace in
                    root["bundle_id"] = "replacement"
                    if scenario == 3 {
                        var routes = workspace["routes"] as! [[String: Any]]
                        routes[0]["user"] = "another-user"; workspace["routes"] = routes
                    }
                }
                archive = try AirGapArchive(workspace: changed, keys: f.archive.keys, using: crypto)
                digest = Data(repeating: 2, count: 32)
            }
            switch scenario {
            case 0: expected = .reviewRequired
            case 1: expected = .staleReview
            case 2: expected = .sourceReplacementNeedsApproval
            case 3: expected = .securityChangesNeedApproval
            default:
                try await f.store.revokeLocally(fingerprint: f.archive.workspace.credentials[0].fingerprint)
                expected = .revokedIdentity
            }
            let catalog = f.persistence.bytes(.catalog)
            do {
                _ = try await f.store.install(archive, envelopeDigest: digest,
                    expectedRevision: scenario == 1 ? nil : f.installed.revision,
                    sourceAndContentsConfirmed: scenario != 0, associateWithExistingWorkspace: scenario == 3)
                XCTFail("Missing approval must refuse before any enrichment")
            } catch { XCTAssertEqual(error as? WorkspaceStoreError, expected) }
            XCTAssertEqual(f.vault.snapshot(f.account), original); XCTAssertEqual(f.vault.enrichments, 0)
            XCTAssertEqual(f.persistence.bytes(.catalog), catalog); XCTAssertNil(f.persistence.bytes(.journal))
        }
    }

    func testSettingsOnlyAndUnsupportedVaultCannotInventPublicMetadata() async throws {
        for supportsEnrichment in [false, true] {
            let f = try await fixture(supportsEnrichment: supportsEnrichment)
            if !supportsEnrichment { _ = try await reimport(f) }
            let archive = try AirGapArchive(workspace: f.archive.workspace.reissuingBundle(id: "public-only"), keys: [], using: crypto)
            _ = try await f.store.install(archive, envelopeDigest: Data(repeating: 2, count: 32),
                expectedRevision: f.installed.revision, sourceAndContentsConfirmed: true, associateWithExistingWorkspace: true)
            XCTAssertNil(try f.vault.publicIdentity(account: f.account))
            XCTAssertEqual(f.vault.enrichments, 0); XCTAssertEqual(f.vault.creates, 1)
        }
    }

    func testFailedAndIndeterminateEnrichmentRetriesKeepExactPrivateItemAndCatalog() async throws {
        for fault in [MetadataTestVault.Fault.beforeEnrichment, .afterEnrichment] {
            let f = try await fixture(), catalog = f.persistence.bytes(.catalog)
            let original = try XCTUnwrap(f.vault.snapshot(f.account))
            f.vault.configure(fault)
            do { _ = try await reimport(f); XCTFail("Injected write failure must remain visible") }
            catch { XCTAssertEqual(error as? FTError, .storage) }
            XCTAssertEqual(f.persistence.bytes(.catalog), catalog); XCTAssertNil(f.persistence.bytes(.journal))
            XCTAssertEqual(f.vault.snapshot(f.account)?.privateContainer, original.privateContainer)
            XCTAssertEqual(f.vault.snapshot(f.account)?.protection, original.protection)
            let receipt = try await reimport(f)
            XCTAssertEqual(receipt.workspace.revision, f.installed.revision)
            XCTAssertEqual(f.vault.creates, 1); XCTAssertEqual(f.vault.enrichments, 1)
            XCTAssertEqual(try f.vault.publicIdentity(account: f.account)?.blob, f.archive.keys[0].publicBlob)
        }
    }

    func testPublicMetadataSubstitutionAndReadFailureDoNotAuthorizeAnUpgrade() async throws {
        let f = try await fixture(), original = f.vault.snapshot(f.account)
        f.vault.overrideIdentity(try SSHPublicIdentity(blob: keys()[1].record.publicBlob))
        do { _ = try await reimport(f); XCTFail("Another identity cannot be reused") }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .identityMismatch) }
        f.vault.overrideIdentity(nil); f.vault.configure(.publicRead)
        do { _ = try await reimport(f); XCTFail("Unreadable is not absent") }
        catch { XCTAssertEqual(error as? FTError, .storage) }
        XCTAssertEqual(f.vault.snapshot(f.account), original); XCTAssertEqual(f.vault.enrichments, 0)
    }

    func testCrossWorkspaceReuseEnrichesTheSameProtectedAccountOnce() async throws {
        let f = try await fixture(), original = try XCTUnwrap(f.vault.snapshot(f.account))
        let changed = try document(f.archive.workspace.raw) { root, workspace in
            workspace["id"] = "other-workspace"; root["bundle_id"] = "other-bundle"
        }
        let archive = try AirGapArchive(workspace: changed, keys: f.archive.keys, using: crypto)
        let second = try await f.store.install(archive, envelopeDigest: Data(repeating: 2, count: 32),
            expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        XCTAssertEqual(second.bindings[0].account, f.account); XCTAssertNotEqual(second.id, f.installed.id)
        XCTAssertEqual(f.vault.creates, 1); XCTAssertEqual(f.vault.enrichments, 1)
        XCTAssertEqual(f.vault.snapshot(f.account)?.privateContainer, original.privateContainer)
        XCTAssertEqual(f.vault.snapshot(f.account)?.protection, original.protection)
        XCTAssertEqual(try f.vault.publicIdentity(account: f.account)?.blob, f.archive.keys[0].publicBlob)
    }

    func testAllIncomingPairsUseProductionDerivationBeforeAnyReusableMetadataChanges() async throws {
        let f = try await fixture(), original = f.vault.snapshot(f.account), catalog = f.persistence.bytes(.catalog)
        let second = try keys()[1]
        var container = second.record.container, outer = ByteCursor(container)
        _ = try outer.take(15); for _ in 0..<3 { _ = try outer.sshString() }
        _ = try outer.uint(4); _ = try outer.sshString()
        let part = try outer.sshString(), partStart = outer.offset - part.count
        var inner = ByteCursor(part)
        _ = try inner.take(8); _ = try inner.sshString(); _ = try inner.sshString()
        let point = try inner.sshString(); _ = try inner.sshString()
        container[partStart + inner.offset - 1] = 2 // d=2 cannot have the d=1 public point.
        let forged = PrivateKeyRecord(credentialID: "other-key", container: container, publicBlob: second.record.publicBlob)
        let changed = try document(f.archive.workspace.raw) { root, workspace in
            root["bundle_id"] = "invalid-pair"
            var credentials = workspace["credentials"] as! [[String: Any]]
            credentials.append(["id": "other-key", "algorithm": second.algorithm, "public_fingerprint": second.fingerprint,
                "requirement": "matching_private_key", "private_material": "not_included"])
            workspace["credentials"] = credentials
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["credential_refs"] = ["key", "other-key"]; workspace["routes"] = routes
        }
        let archive = try AirGapArchive(workspace: changed, keys: [f.archive.keys[0], forged],
            using: MisreportingPointCryptography(point: point))
        do {
            _ = try await f.store.install(archive, envelopeDigest: Data(repeating: 2, count: 32),
                expectedRevision: f.installed.revision, sourceAndContentsConfirmed: true,
                securityChangesConfirmed: true, associateWithExistingWorkspace: true)
            XCTFail("A provider's claimed point cannot replace production private-key validation")
        } catch { XCTAssertEqual(error as? FTError, .malformed("key.identity")) }
        XCTAssertEqual(f.vault.snapshot(f.account), original); XCTAssertEqual(f.vault.enrichments, 0)
        XCTAssertEqual(f.vault.creates, 1); XCTAssertEqual(f.persistence.bytes(.catalog), catalog)
        XCTAssertNil(f.persistence.bytes(.journal))
    }
}
