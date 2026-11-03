import Foundation
import XCTest
@testable import FrankenTermKit

private struct HostAlgorithmPolicySigner: WorkspaceSSHSigning {
    let container: Data
    func cancel(requestID: UUID) {}
    func publicIdentity(requestID: UUID, authorization: WorkspaceSSHAuthorization) async throws -> Data {
        try SSHUserAuthenticationSigner.publicIdentity(privateContainer: container).blob
    }
    func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
              challenge: SSHUserAuthenticationChallenge) async throws -> Data {
        try SSHUserAuthenticationSigner.sign(challenge, privateContainer: container)
    }
}

/// Exercise signature evidence, signed certificate policy, and the durable
/// intersection through the real import and authentication boundaries.
@MainActor
final class WorkspaceHostAlgorithmPolicyTests: XCTestCase {
    private let crypto = TestCryptography()
    private let signatures: Set<String> = ["ssh-ed25519", "rsa-sha2-512", "rsa-sha2-256",
        "ssh-ed25519-cert-v01@openssh.com", "rsa-sha2-512-cert-v01@openssh.com", "rsa-sha2-256-cert-v01@openssh.com"]
    private var support: WorkspaceDocument.RouteSupport {
        .init(routeModes: ["direct_ssh", "frankenterm_mux"],
            authenticationMethods: [.publicKey, .password, .keyboardInteractive],
            credentialAlgorithms: ["ssh-ed25519"],
            hostKeyAlgorithms: ["ssh-ed25519", "ssh-rsa", "ssh-ed25519-cert-v01@openssh.com", "ssh-rsa-cert-v01@openssh.com"],
            capabilities: [WorkspaceDocument.hostCertificateCapability, WorkspaceDocument.hostAlgorithmPolicyCapability],
            maximumJumpHops: 3, hostSignatureAlgorithms: signatures)
    }
    private struct Fixture {
        let store: WorkspaceStore
        let disk: StoreTestPersistence
        let vault: StoreTestVault
        let saved: InstalledWorkspace
        let document: WorkspaceDocument
    }
    private struct Grants {
        let key: WorkspaceSSHAuthorization
        let password: WorkspaceSSHPasswordAuthorization
        let interactive: WorkspaceSSHKeyboardInteractiveAuthorization
    }
    private func rawKey(_ algorithm: String = "ssh-rsa") throws -> WorkspaceDocument.HostKey {
        try .init(algorithm: algorithm, fingerprint: "SHA256:" + Data(repeating: 0, count: 32)
            .base64EncodedString().replacingOccurrences(of: "=", with: ""))
    }
    private func fields(_ key: WorkspaceDocument.HostKey) -> [String: Any] {
        ["algorithm": key.algorithm, "fingerprint": key.fingerprint]
    }
    private func changed(_ document: WorkspaceDocument, bundle: String? = nil,
                         edit: (inout [String: Any]) -> Void) throws -> WorkspaceDocument {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: document.raw) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        edit(&workspace); root["workspace"] = workspace
        if let bundle { root["bundle_id"] = bundle }
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }
    private func document(host: [String]? = nil, ca: [String]? = nil, bundle: String = "algorithm-policy",
                          certificate: SSHHostCertificate? = nil, rawPins: Bool = true,
                          edit: (inout [String: Any]) -> Void = { _ in }) throws -> WorkspaceDocument {
        let (_, fingerprint) = try sampleKey(using: crypto)
        let original = try WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint, bundleID: bundle))
        let pin = try rawKey()
        return try changed(original) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["pins"] = rawPins ? [fields(pin)] : []
            var required: [String] = []
            if let host { trusts[0]["host_key_algorithms"] = host }
            if let ca { trusts[0]["ca_signature_algorithms"] = ca }
            if host != nil || ca != nil { required.append(WorkspaceDocument.hostAlgorithmPolicyCapability) }
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
    private func archive(_ document: WorkspaceDocument) throws -> AirGapArchive {
        let (key, _) = try sampleKey(using: crypto)
        return try AirGapArchive(workspace: document, keys: [key], using: crypto)
    }
    private func install(_ document: WorkspaceDocument, in store: WorkspaceStore, identity: UInt8,
                         replacing previous: InstalledWorkspace? = nil, rollback: Bool = false) async throws -> InstalledWorkspace {
        try await store.install(archive(document), envelopeDigest: Data(repeating: identity, count: 32),
            transferID: Data(repeating: identity, count: 16), expectedRevision: previous?.revision,
            sourceAndContentsConfirmed: true, allowHistoricalRollback: rollback,
            securityChangesConfirmed: previous != nil, associateWithExistingWorkspace: previous != nil).workspace
    }
    private func fixture(_ document: WorkspaceDocument) async throws -> Fixture {
        let disk = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: disk, vault: vault)
        let saved = try await install(document, in: store, identity: 1)
        return .init(store: store, disk: disk, vault: vault, saved: saved, document: document)
    }
    private func readiness(_ document: WorkspaceDocument,
                           profile: WorkspaceDocument.RouteSupport? = nil) throws -> WorkspaceDocument.RouteReadiness {
        try document.readiness(forRouteID: "route", availableCredentialIDs: ["key"],
            reviewedTrustIDs: Set(document.hostTrust.map(\.id)), support: profile ?? support)
    }
    private func verify(_ document: WorkspaceDocument, signature: String?,
                        certificate: SSHHostCertificate? = nil,
                        profile: WorkspaceDocument.RouteSupport? = nil) throws -> WorkspaceDocument.HostVerificationDecision {
        let presented = try certificate.map { try WorkspaceDocument.HostKey(algorithm: $0.algorithm, fingerprint: $0.fingerprint) } ?? rawKey()
        return try document.verifyHostKey(forRouteID: "route", presented: presented,
            hostCertificate: certificate, negotiatedHostSignature: signature,
            reviewedTrustIDs: Set(document.hostTrust.map(\.id)), support: profile ?? support)
    }
    private func policy(_ f: Fixture, saved: InstalledWorkspace? = nil,
                        store: WorkspaceStore? = nil) async throws -> WorkspaceHostAlgorithmPolicy {
        let saved = saved ?? f.saved
        return try await (store ?? f.store).hostAlgorithmPolicy(workspaceID: saved.id,
            expectedRevision: saved.revision, routeID: "route", support: support)
    }
    private func grants(_ f: Fixture, signature: String?) async throws -> Grants {
        let key = try rawKey(), saved = f.saved
        let publicKey = try await f.store.authorizeSSHAuthentication(workspaceID: saved.id, expectedRevision: saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: key, negotiatedHostSignature: signature, support: support)
        let password = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: saved.id, expectedRevision: saved.revision,
            routeID: "route", presentedHostKey: key, negotiatedHostSignature: signature, support: support)
        let interactive = try await f.store.authorizeSSHKeyboardInteractiveAuthentication(workspaceID: saved.id,
            expectedRevision: saved.revision, routeID: "route", presentedHostKey: key,
            negotiatedHostSignature: signature, support: support)
        return .init(key: publicKey, password: password, interactive: interactive)
    }
    private func expect(_ error: SSHAuthenticationError, file: StaticString = #filePath, line: UInt = #line,
                        _ operation: () async throws -> Void) async {
        do { try await operation(); XCTFail("denied evidence was accepted", file: file, line: line) }
        catch let actual { XCTAssertEqual(actual as? SSHAuthenticationError, error, file: file, line: line) }
    }
    private func catalog(_ disk: StoreTestPersistence) throws -> [String: Any] {
        try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(disk.bytes(.catalog))) as? [String: Any])
    }
    private func seed(_ fields: [String: Any], in disk: StoreTestPersistence) throws {
        disk.seed(try JSONSerialization.data(withJSONObject: fields, options: [.sortedKeys]), slot: .catalog)
    }
    private func narrowHistory(_ f: Fixture, to host: [String]) throws {
        var fields = try catalog(f.disk)
        var entries = try XCTUnwrap(fields["hostAlgorithmPolicies"] as? [[String: Any]])
        entries[0]["policy"] = ["hostKeyAlgorithms": host]
        fields["hostAlgorithmPolicies"] = entries; try seed(fields, in: f.disk)
    }
    private func signedFixture(_ id: String) throws -> SSHHostCertificate {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let bytes = try Data(contentsOf: root.appendingPathComponent("fixtures/ios-airgap/ssh-host-certificates.json"))
        let fields = try XCTUnwrap(JSONSerialization.jsonObject(with: bytes) as? [String: Any])
        let entries = try XCTUnwrap(fields["certificates"] as? [[String: Any]])
        let line = try XCTUnwrap(entries.first { $0["id"] as? String == id }?["certificate"] as? String)
        return try SSHHostCertificate(blob: XCTUnwrap(Data(base64Encoded: String(line.split(separator: " ")[1]))))
    }

    func testStrictSchemaPreservesUnconstrainedDenyAllAndOrderedLiteralPolicy() async throws {
        let unrestricted = try document(), denied = try document(host: []), ordered = try document(host: ["rsa-sha2-256", "rsa-sha2-512"])
        XCTAssertNil(unrestricted.hostTrust[0].hostKeyAlgorithms)
        XCTAssertEqual(denied.hostTrust[0].hostKeyAlgorithms, [])
        XCTAssertFalse(try readiness(denied).isLocallyReady)
        let policy = ordered.hostTrust[0].algorithmPolicy
        XCTAssertEqual(policy.filterHostKeyAlgorithms(["rsa-sha2-512", "ssh-ed25519", "rsa-sha2-256"]), ["rsa-sha2-256", "rsa-sha2-512"])
        XCTAssertEqual(policy.intersecting(try .init(hostKeyAlgorithms: ["rsa-sha2-512"])).hostKeyAlgorithms, ["rsa-sha2-512"])
        XCTAssertEqual(policy.intersecting(try .init(hostKeyAlgorithms: ["ssh-ed25519"])).hostKeyAlgorithms, [])
        XCTAssertEqual(try JSONDecoder().decode(WorkspaceHostAlgorithmPolicy.self, from: JSONEncoder().encode(denied.hostTrust[0].algorithmPolicy)), denied.hostTrust[0].algorithmPolicy)
        for invalid in [["ssh-ed25519", "ssh-ed25519"], ["ssh-*"], [""], ["ssh-ed25519,rsa-sha2-512"], ["ssh ed25519"]] {
            XCTAssertThrowsError(try document(host: invalid), "invalid literal list: \(invalid)")
        }
        XCTAssertThrowsError(try document(host: (0...64).map { "future-\($0)" }))
        XCTAssertFalse(try readiness(document(host: ["future-signature"])).isLocallyReady,
                       "preserving a literal name never advertises native support")
        let implicit = try changed(ordered) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0].removeValue(forKey: "required_capabilities"); workspace["host_trust"] = trusts
        }
        XCTAssertTrue(implicit.hostTrust[0].requiredPolicyRequirements.contains(WorkspaceDocument.hostAlgorithmPolicyCapability))
        XCTAssertThrowsError(try changed(unrestricted) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["required_capabilities"] = [WorkspaceDocument.hostAlgorithmPolicyCapability]; workspace["host_trust"] = trusts
        })
    }

    func testRSAHostPolicyRequiresExactQualifiedNegotiatedSignatureNotBlobName() async throws {
        let document = try document(host: ["rsa-sha2-512"])
        XCTAssertTrue(try readiness(document).isLocallyReady)
        XCTAssertEqual(try verify(document, signature: "rsa-sha2-512"), .trustedPin)
        for signature in [nil, "rsa-sha2-256", "ssh-rsa", "ssh-ed25519", "rsa-sha2-512-cert-v01@openssh.com"] as [String?] {
            XCTAssertEqual(try verify(document, signature: signature), .unsupportedAlgorithm)
        }
        let legacy = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
            credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-rsa"])
        XCTAssertTrue(try readiness(document, profile: legacy).issues.contains(.unsupportedRequirement))
        XCTAssertEqual(try verify(document, signature: "rsa-sha2-512", profile: legacy), .unsupportedPolicy)
        let unqualified = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
            credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-rsa"],
            capabilities: [WorkspaceDocument.hostAlgorithmPolicyCapability])
        XCTAssertTrue(try readiness(document, profile: unqualified).issues.contains(.hostAlgorithmUnavailable))
        XCTAssertEqual(try verify(document, signature: "rsa-sha2-512", profile: unqualified), .unsupportedAlgorithm)
        XCTAssertEqual(try verify(self.document(), signature: nil), .trustedPin, "legacy unqualified manifests keep existing behavior")
    }

    func testSignedCertificatePolicyChecksActualCASignatureAndKeepsRawTrustIndependent() async throws {
        let rsa256 = try signedFixture("rsa-by-rsa-sha256"), rsa512 = try signedFixture("ed25519-by-rsa-sha512")
        XCTAssertEqual(rsa256.authority.algorithm, "ssh-rsa"); XCTAssertEqual(rsa512.authority.algorithm, "ssh-rsa")
        XCTAssertEqual(rsa256.signatureAlgorithm, "rsa-sha2-256"); XCTAssertEqual(rsa512.signatureAlgorithm, "rsa-sha2-512")
        for certificate in [rsa256, rsa512] {
            let negotiated = certificate.algorithm == "ssh-rsa-cert-v01@openssh.com"
                ? "rsa-sha2-512-cert-v01@openssh.com" : "ssh-ed25519-cert-v01@openssh.com"
            let accepted = try document(host: [negotiated], ca: [certificate.signatureAlgorithm], certificate: certificate, rawPins: false)
            XCTAssertTrue(try readiness(accepted).isLocallyReady)
            XCTAssertEqual(try verify(accepted, signature: negotiated, certificate: certificate), .trustedCertificate)
            let opposite = certificate.signatureAlgorithm == "rsa-sha2-256" ? "rsa-sha2-512" : "rsa-sha2-256"
            let rejected = try document(host: [negotiated], ca: [opposite], certificate: certificate, rawPins: false)
            XCTAssertEqual(try verify(rejected, signature: negotiated, certificate: certificate), .unsupportedAlgorithm)
            XCTAssertFalse(accepted.hostTrust[0].algorithmPolicy.permits(hostKeyAlgorithm: certificate.subject.algorithm,
                negotiatedHostSignature: certificate.subject.algorithm, certificate: certificate), "certificate and raw key families cannot be substituted")
        }
        let mixed = try document(ca: [], certificate: rsa512)
        XCTAssertTrue(try readiness(mixed).isLocallyReady)
        XCTAssertEqual(try verify(mixed, signature: "rsa-sha2-512"), .trustedPin)
        XCTAssertEqual(try verify(mixed, signature: "ssh-ed25519-cert-v01@openssh.com", certificate: rsa512), .unsupportedAlgorithm)
        XCTAssertFalse(try readiness(document(ca: [], certificate: rsa512, rawPins: false)).isLocallyReady)
    }

    func testCurrentAliasPoliciesConjoinOnlyWithinExactCanonicalTrustScope() async throws {
        let document = try document(host: ["rsa-sha2-512", "rsa-sha2-256"]) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]], alias = trusts[0]
            alias["id"] = "same-scope"; alias["host_key_algorithms"] = ["rsa-sha2-256"]
            var otherPort = alias; otherPort["id"] = "other-port"; otherPort["port"] = 2222; otherPort["host_key_algorithms"] = []
            var otherAlias = alias; otherAlias["id"] = "other-alias"; otherAlias["host_key_alias"] = "separate-trust"; otherAlias["host_key_algorithms"] = []
            trusts += [alias, otherPort, otherAlias]; workspace["host_trust"] = trusts
        }
        XCTAssertEqual(try document.hostAlgorithmPolicy(forTrustID: "trust").hostKeyAlgorithms, ["rsa-sha2-256"])
        XCTAssertEqual(try verify(document, signature: "rsa-sha2-512"), .unsupportedAlgorithm)
        XCTAssertEqual(try verify(document, signature: "rsa-sha2-256"), .trustedPin)
        let historical = try WorkspaceHostAlgorithmPolicy(hostKeyAlgorithms: [])
        XCTAssertEqual(try document.hostAlgorithmPolicy(forTrustID: "trust", additional: [document.hostTrust[0].canonicalTrustScope: historical]).hostKeyAlgorithms, [])
    }

    func testAllAuthenticationGrantsRetainEvidenceAndRecheckNarrowedHistoryWithoutRevisionChange() async throws {
        let f = try await fixture(document(host: ["rsa-sha2-512", "rsa-sha2-256"]))
        let grants = try await grants(f, signature: "rsa-sha2-512")
        XCTAssertEqual(grants.key.negotiatedHostSignature, "rsa-sha2-512")
        XCTAssertEqual(grants.password.negotiatedHostSignature, "rsa-sha2-512")
        XCTAssertEqual(grants.interactive.negotiatedHostSignature, "rsa-sha2-512")
        try await f.store.validateSSHAuthentication(grants.key, support: support)
        try await f.store.validateSSHPasswordAuthentication(grants.password, support: support)
        try await f.store.validateSSHKeyboardInteractiveAuthentication(grants.interactive, support: support)
        for signature in [nil, "ssh-rsa"] as [String?] {
            await expect(.denied) { _ = try await self.grants(f, signature: signature) }
            await expect(.denied) { _ = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: f.saved.id,
                expectedRevision: f.saved.revision, routeID: "route", presentedHostKey: self.rawKey(),
                negotiatedHostSignature: signature, support: self.support) }
            await expect(.denied) { _ = try await f.store.authorizeSSHKeyboardInteractiveAuthentication(workspaceID: f.saved.id,
                expectedRevision: f.saved.revision, routeID: "route", presentedHostKey: self.rawKey(),
                negotiatedHostSignature: signature, support: self.support) }
        }
        try narrowHistory(f, to: ["rsa-sha2-256"])
        await expect(.denied) { try await f.store.validateSSHAuthentication(grants.key, support: self.support) }
        await expect(.denied) { try await f.store.validateSSHPasswordAuthentication(grants.password, support: self.support) }
        await expect(.denied) { try await f.store.validateSSHKeyboardInteractiveAuthentication(grants.interactive, support: self.support) }
        _ = try await self.grants(f, signature: "rsa-sha2-256")
    }

    func testReimportRemovalReopenRollbackAndDisjointPolicyNeverWidenHistory() async throws {
        let original = try document(host: ["rsa-sha2-512", "rsa-sha2-256"], ca: ["rsa-sha2-512", "rsa-sha2-256"])
        let f = try await fixture(original)
        let narrow = try document(host: ["rsa-sha2-512"], ca: ["rsa-sha2-512"], bundle: "narrow")
        let second = try await install(narrow, in: f.store, identity: 2, replacing: f.saved)
        let removed = try document(bundle: "removed")
        let third = try await install(removed, in: f.store, identity: 3, replacing: second)
        let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault)
        let retained = try await policy(f, saved: third, store: reopened)
        XCTAssertEqual(retained.hostKeyAlgorithms, ["rsa-sha2-512"]); XCTAssertEqual(retained.caSignatureAlgorithms, ["rsa-sha2-512"])
        let rollback = try await install(original, in: reopened, identity: 1, replacing: third, rollback: true)
        let afterRollback = try await policy(f, saved: rollback, store: reopened)
        XCTAssertEqual(afterRollback, retained)
        let disjoint = try document(host: ["rsa-sha2-256"], bundle: "disjoint")
        let fourth = try await install(disjoint, in: reopened, identity: 4, replacing: rollback)
        let blocked = try await policy(f, saved: fourth, store: reopened)
        XCTAssertEqual(blocked.hostKeyAlgorithms, []); XCTAssertEqual(blocked.caSignatureAlgorithms, ["rsa-sha2-512"])
        let local = try await reopened.loadWithLocalPolicy(support: support)
        XCTAssertTrue(try XCTUnwrap(local.first).hostAlgorithmPoliciesByTrustScope.values.contains(blocked))
        await expect(.denied) { _ = try await reopened.authorizeSSHPasswordAuthentication(workspaceID: fourth.id,
            expectedRevision: fourth.revision, routeID: "route", presentedHostKey: self.rawKey(),
            negotiatedHostSignature: "rsa-sha2-256", support: self.support) }
        XCTAssertEqual((try catalog(f.disk)["hostAlgorithmPolicies"] as? [Any])?.count, 1)
        XCTAssertEqual(f.vault.creates, 1)
    }

    func testRemovedScopeKeepsTombstoneWithoutLeakingIntoAnotherEndpoint() async throws {
        let f = try await fixture(document(host: ["rsa-sha2-512"]))
        let otherPort = try document(bundle: "other-port") { workspace in
            var routes = workspace["routes"] as! [[String: Any]], trusts = workspace["host_trust"] as! [[String: Any]]
            routes[0]["port"] = 2222; trusts[0]["port"] = 2222
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }
        let second = try await install(otherPort, in: f.store, identity: 2, replacing: f.saved)
        let unrelated = try await policy(f, saved: second)
        XCTAssertFalse(unrelated.isConstrained)
        let restored = try document(bundle: "restored-scope") { workspace in
            var routes = workspace["routes"] as! [[String: Any]], trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["id"] = "renamed"; routes[0]["host_trust_ref"] = "renamed"
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }
        let third = try await install(restored, in: f.store, identity: 3, replacing: second)
        let returned = try await policy(f, saved: third)
        XCTAssertEqual(returned.hostKeyAlgorithms, ["rsa-sha2-512"])
        let state = try await f.store.loadWithLocalPolicy(support: support)
        XCTAssertEqual(try XCTUnwrap(state.first).requiredHostCapabilitiesByTrustScope[f.document.hostTrust[0].canonicalTrustScope],
                       [WorkspaceDocument.hostAlgorithmPolicyCapability])
    }

    func testCorruptPolicyCatalogCannotEraseWeakenDuplicateOrDowngradeMandatoryPayload() async throws {
        let f = try await fixture(document(host: ["rsa-sha2-512"]))
        let original = try catalog(f.disk)
        for corruption in ["missing", "duplicate", "widened", "marker", "downgraded", "legacy-manifest"] {
            var fields = original, entries = try XCTUnwrap(original["hostAlgorithmPolicies"] as? [[String: Any]])
            switch corruption {
            case "missing": entries = []
            case "duplicate": entries.append(entries[0])
            case "widened": entries[0]["policy"] = ["hostKeyAlgorithms": ["rsa-sha2-512", "rsa-sha2-256"]]
            case "marker": fields["hostRequirements"] = []
            case "legacy-manifest": fields["version"] = 9; fields.removeValue(forKey: "hostAlgorithmPolicies")
            default: fields["version"] = 9
            }
            if corruption != "legacy-manifest" { fields["hostAlgorithmPolicies"] = entries }
            if corruption == "legacy-manifest" || corruption == "downgraded" {
                fields.removeValue(forKey: "hostRevocationSources"); fields.removeValue(forKey: "hostRevocationSourceScopes")
            }
            try seed(fields, in: f.disk)
            let bytes = f.disk.bytes(.catalog)
            do { _ = try await WorkspaceStore(persistence: f.disk, vault: f.vault).load(); XCTFail(corruption) }
            catch { XCTAssertEqual(error as? WorkspaceStoreError, .corruptStore, corruption) }
            XCTAssertEqual(f.disk.bytes(.catalog), bytes); XCTAssertEqual(f.vault.count, 1)
        }
        try seed(original, in: f.disk)
        let removed = try await install(document(bundle: "fields-removed"), in: f.store, identity: 2, replacing: f.saved)
        var missingTombstone = try catalog(f.disk); missingTombstone["hostAlgorithmPolicies"] = []
        try seed(missingTombstone, in: f.disk)
        do { _ = try await WorkspaceStore(persistence: f.disk, vault: f.vault).load(); XCTFail("historical mandatory payload vanished after manifest removal: \(removed.id)") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .corruptStore) }
    }

    func testPublishedVersionsTwoThroughNineMigrateWithoutInventingAlgorithmPolicy() async throws {
        for version in 2...9 {
            let f = try await fixture(document())
            var fields = try catalog(f.disk); fields["version"] = version
            fields.removeValue(forKey: "hostAlgorithmPolicies")
            fields.removeValue(forKey: "hostRevocationSources"); fields.removeValue(forKey: "hostRevocationSourceScopes")
            if version < 3 { fields.removeValue(forKey: "hostRevocations") }
            if version < 4 { fields.removeValue(forKey: "scopedHostRevocations"); fields.removeValue(forKey: "hostRequirements") }
            if version < 5 { fields.removeValue(forKey: "machineConnectionPreferences") }
            if version < 6 { fields.removeValue(forKey: "groupNavigationPreferences") }
            let history = try JSONSerialization.data(withJSONObject: XCTUnwrap(fields["acceptedImports"]), options: [.sortedKeys])
            try seed(fields, in: f.disk)
            let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault), loaded = try await reopened.load()
            let saved = try XCTUnwrap(loaded.first)
            XCTAssertEqual(saved.revision, f.saved.revision); XCTAssertEqual(saved.manifest, f.saved.manifest)
            XCTAssertEqual(saved.bindings, f.saved.bindings)
            let effective = try await policy(f, store: reopened)
            XCTAssertFalse(effective.isConstrained)
            let migrated = try catalog(f.disk)
            XCTAssertEqual(migrated["version"] as? Int, 12)
            XCTAssertEqual((migrated["hostAlgorithmPolicies"] as? [Any])?.count, 0)
            XCTAssertEqual(try JSONSerialization.data(withJSONObject: XCTUnwrap(migrated["acceptedImports"]), options: [.sortedKeys]), history)
            XCTAssertEqual(f.vault.creates, 1)
        }
    }

    func testNewTypedPolicyDoesNotClearHistoricalUnresolvedSourceRestriction() async throws {
        let unresolved = try document { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["required_capabilities"] = ["ssh_config.unresolved_policy"]; workspace["host_trust"] = trusts
        }
        let f = try await fixture(unresolved)
        let revised = try await install(document(host: ["rsa-sha2-512"], bundle: "resolved-literals"),
            in: f.store, identity: 2, replacing: f.saved)
        await expect(.denied) { _ = try await self.policy(f, saved: revised) }
        await expect(.denied) { _ = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: revised.id,
            expectedRevision: revised.revision, routeID: "route", presentedHostKey: self.rawKey(),
            negotiatedHostSignature: "rsa-sha2-512", support: self.support) }
        let local = try await f.store.loadWithLocalPolicy(support: support)
        XCTAssertTrue(try XCTUnwrap(local.first).requiredHostCapabilitiesByTrustScope.values
            .contains { $0.contains("ssh_config.unresolved_policy") && $0.contains(WorkspaceDocument.hostAlgorithmPolicyCapability) })
    }

    func testBrokerOffersAndPendingSignaturesRetainAndRevalidateNegotiatedEvidence() async throws {
        let f = try await fixture(document(host: ["rsa-sha2-512", "rsa-sha2-256"]))
        let (privateKey, _) = try sampleKey(using: crypto)
        let broker = SSHAuthenticationBroker(store: f.store, signer: HostAlgorithmPolicySigner(container: privateKey.container), support: support)
        await broker.resume()
        let key = try rawKey()
        let offer = try await broker.publicIdentity(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: key,
            negotiatedHostSignature: "rsa-sha2-512", transportID: UUID())
        let password = try await broker.passwordIdentity(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", presentedHostKey: key, negotiatedHostSignature: "rsa-sha2-512")
        let interactive = try await broker.keyboardInteractiveIdentity(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", presentedHostKey: key, negotiatedHostSignature: "rsa-sha2-512")
        XCTAssertEqual(offer.authorization.negotiatedHostSignature, "rsa-sha2-512")
        XCTAssertEqual(password.authorization.negotiatedHostSignature, "rsa-sha2-512")
        XCTAssertEqual(interactive.authorization.negotiatedHostSignature, "rsa-sha2-512")
        let prepared = try await broker.prepare(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: key, negotiatedHostSignature: "rsa-sha2-512",
            transportID: UUID(), sessionIdentifier: Data(repeating: 71, count: 32), publicBlob: privateKey.publicBlob)
        let signed = try await broker.confirm(requestID: prepared.id)
        XCTAssertEqual(signed.authorization.negotiatedHostSignature, "rsa-sha2-512")
        let pending = try await broker.prepare(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: key, negotiatedHostSignature: "rsa-sha2-512",
            transportID: UUID(), sessionIdentifier: Data(repeating: 72, count: 32), publicBlob: privateKey.publicBlob)
        try narrowHistory(f, to: ["rsa-sha2-256"])
        await expect(.denied) { _ = try await broker.confirm(requestID: pending.id) }
        await expect(.denied) { try await broker.validate(offer) }
        await expect(.denied) { try await broker.validatePasswordAttempt(password) }
        await expect(.denied) { try await broker.validateKeyboardInteractiveAttempt(interactive) }
        await broker.finishKeyboardInteractiveAttempt(interactive); await broker.suspend()
    }

    func testFirstUsePolicyLookupAndPinReviewDoNotInventHandshakeEvidenceOrReadPrivateKeys() async throws {
        let f = try await fixture(document(host: ["rsa-sha2-512"], rawPins: false))
        f.vault.configure(.readFingerprint)
        let effective = try await policy(f)
        XCTAssertEqual(effective.hostKeyAlgorithms, ["rsa-sha2-512"])
        f.vault.configure(nil)
        await expect(.stale) { _ = try await f.store.hostAlgorithmPolicy(workspaceID: f.saved.id,
            expectedRevision: UUID(), routeID: "route", support: self.support) }
        let review = try await f.store.reviewHostKeyPin(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", key: rawKey(), support: support)
        let pinned = try await f.store.acceptHostKeyPin(review, confirmedByUser: true, support: support)
        XCTAssertEqual(try pinned.document().hostTrust[0].pins, [try rawKey()])
        await expect(.denied) { _ = try await f.store.authorizeSSHAuthentication(workspaceID: pinned.id,
            expectedRevision: pinned.revision, routeID: "route", credentialID: "key",
            presentedHostKey: self.rawKey(), support: self.support) }
        let authorized = try await f.store.authorizeSSHAuthentication(workspaceID: pinned.id, expectedRevision: pinned.revision,
            routeID: "route", credentialID: "key", presentedHostKey: rawKey(), negotiatedHostSignature: "rsa-sha2-512", support: support)
        XCTAssertEqual(authorized.negotiatedHostSignature, "rsa-sha2-512")
    }
}
