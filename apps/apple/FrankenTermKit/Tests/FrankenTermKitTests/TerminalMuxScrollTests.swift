import Foundation
import XCTest
@testable import FrankenTermKit

final class TerminalMuxScrollTests: XCTestCase {
    private func fixture() throws -> (TerminalMuxControl, TerminalMuxInputRouting.Destination) {
        let control = TerminalMuxControl()
        let panes = (1...2).map { TerminalMuxPane(paneId: $0, tabId: $0, windowId: 0,
            tabTitle: "", title: "pane", workspace: "test", workingDir: nil,
            size: .init(rows: 24, cols: 80), isActive: $0 == 1, isZoomed: false) }
        try control.inputRouting.replaceInventory(panes.map(\.paneId))
        let selected = try control.inputRouting.select(pane: 1)
        control.inputRouting.didApplyOutput(selected)
        control.publish(panes: panes, shown: 1); control.setVisible(true)
        return (control, selected)
    }

    func testWheelNotchesPreserveTheirCellsAndReversalsUntilDispatched() throws {
        let (control, selected) = try fixture()
        final class Wakes: @unchecked Sendable { let lock = NSLock(); var count = 0 }
        let wakes = Wakes()
        control.setWaker { wakes.lock.withLock { wakes.count += 1 } }
        try control.scroll(steps: 0, column: 1, row: 1, destination: selected)
        XCTAssertNil(control.inputRouting.takePointer(), "no notch, no request")
        try control.scroll(steps: -2, column: 3, row: 4, destination: selected)
        try control.scroll(steps: -1, column: 5, row: 6, destination: selected)
        XCTAssertEqual(wakes.lock.withLock { wakes.count }, 2)
        XCTAssertEqual(control.inputRouting.takePointer()?.action, .wheel(steps: -2, column: 3, row: 4))
        XCTAssertEqual(control.inputRouting.takePointer()?.action, .wheel(steps: -1, column: 5, row: 6))
        XCTAssertNil(control.inputRouting.takePointer())
        XCTAssertThrowsError(try control.scroll(steps: 1000, column: 0, row: 2, destination: selected))
        XCTAssertThrowsError(try control.scroll(steps: 10, column: -5, row: 2, destination: selected))
        try control.scroll(steps: 3, column: 0, row: 0, destination: selected)
        try control.scroll(steps: -3, column: 0, row: 0, destination: selected)
        XCTAssertEqual(control.inputRouting.takePointer()?.action, .wheel(steps: 3, column: 0, row: 0))
        XCTAssertEqual(control.inputRouting.takePointer()?.action, .wheel(steps: -3, column: 0, row: 0))
    }

    func testClicksRequireTheMouseGrabAndRejectQueueOverflow() throws {
        let (control, selected) = try fixture()
        final class Seen: @unchecked Sendable { let lock = NSLock(); var grabs: [Bool] = [] }
        let seen = Seen()
        control.observe { state in seen.lock.withLock { seen.grabs.append(state.mouseGrabbed) } }
        control.setMouseGrabbed(true); control.setMouseGrabbed(true); control.setMouseGrabbed(false)
        XCTAssertEqual(seen.lock.withLock { seen.grabs }, [false, true, false])
        XCTAssertThrowsError(try control.click(column: 3, row: 4, destination: selected))
        control.setMouseGrabbed(true)
        try control.click(column: 3, row: 4, destination: selected)
        XCTAssertThrowsError(try control.click(column: -1, row: 9, destination: selected))
        XCTAssertEqual(control.inputRouting.takePointer()?.action, .click(column: 3, row: 4))
        XCTAssertNil(control.inputRouting.takePointer())
        for column in 0..<TerminalMuxInputRouting.maximumPendingPointers {
            try control.click(column: column, row: 0, destination: selected)
        }
        XCTAssertThrowsError(try control.click(column: 9, row: 0, destination: selected)) {
            XCTAssertEqual($0 as? TerminalInputError, .capacity)
        }
        XCTAssertEqual(control.inputRouting.pendingPointerCount, 8)
    }

    func testRequestingAnotherPaneCancelsPendingGesturesBeforeTheServiceTurn() throws {
        let (control, selected) = try fixture()
        try control.scroll(steps: -2, column: 1, row: 2, destination: selected)
        control.request(pane: 2)
        XCTAssertNil(control.inputRouting.takePointer())
        XCTAssertThrowsError(try control.scroll(steps: -2, column: 1, row: 2, destination: selected))
        XCTAssertEqual(control.takeRequest(), 2)
        let next = try control.inputRouting.select(pane: 2)
        control.publish(shown: 2); control.inputRouting.didApplyOutput(next)
        XCTAssertThrowsError(try control.scroll(steps: -2, column: 1, row: 2, destination: selected))
        try control.scroll(steps: -2, column: 1, row: 2, destination: next)
        XCTAssertEqual(control.inputRouting.takePointer()?.destination, next)
    }

    func testHiddenScenesCancelQueuedGesturesAndRefuseNewOnes() throws {
        let (control, selected) = try fixture()
        control.setMouseGrabbed(true)
        try control.click(column: 1, row: 2, destination: selected)
        control.setVisible(false)
        XCTAssertNil(control.inputRouting.takePointer())
        XCTAssertThrowsError(try control.click(column: 1, row: 2, destination: selected))
        XCTAssertThrowsError(try control.scroll(steps: -1, column: 1, row: 2, destination: selected))
        control.setVisible(true)
        try control.click(column: 1, row: 2, destination: selected)
        XCTAssertNotNil(control.inputRouting.takePointer())
    }

    func testMouseModeChangeCancelsQueuedClicksWithoutDisablingWheelFallback() throws {
        let (control, selected) = try fixture()
        control.setMouseGrabbed(true)
        try control.click(column: 1, row: 2, destination: selected)
        control.setMouseGrabbed(false)
        XCTAssertNil(control.inputRouting.takePointer())
        XCTAssertThrowsError(try control.click(column: 1, row: 2, destination: selected))
        try control.scroll(steps: -1, column: 1, row: 2, destination: selected)
        XCTAssertNotNil(control.inputRouting.takePointer())
    }

    func testGestureWakerMayReadAndUpdateControlWithoutDeadlocking() throws {
        let (control, selected) = try fixture()
        control.setWaker { _ = control.current; control.inputRouting.discardPointers() }
        try control.scroll(steps: -1, column: 1, row: 2, destination: selected)
        XCTAssertNil(control.inputRouting.takePointer())
        control.setWaker(nil)
    }

    func testEndedConnectionRefusesFurtherGestures() throws {
        let (control, selected) = try fixture()
        try control.scroll(steps: -1, column: 1, row: 2, destination: selected)
        control.endQueries()
        XCTAssertNil(control.inputRouting.takePointer())
        XCTAssertThrowsError(try control.scroll(steps: -1, column: 1, row: 2, destination: selected))
    }

    func testReentrantHideCancelsTheJustAdmittedGestureBeforeCallerReturns() throws {
        let (control, selected) = try fixture()
        control.setWaker {
            if control.current.visible { control.setVisible(false) }
        }
        try control.scroll(steps: -1, column: 1, row: 2, destination: selected)
        XCTAssertFalse(control.current.visible)
        XCTAssertNil(control.inputRouting.takePointer())
        control.setWaker(nil)
        control.setVisible(true)
        XCTAssertNil(control.inputRouting.takePointer(), "reshowing must not revive a hidden gesture")
    }

    func testConcurrentVisibilityAndGestureAdmissionLeaveNoHiddenWork() throws {
        let (control, selected) = try fixture()
        DispatchQueue.concurrentPerform(iterations: 1000) { index in
            if index.isMultiple(of: 2) {
                control.setVisible(false)
                control.setVisible(true)
            } else {
                do { try control.scroll(steps: 1, column: 1, row: 2, destination: selected) }
                catch {
                    let failure = error as? TerminalInputError
                    XCTAssertTrue(failure == .capacity || failure == .stale)
                }
            }
        }
        control.setVisible(false)
        XCTAssertNil(control.inputRouting.takePointer())
        XCTAssertEqual(control.inputRouting.pendingPointerCount, 0)
    }
}
