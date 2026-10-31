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
                              subjectSeed: Data = Data(0..<32), authoritySeed: Data = Data(32..<64),
                              criticalOptions: [String: Data] = [:], extensionValues: [String: Data] = [:]) throws -> Data {
    func field(_ bytes: Data) -> Data {
        var value = Data(); value.appendBE(UInt64(bytes.count), bytes: 4); value.append(bytes); return value
    }
    let subject = try Curve25519.Signing.PrivateKey(rawRepresentation: subjectSeed)
    let authority = try Curve25519.Signing.PrivateKey(rawRepresentation: authoritySeed)
    var result = field(Data("ssh-ed25519-cert-v01@openssh.com".utf8))
    result.append(field(Data(repeating: 19, count: 32)))
    result.append(field(subject.publicKey.rawRepresentation))
    result.appendBE(serial, bytes: 8); result.appendBE(1, bytes: 4)
    result.append(field(Data("workspace-fixture".utf8)))
    result.append(field(principals.reduce(into: Data()) { $0.append(field(Data($1.utf8))) }))
    result.appendBE(validAfter, bytes: 8); result.appendBE(validBefore, bytes: 8)
    result.append(field(criticalOptions.keys.sorted().reduce(into: Data()) {
        $0.append(field(Data($1.utf8))); $0.append(field(criticalOptions[$1]!))
    }))
    var permissions = Dictionary(uniqueKeysWithValues: extensions.map { ($0, Data()) })
    permissions.merge(extensionValues) { _, value in value }
    result.append(field(permissions.keys.sorted().reduce(into: Data()) {
        $0.append(field(Data($1.utf8))); $0.append(field(permissions[$1]!))
    }))
    result.append(field(Data()))
    result.append(field(publicBlob(authority.publicKey.rawRepresentation)))
    let signature = try authority.signature(for: result)
    result.append(field(field(Data("ssh-ed25519".utf8)) + field(signature)))
    return result
}

private final class WorkspaceCertificateTestClock: @unchecked Sendable {
    private let lock = NSLock()
    private var instant: Date
    init(_ seconds: TimeInterval) { instant = Date(timeIntervalSince1970: seconds) }
    var now: Date { lock.withLock { instant } }
    func set(_ seconds: TimeInterval) { lock.withLock { instant = Date(timeIntervalSince1970: seconds) } }
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

    private func certificateFile(_ blob: Data, comment: String = "public renewal") throws -> Data {
        let certificate = try SSHUserCertificate(blob: blob)
        return Data("\(certificate.algorithm) \(blob.base64EncodedString()) \(comment)\n".utf8)
    }

    func testPublicRenewalFileUsesOneExactSignedCertificateAndNeverItsComment() throws {
        let fixtures = try UserCertificateFixtures.load()
        for fixture in fixtures.certificates + [fixtures.largeCertificate] {
            let parsed = try WorkspaceCertificateRenewalOverlay.readPublicFile(Data((fixture.certificate + "\r\n").utf8))
            XCTAssertEqual(parsed.blob, fixture.certificateBlob)
        }
        let valid = try certificateFile(workspaceTestCertificate())
        let text = try XCTUnwrap(String(data: valid, encoding: .utf8))
        for invalid in [Data(), Data(repeating: 65, count: 65 * 1024), valid + valid,
                        Data(text.replacingOccurrences(of: "ssh-ed25519-cert-v01@openssh.com", with: "ssh-rsa-cert-v01@openssh.com").utf8),
                        Data("command=\"touch forbidden\" ".utf8) + valid, valid + Data([0]),
                        Data("ssh-ed25519 AAAA raw-key\n".utf8)] {
            XCTAssertThrowsError(try WorkspaceCertificateRenewalOverlay.readPublicFile(invalid))
        }
    }

    func testPhoneCertificateRenewalRepairsExpiredAuthenticationWithoutReimportingThePrivateKey() async throws {
        let old = try workspaceTestCertificate(validBefore: 1_577_836_800)
        let renewed = try workspaceTestCertificate(serial: 2), original = try document([("key", old)])
        let disk = StoreTestPersistence(), vault = StoreTestVault(), (key, _) = try sampleKey(using: crypto)
        let store = WorkspaceStore(persistence: disk, vault: vault)
        let saved = try await store.install(AirGapArchive(workspace: original, keys: [key], using: crypto),
            envelopeDigest: Data(repeating: 31, count: 32), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let before = try await store.routeReadiness(workspaceID: saved.id, support: support)
        XCTAssertEqual(before[0].issues, [.credentialCertificateUnavailable])
        let review = try await store.reviewCertificateRenewal(workspaceID: saved.id, expectedRevision: saved.revision,
            credentialID: "key", certificateFile: certificateFile(renewed, comment: "never saved as a trusted label"))
        XCTAssertEqual(review.currentCertificate.blob, old)
        XCTAssertEqual(review.renewedCertificate.blob, renewed)
        XCTAssertEqual(review.affectedRouteIDs, ["route"])
        let installed = try await store.applyCertificateRenewal(review, confirmedByUser: true)
        XCTAssertEqual(installed.id, saved.id); XCTAssertNotEqual(installed.revision, saved.revision)
        XCTAssertEqual(installed.manifest, saved.manifest); XCTAssertEqual(installed.envelopeDigest, saved.envelopeDigest)
        XCTAssertEqual(installed.bindings, saved.bindings); XCTAssertEqual(vault.creates, 1)
        XCTAssertTrue(installed.connectLater); XCTAssertEqual(installed.certificateRenewals.count, 1)
        XCTAssertEqual(try installed.importedDocument().credentials[0].userCertificate?.blob, old)
        XCTAssertEqual(try installed.document().credentials[0].userCertificate?.blob, renewed)
        XCTAssertFalse(String(data: try XCTUnwrap(disk.bytes(.catalog)), encoding: .utf8)!.contains("never saved as a trusted label"))
        let after = try await store.routeReadiness(workspaceID: saved.id, support: support)
        XCTAssertTrue(after[0].isLocallyReady)
        let host = try XCTUnwrap(original.hostTrust[0].pins.first)
        let authorization = try await store.authorizeSSHAuthentication(workspaceID: installed.id, expectedRevision: installed.revision,
            routeID: "route", credentialID: "key", presentedHostKey: host, support: support)
        let offered = try XCTUnwrap(authorization.userCertificate)
        XCTAssertEqual(offered.blob, renewed)
        let challenge = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(0..<32), user: "operator", publicBlob: offered.blob)
        let signature = try SSHUserAuthenticationSigner.sign(challenge, privateContainer: key.container)
        var reader = ByteCursor(signature)
        XCTAssertEqual(try reader.sshString(), Data("ssh-ed25519".utf8))
        let raw = try reader.sshString(), publicKey = try Curve25519.Signing.PublicKey(rawRepresentation:
            Curve25519.Signing.PrivateKey(rawRepresentation: Data(0..<32)).publicKey.rawRepresentation)
        XCTAssertTrue(publicKey.isValidSignature(raw, for: challenge.signedData))
        let reopened = try await WorkspaceStore(persistence: disk, vault: vault).load()
        XCTAssertEqual(try reopened[0].document().credentials[0].userCertificate?.blob, renewed)
    }

    func testRenewalCannotChangeSubjectIssuerPrincipalsRestrictionsOrExtensionPayloads() async throws {
        let old = try workspaceTestCertificate(validBefore: 2_000_000_000)
        let original = try document([("key", old)]), disk = StoreTestPersistence()
        let store = WorkspaceStore(persistence: disk, vault: StoreTestVault())
        let saved = try await store.install(AirGapArchive(workspace: original, keys: [], using: crypto),
            envelopeDigest: Data(repeating: 32, count: 32), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let invalid: [(Data, WorkspaceCertificateRenewalError)] = [
            (try workspaceTestCertificate(subjectSeed: Data(repeating: 8, count: 32)), .subjectChanged),
            (try workspaceTestCertificate(authoritySeed: Data(repeating: 9, count: 32)), .issuerChanged),
            (try workspaceTestCertificate(principals: []), .policyChanged),
            (try workspaceTestCertificate(principals: ["operator", "root"]), .policyChanged),
            (try workspaceTestCertificate(extensions: ["permit-pty"]), .policyChanged),
            (try workspaceTestCertificate(criticalOptions: ["force-command": SSHUserAuthenticationChallenge.string(Data("ls".utf8))]), .policyChanged),
            (try workspaceTestCertificate(extensionValues: ["future@example.com": Data([1])]), .policyChanged),
            (try workspaceTestCertificate(validAfter: 946_684_799), .notNewer),
            (try workspaceTestCertificate(serial: 2, validBefore: 2_000_000_000), .notNewer)
        ]
        let before = disk.bytes(.catalog)
        for (blob, expected) in invalid {
            do {
                _ = try await store.reviewCertificateRenewal(workspaceID: saved.id, expectedRevision: saved.revision,
                    credentialID: "key", certificateFile: certificateFile(blob))
                XCTFail("renewal changed protected certificate policy")
            } catch { XCTAssertEqual(error as? WorkspaceCertificateRenewalError, expected) }
            XCTAssertEqual(disk.bytes(.catalog), before)
        }
    }

    func testOnlyTheReviewedCertificateAliasRenewsAndOldAuthorizationIsRetired() async throws {
        let old = try workspaceTestCertificate(validBefore: 2_000_000_000), renewed = try workspaceTestCertificate(serial: 2)
        let original = try document([("key", old), ("alias", old)])
        let (key, _) = try sampleKey(using: crypto), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: StoreTestPersistence(), vault: vault)
        let saved = try await store.install(AirGapArchive(workspace: original, keys: [key], using: crypto),
            envelopeDigest: Data(repeating: 33, count: 32), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let host = try XCTUnwrap(original.hostTrust[0].pins.first)
        let oldAuthorization = try await store.authorizeSSHAuthentication(workspaceID: saved.id, expectedRevision: saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: host, support: support)
        let review = try await store.reviewCertificateRenewal(workspaceID: saved.id, expectedRevision: saved.revision,
            credentialID: "key", certificateFile: certificateFile(renewed))
        let installed = try await store.applyCertificateRenewal(review, confirmedByUser: true)
        let certificates = try installed.document().credentials.compactMap(\.userCertificate).map(\.blob)
        XCTAssertEqual(certificates, [renewed, old]); XCTAssertEqual(vault.creates, 1)
        do { try await store.validateSSHAuthentication(oldAuthorization, support: support); XCTFail("old revision survived renewal") }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
        do { _ = try await store.applyCertificateRenewal(review, confirmedByUser: true); XCTFail("review applied twice") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
    }

    func testRenewalRetainsLargeSignedExtensionPayloadAndExactPrincipalBytes() async throws {
        let payload = Data(repeating: 23, count: 8 * 1024)
        let old = try workspaceTestCertificate(principals: ["operator", "caf\u{00e9}"], validBefore: 2_000_000_000,
            extensionValues: ["future@example.com": payload])
        let renewed = try workspaceTestCertificate(serial: 2, principals: ["caf\u{00e9}", "operator"],
            extensionValues: ["future@example.com": payload])
        XCTAssertGreaterThan(old.base64EncodedString().utf8.count, FTLimit.text)
        let original = try document([("key", old)]), disk = StoreTestPersistence()
        let store = WorkspaceStore(persistence: disk, vault: StoreTestVault())
        let saved = try await store.install(AirGapArchive(workspace: original, keys: [], using: crypto),
            envelopeDigest: Data(repeating: 34, count: 32), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        for candidate in [
            try workspaceTestCertificate(principals: ["operator", "cafe\u{0301}"], extensionValues: ["future@example.com": payload]),
            try workspaceTestCertificate(principals: ["operator", "caf\u{00e9}"], extensionValues: ["future@example.com": Data(repeating: 24, count: 8 * 1024)])
        ] {
            do {
                _ = try await store.reviewCertificateRenewal(workspaceID: saved.id, expectedRevision: saved.revision,
                    credentialID: "key", certificateFile: certificateFile(candidate))
                XCTFail("renewal changed exact signed policy bytes")
            } catch { XCTAssertEqual(error as? WorkspaceCertificateRenewalError, .policyChanged) }
        }
        let review = try await store.reviewCertificateRenewal(workspaceID: saved.id, expectedRevision: saved.revision,
            credentialID: "key", certificateFile: certificateFile(renewed))
        let installed = try await store.applyCertificateRenewal(review, confirmedByUser: true)
        XCTAssertEqual(try installed.document().credentials[0].userCertificate?.extensionOptions["future@example.com"], payload)
        let reopened = try await WorkspaceStore(persistence: disk, vault: StoreTestVault()).load()
        XCTAssertEqual(try reopened[0].document().credentials[0].userCertificate?.blob, renewed)
    }

    func testRenewalRequiresCurrentOwnerReviewExplicitConfirmationAndUncancelledApply() async throws {
        let old = try workspaceTestCertificate(validBefore: 2_000_000_000), original = try document([("key", old)])
        let disk = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: disk, vault: vault)
        let saved = try await store.install(AirGapArchive(workspace: original, keys: [], using: crypto),
            envelopeDigest: Data(repeating: 35, count: 32), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let review = try await store.reviewCertificateRenewal(workspaceID: saved.id, expectedRevision: saved.revision,
            credentialID: "key", certificateFile: certificateFile(workspaceTestCertificate(serial: 2)))
        let competing = try await store.reviewCertificateRenewal(workspaceID: saved.id, expectedRevision: saved.revision,
            credentialID: "key", certificateFile: certificateFile(workspaceTestCertificate(serial: 3, validBefore: 4_102_444_801)))
        let before = disk.bytes(.catalog)
        do { _ = try await store.applyCertificateRenewal(review, confirmedByUser: false); XCTFail("confirmation absent") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .reviewRequired) }
        do {
            _ = try await WorkspaceStore(persistence: disk, vault: vault).applyCertificateRenewal(review, confirmedByUser: true)
            XCTFail("another store reused the review")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
        let expired = WorkspaceCertificateRenewalReview(id: review.id, workspaceID: review.workspaceID,
            expectedRevision: review.expectedRevision, workspaceName: review.workspaceName, credentialID: review.credentialID,
            currentCertificate: review.currentCertificate, renewedCertificate: review.renewedCertificate,
            affectedRouteIDs: review.affectedRouteIDs, sourceWitness: review.sourceWitness, owner: review.owner,
            expires: ContinuousClock.now - .seconds(1))
        do { _ = try await store.applyCertificateRenewal(expired, confirmedByUser: true); XCTFail("expired review applied") }
        catch { XCTAssertEqual(error as? WorkspaceCertificateRenewalError, .expiredReview) }
        let cancelled = Task { try await store.applyCertificateRenewal(review, confirmedByUser: true) }
        cancelled.cancel()
        do { _ = try await cancelled.value; XCTFail("cancelled renewal published") }
        catch { XCTAssertTrue(error is CancellationError) }
        XCTAssertEqual(disk.bytes(.catalog), before)
        _ = try await store.applyCertificateRenewal(review, confirmedByUser: true)
        do { _ = try await store.applyCertificateRenewal(competing, confirmedByUser: true); XCTFail("competing review replaced newer state") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
    }

    func testRenewalRechecksCertificateClockAndRawSubjectRevocationAtPublication() async throws {
        let clock = WorkspaceCertificateTestClock(150), old = try workspaceTestCertificate(validAfter: 100, validBefore: 125)
        let renewed = try workspaceTestCertificate(serial: 2, validAfter: 125, validBefore: 200)
        let original = try document([("key", old)]), disk = StoreTestPersistence()
        let store = WorkspaceStore(persistence: disk, vault: StoreTestVault(), certificateRenewalNow: { clock.now })
        let saved = try await store.install(AirGapArchive(workspace: original, keys: [], using: crypto),
            envelopeDigest: Data(repeating: 36, count: 32), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let review = try await store.reviewCertificateRenewal(workspaceID: saved.id, expectedRevision: saved.revision,
            credentialID: "key", certificateFile: certificateFile(renewed))
        let before = disk.bytes(.catalog)
        clock.set(200)
        do { _ = try await store.applyCertificateRenewal(review, confirmedByUser: true); XCTFail("expired certificate published") }
        catch { XCTAssertEqual(error as? FTError, .unsupported("certificate.validity")) }
        clock.set(124)
        do {
            _ = try await store.reviewCertificateRenewal(workspaceID: saved.id, expectedRevision: saved.revision,
                credentialID: "key", certificateFile: certificateFile(renewed))
            XCTFail("future certificate reviewed")
        } catch { XCTAssertEqual(error as? FTError, .unsupported("certificate.validity")) }
        XCTAssertEqual(disk.bytes(.catalog), before)
        clock.set(150)
        try await store.revokeLocally(fingerprint: original.credentials[0].fingerprint)
        let revoked = disk.bytes(.catalog)
        do { _ = try await store.applyCertificateRenewal(review, confirmedByUser: true); XCTFail("revoked subject renewed") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .revokedIdentity) }
        do {
            _ = try await store.reviewCertificateRenewal(workspaceID: saved.id, expectedRevision: saved.revision,
                credentialID: "key", certificateFile: certificateFile(renewed))
            XCTFail("revoked subject reviewed")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .revokedIdentity) }
        XCTAssertEqual(disk.bytes(.catalog), revoked)
    }

    func testRenewalPublicationFailureReportsNoSuccessAndPreservesProtectedIdentity() async throws {
        for (fault, published) in [(StoreTestPersistence.Fault.before(.catalog), false), (.after(.catalog), true)] {
            let old = try workspaceTestCertificate(validBefore: 2_000_000_000), renewed = try workspaceTestCertificate(serial: 2)
            let original = try document([("key", old)]), (key, _) = try sampleKey(using: crypto)
            let disk = StoreTestPersistence(), vault = StoreTestVault(), store = WorkspaceStore(persistence: disk, vault: vault)
            let saved = try await store.install(AirGapArchive(workspace: original, keys: [key], using: crypto),
                envelopeDigest: Data(repeating: 37, count: 32), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
            let approved = try await store.approveConnections(workspaceID: saved.id, expectedRevision: saved.revision, confirmedByUser: true)
            let review = try await store.reviewCertificateRenewal(workspaceID: saved.id, expectedRevision: saved.revision,
                credentialID: "key", certificateFile: certificateFile(renewed))
            disk.configure(fault)
            do { _ = try await store.applyCertificateRenewal(review, confirmedByUser: true); XCTFail("write failure reported success") }
            catch { XCTAssertEqual(error as? FTError, .storage) }
            let reopened = try await WorkspaceStore(persistence: disk, vault: vault).load()
            let current = try XCTUnwrap(reopened.first)
            XCTAssertEqual(current.certificateRenewals.count, published ? 1 : 0)
            XCTAssertEqual(current.revision == approved.revision, !published)
            XCTAssertEqual(current.connectLater, published)
            XCTAssertEqual(try current.document().credentials[0].userCertificate?.blob, published ? renewed : old)
            XCTAssertEqual(current.manifest, saved.manifest); XCTAssertEqual(current.bindings, saved.bindings)
            XCTAssertEqual(vault.count, 1); XCTAssertEqual(vault.creates, 1); XCTAssertNil(disk.bytes(.journal))
        }
    }

    func testRenewalSurvivesCosmeticImportButSourcePolicyChangePermanentlyRemovesIt() async throws {
        let old = try workspaceTestCertificate(validBefore: 1_577_836_800), renewed = try workspaceTestCertificate(serial: 2)
        let original = try document([("key", old)]), (key, _) = try sampleKey(using: crypto)
        let disk = StoreTestPersistence(), vault = StoreTestVault(), store = WorkspaceStore(persistence: disk, vault: vault)
        let saved = try await store.install(AirGapArchive(workspace: original, keys: [key], using: crypto),
            envelopeDigest: Data(repeating: 38, count: 32), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let review = try await store.reviewCertificateRenewal(workspaceID: saved.id, expectedRevision: saved.revision,
            credentialID: "key", certificateFile: certificateFile(renewed))
        let installed = try await store.applyCertificateRenewal(review, confirmedByUser: true)
        let cosmetic = try document([("key", old)], bundleID: "cosmetic") { $0["name"] = "New display name" }
        let cosmeticArchive = try AirGapArchive(workspace: cosmetic, keys: [], using: crypto)
        let preview = try await store.previewImport(cosmeticArchive, envelopeDigest: Data(repeating: 39, count: 32), support: support)
        XCTAssertTrue(preview.routes[0].isLocallyReady)
        let retained = try await store.install(cosmeticArchive, envelopeDigest: Data(repeating: 39, count: 32),
            expectedRevision: installed.revision, sourceAndContentsConfirmed: true, associateWithExistingWorkspace: true).workspace
        XCTAssertEqual(retained.certificateRenewals, installed.certificateRenewals)
        XCTAssertEqual(try retained.document().credentials[0].userCertificate?.blob, renewed)
        let changed = try document([("key", old)], bundleID: "changed-account") { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["user"] = "other"; workspace["routes"] = routes
        }
        let changedArchive = try AirGapArchive(workspace: changed, keys: [], using: crypto)
        let changedPreview = try await store.previewImport(changedArchive, envelopeDigest: Data(repeating: 40, count: 32), support: support)
        XCTAssertEqual(changedPreview.routes[0].issues, [.credentialCertificateUnavailable])
        let invalidated = try await store.install(changedArchive, envelopeDigest: Data(repeating: 40, count: 32),
            expectedRevision: retained.revision, sourceAndContentsConfirmed: true,
            securityChangesConfirmed: true, associateWithExistingWorkspace: true).workspace
        XCTAssertTrue(invalidated.certificateRenewals.isEmpty)
        XCTAssertEqual(try invalidated.document().credentials[0].userCertificate?.blob, old)
        let reverted = try document([("key", old)], bundleID: "reintroduced-account")
        let returned = try await store.install(AirGapArchive(workspace: reverted, keys: [], using: crypto),
            envelopeDigest: Data(repeating: 41, count: 32), expectedRevision: invalidated.revision,
            sourceAndContentsConfirmed: true, securityChangesConfirmed: true, associateWithExistingWorkspace: true).workspace
        XCTAssertTrue(returned.certificateRenewals.isEmpty)
        XCTAssertEqual(try returned.document().credentials[0].userCertificate?.blob, old)
        XCTAssertEqual(returned.bindings, saved.bindings); XCTAssertEqual(vault.creates, 1)
    }

    func testRenewalForPureTransitAccountDoesNotInventARequiredPTY() async throws {
        let old = try workspaceTestCertificate(validBefore: 2_000_000_000, extensions: ["permit-port-forwarding"])
        let renewed = try workspaceTestCertificate(serial: 2, extensions: ["permit-port-forwarding"])
        let original = try document([("key", workspaceTestCertificate()), ("hop-key", old)]) { workspace in
            var routes = workspace["routes"] as! [[String: Any]], jump = routes[0]
            jump["id"] = "jump"; jump["host"] = "jump.example.invalid"; jump["host_trust_ref"] = "jump-trust"
            jump["credential_refs"] = ["hop-key"]; jump.removeValue(forKey: "session_creation")
            routes[0]["credential_refs"] = ["key"]; routes[0]["jump_routes"] = ["jump"]
            routes.append(jump); workspace["routes"] = routes
            var trust = workspace["host_trust"] as! [[String: Any]], jumpTrust = trust[0]
            jumpTrust["id"] = "jump-trust"; jumpTrust["host"] = "jump.example.invalid"
            trust.append(jumpTrust); workspace["host_trust"] = trust
        }
        let (key, _) = try sampleKey(using: crypto), store = WorkspaceStore(persistence: StoreTestPersistence(), vault: StoreTestVault())
        let saved = try await store.install(AirGapArchive(workspace: original, keys: [key], using: crypto),
            envelopeDigest: Data(repeating: 42, count: 32), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let review = try await store.reviewCertificateRenewal(workspaceID: saved.id, expectedRevision: saved.revision,
            credentialID: "hop-key", certificateFile: certificateFile(renewed))
        XCTAssertEqual(review.affectedRouteIDs, ["jump"])
        let installed = try await store.applyCertificateRenewal(review, confirmedByUser: true)
        XCTAssertTrue(try readiness(installed.document()).isLocallyReady)
        XCTAssertEqual(try installed.document().credentials.first { $0.id == "hop-key" }?.userCertificate?.blob, renewed)
    }

    func testVersionEightMigrationDoesNotInventRenewalsAndMalformedOverlaysFailClosed() async throws {
        let old = try workspaceTestCertificate(validBefore: 2_000_000_000), original = try document([("key", old)])
        let disk = StoreTestPersistence(), vault = StoreTestVault(), store = WorkspaceStore(persistence: disk, vault: vault)
        let saved = try await store.install(AirGapArchive(workspace: original, keys: [], using: crypto),
            envelopeDigest: Data(repeating: 43, count: 32), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        var legacy = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(disk.bytes(.catalog))) as? [String: Any])
        var workspaces = try XCTUnwrap(legacy["workspaces"] as? [[String: Any]])
        legacy["version"] = 8; workspaces[0].removeValue(forKey: "certificateRenewals"); legacy["workspaces"] = workspaces
        let history = try JSONSerialization.data(withJSONObject: XCTUnwrap(legacy["acceptedImports"]), options: [.sortedKeys])
        disk.seed(try JSONSerialization.data(withJSONObject: legacy, options: [.sortedKeys]), slot: .catalog)
        let loaded = try await WorkspaceStore(persistence: disk, vault: vault).load(), migrated = try XCTUnwrap(loaded.first)
        XCTAssertTrue(migrated.certificateRenewals.isEmpty); XCTAssertEqual(migrated.revision, saved.revision)
        XCTAssertEqual(migrated.manifest, saved.manifest); XCTAssertEqual(migrated.bindings, saved.bindings)
        let upgraded = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(disk.bytes(.catalog))) as? [String: Any])
        XCTAssertEqual(upgraded["version"] as? Int, 9)
        XCTAssertEqual(try JSONSerialization.data(withJSONObject: XCTUnwrap(upgraded["acceptedImports"]), options: [.sortedKeys]), history)
        let review = try await store.reviewCertificateRenewal(workspaceID: saved.id, expectedRevision: saved.revision,
            credentialID: "key", certificateFile: certificateFile(workspaceTestCertificate(serial: 2)))
        _ = try await store.applyCertificateRenewal(review, confirmedByUser: true)
        let valid = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(disk.bytes(.catalog))) as? [String: Any])
        for kind in ["duplicate", "witness", "signature", "legacy-version"] {
            var catalog = valid, workspaces = try XCTUnwrap(valid["workspaces"] as? [[String: Any]])
            var entries = try XCTUnwrap(workspaces[0]["certificateRenewals"] as? [[String: Any]])
            switch kind {
            case "duplicate": entries.append(entries[0])
            case "witness": entries[0]["sourceWitness"] = Data(repeating: 0, count: 32).base64EncodedString()
            case "signature": entries[0]["certificate"] = Data([1, 2, 3]).base64EncodedString()
            default: catalog["version"] = 8
            }
            workspaces[0]["certificateRenewals"] = entries; catalog["workspaces"] = workspaces
            disk.seed(try JSONSerialization.data(withJSONObject: catalog, options: [.sortedKeys]), slot: .catalog)
            do { _ = try await WorkspaceStore(persistence: disk, vault: vault).load(); XCTFail("malformed renewal overlay loaded: \(kind)") }
            catch { XCTAssertEqual(error as? WorkspaceStoreError, .corruptStore) }
        }
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
