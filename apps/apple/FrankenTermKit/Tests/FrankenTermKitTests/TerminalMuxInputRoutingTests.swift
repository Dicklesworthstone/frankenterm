import Foundation
import XCTest
@testable import FrankenTermKit

final class TerminalMuxInputRoutingTests: XCTestCase {
    func testNewVisitWaitsForAppliedOutputAndRoundTripCannotReviveOldAdmission() throws {
        let routing = TerminalMuxInputRouting()
        try routing.replaceInventory([0, 1])
        let first = try routing.select(pane: 0)
        XCTAssertEqual(try routing.capture(), first)
        XCTAssertThrowsError(try routing.validateAdmission(first))
        routing.didApplyOutput(first)
        XCTAssertNoThrow(try routing.validateAdmission(first))
        let second = try routing.select(pane: 1)
        XCTAssertThrowsError(try routing.validateAdmission(first))
        XCTAssertNoThrow(try routing.validateDelivery(first), "old queued bytes retain their live destination")
        routing.didApplyOutput(first)
        XCTAssertFalse(routing.current.ready)
        routing.didApplyOutput(second)
        XCTAssertNoThrow(try routing.validateAdmission(second))
        let again = try routing.select(pane: 0)
        XCTAssertNotEqual(again, first)
        routing.didApplyOutput(first)
        XCTAssertFalse(routing.current.ready)
        routing.didApplyOutput(again)
        XCTAssertThrowsError(try routing.validateAdmission(first))
        XCTAssertNoThrow(try routing.validateDelivery(first))
    }

    func testObservedRemovalAndReusedNumberRetireEveryOldWriter() throws {
        let routing = TerminalMuxInputRouting()
        try routing.replaceInventory([4, 5])
        let old = try routing.select(pane: 4)
        routing.didApplyOutput(old)
        routing.remove(pane: 4)
        XCTAssertNil(routing.current.destination)
        XCTAssertThrowsError(try routing.capture())
        XCTAssertThrowsError(try routing.validateDelivery(old))
        try routing.replaceInventory([4, 5])
        XCTAssertNil(routing.current.destination, "inventory cannot silently choose a replacement")
        let replacement = try routing.select(pane: 4)
        routing.didApplyOutput(replacement)
        XCTAssertThrowsError(try routing.validateDelivery(old))
        XCTAssertNoThrow(try routing.validateDelivery(replacement))
    }

    func testInventoryReorderingPreservesRegistrationButMissingPaneDoesNot() throws {
        let routing = TerminalMuxInputRouting()
        try routing.replaceInventory([1, 2, 3])
        let first = try routing.select(pane: 1)
        routing.didApplyOutput(first)
        let original = routing.current
        try routing.replaceInventory([3, 1, 2])
        XCTAssertEqual(routing.current, original)
        XCTAssertEqual(try routing.select(pane: 1), first)
        try routing.replaceInventory([2, 3])
        XCTAssertNil(routing.current.destination)
        XCTAssertThrowsError(try routing.validateDelivery(first))
    }

    func testForeignConnectionAndRetiredConnectionNeverAdmitDelivery() throws {
        let first = TerminalMuxInputRouting(), second = TerminalMuxInputRouting()
        try first.replaceInventory([1]); try second.replaceInventory([1])
        let destination = try first.select(pane: 1)
        _ = try second.select(pane: 1)
        second.didApplyOutput(destination)
        XCTAssertFalse(second.current.ready)
        XCTAssertThrowsError(try second.validateDelivery(destination))
        first.retire(); first.retire(); first.didApplyOutput(destination)
        XCTAssertTrue(first.current.retired); XCTAssertFalse(first.current.ready)
        XCTAssertThrowsError(try first.capture())
        XCTAssertThrowsError(try first.validateDelivery(destination))
        XCTAssertThrowsError(try first.replaceInventory([1]))
        XCTAssertThrowsError(try first.select(pane: 1))
    }

    func testInvalidInventoriesLeaveCurrentAdmissionUntouched() throws {
        let routing = TerminalMuxInputRouting()
        try routing.replaceInventory([1])
        let first = try routing.select(pane: 1)
        routing.didApplyOutput(first)
        let state = routing.current
        for values in [[1, 1], [-1], Array(0...4096)] {
            XCTAssertThrowsError(try routing.replaceInventory(values))
            XCTAssertEqual(routing.current, state)
            XCTAssertNoThrow(try routing.validateAdmission(first))
        }
        XCTAssertThrowsError(try routing.select(pane: 9))
        XCTAssertEqual(routing.current, state)
    }

    func testObserversCanReadAndReenterWithoutDeadlockingOrReopeningAnOldVisit() throws {
        final class Seen: @unchecked Sendable {
            let lock = NSLock()
            var states: [TerminalMuxInputRouting.State] = []
        }
        let routing = TerminalMuxInputRouting(), seen = Seen()
        try routing.replaceInventory([1, 2])
        routing.observe { state in
            _ = routing.current
            seen.lock.withLock { seen.states.append(state) }
            if state.ready, state.destination?.paneID == 1 { _ = try? routing.select(pane: 2) }
        }
        let first = try routing.select(pane: 1)
        routing.didApplyOutput(first)
        XCTAssertEqual(routing.current.destination?.paneID, 2)
        XCTAssertFalse(routing.current.ready)
        XCTAssertThrowsError(try routing.validateAdmission(first))
        let revisions = seen.lock.withLock { seen.states.map(\.revision) }
        XCTAssertEqual(revisions, revisions.sorted())
        XCTAssertEqual(Set(revisions).count, revisions.count)
        routing.removeObserver()
        let count = seen.lock.withLock { seen.states.count }
        routing.retire()
        XCTAssertEqual(seen.lock.withLock { seen.states.count }, count)
    }

    func testConcurrentSelectionAndOldOutputCannotExceedTheCurrentVisit() throws {
        let routing = TerminalMuxInputRouting()
        try routing.replaceInventory(Array(0..<64))
        DispatchQueue.concurrentPerform(iterations: 2000) { index in
            if let ticket = try? routing.select(pane: index % 64) {
                routing.didApplyOutput(ticket)
                _ = try? routing.validateDelivery(ticket)
            }
        }
        let current = try routing.capture()
        routing.didApplyOutput(current)
        XCTAssertNoThrow(try routing.validateAdmission(current))
        XCTAssertLessThanOrEqual(routing.current.revision, 4001)
    }
}

private actor MuxOutputConsumer: TerminalSessionConsumer {
    private var release: CheckedContinuation<Void, Never>?
    private(set) var waiting = false
    var failure = false
    func receive(_ update: TerminalSessionUpdate, source: TerminalSource) async throws {
        waiting = true
        await withCheckedContinuation { release = $0 }
        waiting = false
        if failure { throw TerminalInputError.unavailable }
    }
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) async throws {}
    func finish(failing: Bool = false) {
        failure = failing
        let pending = release; release = nil; pending?.resume()
    }
}

private actor MuxOutputTransport: TerminalSessionTransport {
    nonisolated let identity: TerminalConnectionIdentity
    nonisolated let inputSignal = TerminalSessionSignal()
    nonisolated let routing = TerminalMuxInputRouting()
    private var update: TerminalSessionUpdate?
    private var idle: CheckedContinuation<TerminalSessionUpdate, Error>?
    private(set) var applications = 0
    init() throws {
        identity = try .init(workspaceID: "workspace", machineID: "machine", routeID: "route", routeRevision: 1,
                             transportID: UUID(), channelID: UUID())
        try routing.replaceInventory([1, 2])
        let destination = try routing.select(pane: 1)
        update = .init(output: Data("complete redraw".utf8), muxDestination: destination)
    }
    func service() async throws -> TerminalSessionUpdate {
        if let update { self.update = nil; return update }
        return try await withCheckedThrowingContinuation { idle = $0 }
    }
    func wakeService() async { let pending = idle; idle = nil; pending?.resume(returning: .init(output: Data())) }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite { .accepted(bytes.count) }
    func didApplyOutput(_ update: TerminalSessionUpdate) async {
        applications += 1
        if let destination = update.muxDestination { routing.didApplyOutput(destination) }
    }
    func close() async {
        routing.retire(); inputSignal.finish()
        let pending = idle; idle = nil; pending?.resume(throwing: TerminalInputError.unavailable)
    }
}

final class TerminalMuxOutputFenceTests: XCTestCase, @unchecked Sendable {
    func testReadinessWaitsForActualConsumerSuccess() async throws {
        let transport = try MuxOutputTransport(), coordinator = try TerminalInputCoordinator()
        let consumer = MuxOutputConsumer()
        let pump = try await TerminalSessionPump.attach(transport, coordinator: coordinator)
        let run = Task { try await pump.run(consumer: consumer) }
        for _ in 0..<10_000 { if await consumer.waiting { break }; await Task.yield() }
        let waiting = await consumer.waiting
        XCTAssertTrue(waiting)
        XCTAssertFalse(transport.routing.current.ready)
        let before = await transport.applications
        XCTAssertEqual(before, 0)
        await consumer.finish()
        for _ in 0..<10_000 { if transport.routing.current.ready { break }; await Task.yield() }
        XCTAssertTrue(transport.routing.current.ready)
        pump.requestStop(); _ = try await run.value
    }

    func testConsumerFailureNeverEnablesInput() async throws {
        let transport = try MuxOutputTransport(), coordinator = try TerminalInputCoordinator()
        let consumer = MuxOutputConsumer()
        let pump = try await TerminalSessionPump.attach(transport, coordinator: coordinator)
        let run = Task { try await pump.run(consumer: consumer) }
        for _ in 0..<10_000 { if await consumer.waiting { break }; await Task.yield() }
        await consumer.finish(failing: true)
        let result = try await run.value, applied = await transport.applications
        XCTAssertEqual(result, .consumerFailed); XCTAssertEqual(applied, 0)
        XCTAssertFalse(transport.routing.current.ready)
    }

    func testCancellationDuringConsumerDoesNotAcknowledgeItsLateResult() async throws {
        let transport = try MuxOutputTransport(), coordinator = try TerminalInputCoordinator()
        let consumer = MuxOutputConsumer()
        let pump = try await TerminalSessionPump.attach(transport, coordinator: coordinator)
        let run = Task { try await pump.run(consumer: consumer) }
        for _ in 0..<10_000 { if await consumer.waiting { break }; await Task.yield() }
        pump.requestStop()
        await consumer.finish()
        _ = try await run.value
        let applied = await transport.applications
        XCTAssertEqual(applied, 0)
        XCTAssertFalse(transport.routing.current.ready)
    }

    func testOldConsumedOutputCannotEnableANewerPane() async throws {
        let transport = try MuxOutputTransport(), coordinator = try TerminalInputCoordinator()
        let consumer = MuxOutputConsumer()
        let pump = try await TerminalSessionPump.attach(transport, coordinator: coordinator)
        let run = Task { try await pump.run(consumer: consumer) }
        for _ in 0..<10_000 { if await consumer.waiting { break }; await Task.yield() }
        _ = try transport.routing.select(pane: 2)
        await consumer.finish()
        for _ in 0..<10_000 { if await transport.applications > 0 { break }; await Task.yield() }
        XCTAssertEqual(transport.routing.current.destination?.paneID, 2)
        XCTAssertFalse(transport.routing.current.ready)
        pump.requestStop(); _ = try await run.value
    }
}
