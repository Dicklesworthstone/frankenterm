import XCTest
import UIKit
import FrankenTermKit
import FrankenTermSSH
import FrankenTermSSHIntegration
import FrankenTermTerminal
@testable import FrankenTerm

// The app scene's session owner (TerminalSessionScreen.swift) against a live
// sshd, typing through the real NativeTerminalInputView -> TerminalKeyboardSession
// -> TerminalModelRuntime (Rust encoder) path. The remote shell writes its PTY
// size and a computed marker to a host file the Simulator process can read
// (FT_LIVE_SSH_OUT), so the check observes remote effects, not local echo.
// Same live-endpoint variables as LiveSSHTerminalTests; skipped without them.

private final class SceneMemoryVault: WorkspaceCredentialVault, @unchecked Sendable {
    private let lock = NSLock()
    private var items: [String: (container: Data, identity: SSHPublicIdentity)] = [:]
    func create(account: String, privateContainer: Data, fingerprint: String) throws {
        let identity = try SSHUserAuthenticationSigner.publicIdentity(privateContainer: privateContainer)
        guard identity.fingerprint == fingerprint else { throw TerminalModelError.invalid }
        lock.lock(); defer { lock.unlock() }
        items[account] = (privateContainer, identity)
    }
    func fingerprint(account: String) throws -> String? { lock.lock(); defer { lock.unlock() }; return items[account]?.identity.fingerprint }
    func publicIdentity(account: String) throws -> SSHPublicIdentity? { lock.lock(); defer { lock.unlock() }; return items[account]?.identity }
    func remove(account: String) throws { lock.lock(); defer { lock.unlock() }; items[account] = nil }
    func item(_ authorization: WorkspaceSSHAuthorization) throws -> (container: Data, identity: SSHPublicIdentity) {
        lock.lock(); defer { lock.unlock() }
        guard let item = items.values.first(where: { $0.identity.fingerprint == authorization.fingerprint }) else {
            throw TerminalModelError.invalid
        }
        return item
    }
}

private final class SceneMemorySigner: WorkspaceSSHSigning, @unchecked Sendable {
    let vault: SceneMemoryVault
    init(vault: SceneMemoryVault) { self.vault = vault }
    func publicIdentity(requestID: UUID, authorization: WorkspaceSSHAuthorization) async throws -> Data {
        try vault.item(authorization).identity.blob
    }
    func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
              challenge: SSHUserAuthenticationChallenge) async throws -> Data {
        try SSHUserAuthenticationSigner.sign(challenge, privateContainer: vault.item(authorization).container)
    }
    func cancel(requestID: UUID) {}
}

@MainActor
final class TerminalSessionControllerTests: XCTestCase {
    private func document(identity: SSHPublicIdentity, host: SSHPublicIdentity, port: UInt16, user: String) throws -> WorkspaceDocument {
        let theme: [String: Any] = ["id": "theme", "name": "Live", "foreground": "#E6E6E6", "background": "#10223C",
            "accent": "#3B82F6", "ansi": Array(repeating: "#808080", count: 16), "cursor_foreground": "#000000",
            "cursor_background": "#FFFFFF", "cursor_border": "#FFFFFF", "selection_foreground": "#FFFFFF",
            "selection_background": "#345D91"]
        let workspace: [String: Any] = [
            "id": "workspace", "name": "Scene", "groups": [["id": "group", "name": "Local", "machine_order": ["machine"]]],
            "machines": [["id": "machine", "name": "Loopback", "theme_ref": "theme", "route_refs": ["route"],
                           "preferred_route_ref": "route", "auto_connect": false]],
            "routes": [["id": "route", "mode": "direct_ssh", "session_creation": "on_select", "host": "127.0.0.1",
                        "port": Int(port), "user": user, "credential_refs": ["key"], "authentication_methods": ["publickey"],
                        "host_trust_ref": "trust", "jump_routes": []]],
            "credentials": [["id": "key", "algorithm": identity.algorithm, "public_fingerprint": identity.fingerprint,
                             "requirement": "matching_private_key", "private_material": "not_included"]],
            "host_trust": [["id": "trust", "host": "127.0.0.1", "port": Int(port), "revoked_keys": [],
                            "pins": [["algorithm": host.algorithm, "fingerprint": host.fingerprint]]]],
            "themes": [theme], "preferences": ["initial_machine_ref": "machine", "initial_group_ref": "group", "connect_on_open": false]
        ]
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: ["format": "ftworkspace", "version": 1,
            "bundle_id": "scene-test", "source": ["exporter_id": "scene-test"], "required_capabilities": [],
            "workspace": workspace], options: [.sortedKeys]))
    }

    private struct LiveScene {
        let connections: MobileConnections, target: TerminalSessionTarget, outPath: String
    }

    private func liveScene() async throws -> LiveScene {
        let environment = ProcessInfo.processInfo.environment
        guard let port = environment["FT_LIVE_SSH_PORT"].flatMap(UInt16.init), let user = environment["FT_LIVE_SSH_USER"],
              let pem = environment["FT_LIVE_SSH_KEY_PEM_B64"].flatMap({ Data(base64Encoded: $0) }),
              let hostBlob = environment["FT_LIVE_SSH_HOST_KEY_B64"].flatMap({ Data(base64Encoded: $0) }),
              let outPath = environment["FT_LIVE_SSH_OUT"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_SSH_{PORT,USER,KEY_PEM_B64,HOST_KEY_B64,OUT} to run against a live sshd")
        }
        let crypto = AppleCryptography()
        let key = try PrivateKeyRecord.fromPEM(pem, credentialID: "key", using: crypto)
        let identity = try SSHPublicIdentity(blob: key.publicBlob), host = try SSHPublicIdentity(blob: hostBlob)
        let archive = try AirGapArchive(workspace: document(identity: identity, host: host, port: port, user: user),
                                        keys: [key], using: crypto)
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("scene-" + UUID().uuidString)
        let vault = SceneMemoryVault()
        let store = WorkspaceStore(persistence: try FileWorkspacePersistence(directory: root.appendingPathComponent("store")),
                                   vault: vault)
        let installed = try await store.install(archive, envelopeDigest: crypto.sha256(try archive.encoded()),
            transferID: Data(repeating: 9, count: 16), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let connections = try MobileConnections(store: store, journalDirectory: root.appendingPathComponent("journal"),
                                                signer: SceneMemorySigner(vault: vault))
        await connections.setActive(true)
        return LiveScene(connections: connections, target: .init(workspaceID: installed.id, revision: installed.revision,
            machineID: "machine", machineName: "Loopback", routeID: "route", credentialID: "key"), outPath: outPath)
    }

    /// Starts `controller`, puts its view in the key window and returns it.
    private func live(_ controller: TerminalSessionController,
                      acknowledgingEarlierSessions: Bool = false) async throws -> NativeTerminalView {
        await controller.start(acknowledgingEarlierSessions: acknowledgingEarlierSessions)
        XCTAssertEqual(controller.phase, .live)
        let terminal = try XCTUnwrap(controller.terminal)
        let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
        let window = try XCTUnwrap(scenes.flatMap(\.windows).first { $0.isKeyWindow } ?? scenes.first?.windows.first)
        terminal.frame = CGRect(x: 0, y: 0, width: min(window.bounds.width, 402), height: 300)
        window.addSubview(terminal)
        return terminal
    }

    /// Types through UIKit's text-input protocol once a caret has been drawn
    /// (input admission requires a matching rendered projection).
    private func type(_ text: String, into terminal: NativeTerminalView) async throws {
        for _ in 0..<200 {
            // isEditable: input accepted and the rendered caret geometry exists.
            if terminal.input.isEditable {
                terminal.input.insertText(text); terminal.input.insertText("\n")
                return
            }
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTFail("the native input view never became editable")
    }

    func testChromeContrastFollowsTheThemeBackground() {
        XCTAssertTrue(TerminalSessionTarget.isDark(nil))
        XCTAssertTrue(TerminalSessionTarget.isDark("#14213D"))
        XCTAssertTrue(TerminalSessionTarget.isDark("#10223C80"))
        XCTAssertFalse(TerminalSessionTarget.isDark("#FFFFFF"))
        XCTAssertFalse(TerminalSessionTarget.isDark("#FCA311"))
        XCTAssertFalse(TerminalSessionTarget.isDark("#808080"))
    }

    func testSceneControllerTypesThroughTheNativeKeyboardAndResizesThePTY() async throws {
        let scene = try await liveScene(), outPath = scene.outPath, connections = scene.connections
        let controller = TerminalSessionController(target: scene.target, connections: connections)
        let terminal = try await live(controller)
        defer { terminal.removeFromSuperview() }
        try await type("stty size > \(outPath); echo FRANKEN-$((6*7)) >> \(outPath)", into: terminal)
        var output = ""
        for _ in 0..<200 {
            output = (try? String(contentsOfFile: outPath, encoding: .utf8)) ?? ""
            if output.contains("FRANKEN-42") { break }
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertTrue(output.contains("FRANKEN-42"), "the remote shell never ran the typed command (output: \(output))")
        let size = output.split(separator: "\n").first.map(String.init) ?? ""
        XCTAssertNotEqual(size, "24 80", "the PTY must follow the view's viewport, not the provisional 80x24")

        let format = UIGraphicsImageRendererFormat(); format.scale = 2
        let image = UIGraphicsImageRenderer(bounds: terminal.bounds, format: format).image { _ in
            _ = terminal.drawHierarchy(in: terminal.bounds, afterScreenUpdates: true)
        }
        let attachment = XCTAttachment(image: image); attachment.name = "scene-controller-terminal"
        attachment.lifetime = .keepAlways; add(attachment)

        await controller.stop()
        let remaining = await connections.manager.connections()
        XCTAssertTrue(remaining.isEmpty, "stop must settle the connection through the manager")
    }

    /// A shell that exits ends the session from inside the pump task; teardown
    /// must still finish there, and the next session on the machine needs the
    /// user's acknowledgement of the first one's unknown outcome.
    func testRemoteExitSettlesAndANewSessionNeedsAcknowledgement() async throws {
        let scene = try await liveScene(), connections = scene.connections
        let first = TerminalSessionController(target: scene.target, connections: connections)
        let terminal = try await live(first)
        defer { terminal.removeFromSuperview() }
        try await type("exit", into: terminal)
        for _ in 0..<200 where !first.settled { try await Task.sleep(for: .milliseconds(50)) }
        XCTAssertEqual(first.phase, .ended(TerminalSessionController.describe(.peerOutputEnded)))
        XCTAssertTrue(first.settled, "teardown after a remote exit must finish")
        let remaining = await connections.manager.connections()
        XCTAssertTrue(remaining.isEmpty)

        let unacknowledged = first.replacement()
        await unacknowledged.start()
        XCTAssertEqual(unacknowledged.phase, .needsReview, "the ended session's outcome must be reviewed first")
        XCTAssertTrue(unacknowledged.settled)

        let second = unacknowledged.replacement()
        let again = try await live(second, acknowledgingEarlierSessions: true)
        defer { again.removeFromSuperview() }
        try await type("echo AGAIN-$((40+2)) > \(scene.outPath)", into: again)
        var output = ""
        for _ in 0..<200 where !output.contains("AGAIN-42") {
            try await Task.sleep(for: .milliseconds(50))
            output = (try? String(contentsOfFile: scene.outPath, encoding: .utf8)) ?? ""
        }
        XCTAssertTrue(output.contains("AGAIN-42"), "the acknowledged new session must run commands (output: \(output))")
        await second.stop()
        XCTAssertTrue(second.settled)
    }
}
