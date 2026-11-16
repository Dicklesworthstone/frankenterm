import Foundation
import XCTest
@testable import FrankenTermSSH
@testable import FrankenTermKit
import FrankenTermSSHIntegration

private enum InteractiveAuthorityFailure: Error { case wrongPeer, revoked, wrongChallenge, promptNotReached }

private final class InteractiveTestAuthority: NativeSSHInteractiveAuthentication, @unchecked Sendable {
    private let lock = NSLock()
    private let endpoint: NativeSSHEndpoint, hostKey: Data, mode: String
    private var peer: NativeSSHPeer?
    private var retired = false, revoked = false
    private var prompts = 0
    private var pending: CheckedContinuation<[Data], Error>?

    init(endpoint: NativeSSHEndpoint, hostKey: Data, mode: String) {
        self.endpoint = endpoint; self.hostKey = hostKey; self.mode = mode
    }
    var counts: (prompts: Int, waiting: Bool) { lock.withLock { (prompts, pending != nil) } }
    func account(for peer: NativeSSHPeer) async throws -> NativeSSHInteractiveAccount {
        try lock.withLock {
            guard !retired, !revoked else { throw InteractiveAuthorityFailure.revoked }
            guard self.peer == nil, peer.endpoint == endpoint, peer.hostKey == hostKey else {
                throw InteractiveAuthorityFailure.wrongPeer
            }
            self.peer = peer
            return try NativeSSHInteractiveAccount(user: "operator")
        }
    }
    func responses(for challenge: NativeSSHInteractiveChallenge, peer: NativeSSHPeer) async throws -> [Data] {
        try await validate(peer: peer, account: challenge.account)
        guard challenge.transportID == peer.transportID else { throw InteractiveAuthorityFailure.wrongPeer }
        let answers: [Data]
        if mode == "zero-prompts" {
            guard challenge.round == 1, challenge.name == "Notice", challenge.instruction == "Continue.",
                  challenge.prompts.isEmpty else { throw InteractiveAuthorityFailure.wrongChallenge }
            answers = []
        } else if challenge.round == 1 {
            guard challenge.name == "Sign in", challenge.instruction == "Enter the account password.",
                  challenge.prompts.map(\.text) == ["Account:", "Password:"],
                  challenge.prompts.map(\.echo) == [true, false] else { throw InteractiveAuthorityFailure.wrongChallenge }
            var result = [Data("operator".utf8), Data((mode == "wrong-response" ? "wrong" : "pw-é🔐").utf8)]
            if mode == "extra-response" { result.append(Data("extra".utf8)) }
            if mode == "oversize-response" { result[1] = Data(repeating: 65, count: 1025) }
            answers = result
        } else {
            guard challenge.round == 2, challenge.name == "Verification", challenge.instruction == "Enter the one-time code.",
                  challenge.prompts.map(\.text) == ["Verification code:"],
                  challenge.prompts.map(\.echo) == [false] else { throw InteractiveAuthorityFailure.wrongChallenge }
            answers = [Data("654321".utf8)]
        }
        return try await withCheckedThrowingContinuation { continuation in
            let result: Result<[Data], Error>? = lock.withLock {
                guard !retired, !revoked else { return .failure(InteractiveAuthorityFailure.revoked) }
                guard challenge.round == UInt32(prompts + 1) else { return .failure(InteractiveAuthorityFailure.wrongChallenge) }
                prompts += 1
                if ["cancel", "close-prompt", "timeout", "revoke-prompt"].contains(mode) {
                    pending = continuation; return nil
                }
                return .success(answers)
            }
            if let result { continuation.resume(with: result) }
        }
    }
    func validate(peer: NativeSSHPeer, account: NativeSSHInteractiveAccount) async throws {
        try lock.withLock {
            guard !retired, !revoked else { throw InteractiveAuthorityFailure.revoked }
            guard peer == self.peer, peer.endpoint == endpoint, peer.hostKey == hostKey, account.user == "operator" else {
                throw InteractiveAuthorityFailure.wrongPeer
            }
        }
    }
    func cancel(transportID: UUID) {
        let waiting: CheckedContinuation<[Data], Error>? = lock.withLock {
            guard peer == nil || peer?.transportID == transportID else { return nil }
            retired = true
            let value = pending; pending = nil; return value
        }
        waiting?.resume(throwing: CancellationError())
    }
    func revoke(releasingPrompt: Bool = false) {
        let waiting: CheckedContinuation<[Data], Error>? = lock.withLock {
            revoked = true
            guard releasingPrompt else { return nil }
            let value = pending; pending = nil; return value
        }
        waiting?.resume(returning: [Data("operator".utf8), Data("pw-é🔐".utf8)])
    }
}

private actor InteractiveWorkspacePrompt: WorkspaceSSHKeyboardInteractivePrompting {
    struct Route: Sendable {
        let endpoint: NativeSSHEndpoint, hostKey: WorkspaceDocument.HostKey
        let transitForRouteID: String?
    }
    private let workspace: UUID, revision: UUID, routes: [String: Route]
    private var calls: [(route: String, transport: UUID, attempt: UUID, round: UInt32)] = []
    private var identifiers = Set<UUID>()
    init(workspace: UUID, revision: UUID, routes: [String: Route]) {
        self.workspace = workspace; self.revision = revision; self.routes = routes
    }
    func responses(for request: WorkspaceSSHKeyboardInteractiveRequest) async throws -> [Data] {
        let authorization = request.authorization
        guard let route = routes[authorization.routeID], authorization.workspaceID == workspace,
              authorization.workspaceRevision == revision, authorization.user == "operator",
              authorization.host == route.endpoint.host, authorization.port == route.endpoint.port,
              authorization.presentedHostKey == route.hostKey,
              authorization.transitForRouteID == route.transitForRouteID,
              identifiers.insert(request.id).inserted else { throw InteractiveAuthorityFailure.wrongPeer }
        let previous = calls.last { $0.route == authorization.routeID }
        guard request.challenge.round == (previous?.round ?? 0) + 1,
              previous == nil || (previous?.transport == request.transportID && previous?.attempt == request.attemptID) else {
            throw InteractiveAuthorityFailure.wrongChallenge
        }
        calls.append((authorization.routeID, request.transportID, request.attemptID, request.challenge.round))
        if request.challenge.round == 1 {
            guard request.challenge.name == "Sign in", request.challenge.instruction == "Enter the account password.",
                  request.challenge.prompts.map(\.text) == ["Account:", "Password:"],
                  request.challenge.prompts.map(\.echo) == [true, false] else { throw InteractiveAuthorityFailure.wrongChallenge }
            return [Data("operator".utf8), Data("pw-é🔐".utf8)]
        }
        guard request.challenge.round == 2, request.challenge.name == "Verification",
              request.challenge.instruction == "Enter the one-time code.",
              request.challenge.prompts.map(\.text) == ["Verification code:"],
              request.challenge.prompts.map(\.echo) == [false] else { throw InteractiveAuthorityFailure.wrongChallenge }
        return [Data("654321".utf8)]
    }
    nonisolated func cancel(requestID: UUID) {}
    func summary() -> (routes: [String], transports: Int, attempts: Int) {
        (calls.map { $0.route }, Set(calls.map { $0.transport }).count, Set(calls.map { $0.attempt }).count)
    }
}

private actor InteractiveWorkspaceSink: TerminalSessionConsumer {
    private let identity: TerminalConnectionIdentity
    private var prepared = false, ended = false
    private var bytes = Data()
    init(identity: TerminalConnectionIdentity) { self.identity = identity }
    func prepare(_ identity: TerminalConnectionIdentity) throws {
        guard identity == self.identity, !prepared else { throw TerminalInputError.stale }
        prepared = true
    }
    func receive(_ update: TerminalSessionUpdate, source: TerminalSource) throws {
        guard prepared, source.identity == identity, update.output.count <= 16_384,
              bytes.count + update.output.count <= 65_536 else { throw TerminalInputError.stale }
        bytes += update.output; ended = ended || update.endOfOutput
    }
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) throws {
        guard prepared, source.identity == identity, outcomes.count <= 64,
              outcomes.allSatisfy({ $0.receipt.source == source }) else { throw TerminalInputError.stale }
    }
    func summary() -> (Data, Bool) { (bytes, ended) }
}

final class NativeInteractiveAuthenticationTests: XCTestCase, @unchecked Sendable {
    private func integer(_ value: UInt32) -> Data {
        Data([UInt8(truncatingIfNeeded: value >> 24), UInt8(truncatingIfNeeded: value >> 16),
              UInt8(truncatingIfNeeded: value >> 8), UInt8(truncatingIfNeeded: value)])
    }
    private func field(_ value: Data) -> Data { integer(UInt32(value.count)) + value }
    private func packet(name: Data = Data("Sign in".utf8), instruction: Data = Data("Verify this account.".utf8),
                        prompts: [(Data, UInt8)] = [(Data("Code:".utf8), 0)]) -> Data {
        var result = field(name) + field(instruction) + integer(UInt32(prompts.count))
        for (prompt, echo) in prompts { result.append(echo); result += field(prompt) }
        return result
    }
    private func challenge(_ bytes: Data, round: UInt32 = 1) throws -> NativeSSHInteractiveChallenge {
        try NativeSSHInteractiveChallenge(bytes: bytes, round: round, transportID: UUID(),
            account: NativeSSHInteractiveAccount(user: "operator"))
    }

    func testInteractiveAccountRejectsInvalidUsers() throws {
        for user in ["", "bad\nuser", "bad\0user", String(repeating: "a", count: 257)] {
            XCTAssertThrowsError(try NativeSSHInteractiveAccount(user: user))
        }
        XCTAssertEqual(try NativeSSHInteractiveAccount(user: "operator").user, "operator")
    }

    func testChallengePreservesUTF8EchoAndEmptyResponses() throws {
        let value = try challenge(packet(name: Data("Connexion é".utf8),
            prompts: [(Data("Account:".utf8), 1), (Data("Code 🔑:".utf8), 0)]), round: 4)
        XCTAssertEqual(value.round, 4)
        XCTAssertEqual(value.name, "Connexion é")
        XCTAssertEqual(value.prompts.map(\.text), ["Account:", "Code 🔑:"])
        XCTAssertEqual(value.prompts.map(\.echo), [true, false])
        XCTAssertEqual(try value.encodedResponses([Data("alice".utf8), Data()]),
            integer(2) + field(Data("alice".utf8)) + field(Data()))
        XCTAssertEqual(try challenge(packet(prompts: [])).encodedResponses([]), integer(0))
    }

    func testChallengeRejectsMalformedAndOversizedServerFields() throws {
        let valid = packet()
        for bytes in [Data(valid.dropLast()), valid + Data([0]), integer(UInt32.max),
                      packet(name: Data(repeating: 65, count: 257)),
                      packet(instruction: Data(repeating: 65, count: 1025)),
                      packet(prompts: [(Data(repeating: 65, count: 1025), 0)]),
                      packet(prompts: Array(repeating: (Data(), UInt8(0)), count: 9)),
                      packet(prompts: [(Data(), 2)]), packet(name: Data([0xff])),
                      packet(name: Data([0])), Data(repeating: 0, count: 8193)] {
            XCTAssertThrowsError(try challenge(bytes))
        }
        for round in [UInt32(0), 5, .max] { XCTAssertThrowsError(try challenge(valid, round: round)) }
    }

    func testResponsesAreExactBoundedAndNeverKeyRetryEvidence() throws {
        let value = try challenge(packet())
        for responses in [[], [Data(), Data()], [Data(repeating: 65, count: 1025)], [Data([0])], [Data([0xff])]] as [[Data]] {
            XCTAssertThrowsError(try value.encodedResponses(responses))
        }
        XCTAssertEqual(try value.encodedResponses([Data(repeating: 65, count: 1024)]).count, 1032)
        let failure = NativeSSHError.native(-1013)
        XCTAssertTrue(failure.isMachineAnswer)
        XCTAssertFalse(failure.isCredentialRefused)
        XCTAssertFalse(failure.isUnreachable)
    }

    /// Tests/Interop/test_interactive.py supplies an independent real libssh
    /// server and checks the exact requests and terminal effects it observes.
    func testLiveInteractivePromptsAndTerminalOwnership() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let port = env["FT_INTERACTIVE_TEST_PORT"].flatMap(UInt16.init),
              let host = env["FT_INTERACTIVE_TEST_HOSTKEY"].flatMap({ Data(base64Encoded: $0) }),
              let mode = env["FT_INTERACTIVE_TEST_MODE"] else {
            throw XCTSkip("Requires the independent keyboard-interactive SSH interop fixture")
        }
        let endpoint = try NativeSSHEndpoint(host: "127.0.0.1", port: port)
        var expectedHost = host
        if mode == "wrong-host" { expectedHost[expectedHost.startIndex] ^= 1 }
        let authority = InteractiveTestAuthority(endpoint: endpoint, hostKey: expectedHost, mode: mode)
        var ancestor: InteractiveTestAuthority?
        let session: NativeSSHSession
        if mode.hasPrefix("forward-") {
            let hopPort = try XCTUnwrap(env["FT_INTERACTIVE_TEST_HOP_PORT"].flatMap(UInt16.init))
            let hopHost = try XCTUnwrap(env["FT_INTERACTIVE_TEST_HOP_HOSTKEY"].flatMap { Data(base64Encoded: $0) })
            let hopEndpoint = try NativeSSHEndpoint(host: "127.0.0.1", port: hopPort)
            let hopAuthority = InteractiveTestAuthority(endpoint: hopEndpoint, hostKey: hopHost, mode: "success")
            let hop = try await NativeSSHConnector.connect(to: hopEndpoint, timeout: .seconds(5))
            do {
                _ = try await hop.authenticate(usingInteractive: hopAuthority, timeout: .seconds(5))
                session = try await hop.forward(to: endpoint, timeout: .seconds(5))
                await hop.close()
                ancestor = hopAuthority
            } catch { await hop.close(); throw error }
        } else {
            session = try await NativeSSHConnector.connect(to: endpoint, timeout: .seconds(5))
        }
        do {
            if ["cancel", "close-prompt", "timeout", "revoke-prompt"].contains(mode) {
                let work = Task {
                    try await session.authenticate(usingInteractive: authority,
                        timeout: mode == "timeout" ? .seconds(1) : .seconds(5))
                }
                let deadline = ContinuousClock.now + .seconds(3)
                while !authority.counts.waiting, ContinuousClock.now < deadline {
                    try await Task.sleep(for: .milliseconds(5))
                }
                guard authority.counts.waiting else {
                    work.cancel(); _ = try? await work.value
                    XCTFail("Fixture did not reach its prompt before the bounded wait")
                    throw InteractiveAuthorityFailure.promptNotReached
                }
                do { _ = try await session.accounting(); XCTFail("Worker-owned accounting was read concurrently") }
                catch { XCTAssertEqual(error as? NativeSSHError, .busy) }
                if mode == "cancel" { work.cancel() }
                if mode == "close-prompt" { await session.close() }
                if mode == "revoke-prompt" { authority.revoke(releasingPrompt: true) }
                do { _ = try await work.value; XCTFail("A retired prompt authenticated") }
                catch {
                    if mode == "cancel" { XCTAssertTrue(error is CancellationError) }
                    if mode == "timeout" { XCTAssertEqual(error as? NativeSSHError, .authenticationTimedOut) }
                    if mode == "revoke-prompt" { XCTAssertTrue(error is InteractiveAuthorityFailure) }
                }
                XCTAssertEqual(authority.counts.prompts, 1)
            } else if ["wrong-host", "unsupported", "none", "immediate", "partial", "wrong-response", "extra-response", "oversize-response"].contains(mode) {
                do { _ = try await session.authenticate(usingInteractive: authority, timeout: .seconds(5)); XCTFail("A refused exchange authenticated") }
                catch {
                    if mode == "wrong-host" { XCTAssertTrue(error is InteractiveAuthorityFailure) }
                    else { XCTAssertTrue(error is NativeSSHError) }
                    XCTAssertFalse((error as? NativeSSHError)?.isCredentialRefused == true)
                }
                let expected = ["wrong-host", "unsupported", "none", "immediate"].contains(mode) ? 0 : mode == "partial" ? 2 : 1
                XCTAssertEqual(authority.counts.prompts, expected)
            } else {
                let peer = try await session.authenticate(usingInteractive: authority, timeout: .seconds(5))
                XCTAssertEqual(peer.hostKey, host)
                XCTAssertEqual(authority.counts.prompts, mode == "zero-prompts" ? 1 : 2)
                if mode == "revoke-live" || mode == "forward-revoked" {
                    if mode == "revoke-live" { authority.revoke() } else { ancestor?.revoke() }
                    do { try await session.openShell(geometry: .init(columns: 80, rows: 24)); XCTFail("Revoked interactive authority opened a shell") }
                    catch { XCTAssertTrue(error is InteractiveAuthorityFailure) }
                } else {
                    try await session.openShell(geometry: .init(columns: 80, rows: 24))
                    let input = Data("Interactive → owned terminal.\rEND-SESSION\r".utf8)
                    let admission = try await session.admit(input)
                    XCTAssertEqual(admission, .accepted(input.count))
                    var output = Data(), completed = false
                    let deadline = ContinuousClock.now + .seconds(5)
                    while !completed, ContinuousClock.now < deadline {
                        let event = try await session.service(timeout: .seconds(1))
                        output += event.output; completed = event.endOfOutput
                    }
                    XCTAssertTrue(completed)
                    XCTAssertEqual(output, Data("ready\r\nstderr\r\n".utf8) + input)
                }
            }
            await session.close()
            let stats = try await session.accounting()
            XCTAssertEqual(stats.allocatedBytes, 0)
            XCTAssertFalse(authority.counts.waiting)
        } catch { await session.close(); throw error }
    }

    @MainActor
    func testLiveWorkspaceInteractiveFactory() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let port = env["FT_INTERACTIVE_TEST_PORT"].flatMap(UInt16.init),
              let host = env["FT_INTERACTIVE_TEST_HOSTKEY"].flatMap({ Data(base64Encoded: $0) }),
              let mode = env["FT_INTERACTIVE_TEST_MODE"], mode.hasPrefix("workspace-") else {
            throw XCTSkip("Requires the independent workspace keyboard-interactive fixture")
        }
        let jumped = mode.contains("jump"), revoked = mode.hasSuffix("revoked")
        let hostname = jumped ? "target.internal.invalid" : "127.0.0.1"
        var hopEndpoint: NativeSSHEndpoint?, hopKey: WorkspaceDocument.HostKey?
        if jumped {
            let hopPort = try XCTUnwrap(env["FT_INTERACTIVE_TEST_HOP_PORT"].flatMap(UInt16.init))
            let hopHost = try XCTUnwrap(env["FT_INTERACTIVE_TEST_HOP_HOSTKEY"].flatMap { Data(base64Encoded: $0) })
            let identity = try SSHPublicIdentity(blob: hopHost)
            hopEndpoint = try NativeSSHEndpoint(host: "127.0.0.1", port: hopPort)
            hopKey = try WorkspaceDocument.HostKey(algorithm: identity.algorithm, fingerprint: identity.fingerprint)
        }
        let fixture = try await SSHWorkspaceTestFixture.make(hostname: hostname, port: port, hostBlob: host) { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["authentication_methods"] = ["keyboard-interactive"]
            routes[0]["credential_refs"] = [] as [String]
            if let hopEndpoint, let hopKey {
                var hop = routes[0]
                hop["id"] = "hop-route"; hop["host"] = hopEndpoint.host; hop["port"] = Int(hopEndpoint.port)
                hop["host_trust_ref"] = "hop-trust"; hop["session_creation"] = nil
                routes[0]["jump_routes"] = ["hop-route"]
                routes.append(hop)
                var trust = workspace["host_trust"] as! [[String: Any]]
                trust.append(["id": "hop-trust", "host": hopEndpoint.host, "port": Int(hopEndpoint.port),
                    "revoked_keys": [], "pins": [["algorithm": hopKey.algorithm, "fingerprint": hopKey.fingerprint]]])
                workspace["host_trust"] = trust
            }
            workspace["routes"] = routes
        }
        var promptRoutes: [String: InteractiveWorkspacePrompt.Route] = ["route": .init(endpoint: fixture.endpoint,
            hostKey: fixture.host, transitForRouteID: nil)]
        if let hopEndpoint, let hopKey {
            promptRoutes["hop-route"] = .init(endpoint: hopEndpoint, hostKey: hopKey, transitForRouteID: "route")
        }
        let prompt = InteractiveWorkspacePrompt(workspace: fixture.installed.id, revision: fixture.installed.revision, routes: promptRoutes)
        let activity = WorkspaceNetworkActivity()
        let owner = try await WorkspaceNativeSSHConnection.connect(workspaceID: fixture.installed.id,
            expectedRevision: fixture.installed.revision, machineID: "machine", routeID: "route",
            authentication: .keyboardInteractive, confirmedByUser: true, store: fixture.store, broker: fixture.broker,
            keyboardInteractivePrompter: prompt, networkActivity: activity,
            connectionTimeout: .seconds(5), authenticationTimeout: .seconds(5))
        do {
            XCTAssertEqual(owner.peer.hostKey, host)
            XCTAssertEqual(owner.workspaceRevision, fixture.installed.revision)
            XCTAssertEqual(activity.accounting().connections, 1)
            let prompts = await prompt.summary()
            XCTAssertEqual(prompts.routes, jumped ? ["hop-route", "hop-route", "route", "route"] : ["route", "route"])
            XCTAssertEqual(prompts.transports, jumped ? 2 : 1)
            XCTAssertEqual(prompts.attempts, jumped ? 2 : 1)
            XCTAssertEqual(fixture.vault.counts.reads, 0)
            XCTAssertEqual(fixture.vault.counts.signatures, 0)
            let coordinator = try TerminalInputCoordinator(), sink = InteractiveWorkspaceSink(identity: owner.identity)
            if revoked {
                try await fixture.store.revokeHostLocally(workspaceID: fixture.installed.id,
                    trustID: jumped ? "hop-trust" : "trust", key: hopKey ?? fixture.host)
                do {
                    _ = try await owner.openTerminal(geometry: .init(columns: 80, rows: 24), coordinator: coordinator) {
                        try await sink.prepare($0)
                    }
                    XCTFail("A revoked interactive route opened a terminal")
                } catch { XCTAssertTrue(error is SSHAuthenticationError) }
            } else {
                let pump = try await owner.openTerminal(geometry: .init(columns: 80, rows: 24), coordinator: coordinator) {
                    try await sink.prepare($0)
                }
                let stream = try await coordinator.makeStream(sceneID: UUID())
                let focus = try await coordinator.changeFocus(stream, to: pump.source)
                let bytes = Data("Workspace interactive → owned terminal.\rEND-SESSION\r".utf8)
                _ = try await coordinator.submit(focus, encodedBytes: bytes)
                let run = Task { try await pump.run(consumer: sink) }
                let watchdog = Task { try await Task.sleep(for: .seconds(8)); pump.requestStop() }
                defer { watchdog.cancel() }
                let result = try await run.value, summary = await sink.summary()
                XCTAssertEqual(result, .completed(.init(remoteExit: nil, channelClosed: true)))
                XCTAssertEqual(summary.0, Data("ready\r\nstderr\r\n".utf8) + bytes)
                XCTAssertTrue(summary.1)
                _ = try await coordinator.suspend(focus)
                let remaining = try await coordinator.pendingOutcomes(pump.source)
                if !remaining.outcomes.isEmpty { try await coordinator.acknowledgeOutcomes(remaining) }
                try await coordinator.retire(pump.source)
            }
            await owner.close()
            XCTAssertEqual(activity.accounting().connections, 0)
            let offline = try activity.beginOfflineRestore()
            withExtendedLifetime(offline) {}
        } catch { await owner.close(); throw error }
    }
}
