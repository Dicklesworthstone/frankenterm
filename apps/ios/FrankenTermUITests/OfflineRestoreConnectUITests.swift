import XCTest

// The M0 user journey through the real app UI: open an encrypted .ftairgap
// workspace, enter its transfer code, review, restore into the protected
// Keychain, open the machine, Connect over direct SSH and run a command.
// Signing uses the production user-presence signer, so the operator must enroll
// and match simulated Face ID from the host while the test runs (see the runner
// notes in apps/apple/README.md). Inputs via xcodebuild's TEST_RUNNER_ prefix:
//   FT_UI_AIRGAP_PATH (host path of the .ftairgap), FT_UI_AIRGAP_CODE,
//   FT_UI_WORKSPACE_NAME, FT_UI_MACHINE_NAME. Skipped when absent.
final class OfflineRestoreConnectUITests: XCTestCase {
    private func snapshot(_ name: String) {
        let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        attachment.name = name; attachment.lifetime = .keepAlways
        add(attachment)
    }

    @MainActor
    func testRestoreEncryptedWorkspaceAndConnectToItsMachine() throws {
        let environment = ProcessInfo.processInfo.environment
        guard let path = environment["FT_UI_AIRGAP_PATH"], let code = environment["FT_UI_AIRGAP_CODE"],
              let workspace = environment["FT_UI_WORKSPACE_NAME"], let machine = environment["FT_UI_MACHINE_NAME"] else {
            throw XCTSkip("Set TEST_RUNNER_FT_UI_{AIRGAP_PATH,AIRGAP_CODE,WORKSPACE_NAME,MACHINE_NAME}")
        }
        addUIInterruptionMonitor(withDescription: "Face ID permission") { alert in
            for label in ["OK", "Allow"] where alert.buttons[label].exists { alert.buttons[label].tap(); return true }
            return false
        }
        let app = XCUIApplication()
        app.launch()
        XCUIDevice.shared.system.open(URL(fileURLWithPath: path))

        let field = app.secureTextFields["Transfer code"]
        XCTAssertTrue(field.waitForExistence(timeout: 20), "the opened package must reach the transfer-code step")
        snapshot("1-package-collected")
        field.tap(); field.typeText(code)
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

        let saved = app.buttons.containing(NSPredicate(format: "label CONTAINS %@", workspace)).firstMatch
        XCTAssertTrue(saved.waitForExistence(timeout: 20), "the restored workspace must be listed")
        saved.tap()
        let connect = app.buttons["Connect"].firstMatch
        XCTAssertTrue(connect.waitForExistence(timeout: 20), "a ready direct SSH route must offer Connect")
        snapshot("4-machine")
        connect.tap()
        app.tap() // lets the interruption monitor handle a first-use Face ID alert

        let terminal = app.descendants(matching: .any)["Terminal output"].firstMatch
        XCTAssertTrue(terminal.waitForExistence(timeout: 60), "the terminal surface must appear")
        XCTAssertTrue(app.navigationBars[machine].waitForExistence(timeout: 5))
        snapshot("5-terminal")

        // Keystrokes leave through SSH; only the remote shell can turn the
        // arithmetic into 42, and the result is read from the admitted frame.
        app.typeText("echo FRANKEN-$((6*7))\n")
        let evaluated = expectation(for: NSPredicate(format: "value CONTAINS %@", "FRANKEN-42"),
                                    evaluatedWith: terminal)
        let outcome = XCTWaiter().wait(for: [evaluated], timeout: 30)
        snapshot("6-command-output")
        XCTAssertEqual(outcome, .completed, "screen: \(terminal.value as? String ?? "<no text>")")
    }
}
