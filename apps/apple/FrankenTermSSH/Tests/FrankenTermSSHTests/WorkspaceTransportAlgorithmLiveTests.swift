import Foundation
import XCTest
@testable import FrankenTermKit
@testable import FrankenTermSSH
@testable import FrankenTermSSHIntegration

private actor TransportAlgorithmPrompt: WorkspaceSSHPasswordPrompting, WorkspaceSSHKeyboardInteractivePrompting {
    private(set) var passwordCalls = 0, interactiveCalls = 0
    func password(for request: WorkspaceSSHPasswordRequest) throws -> Data {
        passwordCalls += 1
        guard request.authorization.user == "operator" else { throw SSHAuthenticationError.denied }
        return Data("synthetic transport password".utf8)
    }
    func responses(for request: WorkspaceSSHKeyboardInteractiveRequest) throws -> [Data] {
        interactiveCalls += 1
        guard request.authorization.user == "operator", request.challenge.round == 1,
              request.challenge.prompts.map(\.text) == ["Transport code:"] else {
            throw SSHAuthenticationError.invalidChallenge
        }
        return [Data("654321".utf8)]
    }
    nonisolated func cancel(requestID: UUID) {}
}

@MainActor
final class WorkspaceTransportAlgorithmLiveTests: XCTestCase {
    private struct Hop: Decodable { let port: UInt16; let hostKey: String }

    /// The original production store, broker and managed factory connect to
    /// independent peers supplied by test_transport_algorithms.py. A denied
    /// transport must not reach any credential callback or identity offer.
    func testLiveTransportPolicyBeforeCredentials() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard let mode = environment["FT_TRANSPORT_POLICY_CASE"],
              let port = environment["FT_TRANSPORT_POLICY_PORT"].flatMap(UInt16.init),
              let hostBlob = environment["FT_TRANSPORT_POLICY_HOSTKEY"].flatMap({ Data(base64Encoded: $0) }),
              let hopJSON = environment["FT_TRANSPORT_POLICY_HOP"],
              let expectedJSON = environment["FT_TRANSPORT_POLICY_EXPECTED"] else {
            throw XCTSkip("Run Tests/Interop/test_transport_algorithms.py with the actual native library and XCTest product.")
        }
        guard ["allow-publickey", "allow-password", "allow-interactive", "allow-multifactor",
               "deny-publickey", "deny-password", "deny-interactive", "deny-multifactor", "asymmetric",
               "jump-allowed", "jump-denied", "jump-destination-denied", "probe-allowed", "probe-denied",
               "empty", "unsupported"].contains(mode) else { throw SSHAuthenticationError.invalidChallenge }
        let expected = try JSONDecoder().decode([String].self, from: Data(expectedJSON.utf8))
        XCTAssertEqual(expected.count, 5)
        let hop = try JSONDecoder().decode(Hop?.self, from: Data(hopJSON.utf8))
        let hopIdentity = try hop.map { try SSHPublicIdentity(blob: XCTUnwrap(Data(base64Encoded: $0.hostKey))) }
        let isProbe = mode.hasPrefix("probe-"), predial = ["empty", "unsupported"].contains(mode)
        let shouldConnect = mode.hasPrefix("allow-") || mode == "asymmetric" || mode == "jump-allowed"
        let password = mode.hasSuffix("-password"), interactive = mode.hasSuffix("-interactive")
        let multifactor = mode.hasSuffix("-multifactor")
        let usesKey = !password && !interactive
        let exchanges = ["ecdh-sha2-nistp384", "curve25519-sha256"]
        let ciphers = ["aes128-ctr", "aes256-ctr"]
        let macs = ["hmac-sha2-512-etm@openssh.com", "hmac-sha2-256-etm@openssh.com"]
        let f = try await SSHWorkspaceTestFixture.make(hostname: "127.0.0.1", port: port, hostBlob: hostBlob) { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            var trusts = workspace["host_trust"] as! [[String: Any]]
            routes[0]["authentication_methods"] = multifactor ? ["publickey", "keyboard-interactive"]
                : password ? ["password"] : interactive ? ["keyboard-interactive"] : ["publickey"]
            routes[0]["credential_refs"] = usesKey ? ["key"] : [] as [String]
            trusts[0]["kex_algorithms"] = mode == "empty" ? [] : exchanges
            trusts[0]["ciphers"] = mode == "unsupported" ? ["aes256-gcm@openssh.com"] : ciphers
            trusts[0]["macs"] = macs
            trusts[0]["required_capabilities"] = [WorkspaceDocument.transportAlgorithmPolicyCapability]
            if isProbe { trusts[0]["pins"] = [] as [[String: Any]] }
            if let hop, let identity = hopIdentity {
                routes[0]["jump_routes"] = ["hop"]
                var transit = routes[0]
                transit["id"] = "hop"; transit["port"] = Int(hop.port)
                transit["host_trust_ref"] = "hop-trust"; transit["jump_routes"] = [] as [String]
                routes.append(transit)
                trusts.append(["id": "hop-trust", "host": "127.0.0.1", "port": Int(hop.port),
                    "pins": [["algorithm": identity.algorithm, "fingerprint": identity.fingerprint]], "revoked_keys": [],
                    "kex_algorithms": ["ecdh-sha2-nistp256"], "ciphers": ["aes256-ctr"],
                    "macs": ["hmac-sha2-256-etm@openssh.com"],
                    "required_capabilities": [WorkspaceDocument.transportAlgorithmPolicyCapability]])
            }
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }
        let prompt = TransportAlgorithmPrompt(), activity = WorkspaceNetworkActivity()
        if isProbe {
            // Removing all fields in a later export cannot erase the durable
            // policy, even while the route still lacks any positive host pin.
            var root = try XCTUnwrap(JSONSerialization.jsonObject(with: f.archive.workspace.raw) as? [String: Any])
            var workspace = root["workspace"] as! [String: Any]
            var trusts = workspace["host_trust"] as! [[String: Any]]
            for name in ["kex_algorithms", "ciphers", "macs"] { trusts[0].removeValue(forKey: name) }
            trusts[0]["required_capabilities"] = [] as [String]
            workspace["host_trust"] = trusts; root["workspace"] = workspace
            root["bundle_id"] = "later-export-without-transport-policy"
            let archive = try AirGapArchive(workspace: WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root,
                options: [.sortedKeys])), keys: [], using: SSHWorkspaceTestCryptography())
            let installed = try await f.store.install(archive, envelopeDigest: Data(repeating: 93, count: 32),
                expectedRevision: f.installed.revision, sourceAndContentsConfirmed: true,
                securityChangesConfirmed: true, associateWithExistingWorkspace: true).workspace
            XCTAssertFalse(try installed.document().hostTrust[0].algorithmPolicy.transport.isConstrained)
            let retained = try await f.store.hostAlgorithmPolicy(workspaceID: installed.id,
                expectedRevision: installed.revision, routeID: "route", support: WorkspaceNativeSSHAuthentication.routeSupport)
            XCTAssertEqual(retained.transport.keyExchangeAlgorithms, exchanges)
            XCTAssertEqual(retained.transport.ciphers, ciphers)
            XCTAssertEqual(retained.transport.macs, macs)
            let result = await WorkspaceRouteProbe.run(workspaceID: installed.id, expectedRevision: installed.revision,
                routeID: "route", confirmedByUser: true, store: f.store, broker: f.broker,
                timeout: .seconds(5), networkActivity: activity)
            if mode == "probe-allowed" {
                XCTAssertNil(result.failure)
                let peer = try XCTUnwrap(result.peer)
                XCTAssertEqual([peer.keyExchange, peer.outboundCipher, peer.inboundCipher, peer.outboundMAC, peer.inboundMAC], expected)
                XCTAssertEqual(result.hostKey, .pinMismatch)
                XCTAssertEqual(result.observedIdentity, try SSHPublicIdentity(blob: hostBlob))
            } else {
                XCTAssertEqual(result.failure as? NativeSSHError, .native(-1004))
                XCTAssertNil(result.peer); XCTAssertNil(result.hostKey); XCTAssertNil(result.observedIdentity)
            }
            XCTAssertEqual(f.vault.counts.reads, 0); XCTAssertEqual(f.vault.counts.signatures, 0)
            XCTAssertEqual(activity.accounting().connections, 0)
            return
        }
        let choice: WorkspaceSSHAuthenticationChoice = password ? .password : interactive ? .keyboardInteractive : .publicKey("key")
        var connected = false
        do {
            let owner = try await WorkspaceNativeSSHConnection.connect(workspaceID: f.installed.id,
                expectedRevision: f.installed.revision, machineID: "machine", routeID: "route",
                authentication: choice, confirmedByUser: true, store: f.store, broker: f.broker,
                passwordPrompter: prompt, keyboardInteractivePrompter: prompt, networkActivity: activity,
                connectionTimeout: .seconds(5), authenticationTimeout: .seconds(5), attachMux: false)
            do {
                connected = true
                XCTAssertTrue(shouldConnect, "Disallowed transport reached an authenticated owner")
                let peer = owner.peer
                XCTAssertEqual([peer.keyExchange, peer.outboundCipher, peer.inboundCipher, peer.outboundMAC, peer.inboundMAC], expected)
                XCTAssertEqual(peer.hostKey, hostBlob)
                try await owner.validateForPublication()
                if usesKey {
                    let identity = try WorkspaceNativeSSHAuthentication.hostIdentity(for: peer)
                    let evidence = WorkspaceNativeSSHAuthentication.transportAlgorithms(for: peer)
                    let grant = try await f.broker.authorization(workspaceID: f.installed.id,
                        expectedRevision: f.installed.revision, routeID: "route", credentialID: "key",
                        presentedHostKey: identity.key, hostCertificate: identity.certificate,
                        negotiatedHostSignature: peer.hostSignature, negotiatedTransport: evidence)
                    XCTAssertEqual(grant.negotiatedTransport, evidence)
                }
                await owner.close()
            } catch { await owner.close(); throw error }
        } catch {
            if shouldConnect { throw error }
            XCTAssertNil(error as? WorkspaceSSHConnectionHostKeyMismatch)
            if predial { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
            else { XCTAssertEqual(error as? NativeSSHError, mode == "jump-denied" ? .jumpConnectionFailed : .native(-1004)) }
        }
        XCTAssertEqual(connected, shouldConnect)
        let passwordCalls = await prompt.passwordCalls, interactiveCalls = await prompt.interactiveCalls
        XCTAssertEqual(passwordCalls, shouldConnect && password ? 1 : 0)
        XCTAssertEqual(interactiveCalls, shouldConnect && (interactive || multifactor) ? 1 : 0)
        let hopLogins = mode == "jump-allowed" || mode == "jump-destination-denied" ? 1 : 0
        let keyLogins = (shouldConnect && usesKey ? 1 : 0) + hopLogins
        XCTAssertEqual(f.vault.counts.reads, keyLogins); XCTAssertEqual(f.vault.counts.signatures, keyLogins)
        XCTAssertEqual(activity.accounting().connections, 0)
        let unchanged = try await f.store.load()
        XCTAssertEqual(unchanged.first?.manifest, f.installed.manifest)
    }
}
