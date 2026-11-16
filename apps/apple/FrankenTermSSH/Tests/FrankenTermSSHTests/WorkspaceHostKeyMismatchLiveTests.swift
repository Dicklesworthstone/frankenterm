import Foundation
import XCTest
@testable import FrankenTermKit
@testable import FrankenTermSSH
@testable import FrankenTermSSHIntegration

private actor HostMismatchVerifiedHopPrompt: WorkspaceSSHPasswordPrompting, WorkspaceSSHKeyboardInteractivePrompting {
    let fixture: SSHWorkspaceTestFixture
    private(set) var accounts: [String] = []
    init(fixture: SSHWorkspaceTestFixture) { self.fixture = fixture }

    private func validate(workspaceID: UUID, revision: UUID, routeID: String,
                          host: String, port: UInt16, user: String,
                          key: WorkspaceDocument.HostKey, transit: String?) throws {
        guard workspaceID == fixture.installed.id, revision == fixture.installed.revision,
              routeID.hasPrefix("hop-"), transit == "route", user == "operator",
              let route = fixture.archive.workspace.routes.first(where: { $0.id == routeID }),
              route.host == host, route.port == port, route.user == user,
              let trust = fixture.archive.workspace.hostTrust.first(where: { $0.id == route.trustID }),
              trust.pins.contains(key) else { throw SSHAuthenticationError.stale }
    }

    func password(for request: WorkspaceSSHPasswordRequest) throws -> Data {
        let a = request.authorization
        try validate(workspaceID: a.workspaceID, revision: a.workspaceRevision, routeID: a.routeID,
            host: a.host, port: a.port, user: a.user, key: a.presentedHostKey, transit: a.transitForRouteID)
        guard a.hostKeyProbe == nil else { throw SSHAuthenticationError.stale }
        accounts.append("\(a.routeID):password")
        return Data("verified-hop-password".utf8)
    }

    func responses(for request: WorkspaceSSHKeyboardInteractiveRequest) throws -> [Data] {
        let a = request.authorization, challenge = request.challenge
        try validate(workspaceID: a.workspaceID, revision: a.workspaceRevision, routeID: a.routeID,
            host: a.host, port: a.port, user: a.user, key: a.presentedHostKey, transit: a.transitForRouteID)
        guard a.hostKeyProbe == nil, challenge.round == 1, challenge.name == "Verified hop",
              challenge.instruction == "Enter the one-time code.", challenge.prompts.count == 1,
              challenge.prompts[0].text == "One-time code:", !challenge.prompts[0].echo else {
            throw SSHAuthenticationError.invalidChallenge
        }
        accounts.append("\(a.routeID):keyboard-interactive")
        return [Data("654321".utf8)]
    }

    nonisolated func cancel(requestID: UUID) {}
}

@MainActor
final class WorkspaceHostKeyMismatchLiveTests: XCTestCase {
    private struct Hop: Decodable {
        let port: UInt16
        let hostKey: String
        let authentication: String
    }

    /// The independent peers assert that the failing exchange receives no
    /// account, method probe, credential offer, signature, prompt or channel.
    /// Trusted ancestors can authenticate only to reach that exact peer.
    func testLiveMismatchRetainsExactPeerWithoutAuthentication() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard let port = environment["FT_HOST_MISMATCH_PORT"].flatMap(UInt16.init),
              let hostBlob = environment["FT_HOST_MISMATCH_HOSTKEY"].flatMap({ Data(base64Encoded: $0) }),
              let mode = environment["FT_HOST_MISMATCH_MODE"],
              let encodedHops = environment["FT_HOST_MISMATCH_HOPS"] else {
            throw XCTSkip("Run Tests/Interop/test_host_mismatch.py with the built XCTest bundle and native driver.")
        }
        guard ["direct-key", "direct-password", "direct-interactive", "destination-after-hops",
               "second-hop", "revoked-presented", "reviewed-update"].contains(mode) else {
            throw SSHAuthenticationError.invalidChallenge
        }
        let hops = try JSONDecoder().decode([Hop].self, from: Data(encodedHops.utf8))
        guard hops.count <= 3, mode != "second-hop" || hops.count == 3 else {
            throw SSHAuthenticationError.invalidChallenge
        }
        let actualDestination = try SSHPublicIdentity(blob: hostBlob)
        let actualHops = try hops.map { try SSHPublicIdentity(blob: XCTUnwrap(Data(base64Encoded: $0.hostKey))) }
        let previous = try await SSHWorkspaceTestFixture.make(seed: Data(repeating: 77, count: 32))
        let destinationMethod = mode == "direct-password" ? "password"
            : mode == "direct-interactive" ? "keyboard-interactive" : "publickey"
        let f = try await SSHWorkspaceTestFixture.make(hostname: "127.0.0.1", port: port,
            hostBlob: mode == "second-hop" ? hostBlob : previous.hostBlob) { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            var trusts = workspace["host_trust"] as! [[String: Any]]
            routes[0]["authentication_methods"] = [destinationMethod]
            routes[0]["credential_refs"] = destinationMethod == "publickey" ? ["key"] : []
            routes[0]["host_key_alias"] = "destination-alias.invalid"
            routes[0]["jump_routes"] = hops.indices.map { "hop-\($0)" }
            trusts[0]["host_key_alias"] = "destination-alias.invalid"
            for (index, hop) in hops.enumerated() {
                var route = routes[0]
                route["id"] = "hop-\(index)"; route["port"] = Int(hop.port)
                route["host_trust_ref"] = "hop-trust-\(index)"
                route["host_key_alias"] = "hop-\(index)-alias.invalid"
                route["jump_routes"] = []; route["authentication_methods"] = [hop.authentication]
                route["credential_refs"] = []
                routes.append(route)
                let identity = actualHops[index]
                let algorithm = mode == "second-hop" && index == 1 ? previous.host.algorithm : identity.algorithm
                let fingerprint = mode == "second-hop" && index == 1 ? previous.host.fingerprint : identity.fingerprint
                trusts.append(["id": "hop-trust-\(index)", "host": "127.0.0.1", "port": Int(hop.port),
                    "host_key_alias": "hop-\(index)-alias.invalid", "revoked_keys": [],
                    "pins": [["algorithm": algorithm, "fingerprint": fingerprint]]])
            }
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }
        let badIndex: Int? = mode == "second-hop" ? 1 : nil
        let presentedIdentity = badIndex.map { actualHops[$0] } ?? actualDestination
        let presented = try WorkspaceDocument.HostKey(algorithm: presentedIdentity.algorithm,
                                                      fingerprint: presentedIdentity.fingerprint)
        if mode == "revoked-presented" {
            // The configured old pin remains locally ready, but the actually
            // observed new identity is independently denied by the phone.
            try await f.store.revokeHostLocally(workspaceID: f.installed.id, trustID: "trust", key: presented)
        }
        let prompt = HostMismatchVerifiedHopPrompt(fixture: f), activity = WorkspaceNetworkActivity()
        let authentication: WorkspaceSSHAuthenticationChoice = destinationMethod == "password" ? .password
            : destinationMethod == "keyboard-interactive" ? .keyboardInteractive : .publicKey("key")
        func connect(revision: UUID) async throws -> WorkspaceNativeSSHConnection {
            try await WorkspaceNativeSSHConnection.connect(workspaceID: f.installed.id,
                expectedRevision: revision, machineID: "machine", routeID: "route",
                authentication: authentication, confirmedByUser: true, store: f.store, broker: f.broker,
                passwordPrompter: prompt, keyboardInteractivePrompter: prompt, networkActivity: activity,
                connectionTimeout: .seconds(5), authenticationTimeout: .seconds(5), attachMux: false)
        }
        var observed: WorkspaceSSHConnectionHostKeyMismatch?
        do {
            let owner = try await connect(revision: f.installed.revision)
            await owner.close()
            XCTFail("A mismatched or revoked host was authenticated")
        } catch {
            if mode == "revoked-presented" {
                XCTAssertNil(error as? WorkspaceSSHConnectionHostKeyMismatch)
                XCTAssertEqual(error as? SSHAuthenticationError, .denied)
            } else {
                let mismatch = try XCTUnwrap(error as? WorkspaceSSHConnectionHostKeyMismatch,
                    "The failed exchange lost its structured peer identity: \(error)")
                observed = mismatch
                XCTAssertEqual(mismatch.workspaceID, f.installed.id)
                XCTAssertEqual(mismatch.expectedRevision, f.installed.revision)
                XCTAssertEqual(mismatch.machineID, "machine")
                XCTAssertEqual(mismatch.destinationRouteID, "route")
                XCTAssertEqual(mismatch.routeID, badIndex.map { "hop-\($0)" } ?? "route")
                XCTAssertEqual(mismatch.hopIndex, badIndex)
                XCTAssertEqual(mismatch.host, "127.0.0.1")
                XCTAssertEqual(mismatch.port, badIndex.map { hops[$0].port } ?? port)
                XCTAssertEqual(mismatch.user, "operator")
                XCTAssertEqual(mismatch.hostKeyAlias,
                    badIndex.map { "hop-\($0)-alias.invalid" } ?? "destination-alias.invalid")
                XCTAssertEqual(mismatch.expectedKeys, [previous.host])
                XCTAssertEqual(mismatch.presentedKey, presented)
                XCTAssertNil(error as? NativeSSHError, "A host mismatch must not gain automatic route/key retry authority")
                for description in [error.localizedDescription, String(describing: mismatch), String(reflecting: mismatch)] {
                    for detail in [mismatch.host, mismatch.user, mismatch.expectedRevision.uuidString,
                                   previous.host.fingerprint, presented.fingerprint, mismatch.hostKeyAlias ?? ""] {
                        XCTAssertFalse(description.contains(detail), "Generic error output exposed a public review field")
                    }
                }
            }
        }
        let accounts = await prompt.accounts
        let reachedHops = mode == "second-hop" ? 1 : hops.count
        XCTAssertEqual(accounts, hops.prefix(reachedHops).enumerated().map { "hop-\($0.offset):\($0.element.authentication)" })
        XCTAssertEqual(f.vault.counts.reads, 0)
        XCTAssertEqual(f.vault.counts.signatures, 0)
        XCTAssertEqual(activity.accounting().connections, 0)
        let afterFailure = try await f.store.load()
        let unchanged = try XCTUnwrap(afterFailure.first(where: { $0.id == f.installed.id }))
        XCTAssertEqual(unchanged.revision, f.installed.revision)
        XCTAssertEqual(unchanged.manifest, f.installed.manifest, "Inspecting a failure must not publish new trust")

        if mode == "reviewed-update" {
            let mismatch = try XCTUnwrap(observed)
            var root = try XCTUnwrap(JSONSerialization.jsonObject(with: f.archive.workspace.raw) as? [String: Any])
            var workspace = root["workspace"] as! [String: Any]
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["pins"] = [["algorithm": actualDestination.algorithm, "fingerprint": actualDestination.fingerprint]]
            workspace["host_trust"] = trusts; root["workspace"] = workspace
            root["bundle_id"] = "independently-reviewed-host-update"
            let updatedArchive = try AirGapArchive(
                workspace: WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys])),
                keys: [], using: SSHWorkspaceTestCryptography())
            let updated = try await f.store.install(updatedArchive, envelopeDigest: Data(repeating: 47, count: 32),
                expectedRevision: f.installed.revision, sourceAndContentsConfirmed: true,
                securityChangesConfirmed: true, associateWithExistingWorkspace: true).workspace
            XCTAssertNotEqual(mismatch.expectedRevision, updated.revision)
            let owner = try await connect(revision: updated.revision)
            do {
                XCTAssertEqual(owner.peer.hostKey, hostBlob)
                try await owner.validateForPublication()
                await owner.close()
            } catch { await owner.close(); throw error }
            XCTAssertEqual(f.vault.counts.reads, 1)
            XCTAssertEqual(f.vault.counts.signatures, 1)
            XCTAssertEqual(activity.accounting().connections, 0)
        }
    }
}
