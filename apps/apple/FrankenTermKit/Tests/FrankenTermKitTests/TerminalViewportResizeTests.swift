import Foundation
import XCTest
@testable import FrankenTermKit

private actor ResizeProbe {
    var values: [TerminalSessionGeometry] = []
    var result = TerminalResizeAdmission.queued
    var paused = false
    var waiter: CheckedContinuation<TerminalResizeAdmission, Error>?
    func configure(_ result: TerminalResizeAdmission = .queued, paused: Bool = false) {
        self.result = result; self.paused = paused
    }
    func request(_ geometry: TerminalSessionGeometry) async throws -> TerminalResizeAdmission {
        values.append(geometry)
        if paused { return try await withCheckedThrowingContinuation { waiter = $0 } }
        return result
    }
    func release(_ result: TerminalResizeAdmission) { let old = waiter; waiter = nil; old?.resume(returning: result) }
    func fail() { let old = waiter; waiter = nil; old?.resume(throwing: TerminalViewportResizeError.retired) }
    func count() -> Int { values.count }
    func requested() -> [TerminalSessionGeometry] { values }
    func parked() -> Bool { waiter != nil }
}

@MainActor
final class TerminalViewportResizeTests: XCTestCase {
    private func geometry(_ width: UInt32) throws -> TerminalSessionGeometry { try .init(columns: width, rows: 24) }
    private func waitForPark(_ probe: ResizeProbe) async throws {
        for _ in 0..<10000 { if await probe.parked() { return }; await Task.yield() }
        XCTFail("request never parked")
        throw TerminalViewportResizeError.retired
    }
    private func setup() throws -> (TerminalViewportResize<UUID>, ResizeProbe, UUID) {
        let source = UUID(), probe = ResizeProbe()
        return (TerminalViewportResize(source: source, initial: try geometry(80)) { try await probe.request($0) }, probe, source)
    }
    func testIdenticalGeometryDoesNotIssueARequest() async throws {
        let (owner, probe, source) = try setup()
        for _ in 0..<500 { try owner.propose(geometry(80), source: source) }
        await owner.waitUntilSettled()
        let count = await probe.count(); XCTAssertEqual(count, 0); XCTAssertFalse(owner.isPending)
    }
    func testAdmissionDoesNotPretendTheModelWasResized() async throws {
        let (owner, probe, source) = try setup()
        try owner.propose(geometry(90), source: source); await owner.waitUntilSettled()
        XCTAssertEqual(owner.applied, try geometry(80)); XCTAssertTrue(owner.isPending)
        try owner.propose(geometry(100), source: source); await owner.waitUntilSettled()
        var count = await probe.count(); XCTAssertEqual(count, 1)
        try owner.didApply(geometry(90), source: source); await owner.waitUntilSettled()
        count = await probe.count(); XCTAssertEqual(count, 2)
        XCTAssertEqual(owner.applied, try geometry(90))
        try owner.didApply(geometry(100), source: source); XCTAssertFalse(owner.isPending)
    }
    func testLayoutStormRetainsOnlyTheLastProposal() async throws {
        let (owner, probe, source) = try setup()
        try owner.propose(geometry(81), source: source); await owner.waitUntilSettled()
        for i in 0..<10000 { try owner.propose(geometry(UInt32(82 + i % 200)), source: source) }
        try owner.propose(geometry(177), source: source)
        let before = await probe.count(); XCTAssertEqual(before, 1)
        try owner.didApply(geometry(81), source: source); await owner.waitUntilSettled()
        let requested = await probe.requested(); XCTAssertEqual(requested, try [geometry(81), geometry(177)])
        try owner.didApply(geometry(177), source: source); XCTAssertFalse(owner.isPending)
    }
    func testReturnToOriginalSizeIsNotDroppedDuringAFlight() async throws {
        let (owner, probe, source) = try setup()
        try owner.propose(geometry(90), source: source); await owner.waitUntilSettled()
        try owner.propose(geometry(80), source: source)
        try owner.didApply(geometry(90), source: source); await owner.waitUntilSettled()
        let values = await probe.requested(); XCTAssertEqual(values, try [geometry(90), geometry(80)])
    }
    func testLatestEqualsInFlightCancelsAnIntermediateLayout() async throws {
        let (owner, probe, source) = try setup()
        try owner.propose(geometry(90), source: source); await owner.waitUntilSettled()
        try owner.propose(geometry(100), source: source)
        try owner.propose(geometry(90), source: source)
        try owner.didApply(geometry(90), source: source); await owner.waitUntilSettled()
        let count = await probe.count(); XCTAssertEqual(count, 1); XCTAssertFalse(owner.isPending)
    }
    func testCompletionBeforeAdmissionReturnDoesNotLoseTheNextResize() async throws {
        let (owner, probe, source) = try setup()
        await probe.configure(paused: true)
        try owner.propose(geometry(90), source: source); try await waitForPark(probe)
        try owner.propose(geometry(100), source: source)
        try owner.didApply(geometry(90), source: source)
        let count = await probe.count(); XCTAssertEqual(count, 1)
        await probe.configure(); await probe.release(.queued); await owner.waitUntilSettled()
        await owner.waitUntilSettled()
        let values = await probe.requested(); XCTAssertEqual(values, try [geometry(90), geometry(100)])
    }
    func testBusyHasNoPollingOrLayoutTriggeredRetry() async throws {
        let (owner, probe, source) = try setup()
        await probe.configure(.busy)
        try owner.propose(geometry(90), source: source); await owner.waitUntilSettled()
        for _ in 0..<100 { try owner.propose(geometry(100), source: source); await Task.yield() }
        let count = await probe.count(); XCTAssertEqual(count, 1)
        await probe.configure()
        try owner.serviceProgress(source: source); await owner.waitUntilSettled()
        let values = await probe.requested(); XCTAssertEqual(values, try [geometry(90), geometry(100)])
    }
    func testBusyReturnDoesNotLoseProgressAlreadyObserved() async throws {
        let (owner, probe, source) = try setup()
        await probe.configure(paused: true)
        try owner.propose(geometry(90), source: source); try await waitForPark(probe)
        try owner.serviceProgress(source: source)
        await probe.configure(); await probe.release(.busy)
        await owner.waitUntilSettled(); await owner.waitUntilSettled()
        let count = await probe.count(); XCTAssertEqual(count, 2)
    }
    func testWrongSourceCannotProposeOrCompleteAResize() async throws {
        let (owner, probe, source) = try setup(), wrong = UUID()
        XCTAssertThrowsError(try owner.propose(geometry(90), source: wrong))
        try owner.propose(geometry(90), source: source); await owner.waitUntilSettled()
        XCTAssertThrowsError(try owner.didApply(geometry(90), source: wrong))
        XCTAssertThrowsError(try owner.serviceProgress(source: wrong))
        XCTAssertEqual(owner.applied, try geometry(80))
        let count = await probe.count(); XCTAssertEqual(count, 1)
    }
    func testRetirementFencesLateUncooperativeAdmission() async throws {
        let (owner, probe, source) = try setup()
        await probe.configure(paused: true)
        try owner.propose(geometry(90), source: source); try await waitForPark(probe)
        try owner.propose(geometry(100), source: source)
        owner.retire(); owner.retire()
        await probe.release(.queued); await owner.waitUntilSettled()
        XCTAssertTrue(owner.isRetired); XCTAssertFalse(owner.isPending)
        XCTAssertThrowsError(try owner.didApply(geometry(90), source: source))
        XCTAssertThrowsError(try owner.propose(geometry(100), source: source))
        let count = await probe.count(); XCTAssertEqual(count, 1)
    }
    func testFailureRetiresInsteadOfReplayingTheRequest() async throws {
        let (owner, probe, source) = try setup()
        var failures = 0; owner.onFailure = { _ in failures += 1 }
        await probe.configure(paused: true)
        try owner.propose(geometry(90), source: source); try await waitForPark(probe)
        try owner.propose(geometry(100), source: source)
        await probe.fail(); await owner.waitUntilSettled()
        XCTAssertEqual(failures, 1); XCTAssertTrue(owner.isRetired)
        XCTAssertThrowsError(try owner.serviceProgress(source: source))
        let count = await probe.count(); XCTAssertEqual(count, 1)
    }
    func testPixelGeometryChangesAreNotConfusedWithCellEquality() async throws {
        let (owner, probe, source) = try setup()
        let pixels = try TerminalSessionGeometry(columns: 80, rows: 24, pixelWidth: 800, pixelHeight: 480)
        try owner.propose(pixels, source: source); await owner.waitUntilSettled()
        let values = await probe.requested(); XCTAssertEqual(values, [pixels])
        try owner.didApply(pixels, source: source); XCTAssertEqual(owner.applied, pixels)
    }
}
