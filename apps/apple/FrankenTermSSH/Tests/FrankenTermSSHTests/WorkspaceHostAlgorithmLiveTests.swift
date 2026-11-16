import Foundation
import XCTest
@testable import FrankenTermKit
@testable import FrankenTermSSH
@testable import FrankenTermSSHIntegration

private actor HostAlgorithmForbiddenPrompt: WorkspaceSSHPasswordPrompting, WorkspaceSSHKeyboardInteractivePrompting {
    private(set) var requests = 0

    func password(for request: WorkspaceSSHPasswordRequest) throws -> Data {
        requests += 1
        throw SSHAuthenticationError.denied
    }

    func responses(for request: WorkspaceSSHKeyboardInteractiveRequest) throws -> [Data] {
        requests += 1
        throw SSHAuthenticationError.denied
    }

    nonisolated func cancel(requestID: UUID) {}
}

@MainActor
final class WorkspaceHostAlgorithmTests: XCTestCase {
    func testMixedTrustDoesNotNegotiateCertificatesThatNoConfiguredAuthorityCanSign() async throws {
        XCTAssertTrue(NativeSSHSession.supportsHostCertificates)
        let original = try await SSHWorkspaceTestFixture.make()
        for mixed in [true, false] {
            let f = try await SSHWorkspaceTestFixture.make { workspace in
                var trusts = workspace["host_trust"] as! [[String: Any]]
                trusts[0]["certificate_authorities"] = [["algorithm": original.host.algorithm,
                    "fingerprint": original.host.fingerprint]]
                trusts[0]["required_capabilities"] = [WorkspaceDocument.hostCertificateCapability]
                if !mixed { trusts[0]["pins"] = [] as [[String: Any]] }
                workspace["host_trust"] = trusts
            }
            let document = f.archive.workspace, route = document.routes[0]
            for allowed: [String]? in [nil, [], ["rsa-sha2-512"], ["ssh-ed25519"]] {
                let policy = try WorkspaceHostAlgorithmPolicy(caSignatureAlgorithms: allowed)
                let canSign = allowed == nil || allowed == ["ssh-ed25519"]
                if !mixed && !canSign {
                    XCTAssertThrowsError(try WorkspaceNativeSSHAuthentication.hostKeyNegotiation(
                        for: route, document: document, algorithmPolicy: policy)) {
                        XCTAssertEqual($0 as? SSHAuthenticationError, .denied)
                    }
                    continue
                }
                let result = try WorkspaceNativeSSHAuthentication.hostKeyNegotiation(
                    for: route, document: document, algorithmPolicy: policy)
                let expected: NativeSSHHostKeyPolicy = !canSign ? .rawKeys
                    : mixed ? .certificatesAndRawKeys : .certificates
                XCTAssertEqual(result.profile, expected)
                XCTAssertEqual(result.algorithms, expected.algorithms)
            }
        }
    }
}

@MainActor
final class WorkspaceHostAlgorithmLiveTests: XCTestCase {
    private struct Hop: Decodable {
        let port: UInt16
        let hostKey: String
    }

    /// The Python harness supplies independent libssh/OpenSSH peers. Each
    /// prohibited exchange must finish before a username, method probe, key
    /// offer, password request or MFA prompt. Successful cases complete real
    /// signed authentication without opening a shell or terminal channel.
    func testLiveMandatoryAlgorithmsBeforeCredentials() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard let mode = environment["FT_HOST_ALGORITHM_MODE"],
              let port = environment["FT_HOST_ALGORITHM_PORT"].flatMap(UInt16.init),
              let hostBlob = environment["FT_HOST_ALGORITHM_HOSTKEY"].flatMap({ Data(base64Encoded: $0) }),
              let encodedHop = environment["FT_HOST_ALGORITHM_HOP"] else {
            throw XCTSkip("Run Tests/Interop/test_host_algorithms.py with the native adapter and this XCTest product.")
        }
        guard ["raw-256", "raw-512", "deny-256", "deny-512", "deny-password", "deny-interactive",
               "jump-allowed", "jump-denied", "ca-256", "ca-512", "probe-allowed", "probe-denied"].contains(mode) else {
            throw SSHAuthenticationError.invalidChallenge
        }
        XCTAssertTrue(NativeSSHSession.supportsHostAlgorithmPolicy)
        let hop = try JSONDecoder().decode(Hop?.self, from: Data(encodedHop.utf8))
        let hopIdentity = try hop.map { try SSHPublicIdentity(blob: XCTUnwrap(Data(base64Encoded: $0.hostKey))) }
        let certificate = try environment["FT_HOST_ALGORITHM_CERTIFICATE"].map {
            try SSHHostCertificate(blob: XCTUnwrap(Data(base64Encoded: $0)))
        }
        let subject = try SSHPublicIdentity(blob: hostBlob)
        let shouldConnect = ["raw-256", "raw-512", "jump-allowed", "ca-512"].contains(mode)
        let allowed = certificate != nil ? "ssh-ed25519-cert-v01@openssh.com"
            : ["raw-256", "deny-256"].contains(mode) ? "rsa-sha2-256" : "rsa-sha2-512"
        let method = mode == "deny-password" ? "password"
            : mode == "deny-interactive" ? "keyboard-interactive" : "publickey"
        let user = certificate != nil ? try XCTUnwrap(environment["FT_HOST_ALGORITHM_USER"]) : "operator"
        if let certificate {
            XCTAssertTrue(NativeSSHSession.supportsHostCertificates)
            XCTAssertEqual(certificate.subject, subject)
            XCTAssertEqual(certificate.subject.algorithm, "ssh-ed25519")
            XCTAssertEqual(certificate.authority.algorithm, "ssh-rsa")
            XCTAssertEqual(certificate.signatureAlgorithm, mode == "ca-512" ? "rsa-sha2-512" : "rsa-sha2-256")
        } else {
            XCTAssertEqual(subject.algorithm, "ssh-rsa")
        }

        let f = try await SSHWorkspaceTestFixture.make(hostname: "127.0.0.1", port: port, hostBlob: hostBlob) { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            var trusts = workspace["host_trust"] as! [[String: Any]]
            routes[0]["user"] = user
            routes[0]["authentication_methods"] = [method]
            routes[0]["credential_refs"] = method == "publickey" ? ["key"] : []
            trusts[0]["host_key_algorithms"] = [allowed]
            trusts[0]["required_capabilities"] = [WorkspaceDocument.hostAlgorithmPolicyCapability]
            if mode.hasPrefix("probe-") { trusts[0]["pins"] = [] as [[String: Any]] }
            if let certificate {
                trusts[0]["pins"] = [] as [[String: Any]]
                trusts[0]["certificate_authorities"] = [["algorithm": certificate.authority.algorithm,
                    "fingerprint": certificate.authority.fingerprint]]
                trusts[0]["ca_signature_algorithms"] = ["rsa-sha2-512"]
                trusts[0]["required_capabilities"] = [WorkspaceDocument.hostAlgorithmPolicyCapability,
                    WorkspaceDocument.hostCertificateCapability]
            }
            if let hop, let identity = hopIdentity {
                routes[0]["jump_routes"] = ["hop"]
                var hopRoute = routes[0]
                hopRoute["id"] = "hop"; hopRoute["port"] = Int(hop.port)
                hopRoute["host_trust_ref"] = "hop-trust"; hopRoute["jump_routes"] = [] as [String]
                routes.append(hopRoute)
                trusts.append(["id": "hop-trust", "host": "127.0.0.1", "port": Int(hop.port),
                    "pins": [["algorithm": identity.algorithm, "fingerprint": identity.fingerprint]],
                    "revoked_keys": [], "required_capabilities": [WorkspaceDocument.hostAlgorithmPolicyCapability],
                    "host_key_algorithms": [mode == "jump-denied" ? "rsa-sha2-512" : "rsa-sha2-256"]])
            }
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }
        if mode.hasPrefix("probe-") {
            // A later export removes the restriction and still supplies no
            // positive pin. First-use inspection must use the stored earlier
            // requirement, rather than reconstructing policy from this JSON.
            var root = try XCTUnwrap(JSONSerialization.jsonObject(with: f.archive.workspace.raw) as? [String: Any])
            var workspace = root["workspace"] as! [String: Any]
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0].removeValue(forKey: "host_key_algorithms")
            trusts[0]["required_capabilities"] = [] as [String]
            workspace["host_trust"] = trusts; root["workspace"] = workspace
            root["bundle_id"] = "later-export-without-algorithm-field"
            let archive = try AirGapArchive(workspace: WorkspaceDocument(
                data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys])),
                keys: [], using: SSHWorkspaceTestCryptography())
            let installed = try await f.store.install(archive, envelopeDigest: Data(repeating: 81, count: 32),
                expectedRevision: f.installed.revision, sourceAndContentsConfirmed: true,
                securityChangesConfirmed: true, associateWithExistingWorkspace: true).workspace
            XCTAssertNil(try installed.document().hostTrust[0].algorithmPolicy.hostKeyAlgorithms)
            let policy = try await f.store.hostAlgorithmPolicy(workspaceID: installed.id,
                expectedRevision: installed.revision, routeID: "route", support: WorkspaceNativeSSHAuthentication.routeSupport)
            XCTAssertEqual(policy.hostKeyAlgorithms, ["rsa-sha2-512"])
            let activity = WorkspaceNetworkActivity()
            let result = await WorkspaceRouteProbe.run(workspaceID: installed.id,
                expectedRevision: installed.revision, routeID: "route", confirmedByUser: true,
                store: f.store, broker: f.broker, timeout: .seconds(5), networkActivity: activity)
            if mode == "probe-allowed" {
                XCTAssertNil(result.failure)
                XCTAssertEqual(result.peer?.hostSignature, "rsa-sha2-512")
                XCTAssertEqual(result.peer?.hostKey, hostBlob)
                XCTAssertEqual(result.observedIdentity, subject)
                XCTAssertEqual(result.hostKey, .pinMismatch)
            } else {
                XCTAssertEqual(result.failure as? NativeSSHError, .native(-1004))
                XCTAssertNil(result.peer)
                XCTAssertNil(result.hostKey)
                XCTAssertNil(result.observedIdentity)
            }
            XCTAssertEqual(f.vault.counts.reads, 0)
            XCTAssertEqual(f.vault.counts.signatures, 0)
            XCTAssertEqual(activity.accounting().connections, 0)
            let after = try await f.store.load()
            let unchanged = try XCTUnwrap(after.first(where: { $0.id == installed.id }))
            XCTAssertEqual(unchanged.revision, installed.revision)
            XCTAssertEqual(unchanged.manifest, installed.manifest)
            XCTAssertTrue(try unchanged.document().hostTrust[0].pins.isEmpty)
            return
        }
        let policy = try await f.store.hostAlgorithmPolicy(workspaceID: f.installed.id,
            expectedRevision: f.installed.revision, routeID: "route", support: WorkspaceNativeSSHAuthentication.routeSupport)
        XCTAssertEqual(policy.hostKeyAlgorithms, [allowed])
        XCTAssertEqual(policy.caSignatureAlgorithms, certificate == nil ? nil : ["rsa-sha2-512"])
        let prompt = HostAlgorithmForbiddenPrompt(), activity = WorkspaceNetworkActivity()
        let authentication: WorkspaceSSHAuthenticationChoice = method == "password" ? .password
            : method == "keyboard-interactive" ? .keyboardInteractive : .publicKey("key")
        var connected = false
        do {
            let owner = try await WorkspaceNativeSSHConnection.connect(workspaceID: f.installed.id,
                expectedRevision: f.installed.revision, machineID: "machine", routeID: "route",
                authentication: authentication, confirmedByUser: true, store: f.store, broker: f.broker,
                passwordPrompter: prompt, keyboardInteractivePrompter: prompt, networkActivity: activity,
                connectionTimeout: .seconds(5), authenticationTimeout: .seconds(5), attachMux: false)
            do {
                connected = true
                XCTAssertTrue(shouldConnect, "A prohibited host algorithm completed authentication")
                XCTAssertEqual(owner.peer.hostSignature, allowed)
                XCTAssertEqual(owner.peer.hostKey, certificate?.blob ?? hostBlob)
                try await owner.validateForPublication()
                let host = try WorkspaceNativeSSHAuthentication.hostIdentity(for: owner.peer)
                let grant = try await f.broker.authorization(workspaceID: f.installed.id,
                    expectedRevision: f.installed.revision, routeID: "route", credentialID: "key",
                    presentedHostKey: host.key, hostCertificate: host.certificate,
                    negotiatedHostSignature: owner.peer.hostSignature)
                XCTAssertEqual(grant.negotiatedHostSignature, allowed)
                XCTAssertEqual(grant.hostCertificate, certificate)
                await owner.close()
            } catch {
                await owner.close()
                throw error
            }
        } catch {
            if shouldConnect { throw error }
            XCTAssertNil(error as? WorkspaceSSHConnectionHostKeyMismatch,
                "An algorithm restriction is not permission to review or replace a pinned key")
            if mode == "ca-256" {
                XCTAssertEqual(error as? SSHAuthenticationError, .denied)
            } else if mode == "jump-denied" {
                XCTAssertEqual(error as? NativeSSHError, .jumpConnectionFailed)
            } else {
                XCTAssertEqual(error as? NativeSSHError, .native(-1004))
            }
        }
        XCTAssertEqual(connected, shouldConnect)
        let requests = await prompt.requests
        XCTAssertEqual(requests, 0)
        let acceptedLogins = shouldConnect ? (hop == nil ? 1 : 2) : 0
        XCTAssertEqual(f.vault.counts.reads, acceptedLogins)
        XCTAssertEqual(f.vault.counts.signatures, acceptedLogins)
        XCTAssertEqual(activity.accounting().connections, 0)
        let after = try await f.store.load()
        let unchanged = try XCTUnwrap(after.first(where: { $0.id == f.installed.id }))
        XCTAssertEqual(unchanged.revision, f.installed.revision)
        XCTAssertEqual(unchanged.manifest, f.installed.manifest)
    }
}
