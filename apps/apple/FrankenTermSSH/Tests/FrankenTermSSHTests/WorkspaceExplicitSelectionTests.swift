import Foundation
import XCTest
@testable import FrankenTermKit
@testable import FrankenTermSSH
@testable import FrankenTermSSHIntegration

private actor ExplicitSelectionPrompts: WorkspaceSSHPasswordPrompting, WorkspaceSSHKeyboardInteractivePrompting {
    let fixture: SSHWorkspaceTestFixture
    let refuseDestination: Bool
    private(set) var peers: [String] = []

    init(fixture: SSHWorkspaceTestFixture, refuseDestination: Bool) {
        self.fixture = fixture; self.refuseDestination = refuseDestination
    }

    private func validate(workspaceID: UUID, revision: UUID, routeID: String,
                          host: String, port: UInt16, user: String,
                          key: WorkspaceDocument.HostKey, transit: String?) throws {
        guard workspaceID == fixture.installed.id, revision == fixture.installed.revision,
              let route = fixture.archive.workspace.routes.first(where: { $0.id == routeID }),
              route.host == host, route.port == port, route.user == user, user == "operator",
              transit == (routeID == "route" ? nil : "route"),
              let trust = fixture.archive.workspace.hostTrust.first(where: { $0.id == route.trustID }),
              trust.pins.contains(key) else { throw SSHAuthenticationError.stale }
    }

    func password(for request: WorkspaceSSHPasswordRequest) throws -> Data {
        let a = request.authorization
        try validate(workspaceID: a.workspaceID, revision: a.workspaceRevision, routeID: a.routeID,
                     host: a.host, port: a.port, user: a.user, key: a.presentedHostKey, transit: a.transitForRouteID)
        guard a.hostKeyProbe == nil else { throw SSHAuthenticationError.stale }
        peers.append("\(a.routeID):password")
        return Data((refuseDestination && a.routeID == "route" ? "wrong-password" : "selected-password").utf8)
    }

    func responses(for request: WorkspaceSSHKeyboardInteractiveRequest) throws -> [Data] {
        let a = request.authorization, challenge = request.challenge
        try validate(workspaceID: a.workspaceID, revision: a.workspaceRevision, routeID: a.routeID,
                     host: a.host, port: a.port, user: a.user, key: a.presentedHostKey, transit: a.transitForRouteID)
        guard a.hostKeyProbe == nil, challenge.round == 1, challenge.name == "Verification",
              challenge.instruction == "Enter the one-time code.", challenge.prompts.count == 1,
              challenge.prompts[0].text == "Verification code:", !challenge.prompts[0].echo else {
            throw SSHAuthenticationError.invalidChallenge
        }
        peers.append("\(a.routeID):keyboard-interactive")
        return [Data("654321".utf8)]
    }

    nonisolated func cancel(requestID: UUID) {}
}

private actor ExplicitSelectionTerminal: TerminalSessionConsumer {
    let identity: TerminalConnectionIdentity
    private(set) var prepared = false
    private(set) var output = Data()
    private(set) var ended = false
    init(identity: TerminalConnectionIdentity) { self.identity = identity }
    func prepare(_ identity: TerminalConnectionIdentity) throws {
        guard !prepared, self.identity == identity else { throw TerminalInputError.stale }
        prepared = true
    }
    func receive(_ update: TerminalSessionUpdate, source: TerminalSource) throws {
        guard prepared, source.identity == identity, update.output.count <= 16_384,
              output.count + update.output.count <= 65_536 else { throw TerminalInputError.stale }
        output += update.output; ended = ended || update.endOfOutput
    }
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) throws {
        guard prepared, source.identity == identity, outcomes.count <= 64,
              outcomes.allSatisfy({ $0.receipt.source == source }) else { throw TerminalInputError.stale }
    }
}

@MainActor
final class WorkspaceExplicitSelectionTests: XCTestCase {
    private struct Hop: Decodable {
        let port: UInt16
        let hostKey: String
        let authentication: String
    }

    /// The independent libssh fixture has a usable imported key first in every
    /// route. Only the explicitly selected password/MFA method may reach each
    /// server, including a mixed three-hop path and a refused destination.
    func testLiveExplicitRouteAuthentication() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard let port = environment["FT_SELECTION_TEST_PORT"].flatMap(UInt16.init),
              let hostKey = environment["FT_SELECTION_TEST_HOSTKEY"].flatMap({ Data(base64Encoded: $0) }),
              let mode = environment["FT_SELECTION_TEST_MODE"],
              let encodedHops = environment["FT_SELECTION_TEST_HOPS"] else {
            throw XCTSkip("Run Tests/Interop/test_explicit_selection.py with the built XCTest bundle and native driver.")
        }
        let hopInputs = try JSONDecoder().decode([Hop].self, from: Data(encodedHops.utf8))
        guard hopInputs.count <= 3,
              ["password", "interactive", "jump-mixed", "jump-destination-refused",
               "stale", "hop-mismatch", "missing-prompter"].contains(mode) else {
            throw SSHAuthenticationError.invalidChallenge
        }
        let hopIdentities = try hopInputs.map { input -> SSHPublicIdentity in
            try SSHPublicIdentity(blob: XCTUnwrap(Data(base64Encoded: input.hostKey)))
        }
        let methods = ["publickey", "password", "keyboard-interactive"]
        let f = try await SSHWorkspaceTestFixture.make(hostname: "127.0.0.1", port: port, hostBlob: hostKey) { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["authentication_methods"] = methods
            routes[0]["jump_routes"] = hopInputs.indices.map { "hop-\($0)" }
            var trusts = workspace["host_trust"] as! [[String: Any]]
            for (index, hop) in hopInputs.enumerated() {
                var route = routes[0]
                route["id"] = "hop-\(index)"; route["port"] = Int(hop.port)
                route["host_trust_ref"] = "hop-trust-\(index)"; route["jump_routes"] = []
                routes.append(route)
                let identity = hopIdentities[index]
                trusts.append(["id": "hop-trust-\(index)", "host": "127.0.0.1", "port": Int(hop.port),
                    "revoked_keys": [], "pins": [["algorithm": identity.algorithm, "fingerprint": identity.fingerprint]]])
            }
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }
        let states = try await f.store.loadWithLocalPolicy(support: WorkspaceNativeSSHAuthentication.routeSupport)
        let state = try XCTUnwrap(states.first(where: { $0.workspace.id == f.installed.id }))
        XCTAssertEqual(state.credentialIDs, Set(["key"]))
        let options = try WorkspaceSSHRouteOptions(localState: state, machineID: "machine", routeID: "route",
            support: WorkspaceNativeSSHAuthentication.routeSupport)
        XCTAssertEqual(options.hops.map(\.route.id), hopInputs.indices.map { "hop-\($0)" })
        for peer in options.hops + [options.destination] {
            XCTAssertEqual(WorkspaceSSHAuthenticationChoice.choices(for: peer.route, usableCredentialIDs: ["key"]),
                           [.publicKey("key")], "The automatic route still prefers its available imported key")
            XCTAssertEqual(peer.authenticationChoices, [.publicKey("key"), .password, .keyboardInteractive])
        }
        let interactiveDestination = mode == "interactive" || mode == "jump-mixed"
        let selectedAuthentication: WorkspaceSSHAuthenticationChoice = interactiveDestination ? .keyboardInteractive : .password
        let hopChoices: [WorkspaceSSHAuthenticationChoice] = try hopInputs.map {
            switch $0.authentication {
            case "password": return .password
            case "keyboard-interactive": return .keyboardInteractive
            default: throw SSHAuthenticationError.invalidChallenge
            }
        }
        let reviewed = try options.select(authentication: selectedAuthentication, hopAuthentications: hopChoices)
        let selection: WorkspaceSSHConnectionSelection
        if mode == "stale" {
            selection = .init(workspaceID: reviewed.workspaceID, expectedRevision: UUID(),
                machineID: reviewed.machineID, routeID: reviewed.routeID,
                authentication: reviewed.authentication, hops: reviewed.hops)
        } else if mode == "hop-mismatch" {
            selection = .init(workspaceID: reviewed.workspaceID, expectedRevision: reviewed.expectedRevision,
                machineID: reviewed.machineID, routeID: reviewed.routeID, authentication: reviewed.authentication,
                hops: [.init(routeID: "another-hop", authentication: .password)])
        } else { selection = reviewed }
        let prompt = ExplicitSelectionPrompts(fixture: f, refuseDestination: mode == "jump-destination-refused")
        let activity = WorkspaceNetworkActivity()
        let deniedBeforeNetwork = ["stale", "hop-mismatch", "missing-prompter"].contains(mode)
        func connect() async throws -> WorkspaceNativeSSHConnection {
            try await WorkspaceNativeSSHConnection.connect(workspaceID: f.installed.id,
                expectedRevision: f.installed.revision, machineID: "machine", routeID: "route",
                authentication: selectedAuthentication, confirmedByUser: true, store: f.store, broker: f.broker,
                passwordPrompter: prompt,
                keyboardInteractivePrompter: mode == "missing-prompter" ? nil : prompt,
                networkActivity: activity, connectionTimeout: .seconds(5), authenticationTimeout: .seconds(5),
                attachMux: false, selection: selection)
        }
        if deniedBeforeNetwork || mode == "jump-destination-refused" {
            do {
                let owner = try await connect(); await owner.close()
                XCTFail("An invalid or refused explicit choice published a connection")
            } catch {
                if mode == "jump-destination-refused" {
                    XCTAssertEqual(error as? NativeSSHError, .native(-1012))
                    XCTAssertEqual((error as? NativeSSHError)?.isCredentialRefused, false)
                }
            }
        } else {
            let owner = try await connect()
            do {
                XCTAssertEqual(owner.peer.hostKey, hostKey)
                try await owner.validateForPublication()
                let coordinator = try TerminalInputCoordinator()
                let sink = ExplicitSelectionTerminal(identity: owner.identity)
                let pump = try await owner.openTerminal(geometry: .init(columns: 80, rows: 24), coordinator: coordinator) {
                    try await sink.prepare($0)
                }
                let stream = try await coordinator.makeStream(sceneID: UUID())
                let focus = try await coordinator.changeFocus(stream, to: pump.source)
                let bytes = Data("Explicit route → owned terminal.\rEND-SESSION\r".utf8)
                _ = try await coordinator.submit(focus, encodedBytes: bytes)
                let watchdog = Task { try await Task.sleep(for: .seconds(10)); pump.requestStop() }
                defer { watchdog.cancel() }
                let result = try await pump.run(consumer: sink)
                let output = await sink.output, ended = await sink.ended
                XCTAssertEqual(result, .completed(.init(remoteExit: nil, channelClosed: true)))
                XCTAssertEqual(output, Data("ready\r\nstderr\r\n".utf8) + bytes)
                XCTAssertTrue(ended)
                _ = try await coordinator.suspend(focus)
                let outcomes = try await coordinator.pendingOutcomes(pump.source)
                if !outcomes.outcomes.isEmpty { try await coordinator.acknowledgeOutcomes(outcomes) }
                try await coordinator.retire(pump.source)
                await owner.close()
            } catch { await owner.close(); throw error }
        }
        let peers = await prompt.peers
        let expected = deniedBeforeNetwork ? [] : hopInputs.enumerated().map { "hop-\($0.offset):\($0.element.authentication)" }
            + ["route:\(interactiveDestination ? "keyboard-interactive" : "password")"]
        XCTAssertEqual(peers, expected)
        XCTAssertEqual(f.vault.counts.reads, 0)
        XCTAssertEqual(f.vault.counts.signatures, 0)
        XCTAssertEqual(activity.accounting().connections, 0)
    }
}
