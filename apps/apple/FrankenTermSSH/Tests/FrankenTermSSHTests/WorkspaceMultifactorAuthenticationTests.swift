import Foundation
import XCTest
@testable import FrankenTermKit
@testable import FrankenTermSSH
@testable import FrankenTermSSHIntegration

private actor WorkspaceMultifactorPrompt: WorkspaceSSHKeyboardInteractivePrompting {
    private(set) var requests: [WorkspaceSSHKeyboardInteractiveRequest] = []
    private let beforeAnswer: @Sendable (WorkspaceSSHKeyboardInteractiveRequest) async throws -> Void
    init(beforeAnswer: @escaping @Sendable (WorkspaceSSHKeyboardInteractiveRequest) async throws -> Void = { _ in }) {
        self.beforeAnswer = beforeAnswer
    }
    func responses(for request: WorkspaceSSHKeyboardInteractiveRequest) async throws -> [Data] {
        requests.append(request)
        try await beforeAnswer(request)
        guard request.challenge.prompts.count == 1, request.challenge.prompts[0].echo == false else {
            throw SSHAuthenticationError.invalidChallenge
        }
        switch request.challenge.round {
        case 1:
            guard request.challenge.name == "Verification", request.challenge.instruction == "Enter the one-time code.",
                  request.challenge.prompts[0].text == "Verification code:" else { throw SSHAuthenticationError.invalidChallenge }
            return [Data("654321".utf8)]
        case 2:
            guard request.challenge.name == "Recovery", request.challenge.instruction == "Enter the recovery token.",
                  request.challenge.prompts[0].text == "Recovery token:" else { throw SSHAuthenticationError.invalidChallenge }
            return [Data("approved".utf8)]
        default: throw SSHAuthenticationError.invalidChallenge
        }
    }
    nonisolated func cancel(requestID: UUID) {}
}

private actor WorkspaceMultifactorSink: TerminalSessionConsumer {
    let identity: TerminalConnectionIdentity
    private(set) var prepared = false
    private(set) var output = Data()
    private(set) var ended = false
    init(identity: TerminalConnectionIdentity) { self.identity = identity }
    func prepare(_ identity: TerminalConnectionIdentity) throws {
        guard !prepared, identity == self.identity else { throw TerminalInputError.stale }
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
final class WorkspaceMultifactorAuthenticationTests: XCTestCase {
    private func fixture(methods: [String] = ["publickey", "keyboard-interactive"]) async throws -> SSHWorkspaceTestFixture {
        try await .make { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["authentication_methods"] = methods; workspace["routes"] = routes
        }
    }
    private func signed(_ f: SSHWorkspaceTestFixture, transport: UUID) async throws -> WorkspaceNativeSSHAuthentication {
        let authority = f.authority(transport: transport), peer = f.peer(transport: transport)
        let identity = try await authority.identity(for: peer)
        let typed = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(repeating: 9, count: 32),
            user: identity.user, publicBlob: identity.publicKey)
        let challenge = try NativeSSHChallenge(bytes: typed.signedData, transportID: transport, identity: identity)
        let signature = try await authority.signature(for: challenge, peer: peer)
        XCTAssertFalse(signature.isEmpty)
        try await authority.validate(peer: peer, identity: identity)
        return authority
    }
    private func interactive(_ f: SSHWorkspaceTestFixture, transport: UUID,
        key: WorkspaceNativeSSHAuthentication, prompt: WorkspaceMultifactorPrompt) -> WorkspaceNativeSSHInteractiveAuthentication {
        .init(broker: f.broker, prompter: prompt, workspaceID: f.installed.id, revision: f.installed.revision,
            routeID: "route", transportID: transport, endpoint: f.endpoint, continuingPublicKey: key)
    }
    private func challenge(transport: UUID, account: NativeSSHInteractiveAccount, round: UInt32 = 1) throws -> NativeSSHInteractiveChallenge {
        func field(_ value: String) -> Data {
            let bytes = Data(value.utf8)
            var length = UInt32(bytes.count).bigEndian
            return withUnsafeBytes(of: &length) { Data($0) } + bytes
        }
        let first = round == 1
        let bytes = field(first ? "Verification" : "Recovery")
            + field(first ? "Enter the one-time code." : "Enter the recovery token.")
            + Data([0, 0, 0, 1, 0]) + field(first ? "Verification code:" : "Recovery token:")
        return try .init(bytes: bytes, round: round, transportID: transport, account: account)
    }
    private func refuse(_ body: () async throws -> Void, file: StaticString = #filePath, line: UInt = #line) async {
        do { try await body(); XCTFail("A retired or unauthorized factor was accepted", file: file, line: line) } catch {}
    }

    func testPublicKeyOrPasswordPermissionDoesNotGrantKeyboardInteractiveContinuation() async throws {
        for methods in [["publickey"], ["publickey", "password"]] {
            let f = try await fixture(methods: methods), transport = UUID(), prompt = WorkspaceMultifactorPrompt()
            let key = try await signed(f, transport: transport)
            let second = interactive(f, transport: transport, key: key, prompt: prompt)
            await refuse { _ = try await second.account(for: f.peer(transport: transport)) }
            let requests = await prompt.requests
            XCTAssertTrue(requests.isEmpty)
            XCTAssertEqual(f.vault.counts.signatures, 1)
        }
    }

    func testContinuationRequiresTheSignedKeyAndExactWorkspaceRouteHopAndPeer() async throws {
        for variant in 0..<7 {
            let f = try await fixture(), transport = UUID(), prompt = WorkspaceMultifactorPrompt()
            let key: WorkspaceNativeSSHAuthentication
            if variant < 2 {
                key = f.authority(transport: transport)
                if variant == 1 { _ = try await key.identity(for: f.peer(transport: transport)) }
            } else { key = try await signed(f, transport: transport) }
            let second = WorkspaceNativeSSHInteractiveAuthentication(broker: f.broker, prompter: prompt,
                workspaceID: variant == 2 ? UUID() : f.installed.id,
                revision: variant == 3 ? UUID() : f.installed.revision,
                routeID: variant == 4 ? "other-route" : "route",
                transportID: variant == 5 ? UUID() : transport, endpoint: f.endpoint,
                transitForRouteID: variant == 6 ? "destination" : nil, continuingPublicKey: key)
            await refuse { _ = try await second.account(for: f.peer(transport: transport)) }
            let requests = await prompt.requests
            XCTAssertTrue(requests.isEmpty)
            XCTAssertEqual(f.vault.counts.signatures, variant < 2 ? 0 : 1)
        }
    }

    func testTwoRoundsRetainOneSignedKeyAndRetirePromptPermissionAfterCompletion() async throws {
        let f = try await fixture(), transport = UUID(), prompt = WorkspaceMultifactorPrompt()
        let key = try await signed(f, transport: transport), peer = f.peer(transport: transport)
        let second = interactive(f, transport: transport, key: key, prompt: prompt)
        let account = try await second.account(for: peer)
        await refuse { try await second.authenticationCompleted() }
        for round in [UInt32(1), 2] {
            let answers = try await second.responses(for: challenge(transport: transport, account: account, round: round), peer: peer)
            XCTAssertEqual(answers, [Data((round == 1 ? "654321" : "approved").utf8)])
            try await second.validate(peer: peer, account: account)
        }
        try await second.authenticationCompleted()
        try await second.validateCurrent()
        try await key.validateCurrent()
        await refuse { _ = try await second.responses(for: self.challenge(transport: transport, account: account, round: 3), peer: peer) }
        let requests = await prompt.requests
        XCTAssertEqual(requests.map(\.challenge.round), [1, 2])
        XCTAssertEqual(Set(requests.map(\.attemptID)).count, 1)
        XCTAssertEqual(Set(requests.map(\.id)).count, 2)
        XCTAssertTrue(requests.allSatisfy { $0.transportID == transport && $0.authorization.routeID == "route" })
        XCTAssertEqual(f.vault.counts.reads, 1); XCTAssertEqual(f.vault.counts.signatures, 1)
        let busy = await f.broker.isBusy
        XCTAssertFalse(busy)
    }

    func testRevokingTheFirstKeyWhileAnsweringDiscardsCorrectSecondFactor() async throws {
        let f = try await fixture(), transport = UUID()
        let prompt = WorkspaceMultifactorPrompt { _ in
            try await f.store.revokeLocally(fingerprint: f.archive.workspace.credentials[0].fingerprint)
        }
        let key = try await signed(f, transport: transport), peer = f.peer(transport: transport)
        let second = interactive(f, transport: transport, key: key, prompt: prompt)
        let account = try await second.account(for: peer)
        await refuse { _ = try await second.responses(for: self.challenge(transport: transport, account: account), peer: peer) }
        await refuse { try await second.authenticationCompleted() }
        let requests = await prompt.requests
        XCTAssertEqual(requests.count, 1)
        // The second method remains independently permitted. Its permission
        // must not erase the revoked first factor of this particular login.
        _ = try await f.store.authorizeSSHKeyboardInteractiveAuthentication(workspaceID: f.installed.id,
            expectedRevision: f.installed.revision, routeID: "route", presentedHostKey: f.host,
            support: WorkspaceNativeSSHAuthentication.routeSupport)
    }

    func testKeyRevocationWhileQueuedPreventsTheSecondFactorUIFromOpening() async throws {
        let f = try await fixture(), transport = UUID(), prompt = WorkspaceMultifactorPrompt()
        let key = try await signed(f, transport: transport), peer = f.peer(transport: transport)
        let second = interactive(f, transport: transport, key: key, prompt: prompt)
        let account = try await second.account(for: peer)
        let held = try await f.broker.prepare(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
            routeID: "route", credentialID: "key", presentedHostKey: f.host, transportID: UUID(),
            sessionIdentifier: Data(repeating: 4, count: 32), publicBlob: f.publicBlob)
        let waiting = Task { try await second.responses(for: self.challenge(transport: transport, account: account), peer: peer) }
        defer { waiting.cancel() }
        let deadline = ContinuousClock.now + .seconds(5)
        while await f.broker.queuedRequests != 1, ContinuousClock.now < deadline { await Task.yield() }
        let queued = await f.broker.queuedRequests
        XCTAssertEqual(queued, 1)
        try await f.store.revokeLocally(fingerprint: f.archive.workspace.credentials[0].fingerprint)
        await f.broker.cancel(requestID: held.id)
        await refuse { _ = try await waiting.value }
        let requests = await prompt.requests, busy = await f.broker.isBusy
        XCTAssertTrue(requests.isEmpty)
        XCTAssertFalse(busy)
    }

    func testCompletedMultifactorAuthorityRetainsBothDurableFactorsAndExactCancellation() async throws {
        for variant in 0..<5 {
            let f = try await fixture(), transport = UUID(), prompt = WorkspaceMultifactorPrompt()
            let key = try await signed(f, transport: transport), peer = f.peer(transport: transport)
            let second = interactive(f, transport: transport, key: key, prompt: prompt)
            let account = try await second.account(for: peer)
            _ = try await second.responses(for: challenge(transport: transport, account: account), peer: peer)
            try await second.authenticationCompleted()
            second.cancel(transportID: UUID())
            await f.broker.setPasswordForeground(false)
            try await second.validateCurrent()
            await f.broker.setPasswordForeground(true)
            switch variant {
            case 0: try await f.store.revokeLocally(fingerprint: f.archive.workspace.credentials[0].fingerprint)
            case 1: try await f.store.revokeHostLocally(workspaceID: f.installed.id, trustID: "trust", key: f.host)
            case 2: await f.broker.suspend(); await f.broker.resume()
            case 3:
                let replacement = try AirGapArchive(workspace: f.archive.workspace.reissuingBundle(id: "replacement"),
                    keys: [], using: SSHWorkspaceTestCryptography())
                _ = try await f.store.install(replacement, envelopeDigest: Data(repeating: 3, count: 32),
                    expectedRevision: f.installed.revision, sourceAndContentsConfirmed: true,
                    securityChangesConfirmed: true, associateWithExistingWorkspace: true)
            default: second.cancel(transportID: transport)
            }
            await refuse { try await second.validateCurrent() }
            await refuse { try await key.validateCurrent() }
            await refuse { _ = try await second.responses(for: self.challenge(transport: transport, account: account, round: 2), peer: peer) }
        }
    }

    /// The independent loopback SSH server proves the actual factory's policy,
    /// signed-key partial-success transition, prompt ownership and jump path.
    func testLiveWorkspacePublicKeyInteractiveFactory() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard let port = environment["FT_MFA_TEST_PORT"].flatMap(UInt16.init),
              let host = environment["FT_MFA_TEST_HOSTKEY"].flatMap({ Data(base64Encoded: $0) }),
              let mode = environment["FT_MFA_TEST_MODE"] else {
            throw XCTSkip("Run Tests/Interop/test_multifactor.py with the built XCTest bundle and patched native driver.")
        }
        guard NativeSSHSession.supportsPublicKeyInteractive else { throw SSHAuthenticationError.unavailable }
        let jumped = mode == "workspace-jump" || mode == "workspace-hop-key-revoked" || mode == "workspace-jump-destination-refused"
        let hopPort = environment["FT_MFA_TEST_HOP_PORT"].flatMap(UInt16.init)
        let hopHost = environment["FT_MFA_TEST_HOP_HOSTKEY"].flatMap { Data(base64Encoded: $0) }
        var additional: [PrivateKeyRecord] = []
        var hopFingerprint: String?
        if jumped {
            let independent = try await SSHWorkspaceTestFixture.make(seed: Data(repeating: 41, count: 32))
            let record = try XCTUnwrap(independent.archive.keys.first)
            additional = [.init(credentialID: "hop-key", container: record.container, publicBlob: record.publicBlob)]
            hopFingerprint = try SSHPublicIdentity(blob: record.publicBlob).fingerprint
        }
        let hopPin = try hopHost.map { try SSHPublicIdentity(blob: $0) }
        if jumped { XCTAssertNotNil(hopPort); XCTAssertNotNil(hopPin) }
        let f = try await SSHWorkspaceTestFixture.make(hostname: jumped ? "target.internal.invalid" : "127.0.0.1",
            port: port, hostBlob: host, additionalKeys: additional) { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["authentication_methods"] = mode == "workspace-disallowed"
                ? ["publickey", "password"] : ["publickey", "keyboard-interactive"]
            if jumped, let hopPort, let hopPin, let hopFingerprint {
                var hop = routes[0]
                hop["id"] = "hop"; hop["host"] = "127.0.0.1"; hop["port"] = Int(hopPort)
                hop["host_trust_ref"] = "hop-trust"; hop["credential_refs"] = ["hop-key"]
                routes[0]["jump_routes"] = ["hop"]; routes.append(hop)
                var credentials = workspace["credentials"] as! [[String: Any]]
                credentials.append(["id": "hop-key", "algorithm": "ssh-ed25519", "public_fingerprint": hopFingerprint,
                    "requirement": "matching_private_key", "private_material": "not_included"])
                workspace["credentials"] = credentials
                var trusts = workspace["host_trust"] as! [[String: Any]]
                trusts.append(["id": "hop-trust", "host": "127.0.0.1", "port": Int(hopPort), "revoked_keys": [],
                    "pins": [["algorithm": hopPin.algorithm, "fingerprint": hopPin.fingerprint]]])
                workspace["host_trust"] = trusts
            }
            workspace["routes"] = routes
        }
        let prompt = WorkspaceMultifactorPrompt { request in
            guard request.authorization.workspaceID == f.installed.id,
                  request.authorization.workspaceRevision == f.installed.revision,
                  request.authorization.user == "operator",
                  let route = f.archive.workspace.routes.first(where: { $0.id == request.authorization.routeID }),
                  request.authorization.host == route.host, request.authorization.port == route.port,
                  request.authorization.transitForRouteID == (route.id == "hop" ? "route" : nil) else {
                throw SSHAuthenticationError.stale
            }
            if mode == "workspace-key-revoked-prompt" {
                try await f.store.revokeLocally(fingerprint: f.archive.workspace.credentials[0].fingerprint)
            }
        }
        let activity = WorkspaceNetworkActivity()
        func connect() async throws -> WorkspaceNativeSSHConnection {
            try await WorkspaceNativeSSHConnection.connect(workspaceID: f.installed.id,
                expectedRevision: f.installed.revision, machineID: "machine", routeID: "route",
                authentication: .publicKey("key"), confirmedByUser: true, store: f.store, broker: f.broker,
                keyboardInteractivePrompter: mode == "workspace-no-prompter" ? nil : prompt,
                networkActivity: activity, connectionTimeout: .seconds(5), authenticationTimeout: .seconds(5), attachMux: false)
        }
        if ["workspace-disallowed", "workspace-no-prompter", "workspace-key-revoked-prompt", "workspace-jump-destination-refused"].contains(mode) {
            do {
                let owner = try await connect(); await owner.close()
                XCTFail("The denied multifactor exchange published a connection")
            } catch {
                if mode == "workspace-jump-destination-refused" {
                    XCTAssertEqual(error as? WorkspaceSSHConnectionError, .promptedHopRetryRequired)
                } else { XCTAssertFalse((error as? NativeSSHError)?.isCredentialRefused == true) }
            }
            let requests = await prompt.requests
            XCTAssertEqual(requests.count, mode == "workspace-key-revoked-prompt" ? 1 : mode == "workspace-jump-destination-refused" ? 2 : 0)
            XCTAssertEqual(f.vault.counts.signatures, 1)
        } else {
            let owner = try await connect()
            do {
                XCTAssertEqual(owner.peer.hostKey, host)
                try await owner.validateForPublication()
                XCTAssertEqual(f.vault.counts.signatures, jumped ? 2 : 1)
                let requests = await prompt.requests
                let expectedRoutes = mode == "workspace-key-only" ? [] : jumped ? ["hop", "hop", "route", "route"] : ["route", "route"]
                XCTAssertEqual(requests.map(\.authorization.routeID), expectedRoutes)
                XCTAssertEqual(Set(requests.map(\.attemptID)).count, mode == "workspace-key-only" ? 0 : jumped ? 2 : 1)
                XCTAssertEqual(Set(requests.map(\.transportID)).count, mode == "workspace-key-only" ? 0 : jumped ? 2 : 1)
                let coordinator = try TerminalInputCoordinator(), sink = WorkspaceMultifactorSink(identity: owner.identity)
                if mode == "workspace-key-revoked-live" || mode == "workspace-hop-key-revoked" {
                    let fingerprint = mode == "workspace-hop-key-revoked" ? try XCTUnwrap(hopFingerprint)
                        : f.archive.workspace.credentials[0].fingerprint
                    try await f.store.revokeLocally(fingerprint: fingerprint)
                    let readiness = try await f.store.routeReadiness(workspaceID: f.installed.id,
                        support: WorkspaceNativeSSHAuthentication.routeSupport)
                    XCTAssertTrue(try XCTUnwrap(readiness.first(where: { $0.routeID == "route" })).isLocallyReady)
                    await refuse { try await owner.validateForPublication() }
                    await refuse {
                        _ = try await owner.openTerminal(geometry: .init(columns: 80, rows: 24), coordinator: coordinator) {
                            try await sink.prepare($0)
                        }
                    }
                    let prepared = await sink.prepared
                    XCTAssertFalse(prepared)
                } else {
                    XCTAssertTrue(["workspace-success", "workspace-key-only", "workspace-jump"].contains(mode))
                    let pump = try await owner.openTerminal(geometry: .init(columns: 80, rows: 24), coordinator: coordinator) {
                        try await sink.prepare($0)
                    }
                    let stream = try await coordinator.makeStream(sceneID: UUID())
                    let focus = try await coordinator.changeFocus(stream, to: pump.source)
                    let bytes = Data("Workspace MFA → owned terminal.\rEND-SESSION\r".utf8)
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
                }
                await owner.close()
            } catch { await owner.close(); throw error }
        }
        XCTAssertEqual(activity.accounting().connections, 0)
    }
}
