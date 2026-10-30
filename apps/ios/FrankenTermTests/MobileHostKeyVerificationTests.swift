import Foundation
import SwiftUI
import XCTest
import FrankenTermKit
import FrankenTermSSHIntegration
@testable import FrankenTerm

private final class HostCheckNoKeys: WorkspaceCredentialVault, WorkspaceSSHSigning, @unchecked Sendable {
    func create(account: String, privateContainer: Data, fingerprint: String) throws { throw SSHAuthenticationError.denied }
    func fingerprint(account: String) throws -> String? { nil }
    func remove(account: String) throws { throw SSHAuthenticationError.denied }
    func publicIdentity(requestID: UUID, authorization: WorkspaceSSHAuthorization) async throws -> Data {
        throw SSHAuthenticationError.denied
    }
    func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
              challenge: SSHUserAuthenticationChallenge) async throws -> Data { throw SSHAuthenticationError.denied }
    func cancel(requestID: UUID) {}
}

/// Holds cleanup after cancellation, so restoring a normal prompt scope too
/// early is observable without a network or protected private key.
private actor HostCheckGate {
    private var continuation: CheckedContinuation<Void, Never>?
    private(set) var started = false
    func wait() async {
        started = true
        await withCheckedContinuation { continuation = $0 }
    }
    func release() { continuation?.resume(); continuation = nil }
}

@MainActor
final class MobileHostKeyVerificationTests: XCTestCase {
    private struct Fixture {
        let store: WorkspaceStore, broker: SSHAuthenticationBroker
        let saved: InstalledWorkspace, host: WorkspaceDocument.HostKey, probe: WorkspaceHostKeyProbe
    }

    private func fixture(includeDirect: Bool = false) async throws -> Fixture {
        let crypto = AppleCryptography()
        let host = try WorkspaceDocument.HostKey(algorithm: "ssh-ed25519",
            fingerprint: "SHA256:" + Data(repeating: 9, count: 32).base64EncodedString().replacingOccurrences(of: "=", with: ""))
        let theme: [String: Any] = ["id": "theme", "name": "Theme", "foreground": "#FFFFFF", "background": "#000000",
            "accent": "#4080FF", "ansi": Array(repeating: "#808080", count: 16), "cursor_foreground": "#000000",
            "cursor_background": "#FFFFFF", "cursor_border": "#FFFFFF", "selection_foreground": "#FFFFFF",
            "selection_background": "#345D91"]
        func route(_ id: String, host: String, user: String, trust: String, jumps: [String]) -> [String: Any] {
            ["id": id, "mode": "direct_ssh", "host": host, "port": 22, "user": user,
             "authentication_methods": ["password", "keyboard-interactive"], "credential_refs": [] as [String],
             "host_trust_ref": trust, "jump_routes": jumps, "session_creation": "manual"]
        }
        var routes = [route("destination", host: "destination.example", user: "destination-user", trust: "target-trust", jumps: ["hop"]),
                      route("hop", host: "jump.example", user: "jump-user", trust: "jump-trust", jumps: [])]
        if includeDirect { routes.append(route("direct", host: "destination.example", user: "destination-user", trust: "target-trust", jumps: [])) }
        let document = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: [
            "format": "ftworkspace", "version": 1, "bundle_id": "host-check", "source": ["exporter_id": "host-check"],
            "required_capabilities": [] as [String], "workspace": [
                "id": "workspace", "name": "Host check", "groups": [["id": "group", "name": "Group", "machine_order": ["machine"]]],
                "machines": [["id": "machine", "name": "Machine", "theme_ref": "theme", "route_refs": ["destination"],
                              "preferred_route_ref": "destination", "auto_connect": false]],
                "routes": routes, "credentials": [] as [[String: Any]], "themes": [theme],
                "host_trust": [["id": "target-trust", "host": "destination.example", "port": 22, "pins": [] as [[String: String]], "revoked_keys": [] as [[String: String]]],
                               ["id": "jump-trust", "host": "jump.example", "port": 22,
                                "pins": [["algorithm": host.algorithm, "fingerprint": host.fingerprint]], "revoked_keys": [] as [[String: String]]]],
                "preferences": ["initial_machine_ref": "machine", "initial_group_ref": "group", "connect_on_open": false]
            ]
        ], options: [.sortedKeys]))
        let archive = try AirGapArchive(workspace: document, keys: [], using: crypto)
        let noKeys = HostCheckNoKeys()
        let store = WorkspaceStore(persistence: StoreTestPersistence(), vault: noKeys)
        let saved = try await store.install(archive, envelopeDigest: crypto.sha256(try archive.encoded()),
            expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let support = WorkspaceNativeSSHAuthentication.routeSupport
        let probe = try await store.beginHostKeyVerification(workspaceID: saved.id, expectedRevision: saved.revision,
            routeID: "destination", confirmedByUser: true, support: support)
        let broker = SSHAuthenticationBroker(store: store, signer: noKeys, support: support)
        await broker.resume()
        return Fixture(store: store, broker: broker, saved: saved, host: host, probe: probe)
    }

    private func scope(_ f: Fixture, route: String = "destination", probe: Bool) -> MobileSSHPasswordPrompt.Scope {
        .init(workspaceID: f.saved.id, revision: f.saved.revision, machineID: "machine",
              destinationRouteIDs: [route], hostKeyVerification: probe)
    }

    private func password(_ f: Fixture, probe: Bool) async throws -> WorkspaceSSHPasswordIdentity {
        try await f.broker.passwordIdentity(workspaceID: f.saved.id, expectedRevision: f.saved.revision, routeID: "hop",
            presentedHostKey: f.host, transitForRouteID: probe ? "destination" : nil, hostKeyProbe: probe ? f.probe : nil)
    }

    private func keyboardInteractive(_ f: Fixture, probe: Bool) async throws -> WorkspaceSSHKeyboardInteractiveIdentity {
        try await f.broker.keyboardInteractiveIdentity(workspaceID: f.saved.id, expectedRevision: f.saved.revision, routeID: "hop",
            presentedHostKey: f.host, transitForRouteID: probe ? "destination" : nil, hostKeyProbe: probe ? f.probe : nil)
    }

    private func waitFor<Value>(_ read: () -> Value?) async throws -> Value {
        for _ in 0..<300 {
            if let value = read() { return value }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTFail("The admitted synthetic prompt did not appear")
        throw SSHAuthenticationError.unavailable
    }

    func testPasswordProbeAndOrdinaryTerminalScopesCannotAdoptEachOthersRequests() async throws {
        let f = try await fixture(), prompt = MobileSSHPasswordPrompt()
        prompt.setActive(true)
        let selected = try await password(f, probe: true)
        prompt.setScope(scope(f, probe: false))
        do {
            _ = try await f.broker.password(for: selected, transportID: UUID(), prompter: prompt)
            XCTFail("A terminal scope accepted a host-verification prompt")
        } catch {}
        XCTAssertNil(prompt.request)
        prompt.setScope(scope(f, probe: true))
        let task = Task { try await f.broker.password(for: selected, transportID: UUID(), prompter: prompt) }
        let request = try await waitFor { prompt.request }
        XCTAssertEqual(request.authorization.host, "jump.example")
        XCTAssertEqual(request.authorization.user, "jump-user")
        XCTAssertEqual(request.authorization.hostKeyProbe?.id, f.probe.id)
        XCTAssertTrue(MobileSSHAuthenticationSheetRequest.password(request).hostKeyVerification)
        prompt.editSecret("synthetic hop password", requestID: request.id); prompt.submit(requestID: request.id)
        let response = try await task.value
        XCTAssertEqual(response, Data("synthetic hop password".utf8))
        XCTAssertTrue(prompt.secret.isEmpty); XCTAssertNil(prompt.request)
        let ordinary = try await password(f, probe: false)
        prompt.setScope(scope(f, route: "hop", probe: true))
        do {
            _ = try await f.broker.password(for: ordinary, transportID: UUID(), prompter: prompt)
            XCTFail("A probe scope accepted an ordinary terminal login")
        } catch {}
        XCTAssertNil(prompt.request)
        let saved = try await f.store.load()
        XCTAssertTrue(try XCTUnwrap(saved.first).connectLater, "probe authentication does not approve Connect")
        await f.store.endHostKeyVerification(f.probe)
    }

    func testMFAProbeAndOrdinaryScopesAreSeparateAndUseTheVerifiedHopAccount() async throws {
        let f = try await fixture(), prompt = MobileSSHKeyboardInteractivePrompt()
        prompt.setActive(true)
        let refused = try await keyboardInteractive(f, probe: true)
        let challenge = try SSHKeyboardInteractiveChallenge(round: 1, name: "Synthetic MFA", instruction: "",
            prompts: [.init(text: "Code", echo: false)])
        prompt.setScope(scope(f, probe: false))
        do {
            _ = try await f.broker.keyboardInteractiveResponses(for: refused, transportID: UUID(), attemptID: UUID(),
                challenge: challenge, prompter: prompt)
            XCTFail("A terminal scope accepted a probe MFA round")
        } catch {}
        prompt.setScope(scope(f, probe: true))
        let selected = try await keyboardInteractive(f, probe: true)
        let task = Task { try await f.broker.keyboardInteractiveResponses(for: selected, transportID: UUID(), attemptID: UUID(),
            challenge: challenge, prompter: prompt) }
        let request = try await waitFor { prompt.request }
        XCTAssertEqual(request.authorization.host, "jump.example")
        XCTAssertEqual(request.authorization.transitForRouteID, "destination")
        XCTAssertTrue(MobileSSHAuthenticationSheetRequest.keyboardInteractive(request).hostKeyVerification)
        prompt.editAnswer("123456", index: 0, requestID: request.id); prompt.submit(requestID: request.id)
        let responses = try await task.value
        XCTAssertEqual(responses, [Data("123456".utf8)])
        XCTAssertTrue(prompt.answers.isEmpty)
        await f.broker.finishKeyboardInteractiveAttempt(selected)
        let ordinary = try await keyboardInteractive(f, probe: false)
        prompt.setScope(scope(f, route: "hop", probe: true))
        do {
            _ = try await f.broker.keyboardInteractiveResponses(for: ordinary, transportID: UUID(), attemptID: UUID(),
                challenge: challenge, prompter: prompt)
            XCTFail("A probe scope accepted an ordinary MFA round")
        } catch {}
        XCTAssertNil(prompt.request)
        await f.store.endHostKeyVerification(f.probe)
    }

    private func connections(_ f: Fixture) throws -> MobileConnections {
        try MobileConnections(store: f.store,
            journalDirectory: FileManager.default.temporaryDirectory.appendingPathComponent("host-check-" + UUID().uuidString),
            signer: HostCheckNoKeys())
    }

    private func waitForGate(_ gate: HostCheckGate) async throws {
        for _ in 0..<300 {
            if await gate.started { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        throw SSHAuthenticationError.unavailable
    }

    func testTransientInactivityDoesNotCancelTheProbeOwnerAndLatestTerminalScopeReturnsAfterJoin() async throws {
        let f = try await fixture(), connections = try connections(f), gate = HostCheckGate()
        connections.setAuthenticationForeground(true)
        connections.setAuthenticationScope(scope(f, probe: false))
        let probeScope = scope(f, probe: true)
        let task = Task { try await connections.withHostKeyVerificationScope(probeScope) {
            await gate.wait()
            return Task.isCancelled
        } }
        defer { task.cancel(); Task { await gate.release() } }
        try await waitForGate(gate)
        // A system Face ID sheet's inactive/active sequence ends editable
        // prompts, but must leave the admitted key check alive.
        connections.setAuthenticationForeground(false)
        connections.setAuthenticationScope(scope(f, route: "hop", probe: false))
        connections.setAuthenticationForeground(true)
        let selected = try await password(f, probe: true)
        let asking = Task { try await f.broker.password(for: selected, transportID: UUID(), prompter: connections.passwordPrompt) }
        let request = try await waitFor { connections.passwordPrompt.request }
        XCTAssertNil(connections.authenticationPrompt)
        XCTAssertEqual(connections.hostKeyVerificationPrompt?.id, request.id)
        connections.passwordPrompt.editSecret("probe only", requestID: request.id)
        connections.passwordPrompt.submit(requestID: request.id)
        _ = try await asking.value
        await gate.release()
        let cancelled = try await task.value
        XCTAssertFalse(cancelled, "transient inactivity did not cancel the admitted check")
        // The latest ordinary scope was held while the probe owned Keys and
        // is restored after joining; the original destination scope would fail.
        let ordinary = try await password(f, probe: false)
        let normal = Task { try await f.broker.password(for: ordinary, transportID: UUID(), prompter: connections.passwordPrompt) }
        let normalRequest = try await waitFor { connections.passwordPrompt.request }
        XCTAssertEqual(connections.authenticationPrompt?.id, normalRequest.id)
        XCTAssertNil(connections.hostKeyVerificationPrompt)
        connections.passwordPrompt.cancelNow(requestID: normalRequest.id)
        do { _ = try await normal.value; XCTFail("Cancelled password escaped") } catch {}
        await f.store.endHostKeyVerification(f.probe)
    }

    func testCancellationKeepsScopeClosedUntilCleanupJoinsAndCannotCancelLaterTerminalPrompt() async throws {
        let f = try await fixture(), connections = try connections(f), gate = HostCheckGate()
        connections.setAuthenticationForeground(true)
        connections.setAuthenticationScope(scope(f, route: "hop", probe: false))
        let probeScope = scope(f, probe: true)
        let task = Task { try await connections.withHostKeyVerificationScope(probeScope) { await gate.wait(); return 42 } }
        defer { task.cancel(); Task { await gate.release() } }
        try await waitForGate(gate)
        connections.cancelHostKeyVerification()
        let ordinary = try await password(f, probe: false)
        do {
            _ = try await f.broker.password(for: ordinary, transportID: UUID(), prompter: connections.passwordPrompt)
            XCTFail("Ordinary scope returned before the cancelled path finished teardown")
        } catch {}
        do {
            _ = try await connections.withHostKeyVerificationScope(probeScope) { XCTFail("A second check was admitted"); return 0 }
            XCTFail("A new check replaced the one still cleaning up")
        } catch {}
        await gate.release()
        do { _ = try await task.value; XCTFail("A cancelled check returned its delayed result") } catch {}
        let normal = Task { try await f.broker.password(for: ordinary, transportID: UUID(), prompter: connections.passwordPrompt) }
        let request = try await waitFor { connections.passwordPrompt.request }
        connections.cancelHostKeyVerification() // A late cancellation owns no later editor.
        XCTAssertEqual(connections.passwordPrompt.request?.id, request.id)
        connections.passwordPrompt.cancelNow(requestID: request.id)
        do { _ = try await normal.value; XCTFail("Cancelled password escaped") } catch {}
        await f.store.endHostKeyVerification(f.probe)
    }

    func testProbeScopeRequiresOneDestinationAndActivePresentation() async throws {
        let f = try await fixture(), connections = try connections(f)
        let valid = scope(f, probe: true)
        do { _ = try await connections.withHostKeyVerificationScope(valid) { XCTFail("Inactive check ran"); return 0 }; XCTFail() } catch {}
        connections.setAuthenticationForeground(true)
        for invalid in [scope(f, probe: false),
                        MobileSSHPasswordPrompt.Scope(workspaceID: f.saved.id, revision: f.saved.revision, machineID: "machine",
                            destinationRouteIDs: ["destination", "hop"], hostKeyVerification: true)] {
            do { _ = try await connections.withHostKeyVerificationScope(invalid) { XCTFail("Broad scope ran"); return 0 }; XCTFail() } catch {}
        }
        await f.store.endHostKeyVerification(f.probe)
    }

    func testReviewNamesTrustedAccountsAndChoosesTheShortestEligibleConfiguredPath() async throws {
        XCTAssertEqual(LocalHostKeyCheckReview.endpoint(host: "fd7a:115c:a1e0::1", port: 2222, user: "operator"),
                       "operator@[fd7a:115c:a1e0::1]:2222")
        for direct in [false, true] {
            let f = try await fixture(includeDirect: direct), model = MobileWorkspaceModel()
            model.setScenePhase(.active)
            model.workspaces = [f.saved]
            XCTAssertEqual(model.hostKeyVerificationRoute(workspaceID: f.saved.id, trustID: "target-trust")?.id,
                           direct ? "direct" : nil,
                           "a direct check needs no protected-credential snapshot; jump accounts need readiness")
            model.localPolicies[f.saved.id] = try await f.store.loadWithLocalPolicy(support: WorkspaceNativeSSHAuthentication.routeSupport).first
            let review = try XCTUnwrap(model.reviewHostKeyVerification(workspaceID: f.saved.id, trustID: "target-trust"))
            XCTAssertEqual(review.routeID, direct ? "direct" : "destination")
            XCTAssertEqual(review.destination, "destination.example:22")
            XCTAssertEqual(review.jumpAccounts, direct ? [] : ["jump-user@jump.example:22"])
            model.setScenePhase(.inactive)
            XCTAssertNil(model.reviewHostKeyVerification(workspaceID: f.saved.id, trustID: "target-trust"))
            await f.store.endHostKeyVerification(f.probe)
        }
    }
}
