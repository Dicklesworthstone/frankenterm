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
}
