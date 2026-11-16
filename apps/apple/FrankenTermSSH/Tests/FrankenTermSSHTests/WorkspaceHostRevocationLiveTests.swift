import Foundation
import XCTest
@testable import FrankenTermKit
@testable import FrankenTermSSH
@testable import FrankenTermSSHIntegration

private actor RevokedHostForbiddenPrompt: WorkspaceSSHPasswordPrompting, WorkspaceSSHKeyboardInteractivePrompting {
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
final class WorkspaceHostRevocationLiveTests: XCTestCase {
    private struct Hop: Decodable {
        let port: UInt16
        let hostKey: String
    }

    /// Run by the independent loopback/OpenSSH harness. A denied peer must see
    /// no username, method query, key offer, password or MFA response. A valid
    /// jump may authenticate, but it never grants authority to the destination.
    func testLiveRevocationPrecedesEveryCredentialAndSurvivesReimport() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let mode = env["FT_HOST_REVOCATION_MODE"],
              let port = env["FT_HOST_REVOCATION_PORT"].flatMap(UInt16.init),
              let hostBlob = env["FT_HOST_REVOCATION_HOSTKEY"].flatMap({ Data(base64Encoded: $0) }),
              let sourceBytes = env["FT_HOST_REVOCATION_SOURCE"].flatMap({ Data(base64Encoded: $0) }),
              let encodedHop = env["FT_HOST_REVOCATION_HOP"] else {
            throw XCTSkip("Run Tests/Interop/test_host_revocations.py with the real adapter and this XCTest product.")
        }
        let allowedModes: Set<String> = ["raw-allowed", "raw-denied", "password-denied", "interactive-denied",
            "jump-denied", "destination-denied", "probe-history", "certificate-allowed",
            "certificate-serial-denied", "certificate-issuer-denied", "certificate-subject-denied"]
        guard allowedModes.contains(mode) else { throw SSHAuthenticationError.invalidChallenge }
        let source = try SSHRevokedHostKeys.parse(data: sourceBytes, id: "revocations")
        let resource = try JSONSerialization.jsonObject(with: source.json.encoded())
        let hop = try JSONDecoder().decode(Hop?.self, from: Data(encodedHop.utf8))
        let hopIdentity = try hop.map { try SSHPublicIdentity(blob: XCTUnwrap(Data(base64Encoded: $0.hostKey))) }
        let certificate = try env["FT_HOST_REVOCATION_CERTIFICATE"].map {
            try SSHHostCertificate(blob: XCTUnwrap(Data(base64Encoded: $0)))
        }
        let subject = try SSHPublicIdentity(blob: hostBlob)
        if let certificate {
            XCTAssertTrue(NativeSSHSession.supportsHostCertificates)
            XCTAssertEqual(certificate.subject, subject)
            XCTAssertEqual(certificate.serial, 700)
        }
        let shouldConnect = mode == "raw-allowed" || mode == "certificate-allowed"
        let method = mode == "password-denied" ? "password"
            : mode == "interactive-denied" ? "keyboard-interactive" : "publickey"
        let user = certificate == nil ? "operator" : try XCTUnwrap(env["FT_HOST_REVOCATION_USER"])
        let f = try await SSHWorkspaceTestFixture.make(hostname: "127.0.0.1", port: port, hostBlob: hostBlob) { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            var trusts = workspace["host_trust"] as! [[String: Any]]
            routes[0]["user"] = user
            routes[0]["authentication_methods"] = [method]
            routes[0]["credential_refs"] = method == "publickey" ? ["key"] : []
            workspace["host_revocation_sources"] = [resource]
            if mode != "jump-denied" {
                trusts[0]["revocation_source_refs"] = [source.id]
                trusts[0]["required_capabilities"] = [WorkspaceDocument.hostRevocationCapability]
            }
            if mode == "probe-history" { trusts[0]["pins"] = [] as [[String: Any]] }
            if let certificate {
                trusts[0]["pins"] = [] as [[String: Any]]
                trusts[0]["certificate_authorities"] = [["algorithm": certificate.authority.algorithm,
                    "fingerprint": certificate.authority.fingerprint]]
                trusts[0]["required_capabilities"] = [WorkspaceDocument.hostRevocationCapability,
                    WorkspaceDocument.hostCertificateCapability]
            }
            if let hop, let identity = hopIdentity {
                routes[0]["jump_routes"] = ["hop"]
                var hopRoute = routes[0]
                hopRoute["id"] = "hop"; hopRoute["port"] = Int(hop.port)
                hopRoute["host_trust_ref"] = "hop-trust"; hopRoute["jump_routes"] = [] as [String]
                routes.append(hopRoute)
                var hopTrust: [String: Any] = ["id": "hop-trust", "host": "127.0.0.1", "port": Int(hop.port),
                    "pins": [["algorithm": identity.algorithm, "fingerprint": identity.fingerprint]],
                    "revoked_keys": [], "required_capabilities": []]
                if mode == "jump-denied" {
                    hopTrust["revocation_source_refs"] = [source.id]
                    hopTrust["required_capabilities"] = [WorkspaceDocument.hostRevocationCapability]
                }
                trusts.append(hopTrust)
            }
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }

        if mode == "probe-history" {
            var root = try XCTUnwrap(JSONSerialization.jsonObject(with: f.archive.workspace.raw) as? [String: Any])
            var workspace = root["workspace"] as! [String: Any]
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0].removeValue(forKey: "revocation_source_refs")
            trusts[0]["required_capabilities"] = [] as [String]
            workspace.removeValue(forKey: "host_revocation_sources")
            workspace["host_trust"] = trusts; root["workspace"] = workspace
            root["bundle_id"] = "later-export-without-revocation-source"
            let archive = try AirGapArchive(workspace: WorkspaceDocument(
                data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys])),
                keys: [], using: SSHWorkspaceTestCryptography())
            let installed = try await f.store.install(archive, envelopeDigest: Data(repeating: 82, count: 32),
                expectedRevision: f.installed.revision, sourceAndContentsConfirmed: true,
                securityChangesConfirmed: true, associateWithExistingWorkspace: true).workspace
            XCTAssertTrue(try installed.document().hostRevocationSources.isEmpty)
            let activity = WorkspaceNetworkActivity()
            let result = await WorkspaceRouteProbe.run(workspaceID: installed.id, expectedRevision: installed.revision,
                routeID: "route", confirmedByUser: true, store: f.store, broker: f.broker,
                timeout: .seconds(5), networkActivity: activity)
            XCTAssertEqual(result.peer?.hostKey, hostBlob)
            XCTAssertEqual(result.hostKey, .revoked)
            XCTAssertNil(result.observedIdentity, "A revoked observation must never become a first-use pin candidate")
            XCTAssertEqual(f.vault.counts.reads, 0)
            XCTAssertEqual(f.vault.counts.signatures, 0)
            XCTAssertEqual(activity.accounting().connections, 0)
            let after = try await f.store.load()
            XCTAssertEqual(after.first(where: { $0.id == installed.id })?.revision, installed.revision)
            XCTAssertEqual(after.first(where: { $0.id == installed.id })?.manifest, installed.manifest)
            return
        }

        let prompt = RevokedHostForbiddenPrompt(), activity = WorkspaceNetworkActivity()
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
                XCTAssertTrue(shouldConnect, "Revoked host completed authentication")
                XCTAssertEqual(owner.peer.hostKey, certificate?.blob ?? hostBlob)
                try await owner.validateForPublication()
                await owner.close()
            } catch {
                await owner.close()
                throw error
            }
        } catch {
            if shouldConnect { throw error }
            XCTAssertNil(error as? WorkspaceSSHConnectionHostKeyMismatch,
                "Revocation must not become permission to replace a pin")
            if mode == "jump-denied" {
                XCTAssertEqual(error as? NativeSSHError, .jumpConnectionFailed)
            } else { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
        }
        XCTAssertEqual(connected, shouldConnect)
        let requests = await prompt.requests
        XCTAssertEqual(requests, 0)
        let signedLogins = shouldConnect || mode == "destination-denied" ? 1 : 0
        XCTAssertEqual(f.vault.counts.reads, signedLogins)
        XCTAssertEqual(f.vault.counts.signatures, signedLogins)
        XCTAssertEqual(activity.accounting().connections, 0)
        let after = try await f.store.load()
        XCTAssertEqual(after.first(where: { $0.id == f.installed.id })?.revision, f.installed.revision)
        XCTAssertEqual(after.first(where: { $0.id == f.installed.id })?.manifest, f.installed.manifest)
    }
}
