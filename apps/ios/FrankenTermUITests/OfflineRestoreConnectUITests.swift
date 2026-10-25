import XCTest

// The M0 user journey through the real app UI: open an encrypted .ftairgap
// workspace, enter its transfer code, review, restore into the protected
// Keychain, open the machine, Connect over direct SSH and use the shell.
// Signing uses the production user-presence signer, so the operator must enroll
// and match simulated Face ID from the host while the test runs (see the runner
// notes in apps/apple/README.md). Inputs via xcodebuild's TEST_RUNNER_ prefix:
//   FT_UI_AIRGAP_PATH (host path of the .ftairgap), FT_UI_AIRGAP_CODE,
//   FT_UI_WORKSPACE_NAME, FT_UI_MACHINE_NAME. Skipped when absent.
//
// Every check reads text the remote shell produced: each command computes its
// marker, so the echoed command line itself can never satisfy the wait.
final class OfflineRestoreConnectUITests: XCTestCase {
    private struct Inputs { let path: String, code: String, workspace: String, machine: String }

    private func inputs() throws -> Inputs {
        let environment = ProcessInfo.processInfo.environment
        guard let path = environment["FT_UI_AIRGAP_PATH"], let code = environment["FT_UI_AIRGAP_CODE"],
              let workspace = environment["FT_UI_WORKSPACE_NAME"], let machine = environment["FT_UI_MACHINE_NAME"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_UI_{AIRGAP_PATH,AIRGAP_CODE,WORKSPACE_NAME,MACHINE_NAME}")
        }
        return Inputs(path: path, code: code, workspace: workspace, machine: machine)
    }

    private func snapshot(_ name: String) {
        let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        attachment.name = name; attachment.lifetime = .keepAlways
        add(attachment)
    }

    @MainActor
    private func launch() -> XCUIApplication {
        addUIInterruptionMonitor(withDescription: "Face ID permission") { alert in
            for label in ["OK", "Allow"] where alert.buttons[label].exists { alert.buttons[label].tap(); return true }
            return false
        }
        XCUIDevice.shared.orientation = .portrait
        let app = XCUIApplication()
        app.launch()
        return app
    }

    @MainActor
    private func savedWorkspace(_ app: XCUIApplication, _ inputs: Inputs) -> XCUIElement {
        app.buttons.containing(NSPredicate(format: "label CONTAINS %@", inputs.workspace)).firstMatch
    }

    @MainActor
    private func restore(_ app: XCUIApplication, _ inputs: Inputs) {
        XCUIDevice.shared.system.open(URL(fileURLWithPath: inputs.path))
        // A file carries no code frame, so the code is typed once.
        let field = app.secureTextFields["Transfer code"]
        XCTAssertTrue(field.waitForExistence(timeout: 20), "the opened package must reach the unlock step")
        snapshot("1-package-collected")
        field.tap(); field.typeText(inputs.code)
        app.buttons["Unlock"].tap()

        // Unlocked, it asks only Restore.
        let confirm = app.buttons["Restore"]
        XCTAssertTrue(confirm.waitForExistence(timeout: 30), "the unlocked package must reach its review")
        snapshot("2-review")
        confirm.tap()
        let done = app.buttons["Done"]
        XCTAssertTrue(done.waitForExistence(timeout: 30), "the restore must publish")
        snapshot("3-restored")
        done.tap()
    }

    /// Opens the restored machine and connects; an earlier session on it is
    /// acknowledged without asking.
    @MainActor
    private func connect(_ app: XCUIApplication, _ inputs: Inputs) -> XCUIElement {
        let saved = savedWorkspace(app, inputs)
        XCTAssertTrue(saved.waitForExistence(timeout: 20), "the restored workspace must be listed")
        saved.tap()
        let connect = app.buttons["Connect"].firstMatch
        XCTAssertTrue(connect.waitForExistence(timeout: 20), "a ready direct SSH route must offer Connect")
        snapshot("4-machine")
        connect.tap()
        // A first-use Face ID permission is a system alert. It is answered
        // there: a tap on the app could land on a terminal that is already
        // shown (a prepared connection) and put its keyboard away.
        let allow = XCUIApplication(bundleIdentifier: "com.apple.springboard").alerts.buttons["OK"]
        if allow.waitForExistence(timeout: 3) { allow.tap() }

        let terminal = app.descendants(matching: .any)["Terminal output"].firstMatch
        XCTAssertTrue(terminal.waitForExistence(timeout: 60), "the terminal surface must appear")
        XCTAssertTrue(chip(app, inputs.machine, "connected").waitForExistence(timeout: 5), "the strip shows the machine live")
        snapshot("5-terminal")
        return terminal
    }

    /// A new launch returns to the terminal the last one showed and connects
    /// it again, without a tap. A test run on its own, with nothing saved
    /// yet, imports and connects first.
    @MainActor
    private func reopen(_ app: XCUIApplication, _ inputs: Inputs) -> XCUIElement {
        let terminal = app.descendants(matching: .any)["Terminal output"].firstMatch
        guard terminal.waitForExistence(timeout: 20) else {
            // Saved already: the list or the workspace's page is showing.
            if savedWorkspace(app, inputs).exists || app.navigationBars[inputs.workspace].exists {
                XCTFail("a relaunch must return to the terminal the last launch showed")
            } else { restore(app, inputs) }
            return connect(app, inputs)
        }
        let allow = XCUIApplication(bundleIdentifier: "com.apple.springboard").alerts.buttons["OK"]
        if allow.waitForExistence(timeout: 3) { allow.tap() }
        XCTAssertTrue(chip(app, inputs.machine, "connected").waitForExistence(timeout: 60),
                      "the terminal a relaunch returns to connects again")
        snapshot("4r-returned")
        return terminal
    }

    /// A machine's chip in the strip, by name and state.
    @MainActor
    private func chip(_ app: XCUIApplication, _ machine: String, _ state: String) -> XCUIElement {
        app.buttons["\(machine), \(state)"].firstMatch
    }

    private func screen(_ terminal: XCUIElement) -> String { terminal.value as? String ?? "" }

    @discardableResult
    private func waitFor(_ marker: String, in terminal: XCUIElement, timeout: TimeInterval = 15,
                       _ message: String) -> Bool {
        let shown = expectation(for: NSPredicate(format: "value CONTAINS %@", marker), evaluatedWith: terminal)
        let completed = XCTWaiter().wait(for: [shown], timeout: timeout) == .completed
        XCTAssertTrue(completed, "\(message); screen:\n\(screen(terminal))")
        return completed
    }

    /// Fails if `marker` shows up within `timeout`.
    private func never(_ marker: String, in terminal: XCUIElement, timeout: TimeInterval = 2, _ message: String) {
        let shown = XCTNSPredicateExpectation(predicate: NSPredicate(format: "value CONTAINS %@", marker), object: terminal)
        let appeared = XCTWaiter().wait(for: [shown], timeout: timeout) == .completed
        XCTAssertFalse(appeared, "\(message); screen:\n\(screen(terminal))")
    }

    /// The last `SZ-<rows>x<columns>` the shell printed.
    private func reportedSize(_ terminal: XCUIElement) -> (rows: Int, columns: Int)? {
        let text = screen(terminal)
        guard let match = text.matches(of: /SZ-(\d+)x(\d+)/).last,
              let rows = Int(match.1), let columns = Int(match.2) else { return nil }
        return (rows, columns)
    }

    /// Asks the shell for its size and returns what it prints (the echoed
    /// command never matches), waiting up to 15 s for an answer other than
    /// `previous` while the screen still shows the earlier one.
    private func shellSize(_ app: XCUIApplication, _ terminal: XCUIElement,
                           unlike previous: (rows: Int, columns: Int)? = nil) -> (rows: Int, columns: Int)? {
        app.typeText("clear; echo SZ-$(stty size | tr ' ' x)\n")
        var size: (rows: Int, columns: Int)?
        for _ in 0..<15 {
            size = reportedSize(terminal)
            if let size, previous.map({ $0 != size }) ?? true { break }
            sleep(1)
        }
        return size
    }

    @MainActor
    func test1RestoreEncryptedWorkspaceAndConnectToItsMachine() throws {
        let inputs = try inputs()
        let app = launch()
        restore(app, inputs)
        let terminal = connect(app, inputs)

        // Keystrokes leave through SSH; only the remote shell can turn the
        // arithmetic into 42, and the result is read from the admitted frame.
        app.typeText("echo FRANKEN-$((6*7))\n")
        waitFor("FRANKEN-42", in: terminal, timeout: 30, "a typed command must run in the remote shell")
        snapshot("6-command-output")

        // The machine's theme is the terminal's palette: an empty area shows
        // the theme background rather than the engine default.
        if let expected = ProcessInfo.processInfo.environment["FT_UI_THEME_BACKGROUND"].flatMap(rgb) {
            let shown = sample(terminal, x: 0.9, y: 0.45)
            XCTAssertTrue(zip(shown, expected).allSatisfy { abs($0 - $1) <= 6 },
                          "terminal background \(shown) must be the theme's \(expected)")
        }

        // Select Output, under More, shows the received viewport as read-only text.
        app.buttons["terminal.more"].tap()
        app.buttons["Select Output"].tap()
        let captured = app.textViews["Captured terminal output"]
        XCTAssertTrue(captured.waitForExistence(timeout: 10), "Select Output must show the captured viewport")
        XCTAssertTrue((captured.value as? String ?? "").contains("FRANKEN-42"), "the capture holds the shell's output")
        snapshot("6b-select-output")
        app.buttons["Done"].tap()
        XCTAssertTrue(captured.waitForNonExistence(timeout: 5), "Done returns to the terminal")
    }

    private func rgb(_ hex: String) -> [Int]? {
        guard hex.count == 7, let value = Int(hex.dropFirst(), radix: 16) else { return nil }
        return [value >> 16 & 0xFF, value >> 8 & 0xFF, value & 0xFF]
    }

    /// The sRGB color at a unit point (top-left origin) of an element's screenshot.
    private func sample(_ element: XCUIElement, x: CGFloat, y: CGFloat) -> [Int] {
        guard let image = element.screenshot().image.cgImage,
              let space = CGColorSpace(name: CGColorSpace.sRGB) else { return [] }
        var pixel = [UInt8](repeating: 0, count: 4)
        let column = CGFloat(Int(x * CGFloat(image.width))), row = CGFloat(Int(y * CGFloat(image.height)))
        pixel.withUnsafeMutableBytes { bytes in
            let context = CGContext(data: bytes.baseAddress, width: 1, height: 1, bitsPerComponent: 8, bytesPerRow: 4,
                                    space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
            context?.draw(image, in: CGRect(x: -column, y: row + 1 - CGFloat(image.height),
                                            width: CGFloat(image.width), height: CGFloat(image.height)))
        }
        return pixel.prefix(3).map(Int.init)
    }

    @MainActor
    func test2TerminalKeysFullScreenRotationAndBackground() throws {
        let inputs = try inputs()
        let app = launch()
        let terminal = reopen(app, inputs)
        app.typeText("echo READY-$((40+2))\n")
        guard waitFor("READY-42", in: terminal, timeout: 30, "the shell must accept input") else { return }

        // Ctrl from the accessory row: without a delivered ^C the sleep keeps
        // the typed-ahead echo waiting for 30 s, well past this wait. The
        // row's keys are found by identifier: an iPad's keyboard has its own
        // Tab, Escape and hide keys.
        app.typeText("sleep 30\n")
        app.buttons["terminal.key.Control modifier"].tap()
        app.typeText("c")
        app.typeText("echo CTRL-$((40+2))\n")
        waitFor("CTRL-42", in: terminal, timeout: 10, "Ctrl+C from the accessory row must interrupt the shell")

        // Left arrow edits the line: 402 becomes 40+2 only if the cursor moved.
        app.typeText("echo ARROW-$((402))")
        for _ in 0..<3 { app.buttons["terminal.key.Left arrow"].tap() }
        app.typeText("+\n")
        waitFor("ARROW-42", in: terminal, "the accessory Left arrow must move the line editor's cursor")

        // Tab completes the only /usr entry starting with "shar"; without it cd fails.
        app.typeText("cd /usr/shar")
        app.buttons["terminal.key.Tab"].tap()
        app.typeText(" && echo TAB-$(basename $PWD) && cd\n")
        waitFor("TAB-share", in: terminal, "the accessory Tab must reach shell completion")

        // Non-ASCII text arrives as UTF-8 and is drawn back.
        app.typeText("printf '%s-%s\\n' café 日本\n")
        waitFor("café-日本", in: terminal, "typed Unicode must round-trip through the shell")

        // Dismissing the keyboard retires its input; a tap on the terminal
        // establishes a new input focus on the same session.
        app.buttons["terminal.keyboard.hide"].tap()
        XCTAssertTrue(app.keyboards.firstMatch.waitForNonExistence(timeout: 5), "the keyboard must go away")
        terminal.tap()
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 10), "a tap must bring the keyboard back")
        app.typeText("echo BACK-$((40+2))\n")
        waitFor("BACK-42", in: terminal, "the session must accept input again after the keyboard returns")

        // Dragging down scrolls back through history; the button returns live.
        app.typeText("for i in $(seq 1 60); do echo UIH-$i; done\n")
        waitFor("UIH-60", in: terminal, "the shell must print its lines")
        func rows() -> [String] { screen(terminal).components(separatedBy: "\n") }
        for _ in 0..<6 where !rows().contains("UIH-1") {
            terminal.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.15))
                .press(forDuration: 0.05, thenDragTo: terminal.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.85)))
        }
        XCTAssertTrue(rows().contains("UIH-1"), "dragging down must reach the first lines; screen:\n\(screen(terminal))")
        XCTAssertFalse(rows().contains("UIH-60"))
        snapshot("6c-history")
        app.buttons["Return to live output"].tap()
        waitFor("UIH-60", in: terminal, "the button must return to live output")
        XCTAssertTrue(app.buttons["Return to live output"].waitForNonExistence(timeout: 5), "live output hides the button")
        app.typeText("echo LIVE-$((40+2))\n")
        waitFor("LIVE-42", in: terminal, "typing works again on the live screen")

        // A full-screen program on the alternate screen, left with Esc: if Esc
        // is lost, ":q!" is inserted as text and vim never exits.
        app.typeText("vim -u NONE -N\n")
        waitFor("\n~\n~\n~", in: terminal, "vim must draw its empty buffer")
        snapshot("7-vim")
        app.typeText("iFT-INSERT")
        app.buttons["terminal.key.Escape"].tap()
        app.typeText(":q!\n")
        app.typeText("echo ESC-$((40+2))\n")
        waitFor("ESC-42", in: terminal, "Esc must leave insert mode so :q! exits vim")
        XCTAssertFalse(screen(terminal).contains("FT-INSERT"), "leaving vim must restore the primary screen")

        // Rotation changes the remote PTY: landscape has more columns, fewer rows.
        let portrait = try XCTUnwrap(shellSize(app, terminal), "the shell must report its size; screen:\n\(screen(terminal))")
        XCUIDevice.shared.orientation = .landscapeLeft
        sleep(2)
        let landscape = try XCTUnwrap(shellSize(app, terminal, unlike: portrait), screen(terminal))
        snapshot("8-landscape")
        XCTAssertGreaterThan(landscape.columns, portrait.columns, "landscape must widen the PTY")
        XCTAssertLessThan(landscape.rows, portrait.rows, "landscape must shorten the PTY")
        XCUIDevice.shared.orientation = .portrait

        // A quick trip to the home screen keeps the session: the same shell
        // (its variable still set) answers on return.
        app.typeText("export FT_KEPT=yes\n")
        XCUIDevice.shared.press(.home)
        sleep(3)
        app.activate()
        snapshot("9-after-background")
        terminal.tap()
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 30), "the session takes the keyboard again")
        app.typeText("echo KEPT-$FT_KEPT\n")
        waitFor("KEPT-yes", in: terminal, timeout: 30, "the same session runs commands after the background trip")
        snapshot("10-kept")
    }

    /// M1 through the machine strip (numbered to run after the journeys above;
    /// XCTest runs a class's tests in name order, and only the first imports):
    /// with two machines restored, opening the first signs the second in
    /// in the background; choosing it opens its own shell, and switching
    /// back finds the first as it was. Typing reaches only the machine on
    /// screen.
    @MainActor
    func test3MachineStripSwitchesBetweenConnectedMachines() throws {
        let inputs = try inputs()
        guard let second = ProcessInfo.processInfo.environment["FT_UI_SECOND_MACHINE_NAME"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_UI_SECOND_MACHINE_NAME for a package with a second machine")
        }
        let app = launch()
        let terminal = reopen(app, inputs)
        app.typeText("export FT_WHO=first; echo WHO-$FT_WHO-$((1+1))\n")
        waitFor("WHO-first-2", in: terminal, timeout: 30, "the first machine's shell must run")

        let secondChip = app.buttons.matching(NSPredicate(format: "label IN %@",
            ["\(second), signed in, terminal not open", "\(second), connected"])).firstMatch
        XCTAssertTrue(secondChip.waitForExistence(timeout: 60), "the second machine signs in in the background")
        snapshot("11-second-machine-ready")
        secondChip.tap()
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 30), "the second session must take the keyboard")
        let screen = app.descendants(matching: .any)["Terminal output"].firstMatch
        // Each shell says when its terminal is resized: switching machines
        // with the keyboard up must not resize either of them.
        app.typeText("export FT_WHO=second; trap 'echo WINCH-$FT_WHO' WINCH; echo WHO-$FT_WHO-$((2+2))\n")
        waitFor("WHO-second-4", in: screen, timeout: 30, "the second machine's shell must run")
        XCTAssertFalse(self.screen(screen).contains("WHO-first-2"), "the second machine has its own screen")
        snapshot("12-second-terminal")

        chip(app, inputs.machine, "connected").tap()
        waitFor("WHO-first-2", in: screen, timeout: 10, "returning keeps the first machine's screen")
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 10), "the returned machine gets the keyboard")
        app.typeText("trap 'echo WINCH-$FT_WHO' WINCH; echo AGAIN-$FT_WHO-$((3+3))\n")
        waitFor("AGAIN-first-6", in: screen, timeout: 30, "typing after the switch reaches the first machine's shell")
        snapshot("13-back-to-first")

        // The keyboard stays up from one machine to the next, so switching
        // on the strip resizes neither terminal.
        app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "\(second), connected")).firstMatch.tap()
        waitFor("WHO-second-4", in: screen, timeout: 10, "the strip shows the second machine again")
        never("WINCH-second", in: screen, "switching resized the second machine's terminal")
        chip(app, inputs.machine, "connected").tap()
        waitFor("AGAIN-first-6", in: screen, timeout: 10, "the strip shows the first machine again")
        never("WINCH-first", in: screen, "switching resized the first machine's terminal")
        XCTAssertTrue(app.keyboards.firstMatch.exists, "the keyboard stayed up")

        // A hardware keyboard switches too: ⇧⌘] and ⇧⌘[ step through the strip.
        app.typeKey("]", modifierFlags: [.command, .shift])
        waitFor("WHO-second-4", in: screen, timeout: 10, "⇧⌘] shows the next machine")
        XCTAssertFalse(self.screen(screen).contains("AGAIN-first-6"), "the next machine has its own screen")
        app.typeKey("[", modifierFlags: [.command, .shift])
        waitFor("AGAIN-first-6", in: screen, timeout: 10, "⇧⌘[ comes back to the first machine")
        app.typeText("echo KEYS-$FT_WHO-$((4+4))\n")
        waitFor("KEYS-first-8", in: screen, timeout: 30, "typing after a keyboard switch reaches the machine on screen")
        // A hardware key hides the software keyboard, which resizes the
        // terminal just before the switch; the machine left redraws. That is
        // not news.
        XCTAssertFalse(chip(app, second, "connected, new output").waitForExistence(timeout: 2),
                       "a machine just left is not marked for its redraw")
        snapshot("14-keyboard-switch")

        // A machine that prints while another is on screen is marked on the
        // strip until it is on screen again.
        app.typeText("sleep 6; echo AWAY-$FT_WHO-$((5+5))\n")
        waitFor("sleep 6; echo AWAY", in: screen, timeout: 10, "the command is running before the switch")
        app.typeKey("]", modifierFlags: [.command, .shift])
        waitFor("WHO-second-4", in: screen, timeout: 10, "⇧⌘] shows the next machine")
        let marked = chip(app, inputs.machine, "connected, new output")
        XCTAssertTrue(marked.waitForExistence(timeout: 20), "the first machine printed while away and is marked")
        XCTAssertFalse(chip(app, second, "connected, new output").exists, "the machine on screen is never marked")
        snapshot("15-unread-mark")
        marked.tap()
        waitFor("AWAY-first-10", in: screen, timeout: 10, "the mark leads to the new output")
        XCTAssertTrue(chip(app, inputs.machine, "connected").waitForExistence(timeout: 5), "being on screen clears the mark")
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 10), "the returned machine gets the keyboard")
        app.typeKey("k", modifierFlags: .command)
        let overview = app.navigationBars["Fleet Overview"]
        XCTAssertTrue(overview.waitForExistence(timeout: 10), "⌘K opens the fleet overview")
        if overview.exists { overview.buttons["Done"].tap() }
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 10), "closing the overview gives the keyboard back")
        app.typeKey("d", modifierFlags: [.command, .shift])
        let discard = app.buttons["Discard Draft"]
        XCTAssertTrue(discard.waitForExistence(timeout: 10), "⇧⌘D opens Compose Command")
        if discard.exists { discard.tap() }
    }

    /// A newer export of the saved workspace replaces it in one tap, as when
    /// the Mac exports a group the phone already has. The second package
    /// (FT_UI_REIMPORT_PATH, FT_UI_REIMPORT_CODE) is the same workspace with
    /// changed host-trust references, so connection settings change. One
    /// line says so; nothing else is asked. Opened over the live terminal,
    /// the restore ends its sessions first; closing the restore, and later
    /// the replacement, each bring that terminal back connected.
    @MainActor
    func test4NewerExportReplacesTheSavedWorkspaceInOneTap() throws {
        let inputs = try inputs()
        let environment = ProcessInfo.processInfo.environment
        guard let path = environment["FT_UI_REIMPORT_PATH"], let code = environment["FT_UI_REIMPORT_CODE"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_UI_REIMPORT_{PATH,CODE} to a newer export of the same workspace")
        }
        let app = launch()
        _ = reopen(app, inputs)
        // Opened from Files or AirDrop over the live terminal: an offline
        // restore ends its sessions first, in one tap that says what it ends.
        func disconnectForRestore() {
            XCUIDevice.shared.system.open(URL(fileURLWithPath: path))
            let disconnect = app.buttons["Disconnect Sessions and Restore"]
            XCTAssertTrue(disconnect.waitForExistence(timeout: 10), "live sessions pause the restore until disconnected")
            XCTAssertTrue(app.staticTexts["Ends the live sessions in “\(inputs.workspace)”."].exists,
                          "one line names the sessions the button ends")
            disconnect.tap()
        }
        disconnectForRestore()
        let field = app.secureTextFields["Transfer code"]
        XCTAssertTrue(field.waitForExistence(timeout: 20), "the newer package must reach the unlock step")
        // Closing restore instead brings the terminal back, connected.
        app.navigationBars["Restore"].buttons["Close"].tap()
        XCTAssertTrue(chip(app, inputs.machine, "connected").waitForExistence(timeout: 60),
                      "the terminal a cancelled restore closed reconnects")
        snapshot("r0-restore-cancelled")

        disconnectForRestore()
        XCTAssertTrue(field.waitForExistence(timeout: 20), "the newer package must reach the unlock step")
        field.tap(); field.typeText(code)
        app.buttons["Unlock"].tap()

        let replace = app.buttons["Replace"]
        XCTAssertTrue(replace.waitForExistence(timeout: 30), "a newer export of a saved workspace must reach its review")
        let summary = app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH %@",
                                                           "Replaces your saved “\(inputs.workspace)”")).firstMatch
        XCTAssertTrue(summary.exists, "one line says which saved workspace this replaces")
        XCTAssertTrue(summary.label.contains("connections or host keys"), "and that connection settings change: \(summary.label)")
        XCTAssertEqual(app.switches.count, 0, "replacing asks nothing beyond Replace")
        XCTAssertTrue(replace.isEnabled, "Replace is the approval")
        snapshot("r1-replacement-review")
        replace.tap()
        let done = app.buttons["Done"]
        XCTAssertTrue(done.waitForExistence(timeout: 30), "the replacement must publish")
        snapshot("r2-replaced")
        done.tap()

        // The terminal the restore closed opens again on the replaced workspace.
        XCTAssertTrue(chip(app, inputs.machine, "connected").waitForExistence(timeout: 60),
                      "the terminal left showing reconnects after the replacement")
        let terminal = app.descendants(matching: .any)["Terminal output"].firstMatch
        snapshot("r3-reconnected")
        app.typeText("echo AFTER-$((40+2))\n")
        waitFor("AFTER-42", in: terminal, timeout: 30, "the replaced workspace's machine must run commands")

        let list = app.buttons["Add machines"]
        for _ in 0..<2 where !list.exists {
            app.navigationBars.buttons.element(boundBy: 0).tap()
            _ = list.waitForExistence(timeout: 5)
        }
        XCTAssertTrue(savedWorkspace(app, inputs).waitForExistence(timeout: 20), "the replaced workspace must be listed")
        XCTAssertEqual(app.buttons.matching(NSPredicate(format: "label CONTAINS %@", inputs.workspace)).count, 1,
                       "the saved workspace is replaced, not added")
    }
}
