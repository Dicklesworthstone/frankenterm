import Foundation
import XCTest
@testable import FrankenTermKit

private actor RecordingTerminalTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    private var received = Data()
    private var callCount = 0
    private var closedCount = 0
    private var blocked = false
    private var scripted: [TerminalTransportWrite] = []
    private var pending: (Data, CheckedContinuation<TerminalTransportWrite, any Error>)?
    private var waitForCall: [CheckedContinuation<Void, Never>] = []
    private var closeResult: TerminalTransportWrite = .failed

    init(machine: String = "machine", route: String = "route", transportID: UUID = UUID(), channelID: UUID = UUID()) throws {
        identity = try TerminalConnectionIdentity(workspaceID: "workspace", machineID: machine, routeID: route,
            routeRevision: 1, transportID: transportID, channelID: channelID)
    }
    func setBlocked(_ value: Bool, closeResult: TerminalTransportWrite = .failed) {
        blocked = value; self.closeResult = closeResult
    }
    func script(_ results: [TerminalTransportWrite]) { scripted = results }
    func waitUntilWriting() async {
        if pending != nil { return }
        await withCheckedContinuation { waitForCall.append($0) }
    }
    private func apply(_ result: TerminalTransportWrite, bytes: Data) -> TerminalTransportWrite {
        if case .accepted(let n) = result, n > 0, n <= bytes.count { received += bytes.prefix(n) }
        return result
    }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        callCount += 1
        if blocked {
            return try await withCheckedThrowingContinuation { continuation in
                precondition(pending == nil, "one transport lease per channel")
                pending = (bytes, continuation)
                for waiter in waitForCall { waiter.resume() }
                waitForCall.removeAll()
            }
        }
        return apply(scripted.isEmpty ? .accepted(bytes.count) : scripted.removeFirst(), bytes: bytes)
    }
    func release(_ result: TerminalTransportWrite) {
        guard let (bytes, continuation) = pending else { return }
        pending = nil; blocked = false
        continuation.resume(returning: apply(result, bytes: bytes))
    }
    func failWithError() {
        guard let (_, continuation) = pending else { return }
        pending = nil; blocked = false
        continuation.resume(throwing: CancellationError())
    }
    func close() {
        closedCount += 1
        release(closeResult)
    }
    func bytes() -> Data { received }
    func calls() -> Int { callCount }
    func closes() -> Int { closedCount }
}

final class TerminalInputTests: XCTestCase, @unchecked Sendable {
    private func setup(limits: TerminalInputLimits = TerminalInputLimits(), machine: String = "machine") async throws
        -> (TerminalInputCoordinator, RecordingTerminalTransport, TerminalSource, TerminalFocus) {
        let coordinator = try TerminalInputCoordinator(limits: limits)
        let transport = try RecordingTerminalTransport(machine: machine)
        let source = try await coordinator.attach(transport)
        let initial = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(initial, to: source)
        return (coordinator, transport, source, focus)
    }
    private func drain(_ coordinator: TerminalInputCoordinator, _ source: TerminalSource, bytes: Int? = nil) async throws {
        for _ in 0..<10000 {
            let result = try await coordinator.drainOne(source, maximumBytes: bytes)
            if result == .idle || result == .connectionClosed { return }
            guard case .progressed = result else { XCTFail("unexpected stalled transport: \(result)"); return }
        }
        XCTFail("bounded test drain did not settle")
    }
    private func expect(_ expected: TerminalInputError, _ operation: () async throws -> Void,
                        file: StaticString = #filePath, line: UInt = #line) async {
        do { try await operation(); XCTFail("expected \(expected)", file: file, line: line) }
        catch let error as TerminalInputError { XCTAssertEqual(error, expected, file: file, line: line) }
        catch { XCTFail("unexpected error type", file: file, line: line) }
    }

    func testStalledSourceCannotBlockFocusOrRedirectAdmittedInput() async throws {
        let (coordinator, first, sourceA, focusA) = try await setup(machine: "build")
        let second = try RecordingTerminalTransport(machine: "gpu")
        let sourceB = try await coordinator.attach(second)
        await first.setBlocked(true)
        let input = try await coordinator.submit(focusA, encodedBytes: Data("password\r".utf8))
        let running = Task { try await coordinator.drainOne(sourceA) }
        await first.waitUntilWriting()
        let focusB = try await coordinator.changeFocus(focusA, to: sourceB, cleanup: Data("release-old".utf8))
        await expect(.stale) { _ = try await coordinator.submit(focusA, encodedBytes: Data("late".utf8)) }
        _ = try await coordinator.submit(focusB, encodedBytes: Data("new-host\r".utf8))
        let busy = try await coordinator.drainOne(sourceA)
        XCTAssertEqual(busy, .busy)
        try await drain(coordinator, sourceB)
        let before = await second.bytes()
        XCTAssertEqual(before, Data("new-host\r".utf8))
        await first.release(.accepted(2))
        _ = try await running.value
        try await drain(coordinator, sourceA)
        let oldBytes = await first.bytes()
        XCTAssertEqual(oldBytes, Data("password\rrelease-old".utf8))
        let outcomes = try await coordinator.takeOutcomes(sourceA)
        XCTAssertEqual(outcomes.first?.receipt, input)
        XCTAssertEqual(outcomes.first?.disposition, .transportAccepted)
        XCTAssertEqual(outcomes.first?.acceptedOriginalBytes, 9)
    }

    func testRepliesDoNotUseCurrentFocusAndNeverInterleaveWithPaste() async throws {
        let (coordinator, transport, source, focus) = try await setup()
        let other = try RecordingTerminalTransport(machine: "other")
        let destination = try await coordinator.attach(other)
        _ = try await coordinator.paste(focus, text: "hello\nworld", bracketed: true)
        let moved = try await coordinator.changeFocus(focus, to: destination)
        _ = try await coordinator.reply(source, encodedBytes: Data("\u{1B}[24;80R".utf8))
        _ = try await coordinator.submit(moved, encodedBytes: Data("B".utf8))
        try await drain(coordinator, source, bytes: 1)
        try await drain(coordinator, destination)
        let actual = await transport.bytes(), otherBytes = await other.bytes()
        XCTAssertEqual(actual, Data("\u{1B}[200~hello\nworld\u{1B}[201~\u{1B}[24;80R".utf8))
        XCTAssertEqual(otherBytes, Data("B".utf8))
    }

    func testPartialWritesAndWouldBlockDoNotReplayAnyAcceptedPrefix() async throws {
        let (coordinator, transport, source, focus) = try await setup()
        await transport.script([.wouldBlock, .accepted(2), .wouldBlock, .accepted(1)])
        _ = try await coordinator.submit(focus, encodedBytes: Data("abcdef".utf8))
        let first = try await coordinator.drainOne(source)
        XCTAssertEqual(first, .wouldBlock)
        let second = try await coordinator.drainOne(source)
        XCTAssertEqual(second, .progressed(2))
        let third = try await coordinator.drainOne(source)
        XCTAssertEqual(third, .wouldBlock)
        try await drain(coordinator, source)
        let actual = await transport.bytes(), accounting = await coordinator.accounting()
        XCTAssertEqual(actual, Data("abcdef".utf8))
        XCTAssertEqual(accounting.retainedBytes, 0)
        XCTAssertEqual(accounting.inFlightBytes, 0)
    }

    func testNormalAndCleanupByteAndRecordBudgetsAreIndependent() async throws {
        var limits = TerminalInputLimits()
        limits.bytesPerConnection = 8; limits.totalBytes = 8
        limits.recordsPerConnection = 1; limits.totalRecords = 1
        limits.cleanupBytesPerConnection = 16; limits.totalCleanupBytes = 16
        limits.maximumCleanupBytes = 16; limits.cleanupRecordsPerConnection = 1; limits.totalCleanupRecords = 1
        let (coordinator, transport, source, focus) = try await setup(limits: limits)
        _ = try await coordinator.submit(focus, encodedBytes: Data(repeating: 65, count: 8))
        await expect(.capacity) { _ = try await coordinator.submit(focus, encodedBytes: Data([66])) }
        let dropped = try await coordinator.reply(source, encodedBytes: Data("reply".utf8))
        XCTAssertEqual(dropped, .droppedWhole)
        let cleared = try await coordinator.changeFocus(focus, to: nil, cleanup: Data("release".utf8))
        XCTAssertNil(cleared.source)
        let full = await coordinator.accounting()
        XCTAssertEqual(full.retainedBytes, 8); XCTAssertEqual(full.cleanupReservationBytes, 7)
        XCTAssertEqual(full.droppedReplyUnits, 1)
        let _ = full
        try await drain(coordinator, source)
        let restored = try await coordinator.changeFocus(cleared, to: source)
        // Settled records still count until their outcomes are consumed.
        await expect(.capacity) { _ = try await coordinator.submit(restored, encodedBytes: Data([66])) }
        let outcomes = try await coordinator.takeOutcomes(source)
        XCTAssertEqual(outcomes.count, 2)
        _ = try await coordinator.submit(restored, encodedBytes: Data([66]))
        try await drain(coordinator, source)
        let actual = await transport.bytes()
        XCTAssertEqual(actual, Data("AAAAAAAAreleaseB".utf8))
    }

    func testCleanupFailureDoesNotCommitUserInitiatedFocusChange() async throws {
        var limits = TerminalInputLimits()
        limits.cleanupBytesPerConnection = 16; limits.totalCleanupBytes = 16; limits.maximumCleanupBytes = 16
        let (coordinator, _, source, focus) = try await setup(limits: limits)
        let next = try await coordinator.changeFocus(focus, to: source, cleanup: Data(repeating: 7, count: 16))
        await expect(.capacity) { _ = try await coordinator.changeFocus(next, to: nil, cleanup: Data([7])) }
        _ = try await coordinator.submit(next, encodedBytes: Data("still-here".utf8))
        let suspended = try await coordinator.suspend(next, cleanup: Data([7]))
        XCTAssertFalse(suspended)
        await expect(.stale) { _ = try await coordinator.submit(next, encodedBytes: Data("late".utf8)) }
    }

    func testCancelPasteAtEveryWireByteBoundaryKeepsUTF8AndDelimitersValid() async throws {
        let text = "aé界🦀z", start = Data("\u{1B}[200~".utf8), end = Data("\u{1B}[201~".utf8)
        let payload = Data(text.utf8), original = start + payload + end
        for prefix in 0..<original.count {
            let (coordinator, transport, source, focus) = try await setup()
            let receipt = try await coordinator.paste(focus, text: text, bracketed: true)
            if prefix > 0 { _ = try await coordinator.drainOne(source, maximumBytes: prefix) }
            let cancellation = try await coordinator.cancel(receipt)
            XCTAssertEqual(cancellation, .cancelled)
            _ = try await coordinator.reply(source, encodedBytes: Data("R".utf8))
            try await drain(coordinator, source, bytes: 1)
            let actual = await transport.bytes()
            let expected: Data
            if prefix == 0 { expected = Data("R".utf8) }
            else if prefix < start.count { expected = start + end + Data("R".utf8) }
            else if prefix < start.count + payload.count {
                var boundary = prefix - start.count
                while boundary < payload.count && payload[boundary] & 0xC0 == 0x80 { boundary += 1 }
                expected = start + payload.prefix(boundary) + end + Data("R".utf8)
            } else { expected = original + Data("R".utf8) }
            XCTAssertEqual(actual, expected, "wire prefix \(prefix)")
            XCTAssertNotNil(String(data: actual, encoding: .utf8))
            let outcomes = try await coordinator.takeOutcomes(source)
            XCTAssertEqual(outcomes.first?.acceptedOriginalBytes, prefix)
            XCTAssertEqual(outcomes.first?.disposition, prefix == 0 ? .cancelledBeforeWrite : .cancelledAfterPrefix)
            let accounting = await coordinator.accounting()
            XCTAssertEqual(accounting.cleanupReservationBytes, 0)
        }
    }

    func testUnbracketedCancellationFinishesOnlyThePartlyAcceptedScalar() async throws {
        let original = Data("a🦀z".utf8)
        for prefix in 0..<original.count {
            let (coordinator, transport, source, focus) = try await setup()
            let receipt = try await coordinator.paste(focus, text: "a🦀z", bracketed: false)
            if prefix > 0 { _ = try await coordinator.drainOne(source, maximumBytes: prefix) }
            _ = try await coordinator.cancel(receipt)
            try await drain(coordinator, source)
            var boundary = prefix
            while boundary > 0 && boundary < original.count && original[boundary] & 0xC0 == 0x80 { boundary += 1 }
            let actual = await transport.bytes()
            XCTAssertEqual(actual, Data(original.prefix(boundary)))
            XCTAssertNotNil(String(data: actual, encoding: .utf8))
        }
    }

    func testPasteCancellationDuringOutstandingWriteWaitsForItsActualPrefix() async throws {
        let (coordinator, transport, source, focus) = try await setup()
        await transport.setBlocked(true)
        let receipt = try await coordinator.paste(focus, text: "abcdef", bracketed: true)
        let running = Task { try await coordinator.drainOne(source, maximumBytes: 9) }
        await transport.waitUntilWriting()
        let cancellation = try await coordinator.cancel(receipt)
        XCTAssertEqual(cancellation, .pendingWrite)
        let before = await coordinator.accounting()
        XCTAssertEqual(before.inFlightBytes, 9)
        XCTAssertEqual(before.cleanupReservationBytes, 16)
        await transport.release(.accepted(8))
        _ = try await running.value
        _ = try await coordinator.submit(focus, encodedBytes: Data("next".utf8))
        try await drain(coordinator, source)
        let actual = await transport.bytes()
        XCTAssertEqual(actual, Data("\u{1B}[200~ab\u{1B}[201~next".utf8))
    }

    func testLostTransportNeverSendsPasteCleanupToReplacementChannel() async throws {
        let (coordinator, old, source, focus) = try await setup()
        await old.setBlocked(true)
        let receipt = try await coordinator.paste(focus, text: "secret", bracketed: true)
        let running = Task { try await coordinator.drainOne(source) }
        await old.waitUntilWriting()
        _ = try await coordinator.cancel(receipt)
        try await coordinator.disconnect(source)
        _ = try await running.value
        let replacement = try RecordingTerminalTransport()
        let newSource = try await coordinator.attach(replacement)
        let newFocus = try await coordinator.changeFocus(focus, to: newSource)
        _ = try await coordinator.submit(newFocus, encodedBytes: Data("new".utf8))
        try await drain(coordinator, newSource)
        let actual = await replacement.bytes()
        XCTAssertEqual(actual, Data("new".utf8))
        let outcomes = try await coordinator.takeOutcomes(source)
        XCTAssertEqual(outcomes.count, 1)
        XCTAssertEqual(outcomes[0].disposition, .connectionLost(leasedTailUnknown: true))
        XCTAssertEqual(outcomes[0].acceptedCleanupBytes, 0)
    }

    func testCompositionCommitsExactlyOnceAndNeverSynthesizesEnter() async throws {
        let (coordinator, transport, source, focus) = try await setup()
        let composition = try await coordinator.beginComposition(focus)
        let same = try await coordinator.beginComposition(focus)
        XCTAssertEqual(same, composition)
        await expect(.compositionPending) { _ = try await coordinator.changeFocus(focus, to: nil) }
        await expect(.compositionPending) { _ = try await coordinator.submit(focus, encodedBytes: Data("duplicate".utf8)) }
        _ = try await coordinator.commitComposition(composition, encodedBytes: Data("你好é".utf8))
        await expect(.stale) { _ = try await coordinator.commitComposition(composition, encodedBytes: Data("again".utf8)) }
        _ = try await coordinator.changeFocus(focus, to: nil)
        try await drain(coordinator, source)
        let actual = await transport.bytes()
        XCTAssertEqual(actual, Data("你好é".utf8))
    }

    func testFailedCompositionAdmissionRetainsOriginalContextForExplicitCancellation() async throws {
        var limits = TerminalInputLimits()
        limits.bytesPerConnection = 4; limits.totalBytes = 4
        let (coordinator, transport, source, focus) = try await setup(limits: limits)
        let composition = try await coordinator.beginComposition(focus)
        await expect(.capacity) { _ = try await coordinator.commitComposition(composition, encodedBytes: Data("too large".utf8)) }
        await expect(.compositionPending) { _ = try await coordinator.changeFocus(focus, to: nil) }
        try await coordinator.cancelComposition(composition)
        _ = try await coordinator.changeFocus(focus, to: nil)
        try await drain(coordinator, source)
        let actual = await transport.bytes()
        XCTAssertTrue(actual.isEmpty)
    }

    func testSuspensionRetiresCompositionAndLateResponderCallbacks() async throws {
        let (coordinator, transport, source, focus) = try await setup()
        let composition = try await coordinator.beginComposition(focus)
        let suspended = try await coordinator.suspend(focus, cleanup: Data("release".utf8))
        XCTAssertTrue(suspended)
        await expect(.stale) { _ = try await coordinator.commitComposition(composition, encodedBytes: Data("late password".utf8)) }
        await expect(.stale) { _ = try await coordinator.changeFocus(focus, to: source) }
        let resumed = try await coordinator.makeStream(sceneID: focus.sceneID)
        let fresh = try await coordinator.changeFocus(resumed, to: source)
        _ = try await coordinator.submit(fresh, encodedBytes: Data("fresh".utf8))
        try await drain(coordinator, source)
        let actual = await transport.bytes()
        XCTAssertEqual(actual, Data("releasefresh".utf8))
    }

    func testOtherSceneRetainsItsIndependentInputStream() async throws {
        let (coordinator, transport, source, first) = try await setup()
        let next = try await coordinator.makeStream(sceneID: UUID())
        let second = try await coordinator.changeFocus(next, to: source)
        _ = try await coordinator.changeFocus(first, to: nil)
        _ = try await coordinator.submit(second, encodedBytes: Data("scene-two".utf8))
        await expect(.stale) { _ = try await coordinator.submit(first, encodedBytes: Data("scene-one-late".utf8)) }
        try await drain(coordinator, source)
        let actual = await transport.bytes()
        XCTAssertEqual(actual, Data("scene-two".utf8))
    }

    func testOrdinaryEncodedUnitCannotBeCancelledAfterPartialAcceptance() async throws {
        let (coordinator, transport, source, focus) = try await setup()
        let receipt = try await coordinator.submit(focus, encodedBytes: Data("\u{1B}[1;5A".utf8))
        _ = try await coordinator.drainOne(source, maximumBytes: 2)
        let result = try await coordinator.cancel(receipt)
        XCTAssertEqual(result, .tooLate)
        try await drain(coordinator, source)
        let actual = await transport.bytes()
        XCTAssertEqual(actual, Data("\u{1B}[1;5A".utf8))
    }

    func testQueuedCancellationHasNoEffectAndReleasesBytes() async throws {
        let (coordinator, transport, source, focus) = try await setup()
        let a = try await coordinator.submit(focus, encodedBytes: Data("A".utf8))
        let b = try await coordinator.submit(focus, encodedBytes: Data("B".utf8))
        _ = try await coordinator.cancel(b)
        _ = try await coordinator.cancel(a)
        try await drain(coordinator, source)
        let actual = await transport.bytes(), accounting = await coordinator.accounting()
        XCTAssertTrue(actual.isEmpty)
        XCTAssertEqual(accounting.retainedBytes, 0)
        let outcomes = try await coordinator.takeOutcomes(source)
        XCTAssertEqual(outcomes.count, 2)
        XCTAssertTrue(outcomes.allSatisfy { $0.disposition == .cancelledBeforeWrite })
    }

    func testStopAdmissionSettlesQueuedInputWithoutWaitingForTransportClose() async throws {
        let (coordinator, transport, source, focus) = try await setup()
        await transport.setBlocked(true)
        let leased = try await coordinator.submit(focus, encodedBytes: Data("leased".utf8))
        let queued = try await coordinator.submit(focus, encodedBytes: Data("queued".utf8))
        let running = Task { try await coordinator.drainOne(source) }
        await transport.waitUntilWriting()

        try await coordinator.stopAdmission(source)
        try await coordinator.stopAdmission(source) // The barrier is idempotent.
        await expect(.unavailable) { _ = try await coordinator.submit(focus, encodedBytes: Data("late".utf8)) }
        await expect(.unavailable) { _ = try await coordinator.reply(source, encodedBytes: Data("reply".utf8)) }
        let refused = try await coordinator.drainOne(source)
        XCTAssertEqual(refused, .connectionClosed)
        let stopped = try await coordinator.pendingOutcomes(source)
        XCTAssertEqual(stopped.outcomes.map(\.receipt), [queued])
        XCTAssertEqual(stopped.outcomes[0].disposition, .definitelyNotWritten)
        let initialCloses = await transport.closes(), calls = await transport.calls()
        XCTAssertEqual(initialCloses, 0)
        XCTAssertEqual(calls, 1)

        // The previously leased write still owns its eventual exact result.
        await transport.release(.wouldBlock)
        let settled = try await running.value
        XCTAssertEqual(settled, .connectionClosed)
        let outcomes = try await coordinator.pendingOutcomes(source)
        XCTAssertEqual(outcomes.outcomes.map(\.receipt), [queued, leased])
        XCTAssertTrue(outcomes.outcomes.allSatisfy { $0.disposition == .definitelyNotWritten })
        try await coordinator.acknowledgeOutcomes(outcomes)
        _ = try await coordinator.suspend(focus)
        await expect(.unavailable) { try await coordinator.retire(source) }
        let beforeDisconnect = await transport.closes(), bytes = await transport.bytes()
        XCTAssertEqual(beforeDisconnect, 0)
        XCTAssertTrue(bytes.isEmpty)

        try await coordinator.disconnect(source)
        try await coordinator.retire(source)
        let finalCloses = await transport.closes()
        XCTAssertEqual(finalCloses, 1)
    }

    func testDisconnectPreservesLateExactZeroOrPrefixEvidence() async throws {
        for (answer, accepted, disposition) in [
            (TerminalTransportWrite.wouldBlock, 0, TerminalInputDisposition.definitelyNotWritten),
            (.accepted(2), 2, .connectionLost(leasedTailUnknown: false)),
            (.failed, 0, .connectionLost(leasedTailUnknown: true))
        ] {
            let (coordinator, transport, source, focus) = try await setup()
            await transport.setBlocked(true, closeResult: answer)
            _ = try await coordinator.submit(focus, encodedBytes: Data("first".utf8))
            _ = try await coordinator.submit(focus, encodedBytes: Data("queued".utf8))
            let running = Task { try await coordinator.drainOne(source) }
            await transport.waitUntilWriting()
            try await coordinator.disconnect(source)
            _ = try await running.value
            let outcomes = try await coordinator.takeOutcomes(source)
            XCTAssertEqual(outcomes.count, 2)
            XCTAssertEqual(outcomes[0].disposition, .definitelyNotWritten)
            XCTAssertEqual(outcomes[1].acceptedOriginalBytes, accepted)
            XCTAssertEqual(outcomes[1].disposition, disposition)
            let closes = await transport.closes()
            XCTAssertEqual(closes, 1)
        }
    }

    /// What the person reads after the connection ends: input that never left
    /// this phone, and input that left it with its arrival unknown. Accepted
    /// input and the terminal's own replies are not theirs to worry about.
    func testAnEndedConnectionSaysWhichInputNeverLeftAndWhichMayHaveArrived() async throws {
        for (answer, unanswered) in [(TerminalTransportWrite.wouldBlock, false), (.accepted(2), true), (.failed, true)] {
            let (coordinator, transport, source, focus) = try await setup()
            await transport.setBlocked(true, closeResult: answer)
            _ = try await coordinator.submit(focus, encodedBytes: Data("first".utf8))
            _ = try await coordinator.submit(focus, encodedBytes: Data("queued".utf8))
            let running = Task { try await coordinator.drainOne(source) }
            await transport.waitUntilWriting()
            try await coordinator.disconnect(source)
            _ = try await running.value
            let outcomes = try await coordinator.takeOutcomes(source)
            var delivery = TerminalInputDelivery()
            delivery.record(outcomes)
            XCTAssertTrue(delivery.unsent, "the queued input never left (\(answer))")
            XCTAssertEqual(delivery.unanswered, unanswered, "the written input's arrival is unknown (\(answer))")
            XCTAssertFalse(delivery.stoppedPartway)
            // A mux server's confirmation, and its pane's own notice, speak for written input.
            var mux = TerminalInputDelivery()
            mux.record(outcomes, serverConfirms: true)
            XCTAssertTrue(mux.unsent, "\(answer)")
            XCTAssertFalse(mux.unanswered, "\(answer)")
            let message = try XCTUnwrap(delivery.message(machine: "Alpha"))
            XCTAssertEqual(message.contains("It may or may not have arrived."), unanswered, message)
            XCTAssertTrue(message.contains("never left this phone and was not sent."), message)
            XCTAssertTrue(message.hasSuffix("Nothing is sent again by itself: check the screen before you type it again."), message)
        }

        // A reply the connection lost mid-write and accepted input are not the person's interrupted input.
        let (coordinator, transport, source, focus) = try await setup()
        _ = try await coordinator.submit(focus, encodedBytes: Data("ls\r".utf8))
        try await drain(coordinator, source)
        await transport.setBlocked(true, closeResult: .failed)
        _ = try await coordinator.reply(source, encodedBytes: Data("\u{1b}[0n".utf8))
        let running = Task { try await coordinator.drainOne(source) }
        await transport.waitUntilWriting()
        try await coordinator.disconnect(source)
        _ = try await running.value
        let outcomes = try await coordinator.takeOutcomes(source)
        XCTAssertEqual(outcomes.map(\.kind), [.encodedInput, .reply])
        XCTAssertEqual(outcomes.last?.disposition, .connectionLost(leasedTailUnknown: true))
        var delivery = TerminalInputDelivery()
        delivery.record(outcomes)
        XCTAssertTrue(delivery.isEmpty)
        XCTAssertNil(delivery.message(machine: "Alpha"))
        delivery.endedUnanswered()
        XCTAssertEqual(delivery.message(machine: "Alpha"),
                       "Your last input left this phone, but the connection ended before Alpha answered it. It may or may not have arrived. Nothing is sent again by itself: check the screen before you type it again.")

        // A paste stopped after its beginning was sent.
        let (pasting, _, pasteSource, pasteFocus) = try await setup()
        let paste = try await pasting.paste(pasteFocus, text: "echo stopped", bracketed: true)
        _ = try await pasting.drainOne(pasteSource, maximumBytes: 8)
        _ = try await pasting.cancel(paste)
        try await drain(pasting, pasteSource)
        var stopped = TerminalInputDelivery()
        stopped.record(try await pasting.takeOutcomes(pasteSource))
        XCTAssertTrue(stopped.stoppedPartway)
        XCTAssertFalse(stopped.unsent || stopped.unanswered)
        XCTAssertEqual(stopped.message(machine: "Alpha"),
                       "Some input was stopped after only part of it was sent. Nothing is sent again by itself: check the screen before you type it again.")
    }

    func testInvalidTransportCountsCloseWithoutInventingAcceptedBytes() async throws {
        for result in [TerminalTransportWrite.accepted(0), .accepted(-1), .accepted(Int.max), .failed] {
            let (coordinator, transport, source, focus) = try await setup()
            await transport.script([result])
            _ = try await coordinator.submit(focus, encodedBytes: Data("abc".utf8))
            let drained = try await coordinator.drainOne(source)
            XCTAssertEqual(drained, .connectionClosed)
            await expect(.unavailable) { _ = try await coordinator.submit(focus, encodedBytes: Data("later".utf8)) }
            let outcome = try await coordinator.takeOutcomes(source)
            XCTAssertEqual(outcome.first?.acceptedOriginalBytes, 0)
            XCTAssertEqual(outcome.first?.disposition, .connectionLost(leasedTailUnknown: true))
        }
    }

    func testThrownCancellationHasUnknownEffectRatherThanAutomaticReplay() async throws {
        let (coordinator, transport, source, focus) = try await setup()
        await transport.setBlocked(true)
        _ = try await coordinator.submit(focus, encodedBytes: Data("dangerous\r".utf8))
        let running = Task { try await coordinator.drainOne(source) }
        await transport.waitUntilWriting()
        await transport.failWithError()
        let result = try await running.value
        XCTAssertEqual(result, .connectionClosed)
        let outcomes = try await coordinator.takeOutcomes(source)
        XCTAssertEqual(outcomes.first?.disposition, .connectionLost(leasedTailUnknown: true))
        let calls = await transport.calls()
        XCTAssertEqual(calls, 1)
    }

    func testStaleAndForeignHandlesCannotReachTransport() async throws {
        let (first, transport, source, focus) = try await setup()
        let (second, _, _, _) = try await setup()
        await expect(.stale) { _ = try await second.submit(focus, encodedBytes: Data("wrong coordinator".utf8)) }
        await expect(.stale) { _ = try await second.drainOne(source) }
        await expect(.invalidIdentity) { _ = try await first.attach(transport) }
        let lost = try await first.changeFocus(focus, to: nil)
        try await first.disconnect(source)
        _ = try await first.suspend(lost)
        try await first.retire(source)
        let replacement = try RecordingTerminalTransport(machine: source.identity.machineID,
            route: source.identity.routeID, transportID: source.identity.transportID, channelID: source.identity.channelID)
        let fresh = try await first.attach(replacement)
        XCTAssertNotEqual(source, fresh)
        await expect(.stale) { _ = try await first.reply(source, encodedBytes: Data("old reply".utf8)) }
        let actual = await replacement.bytes()
        XCTAssertTrue(actual.isEmpty)
    }

    func testGlobalCapsAccountForOtherConnectionsAndCompletedRecords() async throws {
        var limits = TerminalInputLimits()
        limits.connections = 2; limits.streams = 2; limits.bytesPerConnection = 4; limits.totalBytes = 4
        limits.recordsPerConnection = 2; limits.totalRecords = 2
        let (coordinator, _, sourceA, focusA) = try await setup(limits: limits)
        let other = try RecordingTerminalTransport(machine: "other")
        let sourceB = try await coordinator.attach(other)
        let initialB = try await coordinator.makeStream(sceneID: UUID())
        let focusB = try await coordinator.changeFocus(initialB, to: sourceB)
        _ = try await coordinator.submit(focusA, encodedBytes: Data("AAAA".utf8))
        await expect(.capacity) { _ = try await coordinator.submit(focusB, encodedBytes: Data("B".utf8)) }
        try await drain(coordinator, sourceA)
        _ = try await coordinator.submit(focusB, encodedBytes: Data("B".utf8))
        try await drain(coordinator, sourceB)
        await expect(.capacity) { _ = try await coordinator.submit(focusA, encodedBytes: Data("C".utf8)) }
        _ = try await coordinator.takeOutcomes(sourceA)
        _ = try await coordinator.submit(focusA, encodedBytes: Data("C".utf8))
        await expect(.capacity) { _ = try await coordinator.attach(RecordingTerminalTransport(machine: "third")) }
        await expect(.capacity) { _ = try await coordinator.makeStream(sceneID: UUID()) }
    }

    func testPasteAdmissionRequiresPrepaidCleanupAndRejectsControlInjection() async throws {
        var limits = TerminalInputLimits()
        limits.cleanupBytesPerConnection = 16; limits.totalCleanupBytes = 16; limits.maximumCleanupBytes = 16
        let (coordinator, _, source, focus) = try await setup(limits: limits)
        _ = try await coordinator.paste(focus, text: "A", bracketed: true)
        await expect(.capacity) { _ = try await coordinator.paste(focus, text: "B", bracketed: true) }
        for text in ["\u{1B}[201~bad", "\u{9B}201~bad", "\0", "\u{7F}", "\u{07}"] {
            await expect(.unsafePaste) { _ = try await coordinator.paste(focus, text: text, bracketed: true) }
        }
        await expect(.emptyInput) { _ = try await coordinator.paste(focus, text: "", bracketed: false) }
        try await drain(coordinator, source)
        _ = try await coordinator.paste(focus, text: "tab\there\r\n", bracketed: true)
    }

    func testInvalidLimitsIdentityAndDriverSizesFailWithoutTraps() async throws {
        for n in [Int.min, -1, 0, Int.max] {
            var limits = TerminalInputLimits(); limits.bytesPerConnection = n
            XCTAssertThrowsError(try TerminalInputCoordinator(limits: limits))
            limits = TerminalInputLimits(); limits.maximumWriteBytes = n
            XCTAssertThrowsError(try TerminalInputCoordinator(limits: limits))
        }
        for name in ["", String(repeating: "a", count: 129), "with space", "\n", "hostname/path"] {
            XCTAssertThrowsError(try TerminalConnectionIdentity(workspaceID: "work", machineID: name, routeID: "route",
                routeRevision: 1, transportID: UUID(), channelID: UUID()))
        }
        let (coordinator, _, source, _) = try await setup()
        for n in [Int.min, -1, 0, 65537, Int.max] {
            await expect(.invalidWriteSize) { _ = try await coordinator.drainOne(source, maximumBytes: n) }
        }
        let count = await coordinator.accounting()
        XCTAssertEqual(count.retainedRecords, 0)
    }
}
