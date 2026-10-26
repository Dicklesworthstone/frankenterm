import XCTest
import UIKit
import FrankenTermKit
import FrankenTermSSH
import FrankenTermSSHIntegration
import FrankenTermTerminal

// The M0 data path inside the iOS app process, against a real OpenSSH server:
// restored workspace -> production store/broker/connection manager -> libssh2
// (iOS XCFramework) -> TerminalSessionPump -> TerminalModelRuntime -> the Rust
// engine (NativeTerminalModel) -> NativeTerminalRenderView (Metal).
//
// Only the Keychain vault and its LocalAuthentication-gated signer are replaced
// by an in-memory stand-in. The production AppleWorkspaceVault does store and
// read metadata in the Simulator, but each signature needs an interactive
// user-presence approval that an unattended test cannot give (device
// qualification is ft-ios-native-app-v7gxu.2). Signatures still come from the
// production SSHUserAuthenticationSigner, and the store persists through the
// real FileWorkspacePersistence.
//
// Live endpoint, supplied by the operator through xcodebuild's TEST_RUNNER_
// environment prefix (disposable keys; skipped when absent):
//   FT_LIVE_SSH_PORT, FT_LIVE_SSH_USER,
//   FT_LIVE_SSH_KEY_PEM_B64 (OpenSSH private key, base64 of the whole PEM file),
//   FT_LIVE_SSH_HOST_KEY_B64 (the server's ssh-ed25519 public key blob).

private enum LiveSSHTestError: Error { case invalid }

private final class MemoryVault: WorkspaceCredentialVault, @unchecked Sendable {
    private let lock = NSLock()
    private var items: [String: (container: Data, identity: SSHPublicIdentity)] = [:]
    func create(account: String, privateContainer: Data, fingerprint: String) throws {
        let pem = Data(("-----BEGIN OPENSSH PRIVATE KEY-----\n" + privateContainer.base64EncodedString()
            + "\n-----END OPENSSH PRIVATE KEY-----\n").utf8)
        let identity = try SSHPublicIdentity(blob: PrivateKeyRecord.fromPEM(pem, credentialID: "key",
            using: AppleCryptography()).publicBlob)
        guard identity.fingerprint == fingerprint else { throw LiveSSHTestError.invalid }
        lock.lock(); defer { lock.unlock() }
        guard items[account] == nil else { throw LiveSSHTestError.invalid }
        items[account] = (privateContainer, identity)
    }
    func fingerprint(account: String) throws -> String? {
        lock.lock(); defer { lock.unlock() }; return items[account]?.identity.fingerprint
    }
    func publicIdentity(account: String) throws -> SSHPublicIdentity? {
        lock.lock(); defer { lock.unlock() }; return items[account]?.identity
    }
    func remove(account: String) throws { lock.lock(); defer { lock.unlock() }; items[account] = nil }
    func item(_ authorization: WorkspaceSSHAuthorization) throws -> (container: Data, identity: SSHPublicIdentity) {
        lock.lock(); defer { lock.unlock() }
        guard let item = items.values.first(where: { $0.identity.fingerprint == authorization.fingerprint
            && $0.identity.algorithm == authorization.algorithm }) else { throw LiveSSHTestError.invalid }
        return item
    }
}

private final class MemorySigner: WorkspaceSSHSigning, @unchecked Sendable {
    private let vault: MemoryVault
    init(vault: MemoryVault) { self.vault = vault }
    func publicIdentity(requestID: UUID, authorization: WorkspaceSSHAuthorization) async throws -> Data {
        try vault.item(authorization).identity.blob
    }
    func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
              challenge: SSHUserAuthenticationChallenge) async throws -> Data {
        let item = try vault.item(authorization)
        guard item.identity.blob == challenge.publicBlob, challenge.user == authorization.user else {
            throw LiveSSHTestError.invalid
        }
        return try SSHUserAuthenticationSigner.sign(challenge, privateContainer: item.container)
    }
    func cancel(requestID: UUID) {}
}

/// Publishes each owned frame to the Metal view and keeps its decoded text so
/// the test can wait for real shell output.
@MainActor
private final class ViewDelivery: TerminalModelDelivery {
    let view: NativeTerminalRenderView
    private let connection: WorkspaceSSHConnection
    private(set) var frames = 0
    private(set) var text = ""
    init(view: NativeTerminalRenderView, connection: WorkspaceSSHConnection) {
        self.view = view; self.connection = connection
    }

    func present(_ presentation: TerminalModelPresentation, source: TerminalSource) async throws {
        guard case let .frame(bytes) = presentation else { return }
        let frame = try TerminalRenderFrame.decode(bytes, expectedSource: source)
        text = frame.lines.map { row in
            var slots = Array(repeating: " ", count: frame.columns)
            for cell in row.cells where cell.column < frame.columns && !cell.text.isEmpty { slots[cell.column] = cell.text }
            return slots.joined()
        }.joined(separator: "\n")
        frames += 1
        try view.submit(bytes, source: source)
    }
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) async throws {
        try await connection.recordInput(outcomes, source: source)
    }

    /// Bounded poll: true once a delivered frame shows `needle`.
    func waitFor(_ needle: String, seconds: Int) async -> Bool {
        for _ in 0..<(seconds * 20) {
            if text.contains(needle) { return true }
            try? await Task.sleep(for: .milliseconds(50))
        }
        return text.contains(needle)
    }
}

@MainActor
final class LiveSSHTerminalTests: XCTestCase {
    private func manifest(identity: SSHPublicIdentity, host: SSHPublicIdentity, port: UInt16, user: String) throws -> WorkspaceDocument {
        let theme: [String: Any] = ["id": "theme", "name": "Live", "foreground": "#E6E6E6", "background": "#10223C",
            "accent": "#3B82F6", "ansi": ["#000000", "#CC3333", "#33CC33", "#CCCC33", "#3366CC", "#CC33CC", "#33CCCC", "#E6E6E6",
                                          "#555555", "#FF5555", "#55FF55", "#FFFF55", "#5599FF", "#FF55FF", "#55FFFF", "#FFFFFF"],
            "cursor_foreground": "#000000", "cursor_background": "#FFFFFF", "cursor_border": "#FFFFFF",
            "selection_foreground": "#FFFFFF", "selection_background": "#345D91"]
        let workspace: [String: Any] = [
            "id": "workspace", "name": "Live loopback", "groups": [["id": "group", "name": "Local", "machine_order": ["machine"]]],
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
            "bundle_id": "live-ssh-test", "source": ["exporter_id": "live-ssh-test"], "required_capabilities": [],
            "workspace": workspace], options: [.sortedKeys]))
    }

    private struct Live {
        let installed: InstalledWorkspace, host: SSHPublicIdentity
        let manager: WorkspaceSSHConnectionManager
        let journal: TerminalSessionJournal, coordinator: TerminalInputCoordinator
        func connect() async throws -> WorkspaceSSHConnection {
            try await manager.connect(workspaceID: installed.id, expectedRevision: installed.revision,
                machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true,
                connectionTimeout: .seconds(10), authenticationTimeout: .seconds(20))
        }
    }

    /// The restored workspace and production connection manager for the
    /// operator's live sshd.
    private func live() async throws -> Live {
        let environment = ProcessInfo.processInfo.environment
        guard let port = environment["FT_LIVE_SSH_PORT"].flatMap(UInt16.init), let user = environment["FT_LIVE_SSH_USER"],
              let pem = environment["FT_LIVE_SSH_KEY_PEM_B64"].flatMap({ Data(base64Encoded: $0) }),
              let hostBlob = environment["FT_LIVE_SSH_HOST_KEY_B64"].flatMap({ Data(base64Encoded: $0) }) else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_SSH_{PORT,USER,KEY_PEM_B64,HOST_KEY_B64} to run against a live sshd")
        }
        let crypto = AppleCryptography()
        let key = try PrivateKeyRecord.fromPEM(pem, credentialID: "key", using: crypto)
        let identity = try SSHPublicIdentity(blob: key.publicBlob), host = try SSHPublicIdentity(blob: hostBlob)
        let archive = try AirGapArchive(workspace: manifest(identity: identity, host: host, port: port, user: user),
                                        keys: [key], using: crypto)
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("live-ssh-" + UUID().uuidString)
        let vault = MemoryVault()
        let store = WorkspaceStore(persistence: try FileWorkspacePersistence(directory: root.appendingPathComponent("store")),
                                   vault: vault)
        let installed = try await store.install(archive, envelopeDigest: crypto.sha256(try archive.encoded()),
            transferID: Data(repeating: 7, count: 16), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let broker = SSHAuthenticationBroker(store: store, signer: MemorySigner(vault: vault),
            support: .init(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
                           credentialAlgorithms: [identity.algorithm], hostKeyAlgorithms: [host.algorithm]))
        await broker.resume()
        let journal = try TerminalSessionJournal(directory: root.appendingPathComponent("journal"),
            identifier: "live-ssh-test-" + UUID().uuidString)
        let coordinator = try TerminalInputCoordinator()
        let manager = WorkspaceSSHConnectionManager(store: store, broker: broker, journal: journal, coordinator: coordinator)
        await manager.resume()
        return Live(installed: installed, host: host, manager: manager, journal: journal, coordinator: coordinator)
    }

    func testRestoredWorkspaceRunsARealShellRenderedThroughMetal() async throws {
        let live = try await live()
        let manager = live.manager, journal = live.journal, coordinator = live.coordinator
        let connection = try await live.connect()
        XCTAssertEqual(connection.peer.hostKey, live.host.blob, "authenticated only after the pinned host key verified")
        let geometry = try TerminalSessionGeometry(columns: 64, rows: 16)
        let pump = try await connection.openTerminal(geometry: geometry)
        let model = try NativeTerminalModel(source: pump.source, geometry: geometry, scrollbackRows: 256)
        let view = try await NativeTerminalRenderView(source: pump.source, bundle: .main)
        let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
        let window = try XCTUnwrap(scenes.flatMap(\.windows).first { $0.isKeyWindow } ?? scenes.first?.windows.first)
        view.frame = CGRect(x: 0, y: 0, width: min(window.bounds.width, 402), height: 340)
        window.addSubview(view)
        defer { view.removeFromSuperview() }
        let delivery = ViewDelivery(view: view, connection: connection)
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: delivery)
        let run = Task { try await pump.run(consumer: runtime) }

        let stream = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(stream, to: pump.source)
        // The shell computes 42; our typed input only contains "$((6*7))", so the
        // marker proves remote execution, not local echo.
        _ = try await coordinator.submit(focus, encodedBytes: Data(
            "printf '\\033[1;32mFrankenTerm\\033[0m on iOS over SSH: %s\\n' FRANKEN-$((6*7)); uname -s\r".utf8))
        let arrived = await delivery.waitFor("FRANKEN-42", seconds: 20)
        XCTAssertTrue(arrived, "shell output never reached a frame:\n\(delivery.text)")
        try await Task.sleep(for: .milliseconds(500)) // let the Metal frame present before the snapshot

        let format = UIGraphicsImageRendererFormat(); format.scale = 2
        let image = UIGraphicsImageRenderer(bounds: view.bounds, format: format).image { _ in
            _ = view.drawHierarchy(in: view.bounds, afterScreenUpdates: true)
        }
        let attachment = XCTAttachment(image: image)
        attachment.name = "live-ssh-terminal"; attachment.lifetime = .keepAlways
        add(attachment)

        _ = try await coordinator.submit(focus, encodedBytes: Data("exit\r".utf8))
        let ended = try await run.value
        XCTAssertEqual(ended, .completed(.init(remoteExit: nil, channelClosed: true)))
        try? await manager.close(connectionID: connection.id)
        try await coordinator.suspend(focus)
        try await pump.reconcileStoppedOutcomes { outcomes, source in
            try await connection.recordInput(outcomes, source: source)
        }
        try await connection.prepareInputSettlement(source: pump.source)
        try await coordinator.retire(pump.source)
        try await runtime.close()
        try await manager.close(connectionID: connection.id)
        XCTAssertGreaterThan(delivery.frames, 0)
        let history = try await journal.records()
        XCTAssertTrue(history.first?.inputEvidence.contains(.transportAccepted) == true)
        XCTAssertTrue(history.first?.canResolve == true)
    }

    /// Stop trusting the server's key on this phone while connected: the live
    /// connection ends, and the next connect is refused.
    func testBlockingTheHostKeyEndsTheLiveConnectionAndRefusesTheNext() async throws {
        let live = try await live()
        let connection = try await live.connect()
        let openBefore = await connection.isClosed
        XCTAssertFalse(openBefore)
        let key = try WorkspaceDocument.HostKey(algorithm: live.host.algorithm, fingerprint: live.host.fingerprint)
        let receipt = try await live.manager.revokeHostLocally(workspaceID: live.installed.id, trustID: "trust",
            key: key, confirmedByUser: true)
        XCTAssertEqual(receipt.affectedWorkspaceIDs, [live.installed.id])
        XCTAssertTrue(receipt.retainedConnectionIDs.isEmpty)
        let closed = await connection.isClosed
        XCTAssertTrue(closed, "the connection to the blocked host stayed open")
        let remaining = await live.manager.connections()
        XCTAssertTrue(remaining.isEmpty)
        do {
            _ = try await live.connect()
            XCTFail("a blocked host key admitted a new connection")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
    }
}
