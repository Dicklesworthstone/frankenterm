import XCTest
import UIKit
import CoreText
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

@MainActor
final class TerminalSessionControllerTests: XCTestCase {
    private func document(identity: SSHPublicIdentity, host: SSHPublicIdentity, port: UInt16, user: String) throws -> WorkspaceDocument {
        let theme: [String: Any] = ["id": "theme", "name": "Live", "foreground": "#E6E6E6", "background": "#10223C",
            "accent": "#3B82F6", "ansi": Array(repeating: "#808080", count: 16), "cursor_foreground": "#000000",
            "cursor_background": "#FFFFFF", "cursor_border": "#FFFFFF", "selection_foreground": "#FFFFFF",
            "selection_background": "#345D91"]
        // Two machines on the same loopback host, each with its own route, so
        // each gets its own SSH connection, shell and journal record.
        let workspace: [String: Any] = [
            "id": "workspace", "name": "Scene", "groups": [["id": "group", "name": "Local", "machine_order": ["alpha", "beta"]]],
            "machines": ["alpha", "beta"].map { ["id": $0, "name": $0.capitalized, "theme_ref": "theme", "route_refs": ["route-\($0)"],
                                                 "preferred_route_ref": "route-\($0)", "auto_connect": false] as [String: Any] },
            "routes": ["alpha", "beta"].map { ["id": "route-\($0)", "mode": "direct_ssh", "session_creation": "on_select",
                                               "host": "127.0.0.1", "port": Int(port), "user": user, "credential_refs": ["key"],
                                               "authentication_methods": ["publickey"], "host_trust_ref": "trust",
                                               "jump_routes": []] as [String: Any] },
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
    }

    private func liveScene(liveness: WorkspaceSSHLiveness = .standard) async throws -> LiveScene {
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
                                                journalIdentifier: "scene-test-" + UUID().uuidString,
                                                signer: SceneMemorySigner(vault: vault), liveness: liveness)
        await connections.setActive(true)
        let machines = ["alpha", "beta"].map { id in
            SceneMachine(id: id, name: id.capitalized, target: .init(workspaceID: installed.id, revision: installed.revision,
                machineID: id, machineName: id.capitalized, routeID: "route-\(id)", credentialID: "key"),
                unavailable: "", theme: nil)
        }
        let scene = try WorkspaceTerminalScene(workspaceID: installed.id, workspaceName: "Scene", machines: machines,
                                               initialMachineID: "alpha", connections: connections)
        return LiveScene(connections: connections, scene: scene, outPath: outPath)
    }

    /// Connects a machine through the scene and waits for its session to settle
    /// into a phase other than connecting.
    @discardableResult
    private func connect(_ scene: WorkspaceTerminalScene, _ machine: String,
                         acknowledging: Bool = false) async throws -> TerminalSessionController {
        if acknowledging, let current = scene.sessions[machine] {
            for _ in 0..<200 where !current.recoveryReviewReady {
                try await Task.sleep(for: .milliseconds(50))
            }
            XCTAssertTrue(current.recoveryReviewReady, "a new session must use the exact displayed history")
        }
        scene.connect(machine, acknowledging: acknowledging)
        let session = try XCTUnwrap(scene.sessions[machine])
        for _ in 0..<400 where session.phase == .idle || session.phase == .connecting {
            try await Task.sleep(for: .milliseconds(50))
        }
        return session
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
        let record = try XCTUnwrap(history.first)
        XCTAssertEqual(record.state, .interrupted)
        XCTAssertTrue(record.canResolve)
        XCTAssertTrue(record.inputEvidence.contains(.transportAccepted))
        XCTAssertTrue(record.inputEvidence.mayHaveRemoteEffect)
    }

    /// A shell that exits ends the session from inside the pump task; teardown
    /// must still finish there, and the next session on the machine needs the
    /// user's acknowledgement of the first one's unknown outcome.
    func testRemoteExitSettlesAndANewSessionNeedsAcknowledgement() async throws {
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

        let unacknowledged = try await connect(scene, "alpha")
        XCTAssertNil(first.retainedOutput, "replacing a controller releases its volatile output")
        XCTAssertEqual(unacknowledged.phase, .needsReview, "the ended session's outcome must be reviewed first")
        XCTAssertTrue(unacknowledged.settled)
        await assertReleased(unacknowledged, connections: connections)

        let second = try await connect(scene, "alpha", acknowledging: true)
        let again = try shown(second)
        defer { again.removeFromSuperview() }
        try await type("echo AGAIN-$((40+2)) > \(live.outPath)", into: again)
        let output = try await hostFile(live.outPath, containing: "AGAIN-42")
        XCTAssertTrue(output.contains("AGAIN-42"), "the acknowledged new session must run commands (output: \(output))")
        await scene.close()
        XCTAssertTrue(second.settled)
        await assertReleased(second, connections: connections)
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
        let beta = try shown(try await connect(scene, "beta"))
        defer { beta.removeFromSuperview() }
        XCTAssertEqual(scene.selectedMachineID, "beta", "connecting a machine selects it")
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

    /// The rendered screen once it shows `marker` (or after ~10 s).
    private func screen(_ terminal: NativeTerminalView, containing marker: String) async throws -> String {
        var text = ""
        for _ in 0..<200 where !text.contains(marker) {
            try await Task.sleep(for: .milliseconds(50))
            text = terminal.renderer.accessibilityValue ?? ""
        }
        return text
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

        let beta = try shown(try await connect(scene, "beta"))
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

        let again = try await connect(scene, "alpha", acknowledging: true)
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
            machineName: "Never connected", routeID: "route", credentialID: "key")
        let machine = SceneMachine(id: "machine", name: "Never connected", target: target, unavailable: "", theme: nil)
        let scene = try WorkspaceTerminalScene(workspaceID: target.workspaceID, workspaceName: "Dormant",
            machines: [machine], initialMachineID: "machine", connections: connections)
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
        scene.connect("machine", acknowledging: true)
        XCTAssertTrue(scene.sessions["machine"] === controller)
        let records = try await connections.journal.records(), pending = await connections.manager.pendingCount
        XCTAssertTrue(records.isEmpty)
        XCTAssertEqual(pending, 0)
    }

    /// A new or changed record after review must remain unresolved. A generic
    /// confirmation must never reread and acknowledge unseen session history.
    func testNewSessionActionCannotAcknowledgeRecordsCreatedAfterReview() async throws {
        let (connections, scene, target) = try dormantScene()
        let journal = connections.journal
        func scope() throws -> TerminalSessionScope {
            try .init(workspaceID: target.workspaceID, workspaceRevision: target.revision,
                      machineID: target.machineID, routeID: target.routeID,
                      transportID: UUID(), channelID: UUID())
        }
        let firstEpoch = try await journal.beginCreation(scope: scope())
        try await journal.markInterrupted(epoch: firstEpoch)
        let first = try await connect(scene, "machine")
        for _ in 0..<200 where !first.recoveryReviewReady { try await Task.sleep(for: .milliseconds(20)) }
        XCTAssertTrue(first.recoveryReviewReady)
        let displayed = try XCTUnwrap(first.recoveryRecords.first)
        XCTAssertTrue(displayed.inputEvidence.isEmpty)

        // Another explicit owner resolves the first record and creates a new
        // session while the original review is still visible on this scene.
        try await journal.resolve(displayed)
        let unseen = try await journal.beginCreation(scope: scope())
        try await journal.markInterrupted(epoch: unseen)
        let attempted = try await connect(scene, "machine", acknowledging: true)
        XCTAssertFalse(attempted === first)
        XCTAssertEqual(attempted.phase, .failed(TerminalSessionController.describe(TerminalSessionJournalError.staleReview)))
        for _ in 0..<200 where !attempted.recoveryReviewReady { try await Task.sleep(for: .milliseconds(20)) }
        let records = try await journal.records()
        XCTAssertEqual(records.first(where: { $0.id == unseen.id })?.state, .interrupted)
        XCTAssertEqual(attempted.recoveryRecords.map(\.id), [unseen.id])
        let pending = await connections.manager.pendingCount
        XCTAssertEqual(pending, 0, "stale review must fail before connection admission")
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
