import XCTest
import UIKit
import CoreText
import Network
import FrankenTermKit
import FrankenTermSSH
import FrankenTermSSHIntegration
import FrankenTermTerminal
@testable import FrankenTerm

// The app's live workspace scene (WorkspaceTerminalScene.swift) and its
// per-machine sessions (TerminalSessionScreen.swift) against a live sshd, typing
// through the real NativeTerminalInputView -> TerminalKeyboardSession ->
// TerminalModelRuntime (Rust encoder) path with Kit's navigation owning focus.
// The remote shells write to a host file the Simulator process can read
// (FT_LIVE_SSH_OUT), so checks observe remote effects, not local echo.
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

/// A loopback TCP relay to the live sshd, standing in for the phone's
/// network. Taken down, it drops every relayed connection and refuses new
/// ones; brought back, it listens on the same port again.
private final class LoopbackRelay: @unchecked Sendable {
    private let queue = DispatchQueue(label: "loopback-relay")
    private var target: NWEndpoint.Port
    private var listener: NWListener?
    private var connections: [NWConnection] = []
    /// Bytes reach the relay and go no further, with no reset: a connection
    /// stranded on a route the device left looks like this to the phone.
    private var stalled = false
    /// New connections are reset as soon as the first bytes pass: the phone
    /// reaches the machine, and the connection dies before sign-in finishes.
    private var cutting = false
    private(set) var port: UInt16 = 0

    init(to target: UInt16) throws {
        guard let target = NWEndpoint.Port(rawValue: target) else { throw TerminalModelError.invalid }
        self.target = target
    }

    func up() async throws {
        var attempts = 0
        while true {
            do { return try await listen() }
            catch where port != 0 && attempts < 40 {
                // The listener down() cancelled may hold the port for a moment.
                attempts += 1
                try await Task.sleep(for: .milliseconds(25))
            }
        }
    }

    private func listen() async throws {
        let parameters = NWParameters.tcp
        parameters.allowLocalEndpointReuse = true
        parameters.requiredLocalEndpoint = .hostPort(host: .ipv4(.loopback), port: NWEndpoint.Port(rawValue: port) ?? .any)
        let listener = try NWListener(using: parameters)
        listener.newConnectionHandler = { [weak self] inbound in self?.relay(inbound) }
        port = try await withCheckedThrowingContinuation { continuation in
            listener.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    listener.stateUpdateHandler = nil
                    continuation.resume(returning: listener.port?.rawValue ?? 0)
                case .failed(let error):
                    listener.stateUpdateHandler = nil
                    listener.cancel()
                    continuation.resume(throwing: error)
                default: break
                }
            }
            listener.start(queue: queue)
        }
        queue.sync { self.listener = listener }
    }

    func down() {
        queue.sync {
            listener?.cancel(); listener = nil
            for connection in connections { connection.cancel() }
            connections = []; stalled = false; cutting = false
        }
    }

    func stall() { queue.sync { stalled = true } }

    /// Drops every relayed connection and resets each new one partway
    /// through its handshake, until down().
    func cut() {
        queue.sync {
            cutting = true
            for connection in connections { connection.forceCancel() }
            connections = []
        }
    }

    /// New connections reach another server on the same relay port.
    func retarget(to port: UInt16) throws {
        guard let port = NWEndpoint.Port(rawValue: port) else { throw TerminalModelError.invalid }
        queue.sync { target = port }
    }

    /// Runs on the relay's queue.
    private func relay(_ inbound: NWConnection) {
        let outbound = NWConnection(host: .ipv4(.loopback), port: target, using: .tcp)
        connections += [inbound, outbound]
        for connection in [inbound, outbound] {
            connection.stateUpdateHandler = { state in
                if case .failed = state { inbound.cancel(); outbound.cancel() }
            }
            connection.start(queue: queue)
        }
        pipe(inbound, to: outbound); pipe(outbound, to: inbound)
    }

    private func pipe(_ from: NWConnection, to: NWConnection) {
        from.receive(minimumIncompleteLength: 1, maximumLength: 65_536) { [weak self] data, _, complete, error in
            if let data, !data.isEmpty, self?.cutting == true {
                to.send(content: data, completion: .contentProcessed { _ in from.forceCancel(); to.forceCancel() })
                return
            }
            if let data, !data.isEmpty, self?.stalled == false { to.send(content: data, completion: .contentProcessed { _ in }) }
            if complete || error != nil { from.cancel(); to.cancel() } else { self?.pipe(from, to: to) }
        }
    }
}

@MainActor
final class TerminalSessionControllerTests: XCTestCase {
    func testRouteOrderUsesFirstHopReachabilityRatherThanDestination() {
        let routes: [TerminalSessionRoute] = [
            .init(id: "private-via-tailnet", credentialIDs: ["key"], host: "10.40.0.7", port: 22,
                  jumpHost: "100.91.120.17", jumpPort: 2200),
            .init(id: "tailnet-via-public", credentialIDs: ["key"], host: "100.92.33.44", port: 22,
                  jumpHost: "gateway.example"),
            .init(id: "direct-public", credentialIDs: ["key"], host: "public.example", port: 2222),
            .init(id: "direct-tailnet", credentialIDs: ["key"], host: "100.81.2.3", port: 22)
        ]
        let target = TerminalSessionTarget(workspaceID: UUID(), revision: UUID(), machineID: "build",
                                           machineName: "Build", routes: routes)
        var queried: [String] = []
        let withoutTailnet = target.attemptOrder { host in
            queried.append(host)
            return WorkspaceRouteNetwork(host: host) != .tailnet
        }
        XCTAssertEqual(queried, ["100.91.120.17", "gateway.example", "public.example", "100.81.2.3"],
                       "Only the endpoint reached by the phone determines route priority")
        XCTAssertEqual(withoutTailnet.map(\.id), ["tailnet-via-public", "direct-public", "private-via-tailnet", "direct-tailnet"])
        XCTAssertEqual(target.attemptOrder { _ in true }.map(\.id), routes.map(\.id),
                       "Equally reachable routes retain the configured preference")
        XCTAssertEqual(routes.map(\.networkPort), [2200, 22, 2222, 22])
    }

    func testConnectionCaptionNamesBothEndpointsAndClassifiesTheFirstHop() {
        let publicHop = TerminalSessionRoute(id: "public", credentialIDs: ["key"], host: "100.92.33.44", port: 2222,
                                            jumpHost: "gateway.example")
        XCTAssertEqual(WorkspaceTerminalScreen.via(publicHop), "100.92.33.44:2222 via gateway.example:22")
        let tailnetHop = TerminalSessionRoute(id: "tailnet", credentialIDs: ["key"], host: "fd00::20", port: 2222,
                                             jumpHost: "fd7a:115c:a1e0::1", jumpPort: 2200)
        XCTAssertEqual(WorkspaceTerminalScreen.via(tailnetHop),
                       "over Tailscale · [fd00::20]:2222 via [fd7a:115c:a1e0::1]:2200")
        let direct = TerminalSessionRoute(id: "local", credentialIDs: ["key"], host: "192.168.1.8", port: 22)
        XCTAssertEqual(WorkspaceTerminalScreen.via(direct), "on the local network · 192.168.1.8")
    }

    /// With `muxCommand`, each machine's route is a mux route; with
    /// `directAfterMux` too, a direct route (`creation`) follows it, as the
    /// Mac's export orders them.
    private func document(identity: SSHPublicIdentity, host: SSHPublicIdentity, port: UInt16, user: String,
                          creation: TerminalSessionCreation = .onSelect, muxCommand: String? = nil,
                          directAfterMux: Bool = false, machineIDs: [String] = ["alpha", "beta"]) throws -> WorkspaceDocument {
        let theme: [String: Any] = ["id": "theme", "name": "Live", "foreground": "#E6E6E6", "background": "#10223C",
            "accent": "#3B82F6", "ansi": Array(repeating: "#808080", count: 16), "cursor_foreground": "#000000",
            "cursor_background": "#FFFFFF", "cursor_border": "#FFFFFF", "selection_foreground": "#FFFFFF",
            "selection_background": "#345D91"]
        // Two machines on the same loopback host, each with its own route, so
        // each gets its own SSH connection, shell and journal record.
        let workspace: [String: Any] = [
            "id": "workspace", "name": "Scene", "groups": [["id": "group", "name": "Local", "machine_order": machineIDs]],
            "machines": machineIDs.map { machine -> [String: Any] in
                let refs = muxCommand != nil && directAfterMux ? ["route-\(machine)-mux", "route-\(machine)"] : ["route-\(machine)"]
                return ["id": machine, "name": machine.capitalized, "theme_ref": "theme", "route_refs": refs,
                        "preferred_route_ref": refs[0], "auto_connect": false]
            },
            "routes": machineIDs.flatMap { machine -> [[String: Any]] in
                func route(_ id: String) -> [String: Any] {
                    ["id": id, "host": "127.0.0.1", "port": Int(port), "user": user, "credential_refs": ["key"],
                     "authentication_methods": ["publickey"], "host_trust_ref": "trust", "jump_routes": []]
                }
                var direct = route("route-\(machine)")
                direct["mode"] = "direct_ssh"; direct["session_creation"] = creation.rawValue
                guard let muxCommand else { return [direct] }
                var mux = route(directAfterMux ? "route-\(machine)-mux" : "route-\(machine)")
                mux["mode"] = "frankenterm_mux"; mux["attach_policy"] = "existing_only"
                mux["remote_command"] = muxCommand
                return directAfterMux ? [mux, direct] : [mux]
            },
            "credentials": [["id": "key", "algorithm": identity.algorithm, "public_fingerprint": identity.fingerprint,
                             "requirement": "matching_private_key", "private_material": "not_included"]],
            "host_trust": [["id": "trust", "host": "127.0.0.1", "port": Int(port), "revoked_keys": [],
                            "pins": [["algorithm": host.algorithm, "fingerprint": host.fingerprint]]]],
            "themes": [theme], "preferences": ["initial_machine_ref": "alpha", "initial_group_ref": "group", "connect_on_open": false]
        ]
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: ["format": "ftworkspace", "version": 1,
            "bundle_id": "scene-test", "source": ["exporter_id": "scene-test"], "required_capabilities": [],
            "workspace": workspace], options: [.sortedKeys]))
    }

    private struct LiveScene {
        let connections: MobileConnections, scene: WorkspaceTerminalScene, outPath: String
        /// The app launched again: new connections and scene over the same
        /// installed workspace and session journal.
        let relaunch: () async throws -> LiveScene
    }

    /// `relayPort` routes the machines through a relay to the live sshd. Existing
    /// input tests explicitly start in interactive mode; observer tests exercise
    /// the app's View Only default separately.
    private func liveScene(liveness: WorkspaceSSHLiveness = .standard,
                           creation: TerminalSessionCreation = .onSelect,
                           muxCommand: String? = nil, directAfterMux: Bool = false,
                           machineIDs: [String] = ["alpha", "beta"],
                           autoConnect: Bool = false, relayPort: UInt16? = nil,
                           initialMuxAccessMode: TerminalMuxAccessMode = .interactive) async throws -> LiveScene {
        let environment = ProcessInfo.processInfo.environment
        guard let sshPort = environment["FT_LIVE_SSH_PORT"].flatMap(UInt16.init), let user = environment["FT_LIVE_SSH_USER"],
              let pem = environment["FT_LIVE_SSH_KEY_PEM_B64"].flatMap({ Data(base64Encoded: $0) }),
              let hostBlob = environment["FT_LIVE_SSH_HOST_KEY_B64"].flatMap({ Data(base64Encoded: $0) }),
              let outPath = environment["FT_LIVE_SSH_OUT"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_SSH_{PORT,USER,KEY_PEM_B64,HOST_KEY_B64,OUT} to run against a live sshd")
        }
        let port = relayPort ?? sshPort
        let crypto = AppleCryptography()
        let key = try PrivateKeyRecord.fromPEM(pem, credentialID: "key", using: crypto)
        let identity = try SSHPublicIdentity(blob: key.publicBlob), host = try SSHPublicIdentity(blob: hostBlob)
        let archive = try AirGapArchive(workspace: document(identity: identity, host: host, port: port, user: user,
                                                            creation: creation, muxCommand: muxCommand,
                                                            directAfterMux: directAfterMux, machineIDs: machineIDs),
                                        keys: [key], using: crypto)
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("scene-" + UUID().uuidString)
        let vault = SceneMemoryVault()
        let store = WorkspaceStore(persistence: try FileWorkspacePersistence(directory: root.appendingPathComponent("store")),
                                   vault: vault)
        let installed = try await store.install(archive, envelopeDigest: crypto.sha256(try archive.encoded()),
            transferID: Data(repeating: 9, count: 16), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let journalIdentifier = "scene-test-" + UUID().uuidString
        let machines = machineIDs.map { id in
            let direct = TerminalSessionRoute(id: "route-\(id)", credentialIDs: ["key"], host: "127.0.0.1", port: port,
                                              sessionCreation: creation)
            let mux = TerminalSessionRoute(id: directAfterMux ? "route-\(id)-mux" : "route-\(id)", credentialIDs: ["key"],
                                           host: "127.0.0.1", port: port, mux: true, sessionCreation: .existingOnly)
            let routes = muxCommand == nil ? [direct] : directAfterMux ? [mux, direct] : [mux]
            return SceneMachine(id: id, name: id.capitalized, target: .init(workspaceID: installed.id, revision: installed.revision,
                machineID: id, machineName: id.capitalized, routes: routes), unavailable: "", theme: nil)
        }
        func launch() async throws -> LiveScene {
            let connections = try MobileConnections(store: store, journalDirectory: root.appendingPathComponent("journal"),
                                                    journalIdentifier: journalIdentifier,
                                                    signer: SceneMemorySigner(vault: vault), liveness: liveness)
            await connections.activate()
            let scene = try WorkspaceTerminalScene(workspaceID: installed.id, revision: installed.revision,
                                                   workspaceName: "Scene", machines: machines, initialMachineID: "alpha",
                                                   connections: connections, autoConnect: autoConnect,
                                                   watchesNetworkPath: false, initialMuxAccessMode: initialMuxAccessMode)
            try await appear(scene, selecting: "alpha")
            return LiveScene(connections: connections, scene: scene, outPath: outPath, relaunch: launch)
        }
        return try await launch()
    }

    /// Appearing commits its group and selection asynchronously, and the scene
    /// refuses terminal actions until it has.
    private func appear(_ scene: WorkspaceTerminalScene, selecting machineID: String) async throws {
        scene.appear(selecting: machineID, active: true)
        for _ in 0..<400 where !scene.permitsTerminalCreation {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertTrue(scene.permitsTerminalCreation, "the scene finished appearing")
        XCTAssertEqual(scene.selectedMachineID, machineID)
    }

    /// Existing display/input tests deliberately create a terminal, including
    /// offscreen terminals. Mere background authentication is tested separately.
    @discardableResult
    private func connect(_ scene: WorkspaceTerminalScene, _ machine: String) async throws -> TerminalSessionController {
        scene.openTerminal(machine)
        let session = try XCTUnwrap(scene.sessions[machine])
        for _ in 0..<400 where session.phase == .idle || session.phase == .connecting {
            try await Task.sleep(for: .milliseconds(50))
        }
        // A session goes live before the machine on screen gets its view.
        for _ in 0..<400 where session.phase == .live && session.terminal == nil && scene.shows(machineID: machine) {
            try await Task.sleep(for: .milliseconds(25))
        }
        return session
    }

    func testPreparedBackgroundConnectionOpensOnlyAfterCommittedSelection() async throws {
        let live = try await liveScene(), scene = live.scene
        scene.connect("beta")
        let session = try XCTUnwrap(scene.sessions["beta"])
        for _ in 0..<400 where session.phase == .idle || session.phase == .connecting {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertEqual(session.phase, .ready)
        XCTAssertNil(session.terminal)
        let prepared = await live.connections.manager.connections()
        XCTAssertEqual(prepared.count, 1)
        let before = try await live.connections.journal.records()
        XCTAssertTrue(before.isEmpty, "preparing a connection creates no terminal epoch")
        let accounting = await live.connections.coordinator.accounting()
        XCTAssertEqual(accounting.connections, 0, "authentication does not publish a terminal source")

        scene.select("beta")
        for _ in 0..<400 where session.phase != .live || session.terminal == nil {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertTrue(scene.sessions["beta"] === session)
        XCTAssertEqual(session.phase, .live)
        let opened = await live.connections.manager.connections()
        XCTAssertEqual(opened.count, 1)
        if !session.shellOnly {
            XCTAssertEqual(opened.map(\.id), prepared.map(\.id), "selection uses the authenticated owner")
        }
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        try await type("echo PREPARED-$((40+2)) > \(live.outPath)", into: terminal)
        let output = try await hostFile(live.outPath, containing: "PREPARED-42")
        XCTAssertTrue(output.contains("PREPARED-42"))
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    func testManualConnectionNeedsExplicitTerminalActionAndCoalescesRepeatedActivation() async throws {
        let live = try await liveScene(creation: .manual), scene = live.scene
        scene.connect("alpha")
        let session = try XCTUnwrap(scene.sessions["alpha"])
        for _ in 0..<400 where session.phase == .idle || session.phase == .connecting {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertEqual(session.phase, .ready)
        await session.activateTerminal()
        XCTAssertEqual(session.phase, .ready, "selection does not upgrade a manual route")
        scene.setForeground(false)
        await session.activateTerminal(explicit: true)
        XCTAssertEqual(session.phase, .ready, "even an explicit request waits for foreground admission")
        let before = try await live.connections.journal.records()
        XCTAssertTrue(before.isEmpty)
        let prepared = await live.connections.manager.connections()
        scene.setForeground(true)
        await withTaskGroup(of: Void.self) { group in
            for _ in 0..<16 { group.addTask { await session.activateTerminal(explicit: true) } }
            await group.waitForAll()
        }
        XCTAssertEqual(session.phase, .live)
        let opened = await live.connections.manager.connections()
        XCTAssertEqual(opened.count, 1)
        if !session.shellOnly { XCTAssertEqual(opened.map(\.id), prepared.map(\.id)) }
        let records = try await live.connections.journal.records()
        XCTAssertEqual(terminalRecords(records).count, 1, "repeated actions open one terminal epoch: \(describe(records))")
        XCTAssertEqual(records.count, session.shellOnly ? 2 : 1, describe(records))
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    func testClosingPreparedConnectionRetiresItWithoutCreatingATerminal() async throws {
        let live = try await liveScene(creation: .manual), scene = live.scene
        scene.connect("alpha")
        let session = try XCTUnwrap(scene.sessions["alpha"])
        for _ in 0..<400 where session.phase == .idle || session.phase == .connecting {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertEqual(session.phase, .ready)
        await scene.close()
        await session.activateTerminal(explicit: true)
        await assertReleased(session, connections: live.connections)
        let records = try await live.connections.journal.records()
        let connections = await live.connections.manager.connections()
        XCTAssertTrue(records.isEmpty)
        XCTAssertTrue(connections.isEmpty)
    }

    /// Puts a live session's view in the key window (rendering and the caret
    /// need one) and returns it.
    private func shown(_ session: TerminalSessionController) throws -> NativeTerminalView {
        XCTAssertEqual(session.phase, .live)
        let terminal = try XCTUnwrap(session.terminal)
        let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
        let window = try XCTUnwrap(scenes.flatMap(\.windows).first { $0.isKeyWindow } ?? scenes.first?.windows.first)
        terminal.frame = CGRect(x: 0, y: 0, width: min(window.bounds.width, 402), height: 300)
        window.addSubview(terminal)
        return terminal
    }

    /// The host file the remote shells write, once it contains `marker` (or after ~10 s).
    private func hostFile(_ path: String, containing marker: String) async throws -> String {
        var text = ""
        for _ in 0..<200 where !text.contains(marker) {
            try await Task.sleep(for: .milliseconds(50))
            text = (try? String(contentsOfFile: path, encoding: .utf8)) ?? ""
        }
        return text
    }

    /// Switches the scene the way the rail does and waits for the published focus.
    private func select(_ scene: WorkspaceTerminalScene, _ machine: String) async throws {
        let plan = try XCTUnwrap(try scene.navigation.navigation.plan(machineID: machine))
        try await scene.navigation.perform(plan)
        XCTAssertEqual(scene.selectedMachineID, machine)
        XCTAssertEqual(scene.navigation.phase, .ready)
    }

    /// Switches to a machine whose session connected in the background: it
    /// did not take the screen, and its view attaches on the switch.
    private func shownAfterSelecting(_ scene: WorkspaceTerminalScene, _ machine: String,
                                     connected session: TerminalSessionController) async throws -> NativeTerminalView {
        XCTAssertEqual(session.phase, .live)
        XCTAssertNotEqual(scene.selectedMachineID, machine, "a background connection leaves the screen alone")
        XCTAssertNil(session.terminal, "its view waits until the machine is shown")
        let plan = try XCTUnwrap(try scene.navigation.navigation.plan(machineID: machine))
        try await scene.navigation.perform(plan)
        for _ in 0..<200 where session.terminal == nil || scene.navigation.phase != .ready {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertEqual(scene.selectedMachineID, machine)
        return try shown(session)
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
        // Which of the two conditions failed, so a rare miss names its cause.
        let frame = terminal.renderer.admittedFrame, keyboard = terminal.input.session
        XCTFail("the native input view never became editable (keyboard \(keyboard.phase), "
            + "last failure \(String(describing: keyboard.lastFailure)), live \(String(describing: frame?.isLive)), "
            + "cursor \(String(describing: frame?.cursor)), viewport \(String(describing: frame.map { ($0.viewportFirst, $0.rows, $0.columns) })), "
            + "bounds \(terminal.bounds.size))")
    }

    func testTerminalUsesTheBundledDesktopFontWithItsLicenses() {
        XCTAssertEqual(TerminalSessionController.renderConfiguration.fontName, "PragmasevkaNF")
        for face in ["PragmasevkaNF", "PragmasevkaNF-Bold", "PragmasevkaNF-Italic", "PragmasevkaNF-BoldItalic"] {
            XCTAssertNotNil(UIFont(name: face, size: 12), "\(face) must be bundled and registered")
        }
        // Prompt symbols come from the font itself, not only from the fallback.
        let font = CTFontCreateWithName("PragmasevkaNF" as CFString, 12, nil)
        var character: UniChar = 0xE0B0, glyph = CGGlyph(0)
        XCTAssertTrue(CTFontGetGlyphsForCharacters(font, &character, &glyph, 1))
        XCTAssertNotNil(Bundle.main.url(forResource: "Pragmasevka-LICENSE", withExtension: "md"))
        XCTAssertNotNil(Bundle.main.url(forResource: "NerdFonts-LICENSE", withExtension: "txt"))
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
        let live = try await liveScene(), outPath = live.outPath, connections = live.connections
        let terminal = try shown(try await connect(live.scene, "alpha"))
        defer { terminal.removeFromSuperview() }
        try await type("stty size > \(outPath); echo FRANKEN-$((6*7)) >> \(outPath)", into: terminal)
        let output = try await hostFile(outPath, containing: "FRANKEN-42")
        XCTAssertTrue(output.contains("FRANKEN-42"), "the remote shell never ran the typed command (output: \(output))")
        let size = output.split(separator: "\n").first.map(String.init) ?? ""
        let dimensions = size.split(separator: " ").compactMap { Int($0) }
        XCTAssertEqual(dimensions.count, 2, "stty must report rows and columns (output: \(output))")
        XCTAssertTrue(dimensions.allSatisfy { $0 > 0 }, size)
        XCTAssertNotEqual(size, "24 80", "the PTY must follow the view's viewport, not the provisional 80x24")

        let format = UIGraphicsImageRendererFormat(); format.scale = 2
        let image = UIGraphicsImageRenderer(bounds: terminal.bounds, format: format).image { _ in
            _ = terminal.drawHierarchy(in: terminal.bounds, afterScreenUpdates: true)
        }
        let attachment = XCTAttachment(image: image); attachment.name = "scene-controller-terminal"
        attachment.lifetime = .keepAlways; add(attachment)

        await live.scene.close()
        let remaining = await connections.manager.connections()
        XCTAssertTrue(remaining.isEmpty, "closing the scene must settle the connection through the manager")
        await assertReleased(try XCTUnwrap(live.scene.sessions["alpha"]), connections: connections)
        let history = try await connections.journal.records()
        let record = try XCTUnwrap(terminalRecords(history).first, describe(history))
        XCTAssertEqual(terminalRecords(history).count, 1, describe(history))
        XCTAssertEqual(record.state, .interrupted, describe(history))
        XCTAssertTrue(record.canResolve)
        XCTAssertTrue(record.inputEvidence.contains(.transportAccepted))
        XCTAssertTrue(record.inputEvidence.mayHaveRemoteEffect)
    }

    func testDirectSSHMouseUsesCanonicalBytesWithKeyboardHiddenAndCannotReviveAfterDeactivation() async throws {
        let live = try await liveScene()
        let session = try await connect(live.scene, "alpha")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        let run = UUID().uuidString.prefix(8), done = "MOUSE-DONE-\(run)"
        let expected = Data("\u{1b}[<0;3;2M\u{1b}[<0;3;2m\u{1b}[<64;3;2M".utf8)
        let path = "'" + live.outPath.replacingOccurrences(of: "'", with: "'\\''") + "'"
        // The remote process records real PTY bytes. A single click is both
        // SGR transitions, followed by the wheel report in the same input FIFO.
        try await type("stty raw -echo; printf '\\033[?1000h\\033[?1006h'; "
            + "dd bs=1 count=\(expected.count) 2>/dev/null | od -An -tx1 | tr -d ' \\n' > \(path); "
            + "stty sane; printf '\\033[?1000l\\033[?1006l'; printf '\\n\(done)\\n' >> \(path)", into: terminal)
        for _ in 0..<200 where !terminal.remoteMouseGrabbed || terminal.renderer.pointerProjection == nil {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertTrue(terminal.remoteMouseGrabbed)
        let oldInput = terminal.input
        oldInput.hideKeyboard()
        for _ in 0..<200 where terminal.input === oldInput || !terminal.input.session.acceptsPointer
            || terminal.renderer.pointerProjection == nil {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertFalse(terminal.input === oldInput, "hidden pointer input owns a new responder generation")
        XCTAssertTrue(terminal.input.session.acceptsPointer)
        XCTAssertFalse(terminal.input.isFirstResponder, "pointer focus must not reopen the software keyboard")
        XCTAssertEqual(terminal.input.session.originalFocus.source, terminal.source)
        oldInput.insertText("late callback")
        let click = try XCTUnwrap(terminal.onRemoteClick), wheel = try XCTUnwrap(terminal.onRemoteScroll)
        click(2, 1); wheel(-1, 2, 1)
        await terminal.input.session.waitUntilSettled()
        let output = try await hostFile(live.outPath, containing: done)
        let hex = expected.map { String(format: "%02x", $0) }.joined()
        XCTAssertEqual(output.split(separator: "\n").map(String.init), [hex, done],
                       "the remote shell must receive exactly one complete click and one wheel report")
        XCTAssertEqual(session.phase, .live)

        // Interleave another intentional hide with an entire inactive/active
        // cycle before its activation task can run. The old request is final.
        let hiddenInput = terminal.input, activity = terminal.sceneActivityGeneration
        hiddenInput.hideKeyboard()
        terminal.setSceneActive(false); terminal.setSceneActive(true)
        await hiddenInput.session.waitUntilSuspended()
        for _ in 0..<20 { await Task.yield() }
        XCTAssertNotEqual(terminal.sceneActivityGeneration, activity)
        XCTAssertTrue(terminal.input === hiddenInput)
        XCTAssertFalse(terminal.input.session.acceptsPointer)
        XCTAssertFalse(terminal.input.isFirstResponder)
        await live.scene.activateKeyboard()
        for _ in 0..<200 where !terminal.input.isEditable || !terminal.input.isFirstResponder {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertTrue(terminal.input.isFirstResponder, "the explicit Keyboard action can create a later focus")
        await live.scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// An input method composes on the phone and the shell receives only what
    /// it commits, as UTF-8: a Japanese conversion committed with its chosen
    /// text, a dead key replaced by its letter, and a candidate committed as
    /// it stands. The calls are UIKit's, in the order a keyboard makes them.
    func testComposedTextReachesTheShellOnlyWhenCommitted() async throws {
        let live = try await liveScene()
        let session = try await connect(live.scene, "alpha")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        let input = terminal.input
        for _ in 0..<200 where !input.isEditable { try await Task.sleep(for: .milliseconds(50)) }
        XCTAssertTrue(input.isEditable, "the native input view never became editable")
        let run = UUID().uuidString.prefix(8)
        input.insertText("printf '%s\\n' IME-\(run) ")
        for candidate in ["k", "か", "かn", "かん", "かんj", "かんじ", "漢字"] {
            input.setMarkedText(candidate, selectedRange: NSRange(location: (candidate as NSString).length, length: 0))
        }
        input.insertText("漢字")
        input.insertText(" ")
        input.setMarkedText("´", selectedRange: NSRange(location: 1, length: 0))
        input.insertText("é")
        input.insertText(" ")
        input.setMarkedText("ü", selectedRange: NSRange(location: 1, length: 0))
        input.unmarkText()
        input.insertText(" END-\(run) > \(live.outPath)"); input.insertText("\n")
        let output = try await hostFile(live.outPath, containing: "END-\(run)")
        XCTAssertEqual(output.split(separator: "\n").map(String.init), ["IME-\(run)", "漢字", "é", "ü", "END-\(run)"],
                       "only committed text reaches the shell (output: \(output))")
        await live.scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// An iPad on a large display at the smallest text size has room for more
    /// cells than the terminal can keep both screens of. The grid takes the
    /// full width and as many rows as fit; it used to give up columns to keep
    /// 256 history rows.
    func testALayoutPastTheCellBudgetShowsTheLargestGridThatFits() async throws {
        let live = try await liveScene()
        let session = try await connect(live.scene, "alpha")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        // History for the larger grid to trim.
        try await type("seq 1 400", into: terminal)
        _ = try await screen(terminal, containing: "400")
        let height = 1200.0
        let frame = try await enlarge(terminal, to: CGSize(width: 1800, height: height), beyond: 300)
        let cell = try XCTUnwrap(terminal.renderer.cellSize)
        XCTAssertGreaterThan(Int(height / cell.height) * frame.columns, TerminalSessionGeometry.maximumShownCells,
                             "the view has room for more cells than the terminal keeps")
        XCTAssertLessThanOrEqual(frame.rows * frame.columns, TerminalSessionGeometry.maximumShownCells)
        try await type("stty size > \(live.outPath); echo FIT-$((6*7)) | tee -a \(live.outPath)", into: terminal)
        let output = try await hostFile(live.outPath, containing: "FIT-42")
        XCTAssertEqual(output.split(separator: "\n").first.map(String.init), "\(frame.rows) \(frame.columns)",
                       "the remote shell has the grid the phone draws (output: \(output))")
        let drawn = try await screen(terminal, containing: "FIT-42")
        XCTAssertTrue(drawn.contains("FIT-42"), "the larger grid shows the shell's output (screen: \(drawn))")
        XCTAssertEqual(session.phase, .live)
        await live.scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// A shell that exits ends the session from inside the pump task; teardown
    /// must still finish there, and the next session on the machine needs the
    /// user's acknowledgement of the first one's unknown outcome.
    func testRemoteExitSettlesAndANewSessionStartsWithoutAReview() async throws {
        let live = try await liveScene(), connections = live.connections, scene = live.scene
        let first = try await connect(scene, "alpha")
        let terminal = try shown(first)
        defer { terminal.removeFromSuperview() }
        try await type("printf 'FINAL-%s\\n' $((6*7)); exit", into: terminal)
        for _ in 0..<200 where !first.settled { try await Task.sleep(for: .milliseconds(50)) }
        XCTAssertEqual(first.phase, .ended(TerminalSessionController.describe(.completed(.init(remoteExit: nil, channelClosed: true)))))
        XCTAssertTrue(first.settled, "teardown after a remote exit must finish")
        let retained = try XCTUnwrap(first.retainedOutput, "the ended session keeps its received live viewport")
        XCTAssertEqual(retained.source, terminal.source)
        XCTAssertTrue(retained.document.text.contains("FINAL-42"), "final remote output survives model disposal")
        XCTAssertLessThanOrEqual(retained.document.utf8Bytes, 128 * 1024)
        XCTAssertNil(first.retainedOutputFailure)
        XCTAssertFalse(terminal.input.isEditable, "retained output carries no keyboard authority")
        let remaining = await connections.manager.connections()
        XCTAssertTrue(remaining.isEmpty)
        XCTAssertEqual(scene.navigation.navigation.machines.first?.panes.count, 0, "the ended source leaves navigation")
        await assertReleased(first, connections: connections)

        // The ended session's record is acknowledged as the next one starts.
        let earlier = Set(try await connections.journal.records().map(\.id))
        XCTAssertFalse(earlier.isEmpty)
        let second = try await connect(scene, "alpha")
        XCTAssertNil(first.retainedOutput, "replacing a controller releases its volatile output")
        let again = try shown(second)
        defer { again.removeFromSuperview() }
        try await type("echo AGAIN-$((40+2)) > \(live.outPath)", into: again)
        let output = try await hostFile(live.outPath, containing: "AGAIN-42")
        XCTAssertTrue(output.contains("AGAIN-42"), "the new session must run commands (output: \(output))")
        // The new session's own record stays open while it is live.
        let records = try await connections.journal.records()
        let unresolved = records.filter { earlier.contains($0.id) && $0.requiresReview }
        XCTAssertTrue(unresolved.isEmpty, "starting the new session acknowledged the earlier one: \(describe(records))")
        XCTAssertEqual(terminalRecords(records).filter(\.requiresReview).count, 1, describe(records))
        await scene.close()
        XCTAssertTrue(second.settled)
        await assertReleased(second, connections: connections)
    }

    /// After the app launches again, the machine page's Connect opens a new
    /// shell: the earlier launch's session (cut off when iOS ended the app)
    /// is acknowledged, not reported as still closing.
    func testConnectAfterRelaunchOpensANewShell() async throws {
        let first = try await liveScene()
        let earlier = try await connect(first.scene, "alpha")
        XCTAssertEqual(earlier.phase, .live)
        await first.scene.close()
        await assertReleased(earlier, connections: first.connections)
        let ended = try await first.connections.journal.records().filter(\.requiresReview)
        XCTAssertFalse(ended.isEmpty, "the earlier session's outcome is not yet acknowledged")

        let live = try await first.relaunch(), scene = live.scene
        scene.connect("alpha")
        let session = try XCTUnwrap(scene.sessions["alpha"])
        for _ in 0..<400 where session.phase != .live && !session.settled {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertEqual(session.phase, .live)
        // A session goes live before the machine on screen gets its view.
        for _ in 0..<400 where session.terminal == nil && scene.shows(machineID: "alpha") {
            try await Task.sleep(for: .milliseconds(25))
        }
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        try await type("echo RELAUNCHED-$((40+2)) > \(live.outPath)", into: terminal)
        let output = try await hostFile(live.outPath, containing: "RELAUNCHED-42")
        XCTAssertTrue(output.contains("RELAUNCHED-42"), "the new shell must run commands")
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// M1: two machines stay connected while the scene switches between them,
    /// and typed input reaches exactly the selected machine's shell. Each shell
    /// remembers its own FT_WHO, so the host file records which shell ran each
    /// command, in switching order; a stale input view of the machine that lost
    /// focus cannot inject anything.
    func testSwitchingMachinesKeepsBothSessionsAndRoutesInputToTheSelectedOne() async throws {
        let live = try await liveScene(), scene = live.scene, path = live.outPath + ".machines"
        let alpha = try shown(try await connect(scene, "alpha"))
        defer { alpha.removeFromSuperview() }
        try await type("export FT_WHO=alpha", into: alpha)
        let beta = try await shownAfterSelecting(scene, "beta", connected: try await connect(scene, "beta"))
        defer { beta.removeFromSuperview() }
        try await type("export FT_WHO=beta", into: beta)
        XCTAssertEqual(scene.navigation.navigation.machines.map(\.panes.count), [1, 1])

        try await select(scene, "alpha")
        try await type("echo SEEN-$FT_WHO > \(path)", into: alpha)
        _ = try await hostFile(path, containing: "SEEN-alpha")
        let staleBetaInput = beta.input
        XCTAssertFalse(staleBetaInput.isEditable, "the unselected machine accepts no input")
        staleBetaInput.insertText("echo LEAK-$FT_WHO >> \(path)\n")

        try await select(scene, "beta")
        XCTAssertFalse(alpha.input.isEditable)
        try await type("echo SEEN-$FT_WHO >> \(path)", into: beta)
        _ = try await hostFile(path, containing: "SEEN-beta")
        try await select(scene, "alpha")
        try await type("echo SEEN-$FT_WHO >> \(path)", into: alpha)
        let lines = try await hostFile(path, containing: "SEEN-beta\nSEEN-alpha").split(separator: "\n")
        XCTAssertEqual(lines, ["SEEN-alpha", "SEEN-beta", "SEEN-alpha"], "input must follow the selection exactly")

        await scene.close()
        let remaining = await live.connections.manager.connections()
        XCTAssertTrue(remaining.isEmpty)
        for session in scene.sessions.values { await assertReleased(session, connections: live.connections) }
    }

    /// A layout change (the keyboard's bar on iPad, a window resized) redraws
    /// the live screen; keys typed before its caret is drawn again still run.
    func testKeysTypedWhileTheScreenIsRedrawnReachTheShell() async throws {
        let live = try await liveScene(), scene = live.scene
        let session = try await connect(scene, "alpha")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        try await type("echo READY-$((40+2))", into: terminal)
        _ = try await screen(terminal, containing: "READY-42")

        terminal.frame.size.height -= 120
        terminal.layoutIfNeeded()
        XCTAssertFalse(terminal.input.isEditable, "the caret waits for the redrawn screen")
        terminal.input.insertText("echo REDRAWN-$((40+2)) > \(live.outPath)"); terminal.input.insertText("\n")
        let output = try await hostFile(live.outPath, containing: "REDRAWN-42")
        XCTAssertTrue(output.contains("REDRAWN-42"), "keys typed during the redraw must reach the shell")
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// The rendered screen once it shows `marker` (or after ~10 s).
    private func screen(_ terminal: NativeTerminalView, containing marker: String) async throws -> String {
        var text = ""
        for _ in 0..<200 where !text.contains(marker) {
            try await Task.sleep(for: .milliseconds(50))
            text = terminal.renderer.accessibilityValue ?? ""
        }
        return text
    }

    /// How long one key typed at the shell's prompt takes to show on the
    /// rendered screen as the shell echoes it.
    private func echo(into terminal: NativeTerminalView) async throws -> Duration {
        func shown() -> Int { (terminal.renderer.accessibilityValue ?? "").filter { $0 == "Q" }.count }
        let before = shown(), sent = ContinuousClock.now
        terminal.input.insertText("Q")
        while shown() <= before, ContinuousClock.now - sent < .seconds(2) { try await Task.sleep(for: .milliseconds(2)) }
        let elapsed = ContinuousClock.now - sent
        try await Task.sleep(for: .milliseconds(100))
        return elapsed
    }

    /// Plan 23.2 asks for an echo within the round trip (loopback here) plus
    /// 50 ms at p95; this shared simulator holds the median to that.
    private func assertPromptEcho(_ samples: [Duration], _ context: String) {
        let sorted = samples.sorted(), median = sorted[sorted.count / 2], p95 = sorted[sorted.count * 95 / 100]
        XCTAssertLessThan(median, .milliseconds(50), "keys echoed slowly \(context): median \(median), p95 \(p95)")
        XCTAssertLessThan(p95, .milliseconds(100), "keys echoed slowly \(context): median \(median), p95 \(p95)")
    }

    /// The app's memory as the system counts it against its limit.
    private func footprint() -> UInt64 {
        var info = task_vm_info_data_t()
        var count = mach_msg_type_number_t(MemoryLayout<task_vm_info_data_t>.size / MemoryLayout<natural_t>.size)
        let result = withUnsafeMutablePointer(to: &info) {
            $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
                task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), $0, &count)
            }
        }
        return result == KERN_SUCCESS ? info.phys_footprint : 0
    }

    private func button(_ identifier: String, in view: UIView?) -> UIButton? {
        guard let view else { return nil }
        if let button = view as? UIButton, button.accessibilityIdentifier == identifier { return button }
        return view.subviews.lazy.compactMap { self.button(identifier, in: $0) }.first
    }

    /// Lays the terminal out at `size` points with the smallest text, as an
    /// iPad on a large display can, and returns the first frame drawn wider
    /// than `columns`.
    private func enlarge(_ terminal: NativeTerminalView, to size: CGSize,
                         beyond columns: Int) async throws -> TerminalRenderFrame {
        terminal.frame = CGRect(origin: .zero, size: size)
        terminal.setFontPointSize(NativeTerminalView.fontSizes.lowerBound)
        for _ in 0..<200 where (terminal.renderer.admittedFrame?.columns ?? 0) <= columns {
            try await Task.sleep(for: .milliseconds(50))
        }
        let frame = try XCTUnwrap(terminal.renderer.admittedFrame)
        XCTAssertGreaterThan(frame.columns, columns, "the terminal never drew the larger grid (drawable "
            + "\(terminal.renderer.drawableSize), cell \(String(describing: terminal.renderer.cellSize)))")
        return frame
    }

    /// The first view of `type` inside `root` matching `match`, waiting up to ~5 s.
    private func subview<T: UIView>(_ type: T.Type, in root: UIView,
                                    where match: @escaping (T) -> Bool = { _ in true }) async throws -> T {
        func search(_ view: UIView) -> T? {
            if let found = view as? T, match(found) { return found }
            return view.subviews.lazy.compactMap(search).first
        }
        for _ in 0..<100 {
            if let found = search(root) { return found }
            try await Task.sleep(for: .milliseconds(50))
        }
        return try XCTUnwrap(search(root), "no \(T.self) appeared")
    }

    private func tap(_ title: String, in root: UIView) async throws {
        try await subview(UIButton.self, in: root) { $0.configuration?.title == title }.sendActions(for: .touchUpInside)
    }

    /// Select Output freezes the received viewport the shell drew, copies it
    /// from the panel, and returning to the terminal gives it a working keyboard.
    func testSelectOutputShowsTheShellsOutputAndCopiesIt() async throws {
        let live = try await liveScene(), scene = live.scene
        let session = try await connect(scene, "alpha")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        try await type("echo CLIP-$((6*7))", into: terminal)
        let drawn = try await screen(terminal, containing: "CLIP-42")
        XCTAssertTrue(drawn.contains("CLIP-42"), "the shell's output must be drawn first (screen: \(drawn))")

        UIPasteboard.general.string = "unrelated"
        session.inspectOutput()
        let captured = try await subview(UITextView.self, in: terminal) { $0.accessibilityLabel == "Captured terminal output" }
        XCTAssertTrue(session.clipboardIsPresenting)
        XCTAssertTrue(captured.text.contains("CLIP-42"), "the panel shows the received viewport (text: \(captured.text ?? ""))")
        XCTAssertFalse(terminal.input.isEditable, "the hidden shell accepts no input while output is inspected")
        try await tap("Select All", in: terminal)
        try await tap("Copy Selection", in: terminal)
        XCTAssertTrue(UIPasteboard.general.string?.contains("CLIP-42") == true, "Copy Selection writes the selected output")
        try await tap("Done", in: terminal)
        XCTAssertFalse(session.clipboardIsPresenting)
        XCTAssertNil(captured.window, "Done removes the panel")

        await scene.activateKeyboard()
        try await type("echo BACK-$((40+2)) > \(live.outPath)", into: terminal)
        let output = try await hostFile(live.outPath, containing: "BACK-42")
        XCTAssertTrue(output.contains("BACK-42"), "the terminal takes input again after inspection (output: \(output))")
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// Where `marker` starts on the drawn screen, with the frame that shows it,
    /// waiting up to ~10 s. ASCII only: a character's offset in a row's text
    /// is its column.
    private func cell(of marker: String, in terminal: NativeTerminalView) async throws
        -> (TerminalGridPoint, TerminalRenderFrame) {
        func find() -> (TerminalGridPoint, TerminalRenderFrame)? {
            guard let frame = terminal.renderer.admittedFrame else { return nil }
            for row in frame.lines {
                let text = row.readableText
                if let range = text.range(of: marker) {
                    return (.init(row: row.stableIndex, column: text.distance(from: text.startIndex, to: range.lowerBound)), frame)
                }
            }
            return nil
        }
        for _ in 0..<200 {
            if let found = find() { return found }
            try await Task.sleep(for: .milliseconds(50))
        }
        return try XCTUnwrap(find(), "the screen never showed \(marker) (screen: \(terminal.renderer.accessibilityValue ?? ""))")
    }

    /// What a tap finds on a program's link (OSC 8), on an address in plain
    /// text, and at either end of an address that wraps onto exactly three
    /// rows, the prompt right below it: each destination whole, and nothing
    /// of the prompt. Markers carry this run's id; a mux pane keeps the rows
    /// earlier runs printed.
    private func assertLinks(in terminal: NativeTerminalView, _ context: String) async throws {
        let run = UUID().uuidString.prefix(8)
        try await type(#"printf '\033]8;;https://example.com/pr/%d\033\\LINK-\#(run)-%d\033]8;;\033\\ and\nhttps://example.com/\#(run)/ci/%d\n' $((6*7)) $((6*7)) $((6*7))"#,
                       into: terminal)
        func destination(_ at: TerminalGridPoint, in frame: TerminalRenderFrame) -> String? {
            TerminalLink.at(at, in: frame)?.destination
        }
        func destination(at marker: String, offset: Int) async throws -> String? {
            let (at, frame) = try await cell(of: marker, in: terminal)
            return destination(.init(row: at.row, column: at.column + offset), in: frame)
        }
        let program = try await destination(at: "LINK-\(run)-42", offset: 3)
        XCTAssertEqual(program, "https://example.com/pr/42", "a program's link \(context)")
        let between = try await destination(at: "LINK-\(run)-42", offset: 17)
        XCTAssertNil(between, "the text after a program's link \(context)")
        let plain = try await destination(at: "https://example.com/\(run)/ci/42", offset: 12)
        XCTAssertEqual(plain, "https://example.com/\(run)/ci/42", "an address in the text \(context)")

        // 36 characters around the w's: the address fills three rows exactly.
        let columns = try XCTUnwrap(terminal.renderer.admittedFrame?.columns)
        let ws = 3 * columns - 36
        try await type(#"printf 'https://example.com/\#(run)/%s/end-%d\n' $(head -c \#(ws) /dev/zero | tr '\0' w) $((6*7))"#,
                       into: terminal)
        let long = "https://example.com/\(run)/\(String(repeating: "w", count: ws))/end-42"
        let (first, frame) = try await cell(of: "https://example.com/\(run)/w", in: terminal)
        XCTAssertEqual(first.column, 0)
        XCTAssertEqual(long.count, 3 * frame.columns)
        let last = TerminalGridPoint(row: first.row + 2, column: frame.columns - 1)
        XCTAssertEqual(destination(first, in: frame), long, "the first row of a wrapped address \(context)")
        XCTAssertEqual(destination(last, in: frame), long, "the last row of a wrapped address \(context)")
    }

    /// A tap on a link the shell prints, or on an address in its output, finds
    /// where it goes, over a direct SSH session.
    func testLinksTheShellPrintsAreFoundWhereTheyAreDrawn() async throws {
        let live = try await liveScene(), scene = live.scene
        let session = try await connect(scene, "alpha")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        try await assertLinks(in: terminal, "over SSH")
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// The same through a mux pane: the mirror redraws the pane's rows with
    /// their links and their wraps.
    func testAMuxPanesLinksAreFoundInTheRowsTheMirrorRedraws() async throws {
        guard let command = ProcessInfo.processInfo.environment["FT_LIVE_MUX_COMMAND"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_MUX_COMMAND to reach a disposable mux on the loopback host")
        }
        let live = try await liveScene(muxCommand: command), scene = live.scene
        let session = try await connect(scene, "alpha")
        XCTAssertNotNil(session.muxControl)
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        try await assertLinks(in: terminal, "through the mux")
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// Paste shows the exact clipboard text bound to this terminal. Cancel sends
    /// nothing; confirmation sends it once, without a Return.
    func testReviewedPasteIsSentOnlyOnConfirmationAndAddsNoReturn() async throws {
        let live = try await liveScene(), scene = live.scene, path = live.outPath
        let session = try await connect(scene, "alpha")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        try await type("echo READY-$((40+2)) > \(path)", into: terminal)
        _ = try await hostFile(path, containing: "READY-42")

        let cancelled = "echo CANCELLED-$((40+2)) >> \(path)"
        UIPasteboard.general.string = cancelled
        session.paste()
        let preview = try await subview(UITextView.self, in: terminal) { $0.accessibilityLabel == "Text to paste" }
        XCTAssertEqual(preview.text, cancelled)
        try await tap("Cancel", in: terminal)
        XCTAssertFalse(session.clipboardIsPresenting)
        try await type("echo AFTER-$((40+2)) >> \(path)", into: terminal)
        let afterCancel = try await hostFile(path, containing: "AFTER-42")
        XCTAssertEqual(afterCancel.split(separator: "\n"), ["READY-42", "AFTER-42"], "a cancelled paste sends nothing")

        let confirmed = "echo PASTE-$((40+2)) >> \(path)"
        UIPasteboard.general.string = confirmed
        session.paste()
        _ = try await subview(UITextView.self, in: terminal) { $0.accessibilityLabel == "Text to paste" && $0.text == confirmed }
        try await tap("Paste to This Terminal", in: terminal)
        XCTAssertFalse(session.clipboardIsPresenting)
        try await Task.sleep(for: .seconds(2))
        let unsubmitted = (try? String(contentsOfFile: path, encoding: .utf8)) ?? ""
        XCTAssertFalse(unsubmitted.contains("PASTE-42"), "a paste adds no Return, so the line has not run")
        terminal.input.insertText("\n")
        let submitted = try await hostFile(path, containing: "PASTE-42")
        XCTAssertEqual(submitted.split(separator: "\n"), ["READY-42", "AFTER-42", "PASTE-42"], "the pasted line runs once")
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// The rendered rows once `ready` holds for them (or after ~10 s).
    private func rows(of terminal: NativeTerminalView, until ready: ([String]) -> Bool) async throws -> [String] {
        var rows: [String] = []
        for _ in 0..<200 {
            rows = (terminal.renderer.accessibilityValue ?? "").components(separatedBy: "\n")
            if ready(rows) { break }
            try await Task.sleep(for: .milliseconds(50))
        }
        return rows
    }

    /// Whether a row shows `marker`. Output that arrives at an idle prompt
    /// shares the prompt's row, so markers are matched within rows.
    private func shows(_ rows: [String], _ marker: String) -> Bool { rows.contains { $0.contains(marker) } }

    /// Prints numbered lines well past one screen and waits for the last.
    private func printHistory(into terminal: NativeTerminalView, count: Int) async throws {
        try await type("for i in $(seq 1 \(count)); do echo HIST-$i; done", into: terminal)
        let bottom = try await rows(of: terminal) { $0.contains("HIST-\(count)") }
        XCTAssertTrue(bottom.contains("HIST-\(count)"), "the shell printed its lines (screen: \(bottom))")
        XCTAssertFalse(bottom.contains("HIST-1"), "the first lines scrolled off the screen")
    }

    /// Search uses the canonical retained rows, fences a late same-machine
    /// keyboard handoff, and returns to the original view without sending text.
    func testHistorySearchRevealsRetainedRowsAndReturnsWithoutHiddenInput() async throws {
        let live = try await liveScene(), scene = live.scene
        let session = try await connect(scene, "alpha")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        try await printHistory(into: terminal, count: 80)
        let inspection = try XCTUnwrap(session.inspectHistory())
        XCTAssertEqual(inspection.source, terminal.source)
        XCTAssertTrue(session.clipboardIsPresenting)
        XCTAssertFalse(terminal.input.isEditable)

        // A navigation handoff admitted just before the sheet opened can
        // publish later. Its new responder must not become live underneath it.
        await scene.owner.joinSealed()
        try await scene.navigation.activateSelection()
        XCTAssertFalse(scene.owner.keyboard?.acceptsInput == true)
        XCTAssertFalse(terminal.input.isEditable)

        inspection.query = "HIST-3"
        inspection.search()
        await inspection.waitUntilSettled()
        XCTAssertNil(inspection.failure)
        XCTAssertEqual(terminal.renderer.admittedFrame?.isLive, true, "reading search pages never moves the viewport")
        let result = try XCTUnwrap(inspection.result)
        let match = try XCTUnwrap(result.matches.first { $0.excerpt.trimmingCharacters(in: .whitespaces) == "HIST-3" })
        inspection.reveal(match)
        await inspection.waitUntilSettled()
        XCTAssertNil(inspection.failure)
        XCTAssertTrue(try XCTUnwrap(inspection.preview).text.contains("HIST-3"))
        let selected = try await rows(of: terminal) { $0.contains("HIST-3") }
        XCTAssertTrue(selected.contains("HIST-3"))

        await inspection.close(restoreViewport: false)
        XCTAssertNil(session.historyInspection)
        XCTAssertNotNil(session.historyReturnBookmark)
        XCTAssertFalse(terminal.input.isEditable, "Show in Terminal does not automatically grant a keyboard")
        session.returnFromHistorySearch()
        for _ in 0..<200 where session.historyReturning { try await Task.sleep(for: .milliseconds(20)) }
        XCTAssertFalse(session.historyReturning)
        XCTAssertNil(session.interactionProblem)
        _ = try await rows(of: terminal) { $0.contains("HIST-80") }
        XCTAssertEqual(terminal.renderer.admittedFrame?.isLive, true)
        await scene.activateKeyboard()
        try await type("echo SEARCHED-$((40+2)) > \(live.outPath)", into: terminal)
        let output = try await hostFile(live.outPath, containing: "SEARCHED-42")
        XCTAssertEqual(output, "SEARCHED-42\n", "explicit keyboard activation still targets the original source")
        await scene.close()
        XCTAssertNil(session.retainedOutput, "closing a workspace drops volatile output")
        await assertReleased(session, connections: live.connections)
    }

    /// Scrollback: earlier output scrolls back into view and stays put while
    /// output continues; typing over history is not sent and returns live.
    func testScrollingBackShowsEarlierOutputAndTypingReturnsLive() async throws {
        let live = try await liveScene(), scene = live.scene
        let session = try await connect(scene, "alpha")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        try await printHistory(into: terminal, count: 80)
        try await type("(sleep 3; echo LATE-$((40+2))) &", into: terminal)
        _ = try await rows(of: terminal) { $0.contains { $0.hasPrefix("[1]") } }

        let frame = try XCTUnwrap(terminal.renderer.admittedFrame)
        let history = try XCTUnwrap(frame.history, "frames report the retained history")
        XCTAssertTrue(frame.isLive)
        terminal.scrollHistory(byRows: -Int(history.live - history.first))
        let oldest = try await rows(of: terminal) { $0.contains("HIST-1") }
        XCTAssertTrue(oldest.contains("HIST-1"), "scrolling back shows the first lines (screen: \(oldest))")
        XCTAssertEqual(terminal.renderer.admittedFrame?.viewportFirst, history.first)
        let control = try await subview(UIButton.self, in: terminal) { $0.accessibilityLabel == "Return to live output" }
        XCTAssertFalse(control.isHidden, "history offers the way back")
        XCTAssertFalse(terminal.input.isEditable, "the caret has no place over history")

        try await Task.sleep(for: .seconds(4))
        let kept = try await rows(of: terminal) { _ in true }
        XCTAssertTrue(kept.contains("HIST-1") && !shows(kept, "LATE-42"), "new output keeps the reader's place (screen: \(kept))")

        terminal.input.insertText("x")
        let returned = try await rows(of: terminal) { self.shows($0, "LATE-42") }
        XCTAssertTrue(shows(returned, "LATE-42"), "typing returns to the live screen (screen: \(returned))")
        XCTAssertEqual(terminal.renderer.admittedFrame?.isLive, true)
        XCTAssertTrue(control.isHidden)
        try await type("echo SCROLLED-$((40+2)) > \(live.outPath)", into: terminal)
        let output = try await hostFile(live.outPath, containing: "SCROLLED-42")
        XCTAssertEqual(output, "SCROLLED-42\n", "the keystroke typed over history was not sent")
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// A theme picked while connected re-themes the live terminal from its
    /// next frame, keeps the screen, and is the palette a new session starts in.
    func testAThemePickedWhileConnectedRethemesTheLiveTerminal() async throws {
        let live = try await liveScene(), scene = live.scene
        let session = try await connect(scene, "alpha")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        try await type("echo THEMED-$((40+2))", into: terminal)
        _ = try await rows(of: terminal) { self.shows($0, "THEMED-42") }
        let before = try XCTUnwrap(terminal.renderer.admittedFrame)

        let catalog = try XCTUnwrap(MobileWorkspaceModel.themeCatalog, "the app bundles its theme catalog")
        let dracula = try XCTUnwrap(catalog.theme(named: "Dracula"))
        let base = dracula.terminalBasePalette
        func entry(_ index: Int) -> TerminalRenderRGBA {
            .init(base[index * 4], base[index * 4 + 1], base[index * 4 + 2], base[index * 4 + 3])
        }
        XCTAssertNotEqual(before.palette[257], entry(257), "the unthemed terminal is not already Dracula")
        scene.applyTheme(dracula, to: "alpha")
        XCTAssertEqual(scene.themes["alpha"]?.name, "Dracula", "the chrome follows at once")
        for _ in 0..<200 where terminal.renderer.admittedFrame?.palette[257] != entry(257) {
            try await Task.sleep(for: .milliseconds(50))
        }
        let after = try XCTUnwrap(terminal.renderer.admittedFrame)
        XCTAssertEqual(after.palette[257], entry(257), "background")
        XCTAssertEqual(after.palette[256], entry(256), "foreground")
        XCTAssertEqual(after.palette[1], entry(1), "ANSI red")
        XCTAssertGreaterThan(after.paletteRevision, before.paletteRevision)
        let screen = try await rows(of: terminal) { _ in true }
        XCTAssertTrue(shows(screen, "THEMED-42"), "re-theming keeps the screen (screen: \(screen))")

        // The shell is still live, and a program asking for the background
        // (OSC 11) is told the new theme's. The raw read takes whatever the
        // reply holds within a second, whichever terminator it uses.
        try await type("stty -echo -icanon min 0 time 10; printf '\\033]11;?\\033\\\\'; r=$(dd bs=128 count=1 2>/dev/null);"
                       + " stty sane; echo \"${r#*;}\" > \(live.outPath)", into: terminal)
        let reply = try await hostFile(live.outPath, containing: "rgb:")
        let hex = dracula.background.dropFirst()
        let expected = stride(from: 0, to: 6, by: 2).map { offset -> String in
            let pair = hex.dropFirst(offset).prefix(2).lowercased()
            return pair + pair
        }.joined(separator: "/")
        let shown = try await rows(of: terminal) { _ in true }
        XCTAssertTrue(reply.contains("rgb:\(expected)"), "OSC 11 reports \(expected): \(reply) (screen: \(shown))")
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// M1: switching machines keeps each one's place in its history.
    func testEachMachineKeepsItsHistoryPositionAcrossSwitches() async throws {
        let live = try await liveScene(), scene = live.scene
        let alpha = try shown(try await connect(scene, "alpha"))
        defer { alpha.removeFromSuperview() }
        try await printHistory(into: alpha, count: 60)
        // Output that arrives while alpha is hidden must not move its place.
        try await type("(sleep 3; echo HIDDEN-$((40+2))) &", into: alpha)
        alpha.scrollHistory(byRows: -1_000)
        _ = try await rows(of: alpha) { $0.contains("HIST-1") }
        let place = try XCTUnwrap(alpha.renderer.admittedFrame?.viewportFirst)

        let beta = try await shownAfterSelecting(scene, "beta", connected: try await connect(scene, "beta"))
        defer { beta.removeFromSuperview() }
        try await type("echo BETA-$((40+2))", into: beta)
        _ = try await rows(of: beta) { self.shows($0, "BETA-42") }
        try await Task.sleep(for: .seconds(3))
        // Hidden alpha receives no frames, so anything newer comes from the return.
        let shownBefore = try XCTUnwrap(alpha.renderer.admittedFrame?.revision)
        try await select(scene, "alpha")
        for _ in 0..<200 where (alpha.renderer.admittedFrame?.revision ?? 0) <= shownBefore {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertGreaterThan(try XCTUnwrap(alpha.renderer.admittedFrame?.revision), shownBefore,
                             "returning to alpha draws a new frame")
        let back = try await rows(of: alpha) { _ in true }
        XCTAssertTrue(back.contains("HIST-1") && !shows(back, "HIDDEN-42"), "alpha returns where it was (screen: \(back))")
        XCTAssertEqual(alpha.renderer.admittedFrame?.viewportFirst, place)

        let control = try await subview(UIButton.self, in: alpha) { $0.accessibilityLabel == "Return to live output" }
        control.sendActions(for: .touchUpInside)
        let bottom = try await rows(of: alpha) { self.shows($0, "HIDDEN-42") }
        XCTAssertTrue(shows(bottom, "HIDDEN-42"), "the control returns to live output, including what arrived while hidden (screen: \(bottom))")
        XCTAssertEqual(alpha.renderer.admittedFrame?.isLive, true)
        await scene.close()
        for session in scene.sessions.values { await assertReleased(session, connections: live.connections) }
    }

    /// A server that stops answering (its sshd frozen, as a hung host or a dead
    /// network looks to the phone) is reported lost within the liveness window,
    /// and an explicit new session works once it answers again. An idle server
    /// that answers probes is never reported lost.
    func testAnUnansweringServerIsReportedLostAndAnIdleOneIsNot() async throws {
        let liveness = try WorkspaceSSHLiveness(interval: .seconds(2), missedReplies: 2)
        let live = try await liveScene(liveness: liveness), scene = live.scene
        let session = try await connect(scene, "alpha")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        try await type("echo IDLE-$((40+2))", into: terminal)
        _ = try await rows(of: terminal) { self.shows($0, "IDLE-42") }
        try await Task.sleep(for: .seconds(9))
        XCTAssertEqual(session.phase, .live, "an idle server answers its probes and stays connected")

        // Freeze this shell's sshd; a background job resumes it after the test.
        try await type("(sleep 25; kill -CONT $PPID) & kill -STOP $PPID", into: terminal)
        let frozen = ContinuousClock.now
        for _ in 0..<300 where !session.settled { try await Task.sleep(for: .milliseconds(50)) }
        let detected = ContinuousClock.now - frozen
        XCTAssertEqual(session.phase, .ended(TerminalSessionController.describe(.disconnected)))
        XCTAssertTrue(session.settled)
        XCTAssertLessThan(detected, .seconds(10), "about interval x (missedReplies + 1) after the last reply")
        await assertReleased(session, connections: live.connections)

        let again = try await connect(scene, "alpha")
        let fresh = try shown(again)
        defer { fresh.removeFromSuperview() }
        try await type("echo REVIVED-$((40+2)) > \(live.outPath)", into: fresh)
        let output = try await hostFile(live.outPath, containing: "REVIVED-42")
        XCTAssertTrue(output.contains("REVIVED-42"), "a new session after the loss works (output: \(output))")
        await scene.close()
        await assertReleased(again, connections: live.connections)
    }

    /// A server that drops the connection (its sshd killed, as in a reboot) is
    /// reported as lost, not as something this device chose to close.
    func testADroppedConnectionIsReportedLost() async throws {
        let live = try await liveScene(), scene = live.scene
        let session = try await connect(scene, "alpha")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        try await type("kill -9 $PPID", into: terminal)
        for _ in 0..<200 where !session.settled { try await Task.sleep(for: .milliseconds(50)) }
        XCTAssertEqual(session.phase, .ended(TerminalSessionController.describe(.disconnected)))
        await assertReleased(session, connections: live.connections)
        await scene.close()
    }

    /// The network goes away after Enter, before the machine answered: the
    /// phone says the line left it and may or may not have arrived, sends
    /// nothing again, and the next session keeps saying so until dismissed.
    /// Input the machine answered is not reported. A relay to the loopback
    /// sshd stands in for the network.
    func testInputTheMachineNeverAnsweredIsReportedAfterADropAndNotSentAgain() async throws {
        guard let sshPort = ProcessInfo.processInfo.environment["FT_LIVE_SSH_PORT"].flatMap(UInt16.init) else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_SSH_PORT to run against a live sshd")
        }
        let relay = try LoopbackRelay(to: sshPort)
        try await relay.up()
        defer { relay.down() }
        let live = try await liveScene(machineIDs: ["alpha"], relayPort: relay.port)
        let scene = live.scene, run = UUID().uuidString.prefix(8)
        let first = try await connect(scene, "alpha")
        let terminal = try shown(first)
        defer { terminal.removeFromSuperview() }
        // The host reads the next line without echoing it, so nothing answers it.
        try await type("stty -echo; echo SILENT-$((40+2)); /bin/cat > /dev/null", into: terminal)
        _ = try await rows(of: terminal) { self.shows($0, "SILENT-42") }
        try await Task.sleep(for: .milliseconds(500))
        try await type("LOST-\(run)", into: terminal)
        try await Task.sleep(for: .seconds(1))
        XCTAssertTrue(first.inputDelivery.isEmpty, "a live connection reports nothing")
        relay.down()
        for _ in 0..<200 where !first.settled { try await Task.sleep(for: .milliseconds(50)) }
        XCTAssertTrue(first.dropped, "the connection was lost (\(first.phase))")
        XCTAssertTrue(first.inputDelivery.unanswered, "the line left the phone with no answer")
        XCTAssertFalse(first.inputDelivery.unsent)
        let message = try XCTUnwrap(first.inputDelivery.message(machine: first.target.machineName))
        XCTAssertTrue(message.contains("before Alpha answered it. It may or may not have arrived."), message)
        await assertReleased(first, connections: live.connections)
        // This launch told it; a later launch does not tell it again.
        XCTAssertFalse(TerminalUnansweredInput(workspace: first.target.workspaceID.uuidString, machine: "alpha",
                                               launch: "later").takeFromEarlierLaunch(), "told by this launch only")

        // The next session keeps the notice and types nothing again.
        try await relay.up()
        let again = try await connect(scene, "alpha")
        XCTAssertFalse(again === first)
        XCTAssertEqual(again.phase, .live)
        XCTAssertEqual(again.inputDelivery, first.inputDelivery, "a new live terminal does not erase it")
        let fresh = try shown(again)
        defer { fresh.removeFromSuperview() }
        try await type("echo SEEN-$((40+2))", into: fresh)
        let seen = try await rows(of: fresh) { self.shows($0, "SEEN-42") }
        XCTAssertFalse(shows(seen, "LOST-\(run)"), "the lost line was not typed again (screen: \(seen))")
        again.dismissInputDelivery()
        XCTAssertTrue(again.inputDelivery.isEmpty)

        // Input the machine answered is not reported when the connection drops.
        try await Task.sleep(for: .milliseconds(500))
        relay.down()
        for _ in 0..<200 where !again.settled { try await Task.sleep(for: .milliseconds(50)) }
        XCTAssertTrue(again.dropped, "the connection was lost (\(again.phase))")
        XCTAssertTrue(again.inputDelivery.isEmpty, "answered input is not reported (\(again.inputDelivery))")
        await assertReleased(again, connections: live.connections)
        await scene.close()
    }

    /// A dropped machine keeps retrying, and one the network stays away from
    /// longer than its retries last comes back when the network does, with no
    /// tap. A relay to the loopback sshd stands in for the network. A direct
    /// shell comes back signed in, waiting for New Session (a new shell is
    /// not the old one); through the disposable mux, when set, the machine
    /// reattaches to the pane it was showing, on a mux route and on a direct
    /// route whose default mux command reaches it (an sshd on
    /// TEST_RUNNER_FT_LIVE_SSH_DEFAULT_MUX_PORT that runs that command
    /// against the disposable mux).
    func testADroppedMachineComesBackWhenTheNetworkDoes() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard let sshPort = environment["FT_LIVE_SSH_PORT"].flatMap(UInt16.init) else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_SSH_PORT to run against a live sshd")
        }
        try await dropAndReturn(to: sshPort, muxCommand: nil)
        if let command = environment["FT_LIVE_MUX_COMMAND"] {
            try await dropAndReturn(to: sshPort, muxCommand: command)
        }
        if let port = environment["FT_LIVE_SSH_DEFAULT_MUX_PORT"].flatMap(UInt16.init) {
            try await dropAndReturn(to: port, muxCommand: nil, defaultMux: true)
        }
    }

    private func dropAndReturn(to sshPort: UInt16, muxCommand: String?, defaultMux: Bool = false) async throws {
        let attaches = muxCommand != nil || defaultMux
        let relay = try LoopbackRelay(to: sshPort)
        try await relay.up()
        defer { relay.down() }
        let live = try await liveScene(muxCommand: muxCommand, machineIDs: ["alpha"], autoConnect: true,
                                       relayPort: relay.port)
        let scene = live.scene, run = UUID().uuidString.prefix(8)
        let wifi = TerminalNetworkPath(usable: true, interfaces: ["en0"])
        scene.networkPathChanged(wifi)
        let first = try await connect(scene, "alpha")
        XCTAssertEqual(first.phase, .live)
        for _ in 0..<200 where attaches && first.muxState?.shown == nil {
            try await Task.sleep(for: .milliseconds(25))
        }
        let pane = first.muxState?.shown
        if defaultMux {
            XCTAssertFalse(first.route.mux, "a direct route")
            XCTAssertFalse(first.shellOnly, "attached to the default mux")
            XCTAssertNotNil(pane, "showing one of its panes")
        }

        // The network goes away: the connection drops and every retry is refused.
        relay.down()
        scene.networkPathChanged(.init(usable: false, interfaces: []))
        var retries = Set<UUID>(), retrying = false
        func noteRetry() { if let session = scene.sessions["alpha"], session !== first { retries.insert(session.id) } }
        for _ in 0..<600 where scene.status(of: "alpha") != .attention {
            noteRetry()
            if scene.status(of: "alpha") == .connecting { retrying = true }
            try await Task.sleep(for: .milliseconds(50))
        }
        noteRetry()
        XCTAssertTrue(retrying, "while retries remain it says it is connecting")
        XCTAssertEqual(retries.count, TerminalReconnectPolicy.maximumAttempts, "a retry that cannot connect is followed by the next")
        XCTAssertEqual(scene.status(of: "alpha"), .attention)

        // The network is back but nothing has said so: with its retries spent, it waits.
        try await relay.up()
        let spent = scene.sessions["alpha"]
        try await Task.sleep(for: .seconds(3))
        XCTAssertTrue(scene.sessions["alpha"] === spent)
        XCTAssertEqual(scene.status(of: "alpha"), .attention)

        // The device reports the network back, and the machine comes back.
        scene.networkPathChanged(wifi)
        let expected: TerminalSessionController.Phase = attaches ? .live : .ready
        for _ in 0..<600 where scene.sessions["alpha"] === spent || scene.sessions["alpha"]?.phase != expected {
            try await Task.sleep(for: .milliseconds(50))
        }
        let back = try XCTUnwrap(scene.sessions["alpha"])
        XCTAssertEqual(back.phase, expected, "the machine came back when the network did")
        if !attaches {
            XCTAssertTrue(scene.requiresNewSession("alpha"), "a new shell waits for New Session")
        } else {
            // Showing the pane rebuilds the terminal view.
            for _ in 0..<200 where back.muxState?.shown == nil || back.terminal == nil {
                try await Task.sleep(for: .milliseconds(25))
            }
            XCTAssertEqual(back.muxState?.shown, pane, "back to the pane it was showing")
            let view = try shown(back)
            try await type("echo BACK-\(run) > \(live.outPath).back", into: view)
            let output = try await hostFile(live.outPath + ".back", containing: "BACK-\(run)")
            XCTAssertTrue(output.contains("BACK-\(run)"), "typing reaches the pane (output: \(output))")
            view.removeFromSuperview()
        }
        await scene.close()
        for session in scene.sessions.values { await assertReleased(session, connections: live.connections) }
    }

    /// A new network route is only a hint. A connection that survived it
    /// answers the probe it prompts and stays; one stranded on the old route
    /// (a relay that stops forwarding, as a vanished Wi-Fi address looks to
    /// the phone) is reported lost one liveness interval after the change,
    /// not the usual three after its last byte. Direct, then through the
    /// disposable mux when set.
    func testANewRouteFindsAStrandedConnectionWithinOneInterval() async throws {
        try await strandOnNewRoute(muxCommand: nil)
        if let command = ProcessInfo.processInfo.environment["FT_LIVE_MUX_COMMAND"] {
            try await strandOnNewRoute(muxCommand: command)
        }
    }

    private func strandOnNewRoute(muxCommand: String?) async throws {
        guard let sshPort = ProcessInfo.processInfo.environment["FT_LIVE_SSH_PORT"].flatMap(UInt16.init) else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_SSH_PORT to run against a live sshd")
        }
        let relay = try LoopbackRelay(to: sshPort)
        try await relay.up()
        defer { relay.down() }
        let live = try await liveScene(muxCommand: muxCommand, machineIDs: ["alpha"], relayPort: relay.port)
        let scene = live.scene
        let wifi = TerminalNetworkPath(usable: true, interfaces: ["en0"])
        let cellular = TerminalNetworkPath(usable: true, interfaces: ["pdp_ip0"])
        scene.networkPathChanged(wifi)
        let session = try await connect(scene, "alpha")
        XCTAssertEqual(session.phase, .live)
        for _ in 0..<200 where muxCommand != nil && session.muxState?.shown == nil {
            try await Task.sleep(for: .milliseconds(25))
        }

        // The connection survives a burst of changes: each probe is answered
        // within its interval. After the shell's first prompt nothing else
        // would answer, and the second update comes while libssh2 still
        // refuses another keepalive.
        try await Task.sleep(for: .seconds(3))
        scene.networkPathChanged(cellular)
        try await Task.sleep(for: .seconds(1))
        scene.networkPathChanged(wifi)
        try await Task.sleep(for: .seconds(13))
        XCTAssertEqual(session.phase, .live, "a connection that survived new routes answers their probes and stays")

        // Stranded on the route the device left.
        relay.stall()
        scene.networkPathChanged(cellular)
        let changed = ContinuousClock.now
        for _ in 0..<400 where !session.settled { try await Task.sleep(for: .milliseconds(50)) }
        let detected = ContinuousClock.now - changed
        XCTAssertEqual(session.phase, .ended(TerminalSessionController.describe(.disconnected)))
        XCTAssertTrue(session.dropped)
        XCTAssertLessThan(detected, .seconds(14), "one 10 s interval after the change, not three after the last byte")
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// A retry that meets another host key (a server posing as the machine on
    /// the network the phone came back to) is not a drop: it is the last try,
    /// the machine needs attention, and a network change does not retry it.
    func testARetryThatMeetsAnotherHostKeyIsTheLast() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard let sshPort = environment["FT_LIVE_SSH_PORT"].flatMap(UInt16.init),
              let impostorPort = environment["FT_LIVE_SSH_IMPOSTOR_PORT"].flatMap(UInt16.init) else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_SSH_PORT and TEST_RUNNER_FT_LIVE_SSH_IMPOSTOR_PORT (an sshd with another host key)")
        }
        let relay = try LoopbackRelay(to: sshPort)
        try await relay.up()
        defer { relay.down() }
        let live = try await liveScene(machineIDs: ["alpha"], autoConnect: true, relayPort: relay.port)
        let scene = live.scene
        scene.networkPathChanged(.init(usable: true, interfaces: ["en0"]))
        let first = try await connect(scene, "alpha")
        XCTAssertEqual(first.phase, .live)

        // The connection drops, and a retry reaches a server with another host key.
        relay.down()
        try relay.retarget(to: impostorPort)
        try await relay.up()
        for _ in 0..<300 where scene.sessions["alpha"] === first || scene.status(of: "alpha") != .attention {
            try await Task.sleep(for: .milliseconds(50))
        }
        let last = try XCTUnwrap(scene.sessions["alpha"])
        XCTAssertFalse(last === first, "it retried")
        XCTAssertEqual(scene.status(of: "alpha"), .attention)
        XCTAssertFalse(last.dropped, "meeting another host key is not a drop")
        guard case .couldNotConnect = last.phase else { return XCTFail("expected Couldn't connect, not \(last.phase)") }

        // It is the last try: longer than any retry delay passes, and then
        // the network changes, with no other.
        try await Task.sleep(for: .seconds(5))
        XCTAssertTrue(scene.sessions["alpha"] === last, "no retry follows it")
        scene.networkPathChanged(.init(usable: true, interfaces: ["pdp_ip0"]))
        try await Task.sleep(for: .seconds(4))
        XCTAssertTrue(scene.sessions["alpha"] === last, "nor does a network change retry it")
        await scene.close()
        for session in scene.sessions.values { await assertReleased(session, connections: live.connections) }
    }

    /// A retry that reaches the machine but is reset before sign-in finishes
    /// (a flaky network, a server restarting) learned nothing about the
    /// machine: it is another drop. Every retry runs, and when they are
    /// spent, a network change brings the machine back.
    func testARetryResetBeforeSignInIsAnotherDrop() async throws {
        guard let sshPort = ProcessInfo.processInfo.environment["FT_LIVE_SSH_PORT"].flatMap(UInt16.init) else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_SSH_PORT to run against a live sshd")
        }
        let relay = try LoopbackRelay(to: sshPort)
        try await relay.up()
        defer { relay.down() }
        let live = try await liveScene(machineIDs: ["alpha"], autoConnect: true, relayPort: relay.port)
        let scene = live.scene
        scene.networkPathChanged(.init(usable: true, interfaces: ["en0"]))
        let first = try await connect(scene, "alpha")
        XCTAssertEqual(first.phase, .live)

        // The connection drops, and every retry is reset partway through its handshake.
        relay.cut()
        var retries = Set<UUID>()
        func noteRetry() { if let session = scene.sessions["alpha"], session !== first { retries.insert(session.id) } }
        for _ in 0..<400 where retries.isEmpty || scene.status(of: "alpha") != .attention {
            noteRetry()
            try await Task.sleep(for: .milliseconds(50))
        }
        noteRetry()
        XCTAssertEqual(retries.count, TerminalReconnectPolicy.maximumAttempts, "a reset retry is followed by the next")
        let last = try XCTUnwrap(scene.sessions["alpha"])
        XCTAssertTrue(last.dropped, "a reset before sign-in is a drop")
        guard case .couldNotConnect = last.phase else { return XCTFail("expected Couldn't connect, not \(last.phase)") }

        // The network is whole again, and the device says it changed.
        relay.down()
        try await relay.up()
        scene.networkPathChanged(.init(usable: true, interfaces: ["pdp_ip0"]))
        for _ in 0..<400 where scene.sessions["alpha"] === last || scene.sessions["alpha"]?.phase != .ready {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertEqual(scene.sessions["alpha"]?.phase, .ready, "back, signed in")
        XCTAssertTrue(scene.requiresNewSession("alpha"), "a new shell waits for New Session")
        await scene.close()
        for session in scene.sessions.values { await assertReleased(session, connections: live.connections) }
    }

    /// A machine whose mux attachment dropped reattaches by itself, but if its
    /// mux is gone by then, that exact existing-session intent fails without
    /// replacing it with a direct shell. Needs the disposable mux.
    func testAMachineWhoseMuxIsGoneAfterADropKeepsItsExistingSessionIntent() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard let sshPort = environment["FT_LIVE_SSH_PORT"].flatMap(UInt16.init),
              let command = environment["FT_LIVE_MUX_COMMAND"], let outPath = environment["FT_LIVE_SSH_OUT"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_SSH_PORT, TEST_RUNNER_FT_LIVE_MUX_COMMAND and TEST_RUNNER_FT_LIVE_SSH_OUT")
        }
        // The mux answers until its pane writes this file.
        let gone = "\(outPath).nomux-\(UUID().uuidString.prefix(8))"
        let relay = try LoopbackRelay(to: sshPort)
        try await relay.up()
        defer { relay.down() }
        let live = try await liveScene(muxCommand: "test ! -e \(gone) && \(command)", directAfterMux: true,
                                       machineIDs: ["alpha"], autoConnect: true, relayPort: relay.port)
        let scene = live.scene
        scene.networkPathChanged(.init(usable: true, interfaces: ["en0"]))
        let first = try await connect(scene, "alpha")
        XCTAssertEqual(first.phase, .live)
        XCTAssertFalse(first.shellOnly, "attached to the mux")
        for _ in 0..<200 where first.muxState?.shown == nil || first.terminal == nil {
            try await Task.sleep(for: .milliseconds(25))
        }
        let view = try shown(first)
        try await type("echo gone > \(gone)", into: view)
        let written = try await hostFile(gone, containing: "gone")
        XCTAssertTrue(written.contains("gone"), "the pane wrote the file (contents: \(written))")
        view.removeFromSuperview()

        // The connection drops, and a retry signs in, but the mux is gone.
        relay.down()
        for _ in 0..<200 where !first.settled { try await Task.sleep(for: .milliseconds(25)) }
        try await relay.up()
        for _ in 0..<400 {
            if let session = scene.sessions["alpha"], session !== first,
               session.phase == .failed(TerminalSessionController.describe(TerminalMuxError.unavailable)) { break }
            try await Task.sleep(for: .milliseconds(50))
        }
        // Nothing opens later either.
        try await Task.sleep(for: .seconds(2))
        let back = try XCTUnwrap(scene.sessions["alpha"])
        XCTAssertEqual(back.phase, .failed(TerminalSessionController.describe(TerminalMuxError.unavailable)))
        XCTAssertTrue(back.requiresExistingMux)
        XCTAssertFalse(back.shellOnly, "A dropped shared terminal keeps its existing mux route")
        XCTAssertTrue(back.route.mux)
        XCTAssertNil(back.terminal)
        await scene.close()
        for session in scene.sessions.values { await assertReleased(session, connections: live.connections) }
    }

    func testSharedMuxViewOnlyCanEnableInputWithoutTakingSharedSize() async throws {
        guard let command = ProcessInfo.processInfo.environment["FT_LIVE_MUX_COMMAND"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_MUX_COMMAND to reach a disposable mux")
        }
        let live = try await liveScene(muxCommand: command, machineIDs: ["alpha"], initialMuxAccessMode: .viewOnly)
        let scene = live.scene, observer = try await connect(live.scene, "alpha")
        XCTAssertEqual(observer.phase, .live)
        XCTAssertTrue(observer.attachedToMux); XCTAssertTrue(observer.requiresExistingMux)
        XCTAssertTrue(observer.isViewingOnly)
        let original = try shown(observer)
        XCTAssertTrue(original.input.isReadOnly)
        XCTAssertFalse(original.input.canBecomeFirstResponder)
        XCTAssertFalse(original.input.canReviewPaste)
        XCTAssertTrue(original.renderer.configuration.fitsRemoteGrid)
        for _ in 0..<200 where original.renderer.admittedFrame == nil {
            try await Task.sleep(for: .milliseconds(25))
        }
        let originalGrid = try XCTUnwrap(original.renderer.admittedFrame)
        await scene.activateKeyboard()
        original.requestKeyboard(true)
        original.input.insertText("must not type")
        XCTAssertFalse(original.input.isFirstResponder)
        let pending = await live.connections.coordinator.accounting()
        XCTAssertEqual(pending.retainedBytes, 0); XCTAssertEqual(pending.retainedRecords, 0)
        original.removeFromSuperview()

        scene.reconnectMux(.inputOnly, machineID: "alpha", controllerID: observer.id)
        for _ in 0..<400 where scene.sessions["alpha"] === observer || scene.sessions["alpha"]?.phase != .live {
            try await Task.sleep(for: .milliseconds(25))
        }
        let interactive = try XCTUnwrap(scene.sessions["alpha"])
        // A session goes live before the machine on screen gets its view.
        for _ in 0..<400 where interactive.phase == .live && interactive.terminal == nil
            && scene.shows(machineID: "alpha") {
            try await Task.sleep(for: .milliseconds(25))
        }
        XCTAssertTrue(observer.settled, "The old native owner settled before mode authority changed")
        XCTAssertTrue(observer.requiresExistingMux, "Stop preserves the successful existing-mux intent")
        XCTAssertNotEqual(interactive.id, observer.id)
        XCTAssertEqual(interactive.muxAccessMode, .inputOnly)
        XCTAssertTrue(interactive.requiresExistingMux); XCTAssertFalse(interactive.isViewingOnly)
        let active = try shown(interactive)
        XCTAssertFalse(active.input.isReadOnly)
        XCTAssertTrue(active.renderer.configuration.fitsRemoteGrid)
        let marker = "OBSERVER-CONTROL-\(UUID().uuidString.prefix(8))"
        try await type("stty size > \(live.outPath); trap 'echo WINCH >> \(live.outPath)' WINCH; echo \(marker) >> \(live.outPath)", into: active)
        let received = try await hostFile(live.outPath, containing: marker)
        let sharedSize = "\(originalGrid.rows) \(originalGrid.columns)"
        XCTAssertEqual(received.split(separator: "\n").first.map(String.init), sharedSize)
        for size in [CGSize(width: 390, height: 300), CGSize(width: 740, height: 360)] {
            active.frame = CGRect(origin: .zero, size: size)
            active.layoutIfNeeded()
            active.input.hideKeyboard()
            await scene.activateKeyboard()
            try await Task.sleep(for: .milliseconds(250))
        }
        for _ in 0..<200 where active.renderer.pointerProjection == nil {
            try await Task.sleep(for: .milliseconds(25))
        }
        let projection = try XCTUnwrap(active.renderer.pointerProjection)
        XCTAssertEqual(projection.frame.rows, originalGrid.rows)
        XCTAssertEqual(projection.frame.columns, originalGrid.columns)
        try await type("stty size >> \(live.outPath); trap - WINCH; echo DONE-\(marker) >> \(live.outPath)", into: active)
        let afterLayout = try await hostFile(live.outPath, containing: "DONE-\(marker)")
        XCTAssertFalse(afterLayout.contains("WINCH"), "keyboard and viewport changes must not resize the shared PTY")
        XCTAssertEqual(afterLayout.split(separator: "\n").filter { $0 == sharedSize }.count, 2)
        XCTAssertTrue(afterLayout.contains(marker), "Explicit shared-size input reaches the existing pane")
        active.removeFromSuperview()
        // A stale confirmation from the first observer cannot replace its new owner.
        scene.reconnectMux(.viewOnly, machineID: "alpha", controllerID: observer.id)
        XCTAssertTrue(scene.sessions["alpha"] === interactive)
        scene.reconnectMux(.viewOnly, machineID: "alpha", controllerID: interactive.id)
        for _ in 0..<400 where scene.sessions["alpha"] === interactive || scene.sessions["alpha"]?.phase != .live {
            try await Task.sleep(for: .milliseconds(25))
        }
        let returned = try XCTUnwrap(scene.sessions["alpha"])
        XCTAssertTrue(interactive.settled); XCTAssertTrue(returned.isViewingOnly)
        XCTAssertTrue(returned.requiresExistingMux)
        await scene.close()
        await assertReleased(observer, connections: live.connections)
        await assertReleased(interactive, connections: live.connections)
        await assertReleased(returned, connections: live.connections)
    }

    func testFormerMuxAttachmentNeverCreatesAShellWhenItsMuxIsMissingAfterStop() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let command = env["FT_LIVE_MUX_COMMAND"], let outPath = env["FT_LIVE_SSH_OUT"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_MUX_COMMAND and TEST_RUNNER_FT_LIVE_SSH_OUT")
        }
        for mode in [TerminalMuxAccessMode.viewOnly, .inputOnly, .interactive] {
            // This disposable relay command answers exactly its first attachment.
            // Neither terminal types a command to arrange the failure.
            let once = "\(outPath).mux-once-\(UUID().uuidString.prefix(8))"
            let live = try await liveScene(muxCommand: "test ! -e \(once) && { : > \(once); \(command); }",
                directAfterMux: true, machineIDs: ["alpha"], initialMuxAccessMode: mode)
            let scene = live.scene, first = try await connect(live.scene, "alpha")
            XCTAssertEqual(first.phase, .live); XCTAssertEqual(first.isViewingOnly, mode == .viewOnly)
            XCTAssertTrue(first.requiresExistingMux)
            scene.disconnect("alpha", expectedControllerID: first.id)
            await first.stop()
            XCTAssertTrue(first.settled); XCTAssertTrue(first.requiresExistingMux)
            let replacement = try await connect(scene, "alpha")
            for _ in 0..<200 where !replacement.settled { try await Task.sleep(for: .milliseconds(25)) }
            XCTAssertNotEqual(replacement.id, first.id)
            XCTAssertTrue(replacement.requiresExistingMux)
            XCTAssertEqual(replacement.muxAccessMode, mode)
            XCTAssertEqual(replacement.phase, .failed(TerminalSessionController.describe(TerminalMuxError.unavailable)))
            XCTAssertFalse(replacement.shellOnly, "An existing mux must not become a direct SSH shell")
            XCTAssertTrue(replacement.route.mux)
            XCTAssertNil(replacement.terminal)
            await scene.close()
            await assertReleased(first, connections: live.connections)
            await assertReleased(replacement, connections: live.connections)
        }
    }

    /// A network change while a machine is live gives its next drop a new
    /// retry budget, however briefly it was connected: retries spent before
    /// the change say nothing about the new network. Needs the disposable mux,
    /// whose attachment reconnects to live.
    func testANetworkChangeWhileLiveRenewsTheNextDropsRetries() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard let sshPort = environment["FT_LIVE_SSH_PORT"].flatMap(UInt16.init),
              let command = environment["FT_LIVE_MUX_COMMAND"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_SSH_PORT and TEST_RUNNER_FT_LIVE_MUX_COMMAND")
        }
        let relay = try LoopbackRelay(to: sshPort)
        try await relay.up()
        defer { relay.down() }
        let live = try await liveScene(muxCommand: command, machineIDs: ["alpha"], autoConnect: true,
                                       relayPort: relay.port)
        let scene = live.scene
        scene.networkPathChanged(.init(usable: true, interfaces: ["en0"]))
        let first = try await connect(scene, "alpha")
        XCTAssertEqual(first.phase, .live)

        // A drop whose first retry is refused and whose second reattaches:
        // two of its three retries are spent.
        relay.down()
        var seen: Set<UUID> = [first.id]
        for _ in 0..<200 {
            if let session = scene.sessions["alpha"] { seen.insert(session.id) }
            if seen.count == 2, scene.sessions["alpha"]?.settled == true { break }
            try await Task.sleep(for: .milliseconds(25))
        }
        try await relay.up()
        for _ in 0..<400 where scene.sessions["alpha"] === first || scene.sessions["alpha"]?.phase != .live {
            try await Task.sleep(for: .milliseconds(50))
        }
        let again = try XCTUnwrap(scene.sessions["alpha"])
        XCTAssertEqual(again.phase, .live)

        // The network changes while it is live, and then it drops again.
        scene.networkPathChanged(.init(usable: true, interfaces: ["pdp_ip0"]))
        try await Task.sleep(for: .seconds(1))
        relay.down()
        var retries = Set<UUID>()
        func noteRetry() { if let session = scene.sessions["alpha"], session !== again { retries.insert(session.id) } }
        for _ in 0..<300 where retries.isEmpty || scene.status(of: "alpha") != .attention {
            noteRetry()
            try await Task.sleep(for: .milliseconds(50))
        }
        noteRetry()
        XCTAssertEqual(retries.count, TerminalReconnectPolicy.maximumAttempts, "a new budget after the network changed")
        await scene.close()
        for session in scene.sessions.values { await assertReleased(session, connections: live.connections) }
    }

    /// The Mac's export lists a machine's mux route before its direct routes.
    /// When no mux answers there, the machine gets a shell on its direct route,
    /// as a direct route alone gives one; a machine with only a mux route
    /// keeps the mux's answer. Needs only the loopback sshd: the mux command
    /// fails at once.
    func testAMuxRouteWithNoMuxOpensAShellOnTheMachinesDirectRoute() async throws {
        let noMux = "nc -U /nonexistent/frankenterm-test.sock"
        let live = try await liveScene(muxCommand: noMux, directAfterMux: true), scene = live.scene
        let session = try await connect(scene, "alpha")
        XCTAssertEqual(session.phase, .live)
        XCTAssertTrue(session.shellOnly)
        XCTAssertEqual(session.route.id, "route-alpha", "the shell runs on the direct route")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        let run = UUID().uuidString.prefix(8)
        try await type("echo FALLBACK-\(run)-$((40+2)) > \(live.outPath)", into: terminal)
        let output = try await hostFile(live.outPath, containing: "FALLBACK-\(run)-42")
        XCTAssertTrue(output.contains("FALLBACK-\(run)-42"), "the shell runs commands (output: \(output))")
        await scene.close()
        await assertReleased(session, connections: live.connections)

        let only = try await liveScene(muxCommand: noMux)
        let refused = try await connect(only.scene, "alpha")
        XCTAssertEqual(refused.phase, .failed(TerminalSessionController.describe(TerminalMuxError.unavailable)))
        XCTAssertFalse(refused.shellOnly, "a machine with only a mux route never gets a new shell")
        await only.scene.close()
        await assertReleased(refused, connections: only.connections)
    }

    /// The direct route's own creation policy decides that fallback: selecting
    /// the machine tries its existing-only mux route, and when no mux answers
    /// a manual direct route signs in but opens a shell only when asked.
    func testAMuxRouteWithNoMuxKeepsAManualDirectRouteManual() async throws {
        let live = try await liveScene(creation: .manual, muxCommand: "nc -U /nonexistent/frankenterm-test.sock",
                                       directAfterMux: true), scene = live.scene
        scene.connect("alpha")
        let session = try XCTUnwrap(scene.sessions["alpha"])
        // Ready comes twice: signed in on the mux route, then on the direct one.
        for _ in 0..<400 where !(session.phase == .ready && session.shellOnly) && !session.settled {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertEqual(session.phase, .ready, "signed in on the manual route without opening a shell")
        XCTAssertTrue(session.shellOnly)
        XCTAssertEqual(session.route.id, "route-alpha")
        XCTAssertNil(session.terminal)

        await session.activateTerminal(explicit: true)
        for _ in 0..<400 where session.phase != .live && !session.settled {
            try await Task.sleep(for: .milliseconds(50))
        }
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        let run = UUID().uuidString.prefix(8)
        try await type("echo ASKED-\(run)-$((40+2)) > \(live.outPath)", into: terminal)
        let output = try await hostFile(live.outPath, containing: "ASKED-\(run)-42")
        XCTAssertTrue(output.contains("ASKED-\(run)-42"), "the shell asked for runs commands (output: \(output))")
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// M2 against a real FrankenTerm mux server. The phone attaches to the pane
    /// that is already running instead of starting a shell, typing reaches that
    /// pane's program, the pane takes the phone's size only while shown, and
    /// closing the phone gives the desktop its size back and leaves the program
    /// running for the next attach. TEST_RUNNER_FT_LIVE_MUX_COMMAND reaches a
    /// disposable mux on the loopback host (for example `nc -U <socket>`).
    /// Never point it at a mux someone uses: this test types into its first pane.
    func testAMuxRouteAttachesToTheRunningPaneAndLeavesItRunning() async throws {
        guard let command = ProcessInfo.processInfo.environment["FT_LIVE_MUX_COMMAND"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_MUX_COMMAND to reach a disposable mux on the loopback host")
        }
        let live = try await liveScene(muxCommand: command), out = live.outPath
        let session = try await connect(live.scene, "alpha")
        XCTAssertTrue(session.route.mux)
        XCTAssertFalse(session.shellOnly, "a mux route never falls back to a new shell")
        XCTAssertNotNil(session.muxControl)
        let terminal = try shown(session)
        // The pane outlives runs, and so does the file it writes: mark this run.
        let run = UUID().uuidString.prefix(8)
        try await type("trap 'stty size >> \(out)' WINCH; echo PID-$$ > \(out); stty size >> \(out); echo MUX-\(run)-$((6*7)) >> \(out)",
                       into: terminal)
        let typed = try await hostFile(out, containing: "MUX-\(run)-42")
        let lines = typed.split(separator: "\n").map(String.init)
        let pid = try XCTUnwrap(lines.first { $0.hasPrefix("PID-") }, typed)
        let isSize = { (line: String) in line.split(separator: " ").count == 2 && line.allSatisfy { $0.isNumber || $0 == " " } }
        let marker = try XCTUnwrap(lines.firstIndex(of: "MUX-\(run)-42"), typed)
        XCTAssertTrue(marker > 0 && isSize(lines[marker - 1]), typed)

        // Closing gives the desktop its size back; the program sees a resize.
        let before = (try? String(contentsOfFile: out, encoding: .utf8)) ?? ""
        terminal.removeFromSuperview()
        await live.scene.close()
        await assertReleased(session, connections: live.connections)
        var after: [String] = []
        for _ in 0..<200 where after.isEmpty {
            try await Task.sleep(for: .milliseconds(50))
            let text = (try? String(contentsOfFile: out, encoding: .utf8)) ?? ""
            after = text.dropFirst(before.count).split(separator: "\n").map(String.init).filter(isSize)
        }
        let phoneSize = before.split(separator: "\n").map(String.init).filter(isSize).last
        let desktopSize = try XCTUnwrap(after.last, "the pane got no resize after the phone closed (\(before))")
        XCTAssertNotEqual(desktopSize, phoneSize, "the desktop got its own size back, not the phone's")

        // The same program is still there for the next attach.
        let again = try await liveScene(muxCommand: command)
        let second = try await connect(again.scene, "alpha")
        XCTAssertFalse(second.shellOnly)
        let view = try shown(second)
        defer { view.removeFromSuperview() }
        // The shell expands its own $$; only the same process prints the same PID.
        try await type("trap - WINCH; echo AGAIN-\(run)-$$ >> \(out)", into: view)
        let reattached = try await hostFile(out, containing: "AGAIN-\(run)-\(pid.dropFirst(4))")
        XCTAssertTrue(reattached.contains("AGAIN-\(run)-\(pid.dropFirst(4))"),
                      "the next attach reached the same running program (\(reattached))")
        await again.scene.close()
        await assertReleased(second, connections: again.connections)
    }

    /// ⌘] and ⌘[ step through the shown machine's mux panes, wrapping at
    /// either end. A second press while a switch is still settling is
    /// ignored, not reported as a failed switch. Needs a disposable mux with
    /// two panes (TEST_RUNNER_FT_LIVE_MUX_COMMAND).
    func testAMuxHardwareKeyboardStepsThroughItsPanesAndWraps() async throws {
        guard let command = ProcessInfo.processInfo.environment["FT_LIVE_MUX_COMMAND"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_MUX_COMMAND to reach a disposable mux on the loopback host")
        }
        let live = try await liveScene(muxCommand: command), scene = live.scene
        let session = try await connect(scene, "alpha")
        var view = try shown(session)
        let state = try XCTUnwrap(session.muxState)
        let start = try XCTUnwrap(state.shown)
        guard state.panes.count > 1 else {
            view.removeFromSuperview(); await scene.close()
            throw XCTSkip("The mux needs a second pane")
        }
        /// Presses the shortcut's action `times` at once and waits for the switch.
        func step(forward: Bool, times: Int = 1) async throws -> Int? {
            let before = session.muxState?.shown
            for _ in 0..<times { scene.stepPane(forward: forward) }
            for _ in 0..<400 where session.muxState?.shown == before || session.paneNavigationBusy || session.terminal === view {
                try await Task.sleep(for: .milliseconds(25))
            }
            XCTAssertNil(session.interactionProblem, "a step is not reported as a failed switch")
            view.removeFromSuperview()
            view = try shown(session)
            return session.muxState?.shown
        }
        var visited = [start]
        for _ in 1..<state.panes.count {
            let next = try await step(forward: true)
            visited.append(try XCTUnwrap(next))
        }
        XCTAssertEqual(Set(visited), Set(state.panes.map(\.paneId)), "forward visits every pane once")
        let wrapped = try await step(forward: true)
        XCTAssertEqual(wrapped, start, "after the last pane comes the first")
        let back = try await step(forward: false)
        XCTAssertEqual(back, visited.last, "back from the first pane is the last")
        let once = try await step(forward: false, times: 2)
        XCTAssertEqual(once, visited[visited.count - 2], "two quick presses step once while the first settles")
        view.removeFromSuperview()
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// Unread marks: a mux pane that prints while another pane is on screen is
    /// marked until it is shown again. A pane that just left the screen redraws
    /// at the size it gets back, which is not news. Types into the disposable
    /// mux's panes.
    func testAMuxPaneThatPrintsWhileAnotherIsShownIsUnreadUntilShown() async throws {
        guard let command = ProcessInfo.processInfo.environment["FT_LIVE_MUX_COMMAND"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_MUX_COMMAND to reach a disposable mux on the loopback host")
        }
        let live = try await liveScene(muxCommand: command), scene = live.scene, out = live.outPath + ".unread"
        let run = UUID().uuidString.prefix(8)
        let session = try await connect(scene, "alpha")
        var view = try shown(session)
        let state = try XCTUnwrap(session.muxState)
        let first = try XCTUnwrap(state.shown)
        guard let other = state.panes.first(where: { $0.paneId != first })?.paneId else {
            view.removeFromSuperview(); await scene.close()
            throw XCTSkip("The mux needs a second pane")
        }
        func show(_ pane: Int) async throws {
            session.showPane(pane)
            for _ in 0..<400 where session.muxState?.shown != pane || session.paneNavigationBusy || session.terminal === view {
                try await Task.sleep(for: .milliseconds(25))
            }
            XCTAssertEqual(session.muxState?.shown, pane)
            view.removeFromSuperview()
            view = try shown(session)
        }
        // The first pane leaves the screen idle at its prompt and gets its own
        // size back; its shell redraws.
        try await show(other)
        try await Task.sleep(for: .milliseconds(2_500))
        XCTAssertEqual(session.muxState?.unread, [], "a redraw at the size a pane got back is not news")

        // The other pane starts a command that prints once it is off screen and
        // settled.
        try await type("echo STARTED-\(run) > \(out); sleep 8; echo LATER-\(run)", into: view)
        _ = try await hostFile(out, containing: "STARTED-\(run)")
        try await show(first)
        XCTAssertEqual(session.muxState?.unread, [], "nothing printed yet")
        for _ in 0..<600 where session.muxState?.unread.contains(other) != true {
            try await Task.sleep(for: .milliseconds(25))
        }
        XCTAssertEqual(session.muxState?.unread, [other], "the pane that printed off screen is unread; the one shown is not")
        for _ in 0..<200 where !session.hasUnseenOutput { try await Task.sleep(for: .milliseconds(25)) }
        XCTAssertTrue(session.hasUnseenOutput, "the machine's strip mark follows its unread panes")
        XCTAssertFalse(scene.hasUnseenOutput("alpha"), "the machine on screen shows its panes' marks, not its own")

        try await show(other)
        XCTAssertEqual(session.muxState?.unread, [], "showing the pane reads it")
        for _ in 0..<200 where session.hasUnseenOutput { try await Task.sleep(for: .milliseconds(25)) }
        XCTAssertFalse(session.hasUnseenOutput)

        // The pane prints again while the first is shown: Next New Output is
        // offered. Once the machine disconnects, its last pane list still
        // says unread, but there is nowhere left to go, and it is not offered.
        try await type("sleep 6; echo AGAIN-\(run)", into: view)
        try await show(first)
        for _ in 0..<600 where session.muxState?.unread.contains(other) != true {
            try await Task.sleep(for: .milliseconds(25))
        }
        XCTAssertEqual(session.muxState?.unread, [other])
        XCTAssertTrue(scene.hasUnseenOutputElsewhere, "Next New Output goes to the pane that printed")
        scene.disconnect("alpha")
        for _ in 0..<200 where !session.settled { try await Task.sleep(for: .milliseconds(25)) }
        XCTAssertEqual(session.muxState?.unread, [other], "the machine's last pane list")
        XCTAssertFalse(scene.hasUnseenOutputElsewhere, "a machine that is gone offers no Next New Output")
        view.removeFromSuperview()
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// Unread marks: a machine whose shell prints while another machine is on
    /// screen is marked until it is on screen again; the one on screen never is.
    func testAMachineWhoseShellPrintsWhileAwayIsMarkedUntilShownAgain() async throws {
        let live = try await liveScene(), scene = live.scene, out = live.outPath + ".unseen"
        let run = UUID().uuidString.prefix(8)
        let alphaSession = try await connect(scene, "alpha")
        let alpha = try shown(alphaSession)
        defer { alpha.removeFromSuperview() }
        let betaSession = try await connect(scene, "beta")
        let beta = try await shownAfterSelecting(scene, "beta", connected: betaSession)
        defer { beta.removeFromSuperview() }
        try await select(scene, "alpha")
        XCTAssertFalse(scene.hasUnseenOutput("alpha"))
        XCTAssertFalse(scene.hasUnseenOutput("beta"), "beta printed nothing since it was on screen")

        // SOON lands within the 3 s after leaving that are taken for the redraw
        // of a terminal resized as its keyboard went; AWAY lands after them.
        try await type("echo STARTED-\(run) > \(out); sleep 1; echo SOON-\(run) | tee -a \(out); sleep 5; echo AWAY-\(run)",
                       into: alpha)
        _ = try await hostFile(out, containing: "STARTED-\(run)")
        try await select(scene, "beta")
        XCTAssertFalse(scene.hasUnseenOutput("alpha"), "nothing printed yet")
        let soon = try await hostFile(out, containing: "SOON-\(run)")
        XCTAssertTrue(soon.contains("SOON-\(run)"))
        try await Task.sleep(for: .milliseconds(500))
        XCTAssertFalse(scene.hasUnseenOutput("alpha"), "output just after leaving, as a redraw is, is not news")
        for _ in 0..<400 where !scene.hasUnseenOutput("alpha") { try await Task.sleep(for: .milliseconds(25)) }
        XCTAssertTrue(scene.hasUnseenOutput("alpha"), "alpha printed while beta was on screen")
        XCTAssertFalse(scene.hasUnseenOutput("beta"), "the machine on screen is never marked")

        try await select(scene, "alpha")
        XCTAssertFalse(alphaSession.hasUnseenOutput, "being on screen reads it")
        XCTAssertFalse(scene.hasUnseenOutput("beta"))
        await scene.close()
        for session in scene.sessions.values { await assertReleased(session, connections: live.connections) }
    }

    /// Next New Output: when a mux pane prints on a machine off screen, one
    /// action shows that machine and then that pane, not the pane the machine
    /// last showed; the fleet overview's New output filter lists the same
    /// two. Both machines attach to the disposable mux; beta shows
    /// the pane that prints, so for beta it is not news. (Named to run before
    /// the test that ends a pane of the disposable mux.)
    func testAMuxPaneThatPrintsOnAMachineOffScreenIsWhereNextNewOutputGoes() async throws {
        guard let command = ProcessInfo.processInfo.environment["FT_LIVE_MUX_COMMAND"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_MUX_COMMAND to reach a disposable mux on the loopback host")
        }
        let live = try await liveScene(muxCommand: command), scene = live.scene, out = live.outPath + ".next"
        let run = UUID().uuidString.prefix(8)
        func show(_ pane: Int, _ session: TerminalSessionController, replacing view: NativeTerminalView) async throws
            -> NativeTerminalView {
            session.showPane(pane)
            for _ in 0..<400 where session.muxState?.shown != pane || session.paneNavigationBusy || session.terminal === view {
                try await Task.sleep(for: .milliseconds(25))
            }
            XCTAssertEqual(session.muxState?.shown, pane)
            view.removeFromSuperview()
            return try shown(session)
        }
        let alphaSession = try await connect(scene, "alpha")
        var alpha = try shown(alphaSession)
        let first = try XCTUnwrap(alphaSession.muxState?.shown)
        guard let other = alphaSession.muxState?.panes.first(where: { $0.paneId != first })?.paneId else {
            alpha.removeFromSuperview(); await scene.close()
            throw XCTSkip("The mux needs a second pane")
        }
        // Alpha has shown both panes and is back on its first when it leaves.
        alpha = try await show(other, alphaSession, replacing: alpha)
        alpha = try await show(first, alphaSession, replacing: alpha)
        alpha.removeFromSuperview()
        let betaSession = try await connect(scene, "beta")
        var beta = try await shownAfterSelecting(scene, "beta", connected: betaSession)
        if betaSession.muxState?.shown != other { beta = try await show(other, betaSession, replacing: beta) }
        // Past the settle for alpha's pane leaving the screen.
        try await Task.sleep(for: .milliseconds(3_500))

        try await type("echo NEXT-\(run) | tee \(out)", into: beta)
        _ = try await hostFile(out, containing: "NEXT-\(run)")
        let alphaControl = try XCTUnwrap(alphaSession.muxControl)
        for _ in 0..<600 where !alphaControl.current.unread.contains(other) {
            try await Task.sleep(for: .milliseconds(25))
        }
        XCTAssertTrue(alphaControl.current.unread.contains(other), "alpha, off screen, has the pane that printed unread")
        XCTAssertEqual(alphaControl.current.shown, first, "alpha still shows its first pane")
        for _ in 0..<200 where !scene.hasUnseenOutputElsewhere { try await Task.sleep(for: .milliseconds(25)) }
        XCTAssertTrue(scene.hasUnseenOutputElsewhere, "Next New Output is offered")
        // The fleet overview's New output filter lists alpha and that pane.
        scene.beginOverview()
        let visit = try XCTUnwrap(scene.overviewSessionID, "the fleet overview opens")
        let marked = try await scene.makeOverview().search(.init(filter: .newOutput)).rows
            .map { "\($0.machineID)/\($0.paneID.map(String.init) ?? "machine")" }
        XCTAssertTrue(marked.contains("alpha/machine"), "the overview marks alpha: \(marked)")
        XCTAssertTrue(marked.contains("alpha/\(other)"), "and its pane that printed: \(marked)")
        XCTAssertFalse(marked.contains("alpha/\(first)"), "not the pane alpha last showed: \(marked)")
        XCTAssertFalse(marked.contains("beta/machine") || marked.contains("beta/\(other)"),
                       "nor beta or its pane, on screen: \(marked)")
        scene.dismissOverview(); scene.finishOverviewDismissal(visit)
        for _ in 0..<200 where !scene.permitsTerminalCreation { try await Task.sleep(for: .milliseconds(25)) }
        XCTAssertTrue(scene.permitsTerminalCreation, "closing the overview hands navigation back")

        scene.showNextUnseenOutput()
        for _ in 0..<400 where scene.selectedMachineID != "alpha" || alphaSession.muxState?.shown != other
            || alphaSession.paneNavigationBusy {
            try await Task.sleep(for: .milliseconds(25))
        }
        XCTAssertEqual(scene.selectedMachineID, "alpha", "Next New Output shows the machine")
        XCTAssertEqual(alphaSession.muxState?.shown, other, "and then the pane that printed, not the one it last showed")
        XCTAssertFalse(alphaControl.current.unread.contains(other), "showing the pane reads it")
        XCTAssertNil(scene.problem)
        alpha = try shown(alphaSession)
        let text = try await screen(alpha, containing: "NEXT-\(run)")
        XCTAssertTrue(text.contains("NEXT-\(run)"), "the pane on screen is the one that printed")

        // With nothing waiting, it stays put.
        for _ in 0..<200 where scene.hasUnseenOutputElsewhere { try await Task.sleep(for: .milliseconds(25)) }
        XCTAssertFalse(scene.hasUnseenOutputElsewhere)
        scene.showNextUnseenOutput()
        try await Task.sleep(for: .milliseconds(500))
        XCTAssertEqual(scene.selectedMachineID, "alpha")
        XCTAssertEqual(alphaSession.muxState?.shown, other)
        alpha.removeFromSuperview(); beta.removeFromSuperview()
        await scene.close()
        for session in scene.sessions.values { await assertReleased(session, connections: live.connections) }
    }

    /// A noisy neighbor: while a machine off screen floods output, commands
    /// typed on the one on screen show their output promptly, the flood is
    /// read through to its end, and the app does not grow with what it reads.
    func testAMachineFloodingOffScreenLeavesTheOneOnScreenResponsive() async throws {
        let live = try await liveScene(), scene = live.scene, marks = live.outPath + ".flood"
        let run = UUID().uuidString.prefix(8)
        func halves() -> Int { ((try? String(contentsOfFile: marks, encoding: .utf8)) ?? "").split(separator: "\n").count }
        let alphaSession = try await connect(scene, "alpha")
        let alpha = try shown(alphaSession)
        defer { alpha.removeFromSuperview() }
        let betaSession = try await connect(scene, "beta")
        let beta = try await shownAfterSelecting(scene, "beta", connected: betaSession)
        defer { beta.removeFromSuperview() }
        // Two halves of 75 MB in 10 KB writes, as fast as the phone reads
        // them, and a mark after each. A phone that stopped reading would
        // hold the writer, and the marks would never come.
        try await type("perl -e '$l = (\"x\" x 199 . \"\\n\") x 50; for (1, 2) { print $l for 1 .. 7500; "
            + "open F, \">>\", $ARGV[0]; print F \"HALF\\n\"; close F }' \(marks)", into: beta)
        try await select(scene, "alpha")
        let start = footprint()
        var slowest = Duration.zero
        for ping in 100..<108 {
            let sent = ContinuousClock.now
            try await type("echo PING-\(run)-$((\(ping) + 0))", into: alpha)
            let text = try await screen(alpha, containing: "PING-\(run)-\(ping)")
            XCTAssertTrue(text.contains("PING-\(run)-\(ping)"), "ping \(ping) never showed")
            slowest = max(slowest, ContinuousClock.now - sent)
        }
        var echoes: [Duration] = []
        for _ in 0..<20 { echoes.append(try await echo(into: alpha)) }
        assertPromptEcho(echoes, "beside the flood")
        XCTAssertEqual(halves(), 0, "every ping and key ran while beta still flooded")
        XCTAssertLessThan(slowest, .seconds(1), "the machine on screen answered slowly beside the flood")

        // The first half grows the app once (buffers, allocator caches); the
        // second must not grow it again, as anything kept per byte read would.
        for _ in 0..<1200 where halves() < 1 { try await Task.sleep(for: .milliseconds(50)) }
        XCTAssertEqual(halves(), 1, "beta's first 75 MB were not read within a minute off screen")
        let half = footprint()
        for _ in 0..<1200 where halves() < 2 { try await Task.sleep(for: .milliseconds(50)) }
        XCTAssertEqual(halves(), 2, "beta's second 75 MB were not read within a minute off screen")
        let first = (Int64(half) - Int64(start)) >> 20, second = (Int64(footprint()) - Int64(half)) >> 20
        XCTAssertLessThan(first, 96, "the app grew \(first) MB reading beta's first 75 MB")
        XCTAssertLessThan(second, 16, "the app grew \(second) MB more reading the next 75 MB")
        await scene.close()
        for session in scene.sessions.values { await assertReleased(session, connections: live.connections) }
    }

    /// Plan 18: a group of twenty machines must not cost twenty full
    /// terminals. Each machine visited and left keeps its session and screen
    /// at a small cost: twelve here, each shown, used and left in turn.
    func testEachMachineVisitedAndLeftCostsLittleMemory() async throws {
        let ids = ["alpha"] + (1...11).map { String(format: "m%02d", $0) }
        let live = try await liveScene(machineIDs: ids), scene = live.scene
        var previous = try shown(try await connect(scene, "alpha"))
        try await type("echo READY-$((40+2))", into: previous)
        _ = try await screen(previous, containing: "READY-42")
        let first = footprint()
        for id in ids.dropFirst() {
            let view = try await shownAfterSelecting(scene, id, connected: try await connect(scene, id))
            previous.removeFromSuperview()
            try await type("echo READY-\(id)-$((40+2))", into: view)
            let text = try await screen(view, containing: "READY-\(id)-42")
            XCTAssertTrue(text.contains("READY-\(id)-42"), "\(id) never ran its command")
            previous = view
        }
        let each = ((Int64(footprint()) - Int64(first)) / Int64(ids.count - 1)) >> 10
        XCTAssertLessThan(each, 4096, "each machine visited and left kept \(each) KB")
        XCTAssertEqual(scene.sessions.values.filter { $0.phase == .live }.count, ids.count, "every machine stays connected")
        previous.removeFromSuperview()
        await scene.close()
        for session in scene.sessions.values { await assertReleased(session, connections: live.connections) }
    }

    /// A key typed at the shell's prompt shows on screen promptly.
    func testATypedKeyShowsOnScreenPromptly() async throws {
        let live = try await liveScene(), scene = live.scene
        let session = try await connect(scene, "alpha")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        try await type("echo READY-$((40+2))", into: terminal)
        _ = try await screen(terminal, containing: "READY-42")
        var echoes: [Duration] = []
        for _ in 0..<30 { echoes.append(try await echo(into: terminal)) }
        assertPromptEcho(echoes, "at an idle prompt")
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// Two fingers on the key row's arrows: each sends what its own finger
    /// did. One that repeated sends nothing more as it lifts, though another
    /// arrow was touched meanwhile; a quick touch sends one.
    func testTwoArrowsHeldAtOnceSendWhatEachFingerDid() async throws {
        let live = try await liveScene(), scene = live.scene, out = live.outPath + ".arrows"
        let session = try await connect(scene, "alpha")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        let row = terminal.input.inputAccessoryView
        let left = try XCTUnwrap(button("terminal.key.Left arrow", in: row))
        let right = try XCTUnwrap(button("terminal.key.Right arrow", in: row))
        let control = try XCTUnwrap(button("terminal.key.Control modifier", in: row))
        // Unbuffered, so each line lands as it is read; then how cat ended.
        // The login shell may alias cat, so its path is spelled out.
        try await type("echo started > \(out).started; /bin/cat -uv > \(out); echo \"cat $?\" >> \(out)", into: terminal)
        _ = try await hostFile(out + ".started", containing: "started")
        try await Task.sleep(for: .milliseconds(500))
        // One repeat soon after a touch, then none for the rest of the test.
        terminal.input.keyRepeat = (delay: 0.05, interval: 60)
        left.sendActions(for: .touchDown)
        try await Task.sleep(for: .milliseconds(500))
        right.sendActions(for: .touchDown)
        left.sendActions(for: .touchUpInside)
        right.sendActions(for: .touchUpInside)
        terminal.input.insertText("\n")
        control.sendActions(for: .touchUpInside)
        terminal.input.insertText("d")
        let text = try await hostFile(out, containing: "cat 0")
        let line = text.components(separatedBy: "\n").first ?? ""
        let keys = line.components(separatedBy: "^[").filter { !$0.isEmpty }
        XCTAssertEqual(keys.map(\.last), ["D", "C"], "the left arrow repeated once, the right one was touched "
            + "(got \(text); \(session.phase); screen: \(terminal.renderer.accessibilityValue ?? ""))")
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// Ctrl+C from the key row stops a flood on the machine on screen: the
    /// key is not held behind the output, and the next command runs.
    func testCtrlCStopsAFloodOnTheMachineOnScreen() async throws {
        let live = try await liveScene(), scene = live.scene
        let session = try await connect(scene, "alpha")
        let terminal = try shown(session)
        defer { terminal.removeFromSuperview() }
        try await type("perl -e '$l = (\"y\" x 199 . \"\\n\") x 50; $e = time + 60; print $l while time < $e'", into: terminal)
        _ = try await screen(terminal, containing: String(repeating: "y", count: 40))
        let control = try XCTUnwrap(button("terminal.key.Control modifier", in: terminal.input.inputAccessoryView))
        let sent = ContinuousClock.now
        control.sendActions(for: .touchUpInside)
        terminal.input.insertText("c")
        try await type("echo STOPPED-$((40 + 2))", into: terminal)
        let text = try await screen(terminal, containing: "STOPPED-42")
        XCTAssertTrue(text.contains("STOPPED-42"), "Ctrl+C did not stop the flood")
        XCTAssertLessThan(ContinuousClock.now - sent, .seconds(3), "the flood went on after Ctrl+C")
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// M2: a new connection (the phone is back after its connection ended)
    /// shows the pane the phone was showing, with the same program in it, not
    /// whichever pane the desktop has active. When that pane closed meanwhile
    /// the screen says so and shows the desktop's active pane. Needs a
    /// disposable mux with two panes (TEST_RUNNER_FT_LIVE_MUX_COMMAND): this
    /// test types into them and ends one.
    func testAMuxReconnectComesBackToTheShownPaneAndSaysWhenItClosed() async throws {
        guard let command = ProcessInfo.processInfo.environment["FT_LIVE_MUX_COMMAND"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_MUX_COMMAND to reach a disposable mux on the loopback host")
        }
        let live = try await liveScene(muxCommand: command), scene = live.scene, out = live.outPath
        let run = UUID().uuidString.prefix(8)
        let first = try await connect(scene, "alpha")
        let firstView = try shown(first)
        let state = try XCTUnwrap(first.muxState)
        guard let other = state.panes.first(where: { $0.paneId != state.shown }) else {
            firstView.removeFromSuperview(); await scene.close()
            throw XCTSkip("The mux needs a second pane")
        }
        first.showPane(other.paneId)
        for _ in 0..<400 where first.muxState?.shown != other.paneId || first.paneNavigationBusy || first.terminal === firstView {
            try await Task.sleep(for: .milliseconds(25))
        }
        XCTAssertEqual(first.muxState?.shown, other.paneId)
        firstView.removeFromSuperview()
        let view = try shown(first)
        try await type("echo PANE-\(run)-$$ > \(out)", into: view)
        let typed = try await hostFile(out, containing: "PANE-\(run)-")
        let pid = try XCTUnwrap(typed.split(separator: "\n").first { $0.hasPrefix("PANE-\(run)-") }, typed)
            .dropFirst("PANE-\(run)-".count)
        view.removeFromSuperview()
        await first.stop()
        await assertReleased(first, connections: live.connections)

        // Back after the connection ended: the same pane, the same program.
        let second = try await connect(scene, "alpha")
        for _ in 0..<200 where second.muxState?.shown == nil { try await Task.sleep(for: .milliseconds(25)) }
        XCTAssertEqual(second.muxState?.shown, other.paneId, "the phone came back to the pane it was showing")
        XCTAssertNil(second.interactionProblem)
        let again = try shown(second)
        try await type("echo AGAIN-\(run)-$$ >> \(out); echo BYE-\(run) >> \(out); sleep 3; exit", into: again)
        let text = try await hostFile(out, containing: "BYE-\(run)")
        XCTAssertTrue(text.contains("AGAIN-\(run)-\(pid)"), "the same program answered (\(text))")
        again.removeFromSuperview()
        await second.stop()
        await assertReleased(second, connections: live.connections)

        // That pane ended while the phone was away.
        try await Task.sleep(for: .seconds(4))
        let third = try await connect(scene, "alpha")
        for _ in 0..<200 where third.muxState?.shown == nil || third.interactionProblem == nil {
            try await Task.sleep(for: .milliseconds(25))
        }
        let now = try XCTUnwrap(third.muxState)
        XCTAssertFalse(now.panes.contains { $0.paneId == other.paneId }, "the pane it was showing closed")
        XCTAssertNotNil(now.shown)
        XCTAssertNotEqual(now.shown, other.paneId)
        XCTAssertEqual(third.interactionProblem,
                       "The pane you were viewing closed while you were away. This is the desktop's active pane.")
        await scene.close()
        await assertReleased(third, connections: live.connections)
    }

    /// Plan 2.2, "lose Wi-Fi after pressing Enter", against a real mux. The
    /// pane's program freezes its mux server, so what is typed next goes
    /// unanswered, and then the phone's relay dies. The ended session says at
    /// once that its last input may or may not have arrived. The next
    /// connection comes back to the same pane and program and says it again
    /// even after the first notice is gone (as after a relaunch), from the
    /// pane's own record; one after an ordinary close says nothing. Needs a disposable
    /// mux (TEST_RUNNER_FT_LIVE_MUX_COMMAND): this test stops and resumes its
    /// server and kills the relay processes running that command.
    func testAMuxConnectionLostBeforeInputWasAnsweredSaysSoOnReturn() async throws {
        guard let command = ProcessInfo.processInfo.environment["FT_LIVE_MUX_COMMAND"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_MUX_COMMAND to reach a disposable mux on the loopback host")
        }
        let live = try await liveScene(muxCommand: command), scene = live.scene, out = live.outPath
        let run = UUID().uuidString.prefix(8)
        let first = try await connect(scene, "alpha")
        let view = try shown(first)
        let pane = try XCTUnwrap(first.muxState?.shown)
        try await type("echo PID-\(run)-$$ > \(out)", into: view)
        let typed = try await hostFile(out, containing: "PID-\(run)-")
        let pid = try XCTUnwrap(typed.split(separator: "\n").first { $0.hasPrefix("PID-\(run)-") }, typed)
            .dropFirst("PID-\(run)-".count)
        // The pane's shell is the server's child: it stops the server, and a
        // background job kills the relay and resumes the server after.
        try await type("(sleep 2; pkill -f '\(command)'; sleep 2; kill -CONT $PPID) & kill -STOP $PPID", into: view)
        try await Task.sleep(for: .milliseconds(500))
        try await type("echo LATE-\(run) >> \(out)", into: view)
        for _ in 0..<300 where !first.settled { try await Task.sleep(for: .milliseconds(50)) }
        XCTAssertEqual(first.phase, .ended(TerminalSessionController.describe(.disconnected)))
        XCTAssertTrue(first.dropped)
        let unconfirmed = "before Alpha answered it. It may or may not have arrived."
        XCTAssertTrue(first.inputDelivery.unanswered, "the server had not confirmed it (\(first.inputDelivery))")
        XCTAssertTrue(first.inputDelivery.message(machine: "Alpha")?.contains(unconfirmed) == true)
        view.removeFromSuperview()
        await assertReleased(first, connections: live.connections)
        first.dismissInputDelivery()
        try await Task.sleep(for: .seconds(3))

        let second = try await connect(scene, "alpha")
        for _ in 0..<200 where second.muxState?.shown == nil { try await Task.sleep(for: .milliseconds(25)) }
        XCTAssertEqual(second.muxState?.shown, pane, "the phone came back to the same pane")
        XCTAssertTrue(second.inputDelivery.unanswered, "the pane's record says it again (\(second.inputDelivery))")
        XCTAssertNil(second.interactionProblem, "one notice, not two")
        let again = try shown(second)
        try await type("echo AGAIN-\(run)-$$ >> \(out)", into: again)
        let text = try await hostFile(out, containing: "AGAIN-\(run)-")
        XCTAssertTrue(text.contains("AGAIN-\(run)-\(pid)"), "the same program answered (\(text))")
        again.removeFromSuperview()
        second.dismissInputDelivery()
        await second.stop()
        await assertReleased(second, connections: live.connections)

        // Closed on purpose after its input was answered: nothing to tell.
        let third = try await connect(scene, "alpha")
        for _ in 0..<200 where third.muxState?.shown == nil { try await Task.sleep(for: .milliseconds(25)) }
        XCTAssertEqual(third.muxState?.shown, pane)
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertNil(third.interactionProblem)
        XCTAssertTrue(third.inputDelivery.isEmpty, "\(third.inputDelivery)")
        await scene.close()
        await assertReleased(third, connections: live.connections)
    }

    /// Plan 2.2, "return after a remote restart", against a real mux. Its
    /// server restarts while the phone is away and builds its startup layout
    /// again, so the pane the phone was showing has a successor under the same
    /// pane, tab and window numbers. The phone knows the new run by the
    /// server's incarnation: it says the server restarted and shows the
    /// desktop's active pane, instead of taking the successor for the pane it
    /// left. Needs a disposable mux (TEST_RUNNER_FT_LIVE_MUX_COMMAND) and a
    /// script that restarts it from one of its panes
    /// (TEST_RUNNER_FT_LIVE_MUX_RESTART; arguments: the server's pid, a file,
    /// and a marker it writes there as STOPPING-<marker>, then
    /// RESTARTED-<marker> once the new server listens). It ends the server twice.
    func testAMuxServerThatRestartedWhileAwayIsNotTakenForTheOneThePhoneLeft() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard let command = environment["FT_LIVE_MUX_COMMAND"], let restart = environment["FT_LIVE_MUX_RESTART"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_MUX_COMMAND and TEST_RUNNER_FT_LIVE_MUX_RESTART to restart a disposable mux")
        }
        let live = try await liveScene(muxCommand: command), scene = live.scene, out = live.outPath
        let run = UUID().uuidString.prefix(8)
        let restarted = "FrankenTerm restarted on this machine while you were away, so the pane you were viewing "
            + "may be gone. This is the desktop's active pane."
        // Ends the session, then the server restarts (the pane's shell is the
        // server's child; the script outlives it).
        func restartServer(from session: TerminalSessionController, _ marker: String) async throws {
            let view = try shown(session)
            try await type("nohup /bin/zsh -f \(restart) $PPID \(out) \(marker) </dev/null >/dev/null 2>&1 &!", into: view)
            let started = try await hostFile(out, containing: "STOPPING-\(marker)")
            XCTAssertTrue(started.contains("STOPPING-\(marker)"), "the restart began (\(started))")
            view.removeFromSuperview()
            await session.stop()
            await assertReleased(session, connections: live.connections)
            var text = ""
            for _ in 0..<3 where !text.contains("RESTARTED-\(marker)") {
                text = try await hostFile(out, containing: "RESTARTED-\(marker)")
            }
            XCTAssertTrue(text.contains("RESTARTED-\(marker)"), "the server restarted (\(text))")
        }
        func shownPane(_ session: TerminalSessionController) async throws -> TerminalMuxControl.State {
            for _ in 0..<200 where session.muxState?.shown == nil { try await Task.sleep(for: .milliseconds(25)) }
            return try XCTUnwrap(session.muxState)
        }

        // A fresh server and layout, whatever earlier tests left.
        try await restartServer(from: try await connect(scene, "alpha"), "A-\(run)")

        // Back on a new run: told so, on the desktop's active pane.
        let second = try await connect(scene, "alpha")
        var state = try await shownPane(second)
        XCTAssertEqual(second.interactionProblem, restarted)
        XCTAssertEqual(state.shown, state.panes.first(where: \.isActive)?.paneId)
        // Leave it on the pane the desktop is not on, and restart again into
        // the same layout.
        let other = try XCTUnwrap(state.panes.first { !$0.isActive }, "the startup layout has two panes")
        second.showPane(other.paneId)
        for _ in 0..<400 where second.muxState?.shown != other.paneId || second.paneNavigationBusy {
            try await Task.sleep(for: .milliseconds(25))
        }
        XCTAssertEqual(second.muxState?.shown, other.paneId)
        try await restartServer(from: second, "B-\(run)")

        let third = try await connect(scene, "alpha")
        state = try await shownPane(third)
        XCTAssertTrue(state.panes.contains {
            $0.paneId == other.paneId && $0.tabId == other.tabId && $0.windowId == other.windowId
        }, "the new run repeats the numbers of the pane the phone left (\(state.panes))")
        XCTAssertNotEqual(state.shown, other.paneId, "the successor is not taken for the pane the phone left")
        XCTAssertEqual(state.shown, state.panes.first(where: \.isActive)?.paneId)
        XCTAssertEqual(third.interactionProblem, restarted)
        // The pane on screen is the new run's, and its shell answers.
        let view = try shown(third)
        try await type("echo NEW-\(run)-$((40+2)) >> \(out)", into: view)
        let text = try await hostFile(out, containing: "NEW-\(run)-42")
        XCTAssertTrue(text.contains("NEW-\(run)-42"), text)
        view.removeFromSuperview()
        await scene.close()
        await assertReleased(third, connections: live.connections)
    }

    /// The server pushes changes only for panes a connection has shown, so a
    /// pane the phone has not shown would keep the title it had at attach (an
    /// agent's status, say) unless the phone asks for the pane list again.
    /// Needs a disposable mux with two panes (TEST_RUNNER_FT_LIVE_MUX_COMMAND):
    /// the shown pane's shell retitles its sibling through the sibling's tty.
    func testAMuxPaneNotShownKeepsItsTitleCurrent() async throws {
        guard let command = ProcessInfo.processInfo.environment["FT_LIVE_MUX_COMMAND"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_MUX_COMMAND to reach a disposable mux on the loopback host")
        }
        let live = try await liveScene(muxCommand: command), scene = live.scene
        let run = UUID().uuidString.prefix(8)
        let session = try await connect(scene, "alpha")
        let view = try shown(session)
        let state = try XCTUnwrap(session.muxState)
        guard let other = state.panes.first(where: { $0.paneId != state.shown }) else {
            view.removeFromSuperview(); await scene.close()
            throw XCTSkip("The mux needs a second pane")
        }
        func title() -> String? { session.muxState?.panes.first { $0.paneId == other.paneId }?.title }
        XCTAssertNotEqual(title(), "TITLE-\(run)")
        // The server's other children are the other panes' shells.
        try await type("for p in $(pgrep -P $PPID); do t=$(ps -o tty= -p $p | tr -d ' '); [ /dev/$t = $(tty) ] || printf '\\033]2;TITLE-\(run)\\007' > /dev/$t; done",
                       into: view)
        for _ in 0..<240 where title() != "TITLE-\(run)" { try await Task.sleep(for: .milliseconds(50)) }
        XCTAssertEqual(title(), "TITLE-\(run)", "the pane strip shows the unshown pane's new title")
        view.removeFromSuperview()
        await scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// A 13-inch iPad in landscape at the smallest text size: the pane takes
    /// the whole grid and the phone draws it. The phone used to narrow its grid
    /// to keep 256 history rows beside both screens, using under half the width.
    func testAMuxPaneShownOnALargeScreenAtASmallSizeIsDrawnAtThatSize() async throws {
        guard let command = ProcessInfo.processInfo.environment["FT_LIVE_MUX_COMMAND"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_MUX_COMMAND to reach a disposable mux on the loopback host")
        }
        let live = try await liveScene(muxCommand: command), out = live.outPath
        let session = try await connect(live.scene, "alpha")
        let view = try shown(session)
        let frame = try await enlarge(view, to: CGSize(width: 1376, height: 1032), beyond: 250)
        XCTAssertGreaterThan((frame.rows * 2 + 256) * frame.columns, 65536, "a grid the old budget refused")
        let run = UUID().uuidString.prefix(8)
        try await type("stty size > \(out); echo PANE-\(run)-$((6*7)) | tee -a \(out)", into: view)
        let output = try await hostFile(out, containing: "PANE-\(run)-42")
        XCTAssertEqual(output.split(separator: "\n").first.map(String.init), "\(frame.rows) \(frame.columns)",
                       "the pane has the grid the phone draws (output: \(output))")
        let drawn = try await screen(view, containing: "PANE-\(run)-42")
        XCTAssertTrue(drawn.contains("PANE-\(run)-42"), "the phone draws the pane's output (screen: \(drawn))")
        view.removeFromSuperview()
        await live.scene.close()
        await assertReleased(session, connections: live.connections)
    }

    /// While the phone shows a pane, a larger size someone else gives it (the
    /// desktop; here a second device on a large screen) is asked back to the
    /// phone's size once 256 history rows would no longer fit beside it.
    func testAMuxPaneSomeoneElseMadeLargerIsAskedBackToThePhonesSize() async throws {
        guard let command = ProcessInfo.processInfo.environment["FT_LIVE_MUX_COMMAND"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_MUX_COMMAND to reach a disposable mux on the loopback host")
        }
        let phone = try await liveScene(muxCommand: command), out = phone.outPath
        let session = try await connect(phone.scene, "alpha")
        let view = try shown(session)
        let run = UUID().uuidString.prefix(8)
        try await type("trap 'stty size >> \(out)' WINCH; echo READY-\(run)-$((6*7)) > \(out)", into: view)
        _ = try await hostFile(out, containing: "READY-\(run)-42")
        let small = try XCTUnwrap(view.renderer.admittedFrame)

        let other = try await liveScene(muxCommand: command)
        let tablet = try await connect(other.scene, "alpha")
        let large = try shown(tablet)
        large.frame = CGRect(x: 0, y: 0, width: 1376, height: 1032)
        large.setFontPointSize(NativeTerminalView.fontSizes.lowerBound)
        func sizes() -> [[Int]] {
            ((try? String(contentsOfFile: out, encoding: .utf8)) ?? "").split(separator: "\n")
                .map { $0.split(separator: " ").compactMap { Int($0) } }.filter { $0.count == 2 }
        }
        let phoneSize = [small.rows, small.columns]
        func askedBack() -> Bool { sizes().contains { $0[1] > 250 } && sizes().last == phoneSize }
        // The shell reads the size when its WINCH trap runs, which can be after
        // the phone already asked the pane back. A wider layout tries again;
        // the second device sends it only if its first request settled.
        for width in [1376.0, 1440, 1504] where !askedBack() {
            if width > 1376 { large.frame = CGRect(x: 0, y: 0, width: width, height: 1032) }
            for _ in 0..<200 where !askedBack() { try await Task.sleep(for: .milliseconds(50)) }
        }
        XCTAssertTrue(sizes().contains { $0[1] > 250 }, "the second device gave the pane its size (\(sizes()))")
        XCTAssertEqual(sizes().last, phoneSize, "the phone asked its pane back (\(sizes()))")

        // The scene that appeared last has the input.
        try await type("trap - WINCH", into: large)
        large.removeFromSuperview(); view.removeFromSuperview()
        await other.scene.close()
        await assertReleased(tablet, connections: other.connections)
        await phone.scene.close()
        await assertReleased(session, connections: phone.connections)
    }

    /// A desktop pane larger than View Only can draw (the mux reached by
    /// TEST_RUNNER_FT_LIVE_LARGE_MUX_COMMAND has one 300 x 120 pane) waits for
    /// explicit permission before changing shared geometry. Leaving that
    /// connection restores the desktop size without silently taking it again.
    func testAPaneTooLargeToViewWaitsForExplicitPhoneSizePermission() async throws {
        guard let command = ProcessInfo.processInfo.environment["FT_LIVE_LARGE_MUX_COMMAND"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_LIVE_LARGE_MUX_COMMAND to reach a disposable mux whose pane is too large to view")
        }
        let live = try await liveScene(muxCommand: command, machineIDs: ["alpha"], initialMuxAccessMode: .viewOnly)
        let scene = live.scene, sizesPath = live.outPath + "-large", run = UUID().uuidString.prefix(8)
        func replacement(of old: TerminalSessionController) async throws -> TerminalSessionController {
            for _ in 0..<400 where scene.sessions["alpha"] === old || scene.sessions["alpha"]?.phase != .live
                || scene.sessions["alpha"]?.terminal == nil {
                try await Task.sleep(for: .milliseconds(25))
            }
            return try XCTUnwrap(scene.sessions["alpha"])
        }
        let observer = try await connect(scene, "alpha")
        for _ in 0..<400 where !observer.settled { try await Task.sleep(for: .milliseconds(25)) }
        XCTAssertTrue(observer.paneTooLargeToView); XCTAssertTrue(observer.settled)
        XCTAssertEqual(observer.muxAccessMode, .viewOnly)
        XCTAssertNil(observer.terminal)
        try await Task.sleep(for: .seconds(1))
        XCTAssertIdentical(scene.sessions["alpha"], observer, "an oversized grid cannot grant resize authority")
        XCTAssertEqual(scene.status(of: "alpha"), .attention)
        XCTAssertNil(scene.changingMuxMode("alpha"))
        scene.takePaneAtPhoneSize("alpha", from: observer)
        let fitted = try await replacement(of: observer)
        XCTAssertNotIdentical(fitted, observer)
        XCTAssertTrue(observer.paneTooLargeToView); XCTAssertTrue(observer.settled)
        XCTAssertFalse(observer.dropped, "a pane too large to view is not a drop to retry")
        XCTAssertEqual(fitted.muxAccessMode, .interactive); XCTAssertTrue(fitted.requiresExistingMux)
        XCTAssertNil(scene.changingMuxMode("alpha"))
        let view = try shown(fitted)
        // The pane may still be moving to the view's grid when the shell
        // reads its size; read it again until the two agree.
        var size = "", grid = ""
        for attempt in 0..<5 where size.isEmpty || size != grid {
            if attempt > 0 { try await Task.sleep(for: .milliseconds(200)) }
            try await type("stty size > \(sizesPath); echo FIT-\(run)-\(attempt)-$((6*7)) >> \(sizesPath)", into: view)
            let text = try await hostFile(sizesPath, containing: "FIT-\(run)-\(attempt)-42")
            size = text.split(separator: "\n").first.map(String.init) ?? ""
            grid = view.renderer.admittedFrame.map { "\($0.rows) \($0.columns)" } ?? ""
        }
        XCTAssertEqual(size, grid, "the pane has the grid the phone draws")

        try await type("trap 'stty size >> \(sizesPath)' WINCH; echo TRAP-\(run)-$((6*7)) >> \(sizesPath)", into: view)
        _ = try await hostFile(sizesPath, containing: "TRAP-\(run)-42")
        view.removeFromSuperview()
        scene.reconnectMux(.viewOnly, machineID: "alpha", controllerID: fitted.id)
        XCTAssertEqual(scene.changingMuxMode("alpha"), .viewOnly)
        XCTAssertEqual(scene.status(of: "alpha"), .connecting)
        for _ in 0..<400 where scene.sessions["alpha"] === fitted || scene.sessions["alpha"]?.settled != true {
            try await Task.sleep(for: .milliseconds(25))
        }
        let viewer = try XCTUnwrap(scene.sessions["alpha"])
        XCTAssertEqual(viewer.muxAccessMode, .viewOnly)
        XCTAssertTrue(viewer.paneTooLargeToView); XCTAssertFalse(viewer.dropped)
        let restored = try await hostFile(sizesPath, containing: "120 300")
        XCTAssertTrue(restored.contains("120 300"), "leaving gave the desktop its size back (\(restored))")
        try await Task.sleep(for: .seconds(2))
        XCTAssertIdentical(scene.sessions["alpha"], viewer, "the chosen View Only is neither retaken nor retried")
        XCTAssertEqual(scene.status(of: "alpha"), .attention)

        // Use at Phone Size, on the ended card.
        scene.takePaneAtPhoneSize("alpha", from: viewer)
        XCTAssertEqual(scene.changingMuxMode("alpha"), .interactive)
        let taken = try await replacement(of: viewer)
        XCTAssertEqual(taken.muxAccessMode, .interactive)
        let again = try shown(taken)
        try await type("trap - WINCH; echo UNTRAP-\(run)-$((6*7)) >> \(sizesPath)", into: again)
        _ = try await hostFile(sizesPath, containing: "UNTRAP-\(run)-42")
        again.removeFromSuperview()
        await scene.close()
        for session in [observer, fitted, viewer, taken] { await assertReleased(session, connections: live.connections) }
    }

    /// A direct route first tries the machine's mux. The loopback host has none,
    /// so that attempt leaves an acknowledged record with no input before the
    /// shell's own record. These are the records of terminals that ran.
    private func terminalRecords(_ records: [TerminalSessionRecord]) -> [TerminalSessionRecord] {
        records.filter { $0.state != .acknowledged || !$0.inputEvidence.isEmpty }
    }

    private func describe(_ records: [TerminalSessionRecord]) -> String {
        "\(records.map { "\($0.state) \($0.inputEvidence)" })"
    }

    private func assertReleased(_ controller: TerminalSessionController, connections: MobileConnections,
                                file: StaticString = #filePath, line: UInt = #line) async {
        XCTAssertTrue(controller.settled, file: file, line: line)
        XCTAssertNil(controller.cleanupFailure, file: file, line: line)
        let accounting = await connections.coordinator.accounting()
        XCTAssertEqual(accounting.connections, 0, "source ownership leaked", file: file, line: line)
        XCTAssertEqual(accounting.streams, 0, "responder focus leaked", file: file, line: line)
        XCTAssertEqual(accounting.retainedRecords, 0, "final outcomes were not reconciled", file: file, line: line)
        XCTAssertEqual(accounting.cleanupRecords, 0, file: file, line: line)
        XCTAssertEqual(accounting.retainedBytes, 0, file: file, line: line)
        XCTAssertEqual(accounting.inFlightBytes, 0, file: file, line: line)
    }

    /// No network, Keychain prompt, or live-server environment is needed for
    /// these pre-start lifetime tests. The real manager remains disabled.
    private func dormantScene() throws -> (MobileConnections, WorkspaceTerminalScene, TerminalSessionTarget) {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("scene-stop-" + UUID().uuidString)
        let vault = SceneMemoryVault()
        let store = WorkspaceStore(persistence: try FileWorkspacePersistence(directory: root.appendingPathComponent("store")),
                                   vault: vault)
        let connections = try MobileConnections(store: store, journalDirectory: root.appendingPathComponent("journal"),
                                                journalIdentifier: "scene-stop-test-" + UUID().uuidString,
                                                signer: SceneMemorySigner(vault: vault))
        let target = TerminalSessionTarget(workspaceID: UUID(), revision: UUID(), machineID: "machine",
            machineName: "Never connected", routes: [.init(id: "route", credentialIDs: ["key"], host: "127.0.0.1", port: 22)])
        let machine = SceneMachine(id: "machine", name: "Never connected", target: target, unavailable: "", theme: nil)
        let scene = try WorkspaceTerminalScene(workspaceID: target.workspaceID, revision: target.revision, workspaceName: "Dormant",
            machines: [machine], initialMachineID: "machine", connections: connections, autoConnect: false)
        return (connections, scene, target)
    }

    func testStopBeforeStartPermanentlyRetiresThisController() async throws {
        let (connections, scene, target) = try dormantScene()
        let controller = TerminalSessionController(target: target, connections: connections, focus: scene)
        controller.requestStop()
        await controller.stop()
        await controller.start()
        XCTAssertEqual(controller.phase, .idle)
        XCTAssertNil(controller.terminal)
        await assertReleased(controller, connections: connections)
        let pending = await connections.manager.pendingCount, records = try await connections.journal.records()
        XCTAssertEqual(pending, 0)
        XCTAssertTrue(records.isEmpty)
        await scene.close()
    }

    func testWorkspaceSceneStartsSharedTerminalIntentInViewOnly() async throws {
        let (connections, scene, _) = try dormantScene()
        scene.connect("machine")
        let controller = try XCTUnwrap(scene.sessions["machine"])
        XCTAssertEqual(controller.muxAccessMode, .viewOnly)
        XCTAssertFalse(controller.requiresExistingMux, "Generic Connect can still discover a direct-only machine")
        await scene.close()
        await assertReleased(controller, connections: connections)
    }

    func testConcurrentSceneClosesJoinAndPreventScheduledConnectionStartup() async throws {
        let (connections, scene, _) = try dormantScene()
        scene.connect("machine")
        let controller = try XCTUnwrap(scene.sessions["machine"])
        scene.requestClose()
        await withTaskGroup(of: Void.self) { group in
            for _ in 0..<16 { group.addTask { await scene.close() } }
            await group.waitForAll()
        }
        await controller.retryCleanup()
        await assertReleased(controller, connections: connections)
        scene.connect("machine")
        XCTAssertTrue(scene.sessions["machine"] === controller)
        let records = try await connections.journal.records(), pending = await connections.manager.pendingCount
        XCTAssertTrue(records.isEmpty)
        XCTAssertEqual(pending, 0)
    }

    /// A new session acknowledges its own machine's earlier records as it
    /// starts, whatever the connection then does; other machines' records
    /// stay as they are.
    func testANewSessionAcknowledgesOnlyItsMachinesEarlierRecords() async throws {
        let (connections, scene, target) = try dormantScene()
        let journal = connections.journal
        func scope(_ machineID: String) throws -> TerminalSessionScope {
            try .init(workspaceID: target.workspaceID, workspaceRevision: target.revision,
                      machineID: machineID, routeID: target.routes[0].id,
                      transportID: UUID(), channelID: UUID())
        }
        let earlier = try await journal.beginCreation(scope: scope(target.machineID))
        try await journal.markInterrupted(epoch: earlier)
        let elsewhere = try await journal.beginCreation(scope: scope("other-machine"))
        try await journal.markInterrupted(epoch: elsewhere)
        try await appear(scene, selecting: "machine")
        let attempt = try await connect(scene, "machine")
        XCTAssertNotEqual(attempt.phase, .idle)
        let records = try await journal.records()
        XCTAssertEqual(records.first(where: { $0.id == earlier.id })?.state, .acknowledged)
        XCTAssertEqual(records.first(where: { $0.id == elsewhere.id })?.state, .interrupted)
        await scene.close()
    }

    /// Real UI prompt ownership through the production broker, without a
    /// network, private key, simulator credential or live-server variable.
    private func passwordPromptFixture() async throws -> (SSHAuthenticationBroker, WorkspaceSSHPasswordIdentity, MobileSSHPasswordPrompt) {
        let crypto = AppleCryptography()
        let point = try crypto.ed25519PublicKey(seed: Data(0..<32))
        func field(_ value: Data) -> Data {
            var length = UInt32(value.count).bigEndian
            return withUnsafeBytes(of: &length) { Data($0) } + value
        }
        let identity = try SSHPublicIdentity(blob: field(Data("ssh-ed25519".utf8)) + field(point))
        let initial = try document(identity: identity, host: identity, port: 22, user: "operator")
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: initial.raw) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        var routes = try XCTUnwrap(workspace["routes"] as? [[String: Any]])
        for index in routes.indices {
            routes[index]["authentication_methods"] = ["password"]
            routes[index]["credential_refs"] = [] as [String]
        }
        workspace["routes"] = routes; root["workspace"] = workspace
        let passwordDocument = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
        let archive = try AirGapArchive(workspace: passwordDocument, keys: [], using: crypto)
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("password-prompt-" + UUID().uuidString)
        let vault = SceneMemoryVault()
        let store = WorkspaceStore(persistence: try FileWorkspacePersistence(directory: directory), vault: vault)
        let installed = try await store.install(archive, envelopeDigest: crypto.sha256(try archive.encoded()),
            expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let broker = SSHAuthenticationBroker(store: store, signer: SceneMemorySigner(vault: vault),
                                             support: WorkspaceNativeSSHAuthentication.routeSupport)
        await broker.resume()
        let selected = try await broker.passwordIdentity(workspaceID: installed.id, expectedRevision: installed.revision,
            routeID: "route-alpha", presentedHostKey: WorkspaceDocument.HostKey(algorithm: identity.algorithm, fingerprint: identity.fingerprint))
        let prompt = MobileSSHPasswordPrompt()
        prompt.setActive(true)
        prompt.setScope(.init(workspaceID: installed.id, revision: installed.revision, machineID: "machine-alpha", destinationRouteIDs: ["route-alpha"]))
        return (broker, selected, prompt)
    }

    private func waitForPasswordPrompt(_ prompt: MobileSSHPasswordPrompt) async throws -> WorkspaceSSHPasswordRequest {
        let deadline = ContinuousClock.now + .seconds(3)
        while ContinuousClock.now < deadline {
            if let request = prompt.request { return request }
            await Task.yield()
        }
        XCTFail("The authorized password prompt did not appear")
        throw SSHAuthenticationError.unavailable
    }

    func testPasswordEditorSubmitsOnceAndClearsItsSecret() async throws {
        let (broker, selected, prompt) = try await passwordPromptFixture()
        let task = Task { try await broker.password(for: selected, transportID: UUID(), prompter: prompt) }
        let request = try await waitForPasswordPrompt(prompt)
        XCTAssertEqual(request.authorization.host, "127.0.0.1")
        XCTAssertEqual(request.authorization.user, "operator")
        XCTAssertEqual(request.authorization.presentedHostKey, selected.authorization.presentedHostKey)
        XCTAssertFalse(prompt.canSubmit)
        prompt.editSecret("synthetic password", requestID: request.id)
        XCTAssertTrue(prompt.canSubmit)
        prompt.editSecret(String(repeating: "x", count: 1025), requestID: request.id)
        XCTAssertEqual(prompt.secret, "synthetic password", "Oversized editing must not retain the paste")
        prompt.submit(requestID: request.id)
        XCTAssertNil(prompt.request); XCTAssertTrue(prompt.secret.isEmpty)
        let value = try await task.value
        XCTAssertEqual(value, Data("synthetic password".utf8))
        prompt.submit(requestID: request.id)
        XCTAssertNil(prompt.request)
    }

    func testPasswordEditorCancelsOnInactivityAndDestinationChange() async throws {
        for deactivate in [false, true] {
            let (broker, selected, prompt) = try await passwordPromptFixture()
            let task = Task { try await broker.password(for: selected, transportID: UUID(), prompter: prompt) }
            let request = try await waitForPasswordPrompt(prompt)
            prompt.editSecret("must not leave", requestID: request.id)
            if deactivate { prompt.setActive(false) }
            else { prompt.setScope(.init(workspaceID: selected.authorization.workspaceID,
                revision: selected.authorization.workspaceRevision, machineID: "machine-alpha", destinationRouteIDs: ["route-beta"])) }
            XCTAssertNil(prompt.request); XCTAssertTrue(prompt.secret.isEmpty)
            prompt.setActive(true)
            prompt.submit(requestID: request.id)
            do { _ = try await task.value; XCTFail("Retired prompt leaked a password") } catch {}
        }
    }

    func testDelayedPasswordDismissalCannotCancelAReplacementPrompt() async throws {
        let (broker, selected, prompt) = try await passwordPromptFixture()
        let first = Task { try await broker.password(for: selected, transportID: UUID(), prompter: prompt) }
        let old = try await waitForPasswordPrompt(prompt)
        prompt.editSecret("first", requestID: old.id); prompt.submit(requestID: old.id)
        _ = try await first.value
        let second = Task { try await broker.password(for: selected, transportID: UUID(), prompter: prompt) }
        let current = try await waitForPasswordPrompt(prompt)
        XCTAssertNotEqual(old.id, current.id)
        prompt.cancelNow(requestID: old.id)
        XCTAssertEqual(prompt.request?.id, current.id)
        prompt.editSecret("second", requestID: current.id)
        prompt.editSecret("stale edit", requestID: old.id)
        XCTAssertEqual(prompt.secret, "second")
        XCTAssertTrue(prompt.secretValue(requestID: old.id).isEmpty)
        prompt.submit(requestID: current.id)
        let value = try await second.value
        XCTAssertEqual(value, Data("second".utf8))
    }

    private func interactivePromptFixture() async throws -> (SSHAuthenticationBroker, WorkspaceSSHKeyboardInteractiveIdentity, MobileSSHKeyboardInteractivePrompt) {
        let crypto = AppleCryptography()
        let point = try crypto.ed25519PublicKey(seed: Data(0..<32))
        func field(_ value: Data) -> Data {
            var length = UInt32(value.count).bigEndian
            return withUnsafeBytes(of: &length) { Data($0) } + value
        }
        let host = try SSHPublicIdentity(blob: field(Data("ssh-ed25519".utf8)) + field(point))
        let initial = try document(identity: host, host: host, port: 22, user: "operator")
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: initial.raw) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        var routes = try XCTUnwrap(workspace["routes"] as? [[String: Any]])
        for index in routes.indices {
            routes[index]["authentication_methods"] = ["keyboard-interactive"]
            routes[index]["credential_refs"] = [] as [String]
        }
        workspace["routes"] = routes; root["workspace"] = workspace
        let source = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
        let archive = try AirGapArchive(workspace: source, keys: [], using: crypto)
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("interactive-prompt-" + UUID().uuidString)
        let vault = SceneMemoryVault()
        let store = WorkspaceStore(persistence: try FileWorkspacePersistence(directory: directory), vault: vault)
        let installed = try await store.install(archive, envelopeDigest: crypto.sha256(try archive.encoded()),
            expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let broker = SSHAuthenticationBroker(store: store, signer: SceneMemorySigner(vault: vault),
                                             support: WorkspaceNativeSSHAuthentication.routeSupport)
        await broker.resume()
        let selected = try await broker.keyboardInteractiveIdentity(workspaceID: installed.id, expectedRevision: installed.revision,
            routeID: "route-alpha", presentedHostKey: WorkspaceDocument.HostKey(algorithm: host.algorithm, fingerprint: host.fingerprint))
        let prompt = MobileSSHKeyboardInteractivePrompt()
        prompt.setActive(true)
        prompt.setScope(.init(workspaceID: installed.id, revision: installed.revision, machineID: "machine-alpha", destinationRouteIDs: ["route-alpha"]))
        return (broker, selected, prompt)
    }

    private func waitForInteractivePrompt(_ prompt: MobileSSHKeyboardInteractivePrompt) async throws -> WorkspaceSSHKeyboardInteractiveRequest {
        let deadline = ContinuousClock.now + .seconds(3)
        while ContinuousClock.now < deadline {
            if let request = prompt.request { return request }
            await Task.yield()
        }
        XCTFail("The authorized keyboard-interactive prompt did not appear")
        throw SSHAuthenticationError.unavailable
    }

    func testInteractiveEditorPreservesFieldsAndEchoPolicyAndClearsEveryResponse() async throws {
        let (broker, selected, prompt) = try await interactivePromptFixture()
        let transport = UUID(), attempt = UUID()
        let challenge = try SSHKeyboardInteractiveChallenge(round: 1, name: "Verification", instruction: "Synthetic server instruction",
            prompts: [.init(text: "Account label", echo: true), .init(text: "One-time code", echo: false)])
        let task = Task {
            try await broker.keyboardInteractiveResponses(for: selected, transportID: transport,
                attemptID: attempt, challenge: challenge, prompter: prompt)
        }
        let request = try await waitForInteractivePrompt(prompt)
        XCTAssertEqual(request.authorization.user, "operator")
        XCTAssertEqual(request.authorization.presentedHostKey, selected.authorization.presentedHostKey)
        XCTAssertEqual(request.challenge.prompts.map(\.echo), [true, false])
        prompt.editAnswer("operator", index: 0, requestID: request.id); prompt.editAnswer("012345", index: 1, requestID: request.id)
        prompt.editAnswer(String(repeating: "x", count: 1025), index: 1, requestID: request.id)
        prompt.editAnswer("invalid\0value", index: 0, requestID: request.id)
        XCTAssertEqual(prompt.answers, ["operator", "012345"])
        XCTAssertTrue(prompt.canSubmit)
        prompt.submit(requestID: request.id)
        XCTAssertNil(prompt.request); XCTAssertTrue(prompt.answers.isEmpty)
        let answers = try await task.value
        XCTAssertEqual(answers, [Data("operator".utf8), Data("012345".utf8)])

        // The next informational round still waits for explicit Continue, and
        // dismissal of the previous sheet cannot cancel this replacement.
        let notice = try SSHKeyboardInteractiveChallenge(round: 2, name: "Notice", instruction: "Confirm this sign-in", prompts: [])
        let next = Task {
            try await broker.keyboardInteractiveResponses(for: selected, transportID: transport,
                attemptID: attempt, challenge: notice, prompter: prompt)
        }
        let current = try await waitForInteractivePrompt(prompt)
        XCTAssertNotEqual(current.id, request.id)
        prompt.cancelNow(requestID: request.id)
        XCTAssertEqual(prompt.request?.id, current.id)
        XCTAssertTrue(prompt.canSubmit)
        prompt.submit(requestID: current.id)
        let empty = try await next.value
        XCTAssertTrue(empty.isEmpty)
        let finalChallenge = try SSHKeyboardInteractiveChallenge(round: 3, name: "Confirm", instruction: "",
            prompts: [.init(text: "Final code", echo: false)])
        let last = Task {
            try await broker.keyboardInteractiveResponses(for: selected, transportID: transport,
                attemptID: attempt, challenge: finalChallenge, prompter: prompt)
        }
        let finalRequest = try await waitForInteractivePrompt(prompt)
        prompt.editAnswer("new secret", index: 0, requestID: finalRequest.id)
        prompt.editAnswer("stale visible edit", index: 0, requestID: request.id)
        XCTAssertEqual(prompt.answers, ["new secret"])
        XCTAssertTrue(prompt.answer(at: 0, requestID: request.id).isEmpty,
                      "An old echo-enabled field must not expose a new hidden response")
        prompt.submit(requestID: finalRequest.id)
        let finalAnswers = try await last.value
        XCTAssertEqual(finalAnswers, [Data("new secret".utf8)])
        await broker.finishKeyboardInteractiveAttempt(selected)
    }

    func testInteractiveEditorRetiresAnswersOnInactivityAndMachineChange() async throws {
        for change in 0...2 {
            let (broker, selected, prompt) = try await interactivePromptFixture()
            let challenge = try SSHKeyboardInteractiveChallenge(round: 1, name: "Verification", instruction: "",
                prompts: [.init(text: "One-time code", echo: false)])
            let task = Task {
                try await broker.keyboardInteractiveResponses(for: selected, transportID: UUID(),
                    attemptID: UUID(), challenge: challenge, prompter: prompt)
            }
            let request = try await waitForInteractivePrompt(prompt)
            prompt.editAnswer("must not leave", index: 0, requestID: request.id)
            if change == 0 { prompt.setActive(false) }
            else { prompt.setScope(.init(workspaceID: selected.authorization.workspaceID,
                revision: selected.authorization.workspaceRevision,
                machineID: change == 2 ? "other-machine" : "machine-alpha",
                destinationRouteIDs: change == 2 ? ["route-alpha"] : ["route-beta"])) }
            XCTAssertNil(prompt.request); XCTAssertTrue(prompt.answers.isEmpty)
            prompt.setActive(true)
            prompt.submit(requestID: request.id)
            do { _ = try await task.value; XCTFail("A retired round returned an answer") } catch {}
            await broker.finishKeyboardInteractiveAttempt(selected)
        }
    }

    func testInteractiveRoutesRequireForegroundPromptOwnership() {
        let direct = TerminalSessionRoute(id: "route", credentialIDs: [], host: "destination.invalid", port: 22,
            authenticationChoices: [.keyboardInteractive])
        let jump = TerminalSessionRoute(id: "route", credentialIDs: ["key"], host: "destination.invalid", port: 22,
            promptingJump: true)
        let keyFirst = TerminalSessionRoute(id: "route", credentialIDs: ["key"], host: "destination.invalid", port: 22,
            allowsKeyboardInteractiveContinuation: true)
        let keyOnly = TerminalSessionRoute(id: "route", credentialIDs: ["key"], host: "destination.invalid", port: 22)
        XCTAssertTrue(direct.requiresAuthenticationPrompt)
        XCTAssertTrue(jump.requiresAuthenticationPrompt)
        XCTAssertTrue(keyFirst.requiresAuthenticationPrompt)
        XCTAssertFalse(keyOnly.requiresAuthenticationPrompt)
        XCTAssertEqual(keyFirst.choices, keyOnly.choices, "MFA permission must not reorder or widen authentication candidates")
    }

    func testPotentialPublicKeyMFAAtDestinationOrHopStopsWhenHiddenDuringConnect() async throws {
        for throughHop in [false, true] {
            let (connections, scene, original) = try dormantScene()
            let route = TerminalSessionRoute(id: "route", credentialIDs: ["key"], host: "destination.invalid", port: 22,
                allowsKeyboardInteractiveContinuation: !throughHop, promptingJump: throughHop)
            let target = TerminalSessionTarget(workspaceID: original.workspaceID, revision: original.revision,
                machineID: original.machineID, machineName: original.machineName, routes: [route])
            XCTAssertEqual(route.choices, [.publicKey("key")])
            let controller = TerminalSessionController(target: target, connections: connections, focus: scene)
            var reachedConnecting = false
            controller.onChange = {
                if controller.phase == .connecting {
                    reachedConnecting = true
                    controller.setPresentationVisible(false)
                }
            }
            // Hide synchronously at actual startup admission, before the
            // disabled manager can produce an unrelated offline error.
            await controller.start(openTerminal: false)
            controller.onChange = nil
            XCTAssertTrue(reachedConnecting)
            if case .ended = controller.phase {} else {
                XCTFail("A hidden potential MFA attempt was not cancelled: \(controller.phase)")
            }
            controller.setPresentationVisible(true)
            await controller.start(openTerminal: false)
            if case .ended = controller.phase {} else { XCTFail("Returning to the tab revived its retired attempt") }
            await controller.stop()
            await assertReleased(controller, connections: connections)
            let pending = await connections.manager.pendingCount
            XCTAssertEqual(pending, 0)
            await scene.close()
        }
    }

    func testPublicKeyMFAHopRefusalRetainsNextKeyOnlyForExplicitRetry() async throws {
        let (connections, scene, original) = try dormantScene()
        let route = TerminalSessionRoute(id: "route", credentialIDs: ["first", "second"],
            host: "destination.invalid", port: 22, authenticationChoices: [.publicKey("first"), .publicKey("second")],
            promptingJump: true, jumpHost: "jump.invalid")
        let target = TerminalSessionTarget(workspaceID: original.workspaceID, revision: original.revision,
            machineID: original.machineID, machineName: original.machineName, routes: [route])
        let first = TerminalSessionController(target: target, connections: connections, focus: scene)
        var plan = try first.makeRouteAttemptPlan(candidates: target.routes)
        let refused = try XCTUnwrap(plan.current)
        XCTAssertFalse(refused.selection.authentication.requiresForegroundPrompt,
                       "The first selected method is a key; the completed hop's MFA caused the explicit retry requirement")
        XCTAssertTrue(try first.rememberPasswordHopRefusal(refused, plan: &plan))
        XCTAssertThrowsError(try first.rememberPasswordHopRefusal(refused, plan: &plan))
        let pending = await connections.manager.pendingCount
        XCTAssertEqual(pending, 0, "Preserving a refused destination key must not repeat the hop's MFA")
        let automatic = TerminalSessionController(target: target, connections: connections, focus: scene, reconnectingFrom: first)
        XCTAssertEqual(try automatic.makeRouteAttemptPlan(candidates: target.routes).current?.selection.authentication,
                       .publicKey("first"))
        let retry = TerminalSessionController(target: target, connections: connections, focus: scene, retryingAfterPasswordHop: first)
        var next = try retry.makeRouteAttemptPlan(candidates: target.routes)
        XCTAssertEqual(next.current?.selection.authentication, .publicKey("second"))
        XCTAssertTrue(next.hasCredentialRefusal)
        let last = try XCTUnwrap(next.current)
        XCTAssertFalse(try retry.rememberPasswordHopRefusal(last, plan: &next))
        XCTAssertNil(next.current)
        await scene.close()
    }

    func testPublicKeyMFAExplicitMuxFailureDoesNotAutomaticallySignInAgain() async throws {
        let (connections, scene, original) = try dormantScene()
        let mux = TerminalSessionRoute(id: "mux", credentialIDs: ["key"], host: "destination.invalid", port: 22,
            allowsKeyboardInteractiveContinuation: true, mux: true, sessionCreation: .existingOnly)
        let direct = TerminalSessionRoute(id: "direct", credentialIDs: ["key"], host: "destination.invalid", port: 22,
            allowsKeyboardInteractiveContinuation: true)
        let target = TerminalSessionTarget(workspaceID: original.workspaceID, revision: original.revision,
            machineID: original.machineID, machineName: original.machineName, routes: [mux, direct])
        let failed = TerminalSessionController(target: target, connections: connections, focus: scene)
        XCTAssertEqual(try failed.makeRouteAttemptPlan(candidates: target.routes).current?.selection.routeID, "mux",
                       "An explicit mux route remains eligible even when its key may require MFA")
        XCTAssertTrue(failed.rememberPasswordMuxFallback())
        let pending = await connections.manager.pendingCount
        XCTAssertEqual(pending, 0, "Mux failure must not automatically repeat MFA to create a direct shell")
        let automatic = TerminalSessionController(target: target, connections: connections, focus: scene, reconnectingFrom: failed)
        XCTAssertEqual(try automatic.makeRouteAttemptPlan(candidates: target.routes).current?.selection.routeID, "mux")
        let retry = TerminalSessionController(target: target, connections: connections, focus: scene,
                                             retryingAfterPasswordHop: failed)
        XCTAssertEqual(try retry.makeRouteAttemptPlan(candidates: target.routes).current?.selection,
                       .init(routeID: "direct", authentication: .publicKey("key")))
        let consumed = TerminalSessionController(target: target, connections: connections, focus: scene,
                                                retryingAfterPasswordHop: failed)
        XCTAssertEqual(try consumed.makeRouteAttemptPlan(candidates: target.routes).current?.selection.routeID, "mux")
        await scene.close()
    }

    func testPasswordHopKeyRefusalAdvancesOnlyOnExplicitRetryAndConsumesEachKeyOnce() async throws {
        let (connections, scene, original) = try dormantScene()
        let route = TerminalSessionRoute(id: "route", credentialIDs: ["first", "second", "third"],
            host: "destination.invalid", port: 22, promptingJump: true, jumpHost: "jump.invalid")
        let target = TerminalSessionTarget(workspaceID: original.workspaceID, revision: original.revision,
            machineID: original.machineID, machineName: original.machineName, routes: [route])
        let first = TerminalSessionController(target: target, connections: connections, focus: scene)
        var plan = try first.makeRouteAttemptPlan(candidates: target.routes)
        let refused = try XCTUnwrap(plan.current)
        XCTAssertEqual(refused.selection.authentication, .publicKey("first"))
        XCTAssertTrue(try first.rememberPasswordHopRefusal(refused, plan: &plan))
        XCTAssertThrowsError(try first.rememberPasswordHopRefusal(refused, plan: &plan))
        let pending = await connections.manager.pendingCount
        XCTAssertEqual(pending, 0, "Recording refusal must not start another password login")

        let automatic = TerminalSessionController(target: target, connections: connections, focus: scene, reconnectingFrom: first)
        XCTAssertEqual(try automatic.makeRouteAttemptPlan(candidates: target.routes).current?.selection.authentication,
                       .publicKey("first"), "Automatic reconnect cannot consume explicit Retry metadata")
        let second = TerminalSessionController(target: target, connections: connections, focus: scene, retryingAfterPasswordHop: first)
        var next = try second.makeRouteAttemptPlan(candidates: target.routes)
        XCTAssertEqual(next.current?.selection.authentication, .publicKey("second"))
        XCTAssertTrue(next.hasCredentialRefusal, "The retry cannot silently change trust scopes")
        let secondRefusal = try XCTUnwrap(next.current)
        XCTAssertTrue(try second.rememberPasswordHopRefusal(secondRefusal, plan: &next))
        let third = TerminalSessionController(target: target, connections: connections, focus: scene, retryingAfterPasswordHop: second)
        var last = try third.makeRouteAttemptPlan(candidates: target.routes)
        XCTAssertEqual(last.current?.selection.authentication, .publicKey("third"))
        let lastRefusal = try XCTUnwrap(last.current)
        XCTAssertFalse(try third.rememberPasswordHopRefusal(lastRefusal, plan: &last))
        XCTAssertNil(last.current)
        await scene.close()
    }

    func testPasswordHopRetryCannotSurviveCancellationOrWorkspaceRevisionChange() async throws {
        let (connections, scene, original) = try dormantScene()
        let route = TerminalSessionRoute(id: "route", credentialIDs: ["first", "second"],
            host: "destination.invalid", port: 22, promptingJump: true)
        let target = TerminalSessionTarget(workspaceID: original.workspaceID, revision: original.revision,
            machineID: original.machineID, machineName: original.machineName, routes: [route])
        for cancel in [false, true] {
            let old = TerminalSessionController(target: target, connections: connections, focus: scene)
            var plan = try old.makeRouteAttemptPlan(candidates: target.routes)
            let refusal = try XCTUnwrap(plan.current)
            XCTAssertTrue(try old.rememberPasswordHopRefusal(refusal, plan: &plan))
            if cancel { old.requestStop() }
            let replacement = cancel ? target : TerminalSessionTarget(workspaceID: target.workspaceID, revision: UUID(),
                machineID: target.machineID, machineName: target.machineName, routes: target.routes)
            let next = TerminalSessionController(target: replacement, connections: connections, focus: scene,
                                                   retryingAfterPasswordHop: old)
            XCTAssertEqual(try next.makeRouteAttemptPlan(candidates: replacement.routes).current?.selection.authentication,
                           .publicKey("first"))
            let stale = TerminalSessionController(target: target, connections: connections, focus: scene, retryingAfterPasswordHop: old)
            XCTAssertEqual(try stale.makeRouteAttemptPlan(candidates: target.routes).current?.selection.authentication,
                           .publicKey("first"), "A rejected transfer must not leave reusable Retry metadata")
        }
        await scene.close()
    }

    func testPasswordMuxFailureNeedsAnExplicitScopedRetryBeforeDirectSSH() async throws {
        let (connections, scene, original) = try dormantScene()
        let mux = TerminalSessionRoute(id: "mux", credentialIDs: [], host: "destination.invalid", port: 22,
            authenticationChoices: [.password], mux: true, sessionCreation: .existingOnly)
        let direct = TerminalSessionRoute(id: "direct", credentialIDs: [], host: "destination.invalid", port: 22,
            authenticationChoices: [.password])
        let target = TerminalSessionTarget(workspaceID: original.workspaceID, revision: original.revision,
            machineID: original.machineID, machineName: original.machineName, routes: [mux, direct])
        let failed = TerminalSessionController(target: target, connections: connections, focus: scene)
        XCTAssertTrue(failed.rememberPasswordMuxFallback())
        let pending = await connections.manager.pendingCount
        XCTAssertEqual(pending, 0)
        let retry = TerminalSessionController(target: target, connections: connections, focus: scene,
                                                retryingAfterPasswordHop: failed)
        XCTAssertEqual(try retry.makeRouteAttemptPlan(candidates: target.routes).current?.selection,
                       .init(routeID: "direct", authentication: .password))
        let consumed = TerminalSessionController(target: target, connections: connections, focus: scene,
                                                   retryingAfterPasswordHop: failed)
        XCTAssertEqual(try consumed.makeRouteAttemptPlan(candidates: target.routes).current?.selection.routeID, "mux")

        let cancelled = TerminalSessionController(target: target, connections: connections, focus: scene)
        XCTAssertTrue(cancelled.rememberPasswordMuxFallback())
        cancelled.requestStop()
        let fresh = TerminalSessionController(target: target, connections: connections, focus: scene,
                                                retryingAfterPasswordHop: cancelled)
        XCTAssertEqual(try fresh.makeRouteAttemptPlan(candidates: target.routes).current?.selection.routeID, "mux")
        await scene.close()
    }

    func testStoppingWhileStartupIsPendingJoinsBeforeReportingSettled() async throws {
        let live = try await liveScene(), scene = live.scene
        scene.connect("alpha")
        let controller = try XCTUnwrap(scene.sessions["alpha"])
        for _ in 0..<200 where controller.phase == .idle { await Task.yield() }
        XCTAssertNotEqual(controller.phase, .idle)
        scene.requestClose()
        async let first: Void = scene.close()
        async let second: Void = scene.close()
        _ = await (first, second)
        await assertReleased(controller, connections: live.connections)
        let remaining = await live.connections.manager.connections(), pending = await live.connections.manager.pendingCount
        XCTAssertTrue(remaining.isEmpty)
        XCTAssertEqual(pending, 0)
        XCTAssertNotEqual(controller.phase, .live)
    }
}
