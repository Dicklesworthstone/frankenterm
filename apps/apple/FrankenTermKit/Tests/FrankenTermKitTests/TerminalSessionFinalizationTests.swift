import Foundation
import XCTest
@testable import FrankenTermKit

private enum FinalizationFailure: Error { case delivery }

private actor FinalizationGate {
    private var entered = false
    private var entry: CheckedContinuation<Void, Never>?
    private var paused: CheckedContinuation<Void, Never>?
    func pause() async {
        entered = true; entry?.resume(); entry = nil
        await withCheckedContinuation { paused = $0 }
    }
    func waitUntilEntered() async {
        if !entered { await withCheckedContinuation { entry = $0 } }
    }
    func release() { paused?.resume(); paused = nil }
}

private actor FinalizationTransport: TerminalSessionTransport {
    nonisolated let identity: TerminalConnectionIdentity
    nonisolated let inputSignal = TerminalSessionSignal()
    private var updates: [TerminalSessionUpdate] = []
    private var waiter: CheckedContinuation<TerminalSessionUpdate, Never>?
    private var closed = false
    private var writes = Data()
    private var closeCount = 0
    private var pendingResize: TerminalSessionGeometry?
    private var requestedResizes: [TerminalSessionGeometry] = []
    init() throws {
        identity = try .init(workspaceID: "workspace", machineID: "machine", routeID: "route",
                             routeRevision: 1, transportID: UUID(), channelID: UUID())
    }
    nonisolated func inputAvailable() { inputSignal.notify() }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        guard !closed else { return .failed }
        writes += bytes
        return .accepted(bytes.count)
    }
    func service() async throws -> TerminalSessionUpdate {
        if closed { return .init(output: Data(), endOfOutput: true) }
        if !updates.isEmpty { return updates.removeFirst() }
        return await withCheckedContinuation { waiter = $0 }
    }
    func wakeService() { enqueue(.init(output: Data())) }
    func enqueue(_ update: TerminalSessionUpdate) {
        if let waiter { self.waiter = nil; waiter.resume(returning: update) }
        else { updates.append(update) }
    }
    func close() {
        if !closed { closed = true; closeCount += 1 }
        if let waiter { self.waiter = nil; waiter.resume(returning: .init(output: Data(), endOfOutput: true)) }
    }
    func bytes() -> Data { writes }
    func closes() -> Int { closeCount }
    func requestResize(_ geometry: TerminalSessionGeometry) -> TerminalResizeAdmission {
        guard pendingResize == nil else { return .busy }
        pendingResize = geometry; requestedResizes.append(geometry)
        return .queued
    }
    func completeResize() {
        guard let geometry = pendingResize else { return }
        pendingResize = nil
        enqueue(.init(output: Data("resized".utf8), resized: geometry))
    }
    func resizes() -> [TerminalSessionGeometry] { requestedResizes }
}

private actor FinalizationConsumer: TerminalSessionConsumer {
    private let updateGate: FinalizationGate?
    private let rejectUpdates: Bool, rejectOutcomes: Bool
    private var events: [String] = []
    private var outcomes: [TerminalInputOutcome] = []
    private var resizeWaiter: CheckedContinuation<Void, Never>?
    private var resizeCount = 0, awaitedResizeCount = 0
    init(updateGate: FinalizationGate? = nil, rejectUpdates: Bool = false, rejectOutcomes: Bool = false) {
        self.updateGate = updateGate; self.rejectUpdates = rejectUpdates; self.rejectOutcomes = rejectOutcomes
    }
    func receive(_ update: TerminalSessionUpdate, source: TerminalSource) async throws {
        if let updateGate { await updateGate.pause() }
        if rejectUpdates { throw FinalizationFailure.delivery }
        events.append("consumed:\(String(decoding: update.output, as: UTF8.self))")
    }
    func receive(_ values: [TerminalInputOutcome], source: TerminalSource) async throws {
        if rejectOutcomes { throw FinalizationFailure.delivery }
        outcomes += values
    }
    func resizeApplied(_ geometry: TerminalSessionGeometry, source: TerminalSource) {
        events.append("applied:\(geometry.columns)x\(geometry.rows)")
        resizeCount += 1
        if resizeCount >= awaitedResizeCount { resizeWaiter?.resume(); resizeWaiter = nil }
    }
    func waitForResizeCount(_ count: Int) async {
        guard resizeCount < count else { return }
        awaitedResizeCount = count
        await withCheckedContinuation { resizeWaiter = $0 }
    }
    func log() -> [String] { events }
    func records() -> [TerminalInputOutcome] { outcomes }
}

private actor FinalizationLedger {
    private var batches: [[TerminalInputOutcome]] = []
    private var calls = 0
    private var failureCall: Int?
    init(failureCall: Int? = nil) { self.failureCall = failureCall }
    func deliver(_ records: [TerminalInputOutcome], source: TerminalSource) throws {
        calls += 1
        guard calls != failureCall else { throw FinalizationFailure.delivery }
        guard records.allSatisfy({ $0.receipt.source == source }) else { throw TerminalInputError.stale }
        batches.append(records)
    }
    func snapshot() -> [[TerminalInputOutcome]] { batches }
}

@MainActor
final class TerminalSessionFinalizationTests: XCTestCase {
    private func setup(count: Int = 0) async throws -> (TerminalInputCoordinator, FinalizationTransport, TerminalSessionPump, TerminalFocus) {
        let coordinator = try TerminalInputCoordinator(), transport = try FinalizationTransport()
        let pump = try await TerminalSessionPump.attach(transport, coordinator: coordinator)
        let empty = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(empty, to: pump.source)
        for _ in 0..<count { _ = try await coordinator.submit(focus, encodedBytes: Data([42])) }
        return (coordinator, transport, pump, focus)
    }
    private func retire(_ coordinator: TerminalInputCoordinator, _ pump: TerminalSessionPump, _ focus: TerminalFocus) async throws {
        _ = try await coordinator.suspend(focus)
        try await coordinator.retire(pump.source)
        let accounting = await coordinator.accounting()
        XCTAssertEqual(accounting.connections, 0)
        XCTAssertEqual(accounting.streams, 0)
        XCTAssertEqual(accounting.retainedRecords, 0)
        XCTAssertEqual(accounting.retainedBytes, 0)
    }

    func testStoppedBeforeRunDrainsEveryBoundedBatchAndReleasesSource() async throws {
        let (coordinator, transport, pump, focus) = try await setup(count: 130)
        pump.requestStop()
        let ledger = FinalizationLedger()
        let delivered = try await pump.reconcileStoppedOutcomes { try await ledger.deliver($0, source: $1) }
        let batches = await ledger.snapshot(), bytes = await transport.bytes()
        XCTAssertEqual(delivered, 130)
        XCTAssertEqual(batches.map(\.count), [64, 64, 2])
        XCTAssertEqual(Set(batches.flatMap { $0.map(\.receipt) }).count, 130)
        XCTAssertTrue(batches.flatMap { $0 }.allSatisfy { $0.disposition == .definitelyNotWritten })
        XCTAssertTrue(bytes.isEmpty)
        try await retire(coordinator, pump, focus)
    }

    func testFailedSecondDeliveryRetainsExactPrefixWithoutRepeatingFirst() async throws {
        let (coordinator, _, pump, focus) = try await setup(count: 130)
        pump.requestStop()
        let ledger = FinalizationLedger(failureCall: 2)
        do { _ = try await pump.reconcileStoppedOutcomes { try await ledger.deliver($0, source: $1) }; XCTFail() }
        catch FinalizationFailure.delivery {}
        let pending = try await coordinator.pendingOutcomes(pump.source)
        XCTAssertEqual(pending.outcomes.count, 64)
        let retained = await coordinator.accounting()
        XCTAssertEqual(retained.retainedRecords, 66)
        let delivered = try await pump.reconcileStoppedOutcomes { try await ledger.deliver($0, source: $1) }
        let batches = await ledger.snapshot()
        XCTAssertEqual(delivered, 66)
        XCTAssertEqual(batches.map(\.count), [64, 64, 2])
        XCTAssertEqual(batches[1], pending.outcomes)
        XCTAssertEqual(Set(batches.flatMap { $0.map(\.receipt) }).count, 130)
        try await retire(coordinator, pump, focus)
    }

    func testReconciliationRefusesLivePumpAndCannotStartAnotherRun() async throws {
        let (coordinator, transport, pump, focus) = try await setup()
        let gate = FinalizationGate(), consumer = FinalizationConsumer(updateGate: gate)
        await transport.enqueue(.init(output: Data([1])))
        let task = Task { try await pump.run(consumer: consumer) }
        await gate.waitUntilEntered()
        pump.requestStop()
        do { _ = try await pump.reconcileStoppedOutcomes { _, _ in XCTFail() }; XCTFail() }
        catch TerminalInputError.unavailable {}
        await gate.release()
        _ = try await task.value
        _ = try await pump.reconcileStoppedOutcomes { _, _ in XCTFail() }
        do { _ = try await pump.run(consumer: consumer); XCTFail() }
        catch TerminalInputError.unavailable {}
        try await retire(coordinator, pump, focus)
    }

    func testConcurrentReconciliationIsRejectedAndMetadataNotDuplicated() async throws {
        let (coordinator, _, pump, focus) = try await setup(count: 3)
        pump.requestStop()
        let gate = FinalizationGate(), ledger = FinalizationLedger()
        let task = Task {
            try await pump.reconcileStoppedOutcomes { records, source in
                await gate.pause(); try await ledger.deliver(records, source: source)
            }
        }
        await gate.waitUntilEntered()
        do { _ = try await pump.reconcileStoppedOutcomes { _, _ in XCTFail() }; XCTFail() }
        catch TerminalInputError.unavailable {}
        do { _ = try await pump.run(consumer: FinalizationConsumer()); XCTFail() }
        catch TerminalInputError.unavailable {}
        await gate.release()
        let count = try await task.value
        XCTAssertEqual(count, 3)
        let repeated = try await pump.reconcileStoppedOutcomes { _, _ in XCTFail() }
        XCTAssertEqual(repeated, 0)
        try await retire(coordinator, pump, focus)
    }

    func testCancellationAfterSuccessfulDeliveryStillAcknowledgesItsBatch() async throws {
        let (coordinator, _, pump, focus) = try await setup(count: 3)
        pump.requestStop()
        let gate = FinalizationGate(), ledger = FinalizationLedger()
        let task = Task {
            try await pump.reconcileStoppedOutcomes { records, source in
                await gate.pause(); try await ledger.deliver(records, source: source)
            }
        }
        await gate.waitUntilEntered(); task.cancel(); await gate.release()
        do { _ = try await task.value; XCTFail() } catch is CancellationError {}
        let pending = try await coordinator.pendingOutcomes(pump.source)
        XCTAssertTrue(pending.outcomes.isEmpty)
        let batches = await ledger.snapshot()
        XCTAssertEqual(batches.map(\.count), [3])
        try await retire(coordinator, pump, focus)
    }

    func testThrowingCancelledDeliveryDoesNotAcknowledge() async throws {
        let (coordinator, _, pump, focus) = try await setup(count: 3)
        pump.requestStop()
        let gate = FinalizationGate()
        let task = Task {
            try await pump.reconcileStoppedOutcomes { _, _ in
                await gate.pause(); try Task.checkCancellation()
            }
        }
        await gate.waitUntilEntered(); task.cancel(); await gate.release()
        do { _ = try await task.value; XCTFail() } catch is CancellationError {}
        let pending = try await coordinator.pendingOutcomes(pump.source)
        XCTAssertEqual(pending.outcomes.count, 3)
        let ledger = FinalizationLedger()
        _ = try await pump.reconcileStoppedOutcomes { try await ledger.deliver($0, source: $1) }
        try await retire(coordinator, pump, focus)
    }

    func testRejectedNormalConsumerLeavesTransportAcceptanceForFinalDelivery() async throws {
        let (coordinator, transport, pump, focus) = try await setup(count: 3)
        let end = try await pump.run(consumer: FinalizationConsumer(rejectOutcomes: true))
        XCTAssertEqual(end, .consumerFailed)
        let ledger = FinalizationLedger()
        let count = try await pump.reconcileStoppedOutcomes { try await ledger.deliver($0, source: $1) }
        let records = await ledger.snapshot().flatMap { $0 }, bytes = await transport.bytes()
        XCTAssertEqual(count, 3)
        XCTAssertEqual(bytes, Data([42, 42, 42]))
        XCTAssertTrue(records.allSatisfy { $0.disposition == .transportAccepted })
        try await retire(coordinator, pump, focus)
    }

    func testRepeatedInterruptedSessionsDoNotExhaustConnectionSlots() async throws {
        let coordinator = try TerminalInputCoordinator()
        for _ in 0..<80 {
            let transport = try FinalizationTransport()
            let pump = try await TerminalSessionPump.attach(transport, coordinator: coordinator)
            let empty = try await coordinator.makeStream(sceneID: UUID())
            let focus = try await coordinator.changeFocus(empty, to: pump.source)
            _ = try await coordinator.submit(focus, encodedBytes: Data("unsubmitted".utf8))
            _ = try await coordinator.cleanup(focus, encodedBytes: Data([1]))
            pump.requestStop()
            let ledger = FinalizationLedger()
            let count = try await pump.reconcileStoppedOutcomes { try await ledger.deliver($0, source: $1) }
            XCTAssertEqual(count, 2)
            try await retire(coordinator, pump, focus)
        }
    }

    func testResizeNotificationWaitsForCompleteCanonicalConsumption() async throws {
        let (coordinator, transport, pump, focus) = try await setup()
        let gate = FinalizationGate(), consumer = FinalizationConsumer(updateGate: gate)
        let geometry = try TerminalSessionGeometry(columns: 120, rows: 32)
        await transport.enqueue(.init(output: Data("final".utf8), endOfOutput: true, resized: geometry))
        let task = Task { try await pump.run(consumer: consumer) { trySourceGeometry, source in
            XCTAssertEqual(source, pump.source)
            await consumer.resizeApplied(trySourceGeometry, source: source)
        } }
        await gate.waitUntilEntered()
        let before = await consumer.log()
        XCTAssertTrue(before.isEmpty)
        await gate.release()
        let end = try await task.value, after = await consumer.log()
        XCTAssertEqual(end, .peerOutputEnded)
        XCTAssertEqual(after, ["consumed:final", "applied:120x32"])
        _ = try await pump.reconcileStoppedOutcomes { _, _ in XCTFail() }
        try await retire(coordinator, pump, focus)
    }

    func testRejectedModelUpdateDoesNotPublishAppliedGeometry() async throws {
        let (coordinator, transport, pump, focus) = try await setup()
        await transport.enqueue(.init(output: Data(), resized: try .init(columns: 120, rows: 32)))
        let end = try await pump.run(consumer: FinalizationConsumer(rejectUpdates: true)) { _, _ in XCTFail() }
        XCTAssertEqual(end, .consumerFailed)
        _ = try await pump.reconcileStoppedOutcomes { _, _ in XCTFail() }
        try await retire(coordinator, pump, focus)
    }

    func testStopDuringConsumptionDoesNotReviveRetiredResizeOwner() async throws {
        let (coordinator, transport, pump, focus) = try await setup()
        let gate = FinalizationGate(), consumer = FinalizationConsumer(updateGate: gate)
        await transport.enqueue(.init(output: Data(), resized: try .init(columns: 120, rows: 32)))
        let task = Task { try await pump.run(consumer: consumer) { _, _ in XCTFail() } }
        await gate.waitUntilEntered(); pump.requestStop(); await gate.release()
        let end = try await task.value
        XCTAssertEqual(end, .cancelled)
        _ = try await pump.reconcileStoppedOutcomes { _, _ in XCTFail() }
        try await retire(coordinator, pump, focus)
    }

    func testResizeObserverFailureClosesWithoutReplayingModelUpdate() async throws {
        let (coordinator, transport, pump, focus) = try await setup()
        let consumer = FinalizationConsumer()
        await transport.enqueue(.init(output: Data("once".utf8), resized: try .init(columns: 120, rows: 32)))
        let end = try await pump.run(consumer: consumer) { _, _ in throw FinalizationFailure.delivery }
        let events = await consumer.log()
        XCTAssertEqual(end, .consumerFailed)
        XCTAssertEqual(events, ["consumed:once"])
        _ = try await pump.reconcileStoppedOutcomes { _, _ in XCTFail() }
        try await retire(coordinator, pump, focus)
    }

    func testLivePumpDrivesLatestResizeWithoutPollingOrOptimisticGeometry() async throws {
        let (coordinator, transport, pump, focus) = try await setup()
        let initial = try TerminalSessionGeometry(columns: 80, rows: 24)
        let first = try TerminalSessionGeometry(columns: 100, rows: 25)
        let last = try TerminalSessionGeometry(columns: 140, rows: 30)
        let resize = TerminalViewportResize(source: pump.source, initial: initial) { await transport.requestResize($0) }
        let consumer = FinalizationConsumer()
        try resize.propose(first, source: pump.source)
        await resize.waitUntilSettled()
        for columns in 101...140 {
            try resize.propose(.init(columns: UInt32(columns), rows: 30), source: pump.source)
        }
        let before = await transport.resizes()
        XCTAssertEqual(before, [first])
        XCTAssertEqual(resize.applied, initial)
        let task = Task { try await pump.run(consumer: consumer) { geometry, source in
            try await resize.didApply(geometry, source: source)
            await consumer.resizeApplied(geometry, source: source)
        } }
        await transport.completeResize()
        await consumer.waitForResizeCount(1)
        await resize.waitUntilSettled()
        let requests = await transport.resizes()
        XCTAssertEqual(requests, [first, last])
        XCTAssertEqual(resize.applied, first)
        await transport.completeResize()
        await consumer.waitForResizeCount(2)
        XCTAssertEqual(resize.applied, last)
        XCTAssertFalse(resize.isPending)
        resize.retire(); pump.requestStop()
        _ = try await task.value
        _ = try await pump.reconcileStoppedOutcomes { _, _ in XCTFail() }
        try await retire(coordinator, pump, focus)
    }
}
