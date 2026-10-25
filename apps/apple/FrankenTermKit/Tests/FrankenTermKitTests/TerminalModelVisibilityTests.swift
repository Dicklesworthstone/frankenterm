import Foundation
import XCTest
@testable import FrankenTermKit

/// Opaque model fixture: runtime/coordinator behavior is real, but these bytes
/// are deliberately not claimed to be Rust parser output or an FTSF frame.
private final class VisibilityModel: TerminalSessionModel, @unchecked Sendable {
    let source: TerminalSource
    private let lock = NSLock()
    private var feeds = 0, captures = 0, closes = 0
    private var reply: TerminalModelReply?
    init(source: TerminalSource) { self.source = source }
    func counts() -> (Int, Int, Int) { lock.withLock { (feeds, captures, closes) } }
    func feed(_ bytes: Data) throws {
        try lock.withLock {
            feeds += 1
            guard reply == nil else { throw TerminalModelError.busy }
            reply = try .init(sequence: UInt64(feeds), bytes: Data([0x52]))
        }
    }
    func resize(_ geometry: TerminalSessionGeometry) throws {}
    func encode(_ intent: TerminalKeyboardIntent, maximumBytes: Int) throws -> Data { Data([0x4B]) }
    func preparePaste(_ text: String, maximumBytes: Int) throws -> TerminalPreparedPaste {
        .init(text: text, bracketed: false)
    }
    func copyReply() throws -> TerminalModelReply? { lock.withLock { reply } }
    func settleReply(sequence: UInt64, disposition: TerminalModelReplyDisposition) throws {
        try lock.withLock {
            guard reply?.sequence == sequence else { throw TerminalModelError.stale }
            reply = nil
        }
    }
    func captureFrame(viewportTop: Int64?) throws -> Data {
        lock.withLock { captures += 1; return Data("opaque-frame-\(feeds)".utf8) }
    }
    func setFontRevision(_ revision: UInt64) throws {}
    func setBasePalette(_ rgba: [UInt8]) throws {}
    func close() throws { lock.withLock { closes += 1; reply = nil } }
}

private actor VisibilityTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    private var output = Data()
    init() throws {
        identity = try .init(workspaceID: "workspace", machineID: "machine", routeID: "route",
                             routeRevision: 1, transportID: UUID(), channelID: UUID())
    }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        output += bytes; return .accepted(bytes.count)
    }
    func close() async {}
    func bytes() -> Data { output }
}

private actor VisibilityDelivery: TerminalModelDelivery {
    private var frames: [Data] = []
    private var outcomes = 0
    func present(_ presentation: TerminalModelPresentation, source: TerminalSource) async throws {
        if case let .frame(bytes) = presentation { frames.append(bytes) }
    }
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) async throws {
        self.outcomes += outcomes.count
    }
    func values() -> [Data] { frames }
    func outcomeCount() -> Int { outcomes }
}

private actor VisibilityTransactionGate {
    private var parked: CheckedContinuation<Void, Never>?
    private var arrived: CheckedContinuation<Void, Never>?
    func park() async {
        await withCheckedContinuation { continuation in
            parked = continuation
            let ready = arrived; arrived = nil; ready?.resume()
        }
    }
    func waitForArrival() async {
        if parked != nil { return }
        await withCheckedContinuation { arrived = $0 }
    }
    func release() { let old = parked; parked = nil; old?.resume() }
}

@MainActor
final class TerminalModelVisibilityTests: XCTestCase {
    private struct Fixture {
        let coordinator: TerminalInputCoordinator
        let transport: VisibilityTransport
        let source: TerminalSource
        let model: VisibilityModel
        let delivery: VisibilityDelivery
        let runtime: TerminalModelRuntime
    }
    private func fixture(visible: Bool = false,
                         hiddenOutput: (@Sendable () -> Void)? = nil) async throws -> Fixture {
        let coordinator = try TerminalInputCoordinator(), transport = try VisibilityTransport()
        let source = try await coordinator.attach(transport)
        let model = VisibilityModel(source: source), delivery = VisibilityDelivery()
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: delivery,
                                           initiallyVisible: visible, hiddenOutput: hiddenOutput)
        return .init(coordinator: coordinator, transport: transport, source: source,
                     model: model, delivery: delivery, runtime: runtime)
    }
    private func awaitPresentationWaiter(_ f: Fixture) async -> Bool {
        for _ in 0..<10_000 {
            if await f.runtime.accounting().waitingPresentationRequests == 1 { return true }
            await Task.yield()
        }
        XCTFail("Presentation never reached its bounded waiter")
        return false
    }
    private func block(_ f: Fixture, gate: VisibilityTransactionGate) async -> Task<Void, Error> {
        let task = Task { try await f.runtime.withInputTransaction(source: f.source) { await gate.park() } }
        await gate.waitForArrival()
        return task
    }
    private func cleanup(_ f: Fixture) async throws {
        try await f.runtime.close()
        try await f.coordinator.disconnect(f.source)
        while !(try await f.coordinator.takeOutcomes(f.source, maximum: 256)).isEmpty {}
        try await f.coordinator.retire(f.source)
    }

    func testHiddenOutputAndAnswerbacksContinueWithoutAnySnapshots() async throws {
        let f = try await fixture()
        for _ in 0..<200 { try await f.runtime.receive(.init(output: Data([1])), source: f.source) }
        XCTAssertEqual(f.model.counts().0, 200); XCTAssertEqual(f.model.counts().1, 0)
        let before = await f.delivery.values(); XCTAssertTrue(before.isEmpty)
        while try await f.coordinator.drainOne(f.source) != .idle {}
        let replies = await f.transport.bytes()
        XCTAssertEqual(replies, Data(repeating: 0x52, count: 200))
        let accounting = await f.runtime.accounting()
        XCTAssertEqual(accounting.queuedReplies, 200); XCTAssertEqual(accounting.droppedReplies, 0)
        try await f.runtime.setVisible(true, source: f.source)
        let after = await f.delivery.values()
        XCTAssertEqual(after, [Data("opaque-frame-200".utf8)])
        try await cleanup(f)
    }

    func testOutputWhileHiddenAfterBeingShownIsReportedOnceUntilShownAgain() async throws {
        final class Reports: @unchecked Sendable { let lock = NSLock(); var count = 0 }
        let reports = Reports()
        let f = try await fixture(hiddenOutput: { reports.lock.withLock { reports.count += 1 } })
        func reported() -> Int { reports.lock.withLock { reports.count } }
        try await f.runtime.receive(.init(output: Data([1])), source: f.source)
        XCTAssertEqual(reported(), 0, "a terminal never on screen (a new shell's greeting) has no news")
        try await f.runtime.setVisible(true, source: f.source)
        try await f.runtime.receive(.init(output: Data([1])), source: f.source)
        XCTAssertEqual(reported(), 0, "output on screen is seen")
        try await f.runtime.setVisible(false, source: f.source)
        try await f.runtime.receive(.init(output: Data()), source: f.source)
        XCTAssertEqual(reported(), 0, "an update without output is not news")
        try await f.runtime.receive(.init(output: Data([1])), source: f.source)
        try await f.runtime.receive(.init(output: Data([1])), source: f.source)
        XCTAssertEqual(reported(), 1, "reported once while it stays hidden")
        try await f.runtime.setVisible(true, source: f.source)
        try await f.runtime.setVisible(false, source: f.source)
        try await f.runtime.receive(.init(output: Data([1])), source: f.source)
        XCTAssertEqual(reported(), 2, "shown again, later output is news again")
        try await cleanup(f)
    }

    func testReactivationWaitsForCanonicalTransactionInsteadOfFailingBusy() async throws {
        let f = try await fixture(), gate = VisibilityTransactionGate()
        let blocker = await block(f, gate: gate)
        let show = Task { try await f.runtime.setVisible(true, source: f.source) }
        let waiting = await awaitPresentationWaiter(f)
        XCTAssertTrue(waiting); XCTAssertEqual(f.model.counts().1, 0)
        await gate.release(); try await blocker.value; try await show.value
        XCTAssertEqual(f.model.counts().1, 1)
        try await cleanup(f)
    }

    func testHideInvalidatesWaitingShowWithoutWaitingForCanonicalTransaction() async throws {
        let f = try await fixture(), gate = VisibilityTransactionGate()
        let blocker = await block(f, gate: gate)
        let show = Task { try await f.runtime.setVisible(true, source: f.source) }
        _ = await awaitPresentationWaiter(f)
        try await f.runtime.setVisible(false, source: f.source)
        await gate.release(); try await blocker.value; try await show.value
        XCTAssertEqual(f.model.counts().1, 0)
        try await f.runtime.receive(.init(output: Data([1])), source: f.source)
        XCTAssertEqual(f.model.counts().0, 1); XCTAssertEqual(f.model.counts().1, 0)
        try await cleanup(f)
    }

    func testRefusedSecondShowDoesNotInvalidateAlreadyAdmittedRequest() async throws {
        let f = try await fixture(), gate = VisibilityTransactionGate()
        let blocker = await block(f, gate: gate)
        let show = Task { try await f.runtime.setVisible(true, source: f.source) }
        _ = await awaitPresentationWaiter(f)
        do { try await f.runtime.setVisible(true, source: f.source); XCTFail("second waiter admitted") }
        catch { XCTAssertEqual(error as? TerminalModelError, .busy) }
        let accounting = await f.runtime.accounting()
        XCTAssertEqual(accounting.waitingPresentationRequests, 1)
        await gate.release(); try await blocker.value; try await show.value
        XCTAssertEqual(f.model.counts().1, 1)
        try await cleanup(f)
    }

    func testCancelledWaitingShowReleasesOperationAndRemainsHidden() async throws {
        let f = try await fixture(), gate = VisibilityTransactionGate()
        let blocker = await block(f, gate: gate)
        let show = Task { try await f.runtime.setVisible(true, source: f.source) }
        _ = await awaitPresentationWaiter(f); show.cancel()
        await gate.release(); try await blocker.value
        do { try await show.value; XCTFail("cancelled show published") } catch is CancellationError {}
        XCTAssertEqual(f.model.counts().1, 0)
        try await f.runtime.receive(.init(output: Data([1])), source: f.source)
        XCTAssertEqual(f.model.counts().1, 0)
        try await cleanup(f)
    }

    func testCloseRetiresVisibilityWaiterAndJoinsExistingTransaction() async throws {
        let f = try await fixture(), gate = VisibilityTransactionGate()
        let blocker = await block(f, gate: gate)
        let show = Task { try await f.runtime.setVisible(true, source: f.source) }
        _ = await awaitPresentationWaiter(f)
        await f.runtime.requestClose()
        do { try await show.value; XCTFail("closed show published") }
        catch { XCTAssertEqual(error as? TerminalModelError, .closed) }
        XCTAssertEqual(f.model.counts().2, 0)
        await gate.release(); try await blocker.value
        try await f.runtime.close()
        XCTAssertEqual(f.model.counts().1, 0); XCTAssertEqual(f.model.counts().2, 1)
        try await cleanup(f)
        XCTAssertEqual(f.model.counts().2, 1)
    }

    func testHiddenOutcomesStillReachOwner() async throws {
        let f = try await fixture()
        let empty = try await f.coordinator.makeStream(sceneID: UUID())
        let focus = try await f.coordinator.changeFocus(empty, to: f.source)
        _ = try await f.coordinator.submit(focus, encodedBytes: Data([0x4B]))
        _ = try await f.coordinator.drainOne(f.source)
        let batch = try await f.coordinator.pendingOutcomes(f.source)
        try await f.runtime.receive(batch.outcomes, source: f.source)
        try await f.coordinator.acknowledgeOutcomes(batch)
        let count = await f.delivery.outcomeCount()
        XCTAssertEqual(count, 1); XCTAssertEqual(f.model.counts().1, 0)
        _ = try await f.coordinator.suspend(focus)
        try await cleanup(f)
    }

    func testCoalescedSceneOwnerReturnsToLatestOutputThenRetires() async throws {
        let f = try await fixture()
        let visibility = TerminalPresentationVisibility(source: f.source, initiallyVisible: false) { visible, source in
            try await f.runtime.setVisible(visible, source: source)
        }
        for index in 0..<100 {
            try visibility.request(index.isMultiple(of: 2), source: f.source)
            await visibility.waitUntilSettled()
            try await f.runtime.receive(.init(output: Data([1])), source: f.source)
        }
        let before = f.model.counts().1
        visibility.retire(); await visibility.waitUntilSettled()
        for _ in 0..<100 { try await f.runtime.receive(.init(output: Data([1])), source: f.source) }
        XCTAssertEqual(f.model.counts().0, 200); XCTAssertEqual(f.model.counts().1, before)
        XCTAssertEqual(visibility.appliedVisible, false)
        try await cleanup(f)
    }

    func testDefaultRuntimeVisibilityPreservesExistingConsumers() async throws {
        let f = try await fixture(visible: true)
        try await f.runtime.receive(.init(output: Data([1])), source: f.source)
        XCTAssertEqual(f.model.counts().1, 1)
        try await f.runtime.setVisible(false, source: f.source)
        try await f.runtime.receive(.init(output: Data([2])), source: f.source)
        XCTAssertEqual(f.model.counts().1, 1)
        try await cleanup(f)
    }
}
