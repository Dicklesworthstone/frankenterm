import Foundation
import XCTest
@testable import FrankenTermKit

private actor SessionTestGate {
    private var open = false
    private var waiters: [CheckedContinuation<Void, Never>] = []
    func wait() async {
        if open { return }
        await withCheckedContinuation { waiters.append($0) }
    }
    func release() { open = true; let old = waiters; waiters = []; for waiter in old { waiter.resume() } }
}

/// A bounded channel fixture, not an SSH/parser implementation. Every accepted
/// byte crosses the same real coordinator and pump used by the native adapter.
private actor PumpTransport: TerminalSessionTransport {
    nonisolated let identity: TerminalConnectionIdentity
    nonisolated let inputSignal = TerminalSessionSignal()
    private var wake = false, closed = false
    private var pending = Data(), written = Data()
    private var updates: [TerminalSessionUpdate] = []
    private var waiter: CheckedContinuation<TerminalSessionUpdate, Error>?
    private var serviceCount = 0, closeCount = 0, admits = 0
    private var closes: Task<Void, Never>?
    private var blockedWrites = false
    let firstService = SessionTestGate()
    let closedGate = SessionTestGate()
    let closeRelease: SessionTestGate?

    init(machine: String = "build", closeRelease: SessionTestGate? = nil) throws {
        identity = try TerminalConnectionIdentity(workspaceID: "workspace", machineID: machine, routeID: machine,
            routeRevision: 1, transportID: UUID(), channelID: UUID())
        self.closeRelease = closeRelease
    }
    func writeSome(_ bytes: Data) throws -> TerminalTransportWrite {
        guard !closed else { throw TerminalInputError.unavailable }
        guard pending.isEmpty, !blockedWrites else { return .wouldBlock }
        guard bytes.count <= 4096 else { throw TerminalInputError.capacity }
        pending = bytes; admits += 1
        return .accepted(bytes.count)
    }
    func blockWrites(_ value: Bool) { blockedWrites = value }
    func wakeService() {
        if let waiter { self.waiter = nil; waiter.resume(returning: .init(output: Data())) }
        else { wake = true }
    }
    func service() async throws -> TerminalSessionUpdate {
        guard !closed else { throw TerminalInputError.unavailable }
        serviceCount += 1
        await firstService.release()
        if !pending.isEmpty { let bytes = pending; pending = Data(); written += bytes; return .init(output: bytes) }
        if !updates.isEmpty { return updates.removeFirst() }
        if wake { wake = false; return .init(output: Data()) }
        guard waiter == nil else { throw TerminalInputError.unavailable }
        return try await withCheckedThrowingContinuation { waiter = $0 }
    }
    func inject(_ update: TerminalSessionUpdate) {
        if let waiter { self.waiter = nil; waiter.resume(returning: update) }
        else { updates.append(update) }
    }
    func close() async {
        if let closes { await closes.value; return }
        closed = true; closeCount += 1
        let waiter = self.waiter; self.waiter = nil
        waiter?.resume(throwing: TerminalInputError.unavailable)
        let release = closeRelease, reached = closedGate
        let task = Task { await reached.release(); await release?.wait() }
        closes = task; await task.value
    }
    func stats() -> (Data, Int, Int, Int) { (written, serviceCount, closeCount, admits) }
}

private actor PumpConsumer: TerminalSessionConsumer {
    private var output = Data()
    private var sources: [TerminalSource] = []
    private var outcomes: [TerminalInputOutcome] = []
    private var resize: TerminalSessionGeometry?
    private var eof = false
    private var failureOnOutput = false, failureOnOutcomes = false
    private var thresholds: [(Int, CheckedContinuation<Void, Never>)] = []
    private var outcomeThresholds: [(Int, CheckedContinuation<Void, Never>)] = []
    private let outputHold: SessionTestGate?
    let outputEntered = SessionTestGate()
    let outcomesEntered = SessionTestGate()
    private let coordinator: TerminalInputCoordinator?
    private var reply: Data?
    init(outputHold: SessionTestGate? = nil, coordinator: TerminalInputCoordinator? = nil, reply: Data? = nil) {
        self.outputHold = outputHold; self.coordinator = coordinator; self.reply = reply
    }
    func fail(output: Bool = false, outcomes: Bool = false) { failureOnOutput = output; failureOnOutcomes = outcomes }
    func receive(_ update: TerminalSessionUpdate, source: TerminalSource) async throws {
        await outputEntered.release()
        await outputHold?.wait()
        if failureOnOutput { throw TerminalInputError.unavailable }
        output += update.output; sources.append(source); resize = update.resized ?? resize; eof = eof || update.endOfOutput
        if let coordinator, let reply {
            self.reply = nil
            _ = try await coordinator.reply(source, encodedBytes: reply)
        }
        let ready = thresholds.filter { output.count >= $0.0 }
        thresholds.removeAll { output.count >= $0.0 }
        for (_, continuation) in ready { continuation.resume() }
    }
    func receive(_ values: [TerminalInputOutcome], source: TerminalSource) async throws {
        await outcomesEntered.release()
        if failureOnOutcomes { throw TerminalInputError.unavailable }
        guard values.allSatisfy({ $0.receipt.source == source }) else { throw TerminalInputError.stale }
        outcomes += values
        let ready = outcomeThresholds.filter { outcomes.count >= $0.0 }
        outcomeThresholds.removeAll { outcomes.count >= $0.0 }
        for (_, continuation) in ready { continuation.resume() }
    }
    func waitForBytes(_ count: Int) async {
        if output.count >= count { return }
        await withCheckedContinuation { thresholds.append((count, $0)) }
    }
    func waitForOutcomes(_ count: Int) async {
        if outcomes.count >= count { return }
        await withCheckedContinuation { outcomeThresholds.append((count, $0)) }
    }
    func stats() -> (Data, [TerminalSource], [TerminalInputOutcome], Bool, TerminalSessionGeometry?) {
        (output, sources, outcomes, eof, resize)
    }
}

final class TerminalSessionPumpTests: XCTestCase {
    private func setup() async throws -> (TerminalInputCoordinator, PumpTransport, TerminalSessionPump, TerminalFocus) {
        let coordinator = try TerminalInputCoordinator(), transport = try PumpTransport()
        let pump = try await TerminalSessionPump.attach(transport, coordinator: coordinator)
        let blank = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(blank, to: pump.source)
        return (coordinator, transport, pump, focus)
    }

    func testWakeBeforeWaitAndManyNotificationsCoalesce() async {
        let signal = TerminalSessionSignal()
        for _ in 0..<100_000 { signal.notify() }
        let first = await signal.wait()
        XCTAssertTrue(first)
        let second = Task { await signal.wait() }
        signal.finish()
        let stopped = await second.value
        XCTAssertFalse(stopped)
        signal.notify()
        let after = await signal.wait()
        XCTAssertFalse(after)
    }

    func testCancellationBeforeAndDuringWaitRetiresSignal() async {
        let signal = TerminalSessionSignal()
        let task = Task { await signal.wait() }
        task.cancel()
        let value = await task.value
        XCTAssertFalse(value)
        XCTAssertTrue(signal.isFinished)
        let again = await signal.wait()
        XCTAssertFalse(again)
    }

    func testIdleAdmissionWakesAndFeedsExactBytesWithoutUIHook() async throws {
        let (coordinator, transport, pump, focus) = try await setup()
        let sink = PumpConsumer()
        let run = Task { try await pump.run(consumer: sink) }
        await transport.firstService.wait()
        let bytes = Data("hello é界🦀\r".utf8)
        _ = try await coordinator.submit(focus, encodedBytes: bytes)
        await sink.waitForBytes(bytes.count)
        pump.requestStop()
        let end = try await run.value
        XCTAssertEqual(end, .cancelled)
        let delivered = await sink.stats(), native = await transport.stats()
        XCTAssertEqual(delivered.0, bytes)
        XCTAssertTrue(delivered.1.allSatisfy { $0 == pump.source })
        XCTAssertEqual(native.0, bytes)
        XCTAssertEqual(native.2, 1)
        XCTAssertLessThan(native.1, 20)
    }

    func testQueuedBeforeRunIsNotLostAndRunCannotBeStartedTwice() async throws {
        let (coordinator, transport, pump, focus) = try await setup()
        _ = try await coordinator.submit(focus, encodedBytes: Data("before".utf8))
        let sink = PumpConsumer(), run = Task { try await pump.run(consumer: PumpConsumer()) }
        await transport.firstService.wait()
        do { _ = try await pump.run(consumer: sink); XCTFail("second driver accepted") }
        catch let error as TerminalInputError { XCTAssertEqual(error, .unavailable) }
        pump.requestStop(); _ = try await run.value
        let stats = await transport.stats()
        XCTAssertEqual(stats.0, Data("before".utf8))
        XCTAssertEqual(stats.2, 1)
    }

    func testHiddenTerminalRepliesUseSameFIFOAsPaste() async throws {
        let (coordinator, transport, pump, focus) = try await setup()
        let reply = Data("\u{1B}[1;1R".utf8)
        let sink = PumpConsumer(coordinator: coordinator, reply: reply)
        let text = "a" + String(repeating: "界", count: 4000)
        _ = try await coordinator.paste(focus, text: text, bracketed: true)
        _ = try await coordinator.changeFocus(focus, to: nil)
        let expected = Data("\u{1B}[200~\(text)\u{1B}[201~".utf8) + reply
        let run = Task { try await pump.run(consumer: sink) }
        await sink.waitForBytes(expected.count)
        pump.requestStop(); _ = try await run.value
        let stats = await transport.stats()
        XCTAssertEqual(stats.0, expected)
    }

    func testOutputFailureClosesWithoutTakingQueuedDisposition() async throws {
        let (coordinator, transport, pump, _) = try await setup()
        let sink = PumpConsumer()
        await sink.fail(output: true)
        await transport.inject(.init(output: Data("unconsumed".utf8)))
        let end = try await pump.run(consumer: sink)
        XCTAssertEqual(end, .consumerFailed)
        let stats = await transport.stats()
        XCTAssertEqual(stats.2, 1)
        let sinkStats = await sink.stats()
        XCTAssertTrue(sinkStats.0.isEmpty)
        let blank = try await coordinator.makeStream(sceneID: UUID())
        do { _ = try await coordinator.changeFocus(blank, to: pump.source); XCTFail("closed channel admitted") }
        catch { }
    }

    func testOutcomeFailureKeepsMetadataAndRefusesSilentRetirement() async throws {
        let (coordinator, _, pump, focus) = try await setup()
        let receipt = try await coordinator.submit(focus, encodedBytes: Data("echo\r".utf8))
        let sink = PumpConsumer()
        await sink.fail(outcomes: true)
        let end = try await pump.run(consumer: sink)
        XCTAssertEqual(end, .consumerFailed)
        let batch = try await coordinator.pendingOutcomes(pump.source)
        XCTAssertEqual(batch.outcomes.map(\.receipt), [receipt])
        XCTAssertEqual(batch.outcomes.first?.disposition, .transportAccepted)
        let accounting = await coordinator.accounting()
        XCTAssertEqual(accounting.retainedRecords, 1)
        try await coordinator.acknowledgeOutcomes(batch)
        let empty = try await coordinator.pendingOutcomes(pump.source)
        XCTAssertTrue(empty.outcomes.isEmpty)
    }

    func testOutcomePrefixAcknowledgementCannotConsumeAnotherBatch() async throws {
        let (coordinator, _, pump, focus) = try await setup()
        let a = try await coordinator.submit(focus, encodedBytes: Data([1]))
        _ = try await coordinator.cancel(a)
        let first = try await coordinator.pendingOutcomes(pump.source)
        let b = try await coordinator.submit(focus, encodedBytes: Data([2]))
        _ = try await coordinator.cancel(b)
        try await coordinator.acknowledgeOutcomes(first)
        do { try await coordinator.acknowledgeOutcomes(first); XCTFail("replayed acknowledgement") }
        catch let error as TerminalInputError { XCTAssertEqual(error, .stale) }
        let second = try await coordinator.pendingOutcomes(pump.source)
        XCTAssertEqual(second.outcomes.map(\.receipt), [b])
        let foreign = try TerminalInputCoordinator()
        do { try await foreign.acknowledgeOutcomes(second); XCTFail("foreign source") } catch { }
        try await pump.disconnect()
    }

    func testCancelledQueuedRecordsProgressAcrossBatchesWithoutSocketTraffic() async throws {
        let (coordinator, transport, pump, focus) = try await setup()
        for _ in 0..<200 {
            let receipt = try await coordinator.submit(focus, encodedBytes: Data([1]))
            _ = try await coordinator.cancel(receipt)
        }
        let sink = PumpConsumer()
        let run = Task { try await pump.run(consumer: sink) }
        await sink.waitForOutcomes(200)
        // Delivery and acknowledgement are distinct actor turns. Join the pump
        // after delivery; it must acknowledge a successful callback even if this
        // cancellation arrives before the acknowledgement message is scheduled.
        pump.requestStop(); _ = try await run.value
        let stats = await sink.stats()
        XCTAssertEqual(stats.2.count, 200)
        let accounting = await coordinator.accounting()
        XCTAssertEqual(accounting.retainedRecords, 0)
        let native = await transport.stats()
        XCTAssertEqual(native.3, 0)
        XCTAssertLessThan(native.1, 20)
    }

    func testEOFDeliversFinalOutputBeforeClientDisconnectAndDoesNotSpin() async throws {
        let (_, transport, pump, _) = try await setup()
        await transport.inject(.init(output: Data("final".utf8), endOfOutput: true))
        let sink = PumpConsumer(), end = try await pump.run(consumer: sink)
        XCTAssertEqual(end, .peerOutputEnded)
        let consumer = await sink.stats(), native = await transport.stats()
        XCTAssertEqual(consumer.0, Data("final".utf8))
        XCTAssertTrue(consumer.3)
        XCTAssertEqual(native.1, 1)
        XCTAssertEqual(native.2, 1)
    }

    func testOversizedReadIsNotPassedToTerminal() async throws {
        let (_, transport, pump, _) = try await setup()
        await transport.inject(.init(output: Data(repeating: 1, count: 16_385)))
        let sink = PumpConsumer(), end = try await pump.run(consumer: sink)
        XCTAssertEqual(end, .transportFailed)
        let stats = await sink.stats()
        XCTAssertTrue(stats.0.isEmpty)
    }

    func testRemoteCompletionWaitsForFinalConsumerAndPreservesExactEvidence() async throws {
        for completion in [TerminalSessionCompletion(remoteExit: .exitStatus(7), channelClosed: true),
                           .init(remoteExit: .signal("TERM"), channelClosed: true),
                           .init(remoteExit: nil, channelClosed: true),
                           .init(remoteExit: nil, channelClosed: false)] {
            let (_, transport, pump, _) = try await setup()
            let release = SessionTestGate(), sink = PumpConsumer(outputHold: release)
            await transport.inject(.init(output: Data("final received bytes".utf8), endOfOutput: true,
                                         completion: completion))
            let run = Task { try await pump.run(consumer: sink) }
            await sink.outputEntered.wait()
            let before = await transport.stats()
            XCTAssertEqual(before.2, 0, "remote completion must not retire a suspended final consumer")
            await release.release()
            let end = try await run.value, delivered = await sink.stats(), closed = await transport.stats()
            XCTAssertEqual(end, .completed(completion))
            XCTAssertEqual(delivered.0, Data("final received bytes".utf8))
            XCTAssertTrue(delivered.3)
            XCTAssertEqual(closed.1, 1)
            XCTAssertEqual(closed.2, 1)
        }
    }

    func testCancellationDuringFinalDeliveryDoesNotBecomeRemoteCompletion() async throws {
        let (_, transport, pump, _) = try await setup()
        let release = SessionTestGate(), sink = PumpConsumer(outputHold: release)
        await transport.inject(.init(output: Data("last".utf8), endOfOutput: true,
                                     completion: .init(remoteExit: .exitStatus(7), channelClosed: true)))
        let run = Task { try await pump.run(consumer: sink) }
        await sink.outputEntered.wait()
        pump.requestStop()
        await release.release()
        let end = try await run.value
        XCTAssertEqual(end, .cancelled)
    }

    func testCompletionWithoutFinalOutputBoundaryIsRejected() async throws {
        let (_, transport, pump, _) = try await setup()
        await transport.inject(.init(output: Data("invalid".utf8),
                                     completion: .init(remoteExit: .exitStatus(7), channelClosed: true)))
        let sink = PumpConsumer(), end = try await pump.run(consumer: sink)
        XCTAssertEqual(end, .transportFailed)
        let received = await sink.stats()
        XCTAssertTrue(received.0.isEmpty)
    }

    func testCancellationJoinsNativeCloseAndAlreadyOwnedConsumer() async throws {
        let closeRelease = SessionTestGate(), consumerRelease = SessionTestGate()
        let coordinator = try TerminalInputCoordinator(), transport = try PumpTransport(closeRelease: closeRelease)
        let pump = try await TerminalSessionPump.attach(transport, coordinator: coordinator)
        let blank = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(blank, to: pump.source)
        let sink = PumpConsumer(outputHold: consumerRelease)
        await transport.inject(.init(output: Data("held".utf8)))
        let run = Task { try await pump.run(consumer: sink) }
        await sink.outputEntered.wait()
        run.cancel()
        await transport.closedGate.wait()
        // The consumer is STILL suspended. Stop must already have closed
        // admission, not merely the underlying network descriptor.
        do { _ = try await coordinator.submit(focus, encodedBytes: Data("too late".utf8)); XCTFail("input admitted after stop") }
        catch let error as TerminalInputError { XCTAssertEqual(error, .unavailable) }
        await closeRelease.release()
        await consumerRelease.release()
        let end = try await run.value
        XCTAssertEqual(end, .cancelled)
        let stats = await transport.stats()
        XCTAssertEqual(stats.2, 1)
    }

    func testBlockedConsumerOnOneMachineDoesNotBlockAnother() async throws {
        let gate = SessionTestGate(), coordinator = try TerminalInputCoordinator()
        let a = try PumpTransport(machine: "a"), b = try PumpTransport(machine: "b")
        let pa = try await TerminalSessionPump.attach(a, coordinator: coordinator)
        let pb = try await TerminalSessionPump.attach(b, coordinator: coordinator)
        let sa = PumpConsumer(outputHold: gate), sb = PumpConsumer()
        await a.inject(.init(output: Data("a".utf8)))
        let ta = Task { try await pa.run(consumer: sa) }, tb = Task { try await pb.run(consumer: sb) }
        await sa.outputEntered.wait()
        let blank = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(blank, to: pb.source)
        _ = try await coordinator.submit(focus, encodedBytes: Data("independent".utf8))
        await sb.waitForBytes(11)
        pa.requestStop(); pb.requestStop(); await gate.release()
        _ = try await ta.value; _ = try await tb.value
        let stats = await sb.stats()
        XCTAssertEqual(stats.0, Data("independent".utf8))
    }

    func testResizeCompletionRetainsExactSource() async throws {
        let (_, transport, pump, _) = try await setup()
        let geometry = try TerminalSessionGeometry(columns: 120, rows: 40, pixelWidth: 1200, pixelHeight: 800)
        await transport.inject(.init(output: Data(), endOfOutput: true, resized: geometry))
        let sink = PumpConsumer()
        _ = try await pump.run(consumer: sink)
        let stats = await sink.stats()
        XCTAssertEqual(stats.4, geometry)
        XCTAssertEqual(stats.1, [pump.source])
        XCTAssertThrowsError(try TerminalSessionGeometry(columns: 512, rows: 512))
        XCTAssertThrowsError(try TerminalSessionGeometry(columns: 0, rows: 24))
    }

    func testPrecancelledRunClosesWithoutOfferingInput() async throws {
        let (coordinator, transport, pump, focus) = try await setup()
        _ = try await coordinator.submit(focus, encodedBytes: Data("never".utf8))
        let gate = SessionTestGate(), sink = PumpConsumer()
        let run = Task { await gate.wait(); return try await pump.run(consumer: sink) }
        run.cancel(); await gate.release()
        let end = try await run.value
        XCTAssertEqual(end, .cancelled)
        let stats = await transport.stats()
        XCTAssertEqual(stats.3, 0)
        let outcomes = try await coordinator.pendingOutcomes(pump.source)
        XCTAssertEqual(outcomes.outcomes.first?.disposition, .definitelyNotWritten)
    }

    func testDisconnectBeforeRunAndNoAutomaticReplay() async throws {
        let (coordinator, transport, pump, focus) = try await setup()
        _ = try await coordinator.submit(focus, encodedBytes: Data("no replay".utf8))
        try await pump.disconnect()
        let end = try await pump.run(consumer: PumpConsumer())
        XCTAssertEqual(end, .cancelled)
        let stats = await transport.stats()
        XCTAssertEqual(stats.3, 0)
        XCTAssertEqual(stats.2, 1)
    }
}

extension TerminalSessionPumpTests: @unchecked Sendable {}
