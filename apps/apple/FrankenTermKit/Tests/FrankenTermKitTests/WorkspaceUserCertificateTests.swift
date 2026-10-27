import Foundation
import XCTest
@testable import FrankenTermKit
#if canImport(CryptoKit)
import CryptoKit
#else
import Crypto
#endif

/// Builds a certificate independently of the production parser so lifecycle
/// tests can select exact validity/permission boundaries without sleeping.
/// Parser interoperability is covered separately by ssh-keygen fixtures.
func workspaceTestCertificate(serial: UInt64 = 1, principals: [String] = ["operator"],
                              validAfter: UInt64 = 946_684_800, validBefore: UInt64 = 4_102_444_800,
                              extensions: [String] = ["permit-port-forwarding", "permit-pty"],
                              subjectSeed: Data = Data(0..<32)) throws -> Data {
    func field(_ bytes: Data) -> Data {
        var value = Data(); value.appendBE(UInt64(bytes.count), bytes: 4); value.append(bytes); return value
    }
    let subject = try Curve25519.Signing.PrivateKey(rawRepresentation: subjectSeed)
    let authority = try Curve25519.Signing.PrivateKey(rawRepresentation: Data(32..<64))
    var result = field(Data("ssh-ed25519-cert-v01@openssh.com".utf8))
    result.append(field(Data(repeating: 19, count: 32)))
    result.append(field(subject.publicKey.rawRepresentation))
    result.appendBE(serial, bytes: 8); result.appendBE(1, bytes: 4)
    result.append(field(Data("workspace-fixture".utf8)))
    result.append(field(principals.reduce(into: Data()) { $0.append(field(Data($1.utf8))) }))
    result.appendBE(validAfter, bytes: 8); result.appendBE(validBefore, bytes: 8)
    result.append(field(Data()))
    result.append(field(extensions.sorted().reduce(into: Data()) {
        $0.append(field(Data($1.utf8))); $0.append(field(Data()))
    }))
    result.append(field(Data()))
    result.append(field(publicBlob(authority.publicKey.rawRepresentation)))
    let signature = try authority.signature(for: result)
    result.append(field(field(Data("ssh-ed25519".utf8)) + field(signature)))
    return result
}

@MainActor
final class WorkspaceUserCertificateTests: XCTestCase {
    private let crypto = TestCryptography()
    private let support = WorkspaceDocument.RouteSupport(
        routeModes: ["direct_ssh", "frankenterm_mux"], authenticationMethods: [.publicKey, .password, .keyboardInteractive],
        credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"],
        capabilities: [WorkspaceDocument.userCertificateCapability], maximumJumpHops: 3)

    private func document(_ certificates: [(String, Data)], bundleID: String = "certificates",
                          mutate: (inout [String: Any]) -> Void = { _ in }) throws -> WorkspaceDocument {
        let (_, fingerprint) = try sampleKey(using: crypto)
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest(fingerprint: fingerprint, bundleID: bundleID)) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        workspace["credentials"] = certificates.map { id, blob in
            ["id": id, "algorithm": "ssh-ed25519", "public_fingerprint": fingerprint,
             "requirement": "matching_private_key", "private_material": "not_included",
             "required_capabilities": [WorkspaceDocument.userCertificateCapability],
             "user_certificate": blob.base64EncodedString()] as [String: Any]
        }
        var routes = try XCTUnwrap(workspace["routes"] as? [[String: Any]])
        routes[0]["credential_refs"] = certificates.map(\.0); workspace["routes"] = routes
        mutate(&workspace); root["workspace"] = workspace
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }

    private func readiness(_ document: WorkspaceDocument, time: Date? = Date()) throws -> WorkspaceDocument.RouteReadiness {
        try document.readiness(forRouteID: "route", availableCredentialIDs: Set(document.credentials.map(\.id)),
            reviewedTrustIDs: Set(document.hostTrust.map(\.id)), support: support, certificateTime: time)
    }

    func testCertificateRequiresExplicitCapabilityAndExactSubjectIdentity() throws {
        let certificate = try workspaceTestCertificate(), original = try document([("key", certificate)])
        XCTAssertEqual(original.credentials[0].userCertificate?.blob, certificate)
        XCTAssertTrue(try readiness(original).isLocallyReady)
        let legacy = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
            credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"])
        let blocked = try original.readiness(forRouteID: "route", availableCredentialIDs: ["key"],
            reviewedTrustIDs: ["trust"], support: legacy)
        XCTAssertEqual(blocked.issues, [.unsupportedRequirement]); XCTAssertTrue(blocked.usableCredentialIDs.isEmpty)
        for field in ["required_capabilities", "user_certificate"] {
            XCTAssertThrowsError(try document([("key", certificate)]) { workspace in
                var credentials = workspace["credentials"] as! [[String: Any]]
                credentials[0].removeValue(forKey: field); workspace["credentials"] = credentials
            })
        }
        XCTAssertThrowsError(try document([("key", certificate)]) { workspace in
            var credentials = workspace["credentials"] as! [[String: Any]]
            credentials[0]["public_fingerprint"] = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            workspace["credentials"] = credentials
        })
        XCTAssertThrowsError(try document([("key", certificate)]) { workspace in
            var credentials = workspace["credentials"] as! [[String: Any]]
            credentials[0]["user_certificate"] = certificate.base64EncodedString() + "\n"
            workspace["credentials"] = credentials
        })
    }

    func testClockAndPermissionChecksSeparateFreshReadinessFromEstablishedPolicy() throws {
        let certificate = try workspaceTestCertificate(validAfter: 100, validBefore: 200)
        let bounded = try document([("key", certificate)])
        XCTAssertEqual(try readiness(bounded, time: Date(timeIntervalSince1970: 99)).issues, [.credentialCertificateUnavailable])
        XCTAssertTrue(try readiness(bounded, time: Date(timeIntervalSince1970: 100)).isLocallyReady)
        XCTAssertEqual(try readiness(bounded, time: Date(timeIntervalSince1970: 200)).issues, [.credentialCertificateUnavailable])
        XCTAssertTrue(try readiness(bounded, time: nil).isLocallyReady, "an established session retains its certificate after expiry")
        let wrongAccount = try document([("key", workspaceTestCertificate(principals: ["other"]))])
        XCTAssertEqual(try readiness(wrongAccount, time: nil).issues, [.credentialCertificateUnavailable],
            "durable validation still enforces the account and certificate permissions")
        let noPTY = try workspaceTestCertificate(extensions: ["permit-port-forwarding"])
        XCTAssertEqual(try readiness(document([("key", noPTY)])).issues, [.credentialCertificateUnavailable])
        let mux = try document([("key", noPTY)]) { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["mode"] = "frankenterm_mux"; routes[0]["attach_policy"] = "existing_only"
            routes[0].removeValue(forKey: "session_creation"); workspace["routes"] = routes
        }
        XCTAssertTrue(try readiness(mux).isLocallyReady, "a mux exec channel does not request a PTY")
    }

    func testTransitRequiresForwardingWithoutRequiringTheHopShellPTY() throws {
        func workspace(hop: Data) throws -> WorkspaceDocument {
            try document([("key", workspaceTestCertificate()), ("hop-key", hop)]) { workspace in
                var routes = workspace["routes"] as! [[String: Any]], jump = routes[0]
                jump["id"] = "jump"; jump["host"] = "jump.example.invalid"; jump["host_trust_ref"] = "jump-trust"
                jump["credential_refs"] = ["hop-key"]; jump.removeValue(forKey: "session_creation")
                routes[0]["credential_refs"] = ["key"]; routes[0]["jump_routes"] = ["jump"]
                routes.append(jump); workspace["routes"] = routes
                var trust = workspace["host_trust"] as! [[String: Any]], jumpTrust = trust[0]
                jumpTrust["id"] = "jump-trust"; jumpTrust["host"] = "jump.example.invalid"
                trust.append(jumpTrust); workspace["host_trust"] = trust
            }
        }
        let permitted = try workspace(hop: workspaceTestCertificate(extensions: ["permit-port-forwarding"]))
        XCTAssertTrue(try readiness(permitted).isLocallyReady)
        let refused = try workspace(hop: workspaceTestCertificate(extensions: ["permit-pty"]))
        XCTAssertEqual(try readiness(refused).issues, [.blockedJumpRoute("jump")])
        let hop = try refused.readiness(forRouteID: "jump", availableCredentialIDs: ["key", "hop-key"],
            reviewedTrustIDs: ["trust", "jump-trust"], support: support, transitForRouteID: "route")
        XCTAssertEqual(hop.issues, [.credentialCertificateUnavailable])
    }

    func testArchiveBindsEveryCertificateToDeclaredRefsWhilePrivateKeyStaysDeduplicated() throws {
        let first = try workspaceTestCertificate(), second = try workspaceTestCertificate(serial: 2)
        let workspace = try document([("key", first), ("renewed", second)])
        let (key, _) = try sampleKey(using: crypto)
        func record(_ certificates: [Data]) -> PrivateKeyRecord {
            .init(credentialID: "key", container: key.container, publicBlob: key.publicBlob, certificates: certificates)
        }
        let archive = try AirGapArchive(workspace: workspace, keys: [record([first, second])], using: crypto)
        let reopened = try AirGapArchive(data: archive.encoded(), using: crypto)
        XCTAssertEqual(reopened.keys.count, 1)
        XCTAssertEqual(reopened.keys[0].certificates, [first, second])
        XCTAssertEqual(reopened.workspace.credentials.compactMap(\.userCertificate).map(\.blob), [first, second])
        XCTAssertNoThrow(try AirGapArchive(workspace: workspace, keys: [record([])], using: crypto))
        XCTAssertThrowsError(try AirGapArchive(workspace: workspace, keys: [record([first, first])], using: crypto))
        XCTAssertThrowsError(try AirGapArchive(workspace: workspace, keys: [record([workspaceTestCertificate(serial: 3)])], using: crypto))
        XCTAssertThrowsError(try AirGapArchive(workspace: workspace,
            keys: [record([workspaceTestCertificate(subjectSeed: Data(repeating: 3, count: 32))])], using: crypto))
    }

    func testPrivateKeyRepairBindsDistinctCertificatesToOneProtectedAccount() async throws {
        let first = try workspaceTestCertificate(), second = try workspaceTestCertificate(serial: 2)
        let workspace = try document([("key", first), ("renewed", second)])
        let vault = StoreTestVault(), disk = StoreTestPersistence(), store = WorkspaceStore(persistence: disk, vault: vault)
        let saved = try await store.install(AirGapArchive(workspace: workspace, keys: [], using: crypto),
            envelopeDigest: Data(repeating: 1, count: 32), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let review = try await store.reviewCredentialImport(workspaceID: saved.id, expectedRevision: saved.revision, credentialID: "renewed")
        let (key, _) = try sampleKey(using: crypto)
        let selected = PrivateKeyRecord(credentialID: "renewed", container: key.container, publicBlob: key.publicBlob)
        let repaired = try await store.importCredential(selected, review: review, confirmedByUser: true).workspace
        XCTAssertEqual(vault.creates, 1); XCTAssertEqual(Set(repaired.bindings.map(\.account)).count, 1)
        XCTAssertEqual(Set(repaired.bindings.map(\.credentialID)), ["key", "renewed"])
        XCTAssertEqual(try repaired.document().credentials.compactMap(\.userCertificate).map(\.blob), [first, second])
        let loaded = try await WorkspaceStore(persistence: disk, vault: vault).load()
        XCTAssertEqual(loaded[0].manifest, workspace.raw)
    }

    func testRenewalReusesPrivateAccountAndInvalidatesThePriorCertificateAuthorization() async throws {
        let first = try workspaceTestCertificate(), second = try workspaceTestCertificate(serial: 2)
        let original = try document([("key", first)]), renewed = try document([("key", second)], bundleID: "renewal")
        let (key, _) = try sampleKey(using: crypto), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: StoreTestPersistence(), vault: vault)
        let saved = try await store.install(AirGapArchive(workspace: original, keys: [key], using: crypto),
            envelopeDigest: Data(repeating: 2, count: 32), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let host = try XCTUnwrap(original.hostTrust[0].pins.first)
        let authorization = try await store.authorizeSSHAuthentication(workspaceID: saved.id, expectedRevision: saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: host, support: support)
        XCTAssertEqual(authorization.userCertificate?.blob, first)
        let replacement = try AirGapArchive(workspace: renewed, keys: [], using: crypto)
        do {
            _ = try await store.install(replacement, envelopeDigest: Data(repeating: 3, count: 32),
                expectedRevision: saved.revision, sourceAndContentsConfirmed: true, associateWithExistingWorkspace: true)
            XCTFail("certificate renewal must be reviewed as a security change")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .securityChangesNeedApproval) }
        let installed = try await store.install(replacement, envelopeDigest: Data(repeating: 3, count: 32),
            expectedRevision: saved.revision, sourceAndContentsConfirmed: true,
            securityChangesConfirmed: true, associateWithExistingWorkspace: true).workspace
        XCTAssertEqual(installed.bindings, saved.bindings); XCTAssertEqual(vault.creates, 1)
        let current = try await store.authorizeSSHAuthentication(workspaceID: installed.id, expectedRevision: installed.revision,
            routeID: "route", credentialID: "key", presentedHostKey: host, support: support)
        XCTAssertEqual(current.userCertificate?.blob, second)
        do { try await store.validateSSHAuthentication(authorization, support: support); XCTFail("old certificate authority survived renewal") }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
        try await store.revokeLocally(fingerprint: current.fingerprint)
        do { try await store.validateSSHAuthentication(current, support: support); XCTFail("raw-subject revocation must revoke every certificate") }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
    }

    func testExpiredImportKeepsPublicCertificateButCannotIssueNewAuthentication() async throws {
        let certificate = try workspaceTestCertificate(validAfter: 100, validBefore: 200)
        let workspace = try document([("key", certificate)]), (key, fingerprint) = try sampleKey(using: crypto)
        let store = WorkspaceStore(persistence: StoreTestPersistence(), vault: StoreTestVault())
        let saved = try await store.install(AirGapArchive(workspace: workspace, keys: [key], using: crypto),
            envelopeDigest: Data(repeating: 4, count: 32), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let states = try await store.routeReadiness(workspaceID: saved.id, support: support)
        XCTAssertEqual(states[0].issues, [.credentialCertificateUnavailable])
        let host = try XCTUnwrap(workspace.hostTrust[0].pins.first)
        do {
            _ = try await store.authorizeSSHAuthentication(workspaceID: saved.id, expectedRevision: saved.revision,
                routeID: "route", credentialID: "key", presentedHostKey: host, support: support)
            XCTFail("expired certificate granted a new public offer")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
        let selection = try WorkspaceExportSelection(workspace: workspace, groupIDs: ["group"])
        let review = try selection.review(decisions: [fingerprint: .includePrivateKey], loadedFingerprints: [fingerprint])
        XCTAssertEqual(review.blockedRouteIDs, ["route"]); XCTAssertTrue(review.missingIdentityRouteIDs.isEmpty)
        XCTAssertTrue(review.findings.allSatisfy { $0.disposition == 0 }, "a time boundary must not become a permanent required blocker")
    }

    func testExportProjectsAllSelectedCertificatesOnceAndKeepsOneRawIdentityDecision() throws {
        let first = try workspaceTestCertificate(), second = try workspaceTestCertificate(serial: 2)
        let workspace = try document([("key", first), ("renewed", second), ("alias", first)])
        let (key, fingerprint) = try sampleKey(using: crypto)
        let selection = try WorkspaceExportSelection(workspace: workspace, groupIDs: ["group"])
        XCTAssertEqual(selection.identities.count, 1)
        XCTAssertEqual(selection.identities[0].credentialIDs, ["key", "renewed", "alias"])
        let archive = try selection.archive(decisions: [fingerprint: .includePrivateKey], keys: [key], bundleID: "export", using: crypto)
        XCTAssertEqual(archive.keys.count, 1); XCTAssertEqual(archive.keys[0].publicBlob, key.publicBlob)
        XCTAssertEqual(archive.keys[0].certificates, [first, second])
        XCTAssertEqual(archive.workspace.credentials.compactMap(\.userCertificate).map(\.blob), [first, second, first])
        XCTAssertFalse(archive.portability.contains { $0.disposition == 1 })
    }

    func testLargeCertificateSurvivesManifestProjectionArchiveAndEncryptedTransfer() throws {
        let fixture = try UserCertificateFixtures.load().largeCertificate
        let key = try PrivateKeyRecord.fromPEM(Data(fixture.privatePEM.utf8), credentialID: "key", using: crypto)
        let identity = try SSHPublicIdentity(blob: key.publicBlob)
        let certificate = fixture.certificateBlob
        XCTAssertGreaterThan(certificate.base64EncodedString().utf8.count, FTLimit.text)
        let workspace = try document([("key", workspaceTestCertificate())]) { workspace in
            var credentials = workspace["credentials"] as! [[String: Any]]
            credentials[0]["algorithm"] = identity.algorithm
            credentials[0]["public_fingerprint"] = identity.fingerprint
            credentials[0]["user_certificate"] = certificate.base64EncodedString()
            workspace["credentials"] = credentials
        }
        let encoded = try StrictJSON.encode(StrictJSON.decode(workspace.raw))
        XCTAssertEqual(try WorkspaceDocument(data: encoded).credentials[0].userCertificate?.blob, certificate)
        let selection = try WorkspaceExportSelection(workspace: workspace, groupIDs: ["group"])
        let publicDocument = try selection.publicDocument(bundleID: "large-public")
        XCTAssertEqual(publicDocument.credentials[0].userCertificate?.blob, certificate)
        let archive = try selection.archive(including: [key], using: crypto)
        let sealed = try FTAG.seal(archive, using: crypto)
        let opened = try FTAG.open(sealed.envelope, code: sealed.code, using: crypto)
        XCTAssertEqual(opened.workspace.credentials[0].userCertificate?.blob, certificate)
        XCTAssertEqual(opened.keys[0].certificates, [certificate])
        XCTAssertEqual(opened.keys[0].publicBlob, key.publicBlob)
    }

    func testCertificateJSONLimitDoesNotExpandOrdinaryStringsKeysOrArrays() throws {
        func credentialValue(_ value: JSONValue) -> JSONValue {
            .object(["workspace": .object(["credentials": .array([.object(["user_certificate": value])])])])
        }
        func credentialObject(_ value: Any) -> [String: Any] {
            ["workspace": ["credentials": [["user_certificate": value]]]]
        }
        let large = String(repeating: "A", count: FTLimit.certificateText)
        let allowed = credentialValue(.string(large))
        XCTAssertEqual(try StrictJSON.decode(StrictJSON.encode(allowed)), allowed)
        let oversized = credentialValue(.string(large + "A"))
        XCTAssertThrowsError(try StrictJSON.encode(oversized))
        XCTAssertThrowsError(try StrictJSON.decode(JSONSerialization.data(withJSONObject: credentialObject(large + "A"))))
        let ordinary = String(repeating: "a", count: FTLimit.text + 1)
        for value in [JSONValue.string(ordinary), .object(["name": .string(ordinary)]),
                      .object([ordinary: .string("value")]), credentialValue(.array([.string(ordinary)])),
                      credentialValue(.object(["user_certificate": .string(ordinary)]))] {
            XCTAssertThrowsError(try StrictJSON.encode(value))
        }
        for object: [String: Any] in [["name": ordinary], [ordinary: "value"], credentialObject([ordinary]),
                                     credentialObject(["user_certificate": ordinary])] {
            XCTAssertThrowsError(try StrictJSON.decode(JSONSerialization.data(withJSONObject: object)))
        }
        let wrongPaths: [JSONValue] = [
            .object(["user_certificate": .string(ordinary)]),
            .object(["workspace": .object(["user_certificate": .string(ordinary)])]),
            .object(["workspace": .object(["credentials": .object(["user_certificate": .string(ordinary)])])]),
            .object(["workspace": .object(["credentials": .array([.object(["nested": .object(["user_certificate": .string(ordinary)])])])])]),
            .array([credentialValue(.string(ordinary))])
        ]
        for value in wrongPaths { XCTAssertThrowsError(try StrictJSON.encode(value)) }
        let wrongObjects: [Any] = [
            ["user_certificate": ordinary],
            ["workspace": ["user_certificate": ordinary]],
            ["workspace": ["credentials": ["user_certificate": ordinary]]],
            ["workspace": ["credentials": [["nested": ["user_certificate": ordinary]]]]],
            [credentialObject(ordinary)]
        ]
        for object in wrongObjects {
            XCTAssertThrowsError(try StrictJSON.decode(JSONSerialization.data(withJSONObject: object)))
        }
    }
}
