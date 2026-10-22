import XCTest
@testable import FrankenTermKit

final class TerminalMuxScrollTests: XCTestCase {
    func testWheelNotchesAddUpUntilTakenAndStayBounded() {
        let control = TerminalMuxControl()
        final class Wakes: @unchecked Sendable { let lock = NSLock(); var count = 0 }
        let wakes = Wakes()
        control.setWaker { wakes.lock.withLock { wakes.count += 1 } }

        control.scroll(steps: 0, column: 1, row: 1)
        XCTAssertNil(control.takeScroll(), "no notch, no request")
        control.scroll(steps: -2, column: 3, row: 4)
        control.scroll(steps: -1, column: 5, row: 6)
        XCTAssertEqual(wakes.lock.withLock { wakes.count }, 2, "the transport is woken to send them")
        let taken = control.takeScroll()
        XCTAssertEqual(taken?.steps, -3)
        XCTAssertEqual(taken?.column, 5, "the latest cell")
        XCTAssertEqual(taken?.row, 6)
        XCTAssertNil(control.takeScroll(), "sent once")

        control.scroll(steps: 1000, column: -5, row: 2)
        control.scroll(steps: 10, column: 0, row: 2)
        let bounded = control.takeScroll()
        XCTAssertEqual(bounded?.steps, TerminalMuxControl.maximumScrollSteps)
        XCTAssertEqual(bounded?.column, 0, "no negative cell")

        control.scroll(steps: 3, column: 0, row: 0)
        control.scroll(steps: -3, column: 0, row: 0)
        XCTAssertNil(control.takeScroll(), "notches that cancel out send nothing")
    }

    func testClicksQueueInOrderBoundedAndTheMouseGrabIsPublishedOnChange() {
        let control = TerminalMuxControl()
        final class Seen: @unchecked Sendable { let lock = NSLock(); var grabs: [Bool] = [] }
        let seen = Seen()
        control.observe { state in seen.lock.withLock { seen.grabs.append(state.mouseGrabbed) } }
        control.setMouseGrabbed(true)
        control.setMouseGrabbed(true)
        control.setMouseGrabbed(false)
        XCTAssertEqual(seen.lock.withLock { seen.grabs }, [false, true, false], "only changes are published")

        control.click(column: 3, row: 4)
        control.click(column: -1, row: 9)
        let taken = control.takeClicks()
        XCTAssertEqual(taken.map(\.column), [3, 0])
        XCTAssertEqual(taken.map(\.row), [4, 9])
        XCTAssertTrue(control.takeClicks().isEmpty, "sent once")
        for column in 0..<(TerminalMuxControl.maximumPendingClicks + 5) { control.click(column: column, row: 0) }
        XCTAssertEqual(control.takeClicks().count, TerminalMuxControl.maximumPendingClicks)
    }
}
