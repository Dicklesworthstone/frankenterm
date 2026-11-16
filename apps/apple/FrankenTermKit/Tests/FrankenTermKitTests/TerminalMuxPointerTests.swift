import Foundation
import XCTest
@testable import FrankenTermKit

final class TerminalMuxPointerTests: XCTestCase {
    typealias Routing = TerminalMuxInputRouting
    private func ready(_ pane: Int = 1) throws -> (Routing, Routing.Destination) {
        let routing = Routing()
        try routing.replaceInventory([1, 2, 3])
        let selected = try routing.select(pane: pane)
        routing.didApplyOutput(selected)
        return (routing, selected)
    }
    private func drain(_ routing: Routing) -> [Routing.PointerInput] {
        var received: [Routing.PointerInput] = []
        for _ in 0..<128 {
            guard let event = routing.takePointer() else { return received }
            received.append(event)
        }
        XCTFail("bounded pointer queue did not drain")
        return received
    }

    func testReadinessAndExactDisplayedVisitAreRequiredBeforeAdmission() throws {
        let routing = Routing()
        try routing.replaceInventory([1, 2])
        let first = try routing.select(pane: 1)
        XCTAssertThrowsError(try routing.enqueuePointer(.click(column: 1, row: 2), destination: first))
        XCTAssertEqual(routing.pendingPointerCount, 0)
        routing.didApplyOutput(first)
        XCTAssertTrue(try routing.enqueuePointer(.click(column: 1, row: 2), destination: first))
        XCTAssertEqual(routing.takePointer()?.destination, first)
        let second = try routing.select(pane: 2)
        routing.didApplyOutput(second)
        XCTAssertThrowsError(try routing.enqueuePointer(.click(column: 1, row: 2), destination: first))
        XCTAssertNil(routing.takePointer())
    }

    func testWheelAndClickOrderSurvivesBatchedDispatch() throws {
        let (routing, selected) = try ready()
        try routing.enqueuePointer(.wheel(steps: 21, column: 2, row: 4), destination: selected)
        try routing.enqueuePointer(.click(column: 7, row: 8), destination: selected)
        try routing.enqueuePointer(.wheel(steps: -10, column: 2, row: 4), destination: selected)
        let expected: [Routing.PointerAction] = [
            .wheel(steps: 8, column: 2, row: 4), .wheel(steps: 8, column: 2, row: 4),
            .wheel(steps: 5, column: 2, row: 4), .click(column: 7, row: 8),
            .wheel(steps: -8, column: 2, row: 4), .wheel(steps: -2, column: 2, row: 4)
        ]
        let events = drain(routing)
        XCTAssertEqual(events.map(\.action), expected)
        XCTAssertTrue(events.allSatisfy { $0.destination == selected })
        XCTAssertFalse(routing.hasPendingPointers)
    }

    func testOnlyAdjacentSameCellAndDirectionWheelEventsCoalesce() throws {
        let (routing, selected) = try ready()
        try routing.enqueuePointer(.wheel(steps: 3, column: 2, row: 4), destination: selected)
        try routing.enqueuePointer(.wheel(steps: 4, column: 2, row: 4), destination: selected)
        try routing.enqueuePointer(.wheel(steps: -2, column: 2, row: 4), destination: selected)
        try routing.enqueuePointer(.wheel(steps: 2, column: 3, row: 4), destination: selected)
        try routing.enqueuePointer(.click(column: 3, row: 4), destination: selected)
        try routing.enqueuePointer(.wheel(steps: 1, column: 3, row: 4), destination: selected)
        XCTAssertEqual(routing.pendingPointerCount, 5)
        XCTAssertEqual(drain(routing).map(\.action), [
            .wheel(steps: 7, column: 2, row: 4), .wheel(steps: -2, column: 2, row: 4),
            .wheel(steps: 2, column: 3, row: 4), .click(column: 3, row: 4),
            .wheel(steps: 1, column: 3, row: 4)
        ])
    }

    func testWheelSaturationNeverSilentlyDropsAcceptedNotches() throws {
        let (routing, selected) = try ready()
        for _ in 0..<Routing.maximumPendingPointers {
            try routing.enqueuePointer(.wheel(steps: 64, column: 0, row: 0), destination: selected)
        }
        XCTAssertEqual(routing.pendingPointerCount, 8)
        XCTAssertThrowsError(try routing.enqueuePointer(.wheel(steps: 1, column: 0, row: 0), destination: selected)) {
            XCTAssertEqual($0 as? TerminalInputError, .capacity)
        }
        let events = drain(routing)
        let notches = events.reduce(0) { result, event in
            if case .wheel(let steps, _, _) = event.action { return result + steps }
            XCTFail("wheel became a click"); return result
        }
        XCTAssertEqual(events.count, 64)
        XCTAssertEqual(notches, 512)
    }

    func testTenThousandBlockedTapsRetainOnlyTheOriginalEightInOrder() throws {
        let (routing, selected) = try ready()
        for column in 0..<8 { try routing.enqueuePointer(.click(column: column, row: 0), destination: selected) }
        for _ in 0..<10_000 {
            XCTAssertThrowsError(try routing.enqueuePointer(.click(column: 100, row: 0), destination: selected)) {
                XCTAssertEqual($0 as? TerminalInputError, .capacity)
            }
        }
        XCTAssertEqual(routing.pendingPointerCount, 8)
        XCTAssertEqual(drain(routing).map(\.action), (0..<8).map { .click(column: $0, row: 0) })
    }

    func testSwitchCancelsUnsentGesturesAndReturningCannotReviveOldCallbacks() throws {
        let (routing, original) = try ready()
        try routing.enqueuePointer(.wheel(steps: 20, column: 0, row: 0), destination: original)
        _ = routing.takePointer()
        let other = try routing.select(pane: 2)
        XCTAssertNil(routing.takePointer(), "old wheel tail was not redirected")
        routing.didApplyOutput(other)
        let returned = try routing.select(pane: 1)
        routing.didApplyOutput(returned)
        XCTAssertNotEqual(returned, original)
        XCTAssertThrowsError(try routing.enqueuePointer(.click(column: 1, row: 1), destination: original))
        try routing.enqueuePointer(.click(column: 2, row: 2), destination: returned)
        XCTAssertEqual(routing.takePointer()?.destination, returned)
    }

    func testObservedRemovalAndReuseRetirePendingPointerAuthority() throws {
        let (routing, original) = try ready()
        try routing.enqueuePointer(.click(column: 1, row: 1), destination: original)
        routing.remove(pane: 1)
        XCTAssertFalse(routing.hasPendingPointers)
        try routing.replaceInventory([1, 2])
        let replacement = try routing.select(pane: 1)
        routing.didApplyOutput(replacement)
        XCTAssertThrowsError(try routing.enqueuePointer(.click(column: 1, row: 1), destination: original))
        XCTAssertNil(routing.takePointer())
    }

    func testInventoryRemovalCancelsButReorderAndUnrelatedRemovalPreserveInput() throws {
        let (routing, selected) = try ready()
        try routing.enqueuePointer(.click(column: 5, row: 6), destination: selected)
        try routing.replaceInventory([3, 1, 2])
        routing.remove(pane: 3)
        XCTAssertEqual(routing.takePointer()?.destination, selected)
        try routing.enqueuePointer(.click(column: 5, row: 6), destination: selected)
        try routing.replaceInventory([2])
        XCTAssertNil(routing.takePointer())
    }

    func testHideOrInspectionDiscardsOnlyUnsentPointerWork() throws {
        let (routing, selected) = try ready()
        try routing.enqueuePointer(.wheel(steps: 20, column: 0, row: 0), destination: selected)
        let taken = try XCTUnwrap(routing.takePointer())
        routing.discardPointers()
        XCTAssertNil(routing.takePointer())
        XCTAssertEqual(taken.action, .wheel(steps: 8, column: 0, row: 0))
        XCTAssertNoThrow(try routing.validateDelivery(selected), "discard does not retire already-admitted keyboard input")
        XCTAssertTrue(routing.current.ready)
    }

    func testCoordinatesAndStepLimitsRejectBeforeAnyMutation() throws {
        let (routing, selected) = try ready()
        for bad in [Int.min, -1, 512, Int.max] {
            XCTAssertThrowsError(try routing.enqueuePointer(.click(column: bad, row: 0), destination: selected))
            XCTAssertThrowsError(try routing.enqueuePointer(.wheel(steps: 1, column: 0, row: bad), destination: selected))
        }
        for bad in [Int.min, -65, 65, Int.max] {
            XCTAssertThrowsError(try routing.enqueuePointer(.wheel(steps: bad, column: 0, row: 0), destination: selected))
        }
        XCTAssertFalse(try routing.enqueuePointer(.wheel(steps: 0, column: 0, row: 0), destination: selected))
        XCTAssertNil(routing.takePointer())
        try routing.enqueuePointer(.click(column: 511, row: 511), destination: selected)
        XCTAssertEqual(routing.takePointer()?.action, .click(column: 511, row: 511))
    }

    func testForeignConnectionCannotSupplyAPointerTarget() throws {
        let (routing, _) = try ready()
        let (_, foreign) = try ready()
        XCTAssertThrowsError(try routing.enqueuePointer(.click(column: 1, row: 1), destination: foreign))
        XCTAssertFalse(routing.hasPendingPointers)
    }

    func testWakerMayReenterAndLateRegistrationNoticesPendingWork() throws {
        let (routing, selected) = try ready()
        try routing.enqueuePointer(.click(column: 1, row: 1), destination: selected)
        final class Seen: @unchecked Sendable {
            let lock = NSLock()
            var events: [Routing.PointerInput] = []
        }
        let seen = Seen()
        routing.setPointerWaker {
            if let event = routing.takePointer() { seen.lock.withLock { seen.events.append(event) } }
        }
        try routing.enqueuePointer(.click(column: 2, row: 2), destination: selected)
        XCTAssertEqual(seen.lock.withLock { seen.events.map(\.action) }, [
            .click(column: 1, row: 1), .click(column: 2, row: 2)
        ])
    }

    func testRetirementClearsQueueAndCannotInstallAnotherWaker() throws {
        let (routing, selected) = try ready()
        try routing.enqueuePointer(.click(column: 1, row: 1), destination: selected)
        routing.retire(); routing.retire()
        XCTAssertNil(routing.takePointer())
        routing.setPointerWaker { XCTFail("retired routing was revived") }
        XCTAssertThrowsError(try routing.enqueuePointer(.click(column: 1, row: 1), destination: selected))
        XCTAssertEqual(routing.pendingPointerCount, 0)
    }

    func testConcurrentProducerStormCannotOverbookQueue() throws {
        let (routing, selected) = try ready()
        final class Counts: @unchecked Sendable {
            let lock = NSLock()
            var admitted = 0, refused = 0
        }
        let counts = Counts()
        DispatchQueue.concurrentPerform(iterations: 2000) { index in
            do {
                try routing.enqueuePointer(.click(column: index % 512, row: 0), destination: selected)
                counts.lock.withLock { counts.admitted += 1 }
            } catch {
                XCTAssertEqual(error as? TerminalInputError, .capacity)
                counts.lock.withLock { counts.refused += 1 }
            }
        }
        XCTAssertEqual(counts.admitted, 8); XCTAssertEqual(counts.refused, 1992)
        XCTAssertEqual(drain(routing).count, 8)
    }
}
