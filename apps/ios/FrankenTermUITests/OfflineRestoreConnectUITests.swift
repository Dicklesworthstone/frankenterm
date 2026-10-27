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

    /// The `SZ-<tag>-<rows>x<columns>` the shell printed for `tag` (the
    /// echoed command never matches).
    private func reportedSize(_ terminal: XCUIElement, tag: Substring) -> (rows: Int, columns: Int)? {
        let text = screen(terminal)
        guard let start = text.range(of: "SZ-\(tag)-", options: .backwards)?.upperBound,
              let match = text[start...].prefixMatch(of: /(\d+)x(\d+)/),
              let rows = Int(match.1), let columns = Int(match.2) else { return nil }
        return (rows, columns)
    }

    /// Asks the shell for its size and returns its answer. Each question has
    /// its own tag, so an earlier answer still on screen never counts; while
    /// the answer is still `previous`, it asks again, three times at most.
    private func shellSize(_ app: XCUIApplication, _ terminal: XCUIElement,
                           unlike previous: (rows: Int, columns: Int)? = nil) -> (rows: Int, columns: Int)? {
        var size: (rows: Int, columns: Int)?
        for _ in 0..<3 {
            let tag = UUID().uuidString.prefix(6)
            app.typeText("clear; echo SZ-\(tag)-$(stty size | tr ' ' x)\n")
            for _ in 0..<5 {
                size = reportedSize(terminal, tag: tag)
                if size != nil { break }
                sleep(1)
            }
            if let size, previous.map({ $0 != size }) ?? true { return size }
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

        // A held Left arrow repeats: only a repeat walks the cursor back over
        // the whole line, so the missing "ec" lands at its start.
        app.typeText("ho HOLD-$((40+2))")
        app.buttons["terminal.key.Left arrow"].press(forDuration: 3)
        app.typeText("ec\n")
        waitFor("HOLD-42", in: terminal, "a held accessory Left arrow must repeat to the line's start")

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

        // A tapped link shows where it goes and opens only on a second tap:
        // Cancel leaves typing as it was; Open hands the address to Safari,
        // and the session is still there on return. The link is the top
        // row's first 15 columns; the shell's size places the tap.
        let size = try XCTUnwrap(shellSize(app, terminal), "the shell must report its size; screen:\n\(screen(terminal))")
        app.typeText(#"clear; printf '\033]8;;https://example.com/ft-ui/%d\033\\LINK-%d-LINK-%d\033]8;;\033\\\n' $((6*7)) $((6*7)) $((6*7))"# + "\n")
        waitFor("LINK-42-LINK-42", in: terminal, "the shell must print the link")
        let link = terminal.coordinate(withNormalizedOffset: CGVector(dx: 4.5 / Double(size.columns), dy: 0.5 / Double(size.rows)))
        link.tap()
        let destination = app.staticTexts["https://example.com/ft-ui/42"]
        XCTAssertTrue(destination.waitForExistence(timeout: 10), "a tap on the link must show where it goes")
        XCTAssertTrue(app.buttons["Copy Link"].exists)
        snapshot("11-link")
        // The choices show in a popover by the link (iPhone too, on iOS 26),
        // put away by a tap outside it; a sheet would have Cancel.
        if app.buttons["Cancel"].exists { app.buttons["Cancel"].tap() } else { app.otherElements["PopoverDismissRegion"].tap() }
        XCTAssertTrue(destination.waitForNonExistence(timeout: 5), "Cancel puts the choices away")
        app.typeText("echo AFTER-$((40+2))\n")
        waitFor("AFTER-42", in: terminal, "typing goes on after the choices are put away")

        link.tap()
        let open = app.buttons["Open"].firstMatch
        XCTAssertTrue(open.waitForExistence(timeout: 10), "a web address offers Open")
        open.tap()
        let safari = XCUIApplication(bundleIdentifier: "com.apple.mobilesafari")
        XCTAssertTrue(safari.wait(for: .runningForeground, timeout: 20), "Open must hand the address to Safari")
        snapshot("12-opened")
        app.activate()
        terminal.tap()
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 30), "the session takes the keyboard again")
        app.typeText("echo OPENED-$FT_KEPT\n")
        waitFor("OPENED-yes", in: terminal, timeout: 30, "the same session runs commands after Safari")
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
        // The fleet overview's New output filter lists the marked machine alone.
        app.buttons["terminal.fleet.open"].tap()
        XCTAssertTrue(app.navigationBars["Fleet Overview"].waitForExistence(timeout: 10), "the fleet overview opens")
        app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Status at refresh")).firstMatch.tap()
        app.buttons["New output"].firstMatch.tap()
        func overviewRow(_ machine: String) -> XCUIElement {
            app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@ AND label CONTAINS %@", machine, "New output")).firstMatch
        }
        XCTAssertTrue(overviewRow(inputs.machine).waitForExistence(timeout: 10), "the overview marks the machine that printed")
        XCTAssertFalse(overviewRow(second).exists, "the machine on screen is not listed as new output")
        snapshot("15a-overview-new-output")
        app.navigationBars["Fleet Overview"].buttons["Done"].tap()
        XCTAssertTrue(app.navigationBars["Fleet Overview"].waitForNonExistence(timeout: 10), "the overview closes")
        // The strip then ends in Next New Output, which goes to it.
        let nextNew = app.buttons["terminal.unseen.next"]
        XCTAssertTrue(nextNew.waitForExistence(timeout: 5), "Next New Output is offered while a machine is marked")
        nextNew.tap()
        waitFor("AWAY-first-10", in: screen, timeout: 10, "Next New Output leads to the new output")
        XCTAssertTrue(chip(app, inputs.machine, "connected").waitForExistence(timeout: 5), "being on screen clears the mark")
        XCTAssertTrue(nextNew.waitForNonExistence(timeout: 5), "with nothing unseen, Next New Output goes away")
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 10), "the returned machine gets the keyboard")

        // ⌘J does the same from a hardware keyboard, here to the second machine.
        app.typeKey("]", modifierFlags: [.command, .shift])
        waitFor("WHO-second-4", in: screen, timeout: 10, "⇧⌘] shows the second machine")
        app.typeText("sleep 6; echo LATER-$FT_WHO-$((6+6))\n")
        waitFor("sleep 6; echo LATER", in: screen, timeout: 10, "the command is running before the switch")
        app.typeKey("[", modifierFlags: [.command, .shift])
        waitFor("AWAY-first-10", in: screen, timeout: 10, "⇧⌘[ shows the first machine")
        XCTAssertTrue(chip(app, second, "connected, new output").waitForExistence(timeout: 20),
                      "the second machine printed while away and is marked")
        app.typeKey("j", modifierFlags: .command)
        waitFor("LATER-second-12", in: screen, timeout: 10, "⌘J leads to the new output")
        XCTAssertTrue(chip(app, second, "connected").waitForExistence(timeout: 5), "being on screen clears the mark")
        app.typeKey("j", modifierFlags: .command)
        never("AWAY-first-10", in: screen, "with nothing unseen, ⌘J stays put")
        snapshot("15b-next-new-output")
        app.typeText("echo STAY-$FT_WHO-$((7+7))\n")
        waitFor("STAY-second-14", in: screen, timeout: 30, "typing after ⌘J reaches the machine it showed")
        // Later tests relaunch into the first machine. (Its screen may have
        // scrolled: hardware keys hide the keyboard, and its shell traps the
        // resize, so a fresh line proves the switch.)
        chip(app, inputs.machine, "connected").tap()
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 10), "the returned machine gets the keyboard")
        app.typeText("echo BACK-$FT_WHO-$((8+8))\n")
        waitFor("BACK-first-16", in: screen, timeout: 30, "the strip shows the first machine again")
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

    /// A machine switched off Connect with Group on this phone stays out of
    /// its group's background sign-in, also after a relaunch, and still
    /// connects when asked. Use Workspace Default puts it back in the group.
    /// Needs the second machine. Starts and ends on the workspace list, as the
    /// previous test leaves it and the next expects.
    @MainActor
    func test5AMachineOffConnectWithGroupConnectsOnlyWhenAsked() throws {
        let inputs = try inputs()
        guard let second = ProcessInfo.processInfo.environment["FT_UI_SECOND_MACHINE_NAME"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_UI_SECOND_MACHINE_NAME for a package with a second machine")
        }
        var app = launch()
        // No terminal was reopened, so no group has signed anything in yet.
        // (A terminal's back button also carries the workspace's name.)
        XCTAssertTrue(app.buttons["Add machines"].waitForExistence(timeout: 20),
                      "the previous test leaves the workspace list on screen")
        savedWorkspace(app, inputs).tap()
        // The machine pager keeps its neighbors' pages in the tree; the one in
        // view is the hittable one.
        func onPage(_ query: XCUIElementQuery) -> XCUIElement? {
            (0..<query.count).map { query.element(boundBy: $0) }.first { $0.isHittable }
        }
        func show(_ machine: String) {
            app.buttons["Machines"].tap()
            let item = app.buttons[machine].firstMatch
            XCTAssertTrue(item.waitForExistence(timeout: 10), "the Machines menu lists \(machine)")
            item.tap()
            let title = app.staticTexts.matching(NSPredicate(format: "label == %@", machine))
            for _ in 0..<20 where onPage(title) == nil { usleep(250_000) }
            XCTAssertNotNil(onPage(title), "\(machine)'s page is in view")
        }
        func groupSwitch() -> XCUIElement? {
            let switches = app.switches.matching(NSPredicate(format: "label == %@", "Connect with Group"))
            for _ in 0..<3 where onPage(switches) == nil { app.swipeUp() }
            return onPage(switches)
        }
        func warmed() -> XCUIElement {
            app.buttons.matching(NSPredicate(format: "label IN %@",
                ["\(second), signed in, terminal not open", "\(second), connected", "\(second), connecting"])).firstMatch
        }
        func reset() -> XCUIElement { app.buttons["Use Workspace Default (On)"] }
        func openTerminal() {
            let open = app.buttons.matching(NSPredicate(format: "label IN %@", ["Connect", "Open Terminal"]))
            for _ in 0..<3 where onPage(open) == nil { app.swipeDown() }
            onPage(open)?.tap()
            let allow = XCUIApplication(bundleIdentifier: "com.apple.springboard").alerts.buttons["OK"]
            if allow.waitForExistence(timeout: 3) { allow.tap() }
            XCTAssertTrue(chip(app, inputs.machine, "connected").waitForExistence(timeout: 60), "the first machine connects")
        }

        show(second)
        let off = try XCTUnwrap(groupSwitch(), "the machine page offers Connect with Group")
        XCTAssertEqual(off.value as? String, "1", "the package's group connects this machine")
        off.coordinate(withNormalizedOffset: CGVector(dx: 0.95, dy: 0.5)).tap()
        XCTAssertTrue(reset().waitForExistence(timeout: 10), "the choice is saved as this phone's own")
        XCTAssertEqual(groupSwitch()?.value as? String, "0")
        snapshot("g1-switched-off")

        // The group opens without it, and the strip says it is not connected.
        show(inputs.machine)
        openTerminal()
        XCTAssertTrue(chip(app, second, "disconnected").waitForExistence(timeout: 10), "the strip lists it disconnected")
        XCTAssertFalse(warmed().waitForExistence(timeout: 20), "a machine switched off stays out of the group's sign-in")
        snapshot("g2-group-without-it")
        // Asked, it connects.
        chip(app, second, "disconnected").tap()
        let connect = app.buttons["Connect"].firstMatch
        XCTAssertTrue(connect.waitForExistence(timeout: 10), "a disconnected machine offers Connect")
        connect.tap()
        XCTAssertTrue(chip(app, second, "connected").waitForExistence(timeout: 60), "Connect still connects it")
        let screen = app.descendants(matching: .any)["Terminal output"].firstMatch
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 30), "its terminal takes the keyboard")
        app.typeText("echo ASKED-$((20+1))\n")
        waitFor("ASKED-21", in: screen, timeout: 30, "the machine connected on request runs commands")
        chip(app, inputs.machine, "connected").tap()
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 10), "the first machine gets the keyboard back")
        app.typeText("echo HOME-$((30+3))\n")
        waitFor("HOME-33", in: screen, timeout: 30, "the first machine is on screen before the relaunch")

        // The choice holds across a relaunch: the group comes back without it.
        app = launch()
        _ = reopen(app, inputs)
        XCTAssertTrue(chip(app, second, "disconnected").waitForExistence(timeout: 10), "after a relaunch it waits again")
        XCTAssertFalse(warmed().waitForExistence(timeout: 20), "after a relaunch it stays out of the group's sign-in")
        snapshot("g3-relaunched")

        // Use Workspace Default puts it back; the open group signs it in.
        app.navigationBars.buttons.element(boundBy: 0).tap()
        XCTAssertTrue(app.buttons["Machines"].waitForExistence(timeout: 10), "back on the machine pages")
        show(second)
        XCTAssertTrue(reset().waitForExistence(timeout: 10), "the saved choice offers the workspace default")
        reset().tap()
        XCTAssertTrue(reset().waitForNonExistence(timeout: 10), "the phone's own choice is gone")
        XCTAssertEqual(groupSwitch()?.value as? String, "1", "the workspace default is on")
        show(inputs.machine)
        openTerminal()
        XCTAssertTrue(warmed().waitForExistence(timeout: 60), "back in the group, it signs in in the background")
        snapshot("g4-default-again")

        let list = app.buttons["Add machines"]
        for _ in 0..<3 where !list.exists {
            app.navigationBars.buttons.element(boundBy: 0).tap()
            _ = list.waitForExistence(timeout: 5)
        }
        XCTAssertTrue(savedWorkspace(app, inputs).waitForExistence(timeout: 20), "the next test starts on the workspace list")
    }

    /// Stop trusting the server's host key from Keys while connected: the
    /// session ends, the key shows as blocked, and the machine no longer
    /// offers Connect. Runs last: the block stays saved in this simulator.
    @MainActor
    func test6BlockingTheHostKeyEndsTheSessionAndKeepsItBlocked() throws {
        let inputs = try inputs()
        let app = launch()
        // The previous test leaves the workspace list on screen.
        let terminal = connect(app, inputs)
        app.typeText("echo BLOCK-$((40+2))\n")
        waitFor("BLOCK-42", in: terminal, timeout: 30, "the session runs before its host key is blocked")

        let keys = app.buttons["Keys"]
        for _ in 0..<2 where !keys.exists {
            app.navigationBars.buttons.element(boundBy: 0).tap()
            _ = keys.waitForExistence(timeout: 5)
        }
        XCTAssertTrue(keys.exists, "the machine page offers Keys")
        keys.tap()
        let block = app.buttons["Stop Trusting on This Phone"].firstMatch
        for _ in 0..<4 where !block.isHittable { app.swipeUp() }
        XCTAssertTrue(block.waitForExistence(timeout: 10), "Keys lists the host's trusted key")
        // The replacement gives each route its own trust record for the one
        // loopback host and key; Keys shows that host and key once.
        XCTAssertEqual(app.buttons.matching(NSPredicate(format: "label == %@", "Stop Trusting on This Phone")).count, 1,
                       "one row per host and port, each key once")
        snapshot("h1-host-keys")
        // Every trusted host key the workspace lists is blocked in turn.
        var blocked = 0
        while block.exists, blocked < 4 {
            for _ in 0..<4 where !block.isHittable { app.swipeUp() }
            block.tap()
            let confirm = app.buttons["Stop Trusting and Disconnect"]
            XCTAssertTrue(confirm.waitForExistence(timeout: 10), "blocking asks once, naming what disconnects")
            if blocked == 0 { snapshot("h2-confirm") }
            confirm.tap()
            let saved = app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH %@",
                                                             "This host key is blocked on this phone")).firstMatch
            XCTAssertTrue(saved.waitForExistence(timeout: 30), "the block is saved and reported")
            blocked += 1
            _ = app.staticTexts["Blocked on this phone"].waitForExistence(timeout: 10)
            sleep(1)
        }
        XCTAssertFalse(block.exists, "no host key is still trusted")
        XCTAssertTrue(app.staticTexts["Blocked on this phone"].exists, "the key shows as blocked")
        snapshot("h3-blocked")
        app.navigationBars["SSH Keys"].buttons["Done"].tap()

        let unready = app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH %@",
                                                           "No route to this machine is ready")).firstMatch
        XCTAssertTrue(unready.waitForExistence(timeout: 10), "the machine page says no route is ready")
        XCTAssertFalse(app.buttons["Connect"].exists, "the machine no longer offers Connect")
        XCTAssertFalse(chip(app, inputs.machine, "connected").exists, "the session to the blocked host ended")
        app.buttons["Routes"].firstMatch.tap()
        let why = app.staticTexts["Every host key this route trusts is revoked or blocked on this phone."].firstMatch
        XCTAssertTrue(why.waitForExistence(timeout: 10), "the route says why it cannot connect")
        snapshot("h4-machine-unready")
    }
}
