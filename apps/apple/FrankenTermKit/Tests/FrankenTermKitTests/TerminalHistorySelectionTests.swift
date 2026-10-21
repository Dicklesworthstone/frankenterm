import Foundation
import XCTest
@testable import FrankenTermKit

private enum HistorySelectionFailure: Error { case model }

/// Records applied viewports; an apply can be held open to build up requests.
private actor HistoryModel {
    private(set) var applied: [Int64?] = []
    private var holding = false, failNext = false
    private var held: CheckedContinuation<Void, Never>?
    private var entered: CheckedContinuation<Void, Never>?
    func hold() { holding = true }
    func failOnce() { failNext = true }
    func apply(_ top: Int64?) async throws {
        applied.append(top)
        if failNext { failNext = false; throw HistorySelectionFailure.model }
        guard holding else { return }
        entered?.resume(); entered = nil
        await withCheckedContinuation { held = $0 }
    }
    func waitUntilHolding() async {
        if held != nil { return }
        await withCheckedContinuation { entered = $0 }
    }
    func release() { holding = false; held?.resume(); held = nil }
}

@MainActor
final class TerminalHistorySelectionTests: XCTestCase {
    private func selection(_ model: HistoryModel) -> TerminalHistorySelection<String> {
        TerminalHistorySelection(source: "pane") { top, _ in try await model.apply(top) }
    }

    func testAPanBurstAppliesTheFirstAndTheNewestRequestOnly() async throws {
        let model = HistoryModel(), owner = selection(model)
        await model.hold()
        try owner.request(top: 10, source: "pane")
        await model.waitUntilHolding()
        for top in 11...1_000 { try owner.request(top: Int64(top), source: "pane") }
        try owner.request(top: nil, source: "pane")
        await model.release()
        await owner.waitUntilSettled()
        let applied = await model.applied
        XCTAssertEqual(applied, [10, nil])
        XCTAssertTrue(owner.isSettled)
    }

    func testARepeatedRequestIsAppliedAgain() async throws {
        // The model may have moved the viewport (typing, eviction) since.
        let model = HistoryModel(), owner = selection(model)
        try owner.request(top: 5, source: "pane"); await owner.waitUntilSettled()
        try owner.request(top: 5, source: "pane"); await owner.waitUntilSettled()
        let applied = await model.applied
        XCTAssertEqual(applied, [5, 5])
    }

    func testRetirementDropsTheWaitingRequestAndJoinsTheCallInFlight() async throws {
        let model = HistoryModel(), owner = selection(model)
        await model.hold()
        try owner.request(top: 1, source: "pane")
        await model.waitUntilHolding()
        try owner.request(top: 2, source: "pane")
        owner.retire()
        XCTAssertFalse(owner.isSettled, "the call in flight is still owned")
        XCTAssertThrowsError(try owner.request(top: 3, source: "pane"))
        await model.release()
        await owner.waitUntilSettled()
        let applied = await model.applied
        XCTAssertEqual(applied, [1])
    }

    func testFailureIsReportedOnceWithoutRetryAndALaterRequestRunsAgain() async throws {
        let model = HistoryModel(), owner = selection(model)
        var failures = 0
        owner.onFailure = { _ in failures += 1 }
        await model.failOnce()
        try owner.request(top: 7, source: "pane"); await owner.waitUntilSettled()
        XCTAssertEqual(failures, 1)
        try owner.request(top: 8, source: "pane"); await owner.waitUntilSettled()
        XCTAssertEqual(failures, 1)
        let applied = await model.applied
        XCTAssertEqual(applied, [7, 8])
    }

    func testAnotherSourceIsRefused() async throws {
        let model = HistoryModel(), owner = selection(model)
        XCTAssertThrowsError(try owner.request(top: 1, source: "other"))
        await owner.waitUntilSettled()
        let applied = await model.applied
        XCTAssertTrue(applied.isEmpty)
    }
}
