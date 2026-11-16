import Foundation
import XCTest
@testable import FrankenTermKit

private enum RecoveryTestError: Error { case unexpected }
private actor RecoveryTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    private var output = Data()
    private var closes = 0
    init(_ machine: String = "build") throws {
        identity = try .init(workspaceID: "w", machineID: machine, routeID: "r", routeRevision: 1,
            transportID: UUID(), channelID: UUID())
    }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        let count = min(3, bytes.count); output.append(bytes.prefix(count)); return .accepted(count)
    }
    func close() async { closes += 1 }
    func bytes() -> Data { output }
    func closeCount() -> Int { closes }
}

/// Synthetic encoding exposes order; it is not the Rust terminal. The complete
/// production keyboard and input coordinator perform queueing and delivery.
private actor RecoveryEncoder: TerminalKeyboardTransactions {
    nonisolated let supportsPaste = true
    private var failure: (any Error & Sendable)?
    private var afterTransaction: (any Error & Sendable)?
    private var parked = false, entered = false
    private var waiter: CheckedContinuation<Void, Never>?
    func failOnce(_ error: any Error & Sendable) { failure = error }
    func failAfterTransaction(_ error: any Error & Sendable) { afterTransaction = error }
    func park() { parked = true; entered = false }
    func didEnter() -> Bool { entered }
    func release() { parked = false; waiter?.resume(); waiter = nil }
    func withInputTransaction<Value: Sendable>(source: TerminalSource,
        operation: @Sendable () async throws -> Value) async throws -> Value {
        let result = try await operation()
        if let error = afterTransaction { afterTransaction = nil; throw error }
        return result
    }
    func encode(_ intent: TerminalKeyboardIntent, source: TerminalSource, maximumBytes: Int) async throws -> Data {
        entered = true
        if parked { await withCheckedContinuation { waiter = $0 } }
        try Task.checkCancellation()
        if let error = failure { failure = nil; throw error }
        let bytes: Data
        switch intent {
        case .text(let text): bytes = Data(text.utf8)
        case .key(let key, let phase):
            let tag: String
            switch phase { case .down: tag = "D"; case .repeated: tag = "R"; case .up: tag = "U"; case .press: tag = "P" }
            bytes = Data("<\(tag):\(key.usage)>".utf8)
        case .releaseAll(let keys): bytes = Data(keys.map { "<U:\($0.usage)>" }.joined().utf8)
        }
        guard bytes.count <= maximumBytes else { throw TerminalKeyboardError.capacity }
        return bytes
    }
    func preparePaste(_ text: String, source: TerminalSource, maximumBytes: Int) async throws -> TerminalPreparedPaste {
        guard text.utf8.count <= maximumBytes else { throw TerminalKeyboardError.capacity }
        return .init(text: text, bracketed: true)
    }
}

@MainActor
final class TerminalKeyboardRecoveryTests: XCTestCase {
    private struct Fixture {
        let coordinator: TerminalInputCoordinator, transport: RecoveryTransport
        let source: TerminalSource, encoder: RecoveryEncoder, keyboard: TerminalKeyboardSession
    }
    private func fixture(capacity: Int = 8, events: Int = 128, bytes: Int = 32_768) async throws -> Fixture {
        var limits = TerminalInputLimits(); limits.bytesPerConnection = capacity; limits.totalBytes = capacity
        let coordinator = try TerminalInputCoordinator(limits: limits), transport = try RecoveryTransport()
        let source = try await coordinator.attach(transport), empty = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(empty, to: source), encoder = RecoveryEncoder()
        return Fixture(coordinator: coordinator, transport: transport, source: source, encoder: encoder,
            keyboard: try TerminalKeyboardSession(focus: focus, coordinator: coordinator, encoder: encoder,
                maximumEvents: events, maximumBytes: bytes))
    }
    private func drain(_ f: Fixture) async throws {
        for _ in 0..<100_000 {
            if try await f.coordinator.drainOne(f.source) == .idle {
                _ = try await f.coordinator.takeOutcomes(f.source); return
            }
        }
        XCTFail("controlled transport did not drain")
    }
    private func block(_ f: Fixture, head: String = "AA", tail: String = "BB") async throws -> TerminalKeyboardRecovery {
        try f.keyboard.insertText("12345678"); await f.keyboard.waitUntilSettled()
        try f.keyboard.insertText(head); try f.keyboard.insertText(tail); await f.keyboard.waitUntilSettled()
        return try XCTUnwrap(f.keyboard.recovery)
    }
    private func finish(_ f: Fixture) async {
        f.keyboard.suspend(); await f.keyboard.waitUntilSuspended(); try? await f.coordinator.disconnect(f.source)
    }
    private func waitForEncoder(_ encoder: RecoveryEncoder) async {
        for _ in 0..<10_000 { if await encoder.didEnter() { return }; await Task.yield() }
        XCTFail("encoder did not enter")
    }
    func testRetryRetainsSuffixAndNeverDuplicatesAdmittedPrefix() async throws {
        let f = try await fixture(), recovery = try await block(f)
        XCTAssertEqual(recovery.keyboardID, f.keyboard.id); XCTAssertEqual(recovery.focus, f.keyboard.originalFocus)
        XCTAssertEqual(recovery.accounting.pendingEvents, 2); XCTAssertEqual(recovery.accounting.pendingUTF8Bytes, 4)
        XCTAssertEqual(recovery.accounting.admittedEvents, 1); XCTAssertEqual(recovery.failure, .capacity)
        XCTAssertTrue(recovery.canRetry); try await drain(f)
        try f.keyboard.retryLocalAdmission(expectedRecovery: recovery.id)
        XCTAssertNil(f.keyboard.recovery)
        XCTAssertThrowsError(try f.keyboard.retryLocalAdmission(expectedRecovery: recovery.id))
        await f.keyboard.waitUntilSettled(); try await drain(f)
        let output = await f.transport.bytes(), closes = await f.transport.closeCount()
        XCTAssertEqual(output, Data("12345678AABB".utf8)); XCTAssertEqual(closes, 0); await finish(f)
    }
    func testFreeCapacityDoesNotAutomaticallyRetryInput() async throws {
        let f = try await fixture(), recovery = try await block(f)
        try await drain(f); for _ in 0..<100 { await Task.yield() }
        let output = await f.transport.bytes(); XCTAssertEqual(output, Data("12345678".utf8))
        XCTAssertEqual(f.keyboard.phase, .blocked); XCTAssertEqual(f.keyboard.recovery, recovery); await finish(f)
    }
    func testSecondFailureRequiresNewDecisionOnSameKeyboard() async throws {
        let f = try await fixture(), original = try await block(f)
        try f.keyboard.retryLocalAdmission(expectedRecovery: original.id); await f.keyboard.waitUntilSettled()
        let next = try XCTUnwrap(f.keyboard.recovery); XCTAssertNotEqual(next.id, original.id)
        XCTAssertThrowsError(try f.keyboard.retryLocalAdmission(expectedRecovery: original.id)) {
            XCTAssertEqual($0 as? TerminalInputError, .stale)
        }
        XCTAssertThrowsError(try f.keyboard.discardLocalAdmission(expectedRecovery: original.id))
        XCTAssertEqual(f.keyboard.recovery, next)
        try await drain(f); try f.keyboard.retryLocalAdmission(expectedRecovery: next.id)
        await f.keyboard.waitUntilSettled(); try await drain(f)
        let output = await f.transport.bytes(); XCTAssertEqual(output, Data("12345678AABB".utf8)); await finish(f)
    }
    func testAnotherKeyboardCannotConsumeDecision() async throws {
        let a = try await fixture(), b = try await fixture(), old = try await block(a), current = try await block(b)
        XCTAssertThrowsError(try b.keyboard.retryLocalAdmission(expectedRecovery: old.id))
        XCTAssertThrowsError(try b.keyboard.discardLocalAdmission(expectedRecovery: old.id))
        XCTAssertEqual(b.keyboard.recovery, current); await finish(a); await finish(b)
    }
    func testDiscardDropsOnlyUnadmittedTextAndKeepsConnectionOpen() async throws {
        let f = try await fixture(), recovery = try await block(f)
        try f.keyboard.discardLocalAdmission(expectedRecovery: recovery.id)
        XCTAssertNil(f.keyboard.recovery); XCTAssertEqual(f.keyboard.phase, .suspending)
        await f.keyboard.waitUntilSuspended(); try await drain(f)
        let output = await f.transport.bytes(), closes = await f.transport.closeCount()
        XCTAssertEqual(output, Data("12345678".utf8)); XCTAssertEqual(closes, 0)
        XCTAssertEqual(f.keyboard.accounting.discardedUnadmittedEvents, 2)
        let vacant = try await f.coordinator.makeStream(sceneID: UUID())
        let focus = try await f.coordinator.changeFocus(vacant, to: f.source)
        _ = try await f.coordinator.submit(focus, encodedBytes: Data("next".utf8)); try await drain(f)
        let later = await f.transport.bytes(); XCTAssertEqual(later, Data("12345678next".utf8))
        _ = try await f.coordinator.suspend(focus); await finish(f)
    }
    func testDiscardSettlesHeldKeyWithoutCommittingComposition() async throws {
        let f = try await fixture(capacity: 5)
        try f.keyboard.key(TerminalHardwareKey(usage: 4), phase: .down); await f.keyboard.waitUntilSettled()
        await f.encoder.failOnce(TerminalKeyboardError.encoding)
        try f.keyboard.setMarkedText("秘密", selectedRange: NSRange(location: 2, length: 0))
        await f.keyboard.waitUntilSettled()
        let recovery = try XCTUnwrap(f.keyboard.recovery)
        XCTAssertEqual(recovery.accounting.heldKeys, 1); XCTAssertEqual(recovery.accounting.compositionUTF8Bytes, 6)
        try f.keyboard.discardLocalAdmission(expectedRecovery: recovery.id)
        await f.keyboard.waitUntilSuspended(); try await drain(f)
        let output = await f.transport.bytes(); XCTAssertEqual(output, Data("<D:4><U:4>".utf8))
        XCTAssertFalse(f.keyboard.document.isMarked); XCTAssertEqual(f.keyboard.accounting.heldKeys, 0); await finish(f)
    }
    func testCompositionRetryCommitsExactlyOnceWithoutReturn() async throws {
        let f = try await fixture()
        try f.keyboard.insertText("12345678"); await f.keyboard.waitUntilSettled()
        try f.keyboard.setMarkedText("候補", selectedRange: NSRange(location: 2, length: 0))
        try f.keyboard.unmarkText(); await f.keyboard.waitUntilSettled()
        let recovery = try XCTUnwrap(f.keyboard.recovery)
        try await drain(f); try f.keyboard.retryLocalAdmission(expectedRecovery: recovery.id)
        await f.keyboard.waitUntilSettled(); try await drain(f)
        let output = await f.transport.bytes(); XCTAssertEqual(output, Data("12345678候補".utf8)); await finish(f)
    }
    func testPasteRetryUsesOriginalReviewAndCoordinatorBrackets() async throws {
        let f = try await fixture(capacity: 32)
        try f.keyboard.insertText(String(repeating: "x", count: 32)); await f.keyboard.waitUntilSettled()
        let review = try TerminalPasteReview(text: "語\n界", keyboardID: f.keyboard.id, focus: f.keyboard.originalFocus)
        try f.keyboard.paste(review); await f.keyboard.waitUntilSettled()
        let recovery = try XCTUnwrap(f.keyboard.recovery)
        try await drain(f); try f.keyboard.retryLocalAdmission(expectedRecovery: recovery.id)
        await f.keyboard.waitUntilSettled(); try await drain(f)
        let output = await f.transport.bytes()
        XCTAssertEqual(output, Data((String(repeating: "x", count: 32) + "\u{1b}[200~語\n界\u{1b}[201~").utf8)); await finish(f)
    }
    func testKnownEncoderRejectionRetryButUnknownFailureDiscardOnly() async throws {
        let f = try await fixture(capacity: 128)
        await f.encoder.failOnce(TerminalKeyboardError.encoding)
        try f.keyboard.insertText("é🦀"); await f.keyboard.waitUntilSettled()
        let first = try XCTUnwrap(f.keyboard.recovery); XCTAssertTrue(first.canRetry)
        try f.keyboard.retryLocalAdmission(expectedRecovery: first.id); await f.keyboard.waitUntilSettled(); try await drain(f)
        await f.encoder.failOnce(RecoveryTestError.unexpected)
        try f.keyboard.insertText("unavailable"); await f.keyboard.waitUntilSettled()
        let second = try XCTUnwrap(f.keyboard.recovery); XCTAssertFalse(second.canRetry)
        XCTAssertThrowsError(try f.keyboard.retryLocalAdmission(expectedRecovery: second.id))
        try f.keyboard.discardLocalAdmission(expectedRecovery: second.id)
        await f.keyboard.waitUntilSuspended(); try await drain(f)
        let output = await f.transport.bytes(); XCTAssertEqual(output, Data("é🦀".utf8)); await finish(f)
    }
    func testTransactionErrorAfterAdmissionNeverReplaysAcceptedHead() async throws {
        let f = try await fixture(capacity: 128)
        await f.encoder.failAfterTransaction(TerminalKeyboardError.capacity)
        try f.keyboard.insertText("already"); try f.keyboard.insertText("unsent"); await f.keyboard.waitUntilSettled()
        let recovery = try XCTUnwrap(f.keyboard.recovery)
        XCTAssertFalse(recovery.canRetry); XCTAssertEqual(recovery.accounting.pendingEvents, 1)
        XCTAssertEqual(recovery.accounting.pendingUTF8Bytes, 6); XCTAssertEqual(recovery.accounting.admittedEvents, 1)
        XCTAssertThrowsError(try f.keyboard.retryLocalAdmission(expectedRecovery: recovery.id))
        try f.keyboard.discardLocalAdmission(expectedRecovery: recovery.id)
        await f.keyboard.waitUntilSuspended(); try await drain(f)
        let output = await f.transport.bytes(); XCTAssertEqual(output, Data("already".utf8))
        XCTAssertEqual(f.keyboard.accounting.discardedUnadmittedEvents, 1); await finish(f)
    }
    func testAcceptedLastEventCanResetButCannotReplay() async throws {
        let f = try await fixture(capacity: 128)
        await f.encoder.failAfterTransaction(TerminalKeyboardError.encoding)
        try f.keyboard.insertText("only"); await f.keyboard.waitUntilSettled()
        let recovery = try XCTUnwrap(f.keyboard.recovery)
        XCTAssertEqual(recovery.accounting.pendingEvents, 0); XCTAssertFalse(recovery.canRetry)
        try f.keyboard.discardLocalAdmission(expectedRecovery: recovery.id)
        await f.keyboard.waitUntilSuspended(); try await drain(f)
        let output = await f.transport.bytes(); XCTAssertEqual(output, Data("only".utf8))
        XCTAssertEqual(f.keyboard.accounting.discardedUnadmittedEvents, 0); await finish(f)
    }
    func testStaleFocusOffersDiscardNeverRetry() async throws {
        let f = try await fixture(); _ = try await f.coordinator.suspend(f.keyboard.originalFocus)
        try f.keyboard.insertText("stale"); await f.keyboard.waitUntilSettled()
        let recovery = try XCTUnwrap(f.keyboard.recovery); XCTAssertFalse(recovery.canRetry)
        XCTAssertThrowsError(try f.keyboard.retryLocalAdmission(expectedRecovery: recovery.id))
        try f.keyboard.discardLocalAdmission(expectedRecovery: recovery.id); await f.keyboard.waitUntilSuspended()
        let output = await f.transport.bytes(); XCTAssertTrue(output.isEmpty); await finish(f)
    }
    func testImmediateQueueRefusalDoesNotBlockOrRetainRejectedText() async throws {
        let f = try await fixture(capacity: 128, events: 2, bytes: 16)
        await f.encoder.park(); try f.keyboard.insertText("12345678"); await waitForEncoder(f.encoder)
        try f.keyboard.insertText("abcdefgh"); XCTAssertThrowsError(try f.keyboard.insertText("rejected"))
        XCTAssertNil(f.keyboard.recovery); XCTAssertEqual(f.keyboard.phase, .active)
        XCTAssertEqual(f.keyboard.accounting.pendingEvents, 2)
        await f.encoder.release(); await f.keyboard.waitUntilSettled(); try await drain(f)
        let output = await f.transport.bytes(); XCTAssertEqual(output, Data("12345678abcdefgh".utf8)); await finish(f)
    }
    func testSuspensionInvalidatesDecisionSynchronously() async throws {
        let f = try await fixture(), recovery = try await block(f)
        f.keyboard.suspend(); XCTAssertNil(f.keyboard.recovery)
        XCTAssertThrowsError(try f.keyboard.retryLocalAdmission(expectedRecovery: recovery.id))
        XCTAssertThrowsError(try f.keyboard.discardLocalAdmission(expectedRecovery: recovery.id))
        await f.keyboard.waitUntilSuspended(); try await drain(f)
        let output = await f.transport.bytes(); XCTAssertEqual(output, Data("12345678".utf8)); await finish(f)
    }
    func testRecoveryDiagnosticsDoNotExposeContents() async throws {
        let f = try await fixture(), recovery = try await block(f, head: "secret", tail: "password")
        XCTAssertEqual(String(reflecting: recovery), "TerminalKeyboardRecovery([redacted])"); await finish(f)
    }
    func testRetrySynchronouslyPublishesDecisionConsumption() async throws {
        let f = try await fixture(), recovery = try await block(f)
        var phases: [TerminalKeyboardPhase] = []
        let token = try f.keyboard.observeState { phases.append(f.keyboard.phase) }
        try await drain(f); try f.keyboard.retryLocalAdmission(expectedRecovery: recovery.id)
        XCTAssertEqual(phases.first, .active); XCTAssertNil(f.keyboard.recovery)
        await f.keyboard.waitUntilSettled(); f.keyboard.removeStateObserver(token); await finish(f)
    }
    func testReentrantDiscardDoesNotLeakOrCommitPendingText() async throws {
        let f = try await fixture()
        let token = try f.keyboard.observeState {
            if let recovery = f.keyboard.recovery { try? f.keyboard.discardLocalAdmission(expectedRecovery: recovery.id) }
        }
        try f.keyboard.insertText("12345678"); await f.keyboard.waitUntilSettled()
        try f.keyboard.insertText("unsent"); await f.keyboard.waitUntilSettled()
        await f.keyboard.waitUntilSuspended(); try await drain(f)
        let output = await f.transport.bytes(); XCTAssertEqual(output, Data("12345678".utf8))
        XCTAssertEqual(f.keyboard.phase, .retired); f.keyboard.removeStateObserver(token); await finish(f)
    }
    func testThousandFailedDecisionsStayBoundedAndDoNotReplay() async throws {
        let f = try await fixture(), first = try await block(f)
        var previous = first
        for _ in 0..<1000 {
            try f.keyboard.retryLocalAdmission(expectedRecovery: previous.id); await f.keyboard.waitUntilSettled()
            let current = try XCTUnwrap(f.keyboard.recovery)
            XCTAssertNotEqual(previous.id, current.id); XCTAssertEqual(current.accounting.pendingEvents, 2)
            XCTAssertEqual(current.accounting.pendingUTF8Bytes, 4); previous = current
        }
        XCTAssertThrowsError(try f.keyboard.retryLocalAdmission(expectedRecovery: first.id))
        try await drain(f); try f.keyboard.retryLocalAdmission(expectedRecovery: previous.id)
        await f.keyboard.waitUntilSettled(); try await drain(f)
        let output = await f.transport.bytes(); XCTAssertEqual(output, Data("12345678AABB".utf8)); await finish(f)
    }
}
