import XCTest
@testable import FrankenTermKit

final class TerminalSplitLayoutTests: XCTestCase {
    func testFocusTransfersKeepTheSameTwoSlotsAndNavigationReplacesOnlyTheFocusedSlot() throws {
        let inventory = ["alpha", "beta", "gamma", "delta"]
        var split = TerminalSplitLayout()
        try split.open(first: "alpha", second: "beta", available: inventory)
        split.reconcile(previous: "alpha", selected: "beta", available: inventory)
        XCTAssertEqual(split.machineIDs, ["alpha", "beta"])
        split.reconcile(previous: "beta", selected: "gamma", available: inventory)
        XCTAssertEqual(split.machineIDs, ["alpha", "gamma"])
        split.reconcile(previous: "gamma", selected: "alpha", available: inventory)
        split.reconcile(previous: "alpha", selected: "delta", available: inventory)
        XCTAssertEqual(split.machineIDs, ["delta", "gamma"])
        var previous = "delta"
        for selected in (0..<1000).map({ inventory[$0 % inventory.count] }) {
            split.reconcile(previous: previous, selected: selected, available: inventory)
            XCTAssertEqual(split.machineIDs.count, 2)
            XCTAssertEqual(Set(split.machineIDs).count, 2, "one UIView/session must never occupy both slots")
            XCTAssertTrue(split.machineIDs.contains(selected))
            previous = selected
        }
    }

    func testNarrowWindowsPreservePairButPresentOnlyTheCurrentFocus() throws {
        var split = TerminalSplitLayout()
        try split.open(first: "alpha", second: "beta", available: ["alpha", "beta"])
        for width in [0.0, 400.0, 719.0, Double.infinity, Double.nan] {
            XCTAssertFalse(TerminalSplitLayout.supports(width: width, isPad: true))
        }
        XCTAssertFalse(TerminalSplitLayout.supports(width: 1024, isPad: false))
        XCTAssertTrue(TerminalSplitLayout.supports(width: 720, isPad: true))
        XCTAssertEqual(split.visibleMachineIDs(selected: "beta", expanded: false), ["beta"])
        XCTAssertEqual(split.visibleMachineIDs(selected: "alpha", expanded: false), ["alpha"])
        XCTAssertEqual(split.visibleMachineIDs(selected: "beta", expanded: true), ["alpha", "beta"])
        XCTAssertNil(split.visibleMachineIDs(selected: nil, expanded: true).first)
        XCTAssertEqual(split.machineIDs, ["alpha", "beta"])
    }

    func testInvalidSelectionAndGroupRemovalCannotRetargetACompanion() throws {
        var split = TerminalSplitLayout()
        try split.open(first: "alpha", second: "beta", available: ["alpha", "beta", "gamma"])
        XCTAssertThrowsError(try split.open(first: "alpha", second: "alpha", available: ["alpha"]))
        XCTAssertThrowsError(try split.open(first: "alpha", second: "missing", available: ["alpha"]))
        XCTAssertEqual(split.machineIDs, ["alpha", "beta"])
        split.reconcile(previous: "alpha", selected: "gamma", available: ["alpha", "gamma"])
        XCTAssertFalse(split.isSplit)
        XCTAssertEqual(split.visibleMachineIDs(selected: "gamma", expanded: true), ["gamma"])
    }
}
