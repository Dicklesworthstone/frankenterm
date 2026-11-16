import Foundation
import XCTest
@testable import FrankenTermKit

private actor VisibilityProbe {
    enum Failure: Error { case injected }
    private var calls: [(Bool, Int)] = []
    private var current = 0, maximum = 0
    private var shouldBlock = false
    private var parked: CheckedContinuation<Void, Never>?
    private var arrival: CheckedContinuation<Void, Never>?
    private var failNext = false

    func blockNext(failing: Bool = false) { shouldBlock = true; failNext = failing }
    func apply(_ value: Bool, _ source: Int) async throws {
        calls.append((value, source)); current += 1; maximum = max(maximum, current)
        defer { current -= 1 }
        let fail = failNext; failNext = false
        if shouldBlock {
            shouldBlock = false
            await withCheckedContinuation { continuation in
                parked = continuation
                let ready = arrival; arrival = nil; ready?.resume()
            }
        }
        if fail { throw Failure.injected }
    }
    func waitForApply() async {
        if parked != nil { return }
        await withCheckedContinuation { arrival = $0 }
    }
    func release() { let old = parked; parked = nil; old?.resume() }
    func values() -> [Bool] { calls.map(\.0) }
    func sources() -> [Int] { calls.map(\.1) }
    func maximumActive() -> Int { maximum }
}

@MainActor
final class TerminalPresentationVisibilityTests: XCTestCase {
    private func owner(_ probe: VisibilityProbe, initial: Bool = false) -> TerminalPresentationVisibility<Int> {
        .init(source: 7, initiallyVisible: initial) { value, source in try await probe.apply(value, source) }
    }

    func testInitialPolicyDoesNotLaunchWork() async {
        let probe = VisibilityProbe(), owner = owner(probe)
        await owner.waitUntilSettled()
        let calls = await probe.values()
        XCTAssertEqual(calls, [])
        XCTAssertEqual(owner.appliedVisible, false)
        XCTAssertTrue(owner.isSettled)
    }

    func testHideWaitsForActualShowThenAppliesNewestIntent() async throws {
        let probe = VisibilityProbe(), owner = owner(probe)
        await probe.blockNext()
        try owner.request(true, source: 7)
        await probe.waitForApply()
        try owner.request(false, source: 7)
        XCTAssertFalse(owner.isSettled)
        await probe.release(); await owner.waitUntilSettled()
        let calls = await probe.values(), sources = await probe.sources()
        XCTAssertEqual(calls, [true, false]); XCTAssertEqual(sources, [7, 7])
        XCTAssertEqual(owner.appliedVisible, false)
    }

    func testTenThousandRequestsRetainOnlyLatestValueAndOneApply() async throws {
        let probe = VisibilityProbe(), owner = owner(probe)
        await probe.blockNext()
        try owner.request(true, source: 7); await probe.waitForApply()
        for index in 0..<10_000 { try owner.request(index.isMultiple(of: 2), source: 7) }
        let whileBlocked = await probe.values()
        XCTAssertEqual(whileBlocked, [true])
        await probe.release(); await owner.waitUntilSettled()
        let calls = await probe.values(), concurrency = await probe.maximumActive()
        XCTAssertEqual(calls, [true, false]); XCTAssertEqual(concurrency, 1)
        XCTAssertEqual(owner.appliedVisible, false)
    }

    func testReturningToInFlightStateNeedsNoRedundantApply() async throws {
        let probe = VisibilityProbe(), owner = owner(probe)
        await probe.blockNext()
        try owner.request(true, source: 7); await probe.waitForApply()
        try owner.request(false, source: 7); try owner.request(true, source: 7)
        await probe.release(); await owner.waitUntilSettled()
        let calls = await probe.values()
        XCTAssertEqual(calls, [true]); XCTAssertEqual(owner.appliedVisible, true)
    }

    func testRetirementDuringShowCannotResurrectPresentation() async throws {
        let probe = VisibilityProbe(), owner = owner(probe)
        await probe.blockNext()
        try owner.request(true, source: 7); await probe.waitForApply()
        owner.retire(); owner.retire()
        XCTAssertThrowsError(try owner.request(true, source: 7))
        await probe.release(); await owner.waitUntilSettled()
        let calls = await probe.values()
        XCTAssertEqual(calls, [true, false]); XCTAssertEqual(owner.appliedVisible, false)
        XCTAssertTrue(owner.isRetired)
    }

    func testRetirementOfAlreadyHiddenModelIsIdle() async {
        let probe = VisibilityProbe(), owner = owner(probe)
        owner.retire(); await owner.waitUntilSettled()
        let calls = await probe.values()
        XCTAssertEqual(calls, [])
    }

    func testFailedCurrentRequestNeedsExplicitRetry() async throws {
        let probe = VisibilityProbe(), owner = owner(probe)
        var failures = 0; owner.onFailure = { _ in failures += 1 }
        await probe.blockNext(failing: true)
        try owner.request(true, source: 7); await probe.waitForApply()
        await probe.release(); await owner.waitUntilSettled()
        XCTAssertEqual(failures, 1); XCTAssertNotNil(owner.lastFailure); XCTAssertNil(owner.appliedVisible)
        try owner.request(true, source: 7)
        await owner.waitUntilSettled()
        let beforeRetry = await probe.values(); XCTAssertEqual(beforeRetry, [true])
        owner.retry(); await owner.waitUntilSettled()
        let calls = await probe.values()
        XCTAssertEqual(calls, [true, true]); XCTAssertEqual(owner.appliedVisible, true)
        XCTAssertNil(owner.lastFailure)
    }

    func testObsoleteFailureDoesNotCloseNewerHiddenState() async throws {
        let probe = VisibilityProbe(), owner = owner(probe)
        var failures = 0; owner.onFailure = { _ in failures += 1 }
        await probe.blockNext(failing: true)
        try owner.request(true, source: 7); await probe.waitForApply()
        owner.retire()
        await probe.release(); await owner.waitUntilSettled()
        let calls = await probe.values()
        XCTAssertEqual(calls, [true, false]); XCTAssertEqual(failures, 0)
        XCTAssertEqual(owner.appliedVisible, false); XCTAssertNil(owner.lastFailure)
    }

    func testFailureObserverCanRetireWithoutLosingFinalHide() async throws {
        let probe = VisibilityProbe(), owner = owner(probe)
        owner.onFailure = { [weak owner] _ in owner?.retire() }
        await probe.blockNext(failing: true)
        try owner.request(true, source: 7); await probe.waitForApply()
        await probe.release(); await owner.waitUntilSettled()
        let calls = await probe.values()
        XCTAssertEqual(calls, [true, false]); XCTAssertTrue(owner.isRetired)
        XCTAssertEqual(owner.appliedVisible, false)
    }

    func testStaleSourceCannotChangeIntent() async throws {
        let probe = VisibilityProbe(), owner = owner(probe)
        XCTAssertThrowsError(try owner.request(true, source: 8)) { error in
            XCTAssertEqual(error as? TerminalPresentationVisibilityError, .staleSource)
        }
        await owner.waitUntilSettled()
        let calls = await probe.values()
        XCTAssertEqual(calls, []); XCTAssertFalse(owner.requestedVisible)
    }

    func testConcurrentJoinersWaitForHideSettlement() async throws {
        let probe = VisibilityProbe(), owner = owner(probe, initial: true)
        await probe.blockNext()
        owner.retire(); await probe.waitForApply()
        let first = Task { await owner.waitUntilSettled(); return owner.appliedVisible }
        let second = Task { await owner.waitUntilSettled(); return owner.appliedVisible }
        XCTAssertFalse(owner.isSettled)
        await probe.release()
        let a = await first.value, b = await second.value
        XCTAssertEqual(a, false); XCTAssertEqual(b, false)
    }

    func testRetiredHideFailureRetainsRetryPath() async {
        let probe = VisibilityProbe(), owner = owner(probe, initial: true)
        await probe.blockNext(failing: true)
        owner.retire(); await probe.waitForApply()
        await probe.release(); await owner.waitUntilSettled()
        XCTAssertNotNil(owner.lastFailure); XCTAssertNil(owner.appliedVisible)
        owner.retry(); await owner.waitUntilSettled()
        let calls = await probe.values()
        XCTAssertEqual(calls, [false, false]); XCTAssertEqual(owner.appliedVisible, false)
    }
}
