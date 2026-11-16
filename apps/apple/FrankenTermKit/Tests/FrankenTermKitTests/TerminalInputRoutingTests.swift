import Foundation
import XCTest
@testable import FrankenTermKit

private final class InputRouteProbe: @unchecked Sendable {
    struct Destination: Equatable, Sendable { let pane: Int, incarnation: UUID, visit: UUID }
    let lock = NSLock()
    var selected = Destination(pane: 1, incarnation: UUID(), visit: UUID())
    var live: [Int: UUID] = [:]
    var output: [Int: Data] = [:]
    var block = false, closed = false, captures = 0, unboundWrites = 0
    var maximumPrefix = Int.max
    var pending: CheckedContinuation<Void, Never>?
    var holdNext = false
    init() { live[1] = selected.incarnation }
    func select(_ pane: Int) {
        lock.withLock {
            let incarnation = live[pane] ?? UUID()
            live[pane] = incarnation
            selected = Destination(pane: pane, incarnation: incarnation, visit: UUID())
        }
    }
    func remove(_ pane: Int) { lock.withLock { _ = live.removeValue(forKey: pane) } }
    func bytes(_ pane: Int) -> Data { lock.withLock { output[pane] ?? Data() } }
    func release() {
        let continuation = lock.withLock { let value = pending; pending = nil; return value }
        continuation?.resume()
    }
}

private struct CapturedInputRoute: TerminalInputWriter {
    let probe: InputRouteProbe, destination: InputRouteProbe.Destination
    func validateInputAdmission() throws {
        try probe.lock.withLock {
            guard !probe.closed, probe.selected == destination else { throw TerminalInputError.stale }
        }
    }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        let shouldHold = probe.lock.withLock { let value = probe.holdNext; probe.holdNext = false; return value }
        if shouldHold {
            await withCheckedContinuation { continuation in probe.lock.withLock { probe.pending = continuation } }
        }
        return try probe.lock.withLock {
            guard !probe.closed, probe.live[destination.pane] == destination.incarnation else { throw TerminalInputError.stale }
            if probe.block { return .wouldBlock }
            let count = min(bytes.count, probe.maximumPrefix)
            probe.output[destination.pane, default: Data()].append(bytes.prefix(count))
            return .accepted(count)
        }
    }
}

private final class RoutedInputTransport: TerminalByteTransport, Sendable {
    let identity: TerminalConnectionIdentity
    let probe: InputRouteProbe
    let acceptsModelReplies = false
    init(_ probe: InputRouteProbe) throws {
        self.probe = probe
        identity = try .init(workspaceID: "workspace", machineID: "machine", routeID: "route", routeRevision: 1,
                             transportID: UUID(), channelID: UUID())
    }
    func captureInputWriter() throws -> any TerminalInputWriter {
        try probe.lock.withLock {
            guard !probe.closed else { throw TerminalInputError.unavailable }
            probe.captures += 1
            return CapturedInputRoute(probe: probe, destination: probe.selected)
        }
    }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        probe.lock.withLock { probe.unboundWrites += 1 }
        throw TerminalInputError.invalidIdentity
    }
    func close() async { probe.lock.withLock { probe.closed = true }; probe.release() }
}

private actor DirectInputTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    private(set) var output = Data()
    init() throws {
        identity = try .init(workspaceID: "workspace", machineID: "direct", routeID: "route", routeRevision: 1,
                             transportID: UUID(), channelID: UUID())
    }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite { output += bytes; return .accepted(bytes.count) }
    func close() async {}
}

final class TerminalInputRoutingTests: XCTestCase, @unchecked Sendable {
    private struct Fixture {
        let probe: InputRouteProbe, transport: RoutedInputTransport
        let coordinator: TerminalInputCoordinator, source: TerminalSource, focus: TerminalFocus
    }
    private func fixture(limits: TerminalInputLimits = .init()) async throws -> Fixture {
        let probe = InputRouteProbe(), transport = try RoutedInputTransport(probe)
        let coordinator = try TerminalInputCoordinator(limits: limits)
        let source = try await coordinator.attach(transport)
        let empty = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(empty, to: source)
        return .init(probe: probe, transport: transport, coordinator: coordinator, source: source, focus: focus)
    }
    private func drain(_ fixture: Fixture) async throws {
        for _ in 0..<1024 {
            switch try await fixture.coordinator.drainOne(fixture.source) {
            case .progressed: continue
            case .idle: return
            default: XCTFail("Unexpected drain result"); return
            }
        }
        XCTFail("Drain did not settle")
    }
    private func expectStale(_ operation: () async throws -> Void) async {
        do { try await operation(); XCTFail("Obsolete focus accepted input") }
        catch { XCTAssertEqual(error as? TerminalInputError, .stale) }
    }

    func testQueuedBytesKeepTheirPaneAcrossFocusChangesAndWouldBlock() async throws {
        let f = try await fixture()
        _ = try await f.coordinator.submit(f.focus, encodedBytes: Data("one".utf8))
        f.probe.lock.withLock { f.probe.block = true }
        let blocked = try await f.coordinator.drainOne(f.source)
        XCTAssertEqual(blocked, .wouldBlock)
        f.probe.select(2)
        let next = try await f.coordinator.changeFocus(f.focus, to: f.source, cleanup: Data("release-one".utf8))
        _ = try await f.coordinator.submit(next, encodedBytes: Data("two".utf8))
        f.probe.lock.withLock { f.probe.block = false; f.probe.maximumPrefix = 2 }
        try await drain(f)
        XCTAssertEqual(f.probe.bytes(1), Data("onerelease-one".utf8))
        XCTAssertEqual(f.probe.bytes(2), Data("two".utf8))
        XCTAssertEqual(f.probe.lock.withLock { f.probe.unboundWrites }, 0)
        XCTAssertEqual(f.probe.lock.withLock { f.probe.captures }, 2, "capture per focus, not per write or chunk")
    }

    func testPartialPasteCancellationNeverClosesBracketsOnAnotherPane() async throws {
        let text = "é👩🏽‍💻abc"
        let complete = Data(("\u{1b}[200~" + text + "\u{1b}[201~").utf8)
        for prefix in 1..<complete.count {
            let f = try await fixture()
            f.probe.lock.withLock { f.probe.maximumPrefix = prefix }
            let receipt = try await f.coordinator.paste(f.focus, text: text, bracketed: true)
            _ = try await f.coordinator.drainOne(f.source)
            f.probe.select(2)
            let next = try await f.coordinator.changeFocus(f.focus, to: f.source)
            _ = try await f.coordinator.cancel(receipt)
            _ = try await f.coordinator.submit(next, encodedBytes: Data("B".utf8))
            try await drain(f)
            let first = f.probe.bytes(1)
            XCTAssertTrue(first.starts(with: Data("\u{1b}[200~".utf8)), "prefix \(prefix)")
            XCTAssertTrue(first.suffix(6) == Data("\u{1b}[201~".utf8), "prefix \(prefix)")
            XCTAssertNotNil(String(data: first, encoding: .utf8), "prefix \(prefix)")
            XCTAssertEqual(f.probe.bytes(2), Data("B".utf8), "prefix \(prefix)")
            let outcomes = try await f.coordinator.takeOutcomes(f.source)
            XCTAssertEqual(outcomes.first?.receipt, receipt)
            XCTAssertEqual(outcomes.first?.disposition, .cancelledAfterPrefix)
        }
    }

    func testStaleResponderCannotAdmitMoreInputOrReviveAfterRoundTrip() async throws {
        let f = try await fixture()
        f.probe.select(2)
        await expectStale { _ = try await f.coordinator.submit(f.focus, encodedBytes: Data([65])) }
        await expectStale { _ = try await f.coordinator.paste(f.focus, text: "text", bracketed: false) }
        await expectStale { _ = try await f.coordinator.beginComposition(f.focus) }
        f.probe.select(1)
        await expectStale { _ = try await f.coordinator.submit(f.focus, encodedBytes: Data([65])) }
        let accounting = await f.coordinator.accounting()
        XCTAssertEqual(accounting.retainedBytes, 0)
        XCTAssertEqual(accounting.retainedRecords, 0)
    }

    func testCompositionRemainsOwnedByOriginalFocusWhenPaneChanges() async throws {
        let f = try await fixture()
        let composition = try await f.coordinator.beginComposition(f.focus)
        f.probe.select(2)
        await expectStale { _ = try await f.coordinator.commitComposition(composition, encodedBytes: Data("draft".utf8)) }
        do { _ = try await f.coordinator.changeFocus(f.focus, to: f.source); XCTFail("Composition disappeared") }
        catch { XCTAssertEqual(error as? TerminalInputError, .compositionPending) }
        try await f.coordinator.cancelComposition(composition)
        let next = try await f.coordinator.changeFocus(f.focus, to: f.source)
        _ = try await f.coordinator.submit(next, encodedBytes: Data("new".utf8))
        try await drain(f)
        XCTAssertTrue(f.probe.bytes(1).isEmpty)
        XCTAssertEqual(f.probe.bytes(2), Data("new".utf8))
    }

    func testHeldKeyCleanupAndSuspensionUseTheOriginalCapturedWriter() async throws {
        let f = try await fixture()
        _ = try await f.coordinator.submit(f.focus, encodedBytes: Data("down".utf8))
        f.probe.select(2)
        _ = try await f.coordinator.cleanup(f.focus, encodedBytes: Data("up".utf8))
        let cleaned = try await f.coordinator.suspend(f.focus, cleanup: Data("release-all".utf8))
        XCTAssertTrue(cleaned)
        try await drain(f)
        XCTAssertEqual(f.probe.bytes(1), Data("downuprelease-all".utf8))
        XCTAssertTrue(f.probe.bytes(2).isEmpty)
    }

    func testPaneRemovalAndIDReuseNeverRetargetAnAdmittedOperation() async throws {
        let f = try await fixture()
        _ = try await f.coordinator.submit(f.focus, encodedBytes: Data("do-not-redirect".utf8))
        f.probe.remove(1); f.probe.select(1)
        let result = try await f.coordinator.drainOne(f.source)
        XCTAssertEqual(result, .connectionClosed)
        XCTAssertTrue(f.probe.bytes(1).isEmpty)
        let outcomes = try await f.coordinator.takeOutcomes(f.source)
        XCTAssertEqual(outcomes.count, 1)
        XCTAssertEqual(outcomes[0].disposition, .connectionLost(leasedTailUnknown: true))
    }

    func testFailedDestinationCaptureDoesNotPublishFocusOrQueueCleanup() async throws {
        let f = try await fixture()
        f.probe.lock.withLock { f.probe.closed = true }
        do { _ = try await f.coordinator.changeFocus(f.focus, to: f.source, cleanup: Data([88])); XCTFail("capture succeeded") }
        catch { XCTAssertEqual(error as? TerminalInputError, .unavailable) }
        f.probe.lock.withLock { f.probe.closed = false }
        _ = try await f.coordinator.submit(f.focus, encodedBytes: Data("original".utf8))
        try await drain(f)
        XCTAssertEqual(f.probe.bytes(1), Data("original".utf8))
    }

    func testCancellationDuringLeasedWriteKeepsItsDestinationAndCleanup() async throws {
        let f = try await fixture()
        let receipt = try await f.coordinator.paste(f.focus, text: "abc", bracketed: true)
        f.probe.lock.withLock { f.probe.holdNext = true; f.probe.maximumPrefix = 2 }
        let write = Task { try await f.coordinator.drainOne(f.source) }
        for _ in 0..<10_000 {
            if f.probe.lock.withLock({ f.probe.pending != nil }) { break }
            await Task.yield()
        }
        XCTAssertTrue(f.probe.lock.withLock { f.probe.pending != nil })
        f.probe.select(2)
        let cancelled = try await f.coordinator.cancel(receipt)
        XCTAssertEqual(cancelled, .pendingWrite)
        let next = try await f.coordinator.changeFocus(f.focus, to: f.source)
        _ = try await f.coordinator.submit(next, encodedBytes: Data("B".utf8))
        f.probe.release()
        _ = try await write.value
        try await drain(f)
        XCTAssertEqual(f.probe.bytes(1), Data("\u{1b}[200~\u{1b}[201~".utf8))
        XCTAssertEqual(f.probe.bytes(2), Data("B".utf8))
    }

    func testMuxMirrorRepliesAreSuppressedWithoutWriterCaptureOrQueueCharge() async throws {
        let f = try await fixture()
        let result = try await f.coordinator.reply(f.source, encodedBytes: Data("reply".utf8))
        guard case .droppedWhole = result else { return XCTFail("Mirror generated duplicate answerback") }
        let accounting = await f.coordinator.accounting()
        XCTAssertEqual(accounting.droppedReplyUnits, 1)
        XCTAssertEqual(accounting.retainedBytes, 0); XCTAssertEqual(accounting.retainedRecords, 0)
        XCTAssertEqual(f.probe.lock.withLock { f.probe.captures }, 1)
    }

    func testDirectSSHDefaultsPreserveInputCleanupAndHiddenAnswerbacks() async throws {
        let coordinator = try TerminalInputCoordinator(), transport = try DirectInputTransport()
        let source = try await coordinator.attach(transport)
        let empty = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(empty, to: source)
        _ = try await coordinator.submit(focus, encodedBytes: Data("key".utf8))
        _ = try await coordinator.suspend(focus, cleanup: Data("release".utf8))
        _ = try await coordinator.reply(source, encodedBytes: Data("reply".utf8))
        for _ in 0..<3 { _ = try await coordinator.drainOne(source) }
        let output = await transport.output
        XCTAssertEqual(output, Data("keyreleasereply".utf8))
    }

    func testManySameConnectionPaneVisitsKeepExactByteStreamsAndBudget() async throws {
        var limits = TerminalInputLimits(); limits.recordsPerConnection = 4096; limits.totalRecords = 4096
        let f = try await fixture(limits: limits)
        var focus = f.focus, expected: [Int: Data] = [:]
        for visit in 0..<600 {
            let pane = 1 + visit % 12
            if visit > 0 {
                f.probe.select(pane)
                focus = try await f.coordinator.changeFocus(focus, to: f.source)
            }
            let bytes = Data("\(visit);".utf8)
            _ = try await f.coordinator.submit(focus, encodedBytes: bytes)
            expected[pane, default: Data()] += bytes
        }
        f.probe.lock.withLock { f.probe.maximumPrefix = 4 }
        for _ in 0..<4096 {
            if try await f.coordinator.drainOne(f.source) == .idle { break }
        }
        for (pane, bytes) in expected { XCTAssertEqual(f.probe.bytes(pane), bytes) }
        let accounting = await f.coordinator.accounting()
        XCTAssertEqual(accounting.retainedBytes, 0); XCTAssertEqual(accounting.inFlightBytes, 0)
        XCTAssertEqual(accounting.retainedRecords, 600)
        XCTAssertEqual(f.probe.lock.withLock { f.probe.unboundWrites }, 0)
    }
}
