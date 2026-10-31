import Foundation
import XCTest
@testable import FrankenTermKit

private struct HostCertificatePolicySigner: WorkspaceSSHSigning {
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

/// Signed certificates exercise real workspace, import, durable-policy and
/// authentication paths. Doubles replace storage boundaries, not verification.
@MainActor
final class WorkspaceHostCertificatePolicyTests: XCTestCase {
    private let crypto = TestCryptography()
    private let support = WorkspaceDocument.RouteSupport(
        routeModes: ["direct_ssh", "frankenterm_mux"],
        authenticationMethods: [.publicKey, .password, .keyboardInteractive],
        credentialAlgorithms: ["ssh-ed25519"],
        hostKeyAlgorithms: ["ssh-ed25519", "ssh-ed25519-cert-v01@openssh.com"],
        capabilities: [WorkspaceDocument.hostCertificateCapability], maximumJumpHops: 3)

    private struct Fixture {
        let store: WorkspaceStore
        let disk: StoreTestPersistence
        let vault: StoreTestVault
        let saved: InstalledWorkspace
        let archive: AirGapArchive
    }

    private struct Grants {
        let publicKey: WorkspaceSSHAuthorization
        let password: WorkspaceSSHPasswordAuthorization
        let keyboardInteractive: WorkspaceSSHKeyboardInteractiveAuthorization
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

    private func rewrite(_ document: WorkspaceDocument, bundle: String? = nil,
                         edit: (inout [String: Any]) -> Void) throws -> WorkspaceDocument {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: document.raw) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        edit(&workspace); root["workspace"] = workspace
        if let bundle { root["bundle_id"] = bundle }
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }

    private func document(_ certificate: SSHHostCertificate, bundle: String = "host-certificates",
                          authorities: [WorkspaceDocument.HostKey]? = nil,
                          pins: [WorkspaceDocument.HostKey] = [],
                          edit: (inout [String: Any]) -> Void = { _ in }) throws -> WorkspaceDocument {
        let (_, fingerprint) = try sampleKey(using: crypto)
        let source = try WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint, bundleID: bundle))
        let authorities = try authorities ?? [key(certificate.authority)]
        return try rewrite(source) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["pins"] = pins.map(fields)
            trusts[0]["certificate_authorities"] = authorities.map(fields)
            trusts[0]["required_capabilities"] = authorities.isEmpty ? [] : [WorkspaceDocument.hostCertificateCapability]
            workspace["host_trust"] = trusts
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["authentication_methods"] = ["publickey", "password", "keyboard-interactive"]
            workspace["routes"] = routes
            edit(&workspace)
        }
    }

    private func archive(_ document: WorkspaceDocument) throws -> AirGapArchive {
        let (privateKey, _) = try sampleKey(using: crypto)
        return try AirGapArchive(workspace: document, keys: [privateKey], using: crypto)
    }

    private func install(_ archive: AirGapArchive, in store: WorkspaceStore, identity: UInt8,
                         replacing previous: InstalledWorkspace? = nil,
                         allowRollback: Bool = false) async throws -> InstalledWorkspace {
        try await store.install(archive, envelopeDigest: Data(repeating: identity, count: 32),
            transferID: Data(repeating: identity, count: 16), expectedRevision: previous?.revision,
            sourceAndContentsConfirmed: true, allowHistoricalRollback: allowRollback,
            securityChangesConfirmed: previous != nil, associateWithExistingWorkspace: previous != nil).workspace
    }

    private func fixture(_ document: WorkspaceDocument) async throws -> Fixture {
        let disk = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: disk, vault: vault), source = try archive(document)
        let saved = try await install(source, in: store, identity: 1)
        return .init(store: store, disk: disk, vault: vault, saved: saved, archive: source)
    }

    private func readiness(_ document: WorkspaceDocument,
                           support: WorkspaceDocument.RouteSupport? = nil) throws -> WorkspaceDocument.RouteReadiness {
        try document.readiness(forRouteID: "route", availableCredentialIDs: ["key"],
            reviewedTrustIDs: Set(document.hostTrust.map(\.id)), support: support ?? self.support)
    }

    private func verify(_ document: WorkspaceDocument, _ certificate: SSHHostCertificate,
                        routeID: String = "route") throws -> WorkspaceDocument.HostVerificationDecision {
        try document.verifyHostKey(forRouteID: routeID, presented: key(certificate), hostCertificate: certificate,
            reviewedTrustIDs: Set(document.hostTrust.map(\.id)), support: support)
    }

    private func grants(_ fixture: Fixture, certificate: SSHHostCertificate,
                        saved: InstalledWorkspace? = nil) async throws -> Grants {
        let saved = saved ?? fixture.saved, presented = try key(certificate)
        let publicKey = try await fixture.store.authorizeSSHAuthentication(workspaceID: saved.id,
            expectedRevision: saved.revision, routeID: "route", credentialID: "key",
            presentedHostKey: presented, hostCertificate: certificate, support: support)
        let password = try await fixture.store.authorizeSSHPasswordAuthentication(workspaceID: saved.id,
            expectedRevision: saved.revision, routeID: "route", presentedHostKey: presented,
            hostCertificate: certificate, support: support)
        let keyboardInteractive = try await fixture.store.authorizeSSHKeyboardInteractiveAuthentication(
            workspaceID: saved.id, expectedRevision: saved.revision, routeID: "route",
            presentedHostKey: presented, hostCertificate: certificate, support: support)
        return .init(publicKey: publicKey, password: password, keyboardInteractive: keyboardInteractive)
    }

    private func expectAuthenticationError(_ expected: SSHAuthenticationError,
        file: StaticString = #filePath, line: UInt = #line,
        operation: () async throws -> Void) async {
        do { try await operation(); XCTFail("authentication accepted stale or denied host evidence", file: file, line: line) }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, expected, file: file, line: line) }
    }

    private func assertInvalid(_ grants: Grants, in store: WorkspaceStore, error: SSHAuthenticationError,
                               file: StaticString = #filePath, line: UInt = #line) async {
        await expectAuthenticationError(error, file: file, line: line) {
            try await store.validateSSHAuthentication(grants.publicKey, support: self.support)
        }
        await expectAuthenticationError(error, file: file, line: line) {
            try await store.validateSSHPasswordAuthentication(grants.password, support: self.support)
        }
        await expectAuthenticationError(error, file: file, line: line) {
            try await store.validateSSHKeyboardInteractiveAuthentication(grants.keyboardInteractive, support: self.support)
        }
    }

    func testSignedAuthorityOnlyTrustRequiresReviewAndQualifiedCertificateTransport() throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate())
        let document = try document(certificate), trust = try XCTUnwrap(document.hostTrust.first)
        XCTAssertTrue(trust.pins.isEmpty)
        XCTAssertEqual(trust.certificateAuthorities, [try key(certificate.authority)])
        XCTAssertTrue(trust.hasConfiguredIdentity)
        XCTAssertTrue(try readiness(document).isLocallyReady)
        XCTAssertEqual(try verify(document, certificate), .trustedCertificate)
        XCTAssertEqual(try document.verifyHostKey(forRouteID: "route", presented: key(certificate),
            hostCertificate: certificate, reviewedTrustIDs: [], support: support), .reviewRequired)
        let rawOnly = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
            credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"])
        let legacy = try readiness(document, support: rawOnly)
        XCTAssertTrue(legacy.issues.contains(.unsupportedRequirement))
        XCTAssertTrue(legacy.issues.contains(.hostAlgorithmUnavailable))
        XCTAssertFalse(legacy.issues.contains(.hostTrustMissing))
        XCTAssertEqual(try document.verifyHostKey(forRouteID: "route", presented: key(certificate),
            hostCertificate: certificate, reviewedTrustIDs: ["trust"], support: rawOnly), .unsupportedPolicy)
        // The CA field itself is mandatory policy even if a producer omits its marker.
        let withoutMarker = try rewrite(document) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0].removeValue(forKey: "required_capabilities"); workspace["host_trust"] = trusts
        }
        XCTAssertTrue(try readiness(withoutMarker, support: rawOnly).issues.contains(.unsupportedRequirement))
        let detail = try document.readiness(for: "route", availableCredentialIDs: ["key"], acceptedTrustIDs: ["trust"],
            supportedAuthenticationMethods: support.authenticationMethods,
            supportedCredentialAlgorithms: support.credentialAlgorithms,
            supportedHostKeyAlgorithms: support.hostKeyAlgorithms, supportedCapabilities: support.capabilities)
        XCTAssertTrue(detail.issues.isEmpty)
    }

    func testCertificateEvidenceMustMatchTheExactPresentedBlobAndAuthority() throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate())
        let renewed = try SSHHostCertificate(blob: workspaceTestHostCertificate(serial: 2))
        let otherSubject = try SSHHostCertificate(blob: workspaceTestHostCertificate(subjectSeed: Data(repeating: 7, count: 32)))
        let otherAuthority = try SSHHostCertificate(blob: workspaceTestHostCertificate(authoritySeed: Data(repeating: 8, count: 32)))
        let document = try document(certificate), presented = try key(certificate)
        XCTAssertEqual(try document.verifyHostKey(forRouteID: "route", presented: presented,
            reviewedTrustIDs: ["trust"], support: support), .unsupportedCertificate)
        XCTAssertEqual(try document.verifyHostKey(forRouteID: "route", presented: presented,
            certificateAuthority: key(certificate.authority), reviewedTrustIDs: ["trust"], support: support), .unsupportedCertificate)
        for witness in [renewed, otherSubject, otherAuthority] {
            XCTAssertEqual(try document.verifyHostKey(forRouteID: "route", presented: presented, hostCertificate: witness,
                reviewedTrustIDs: ["trust"], support: support), .unsupportedCertificate)
        }
        XCTAssertEqual(try document.verifyHostKey(forRouteID: "route", presented: key(certificate.subject),
            hostCertificate: certificate, reviewedTrustIDs: ["trust"], support: support), .unsupportedCertificate)
        XCTAssertEqual(try document.verifyHostKey(forRouteID: "route", presented: presented,
            certificateAuthority: key(otherAuthority.authority), hostCertificate: certificate,
            reviewedTrustIDs: ["trust"], support: support), .unsupportedCertificate)
        XCTAssertEqual(try verify(document, otherAuthority), .pinMismatch)
        XCTAssertEqual(try document.verifyHostKey(forRouteID: "route", presented: key(certificate.subject),
            reviewedTrustIDs: ["trust"], support: support), .pinMismatch)
        var forged = certificate.blob
        forged[forged.count - 1] ^= 1
        XCTAssertThrowsError(try SSHHostCertificate(blob: forged), "a supplied CA label cannot turn an invalid signature into a witness")
    }

    func testAliasPrincipalUsesOriginalBytesWhileDenialsUseCanonicalScope() throws {
        let matching = try SSHHostCertificate(blob: workspaceTestHostCertificate(principals: ["Verified-Alias."]))
        let aliasDocument = try document(matching) { workspace in
            var routes = workspace["routes"] as! [[String: Any]], trusts = workspace["host_trust"] as! [[String: Any]]
            routes[0]["host_key_alias"] = "Verified-Alias."; routes[0]["port"] = 2222
            trusts[0]["host_key_alias"] = "Verified-Alias."; trusts[0]["port"] = 2222
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }
        XCTAssertEqual(aliasDocument.hostTrust[0].hostKeyAlias, "verified-alias")
        XCTAssertEqual(aliasDocument.hostTrust[0].certificatePrincipal, "Verified-Alias.")
        XCTAssertEqual(try verify(aliasDocument, matching), .trustedCertificate)
        for principal in ["verified-alias", "verified-alias.", "Verified-Alias", "host.example.invalid", "[Verified-Alias.]:2222"] {
            let wrong = try SSHHostCertificate(blob: workspaceTestHostCertificate(principals: [principal]))
            XCTAssertEqual(try verify(aliasDocument, wrong), .unsupportedCertificate, principal)
        }
        let canonical = try rewrite(aliasDocument) { workspace in
            var routes = workspace["routes"] as! [[String: Any]], trusts = workspace["host_trust"] as! [[String: Any]]
            routes[0]["host_key_alias"] = "verified-alias"; trusts[0]["host_key_alias"] = "verified-alias"
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }
        XCTAssertEqual(aliasDocument.hostTrust[0].canonicalTrustScope, canonical.hostTrust[0].canonicalTrustScope)
        XCTAssertEqual(try aliasDocument.verifyHostKey(forRouteID: "route", presented: key(matching), hostCertificate: matching,
            reviewedTrustIDs: ["trust"], support: support,
            revokedHostKeysByTrustScope: [canonical.hostTrust[0].canonicalTrustScope: [key(matching.authority)]]), .revoked)
    }

    func testHostnamePrincipalDoesNotLoseTheOriginalTrailingDot() throws {
        let dotted = try SSHHostCertificate(blob: workspaceTestHostCertificate(principals: ["host.example.invalid."]))
        let document = try document(dotted) { workspace in
            var routes = workspace["routes"] as! [[String: Any]], trusts = workspace["host_trust"] as! [[String: Any]]
            routes[0]["host"] = "host.example.invalid."; trusts[0]["host"] = "host.example.invalid."
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }
        XCTAssertEqual(document.routes[0].host, "host.example.invalid")
        XCTAssertEqual(document.hostTrust[0].certificatePrincipal, "host.example.invalid.")
        XCTAssertEqual(try verify(document, dotted), .trustedCertificate)
        XCTAssertEqual(try verify(document, SSHHostCertificate(blob: workspaceTestHostCertificate())), .unsupportedCertificate)
    }

    func testAuthorityIsNeverBorrowedFromAnotherPortOrTrustRecord() throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate())
        for otherPort in [22, 2222] {
            let scoped = try document(certificate) { workspace in
                var trusts = workspace["host_trust"] as! [[String: Any]], other = trusts[0]
                trusts[0]["certificate_authorities"] = []; trusts[0]["required_capabilities"] = []
                other["id"] = "other-trust"; other["port"] = otherPort
                trusts.append(other); workspace["host_trust"] = trusts
            }
            XCTAssertEqual(try verify(scoped, certificate), .pinMismatch)
            XCTAssertTrue(try readiness(scoped).issues.contains(.hostTrustMissing))
        }
        let original = try document(certificate)
        for (field, value) in [("port", 2222 as Any), ("host", "other.example.invalid" as Any),
                               ("host", "HOST.EXAMPLE.INVALID" as Any), ("host", "host.example.invalid." as Any),
                               ("host_key_alias", "another-name" as Any)] {
            XCTAssertThrowsError(try rewrite(original) { workspace in
                var trusts = workspace["host_trust"] as! [[String: Any]]
                trusts[0][field] = value; workspace["host_trust"] = trusts
            }, "a route must not reference trust from a different endpoint or alias")
        }
        let aliased = try rewrite(original) { workspace in
            var routes = workspace["routes"] as! [[String: Any]], trusts = workspace["host_trust"] as! [[String: Any]]
            routes[0]["host_key_alias"] = "Verified-Alias."; trusts[0]["host_key_alias"] = "Verified-Alias."
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }
        for differentPrincipal in ["verified-alias.", "Verified-Alias", "verified-alias"] {
            XCTAssertThrowsError(try rewrite(aliased) { workspace in
                var trusts = workspace["host_trust"] as! [[String: Any]]
                trusts[0]["host_key_alias"] = differentPrincipal; workspace["host_trust"] = trusts
            }, "canonical denial-scope equivalence cannot authorize a different signed principal")
        }
        let composed = "caf\u{00e9}.example.invalid", decomposed = "cafe\u{0301}.example.invalid"
        XCTAssertEqual(try WorkspaceDocument.host(composed), try WorkspaceDocument.host(decomposed))
        XCTAssertFalse(composed.utf8.elementsEqual(decomposed.utf8))
        let unicode = try rewrite(original) { workspace in
            var routes = workspace["routes"] as! [[String: Any]], trusts = workspace["host_trust"] as! [[String: Any]]
            routes[0]["host"] = composed; trusts[0]["host"] = composed
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }
        XCTAssertThrowsError(try rewrite(unicode) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["host"] = decomposed; workspace["host_trust"] = trusts
        }, "Swift String canonical equivalence cannot substitute different certificate-principal bytes")
        let rawOnly = try document(certificate, authorities: [], pins: [key(certificate.subject)])
        XCTAssertNoThrow(try rewrite(rawOnly) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["host"] = "HOST.EXAMPLE.INVALID."; workspace["host_trust"] = trusts
        }, "raw-pin workspaces retain their existing canonical endpoint semantics")
    }

    func testMixedPinsAndAuthoritiesVerifySeparatelyAndRevocationNeverFallsBack() throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate())
        let subject = try key(certificate.subject), authority = try key(certificate.authority)
        let mixed = try document(certificate, pins: [subject])
        XCTAssertEqual(try verify(mixed, certificate), .trustedCertificate)
        XCTAssertEqual(try mixed.verifyHostKey(forRouteID: "route", presented: subject,
            reviewedTrustIDs: ["trust"], support: support), .trustedPin)
        let caRevoked = try rewrite(mixed) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["revoked_keys"] = [fields(authority)]; workspace["host_trust"] = trusts
        }
        XCTAssertTrue(try readiness(caRevoked).isLocallyReady, "the separately reviewed raw pin remains usable")
        XCTAssertEqual(try caRevoked.verifyHostKey(forRouteID: "route", presented: subject,
            reviewedTrustIDs: ["trust"], support: support), .trustedPin)
        XCTAssertEqual(try verify(caRevoked, certificate), .revoked, "a certificate cannot fall back to its pinned subject")
        let subjectRevoked = try rewrite(mixed) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["revoked_keys"] = [fields(subject)]; workspace["host_trust"] = trusts
        }
        XCTAssertEqual(try verify(subjectRevoked, certificate), .revoked)
    }

    func testNegativeAuthoritySubjectAndCertificateWinBeforeQualificationOrReview() throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate())
        let source = try document(certificate)
        for revoked in try [key(certificate.authority), key(certificate.subject), key(certificate)] {
            let document = try rewrite(source) { workspace in
                var trusts = workspace["host_trust"] as! [[String: Any]], denial = trusts[0]
                denial["id"] = "same-scope-denial"; denial["pins"] = []; denial["certificate_authorities"] = []
                denial["revoked_keys"] = [fields(revoked)]; denial["required_capabilities"] = []
                trusts.append(denial); workspace["host_trust"] = trusts
            }
            XCTAssertEqual(try document.verifyHostKey(forRouteID: "route", presented: key(certificate),
                hostCertificate: certificate, reviewedTrustIDs: [], support: .offlineOnly), .revoked)
        }
        XCTAssertEqual(try source.verifyHostKey(forRouteID: "route", presented: key(certificate),
            hostCertificate: certificate, reviewedTrustIDs: ["trust"], locallyRejectedTrustIDs: ["trust"],
            support: support), .locallyRejected)
    }

    func testImportedCertificateDenialsSurviveRenamingReopenAndHistoricalReplay() async throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate())
        for revoked in try [key(certificate.authority), key(certificate.subject), key(certificate)] {
            let original = try document(certificate), f = try await fixture(original)
            let denied = try rewrite(original, bundle: "revoked") { workspace in
                var trusts = workspace["host_trust"] as! [[String: Any]]
                trusts[0]["revoked_keys"] = [fields(revoked)]; workspace["host_trust"] = trusts
            }
            let second = try await install(archive(denied), in: f.store, identity: 2, replacing: f.saved)
            let renamed = try rewrite(original, bundle: "renamed") { workspace in
                var routes = workspace["routes"] as! [[String: Any]], trusts = workspace["host_trust"] as! [[String: Any]]
                routes[0]["host_trust_ref"] = "renamed-trust"; trusts[0]["id"] = "renamed-trust"
                workspace["routes"] = routes; workspace["host_trust"] = trusts
            }
            let third = try await install(archive(renamed), in: f.store, identity: 3, replacing: second)
            XCTAssertTrue(try third.document().hostTrust[0].revokedKeys.isEmpty)
            let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault)
            let decision = try await reopened.verifyHostKey(workspaceID: third.id, routeID: "route",
                presented: key(certificate), hostCertificate: certificate, support: support)
            XCTAssertEqual(decision, .revoked)
            let historical = try await install(f.archive, in: reopened, identity: 1, replacing: third, allowRollback: true)
            let afterReplay = try await reopened.verifyHostKey(workspaceID: historical.id, routeID: "route",
                presented: key(certificate), hostCertificate: certificate, support: support)
            XCTAssertEqual(afterReplay, .revoked)
            XCTAssertTrue(historical.connectLater)
            XCTAssertEqual(f.vault.creates, 1)
        }
    }

    func testOldUnresolvedTrustPolicyCannotBeClearedByACertificateBearingReimport() async throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate())
        let blocked = try document(certificate, bundle: "old-source", authorities: []) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["required_capabilities"] = ["ssh_config.unresolved_policy"]
            workspace["host_trust"] = trusts
        }
        let f = try await fixture(blocked)
        let upgraded = try document(certificate, bundle: "new-ca-source")
        let replacement = try await install(archive(upgraded), in: f.store, identity: 2, replacing: f.saved)
        let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault)
        let states = try await reopened.routeReadiness(workspaceID: replacement.id, support: support)
        XCTAssertTrue(try XCTUnwrap(states.first).issues.contains(.unsupportedRequirement))
        let decision = try await reopened.verifyHostKey(workspaceID: replacement.id, routeID: "route",
            presented: key(certificate), hostCertificate: certificate, support: support)
        XCTAssertEqual(decision, .unsupportedPolicy)
        let required = try await reopened.requiredHostCapabilities(workspaceID: replacement.id)
        let scope = upgraded.hostTrust[0].canonicalTrustScope
        XCTAssertTrue(required[scope]?.contains("ssh_config.unresolved_policy") == true)
        XCTAssertTrue(required[scope]?.contains(WorkspaceDocument.hostCertificateCapability) == true)
    }

    func testEveryAuthenticationGrantRetainsTheExactCertificateAndRejectsAChangedRevision() async throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate())
        let document = try document(certificate), f = try await fixture(document)
        let granted = try await grants(f, certificate: certificate)
        XCTAssertEqual(granted.publicKey.hostCertificate?.blob, certificate.blob)
        XCTAssertEqual(granted.password.hostCertificate?.blob, certificate.blob)
        XCTAssertEqual(granted.keyboardInteractive.hostCertificate?.blob, certificate.blob)
        XCTAssertEqual(granted.publicKey.presentedHostKey, try key(certificate))
        try await f.store.validateSSHAuthentication(granted.publicKey, support: support)
        try await f.store.validateSSHPasswordAuthentication(granted.password, support: support)
        try await f.store.validateSSHKeyboardInteractiveAuthentication(granted.keyboardInteractive, support: support)
        let next = try document.reissuingBundle(id: "next-import")
        let replacement = try await install(archive(next), in: f.store, identity: 2, replacing: f.saved)
        XCTAssertNotEqual(replacement.revision, f.saved.revision)
        await assertInvalid(granted, in: f.store, error: .stale)
        let current = try await grants(f, certificate: certificate, saved: replacement)
        XCTAssertEqual(current.password.hostCertificate, certificate)
        await assertInvalid(current, in: WorkspaceStore(persistence: f.disk, vault: f.vault), error: .stale)
    }

    func testEveryGrantRechecksLocalAuthoritySubjectAndCertificateDenialsWithoutRevisionChange() async throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate())
        for revoked in try [key(certificate.authority), key(certificate.subject), key(certificate)] {
            let f = try await fixture(document(certificate)), granted = try await grants(f, certificate: certificate)
            try await f.store.revokeHostLocally(workspaceID: f.saved.id, trustID: "trust", key: revoked)
            let stillInstalled = try await f.store.load()
            XCTAssertEqual(stillInstalled.first?.revision, f.saved.revision)
            await assertInvalid(granted, in: f.store, error: .denied)
            let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault)
            let decision = try await reopened.verifyHostKey(workspaceID: f.saved.id, routeID: "route",
                presented: key(certificate), hostCertificate: certificate, support: support)
            XCTAssertEqual(decision, .revoked)
        }
    }

    func testAuthenticationCannotOfferAnyMethodWithMissingOrDifferentCertificateEvidence() async throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate())
        let another = try SSHHostCertificate(blob: workspaceTestHostCertificate(serial: 2))
        let f = try await fixture(document(certificate)), presented = try key(certificate)
        for witness in [nil, another] as [SSHHostCertificate?] {
            await expectAuthenticationError(.denied) {
                _ = try await f.store.authorizeSSHAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
                    routeID: "route", credentialID: "key", presentedHostKey: presented, hostCertificate: witness, support: self.support)
            }
            await expectAuthenticationError(.denied) {
                _ = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
                    routeID: "route", presentedHostKey: presented, hostCertificate: witness, support: self.support)
            }
            await expectAuthenticationError(.denied) {
                _ = try await f.store.authorizeSSHKeyboardInteractiveAuthentication(workspaceID: f.saved.id,
                    expectedRevision: f.saved.revision, routeID: "route", presentedHostKey: presented,
                    hostCertificate: witness, support: self.support)
            }
        }
        let wrongCA = try SSHHostCertificate(blob: workspaceTestHostCertificate(authoritySeed: Data(repeating: 8, count: 32)))
        let before = f.disk.bytes(.catalog)
        for method in ["publickey", "password", "keyboard-interactive"] {
            do {
                switch method {
                case "publickey":
                    _ = try await f.store.authorizeSSHAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
                        routeID: "route", credentialID: "key", presentedHostKey: key(wrongCA),
                        hostCertificate: wrongCA, support: support)
                case "password":
                    _ = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: f.saved.id,
                        expectedRevision: f.saved.revision, routeID: "route", presentedHostKey: key(wrongCA),
                        hostCertificate: wrongCA, support: support)
                default:
                    _ = try await f.store.authorizeSSHKeyboardInteractiveAuthentication(workspaceID: f.saved.id,
                        expectedRevision: f.saved.revision, routeID: "route", presentedHostKey: key(wrongCA),
                        hostCertificate: wrongCA, support: support)
                }
                XCTFail("a verified certificate from an untrusted issuer authorized \(method)")
            } catch {
                let mismatch = try XCTUnwrap(error as? WorkspaceSSHHostKeyMismatch)
                XCTAssertEqual(mismatch.workspaceID, f.saved.id)
                XCTAssertEqual(mismatch.expectedRevision, f.saved.revision)
                XCTAssertEqual(mismatch.routeID, "route")
                XCTAssertEqual(mismatch.certificatePrincipal, "host.example.invalid")
                XCTAssertTrue(mismatch.expectedKeys.isEmpty)
                XCTAssertEqual(mismatch.expectedCertificateAuthorities, f.archive.workspace.hostTrust[0].certificateAuthorities)
                XCTAssertEqual(mismatch.presentedKey, try key(wrongCA))
                XCTAssertEqual(mismatch.hostCertificate?.blob, wrongCA.blob)
                XCTAssertFalse(mismatch.description.contains(wrongCA.fingerprint))
            }
            XCTAssertEqual(f.disk.bytes(.catalog), before, "mismatch review must not publish trust")
        }
    }

    func testExpiredOrNotYetValidCertificateCannotAuthorizeAnyInitialAuthenticationMethod() async throws {
        let certificates = try [
            SSHHostCertificate(blob: workspaceTestHostCertificate(validBefore: 1_577_836_800)),
            SSHHostCertificate(blob: workspaceTestHostCertificate(validAfter: 4_102_444_800, validBefore: 7_258_118_400))
        ]
        for certificate in certificates {
            let source = try document(certificate), f = try await fixture(source), presented = try key(certificate)
            XCTAssertTrue(try readiness(source).isLocallyReady, "local CA readiness is not proof about a future peer certificate")
            XCTAssertEqual(try verify(source, certificate), .unsupportedCertificate)
            await expectAuthenticationError(.denied) {
                _ = try await f.store.authorizeSSHAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
                    routeID: "route", credentialID: "key", presentedHostKey: presented,
                    hostCertificate: certificate, support: self.support)
            }
            await expectAuthenticationError(.denied) {
                _ = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
                    routeID: "route", presentedHostKey: presented, hostCertificate: certificate, support: self.support)
            }
            await expectAuthenticationError(.denied) {
                _ = try await f.store.authorizeSSHKeyboardInteractiveAuthentication(workspaceID: f.saved.id,
                    expectedRevision: f.saved.revision, routeID: "route", presentedHostKey: presented,
                    hostCertificate: certificate, support: self.support)
            }
        }
    }

    func testPhonePinReviewAndForgedOverlayCannotReplaceImportedAuthorityTrust() async throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate())
        let document = try document(certificate), f = try await fixture(document)
        XCTAssertNil(try f.saved.phoneHostKeyPin(forRouteID: "route"))
        do {
            _ = try await f.store.hostKeyVerificationDocument(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
                routeID: "route", support: support)
            XCTFail("CA trust was treated as an unconfigured endpoint")
        } catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .alreadyConfigured) }
        do {
            _ = try await f.store.reviewHostKeyPin(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
                routeID: "route", key: key(certificate.subject), support: support)
            XCTFail("phone-local raw pin replaced imported CA policy")
        } catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .alreadyConfigured) }
        let forged = WorkspaceHostKeyPin(trustID: "trust", key: try key(certificate.subject),
            sourceWitness: try WorkspaceHostKeyOverlay.witness(trustID: "trust", in: document))
        let overlaid = try WorkspaceHostKeyOverlay.document(effective: document, imported: document, pins: [forged])
        XCTAssertTrue(overlaid.hostTrust[0].pins.isEmpty)
        XCTAssertEqual(overlaid.hostTrust[0].certificateAuthorities, document.hostTrust[0].certificateAuthorities)
        XCTAssertEqual(try verify(overlaid, certificate), .trustedCertificate)
    }

    func testBrokerRetainsCertificateAcrossAllMethodOffersAndRejectsLaterRevocation() async throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate())
        let f = try await fixture(document(certificate)), (privateKey, _) = try sampleKey(using: crypto)
        let broker = SSHAuthenticationBroker(store: f.store,
            signer: HostCertificatePolicySigner(container: privateKey.container), support: support)
        await broker.resume()
        let presented = try key(certificate)
        let publicKey = try await broker.publicIdentity(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: presented,
            hostCertificate: certificate, transportID: UUID())
        let password = try await broker.passwordIdentity(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", presentedHostKey: presented, hostCertificate: certificate)
        let keyboardInteractive = try await broker.keyboardInteractiveIdentity(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, routeID: "route", presentedHostKey: presented, hostCertificate: certificate)
        XCTAssertEqual(publicKey.authorization.hostCertificate?.blob, certificate.blob)
        XCTAssertEqual(password.authorization.hostCertificate?.blob, certificate.blob)
        XCTAssertEqual(keyboardInteractive.authorization.hostCertificate?.blob, certificate.blob)
        XCTAssertEqual(publicKey.authenticationBlob, privateKey.publicBlob, "host and user certificate evidence have different roles")
        try await broker.validateForAuthentication(publicKey)
        try await broker.validatePasswordAttempt(password)
        try await broker.validateKeyboardInteractiveAttempt(keyboardInteractive)
        let signedRequest = try await broker.prepare(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: presented, hostCertificate: certificate,
            transportID: UUID(), sessionIdentifier: Data(repeating: 0xB7, count: 32), publicBlob: privateKey.publicBlob)
        let signature = try await broker.confirm(requestID: signedRequest.id)
        XCTAssertEqual(signature.authorization.hostCertificate?.blob, certificate.blob)
        XCTAssertEqual(signature.publicBlob, privateKey.publicBlob)
        let pending = try await broker.prepare(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: presented, hostCertificate: certificate,
            transportID: UUID(), sessionIdentifier: Data(repeating: 0xC8, count: 32), publicBlob: privateKey.publicBlob)
        try await f.store.revokeHostLocally(workspaceID: f.saved.id, trustID: "trust", key: key(certificate.subject))
        await expectAuthenticationError(.denied) { _ = try await broker.confirm(requestID: pending.id) }
        await expectAuthenticationError(.denied) { try await broker.validate(publicKey) }
        await expectAuthenticationError(.denied) { try await broker.validatePasswordAttempt(password) }
        await expectAuthenticationError(.denied) { try await broker.validateKeyboardInteractiveAttempt(keyboardInteractive) }
        await broker.finishKeyboardInteractiveAttempt(keyboardInteractive)
        await broker.suspend()
    }
}
