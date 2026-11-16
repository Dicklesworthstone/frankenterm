import Foundation
import XCTest
@testable import FrankenTermKit

private struct TransportAlgorithmPolicySigner: WorkspaceSSHSigning {
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

@MainActor
final class WorkspaceSSHTransportAlgorithmPolicyTests: XCTestCase {
    private let crypto = TestCryptography()
    private let kex = "curve25519-sha256", otherKEX = "ecdh-sha2-nistp256"
    private let cipher = "aes256-ctr", otherCipher = "aes128-ctr"
    private let mac = "hmac-sha2-256-etm@openssh.com", otherMAC = "hmac-sha2-512-etm@openssh.com"

    private func profile(capability: Bool = true, kex: Set<String>? = nil,
                         ciphers: Set<String>? = nil, macs: Set<String>? = nil) -> WorkspaceDocument.RouteSupport {
        var capabilities: Set<String> = [WorkspaceDocument.hostAlgorithmPolicyCapability, WorkspaceDocument.hostCertificateCapability]
        if capability { capabilities.insert(WorkspaceDocument.transportAlgorithmPolicyCapability) }
        return .init(routeModes: ["direct_ssh", "frankenterm_mux"], authenticationMethods: [.publicKey, .password, .keyboardInteractive],
            credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519", "ssh-ed25519-cert-v01@openssh.com"],
            capabilities: capabilities, maximumJumpHops: 3,
            hostSignatureAlgorithms: ["ssh-ed25519", "ssh-ed25519-cert-v01@openssh.com"],
            keyExchangeAlgorithms: kex ?? [self.kex, otherKEX],
            ciphers: ciphers ?? [cipher, otherCipher], macs: macs ?? [mac, otherMAC])
    }
    private func evidence(kex: String? = nil, outbound: String? = nil, inbound: String? = nil,
                          outboundMAC: String? = nil, inboundMAC: String? = nil) -> SSHTransportAlgorithms {
        .init(keyExchange: kex ?? self.kex, outboundCipher: outbound ?? cipher, inboundCipher: inbound ?? cipher,
              outboundMAC: outboundMAC ?? mac, inboundMAC: inboundMAC ?? mac)
    }
    private func pin() throws -> WorkspaceDocument.HostKey {
        try .init(algorithm: "ssh-ed25519", fingerprint: "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
    }
    private func changed(_ document: WorkspaceDocument, bundle: String? = nil,
                         edit: (inout [String: Any]) -> Void) throws -> WorkspaceDocument {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: document.raw) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        edit(&workspace); root["workspace"] = workspace
        if let bundle { root["bundle_id"] = bundle }
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }
    private func document(kex: [String]? = nil, ciphers: [String]? = nil, macs: [String]? = nil,
                          bundle: String = "transport-policy", pin: Bool = true,
                          edit: (inout [String: Any]) -> Void = { _ in }) throws -> WorkspaceDocument {
        let (_, fingerprint) = try sampleKey(using: crypto)
        let original = try WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint, bundleID: bundle))
        return try changed(original) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            if let kex { trusts[0]["kex_algorithms"] = kex }
            if let ciphers { trusts[0]["ciphers"] = ciphers }
            if let macs { trusts[0]["macs"] = macs }
            if kex != nil || ciphers != nil || macs != nil {
                trusts[0]["required_capabilities"] = [WorkspaceDocument.transportAlgorithmPolicyCapability]
            }
            if !pin { trusts[0]["pins"] = [] }
            workspace["host_trust"] = trusts
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["authentication_methods"] = ["publickey", "password", "keyboard-interactive"]
            workspace["routes"] = routes; edit(&workspace)
        }
    }
    private struct Fixture {
        let store: WorkspaceStore, disk: StoreTestPersistence, vault: StoreTestVault
        let saved: InstalledWorkspace
    }
    private func install(_ document: WorkspaceDocument, in store: WorkspaceStore,
                         previous: InstalledWorkspace? = nil, identity: UInt8 = 1) async throws -> InstalledWorkspace {
        let (key, _) = try sampleKey(using: crypto)
        let archive = try AirGapArchive(workspace: document, keys: [key], using: crypto)
        return try await store.install(archive, envelopeDigest: Data(repeating: identity, count: 32),
            transferID: Data(repeating: identity, count: 16), expectedRevision: previous?.revision,
            sourceAndContentsConfirmed: true, securityChangesConfirmed: previous != nil,
            associateWithExistingWorkspace: previous != nil).workspace
    }
    private func fixture(_ document: WorkspaceDocument) async throws -> Fixture {
        let disk = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: disk, vault: vault)
        let saved = try await install(document, in: store)
        return .init(store: store, disk: disk, vault: vault, saved: saved)
    }
    private func catalog(_ disk: StoreTestPersistence) throws -> [String: Any] {
        try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(disk.bytes(.catalog))) as? [String: Any])
    }
    private func seed(_ fields: [String: Any], in disk: StoreTestPersistence) throws {
        disk.seed(try JSONSerialization.data(withJSONObject: fields, options: [.sortedKeys]), slot: .catalog)
    }
    private func narrowHistory(_ f: Fixture, ciphers: [String]) throws {
        var fields = try catalog(f.disk)
        var entries = try XCTUnwrap(fields["hostAlgorithmPolicies"] as? [[String: Any]])
        var policy = try XCTUnwrap(entries.first?["policy"] as? [String: Any])
        var transport = try XCTUnwrap(policy["transport"] as? [String: Any])
        transport["ciphers"] = ciphers; policy["transport"] = transport
        entries[0]["policy"] = policy; fields["hostAlgorithmPolicies"] = entries
        try seed(fields, in: f.disk)
    }
    private func denied(file: StaticString = #filePath, line: UInt = #line,
                        _ operation: () async throws -> Void) async {
        do { try await operation(); XCTFail("restricted transport was accepted", file: file, line: line) }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied, file: file, line: line) }
    }
    private func readiness(_ document: WorkspaceDocument, support: WorkspaceDocument.RouteSupport? = nil) throws -> WorkspaceDocument.RouteReadiness {
        try document.readiness(forRouteID: "route", availableCredentialIDs: ["key"],
            reviewedTrustIDs: Set(document.hostTrust.map(\.id)), support: support ?? profile())
    }
    private func verify(_ document: WorkspaceDocument, _ transport: SSHTransportAlgorithms?,
                        support: WorkspaceDocument.RouteSupport? = nil) throws -> WorkspaceDocument.HostVerificationDecision {
        try document.verifyHostKey(forRouteID: "route", presented: pin(), negotiatedHostSignature: "ssh-ed25519",
            negotiatedTransport: transport, reviewedTrustIDs: Set(document.hostTrust.map(\.id)), support: support ?? profile())
    }

    func testLiteralBoundsOrderedIntersectionAndCodablePreserveNilVersusDenyAll() async throws {
        let policy = try WorkspaceSSHTransportAlgorithmPolicy(keyExchangeAlgorithms: [otherKEX, kex],
            ciphers: [otherCipher, cipher], macs: [otherMAC, mac])
        XCTAssertEqual(policy.filterKeyExchangeAlgorithms([kex, otherKEX]), [otherKEX, kex])
        XCTAssertEqual(policy.filterCiphers([cipher, otherCipher]), [otherCipher, cipher])
        XCTAssertEqual(policy.filterMACs([mac]), [mac])
        let narrowed = policy.intersecting(try .init(keyExchangeAlgorithms: [kex], ciphers: [cipher], macs: []))
        XCTAssertEqual(narrowed.keyExchangeAlgorithms, [kex]); XCTAssertEqual(narrowed.ciphers, [cipher])
        XCTAssertEqual(narrowed.macs, []); XCTAssertFalse(narrowed.permits(evidence()))
        XCTAssertEqual(WorkspaceSSHTransportAlgorithmPolicy.unconstrained.intersecting(policy), policy)
        XCTAssertNil(WorkspaceSSHTransportAlgorithmPolicy.unconstrained.ciphers)
        XCTAssertEqual(try JSONDecoder().decode(WorkspaceSSHTransportAlgorithmPolicy.self,
            from: JSONEncoder().encode(narrowed)), narrowed)
        for invalid in [[""], ["aes*-ctr"], ["aes256-ctr,aes128-ctr"], ["a b"], ["a\n"], ["same", "same"],
                        [String(repeating: "a", count: 129)], (0...64).map({ "algorithm-\($0)" })] {
            XCTAssertThrowsError(try WorkspaceSSHTransportAlgorithmPolicy(ciphers: invalid))
        }
    }

    func testEverySuppliedNegotiatedNameIsValidatedAndBothDirectionsMustMatch() async throws {
        let policy = try WorkspaceSSHTransportAlgorithmPolicy(keyExchangeAlgorithms: [kex], ciphers: [cipher], macs: [mac])
        XCTAssertTrue(policy.permits(evidence())); XCTAssertFalse(policy.permits(nil))
        for invalid in [evidence(kex: otherKEX), evidence(outbound: otherCipher), evidence(inbound: otherCipher),
                        evidence(outboundMAC: otherMAC), evidence(inboundMAC: otherMAC)] {
            XCTAssertFalse(policy.permits(invalid))
        }
        let onlyCipher = try WorkspaceSSHTransportAlgorithmPolicy(ciphers: [cipher])
        XCTAssertTrue(onlyCipher.permits(evidence(kex: otherKEX, inboundMAC: otherMAC)))
        for malformed in [evidence(kex: ""), evidence(outbound: "aes256-ctr,aes128-ctr"), evidence(inbound: "bad cipher"),
                          evidence(outboundMAC: "*"), evidence(inboundMAC: String(repeating: "a", count: 129))] {
            XCTAssertFalse(onlyCipher.permits(malformed))
            XCTAssertFalse(WorkspaceSSHTransportAlgorithmPolicy.unconstrained.permits(malformed))
        }
        XCTAssertTrue(WorkspaceSSHTransportAlgorithmPolicy.unconstrained.permits(nil))
        XCTAssertFalse(evidence(inbound: otherCipher).isSupported(keyExchangeAlgorithms: [kex], ciphers: [cipher], macs: [mac]))
    }

    func testManifestCapabilityIsImplicitDistinctAndCannotExistWithoutItsPayload() async throws {
        let original = try document(kex: [kex], ciphers: [cipher], macs: [mac])
        let implicit = try changed(original) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0].removeValue(forKey: "required_capabilities"); workspace["host_trust"] = trusts
        }
        XCTAssertEqual(implicit.hostTrust[0].requiredPolicyRequirements, [WorkspaceDocument.transportAlgorithmPolicyCapability])
        XCTAssertTrue(implicit.hostTrust[0].algorithmPolicy.isConstrained)
        XCTAssertFalse(implicit.hostTrust[0].algorithmPolicy.isSignatureConstrained)
        XCTAssertEqual(try WorkspaceDocument(data: implicit.raw).hostTrust[0].algorithmPolicy, implicit.hostTrust[0].algorithmPolicy)
        for capability in [WorkspaceDocument.transportAlgorithmPolicyCapability, WorkspaceDocument.hostAlgorithmPolicyCapability] {
            XCTAssertThrowsError(try changed(document()) { workspace in
                var trusts = workspace["host_trust"] as! [[String: Any]]
                trusts[0]["required_capabilities"] = [capability]; workspace["host_trust"] = trusts
            })
        }
        XCTAssertThrowsError(try changed(original) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["required_capabilities"] = [WorkspaceDocument.hostAlgorithmPolicyCapability]
            workspace["host_trust"] = trusts
        }, "Transport fields cannot masquerade as a host-signature policy.")
    }

    func testReadinessRequiresActualQualifiedAlgorithmsRatherThanOnlyCapability() async throws {
        let configured = try document(kex: [kex], ciphers: [cipher], macs: [mac])
        XCTAssertTrue(try readiness(configured).isLocallyReady)
        XCTAssertTrue(try readiness(configured, support: profile(capability: false)).issues.contains(.unsupportedRequirement))
        for unqualified in [profile(kex: []), profile(ciphers: []), profile(macs: []),
                            profile(kex: [otherKEX]), profile(ciphers: [otherCipher]), profile(macs: [otherMAC])] {
            XCTAssertTrue(try readiness(configured, support: unqualified).issues.contains(.transportAlgorithmUnavailable))
            XCTAssertEqual(try verify(configured, evidence(), support: unqualified), .unsupportedAlgorithm)
        }
        for impossible in try [document(kex: []), document(ciphers: []), document(macs: []), document(kex: ["future-kex"])] {
            XCTAssertTrue(try readiness(impossible).issues.contains(.transportAlgorithmUnavailable))
        }
        let partial = try document(ciphers: [cipher])
        XCTAssertTrue(try readiness(partial, support: profile(kex: [])).issues.contains(.transportAlgorithmUnavailable))
        let legacy = try document()
        XCTAssertTrue(try readiness(legacy, support: profile(capability: false, kex: [], ciphers: [], macs: [])).isLocallyReady)
        XCTAssertEqual(try verify(legacy, nil, support: profile(capability: false)), .trustedPin)
    }

    func testRawAndCertificateHostChecksRequireTheExactTransportEvidence() async throws {
        let configured = try document(kex: [kex], ciphers: [cipher], macs: [mac])
        XCTAssertEqual(try verify(configured, evidence()), .trustedPin)
        for refused in [nil, evidence(inbound: otherCipher), evidence(inboundMAC: otherMAC)] as [SSHTransportAlgorithms?] {
            XCTAssertEqual(try verify(configured, refused), .unsupportedAlgorithm)
            XCTAssertEqual(try configured.evaluateHostKey(pin(), trustID: "trust", locallyAccepted: true,
                supportedCapabilities: profile().capabilities, negotiatedHostSignature: "ssh-ed25519",
                negotiatedTransport: refused), .requiredPolicyUnavailable)
        }
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("fixtures/ios-airgap/ssh-host-certificates.json"))
        let fixture = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let certificates = try XCTUnwrap(fixture["certificates"] as? [[String: Any]])
        let line = try XCTUnwrap(certificates.first { $0["id"] as? String == "ed25519-by-p256" }?["certificate"] as? String)
        let certificate = try SSHHostCertificate(blob: XCTUnwrap(Data(base64Encoded: String(line.split(separator: " ")[1]))))
        let certified = try changed(configured) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["pins"] = []; trusts[0]["certificate_authorities"] = [["algorithm": certificate.authority.algorithm,
                "fingerprint": certificate.authority.fingerprint]]
            workspace["host_trust"] = trusts
        }
        let key = try WorkspaceDocument.HostKey(algorithm: certificate.algorithm, fingerprint: certificate.fingerprint)
        for (transport, expected) in [(evidence(), WorkspaceDocument.HostVerificationDecision.trustedCertificate),
                                      (evidence(inboundMAC: otherMAC), .unsupportedAlgorithm)] {
            XCTAssertEqual(try certified.verifyHostKey(forRouteID: "route", presented: key, hostCertificate: certificate,
                negotiatedHostSignature: certificate.algorithm, negotiatedTransport: transport,
                reviewedTrustIDs: ["trust"], support: profile()), expected)
        }
    }

    func testSameScopeAliasesConjoinTransportWhileOtherPortsRemainIndependent() async throws {
        let configured = try document(ciphers: [cipher, otherCipher]) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]], alias = trusts[0]
            alias["id"] = "same-scope"; alias["ciphers"] = [otherCipher]
            var other = alias; other["id"] = "other-port"; other["port"] = 2222; other["ciphers"] = []
            trusts += [alias, other]; workspace["host_trust"] = trusts
        }
        XCTAssertEqual(try configured.hostAlgorithmPolicy(forTrustID: "trust").transport.ciphers, [otherCipher])
        XCTAssertEqual(try verify(configured, evidence()), .unsupportedAlgorithm)
        XCTAssertEqual(try verify(configured, evidence(outbound: otherCipher, inbound: otherCipher)), .trustedPin)
        let deny = try WorkspaceHostAlgorithmPolicy(transport: .init(ciphers: []))
        let effective = try configured.hostAlgorithmPolicy(forTrustID: "trust",
            additional: [configured.hostTrust[0].canonicalTrustScope: deny])
        XCTAssertEqual(effective.transport.ciphers, [])
    }

    func testAllAuthenticationGrantsRetainAndRevalidateBothTransportDirections() async throws {
        let f = try await fixture(document(kex: [kex], ciphers: [cipher, otherCipher], macs: [mac]))
        let key = try pin(), accepted = evidence(), support = profile()
        for missing in [nil, evidence(inboundMAC: otherMAC)] as [SSHTransportAlgorithms?] {
            await denied { _ = try await f.store.authorizeSSHAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
                routeID: "route", credentialID: "key", presentedHostKey: key, negotiatedTransport: missing, support: support) }
            await denied { _ = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
                routeID: "route", presentedHostKey: key, negotiatedTransport: missing, support: support) }
            await denied { _ = try await f.store.authorizeSSHKeyboardInteractiveAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
                routeID: "route", presentedHostKey: key, negotiatedTransport: missing, support: support) }
        }
        let publicKey = try await f.store.authorizeSSHAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: key, negotiatedTransport: accepted, support: support)
        let password = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", presentedHostKey: key, negotiatedTransport: accepted, support: support)
        let interactive = try await f.store.authorizeSSHKeyboardInteractiveAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", presentedHostKey: key, negotiatedTransport: accepted, support: support)
        XCTAssertEqual(publicKey.negotiatedTransport, accepted); XCTAssertEqual(password.negotiatedTransport, accepted)
        XCTAssertEqual(interactive.negotiatedTransport, accepted)
        try await f.store.validateSSHAuthentication(publicKey, support: support)
        try await f.store.validateSSHPasswordAuthentication(password, support: support)
        try await f.store.validateSSHKeyboardInteractiveAuthentication(interactive, support: support)
        try narrowHistory(f, ciphers: [otherCipher])
        await denied { try await f.store.validateSSHAuthentication(publicKey, support: support) }
        await denied { try await f.store.validateSSHPasswordAuthentication(password, support: support) }
        await denied { try await f.store.validateSSHKeyboardInteractiveAuthentication(interactive, support: support) }
    }

    func testBrokerOfferPromptAndPendingSignaturePreserveTheNegotiatedWitness() async throws {
        let f = try await fixture(document(ciphers: [cipher, otherCipher]))
        let (privateKey, _) = try sampleKey(using: crypto), key = try pin(), accepted = evidence()
        let broker = SSHAuthenticationBroker(store: f.store,
            signer: TransportAlgorithmPolicySigner(container: privateKey.container), support: profile())
        await broker.resume()
        let offer = try await broker.publicIdentity(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: key, negotiatedTransport: accepted, transportID: UUID())
        let password = try await broker.passwordIdentity(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", presentedHostKey: key, negotiatedTransport: accepted)
        let interactive = try await broker.keyboardInteractiveIdentity(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", presentedHostKey: key, negotiatedTransport: accepted)
        XCTAssertEqual(offer.authorization.negotiatedTransport, accepted)
        XCTAssertEqual(password.authorization.negotiatedTransport, accepted)
        XCTAssertEqual(interactive.authorization.negotiatedTransport, accepted)
        let request = try await broker.prepare(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: key, negotiatedTransport: accepted,
            transportID: UUID(), sessionIdentifier: Data(repeating: 91, count: 32), publicBlob: privateKey.publicBlob)
        let signed = try await broker.confirm(requestID: request.id)
        XCTAssertEqual(signed.authorization.negotiatedTransport, accepted)
        let pending = try await broker.prepare(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: key, negotiatedTransport: accepted,
            transportID: UUID(), sessionIdentifier: Data(repeating: 92, count: 32), publicBlob: privateKey.publicBlob)
        try narrowHistory(f, ciphers: [otherCipher])
        await denied { _ = try await broker.confirm(requestID: pending.id) }
        await denied { try await broker.validate(offer) }
        await denied { try await broker.validatePasswordAttempt(password) }
        await denied { try await broker.validateKeyboardInteractiveAttempt(interactive) }
        await broker.finishKeyboardInteractiveAttempt(interactive); await broker.suspend()
    }

    func testReimportAndRelaunchRetainHistoricalRestrictionsWithoutAffectingAnotherEndpoint() async throws {
        let f = try await fixture(document(kex: [kex], ciphers: [cipher, otherCipher]))
        let second = try await install(document(ciphers: [otherCipher], bundle: "narrower"), in: f.store,
            previous: f.saved, identity: 2)
        let third = try await install(document(bundle: "fields-removed"), in: f.store, previous: second, identity: 3)
        let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault)
        let retained = try await reopened.hostAlgorithmPolicy(workspaceID: third.id, expectedRevision: third.revision,
            routeID: "route", support: profile())
        XCTAssertEqual(retained.transport.keyExchangeAlgorithms, [kex]); XCTAssertEqual(retained.transport.ciphers, [otherCipher])
        await denied { _ = try await reopened.authorizeSSHPasswordAuthentication(workspaceID: third.id,
            expectedRevision: third.revision, routeID: "route", presentedHostKey: self.pin(),
            negotiatedTransport: self.evidence(), support: self.profile()) }
        let accepted = try await reopened.authorizeSSHPasswordAuthentication(workspaceID: third.id,
            expectedRevision: third.revision, routeID: "route", presentedHostKey: pin(),
            negotiatedTransport: evidence(outbound: otherCipher, inbound: otherCipher), support: profile())
        XCTAssertEqual(accepted.negotiatedTransport?.inboundCipher, otherCipher)
        let other = try document(bundle: "different-endpoint") { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]], routes = workspace["routes"] as! [[String: Any]]
            trusts[0]["port"] = 2222; routes[0]["port"] = 2222
            workspace["host_trust"] = trusts; workspace["routes"] = routes
        }
        let fourth = try await install(other, in: reopened, previous: third, identity: 4)
        let independent = try await reopened.hostAlgorithmPolicy(workspaceID: fourth.id, expectedRevision: fourth.revision,
            routeID: "route", support: profile())
        XCTAssertFalse(independent.transport.isConstrained)
        let fifth = try await install(document(bundle: "scope-returned"), in: reopened, previous: fourth, identity: 5)
        let returned = try await reopened.hostAlgorithmPolicy(workspaceID: fifth.id, expectedRevision: fifth.revision,
            routeID: "route", support: profile())
        XCTAssertEqual(returned.transport, retained.transport)
        XCTAssertEqual(f.vault.creates, 1)
    }

    func testVersionElevenMigratesSignatureHistoryWithoutInventingTransportRestrictions() async throws {
        let legacy = try document { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["host_key_algorithms"] = ["ssh-ed25519"]
            trusts[0]["required_capabilities"] = [WorkspaceDocument.hostAlgorithmPolicyCapability]
            workspace["host_trust"] = trusts
        }
        let f = try await fixture(legacy)
        var fields = try catalog(f.disk), entries = try XCTUnwrap(catalog(f.disk)["hostAlgorithmPolicies"] as? [[String: Any]])
        var policy = try XCTUnwrap(entries[0]["policy"] as? [String: Any])
        policy.removeValue(forKey: "transport"); entries[0]["policy"] = policy
        fields["hostAlgorithmPolicies"] = entries; fields["version"] = 11
        let history = try JSONSerialization.data(withJSONObject: XCTUnwrap(fields["acceptedImports"]), options: [.sortedKeys])
        try seed(fields, in: f.disk)
        let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault)
        let loaded = try await reopened.load(), saved = try XCTUnwrap(loaded.first)
        XCTAssertEqual(saved.revision, f.saved.revision); XCTAssertEqual(saved.bindings, f.saved.bindings)
        let effective = try await reopened.hostAlgorithmPolicy(workspaceID: saved.id, expectedRevision: saved.revision,
            routeID: "route", support: profile())
        XCTAssertEqual(effective.hostKeyAlgorithms, ["ssh-ed25519"]); XCTAssertFalse(effective.transport.isConstrained)
        let migrated = try catalog(f.disk)
        XCTAssertEqual(migrated["version"] as? Int, 12)
        XCTAssertEqual(try JSONSerialization.data(withJSONObject: XCTUnwrap(migrated["acceptedImports"]), options: [.sortedKeys]), history)
        XCTAssertEqual(f.vault.creates, 1)
    }

    func testCatalogCannotDowngradeEraseOrWeakenTransportPolicyOrItsCapability() async throws {
        let f = try await fixture(document(ciphers: [cipher]))
        let original = try catalog(f.disk)
        for corruption in ["version", "payload", "marker", "weakened", "broadened", "duplicate"] {
            var fields = original, entries = try XCTUnwrap(original["hostAlgorithmPolicies"] as? [[String: Any]])
            var policy = try XCTUnwrap(entries[0]["policy"] as? [String: Any])
            switch corruption {
            case "version": fields["version"] = 11
            case "payload": policy.removeValue(forKey: "transport")
            case "marker": fields["hostRequirements"] = []
            case "weakened": policy["transport"] = [:] as [String: Any]
            case "broadened": policy["transport"] = ["ciphers": [cipher, otherCipher]]
            default: entries.append(entries[0])
            }
            entries[0]["policy"] = policy; fields["hostAlgorithmPolicies"] = entries
            try seed(fields, in: f.disk)
            let bytes = f.disk.bytes(.catalog)
            do { _ = try await WorkspaceStore(persistence: f.disk, vault: f.vault).load(); XCTFail(corruption) }
            catch { XCTAssertEqual(error as? WorkspaceStoreError, .corruptStore, corruption) }
            XCTAssertEqual(f.disk.bytes(.catalog), bytes); XCTAssertEqual(f.vault.count, 1)
        }
        try seed(original, in: f.disk)
        _ = try await install(document(bundle: "removed"), in: f.store, previous: f.saved, identity: 2)
        var missing = try catalog(f.disk); missing["hostAlgorithmPolicies"] = []
        try seed(missing, in: f.disk)
        do { _ = try await WorkspaceStore(persistence: f.disk, vault: f.vault).load(); XCTFail("historical policy was erased") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .corruptStore) }
    }

    func testFirstUseAndJumpAuthenticationApplyTheirOwnTransportPolicies() async throws {
        let unpinned = try await fixture(document(ciphers: [cipher], pin: false))
        unpinned.vault.configure(.readFingerprint)
        let effective = try await unpinned.store.hostAlgorithmPolicy(workspaceID: unpinned.saved.id,
            expectedRevision: unpinned.saved.revision, routeID: "route", support: profile())
        XCTAssertEqual(effective.transport.ciphers, [cipher])
        unpinned.vault.configure(nil)
        do {
            _ = try await unpinned.store.beginHostKeyVerification(workspaceID: unpinned.saved.id,
                expectedRevision: unpinned.saved.revision, routeID: "route", confirmedByUser: true,
                support: profile(ciphers: [otherCipher]))
            XCTFail("a first-use probe ignored an impossible transport policy")
        } catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .unsupportedPolicy) }
        let review = try await unpinned.store.reviewHostKeyPin(workspaceID: unpinned.saved.id,
            expectedRevision: unpinned.saved.revision, routeID: "route", key: pin(), support: profile())
        let pinned = try await unpinned.store.acceptHostKeyPin(review, confirmedByUser: true, support: profile())
        await denied { _ = try await unpinned.store.authorizeSSHPasswordAuthentication(workspaceID: pinned.id,
            expectedRevision: pinned.revision, routeID: "route", presentedHostKey: self.pin(), support: self.profile()) }

        let jumped = try document { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]], hopTrust = trusts[0]
            hopTrust["id"] = "hop-trust"; hopTrust["host"] = "jump.example.invalid"
            hopTrust["ciphers"] = [cipher]; hopTrust["required_capabilities"] = [WorkspaceDocument.transportAlgorithmPolicyCapability]
            trusts.append(hopTrust); workspace["host_trust"] = trusts
            var routes = workspace["routes"] as! [[String: Any]], hop = routes[0]
            hop["id"] = "hop"; hop["host"] = "jump.example.invalid"; hop["host_trust_ref"] = "hop-trust"
            routes[0]["jump_routes"] = ["hop"]; routes.append(hop); workspace["routes"] = routes
        }
        let f = try await fixture(jumped)
        XCTAssertTrue(try readiness(jumped).isLocallyReady)
        let good = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "hop", presentedHostKey: pin(), negotiatedTransport: evidence(), support: profile(), transitForRouteID: "route")
        XCTAssertEqual(good.negotiatedTransport, evidence())
        await denied { _ = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, routeID: "hop", presentedHostKey: self.pin(),
            negotiatedTransport: self.evidence(inbound: self.otherCipher), support: self.profile(), transitForRouteID: "route") }
        let destination = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, routeID: "route", presentedHostKey: pin(),
            negotiatedTransport: evidence(inbound: otherCipher), support: profile())
        XCTAssertEqual(destination.negotiatedTransport?.inboundCipher, otherCipher)
    }
}
