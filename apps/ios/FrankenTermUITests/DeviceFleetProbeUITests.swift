import XCTest

// A read-only check, on a real iPhone, that every saved machine connects.
// It attaches to the FrankenTerm already installed there (activate(), never
// launch(): no launch arguments, nothing reset), reads each machine chip's
// accessibility label ("<name>, <status>") until all say connected, and
// reports how each one got there. It never taps a terminal or types: on a
// real phone the panes are the operator's. Inputs via xcodebuild's
// TEST_RUNNER_ prefix: FT_DEVICE_PROBE=1 to run, FT_DEVICE_PROBE_WAIT seconds
// (default 120). Skipped otherwise, so simulator runs of the suite pass it by.
// apps/apple/README.md says how to run it without reinstalling the app.
final class DeviceFleetProbeUITests: XCTestCase {
    /// MachineStrip.describe's words. Each is matched after ", ", so
    /// "disconnected" never reads as "connected".
    private static let statuses = [
        "connected", "signed in, terminal not open", "connecting", "needs attention",
        "disconnected", "not available on this device",
    ]

    /// Every machine chip on screen or scrolled off it, by name, from one
    /// snapshot of the accessibility tree.
    @MainActor
    private func chips(_ app: XCUIApplication) -> [String: String] {
        guard let root = try? app.snapshot() else { return [:] }
        var found: [String: String] = [:]
        var pending: [any XCUIElementSnapshot] = [root]
        while let node = pending.popLast() {
            pending.append(contentsOf: node.children)
            guard node.elementType == .button else { continue }
            var label = node.label
            if label.hasSuffix(", new output") { label.removeLast(", new output".count) }
            for status in Self.statuses where label.hasSuffix(", " + status) {
                found[String(label.dropLast(status.count + 2))] = status
            }
        }
        return found
    }

    @MainActor
    func testEverySavedMachineConnects() throws {
        let environment = ProcessInfo.processInfo.environment
        guard environment["FT_DEVICE_PROBE"] == "1" else {
            throw XCTSkip("Set TEST_RUNNER_FT_DEVICE_PROBE=1 to probe the FrankenTerm installed on a device")
        }
        let wait = environment["FT_DEVICE_PROBE_WAIT"].flatMap(TimeInterval.init) ?? 120
        let app = XCUIApplication(bundleIdentifier: "org.frankenterm.mobile")
        app.activate()
        let start = Date()
        var last: [String: String] = [:]
        var timeline: [String] = []
        repeat {
            let now = chips(app)
            if now != last {
                let states = now.sorted { $0.key < $1.key }.map { "\($0.key): \($0.value)" }
                timeline.append(String(format: "%6.1f s  ", Date().timeIntervalSince(start)) + states.joined(separator: "; "))
                last = now
            }
            if !now.isEmpty, now.values.allSatisfy({ $0 == "connected" }) { break }
            sleep(2)
        } while Date().timeIntervalSince(start) < wait

        for line in timeline { print("FT_DEVICE_PROBE", line) }
        let report = XCTAttachment(string: timeline.joined(separator: "\n"))
        report.name = "chip-timeline"; report.lifetime = .keepAlways
        add(report)
        let screen = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        screen.name = "fleet"; screen.lifetime = .keepAlways
        add(screen)

        XCTAssertFalse(last.isEmpty, "no machine strip: the app is not showing a workspace's terminal")
        let waiting = last.filter { $0.value != "connected" }.sorted { $0.key < $1.key }
        XCTAssertTrue(waiting.isEmpty, "not connected after \(Int(wait)) s: "
            + waiting.map { "\($0.key) (\($0.value))" }.joined(separator: ", "))
    }
}
