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
        let field = app.secureTextFields["Transfer code"]
        XCTAssertTrue(field.waitForExistence(timeout: 20), "the opened package must reach the transfer-code step")
        snapshot("1-package-collected")
        field.tap(); field.typeText(inputs.code)
        app.buttons["Decrypt and Review"].tap()

        let approve = app.switches["I verified the comparison value on my Mac and approve these settings, trust records, and keys."]
        XCTAssertTrue(approve.waitForExistence(timeout: 20), "decryption must reach the review step")
        snapshot("2-review")
        if approve.value as? String != "1" { approve.switches.firstMatch.exists ? approve.switches.firstMatch.tap() : approve.tap() }
        app.buttons["Restore Offline"].tap()

        let done = app.buttons["Done"]
        XCTAssertTrue(done.waitForExistence(timeout: 30), "the restore must publish")
        snapshot("3-restored")
        done.tap()
    }

    /// Opens the restored machine and connects. `reviewed` reports whether an
    /// earlier session's unknown-outcome review had to be acknowledged first.
    @MainActor
    private func connect(_ app: XCUIApplication, _ inputs: Inputs) -> (terminal: XCUIElement, reviewed: Bool) {
        let saved = savedWorkspace(app, inputs)
        XCTAssertTrue(saved.waitForExistence(timeout: 20), "the restored workspace must be listed")
        saved.tap()
        let connect = app.buttons["Connect"].firstMatch
        XCTAssertTrue(connect.waitForExistence(timeout: 20), "a ready direct SSH route must offer Connect")
        snapshot("4-machine")
        connect.tap()
        app.tap() // lets the interruption monitor handle a first-use Face ID alert

        let terminal = app.descendants(matching: .any)["Terminal output"].firstMatch
        let review = app.buttons["Start New Session"]
        var reviewed = false
        let deadline = Date().addingTimeInterval(60)
        while !terminal.exists, Date() < deadline {
            if !reviewed, review.exists {
                XCTAssertTrue(app.staticTexts["Earlier session on this machine"].exists)
                snapshot("4b-earlier-session-review")
                review.tap(); app.tap(); reviewed = true
            }
            usleep(250_000)
        }
        XCTAssertTrue(terminal.exists, "the terminal surface must appear")
        XCTAssertTrue(app.navigationBars[inputs.machine].waitForExistence(timeout: 5))
        snapshot("5-terminal")
        return (terminal, reviewed)
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

    /// The last `SZ-<rows>x<columns>` the shell printed.
    private func reportedSize(_ terminal: XCUIElement) -> (rows: Int, columns: Int)? {
        let text = screen(terminal)
        guard let match = text.matches(of: /SZ-(\d+)x(\d+)/).last,
              let rows = Int(match.1), let columns = Int(match.2) else { return nil }
        return (rows, columns)
    }

    @MainActor
    func testRestoreEncryptedWorkspaceAndConnectToItsMachine() throws {
        let inputs = try inputs()
        let app = launch()
        restore(app, inputs)
        let (terminal, reviewed) = connect(app, inputs)
        XCTAssertFalse(reviewed, "a freshly restored machine has no earlier session to review")

        // Keystrokes leave through SSH; only the remote shell can turn the
        // arithmetic into 42, and the result is read from the admitted frame.
        app.typeText("echo FRANKEN-$((6*7))\n")
        waitFor("FRANKEN-42", in: terminal, timeout: 30, "a typed command must run in the remote shell")
        snapshot("6-command-output")
    }

    @MainActor
    func testTerminalKeysFullScreenRotationAndBackground() throws {
        let inputs = try inputs()
        let app = launch()
        let reused = savedWorkspace(app, inputs).waitForExistence(timeout: 5)
        if !reused { restore(app, inputs) }
        let (terminal, reviewed) = connect(app, inputs)
        // The journey test's session ended when its app process was killed.
        if reused { XCTAssertTrue(reviewed, "a machine used by an earlier launch must show the unknown-outcome review") }
        app.typeText("echo READY-$((40+2))\n")
        guard waitFor("READY-42", in: terminal, timeout: 30, "the shell must accept input") else { return }

        // Ctrl from the accessory row: without a delivered ^C the sleep keeps
        // the typed-ahead echo waiting for 30 s, well past this wait.
        app.typeText("sleep 30\n")
        app.buttons["Control modifier"].tap()
        app.typeText("c")
        app.typeText("echo CTRL-$((40+2))\n")
        waitFor("CTRL-42", in: terminal, timeout: 10, "Ctrl+C from the accessory row must interrupt the shell")

        // Left arrow edits the line: 402 becomes 40+2 only if the cursor moved.
        app.typeText("echo ARROW-$((402))")
        for _ in 0..<3 { app.buttons["Left arrow"].tap() }
        app.typeText("+\n")
        waitFor("ARROW-42", in: terminal, "the accessory Left arrow must move the line editor's cursor")

        // Tab completes the only /usr entry starting with "shar"; without it cd fails.
        app.typeText("cd /usr/shar")
        app.buttons["Tab"].tap()
        app.typeText(" && echo TAB-$(basename $PWD) && cd\n")
        waitFor("TAB-share", in: terminal, "the accessory Tab must reach shell completion")

        // Non-ASCII text arrives as UTF-8 and is drawn back.
        app.typeText("printf '%s-%s\\n' café 日本\n")
        waitFor("café-日本", in: terminal, "typed Unicode must round-trip through the shell")

        // A full-screen program on the alternate screen, left with Esc: if Esc
        // is lost, ":q!" is inserted as text and vim never exits.
        app.typeText("vim -u NONE -N\n")
        waitFor("\n~\n~\n~", in: terminal, "vim must draw its empty buffer")
        snapshot("7-vim")
        app.typeText("iFT-INSERT")
        app.buttons["Escape"].tap()
        app.typeText(":q!\n")
        app.typeText("echo ESC-$((40+2))\n")
        waitFor("ESC-42", in: terminal, "Esc must leave insert mode so :q! exits vim")
        XCTAssertFalse(screen(terminal).contains("FT-INSERT"), "leaving vim must restore the primary screen")

        // Rotation changes the remote PTY: landscape has more columns, fewer rows.
        app.typeText("clear; echo SZ-$(stty size | tr ' ' x)\n")
        XCTAssertTrue(waitFor("SZ-", in: terminal, "the shell must report its size"))
        sleep(1)
        let portrait = try XCTUnwrap(reportedSize(terminal), screen(terminal))
        XCUIDevice.shared.orientation = .landscapeLeft
        sleep(2)
        app.typeText("clear; echo SZ-$(stty size | tr ' ' x)\n")
        sleep(2)
        let landscape = try XCTUnwrap(reportedSize(terminal), screen(terminal))
        snapshot("8-landscape")
        XCTAssertGreaterThan(landscape.columns, portrait.columns, "landscape must widen the PTY")
        XCTAssertLessThan(landscape.rows, portrait.rows, "landscape must shorten the PTY")
        XCUIDevice.shared.orientation = .portrait

        // Background seals the connection; on return the screen must say so,
        // state the unknown outcome, and offer a new session that works.
        XCUIDevice.shared.press(.home)
        sleep(3)
        app.activate()
        let again = app.buttons["Start New Session"]
        XCTAssertTrue(again.waitForExistence(timeout: 15), "a session closed by backgrounding must offer a new one")
        XCTAssertTrue(app.staticTexts.containing(NSPredicate(format: "label CONTAINS %@", "may or may not have run")).firstMatch.exists)
        snapshot("9-after-background")
        again.tap()
        app.tap()
        // The ended view stays visible until its successor replaces it; the
        // keyboard returns only when the new session's view takes focus.
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 60), "the new session must take keyboard focus")
        let fresh = app.descendants(matching: .any)["Terminal output"].firstMatch
        app.typeText("echo AGAIN-$((40+2))\n")
        waitFor("AGAIN-42", in: fresh, timeout: 30, "the reconnected session must run commands")
        snapshot("10-reconnected")
    }
}
