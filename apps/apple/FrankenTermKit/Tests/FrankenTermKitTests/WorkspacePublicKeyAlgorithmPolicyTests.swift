import Foundation
import XCTest
@testable import FrankenTermKit

private actor PublicKeyPolicySigner: WorkspaceSSHSigning {
    let container: Data
    private(set) var signatureCount = 0
    init(container: Data) { self.container = container }
    nonisolated func cancel(requestID: UUID) {}
    func publicIdentity(requestID: UUID, authorization: WorkspaceSSHAuthorization) async throws -> Data {
        try SSHUserAuthenticationSigner.publicIdentity(privateContainer: container).blob
    }
    func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
              challenge: SSHUserAuthenticationChallenge) async throws -> Data {
        signatureCount += 1
        return try SSHUserAuthenticationSigner.sign(challenge, privateContainer: container)
    }
}

@MainActor
final class WorkspacePublicKeyAlgorithmPolicyTests: XCTestCase {
    private let crypto = TestCryptography()
    private let signatures: Set<String> = ["ssh-ed25519", "rsa-sha2-512", "rsa-sha2-256",
        "ssh-ed25519-cert-v01@openssh.com", "rsa-sha2-512-cert-v01@openssh.com",
        "rsa-sha2-256-cert-v01@openssh.com"]

    private func profile(capabilities: Set<String>? = nil, signatures: Set<String>? = nil) -> WorkspaceDocument.RouteSupport {
        .init(routeModes: ["direct_ssh", "frankenterm_mux"],
            authenticationMethods: [.publicKey, .password, .keyboardInteractive],
            credentialAlgorithms: ["ssh-ed25519", "ssh-rsa"], hostKeyAlgorithms: ["ssh-ed25519"],
            capabilities: capabilities ?? [WorkspaceDocument.publicKeyAlgorithmPolicyCapability,
                WorkspaceDocument.userCertificateCapability],
            maximumJumpHops: 3, publicKeySignatureAlgorithms: signatures ?? self.signatures)
    }

    private func document(_ algorithms: [String]?, key: PrivateKeyRecord? = nil,
                          certificate: Data? = nil, bundle: String = "public-key-policy",
                          mutate: (inout [String: Any]) -> Void = { _ in }) throws -> WorkspaceDocument {
        let (sample, fingerprint) = try sampleKey(using: crypto)
        let selected = key ?? sample
        let identity = try SSHPublicIdentity(blob: selected.publicBlob)
        var root = try XCTUnwrap(JSONSerialization.jsonObject(
            with: sampleManifest(fingerprint: fingerprint, bundleID: bundle)) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        var credentials = try XCTUnwrap(workspace["credentials"] as? [[String: Any]])
        credentials[0]["algorithm"] = identity.algorithm
        credentials[0]["public_fingerprint"] = identity.fingerprint
        if let certificate {
            credentials[0]["user_certificate"] = certificate.base64EncodedString()
            credentials[0]["required_capabilities"] = [WorkspaceDocument.userCertificateCapability]
        }
        workspace["credentials"] = credentials
        var routes = try XCTUnwrap(workspace["routes"] as? [[String: Any]])
        routes[0]["pubkey_accepted_algorithms"] = algorithms
        // Omit required_capabilities deliberately: the typed field must require
        // enforcement even when the source forgot its capability advertisement.
        workspace["routes"] = routes
        mutate(&workspace); root["workspace"] = workspace
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }

    private func readiness(_ document: WorkspaceDocument, support: WorkspaceDocument.RouteSupport? = nil) throws -> WorkspaceDocument.RouteReadiness {
        try document.readiness(forRouteID: "route", availableCredentialIDs: Set(document.credentials.map(\.id)),
            reviewedTrustIDs: Set(document.hostTrust.map(\.id)), support: support ?? profile())
    }

    private func installed(_ document: WorkspaceDocument, key: PrivateKeyRecord) async throws -> (WorkspaceStore, InstalledWorkspace) {
        let store = WorkspaceStore(persistence: StoreTestPersistence(), vault: StoreTestVault())
        let archive = try AirGapArchive(workspace: document, keys: [key], using: crypto)
        let saved = try await store.install(archive, envelopeDigest: Data(repeating: 1, count: 32),
            transferID: Data(repeating: 1, count: 16), expectedRevision: nil,
            sourceAndContentsConfirmed: true).workspace
        return (store, saved)
    }

    func testAbsentEmptyUnknownAndBoundedPoliciesRemainDistinct() throws {
        let unconstrained = try document(nil)
        XCTAssertFalse(unconstrained.routes[0].publicKeyAlgorithmPolicy.isConstrained)
        XCTAssertTrue(try readiness(unconstrained, support: profile(capabilities: [], signatures: [])).isLocallyReady)
        for list in [[], ["unknown-key@example.invalid"], ["rsa-sha2-256"]] {
            let restricted = try document(list)
            XCTAssertEqual(restricted.routes[0].publicKeyAlgorithmPolicy.acceptedAlgorithms, list)
            XCTAssertTrue(restricted.routes[0].requiredCapabilities.contains(WorkspaceDocument.publicKeyAlgorithmPolicyCapability))
            let result = try readiness(restricted)
            XCTAssertEqual(result.usableCredentialIDs, [])
            XCTAssertTrue(result.issues.contains(.credentialAlgorithmUnavailable))
        }
        XCTAssertThrowsError(try document(["ssh-ed25519", "ssh-ed25519"]))
        XCTAssertThrowsError(try document(["ssh-ed25519,rsa-sha2-256"]))
        XCTAssertThrowsError(try document((0..<65).map { "algorithm-\($0)" }))
        let allowed = try document(["ssh-ed25519"])
        XCTAssertTrue(try readiness(allowed).isLocallyReady)
        XCTAssertTrue(try readiness(allowed, support: profile(capabilities: [])).issues.contains(.unsupportedRequirement))
        XCTAssertTrue(try readiness(allowed, support: profile(signatures: [])).issues.contains(.credentialAlgorithmUnavailable))
    }

    func testExplicitPasswordRemainsUsableWhenPublicKeyPolicyDeniesEveryKey() throws {
        let restricted = try document([]) { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["authentication_methods"] = ["publickey", "password"]
            workspace["routes"] = routes
        }
        let result = try readiness(restricted)
        XCTAssertTrue(result.isLocallyReady)
        XCTAssertEqual(result.usableCredentialIDs, [])
        // An older adapter cannot establish that it enforces the empty list;
        // capability admission remains mandatory for the complete route.
        XCTAssertTrue(try readiness(restricted, support: profile(capabilities: [])).issues.contains(.unsupportedRequirement))
        let detail = try restricted.readiness(for: "route", availableCredentialIDs: ["key"],
            acceptedTrustIDs: Set(restricted.hostTrust.map(\.id)),
            supportedAuthenticationMethods: [.publicKey, .password], supportedCredentialAlgorithms: ["ssh-ed25519"],
            supportedHostKeyAlgorithms: ["ssh-ed25519"],
            supportedCapabilities: [WorkspaceDocument.publicKeyAlgorithmPolicyCapability],
            supportedPublicKeySignatureAlgorithms: signatures)
        XCTAssertEqual(detail.authenticationMethods, [.password])
        XCTAssertEqual(detail.usableCredentialIDs, [])
    }

    func testEveryHopKeepsItsOwnPolicyAndCredentialFamily() throws {
        let fixture = try XCTUnwrap(RSAFixtures.load().keys.first)
        let rsa = try PrivateKeyRecord.fromPEM(Data(fixture.clearPEM.utf8), credentialID: "key", using: crypto)
        let (_, edFingerprint) = try sampleKey(using: crypto)
        let routed = try document(["rsa-sha2-256"], key: rsa) { workspace in
            var credentials = workspace["credentials"] as! [[String: Any]]
            var hopKey = credentials[0]
            hopKey["id"] = "hop-key"; hopKey["algorithm"] = "ssh-ed25519"
            hopKey["public_fingerprint"] = edFingerprint
            credentials.append(hopKey); workspace["credentials"] = credentials
            var routes = workspace["routes"] as! [[String: Any]]
            var hop = routes[0]
            hop["id"] = "jump"; hop["credential_refs"] = ["hop-key"]
            hop["pubkey_accepted_algorithms"] = ["ssh-ed25519"]
            routes[0]["jump_routes"] = ["jump"]
            routes.append(hop); workspace["routes"] = routes
        }
        XCTAssertTrue(try readiness(routed).isLocallyReady)
        let hop = try routed.readiness(forRouteID: "jump", availableCredentialIDs: ["key", "hop-key"],
            reviewedTrustIDs: Set(routed.hostTrust.map(\.id)), support: profile(), transitForRouteID: "route")
        XCTAssertEqual(hop.usableCredentialIDs, ["hop-key"])
        let blocked = try readiness(routed, support: profile(signatures: ["rsa-sha2-256"]))
        XCTAssertTrue(blocked.issues.contains(.blockedJumpRoute("jump")))
    }

    func testRSAPolicyRejectsWrongHashBeforeSignerAndSignsAllowedSHA256() async throws {
        let fixture = try XCTUnwrap(RSAFixtures.load().keys.first)
        let key = try PrivateKeyRecord.fromPEM(Data(fixture.clearPEM.utf8), credentialID: "key", using: crypto)
        let document = try document(["rsa-sha2-256"], key: key)
        let (store, saved) = try await installed(document, key: key)
        let signer = PublicKeyPolicySigner(container: key.container)
        let broker = SSHAuthenticationBroker(store: store, signer: signer, support: profile())
        await broker.resume()
        let pin = try XCTUnwrap(document.hostTrust[0].pins.first)
        do {
            _ = try await broker.prepare(workspaceID: saved.id, expectedRevision: saved.revision,
                routeID: "route", credentialID: "key", presentedHostKey: pin, transportID: UUID(),
                sessionIdentifier: Data(0..<32), publicBlob: key.publicBlob, signatureAlgorithm: "rsa-sha2-512")
            XCTFail("A denied hash must never reach the protected signer")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
        let before = await signer.signatureCount
        XCTAssertEqual(before, 0)
        let request = try await broker.prepare(workspaceID: saved.id, expectedRevision: saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: pin, transportID: UUID(),
            sessionIdentifier: Data(0..<32), publicBlob: key.publicBlob, signatureAlgorithm: "rsa-sha2-256")
        XCTAssertEqual(request.authorization.publicKeyAlgorithmPolicy.acceptedAlgorithms, ["rsa-sha2-256"])
        let signed = try await broker.confirm(requestID: request.id)
        XCTAssertEqual(signed.algorithm, "rsa-sha2-256")
        var bytes = ByteCursor(signed.signatureBlob)
        XCTAssertEqual(try bytes.sshString(), Data("rsa-sha2-256".utf8))
        XCTAssertEqual(try bytes.sshString().count, 256)
        XCTAssertEqual(bytes.remaining, 0)
        let after = await signer.signatureCount
        XCTAssertEqual(after, 1)
    }

    func testCertificatePolicyUsesRequestNameAndRetainsRawSignatureEnvelope() async throws {
        let (key, _) = try sampleKey(using: crypto)
        let certificate = try workspaceTestCertificate()
        let requestName = "ssh-ed25519-cert-v01@openssh.com"
        let document = try document([requestName], certificate: certificate)
        let rawOnly = try self.document(["ssh-ed25519"], certificate: certificate)
        XCTAssertTrue(try readiness(rawOnly).issues.contains(.credentialAlgorithmUnavailable))
        let (store, saved) = try await installed(document, key: key)
        let signer = PublicKeyPolicySigner(container: key.container)
        let broker = SSHAuthenticationBroker(store: store, signer: signer, support: profile())
        await broker.resume()
        let request = try await broker.prepare(workspaceID: saved.id, expectedRevision: saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: XCTUnwrap(document.hostTrust[0].pins.first),
            transportID: UUID(), sessionIdentifier: Data(0..<32), publicBlob: certificate, signatureAlgorithm: requestName)
        let signed = try await broker.confirm(requestID: request.id)
        XCTAssertEqual(signed.algorithm, "ssh-ed25519")
        XCTAssertEqual(signed.publicBlob, certificate)
        let rsaPolicy = try WorkspacePublicKeyAlgorithmPolicy(acceptedAlgorithms: ["rsa-sha2-256-cert-v01@openssh.com"])
        XCTAssertEqual(rsaPolicy.filterAlgorithms(forKeyAlgorithm: "ssh-rsa-cert-v01@openssh.com",
            qualified: Array(signatures)), ["rsa-sha2-256-cert-v01@openssh.com"])
        XCTAssertEqual(rsaPolicy.filterAlgorithms(forKeyAlgorithm: "ssh-rsa", qualified: Array(signatures)), [])
    }

    func testReplacementPolicyInvalidatesPendingSigningAndLocalKeyWitnesses() async throws {
        let (key, _) = try sampleKey(using: crypto)
        let initial = try document(["ssh-ed25519"])
        let replacement = try document(["ssh-ed25519", "rsa-sha2-256"], bundle: "changed-public-key-policy")
        let (store, saved) = try await installed(initial, key: key)
        let signer = PublicKeyPolicySigner(container: key.container)
        let broker = SSHAuthenticationBroker(store: store, signer: signer, support: profile())
        await broker.resume()
        let request = try await broker.prepare(workspaceID: saved.id, expectedRevision: saved.revision,
            routeID: "route", credentialID: "key", presentedHostKey: XCTUnwrap(initial.hostTrust[0].pins.first),
            transportID: UUID(), sessionIdentifier: Data(0..<32), publicBlob: key.publicBlob)
        let archive = try AirGapArchive(workspace: replacement, keys: [], using: crypto)
        _ = try await store.install(archive, envelopeDigest: Data(repeating: 2, count: 32),
            transferID: Data(repeating: 2, count: 16), expectedRevision: saved.revision,
            sourceAndContentsConfirmed: true, securityChangesConfirmed: true, associateWithExistingWorkspace: true)
        do {
            _ = try await broker.confirm(requestID: request.id)
            XCTFail("Replacing route algorithm policy must retire a pending authorization")
        } catch {}
        let count = await signer.signatureCount
        XCTAssertEqual(count, 0)
        XCTAssertNotEqual(try WorkspaceDeviceKeyOverlay.witness(routeID: "route", in: initial),
            try WorkspaceDeviceKeyOverlay.witness(routeID: "route", in: replacement))
        XCTAssertNotEqual(try WorkspaceDeviceKeyOverlay.certificateRenewalWitnesses(routeIDs: ["route"], in: initial),
            try WorkspaceDeviceKeyOverlay.certificateRenewalWitnesses(routeIDs: ["route"], in: replacement))
        XCTAssertTrue(WorkspaceDeviceKeyOverlay.canAssign(initial.routes[0], in: initial))
        for list in [[], ["rsa-sha2-256"], ["ssh-ed25519-cert-v01@openssh.com"]] {
            let denied = try document(list)
            XCTAssertFalse(WorkspaceDeviceKeyOverlay.canAssign(denied.routes[0], in: denied))
            XCTAssertThrowsError(try WorkspaceDeviceKeyOverlay.assignments(routeIDs: ["route"],
                credentialID: "phone-key-example", in: denied))
        }
    }
}
