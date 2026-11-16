import Foundation
import XCTest
@testable import FrankenTermKit

private actor ControlledTerminalTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    private var output = Data()
    private var writes = 0, closes = 0
    private var closed = false
    private var delayWrites = false, delayClose = false, fragmented = false
    private var writeWaiter: CheckedContinuation<TerminalTransportWrite, any Error>?
    private var closeWaiter: CheckedContinuation<Void, Never>?
    private var writeObservers: [CheckedContinuation<Void, Never>] = []
    private var closeObservers: [CheckedContinuation<Void, Never>] = []
    init(machine: String = "machine") throws {
        identity = try TerminalConnectionIdentity(workspaceID: "work", machineID: machine, routeID: "route-" + machine,
            routeRevision: 1, transportID: UUID(), channelID: UUID())
    }
    func configure(delayWrites: Bool = false, delayClose: Bool = false, fragmented: Bool = false) {
        self.delayWrites = delayWrites; self.delayClose = delayClose; self.fragmented = fragmented
    }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        writes += 1
        if delayWrites {
            return try await withCheckedThrowingContinuation { continuation in
                precondition(writeWaiter == nil)
                writeWaiter = continuation
                for observer in writeObservers { observer.resume() }
                writeObservers.removeAll()
            }
        }
        guard !closed else { return .failed }
        if fragmented && writes % 11 == 0 { return .wouldBlock }
        let count = fragmented ? min(bytes.count, (writes % 5) + 1) : bytes.count
        output += bytes.prefix(count)
        return .accepted(count)
    }
    func close() async {
        closes += 1
        if delayClose {
            await withCheckedContinuation { continuation in
                precondition(closeWaiter == nil)
                closeWaiter = continuation
                for observer in closeObservers { observer.resume() }
                closeObservers.removeAll()
            }
        }
        closed = true
        writeWaiter?.resume(returning: .failed); writeWaiter = nil
    }
    func waitForWrite() async {
        if writeWaiter != nil { return }
        await withCheckedContinuation { writeObservers.append($0) }
    }
    func waitForClose() async {
        if closeWaiter != nil { return }
        await withCheckedContinuation { closeObservers.append($0) }
    }
    func releaseClose() { delayClose = false; closeWaiter?.resume(); closeWaiter = nil }
    func isClosed() -> Bool { closed }
    func bytes() -> Data { output }
    func writeCalls() -> Int { writes }
    func closeCalls() -> Int { closes }
}

private actor StartedCallers {
    private var started = 0
    private var waiter: CheckedContinuation<Void, Never>?
    private let expected: Int
    init(_ expected: Int) { self.expected = expected }
    func start() {
        started += 1
        if started == expected { waiter?.resume(); waiter = nil }
    }
    func wait() async {
        if started == expected { return }
        await withCheckedContinuation { waiter = $0 }
    }
}

final class TerminalInputConcurrencyTests: XCTestCase, @unchecked Sendable {
    private func setup(limits: TerminalInputLimits = TerminalInputLimits()) async throws
        -> (TerminalInputCoordinator, ControlledTerminalTransport, TerminalSource, TerminalFocus) {
        let coordinator = try TerminalInputCoordinator(limits: limits)
        let transport = try ControlledTerminalTransport()
        let source = try await coordinator.attach(transport)
        let initial = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(initial, to: source)
        return (coordinator, transport, source, focus)
    }

    func testCancelledDriverDoesNotLeaseOrDiscardQueuedInput() async throws {
        let (coordinator, transport, source, focus) = try await setup()
        _ = try await coordinator.submit(focus, encodedBytes: Data("exactly-once".utf8))
        let driver = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await coordinator.drainOne(source)
        }
        do { _ = try await driver.value; XCTFail("cancelled driver wrote input") }
        catch is CancellationError { }
        catch { XCTFail("unexpected failure") }
        let callsBefore = await transport.writeCalls(), queued = await coordinator.accounting()
        XCTAssertEqual(callsBefore, 0)
        XCTAssertEqual(queued.retainedBytes, 12)
        XCTAssertEqual(queued.inFlightBytes, 0)
        let result = try await coordinator.drainOne(source)
        XCTAssertEqual(result, .progressed(12))
        let bytes = await transport.bytes()
        XCTAssertEqual(bytes, Data("exactly-once".utf8))
    }

    func testEveryConcurrentDisconnectJoinsTheSameActualClose() async throws {
        let (coordinator, transport, source, focus) = try await setup()
        await transport.configure(delayClose: true)
        let first = Task { try await coordinator.disconnect(source); return await transport.isClosed() }
        await transport.waitForClose()
        let started = StartedCallers(16)
        let followers = (0..<16).map { _ in Task {
            await started.start()
            try await coordinator.disconnect(source)
            return await transport.isClosed()
        } }
        await started.wait()
        // Exercise overlapping callers while the actual close stays suspended.
        // Returned booleans record the state at each method's return, not the
        // later state when this test reads its completed Task value.
        for _ in 0..<64 { await Task.yield() }
        await transport.releaseClose()
        let firstResult = try await first.value
        XCTAssertTrue(firstResult)
        for follower in followers { let result = try await follower.value; XCTAssertTrue(result) }
        let count = await transport.closeCalls()
        XCTAssertEqual(count, 1)
        _ = try await coordinator.suspend(focus)
        try await coordinator.retire(source)
        let final = await coordinator.accounting()
        XCTAssertEqual(final.connections, 0)
    }

    func testCloseRetainsOutstandingWriteOwnershipUntilItsCallbackSettles() async throws {
        let (coordinator, transport, source, focus) = try await setup()
        await transport.configure(delayWrites: true, delayClose: true)
        _ = try await coordinator.submit(focus, encodedBytes: Data(repeating: 65, count: 128 * 1024))
        let writing = Task { try await coordinator.drainOne(source) }
        await transport.waitForWrite()
        let closing = Task { try await coordinator.disconnect(source) }
        await transport.waitForClose()
        let during = await coordinator.accounting()
        XCTAssertEqual(during.retainedBytes, 128 * 1024)
        XCTAssertEqual(during.inFlightBytes, 4096)
        do { try await coordinator.retire(source); XCTFail("retired an outstanding write") }
        catch let error as TerminalInputError { XCTAssertEqual(error, .unavailable) }
        await transport.releaseClose()
        try await closing.value
        _ = try await writing.value
        let after = await coordinator.accounting()
        XCTAssertEqual(after.retainedBytes, 0)
        XCTAssertEqual(after.inFlightBytes, 0)
        XCTAssertEqual(after.retainedRecords, 1)
        let outcomes = try await coordinator.takeOutcomes(source)
        XCTAssertEqual(outcomes[0].disposition, .connectionLost(leasedTailUnknown: true))
        _ = try await coordinator.suspend(focus)
        try await coordinator.retire(source)
    }

    func testCompletePasteWireExtentIsReservedNotJustItsText() async throws {
        var limits = TerminalInputLimits()
        limits.bytesPerConnection = 13; limits.totalBytes = 13
        let (coordinator, transport, source, focus) = try await setup(limits: limits)
        do { _ = try await coordinator.paste(focus, text: "AB", bracketed: true); XCTFail("ignored bracket overhead") }
        catch let error as TerminalInputError { XCTAssertEqual(error, .capacity) }
        let refused = await coordinator.accounting()
        XCTAssertEqual(refused.retainedBytes, 0)
        XCTAssertEqual(refused.cleanupReservationBytes, 0)
        XCTAssertEqual(refused.retainedRecords, 0)
        _ = try await coordinator.paste(focus, text: "A", bracketed: true)
        let accepted = await coordinator.accounting()
        XCTAssertEqual(accepted.retainedBytes, 13)
        XCTAssertEqual(accepted.cleanupReservationBytes, 16)
        _ = try await coordinator.drainOne(source)
        let bytes = await transport.bytes()
        XCTAssertEqual(bytes, Data("\u{1B}[200~A\u{1B}[201~".utf8))
    }

    func testNonzeroDataSliceAndCallerMutationDoNotAlterAdmittedBytes() async throws {
        let (coordinator, transport, source, focus) = try await setup()
        var original = Data("discard|é🦀|discard".utf8)
        let selected = original.dropFirst(8).dropLast(8)
        XCTAssertNotEqual(selected.startIndex, 0)
        _ = try await coordinator.submit(focus, encodedBytes: selected)
        original.resetBytes(in: original.startIndex..<original.endIndex)
        for _ in 0..<20 {
            if try await coordinator.drainOne(source, maximumBytes: 1) == .idle { break }
        }
        let actual = await transport.bytes()
        XCTAssertEqual(actual, Data("é🦀".utf8))
    }

    func testFiveSourceInterleavingsKeepIndependentWireOracles() async throws {
        let coordinator = try TerminalInputCoordinator()
        var transports: [ControlledTerminalTransport] = [], sources: [TerminalSource] = [], expected = Array(repeating: Data(), count: 5)
        for index in 0..<5 {
            let transport = try ControlledTerminalTransport(machine: "machine-\(index)")
            await transport.configure(fragmented: true)
            transports.append(transport); sources.append(try await coordinator.attach(transport))
        }
        var focus = try await coordinator.makeStream(sceneID: UUID())
        for step in 0..<700 {
            let destination = (step * 17 + step / 7) % 5
            let previous = focus
            focus = try await coordinator.changeFocus(focus, to: sources[destination])
            let bytes = Data("[\(step):é界🦀]\r".utf8)
            let receipt = try await coordinator.submit(focus, encodedBytes: bytes)
            if step % 9 == 0 {
                let result = try await coordinator.cancel(receipt)
                XCTAssertEqual(result, .cancelled)
            } else { expected[destination] += bytes }
            if step % 13 == 0 {
                let composition = try await coordinator.beginComposition(focus)
                let composed = Data("候補\(step)".utf8)
                _ = try await coordinator.commitComposition(composition, encodedBytes: composed)
                expected[destination] += composed
            }
            if step % 7 == 0 {
                let hidden = (destination + 2) % 5, answer = Data("\u{1B}[0n".utf8)
                guard case .queued = try await coordinator.reply(sources[hidden], encodedBytes: answer) else {
                    XCTFail("unexpected reply pressure"); return
                }
                expected[hidden] += answer
            }
            do { _ = try await coordinator.submit(previous, encodedBytes: Data("stale".utf8)); XCTFail("old focus accepted") }
            catch let error as TerminalInputError { XCTAssertEqual(error, .stale) }
            _ = try await coordinator.drainOne(sources[(step * 3) % 5], maximumBytes: 3)
            if step % 31 == 0 {
                for source in sources { _ = try await coordinator.takeOutcomes(source) }
            }
            let accounting = await coordinator.accounting()
            XCTAssertLessThanOrEqual(accounting.retainedBytes, 1024 * 1024)
            XCTAssertLessThanOrEqual(accounting.retainedRecords, 4096)
        }
        for index in 0..<5 {
            var finished = false
            for _ in 0..<100_000 {
                let result = try await coordinator.drainOne(sources[index], maximumBytes: 7)
                if result == .idle { finished = true; break }
                XCTAssertNotEqual(result, .connectionClosed)
            }
            XCTAssertTrue(finished)
            let actual = await transports[index].bytes()
            XCTAssertEqual(actual, expected[index], "source \(index) lost, duplicated, or received another source's bytes")
            while !(try await coordinator.takeOutcomes(sources[index], maximum: 256)).isEmpty { }
        }
        let final = await coordinator.accounting()
        XCTAssertEqual(final.retainedBytes, 0)
        XCTAssertEqual(final.retainedRecords, 0)
    }
}
