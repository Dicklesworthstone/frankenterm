import Foundation
import XCTest
@testable import FrankenTermKit
#if canImport(CryptoKit)
import CryptoKit
#else
import Crypto
#endif

/// The parser oracle checks OpenSSH compatibility. These tests exercise the
/// actual manifest, durable store and credential-authorization boundaries.
@MainActor
final class WorkspaceHostRevocationPolicyTests: XCTestCase {
    private typealias Policy = WorkspaceHostRevocationPolicy
    private typealias Source = WorkspaceHostRevocationSource
    private let crypto = TestCryptography()
    private var support: WorkspaceDocument.RouteSupport {
        .init(routeModes: ["direct_ssh", "frankenterm_mux"],
            authenticationMethods: [.publicKey, .password, .keyboardInteractive],
            credentialAlgorithms: ["ssh-ed25519"],
            hostKeyAlgorithms: ["ssh-ed25519", "ssh-ed25519-cert-v01@openssh.com"],
            capabilities: [WorkspaceDocument.hostCertificateCapability, WorkspaceDocument.hostRevocationCapability,
                WorkspaceDocument.hostAlgorithmPolicyCapability], maximumJumpHops: 3,
            hostSignatureAlgorithms: ["ssh-ed25519", "ssh-ed25519-cert-v01@openssh.com"])
    }
    private struct Fixture {
        let store: WorkspaceStore
        let disk: StoreTestPersistence
        let vault: StoreTestVault
        let saved: InstalledWorkspace
        let document: WorkspaceDocument
    }
    private func identity() throws -> SSHPublicIdentity {
        try SSHPublicIdentity(blob: sampleKey(using: crypto).0.publicBlob)
    }
    private func key(_ identity: SSHPublicIdentity) throws -> WorkspaceDocument.HostKey {
        try .init(algorithm: identity.algorithm, fingerprint: identity.fingerprint)
    }
    private func key(_ certificate: SSHHostCertificate) throws -> WorkspaceDocument.HostKey {
        try .init(algorithm: certificate.algorithm, fingerprint: certificate.fingerprint)
    }
    private func fields(_ key: WorkspaceDocument.HostKey) -> [String: Any] {
        ["algorithm": key.algorithm, "fingerprint": key.fingerprint]
    }
    private func source(_ id: String, policy: Policy = .empty, version: UInt64 = 1) throws -> Source {
        let digest = Data(SHA256.hash(data: Data(id.utf8))).base64EncodedString().replacingOccurrences(of: "=", with: "")
        return try .init(id: id, sourceKind: "openssh_krl_v1", sourceSHA256: "SHA256:" + digest,
                         krlVersion: version, generatedAt: 0, policy: policy)
    }
    private func changed(_ document: WorkspaceDocument, bundle: String? = nil,
                         edit: (inout [String: Any]) -> Void) throws -> WorkspaceDocument {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: document.raw) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        edit(&workspace); root["workspace"] = workspace
        if let bundle { root["bundle_id"] = bundle }
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }
    private func document(sources: [Source] = [], certificate: SSHHostCertificate? = nil,
                          rawPins: Bool = true, bundle: String = "revocation-policy",
                          edit: (inout [String: Any]) -> Void = { _ in }) throws -> WorkspaceDocument {
        let (_, fingerprint) = try sampleKey(using: crypto), pin = try key(identity())
        let original = try WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint, bundleID: bundle))
        let resources = try sources.map { try JSONSerialization.jsonObject(with: StrictJSON.encode($0.json)) }
        return try changed(original) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]], required: [String] = []
            trusts[0]["pins"] = rawPins ? [fields(pin)] : []
            if !sources.isEmpty {
                workspace["host_revocation_sources"] = resources
                trusts[0]["revocation_source_refs"] = sources.map(\.id)
                required.append(WorkspaceDocument.hostRevocationCapability)
            }
            if let certificate {
                trusts[0]["certificate_authorities"] = [["algorithm": certificate.authority.algorithm,
                    "fingerprint": certificate.authority.fingerprint]]
                required.append(WorkspaceDocument.hostCertificateCapability)
            }
            trusts[0]["required_capabilities"] = required; workspace["host_trust"] = trusts
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["authentication_methods"] = ["publickey", "password", "keyboard-interactive"]
            workspace["routes"] = routes; edit(&workspace)
        }
    }
    private func install(_ document: WorkspaceDocument, in store: WorkspaceStore, identity: UInt8,
                         replacing old: InstalledWorkspace? = nil, rollback: Bool = false) async throws -> InstalledWorkspace {
        let (privateKey, _) = try sampleKey(using: crypto)
        let archive = try AirGapArchive(workspace: document, keys: [privateKey], using: crypto)
        return try await store.install(archive, envelopeDigest: Data(repeating: identity, count: 32),
            transferID: Data(repeating: identity, count: 16), expectedRevision: old?.revision,
            sourceAndContentsConfirmed: true, allowHistoricalRollback: rollback,
            securityChangesConfirmed: old != nil, associateWithExistingWorkspace: old != nil).workspace
    }
    private func fixture(_ document: WorkspaceDocument) async throws -> Fixture {
        let disk = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: disk, vault: vault)
        let saved = try await install(document, in: store, identity: 1)
        return .init(store: store, disk: disk, vault: vault, saved: saved, document: document)
    }
    private func verify(_ document: WorkspaceDocument, identity: SSHPublicIdentity? = nil,
                        certificate: SSHHostCertificate? = nil,
                        profile: WorkspaceDocument.RouteSupport? = nil) throws -> WorkspaceDocument.HostVerificationDecision {
        let presented = try certificate.map(key) ?? key(self.identity())
        return try document.verifyHostKey(forRouteID: "route", presented: presented, hostCertificate: certificate,
            rawHostIdentity: identity, reviewedTrustIDs: Set(document.hostTrust.map(\.id)), support: profile ?? support)
    }
    private func catalog(_ disk: StoreTestPersistence) throws -> [String: Any] {
        try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(disk.bytes(.catalog))) as? [String: Any])
    }
    private func seed(_ fields: [String: Any], in disk: StoreTestPersistence) throws {
        disk.seed(try JSONSerialization.data(withJSONObject: fields, options: [.sortedKeys]), slot: .catalog)
    }
    private func appendHistory(_ source: Source, to f: Fixture) throws {
        var fields = try catalog(f.disk)
        var pool = try XCTUnwrap(fields["hostRevocationSources"] as? [[String: Any]])
        pool.append(["workspaceID": f.saved.id.uuidString,
                     "source": try JSONSerialization.jsonObject(with: JSONEncoder().encode(source))])
        var scopes = try XCTUnwrap(fields["hostRevocationSourceScopes"] as? [[String: Any]])
        var keys = try XCTUnwrap(scopes[0]["sourceKeys"] as? [String])
        keys.append(source.sourceKind + ":" + source.sourceSHA256); scopes[0]["sourceKeys"] = keys
        fields["hostRevocationSources"] = pool; fields["hostRevocationSourceScopes"] = scopes
        try seed(fields, in: f.disk)
    }
    private func sources(_ f: Fixture, saved: InstalledWorkspace? = nil,
                         store: WorkspaceStore? = nil) async throws -> [Source] {
        let saved = saved ?? f.saved
        return try await (store ?? f.store).hostRevocationSources(workspaceID: saved.id,
            expectedRevision: saved.revision, routeID: "route", support: support)
    }
    private func expectDenied(file: StaticString = #filePath, line: UInt = #line,
                              _ operation: () async throws -> Void) async {
        do { try await operation(); XCTFail("revoked or incomplete evidence was accepted", file: file, line: line) }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied, file: file, line: line) }
    }

    func testManifestReferencesRequireBoundedCompleteSnapshotsAndCapability() async throws {
        let snapshot = try source("empty-snapshot"), original = try document(sources: [snapshot])
        XCTAssertEqual(original.hostRevocationSources, [snapshot])
        XCTAssertEqual(try original.hostRevocationSources(forTrustID: "trust"), [snapshot])
        let implicit = try changed(original) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0].removeValue(forKey: "required_capabilities"); workspace["host_trust"] = trusts
        }
        XCTAssertTrue(implicit.hostTrust[0].requiredPolicyRequirements.contains(WorkspaceDocument.hostRevocationCapability))
        let legacy = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
            credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"])
        XCTAssertEqual(try verify(implicit, identity: identity(), profile: legacy), .unsupportedPolicy)
        XCTAssertEqual(try verify(original, identity: identity()), .trustedPin)
        for bad in ["missing-reference", "duplicate-resource", "missing-payload", "contradictory-snapshot"] {
            XCTAssertThrowsError(try changed(original) { workspace in
                var trusts = workspace["host_trust"] as! [[String: Any]]
                var resources = workspace["host_revocation_sources"] as! [[String: Any]]
                switch bad {
                case "missing-reference": trusts[0]["revocation_source_refs"] = ["absent"]
                case "duplicate-resource": resources.append(resources[0])
                case "missing-payload": trusts[0].removeValue(forKey: "revocation_source_refs")
                default:
                    var contradictory = resources[0]; contradictory["id"] = "renamed-source"
                    contradictory["revoked_sha256"] = [Data(repeating: 1, count: 32).base64EncodedString()]
                    resources.append(contradictory)
                }
                workspace["host_trust"] = trusts; workspace["host_revocation_sources"] = resources
            }, bad)
        }
    }

    func testSHA1RequiresTheExactActualRawKeyWitnessBeforeAnyCredentialGrant() async throws {
        let actual = try identity(), other = try SSHHostCertificate(blob: workspaceTestHostCertificate(subjectSeed: Data(64..<96))).subject
        let permitted = try source("different-key", policy: Policy(revokedSHA1: [Data(Insecure.SHA1.hash(data: other.blob))]))
        let document = try document(sources: [permitted])
        XCTAssertEqual(try verify(document), .unsupportedPolicy)
        XCTAssertEqual(try verify(document, identity: other), .unsupportedPolicy)
        XCTAssertEqual(try verify(document, identity: actual), .trustedPin)
        let denied = try source("actual-key", policy: Policy(revokedSHA1: [Data(Insecure.SHA1.hash(data: actual.blob))]))
        XCTAssertEqual(try verify(self.document(sources: [denied]), identity: actual), .revoked)
        let f = try await fixture(document), presented = try key(actual)
        await expectDenied { _ = try await f.store.authorizeSSHAuthentication(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, routeID: "route", credentialID: "key", presentedHostKey: presented,
            support: self.support) }
        await expectDenied { _ = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, routeID: "route", presentedHostKey: presented,
            rawHostIdentity: other, support: self.support) }
        await expectDenied { _ = try await f.store.authorizeSSHKeyboardInteractiveAuthentication(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, routeID: "route", presentedHostKey: presented, support: self.support) }
        let accepted = try await f.store.authorizeSSHAuthentication(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, routeID: "route", credentialID: "key", presentedHostKey: presented,
            rawHostIdentity: actual, support: support)
        XCTAssertEqual(accepted.rawHostIdentity, actual)
    }

    func testCertificateSerialKeyIDSubjectAndIssuerDenialsPrecedePositiveCATrust() async throws {
        let id = Data([0xff, 0x65, 0xcc, 0x81])
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate(serial: 42, keyID: id))
        let differentCA = try SSHHostCertificate(blob: workspaceTestHostCertificate(authoritySeed: Data(64..<96))).authority
        let rules: [Policy] = [
            try .init(certificates: [.init(authoritySHA256: certificate.authority.fingerprint,
                serialRanges: [.init(first: 40, last: 45)])]),
            try .init(certificates: [.init(keyIDs: [id])]),
            try .init(revokedSHA256: [Data(SHA256.hash(data: certificate.subject.blob))]),
            try .init(revokedSHA1: [Data(Insecure.SHA1.hash(data: certificate.authority.blob))])
        ]
        for (index, rule) in rules.enumerated() {
            let denied = try document(sources: [source("certificate-denial-\(index)", policy: rule)], certificate: certificate, rawPins: false)
            XCTAssertEqual(try verify(denied, certificate: certificate), .revoked)
            XCTAssertEqual(try verify(denied, certificate: certificate, profile: .offlineOnly), .revoked,
                           "known negative evidence must not become merely unsupported trust")
        }
        let unrelated = try source("other-ca", policy: Policy(certificates: [.init(authoritySHA256: differentCA.fingerprint,
            serialRanges: [.init(first: 1, last: UInt64.max)], keyIDs: [id])]))
        XCTAssertEqual(try verify(document(sources: [unrelated], certificate: certificate, rawPins: false), certificate: certificate), .trustedCertificate)
        let serials = try source("all-nonzero", policy: Policy(certificates: [.init(serialRanges: [.init(first: 1, last: UInt64.max)])]))
        let zero = try SSHHostCertificate(blob: workspaceTestHostCertificate(serial: 0, keyID: Data()))
        XCTAssertEqual(try verify(document(sources: [serials], certificate: zero, rawPins: false), certificate: zero), .trustedCertificate)
        XCTAssertEqual(try verify(document(sources: [unrelated], certificate: certificate), identity: certificate.subject,
                                  certificate: certificate), .unsupportedPolicy, "a raw identity cannot substitute for a certificate witness")
    }

    func testEveryGrantRetainsRawEvidenceAndRejectsNewHistoryWithoutRevisionChange() async throws {
        let actual = try identity(), presented = try key(actual)
        let f = try await fixture(document(sources: [source("reviewed-empty")]))
        let keyGrant = try await f.store.authorizeSSHAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: presented, rawHostIdentity: actual, support: support)
        let password = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", presentedHostKey: presented, rawHostIdentity: actual, support: support)
        let interactive = try await f.store.authorizeSSHKeyboardInteractiveAuthentication(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, routeID: "route", presentedHostKey: presented, rawHostIdentity: actual, support: support)
        XCTAssertEqual(keyGrant.rawHostIdentity, actual); XCTAssertEqual(password.rawHostIdentity, actual)
        XCTAssertEqual(interactive.rawHostIdentity, actual)
        try await f.store.validateSSHAuthentication(keyGrant, support: support)
        try await f.store.validateSSHPasswordAuthentication(password, support: support)
        try await f.store.validateSSHKeyboardInteractiveAuthentication(interactive, support: support)
        try appendHistory(source("local-sha1-denial", policy: Policy(revokedSHA1: [Data(Insecure.SHA1.hash(data: actual.blob))])), to: f)
        await expectDenied { try await f.store.validateSSHAuthentication(keyGrant, support: self.support) }
        await expectDenied { try await f.store.validateSSHPasswordAuthentication(password, support: self.support) }
        await expectDenied { try await f.store.validateSSHKeyboardInteractiveAuthentication(interactive, support: self.support) }
        let loaded = try await f.store.load()
        XCTAssertEqual(loaded.first?.revision, f.saved.revision); XCTAssertEqual(f.vault.creates, 1)
    }

    func testEveryCertificateGrantRechecksRetainedExactSerialAndKeyID() async throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate(serial: 91, keyID: Data([0xff, 65])))
        let f = try await fixture(document(sources: [source("empty-certificate-source")], certificate: certificate, rawPins: false))
        let presented = try key(certificate)
        let keyGrant = try await f.store.authorizeSSHAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: presented, hostCertificate: certificate, support: support)
        let password = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", presentedHostKey: presented, hostCertificate: certificate, support: support)
        let interactive = try await f.store.authorizeSSHKeyboardInteractiveAuthentication(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, routeID: "route", presentedHostKey: presented, hostCertificate: certificate, support: support)
        XCTAssertEqual(keyGrant.hostCertificate, certificate); XCTAssertEqual(password.hostCertificate, certificate)
        XCTAssertEqual(interactive.hostCertificate?.keyID, Data([0xff, 65])); XCTAssertNil(keyGrant.rawHostIdentity)
        try appendHistory(source("serial-revoked", policy: Policy(certificates: [.init(authoritySHA256: certificate.authority.fingerprint,
            serialRanges: [.init(first: 91, last: 91)])])), to: f)
        await expectDenied { try await f.store.validateSSHAuthentication(keyGrant, support: self.support) }
        await expectDenied { try await f.store.validateSSHPasswordAuthentication(password, support: self.support) }
        await expectDenied { try await f.store.validateSSHKeyboardInteractiveAuthentication(interactive, support: self.support) }
    }

    func testReimportRemovalReopenAndHistoricalRollbackNeverReplaceEarlierDenials() async throws {
        let raw = try identity()
        let first = try source("old-high-version", policy: Policy(revokedSHA256: [Data(SHA256.hash(data: raw.blob))]), version: UInt64.max)
        let second = try source("new-low-version", policy: Policy(revokedSHA1: [Data(repeating: 19, count: 20)]), version: 0)
        let original = try document(sources: [first]), f = try await fixture(original)
        let revised = try await install(document(sources: [second], bundle: "new-low-version"), in: f.store, identity: 2, replacing: f.saved)
        let removed = try await install(document(bundle: "sources-removed"), in: f.store, identity: 3, replacing: revised)
        let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault)
        let retained = try await sources(f, saved: removed, store: reopened)
        XCTAssertEqual(Set(retained.map(\.sourceSHA256)), [first.sourceSHA256, second.sourceSHA256])
        let incoming = try document(bundle: "preview-without-sources")
        let preview = try await reopened.preview(incoming, envelopeDigest: Data(repeating: 7, count: 32))
        XCTAssertEqual(preview.preservedHostRevocationSourceCount, 2)
        let assessment = try await reopened.assessImport(incoming, envelopeDigest: Data(repeating: 7, count: 32),
            transferID: Data(repeating: 7, count: 16))
        XCTAssertEqual(assessment.preservedHostRevocationSourceCount, 2)
        XCTAssertEqual(assessment.preservedHostRevocationSourcesByTrustScope.values.flatMap { $0 }, retained)
        let restore = OfflineRestore(using: crypto, store: reopened)
        let initial = await restore.status()
        let ready = try await restore.acceptWorkspaceDocument(incoming.raw, generation: initial.generation)
        let review = try XCTUnwrap(ready.review)
        XCTAssertEqual(ready.phase, .review)
        XCTAssertEqual(review.preservedHostRevocationSourceCount, 2)
        XCTAssertEqual(review.preservedHostRevocationSourcesByTrustScope,
            assessment.preservedHostRevocationSourcesByTrustScope)
        let decision = try await reopened.verifyHostKey(workspaceID: removed.id, routeID: "route", presented: key(raw), rawHostIdentity: raw, support: support)
        XCTAssertEqual(decision, .revoked)
        let rolledBack = try await install(original, in: reopened, identity: 1, replacing: removed, rollback: true)
        let afterRollback = try await sources(f, saved: rolledBack, store: reopened)
        XCTAssertEqual(afterRollback, retained)
        let renamed = try Source(id: "renamed-resource", sourceKind: first.sourceKind, sourceSHA256: first.sourceSHA256,
            krlVersion: first.krlVersion, generatedAt: first.generatedAt, policy: first.policy)
        let latest = try await install(document(sources: [renamed], bundle: "renamed-source"), in: reopened, identity: 4, replacing: rolledBack)
        let afterRename = try await sources(f, saved: latest, store: reopened)
        XCTAssertEqual(afterRename.count, 2)
        XCTAssertEqual((try catalog(f.disk)["hostRevocationSources"] as? [Any])?.count, 2)
        XCTAssertEqual(f.vault.creates, 1)
    }

    func testScopeTombstonesSurviveTrustRenameWithoutLeakingAcrossPortOrAlias() async throws {
        let snapshot = try source("scoped-denial", policy: Policy(revokedSHA256: [Data(SHA256.hash(data: identity().blob))]))
        let f = try await fixture(document(sources: [snapshot]))
        let differentPort = try document(bundle: "other-port") { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]], routes = workspace["routes"] as! [[String: Any]]
            trusts[0]["port"] = 2222; routes[0]["port"] = 2222
            workspace["host_trust"] = trusts; workspace["routes"] = routes
        }
        let second = try await install(differentPort, in: f.store, identity: 2, replacing: f.saved)
        let otherPortSources = try await sources(f, saved: second)
        XCTAssertTrue(otherPortSources.isEmpty)
        let differentAlias = try document(bundle: "other-alias") { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]], routes = workspace["routes"] as! [[String: Any]]
            trusts[0]["host_key_alias"] = "separate.example.invalid"; routes[0]["host_key_alias"] = "separate.example.invalid"
            workspace["host_trust"] = trusts; workspace["routes"] = routes
        }
        let third = try await install(differentAlias, in: f.store, identity: 3, replacing: second)
        let otherAliasSources = try await sources(f, saved: third)
        XCTAssertTrue(otherAliasSources.isEmpty)
        let returned = try document(bundle: "renamed-trust") { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]], routes = workspace["routes"] as! [[String: Any]]
            trusts[0]["id"] = "renamed-trust"; routes[0]["host_trust_ref"] = "renamed-trust"
            workspace["host_trust"] = trusts; workspace["routes"] = routes
        }
        let fourth = try await install(returned, in: f.store, identity: 4, replacing: third)
        let restoredSources = try await sources(f, saved: fourth)
        XCTAssertEqual(restoredSources, [snapshot])
        let unrelatedWorkspace = try changed(returned, bundle: "independent-workspace") { $0["id"] = "other-workspace" }
        let other = try await install(unrelatedWorkspace, in: f.store, identity: 5)
        let otherWorkspaceSources = try await sources(f, saved: other)
        XCTAssertTrue(otherWorkspaceSources.isEmpty)
    }

    func testCurrentAliasesUnionPoliciesOnlyWithinTheCanonicalTrustScope() async throws {
        let snapshot = try source("alias-denial", policy: Policy(revokedSHA256: [Data(SHA256.hash(data: identity().blob))]))
        let original = try document(sources: [snapshot])
        let aliases = try changed(original) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]], alias = trusts[0]
            trusts[0].removeValue(forKey: "revocation_source_refs"); trusts[0]["required_capabilities"] = []
            alias["id"] = "same-scope"; alias["host"] = "HOST.EXAMPLE.INVALID."
            trusts.append(alias); workspace["host_trust"] = trusts
        }
        XCTAssertEqual(try verify(aliases, identity: identity()), .revoked)
        let otherPort = try changed(aliases) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]; trusts[1]["port"] = 2222; workspace["host_trust"] = trusts
        }
        XCTAssertEqual(try verify(otherPort, identity: identity()), .trustedPin)
        XCTAssertTrue(try otherPort.hostRevocationSources(forTrustID: "trust").isEmpty)
    }

    func testContradictoryProvenanceCannotReplaceAcceptedSourceOrPublishAnyPartialImport() async throws {
        let first = try source("immutable-source", policy: Policy(revokedSHA256: [Data(repeating: 12, count: 32)]))
        let f = try await fixture(document(sources: [first]))
        let forged = try Source(id: "new-resource-name", sourceKind: first.sourceKind, sourceSHA256: first.sourceSHA256,
            krlVersion: first.krlVersion, generatedAt: first.generatedAt, policy: .empty)
        let before = f.disk.bytes(.catalog), creates = f.vault.creates
        do {
            _ = try await install(document(sources: [forged], bundle: "contradiction"), in: f.store, identity: 2, replacing: f.saved)
            XCTFail("a digest claim must not overwrite a different normalized policy")
        } catch { XCTAssertEqual(error as? FTError, .malformed("trust.revocation_source_conflict")) }
        XCTAssertEqual(f.disk.bytes(.catalog), before); XCTAssertEqual(f.vault.creates, creates)
        let retained = try await sources(f)
        XCTAssertEqual(retained, [first])
    }

    func testCatalogCorruptionCannotDropDuplicateWeakenOrDowngradeRevocationHistory() async throws {
        let first = try source("catalog-denial", policy: Policy(revokedSHA256: [Data(repeating: 13, count: 32)]))
        let f = try await fixture(document(sources: [first])), original = try catalog(f.disk)
        for mutation in ["missing-pool", "missing-scopes", "duplicate-source", "duplicate-scope", "unresolved-reference",
                         "weak-source", "marker", "downgraded", "legacy-manifest"] {
            var fields = original
            var pool = try XCTUnwrap(fields["hostRevocationSources"] as? [[String: Any]])
            var scopes = try XCTUnwrap(fields["hostRevocationSourceScopes"] as? [[String: Any]])
            switch mutation {
            case "missing-pool": pool = []
            case "missing-scopes": scopes = []
            case "duplicate-source": pool.append(pool[0])
            case "duplicate-scope": scopes.append(scopes[0])
            case "unresolved-reference": scopes[0]["sourceKeys"] = ["openssh_krl_v1:SHA256:absent"]
            case "weak-source":
                let weak = try Source(id: first.id, sourceKind: first.sourceKind, sourceSHA256: first.sourceSHA256,
                    krlVersion: first.krlVersion, generatedAt: first.generatedAt, policy: .empty)
                pool[0]["source"] = try JSONSerialization.jsonObject(with: JSONEncoder().encode(weak))
            case "marker": fields["hostRequirements"] = []
            default: fields["version"] = 10
            }
            fields["hostRevocationSources"] = pool; fields["hostRevocationSourceScopes"] = scopes
            if mutation == "legacy-manifest" {
                fields.removeValue(forKey: "hostRevocationSources"); fields.removeValue(forKey: "hostRevocationSourceScopes")
            }
            try seed(fields, in: f.disk); let damaged = f.disk.bytes(.catalog)
            do { _ = try await WorkspaceStore(persistence: f.disk, vault: f.vault).load(); XCTFail(mutation) }
            catch { XCTAssertEqual(error as? WorkspaceStoreError, .corruptStore, mutation) }
            XCTAssertEqual(f.disk.bytes(.catalog), damaged); XCTAssertEqual(f.vault.count, 1)
        }
        try seed(original, in: f.disk)
        _ = try await install(document(bundle: "removed-source-fields"), in: f.store, identity: 2, replacing: f.saved)
        var missingTombstone = try catalog(f.disk); missingTombstone["hostRevocationSources"] = []
        try seed(missingTombstone, in: f.disk)
        do { _ = try await WorkspaceStore(persistence: f.disk, vault: f.vault).load(); XCTFail("removed source history vanished") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .corruptStore) }
    }

    func testCatalogTenMigrationPreservesExistingPolicyWithoutInventingRevocationSources() async throws {
        let legacy = try document { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["host_key_algorithms"] = ["ssh-ed25519"]
            trusts[0]["required_capabilities"] = [WorkspaceDocument.hostAlgorithmPolicyCapability]
            workspace["host_trust"] = trusts
        }
        let f = try await fixture(legacy)
        var fields = try catalog(f.disk); fields["version"] = 10
        fields.removeValue(forKey: "hostRevocationSources"); fields.removeValue(forKey: "hostRevocationSourceScopes")
        let history = try JSONSerialization.data(withJSONObject: XCTUnwrap(fields["acceptedImports"]), options: [.sortedKeys])
        try seed(fields, in: f.disk)
        let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault), loaded = try await reopened.load()
        XCTAssertEqual(loaded.first?.manifest, f.saved.manifest); XCTAssertEqual(loaded.first?.revision, f.saved.revision)
        let migratedSources = try await sources(f, store: reopened)
        XCTAssertTrue(migratedSources.isEmpty)
        let policy = try await reopened.hostAlgorithmPolicy(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", support: support)
        XCTAssertEqual(policy.hostKeyAlgorithms, ["ssh-ed25519"])
        let upgraded = try catalog(f.disk)
        XCTAssertEqual(upgraded["version"] as? Int, 12)
        XCTAssertEqual((upgraded["hostRevocationSources"] as? [Any])?.count, 0)
        XCTAssertEqual((upgraded["hostRevocationSourceScopes"] as? [Any])?.count, 0)
        XCTAssertEqual(try JSONSerialization.data(withJSONObject: XCTUnwrap(upgraded["acceptedImports"]), options: [.sortedKeys]), history)
        XCTAssertEqual(f.vault.creates, 1)
    }

    func testSourceSnapshotsCannotClearHistoricalUnresolvedPolicyAndProbeReadsStayPublic() async throws {
        let snapshot = try source("empty-public-snapshot")
        let unconfigured = try await fixture(document(sources: [snapshot], rawPins: false))
        unconfigured.vault.configure(.readFingerprint)
        let publicSources = try await sources(unconfigured)
        XCTAssertEqual(publicSources, [snapshot], "pre-handshake policy lookup must not require a private-key read or a positive pin")
        unconfigured.vault.configure(nil)
        do {
            _ = try await unconfigured.store.hostRevocationSources(workspaceID: unconfigured.saved.id,
                expectedRevision: UUID(), routeID: "route", support: support)
            XCTFail("stale probe received a current source snapshot")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
        let unresolved = try document { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["required_capabilities"] = ["ssh_config.unresolved_policy"]; workspace["host_trust"] = trusts
        }
        let f = try await fixture(unresolved)
        let revised = try await install(document(sources: [snapshot], bundle: "now-has-krl"), in: f.store, identity: 2, replacing: f.saved)
        await expectDenied { _ = try await self.sources(f, saved: revised) }
        await expectDenied { _ = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: revised.id,
            expectedRevision: revised.revision, routeID: "route", presentedHostKey: self.key(self.identity()),
            rawHostIdentity: self.identity(), support: self.support) }
        let local = try await f.store.loadWithLocalPolicy(support: support)
        XCTAssertTrue(try XCTUnwrap(local.first).requiredHostCapabilitiesByTrustScope.values.contains {
            $0.contains("ssh_config.unresolved_policy") && $0.contains(WorkspaceDocument.hostRevocationCapability)
        })
    }

    func testSameResourceIDContentChangeRequiresSecurityApprovalAndInvalidatesLocalWitnesses() async throws {
        let before = try source("stable-resource")
        let revisedBytes = try source("replacement-bytes", policy: Policy(revokedSHA256: [Data(repeating: 44, count: 32)]), version: 2)
        let after = try Source(id: before.id, sourceKind: revisedBytes.sourceKind, sourceSHA256: revisedBytes.sourceSHA256,
            krlVersion: revisedBytes.krlVersion, generatedAt: revisedBytes.generatedAt, policy: revisedBytes.policy)
        let original = try document(sources: [before], rawPins: false)
        let incoming = try document(sources: [after], rawPins: false, bundle: "same-resource-new-contents")
        XCTAssertEqual(original.hostTrust[0].revocationSourceIDs, incoming.hostTrust[0].revocationSourceIDs)
        let review = try WorkspaceChangeReview(previous: original, incoming: incoming)
        XCTAssertTrue(review.requiresSecurityApproval)
        XCTAssertTrue(review.securityChanges.contains { $0.entity == .hostRevocationSource
            && $0.entityID == before.id && $0.field == "revoked_sha256" })
        XCTAssertFalse(review.changes.contains { $0.entity == .hostTrust }, "unchanged references cannot hide a changed resource")

        let pinWitness = try WorkspaceHostKeyOverlay.witness(trustID: "trust", in: original)
        let deviceWitness = try WorkspaceDeviceKeyOverlay.witness(routeID: "route", in: original)
        XCTAssertNotEqual(try WorkspaceHostKeyOverlay.witness(trustID: "trust", in: incoming), pinWitness)
        XCTAssertNotEqual(try WorkspaceDeviceKeyOverlay.witness(routeID: "route", in: incoming), deviceWitness)
        let pin = WorkspaceHostKeyPin(trustID: "trust", key: try key(identity()), sourceWitness: pinWitness)
        let oldOverlay = try WorkspaceHostKeyOverlay.document(effective: original, imported: original, pins: [pin])
        XCTAssertEqual(oldOverlay.hostTrust[0].pins, [pin.key])
        let newOverlay = try WorkspaceHostKeyOverlay.document(effective: incoming, imported: incoming, pins: [pin])
        XCTAssertTrue(newOverlay.hostTrust[0].pins.isEmpty)
        let assignments = try WorkspaceDeviceKeyOverlay.assignments(routeIDs: ["route"], credentialID: "key", in: original)
        XCTAssertTrue(try WorkspaceDeviceKeyOverlay.reviewRequiredRouteIDs(imported: original, assignments: assignments).isEmpty)
        XCTAssertEqual(try WorkspaceDeviceKeyOverlay.reviewRequiredRouteIDs(imported: incoming, assignments: assignments), ["route"])
        let reissued = try original.reissuingBundle(id: "identical-resource-new-envelope")
        XCTAssertEqual(try WorkspaceHostKeyOverlay.witness(trustID: "trust", in: reissued), pinWitness)
        XCTAssertEqual(try WorkspaceDeviceKeyOverlay.witness(routeID: "route", in: reissued), deviceWitness)

        let f = try await fixture(original), (privateKey, _) = try sampleKey(using: crypto)
        let archive = try AirGapArchive(workspace: incoming, keys: [privateKey], using: crypto), bytes = f.disk.bytes(.catalog)
        do {
            _ = try await f.store.install(archive, envelopeDigest: Data(repeating: 2, count: 32),
                transferID: Data(repeating: 2, count: 16), expectedRevision: f.saved.revision,
                sourceAndContentsConfirmed: true, securityChangesConfirmed: false, associateWithExistingWorkspace: true)
            XCTFail("a reference-only review silently approved replacement revocation bytes")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .securityChangesNeedApproval) }
        XCTAssertEqual(f.disk.bytes(.catalog), bytes); XCTAssertEqual(f.vault.creates, 1)
    }

    func testResourcePortabilityFollowsAliasScopeAndJumpDependenciesOnly() async throws {
        let required = try source("required-hop-source"), unused = try source("unreferenced-source")
        let graph = try document(sources: [required, unused]) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]], routes = workspace["routes"] as! [[String: Any]]
            trusts[0].removeValue(forKey: "revocation_source_refs"); trusts[0]["required_capabilities"] = []
            var hopTrust = trusts[0], aliasTrust = trusts[0], independentTrust = trusts[0]
            hopTrust["id"] = "hop-trust"; hopTrust["host"] = "jump.example.invalid"
            aliasTrust["id"] = "hop-policy-alias"; aliasTrust["host"] = "JUMP.EXAMPLE.INVALID."
            aliasTrust["revocation_source_refs"] = [required.id]
            aliasTrust["required_capabilities"] = [WorkspaceDocument.hostRevocationCapability]
            independentTrust["id"] = "independent-trust"; independentTrust["host"] = "independent.example.invalid"
            trusts += [hopTrust, aliasTrust, independentTrust]
            var hop = routes[0], independent = routes[0]
            routes[0]["jump_routes"] = ["hop"]
            hop["id"] = "hop"; hop["host"] = "jump.example.invalid"; hop["host_trust_ref"] = "hop-trust"
            independent["id"] = "independent"; independent["host"] = "independent.example.invalid"
            independent["host_trust_ref"] = "independent-trust"
            routes += [hop, independent]
            workspace["host_trust"] = trusts; workspace["routes"] = routes
            var machines = workspace["machines"] as! [[String: Any]]
            machines[1]["route_refs"] = ["independent"]; machines[1]["preferred_route_ref"] = "independent"
            workspace["machines"] = machines
        }
        XCTAssertTrue(graph.entityIDs.contains(required.id)); XCTAssertTrue(graph.entityIDs.contains(unused.id))
        XCTAssertEqual(WorkspacePortabilityPolicy.blockedRouteIDs(in: graph, requiredEntityIDs: [required.id]), ["hop", "route"])
        XCTAssertTrue(WorkspacePortabilityPolicy.blockedRouteIDs(in: graph, requiredEntityIDs: [unused.id]).isEmpty)
        XCTAssertEqual(WorkspacePortabilityPolicy.blockedRouteIDs(in: graph, requiredEntityIDs: ["unknown-source"]),
                       ["hop", "route", "independent"])
        let baseline = try WorkspaceDeviceKeyOverlay.witness(routeID: "route", in: graph)
        let changedPolicy = try changed(graph) { workspace in
            var resources = workspace["host_revocation_sources"] as! [[String: Any]]
            resources[0]["revoked_sha256"] = [Data(repeating: 55, count: 32).base64EncodedString()]
            workspace["host_revocation_sources"] = resources
        }
        XCTAssertNotEqual(try WorkspaceDeviceKeyOverlay.witness(routeID: "route", in: changedPolicy), baseline,
                          "destination identity review binds the hop's same-scope source content")
        XCTAssertEqual(try WorkspaceDeviceKeyOverlay.witness(routeID: "independent", in: changedPolicy),
                       try WorkspaceDeviceKeyOverlay.witness(routeID: "independent", in: graph))
    }
}
