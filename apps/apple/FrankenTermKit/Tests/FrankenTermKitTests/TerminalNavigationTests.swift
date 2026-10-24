import Foundation
import XCTest
@testable import FrankenTermKit

final class TerminalNavigationTests: XCTestCase {
    private func pane(_ name: String = "pane", id: UUID = UUID(), incarnation: UUID = UUID()) throws -> TerminalNavigationPane {
        try .init(id: .init(id: id, incarnation: incarnation), title: name)
    }
    private func machine(_ id: String, _ panes: [TerminalNavigationPane] = []) throws -> TerminalNavigationMachine {
        try .init(id: id, name: id, panes: panes)
    }
    private func move(_ navigation: inout TerminalNavigation, _ axis: TerminalNavigationAxis,
                      _ direction: TerminalNavigationDirection = .next) throws {
        try navigation.commit(XCTUnwrap(navigation.plan(axis: axis, direction: direction)))
    }

    func testTwoRingsRememberEachMachineAndDoNotReorderOtherPanes() throws {
        let a = try [pane("a1"), pane("a2"), pane("a3")], b = try [pane("b1"), pane("b2")]
        let inventory = try [machine("A", a), machine("B", b), machine("offline")]
        var navigation = try TerminalNavigation(machines: inventory)
        try move(&navigation, .pane)
        XCTAssertEqual(navigation.selection?.paneID, a[1].id)
        try move(&navigation, .machine)
        XCTAssertEqual(navigation.selection?.paneID, b[0].id)
        try move(&navigation, .pane)
        try move(&navigation, .machine)
        XCTAssertEqual(navigation.selection, .init(machineID: "offline", paneID: nil))
        XCTAssertNil(try navigation.plan(axis: .pane, direction: .next))
        try move(&navigation, .machine)
        XCTAssertEqual(navigation.selection?.paneID, a[1].id)
        try move(&navigation, .machine)
        XCTAssertEqual(navigation.selection?.paneID, b[1].id)
        XCTAssertEqual(navigation.machines, inventory)
    }

    func testPreviewDoesNotChangeFocusAndCommitsExactlyOnce() throws {
        var navigation = try TerminalNavigation(machines: [machine("A", [pane()]), machine("B", [pane()])])
        let original = navigation.selection
        let plan = try XCTUnwrap(navigation.plan(axis: .machine, direction: .next))
        XCTAssertEqual(navigation.selection, original)
        XCTAssertEqual(plan.source, original)
        try navigation.validate(plan)
        XCTAssertEqual(try navigation.commit(plan), plan.destination)
        XCTAssertThrowsError(try navigation.commit(plan)) { XCTAssertEqual($0 as? TerminalNavigationError, .stale) }
    }

    func testRingWrapAndOneItemNoOp() throws {
        var navigation = try TerminalNavigation(machines: [machine("A"), machine("B"), machine("C")])
        let previous = try XCTUnwrap(navigation.plan(axis: .machine, direction: .previous))
        XCTAssertTrue(previous.wraps); XCTAssertEqual(previous.destination.machineID, "C")
        XCTAssertNil(try navigation.plan(axis: .machine, direction: .previous, wrap: false))
        try navigation.commit(previous)
        XCTAssertTrue(try XCTUnwrap(navigation.plan(axis: .machine, direction: .next)).wraps)
        let one = try TerminalNavigation(machines: [machine("only", [pane()])])
        XCTAssertNil(try one.plan(axis: .machine, direction: .next))
        XCTAssertNil(try one.plan(axis: .pane, direction: .previous))
        XCTAssertNil(try one.plan(machineID: "only"))
    }

    func testInventoryReorderCannotRetargetCapturedGesture() throws {
        let a = try machine("A", [pane()]), b = try machine("B", [pane()]), c = try machine("C", [pane()])
        var navigation = try TerminalNavigation(machines: [a, b, c])
        let plan = try XCTUnwrap(navigation.plan(axis: .machine, direction: .next))
        XCTAssertFalse(try navigation.replaceInventory([a, c, b]))
        XCTAssertThrowsError(try navigation.commit(plan))
        XCTAssertEqual(navigation.selection?.machineID, "A")
        try move(&navigation, .machine)
        XCTAssertEqual(navigation.selection?.machineID, "C")
    }

    func testClosedPaneAndReusedTabCannotInheritFocusOrViewport() throws {
        let tab = UUID(), old = try pane("same title", id: tab)
        let replacement = try pane("same title", id: tab), other = try pane()
        var navigation = try TerminalNavigation(machines: [machine("A", [old, other]), machine("B")])
        let original = try XCTUnwrap(navigation.viewWitness)
        try navigation.remember(.init(topRow: -90, fontPointSize: 18), for: original)
        let pending = try XCTUnwrap(navigation.plan(paneID: other.id))
        XCTAssertTrue(try navigation.replaceInventory([machine("A", [replacement, other]), machine("B")]))
        XCTAssertEqual(navigation.selection, .init(machineID: "A", paneID: nil))
        XCTAssertNil(navigation.viewport(for: old.id)); XCTAssertNil(navigation.viewport(for: replacement.id))
        XCTAssertThrowsError(try navigation.commit(pending))
        XCTAssertThrowsError(try navigation.remember(.init(), for: original))
        XCTAssertNil(try navigation.plan(axis: .pane, direction: .next))
        try navigation.commit(XCTUnwrap(navigation.plan(paneID: replacement.id)))
        XCTAssertEqual(navigation.selection?.paneID, replacement.id)
    }

    func testRemovedMachineDoesNotSelectOrConnectItsNeighbor() throws {
        var navigation = try TerminalNavigation(machines: [machine("A", [pane()]), machine("B", [pane()])])
        let b = navigation.machines[1]
        XCTAssertTrue(try navigation.replaceInventory([b]))
        XCTAssertNil(navigation.selection)
        XCTAssertNil(try navigation.plan(axis: .machine, direction: .next))
        try navigation.commit(XCTUnwrap(navigation.plan(machineID: "B")))
        XCTAssertEqual(navigation.selection?.machineID, "B")
    }

    func testEmptyMachineDoesNotAcquireNewPaneFromDiscovery() throws {
        var navigation = try TerminalNavigation(machines: [machine("A")])
        let created = try pane()
        XCTAssertFalse(try navigation.replaceInventory([machine("A", [created])]))
        XCTAssertNil(navigation.selection?.paneID)
        try navigation.commit(XCTUnwrap(navigation.plan(machineID: "A")))
        XCTAssertEqual(navigation.selection?.paneID, created.id)
    }

    func testIndependentScenesAndRetiredPlans() throws {
        let machines = try [machine("A", [pane(), pane()]), machine("B", [pane()])]
        var first = try TerminalNavigation(machines: machines), second = try TerminalNavigation(machines: machines)
        let plan = try XCTUnwrap(first.plan(axis: .machine, direction: .next))
        XCTAssertThrowsError(try second.commit(plan))
        try first.invalidatePlans()
        XCTAssertThrowsError(try first.commit(plan))
        try move(&first, .pane)
        XCTAssertEqual(second.selection?.paneID, machines[0].panes[0].id)
        try move(&second, .machine)
        XCTAssertEqual(first.selection?.machineID, "A")
    }

    func testDirectPickerCapturesExactPaneAcrossMachineRings() throws {
        let a = try [pane(), pane()], b = try [pane(), pane()]
        var navigation = try TerminalNavigation(machines: [machine("A", a), machine("B", b)])
        let plan = try XCTUnwrap(navigation.plan(paneID: b[1].id))
        XCTAssertEqual(plan.axis, .machine)
        try navigation.commit(plan)
        XCTAssertNil(try navigation.plan(paneID: b[1].id))
        XCTAssertEqual(try XCTUnwrap(navigation.plan(paneID: b[0].id)).axis, .pane)
        XCTAssertThrowsError(try navigation.plan(paneID: .init(id: UUID(), incarnation: UUID())))
        XCTAssertThrowsError(try navigation.plan(machineID: "not-in-group"))
    }

    func testViewportIsLocalExactSourcePreference() throws {
        let a = try pane(), b = try pane()
        var navigation = try TerminalNavigation(machines: [machine("A", [a, b])])
        let first = try XCTUnwrap(navigation.viewWitness)
        let view = try TerminalNavigationViewport(topRow: -400, fontPointSize: 21)
        try navigation.remember(view, for: first)
        try move(&navigation, .pane)
        XCTAssertThrowsError(try navigation.remember(.init(topRow: 0), for: first))
        XCTAssertEqual(navigation.viewport(for: a.id), view)
        XCTAssertNil(navigation.viewport(for: b.id))
        try move(&navigation, .pane)
        XCTAssertEqual(navigation.viewport(for: try XCTUnwrap(navigation.selection?.paneID)), view)
        XCTAssertThrowsError(try navigation.remember(.init(topRow: -1), for: first))
        let current = try XCTUnwrap(navigation.viewWitness)
        try navigation.remember(.init(topRow: -22), for: current)
        XCTAssertEqual(navigation.viewport(for: a.id)?.topRow, -22)
        try navigation.invalidatePlans()
        XCTAssertThrowsError(try navigation.remember(.init(), for: current))
        for size in [Double.nan, .infinity, -1, 0, 49] {
            XCTAssertThrowsError(try TerminalNavigationViewport(fontPointSize: size))
        }
        XCTAssertThrowsError(try TerminalNavigationViewport(topRow: .min))
    }

    func testInventoryValidationIsAtomicAndBounded() throws {
        let a = try pane(), machineA = try machine("A", [a])
        XCTAssertThrowsError(try TerminalNavigationMachine(id: "not a host", name: "name", panes: []))
        XCTAssertThrowsError(try machine("A", [a, a]))
        XCTAssertThrowsError(try TerminalNavigation(machines: [machineA, machineA]))
        XCTAssertThrowsError(try TerminalNavigation(machines: [machineA, machine("B", [a])]))
        XCTAssertThrowsError(try TerminalNavigation(machines: [machineA], initialMachineID: "B"))
        XCTAssertThrowsError(try TerminalNavigation(machines: (0..<129).map { try machine("m\($0)") }))
        XCTAssertThrowsError(try machine("A", (0..<257).map { _ in try pane() }))
        var navigation = try TerminalNavigation(machines: [machineA, machine("B")])
        let plan = try XCTUnwrap(navigation.plan(machineID: "B"))
        XCTAssertThrowsError(try navigation.replaceInventory([machineA, machineA]))
        try navigation.validate(plan)
        XCTAssertFalse(try navigation.replaceInventory(navigation.machines))
        try navigation.commit(plan)
        let empty = try TerminalNavigation(machines: [])
        XCTAssertNil(empty.selection)
    }

    func testFleetNavigationStressMatchesIndependentRingPositions() throws {
        let machines = try (0..<20).map { i in try machine("m\(i)", (0..<10).map { j in try pane("p\(j)") }) }
        var navigation = try TerminalNavigation(machines: machines)
        var machineIndex = 0, paneIndices = Array(repeating: 0, count: 20)
        for step in 0..<4000 {
            let forward = step % 7 != 0, axis: TerminalNavigationAxis = step % 3 == 0 ? .machine : .pane
            let old = navigation.selection
            let plan = try XCTUnwrap(navigation.plan(axis: axis, direction: forward ? .next : .previous))
            XCTAssertEqual(navigation.selection, old)
            if axis == .machine { machineIndex = (machineIndex + (forward ? 1 : 19)) % 20 }
            else { paneIndices[machineIndex] = (paneIndices[machineIndex] + (forward ? 1 : 9)) % 10 }
            try navigation.commit(plan)
            XCTAssertEqual(navigation.selection?.machineID, "m\(machineIndex)")
            XCTAssertEqual(navigation.selection?.paneID, machines[machineIndex].panes[paneIndices[machineIndex]].id)
        }
        XCTAssertEqual(navigation.machines, machines)
    }

    func testGestureDistinguishesOneAndTwoFingers() throws {
        for (touches, expected) in [(1, TerminalNavigationAxis.machine), (2, .pane)] {
            var gesture = try TerminalNavigationGesture(touches: touches)
            XCTAssertEqual(gesture.update(touches: touches, x: -60, y: 3, elapsed: 0.2), .dragging(expected))
            let result = try XCTUnwrap(gesture.finish(width: 320, velocityX: -300))
            XCTAssertEqual(result.0, expected); XCTAssertEqual(result.1, .next)
            XCTAssertEqual(gesture.state, .finished)
            XCTAssertNil(gesture.finish(width: 320, velocityX: 1000))
        }
    }

    func testSecondFingerBeforeClassificationRebasesCentroid() throws {
        var gesture = try TerminalNavigationGesture(touches: 1)
        XCTAssertEqual(gesture.update(touches: 1, x: 20, y: 0, elapsed: 0.02), .possible)
        XCTAssertEqual(gesture.update(touches: 2, x: 200, y: 200, elapsed: 0.03), .possible)
        XCTAssertEqual(gesture.update(touches: 2, x: 202, y: 202, elapsed: 0.04), .possible)
        XCTAssertEqual(gesture.update(touches: 2, x: 260, y: 200, elapsed: 0.2), .dragging(.pane))
        XCTAssertEqual(try XCTUnwrap(gesture.finish(width: 320, velocityX: 300)).1, .previous)
    }

    func testLateFingerChangeVerticalScrollPinchAndThirdTouchCancel() throws {
        var one = try TerminalNavigationGesture(touches: 1)
        _ = one.update(touches: 1, x: 30, y: 0, elapsed: 0.1)
        XCTAssertEqual(one.update(touches: 2, x: 200, y: 0, elapsed: 0.2), .cancelled)
        XCTAssertNil(one.finish(width: 320, velocityX: 500))
        var two = try TerminalNavigationGesture(touches: 2)
        _ = two.update(touches: 2, x: -30, y: 0, elapsed: 0.1)
        XCTAssertEqual(two.update(touches: 1, x: -100, y: 0, elapsed: 0.2), .cancelled)
        var vertical = try TerminalNavigationGesture(touches: 1)
        XCTAssertEqual(vertical.update(touches: 1, x: 3, y: 20, elapsed: 0.1), .cancelled)
        XCTAssertEqual(vertical.update(touches: 1, x: 200, y: 0, elapsed: 0.2), .cancelled)
        var third = try TerminalNavigationGesture(touches: 2)
        XCTAssertEqual(third.update(touches: 3, x: 100, y: 0, elapsed: 0.1), .cancelled)
        var pinch = try TerminalNavigationGesture(touches: 2)
        pinch.cancel()
        XCTAssertNil(pinch.finish(width: 320, velocityX: 500))
    }

    func testGestureRejectsInvalidSamplesAndInsufficientOrReversedMotion() throws {
        XCTAssertThrowsError(try TerminalNavigationGesture(touches: 0))
        for sample in [(Double.nan, 0.0, 0.1), (0.0, Double.infinity, 0.1), (30.0, 0.0, -1.0)] {
            var gesture = try TerminalNavigationGesture(touches: 1)
            XCTAssertEqual(gesture.update(touches: 1, x: sample.0, y: sample.1, elapsed: sample.2), .cancelled)
        }
        var backwardsTime = try TerminalNavigationGesture(touches: 1)
        _ = backwardsTime.update(touches: 1, x: 10, y: 0, elapsed: 0.5)
        XCTAssertEqual(backwardsTime.update(touches: 1, x: 30, y: 0, elapsed: 0.4), .cancelled)
        for (x, velocity) in [(13.0, 0.0), (-40.0, 1000.0)] {
            var gesture = try TerminalNavigationGesture(touches: 1)
            _ = gesture.update(touches: 1, x: x, y: 0, elapsed: 0.1)
            XCTAssertNil(gesture.finish(width: 320, velocityX: velocity))
        }
    }
}
