import Foundation
import XCTest
@testable import FrankenTermKit

private actor KeyboardTestTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    var output = Data()
    init(machine: String) throws {
        identity = try .init(workspaceID: "work", machineID: machine, routeID: "route", routeRevision: 1,
                             transportID: UUID(), channelID: UUID())
    }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        let count = min(3, bytes.count); output += bytes.prefix(count); return .accepted(count)
    }
    func close() async {}
    func bytes() -> Data { output }
}

/// Deliberately synthetic key encoder. Production must use the canonical Rust
/// mode/keyboard implementation; these tokens expose duplication and ordering.
private actor KeyboardTestEncoder: TerminalKeyboardEncoding {
    var shouldBlock = false, failNext = false, entered = false
    var continuation: CheckedContinuation<Void, Never>?
    var calls = 0
    func block() { shouldBlock = true }
    func fail() { failNext = true }
    func isEntered() -> Bool { entered }
    func release() { shouldBlock = false; continuation?.resume(); continuation = nil }
    func count() -> Int { calls }
    func encode(_ intent: TerminalKeyboardIntent, source: TerminalSource, maximumBytes: Int) async throws -> Data {
        calls += 1; entered = true
        if shouldBlock { await withCheckedContinuation { continuation = $0 } }
        try Task.checkCancellation()
        if failNext { failNext = false; throw TerminalKeyboardError.encoding }
        let bytes: Data
        switch intent {
        case .text(let text): bytes = Data(text.utf8)
        case .key(let key, let phase):
            let prefix: String
            switch phase { case .down: prefix = "D"; case .up: prefix = "U"; case .repeated: prefix = "R"; case .press: prefix = "P" }
            bytes = Data("<\(prefix):\(key.usage)>".utf8)
        case .releaseAll(let keys): bytes = Data(keys.map { "<U:\($0.usage)>" }.joined().utf8)
        }
        guard bytes.count <= maximumBytes else { throw TerminalKeyboardError.capacity }
        return bytes
    }
}

final class TerminalMarkedTextTests: XCTestCase {
    func testUTF16SelectionRejectsHalfSurrogateAndOverflowWithoutMutation() throws {
        var doc = try TerminalMarkedText()
        try doc.mark("A🦀B", selectedRange: NSRange(location: 3, length: 0))
        for range in [NSRange(location: 2, length: 0), NSRange(location: 1, length: 1),
                      NSRange(location: -1, length: 0), NSRange(location: 0, length: -1),
                      NSRange(location: Int.max, length: 1), NSRange(location: 1, length: Int.max)] {
            XCTAssertThrowsError(try doc.select(range))
            XCTAssertEqual(doc.selection, NSRange(location: 3, length: 0))
        }
        XCTAssertEqual(try doc.substring(NSRange(location: 1, length: 2)), "🦀")
        try doc.replace(NSRange(location: 1, length: 2), with: "界")
        XCTAssertEqual(doc.text, "A界B")
        XCTAssertEqual(doc.selection.location, 2)
    }
    func testBackspaceRemovesWholeCombiningZWJAndFlagClusters() throws {
        for cluster in ["e\u{301}", "👩‍👩‍👧‍👧", "🇺🇸", "👍🏽", "क्‍ष"] {
            var doc = try TerminalMarkedText()
            try doc.mark("prefix" + cluster, selectedRange: NSRange(location: ("prefix" + cluster).utf16.count, length: 0))
            try doc.deleteBackward()
            XCTAssertEqual(doc.text, "prefix", cluster)
            XCTAssertEqual(doc.selection.location, 6)
        }
    }
    func testEmptyMarkedBackspaceDoesNotBecomeRemoteBackspace() throws {
        var doc = try TerminalMarkedText()
        try doc.mark("", selectedRange: NSRange(location: 0, length: 0))
        try doc.deleteBackward()
        XCTAssertTrue(doc.isMarked)
        XCTAssertEqual(doc.text, "")
        doc.clear(); XCTAssertFalse(doc.isMarked)
    }
    func testCompositionReplacementIsAtomicAndByteBounded() throws {
        var doc = try TerminalMarkedText(maximumUTF8Bytes: 8)
        try doc.mark("🦀🦀", selectedRange: NSRange(location: 4, length: 0))
        XCTAssertThrowsError(try doc.mark("🦀🦀x", selectedRange: NSRange(location: 0, length: 0)))
        XCTAssertThrowsError(try doc.replace(NSRange(location: 0, length: 0), with: "x"))
        XCTAssertEqual(doc.text, "🦀🦀")
        try doc.replace(NSRange(location: 0, length: 2), with: "ab")
        XCTAssertEqual(doc.text, "ab🦀")
        XCTAssertEqual(doc.selection.location, 2)
        XCTAssertFalse(String(reflecting: doc).contains("🦀"))
    }
    func testScalarBoundariesInsideCombiningTextAreValidForIME() throws {
        var doc = try TerminalMarkedText()
        try doc.mark("e\u{301}", selectedRange: NSRange(location: 1, length: 0))
        XCTAssertEqual(try doc.substring(NSRange(location: 1, length: 1)), "\u{301}")
        try doc.deleteBackward()
        XCTAssertEqual(doc.text, "")
    }
    func testSelectedRangeReplacementAndDeletionPreserveSurroundingText() throws {
        var doc = try TerminalMarkedText()
        try doc.mark("abc界def", selectedRange: NSRange(location: 2, length: 3))
        try doc.deleteBackward(); XCTAssertEqual(doc.text, "abef")
        try doc.replace(NSRange(location: 1, length: 2), with: "語")
        XCTAssertEqual(doc.text, "a語f")
    }
}

@MainActor
final class TerminalKeyboardSessionTests: XCTestCase {
    private func setup(limits: TerminalInputLimits = TerminalInputLimits(), maximumEvents: Int = 128,
                       maximumBytes: Int = 32_768) async throws
        -> (TerminalInputCoordinator, KeyboardTestTransport, TerminalSource, KeyboardTestEncoder, TerminalKeyboardSession) {
        let coordinator = try TerminalInputCoordinator(limits: limits)
        let transport = try KeyboardTestTransport(machine: "build")
        let source = try await coordinator.attach(transport)
        let empty = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(empty, to: source)
        let encoder = KeyboardTestEncoder()
        return (coordinator, transport, source, encoder,
                try TerminalKeyboardSession(focus: focus, coordinator: coordinator, encoder: encoder,
                                             maximumEvents: maximumEvents, maximumBytes: maximumBytes))
    }
    private func drain(_ coordinator: TerminalInputCoordinator, _ source: TerminalSource) async throws {
        for _ in 0..<10_000 {
            if try await coordinator.drainOne(source) == .idle { return }
        }
        XCTFail("bounded synthetic transport did not drain")
    }
    private func entered(_ encoder: KeyboardTestEncoder) async {
        for _ in 0..<10_000 {
            if await encoder.isEntered() { return }; await Task.yield()
        }
        XCTFail("controlled encoder did not enter")
    }
    func testRealCoordinatorReceivesCommittedTextExactlyOnceWithoutReturn() async throws {
        let (coordinator, transport, source, _, session) = try await setup()
        try session.setMarkedText("候", selectedRange: NSRange(location: 1, length: 0))
        try session.setMarkedText("候補", selectedRange: NSRange(location: 2, length: 0))
        await session.waitUntilSettled()
        try await drain(coordinator, source)
        let before = await transport.bytes(); XCTAssertTrue(before.isEmpty)
        try session.unmarkText(); try session.unmarkText()
        await session.waitUntilSettled(); try await drain(coordinator, source)
        let after = await transport.bytes(); XCTAssertEqual(after, Data("候補".utf8))
        XCTAssertFalse(session.document.isMarked)
        session.suspend(); await session.waitUntilSuspended()
    }
    func testCandidateInsertReplacesMarkedTextAndRepeatedActualTextIsNotDeduplicated() async throws {
        let (coordinator, transport, source, _, session) = try await setup()
        try session.setMarkedText("ni", selectedRange: NSRange(location: 2, length: 0))
        try session.insertText("你")
        try session.insertText("你")
        await session.waitUntilSettled(); try await drain(coordinator, source)
        let bytes = await transport.bytes(); XCTAssertEqual(bytes, Data("你你".utf8))
        session.suspend(); await session.waitUntilSuspended()
    }
    func testMarkedBackspaceNeverLeaksToTerminal() async throws {
        let (coordinator, transport, source, _, session) = try await setup()
        try session.setMarkedText("🦀", selectedRange: NSRange(location: 2, length: 0))
        try session.deleteBackward(); try session.deleteBackward(); try session.unmarkText()
        await session.waitUntilSettled(); try await drain(coordinator, source)
        let before = await transport.bytes(); XCTAssertEqual(before, Data())
        try session.deleteBackward()
        await session.waitUntilSettled(); try await drain(coordinator, source)
        let after = await transport.bytes(); XCTAssertEqual(after, Data("<P:42>".utf8))
        session.suspend(); await session.waitUntilSuspended()
    }
    func testBlockedEncoderCannotRetargetCompositionAcrossHandoff() async throws {
        let (coordinator, transport, source, encoder, session) = try await setup()
        let second = try KeyboardTestTransport(machine: "gpu"), destination = try await coordinator.attach(second)
        await encoder.block()
        try session.setMarkedText("original", selectedRange: NSRange(location: 8, length: 0))
        let switcher = Task { try await session.handoff(to: destination) }
        await entered(encoder)
        XCTAssertFalse(session.acceptsInput)
        XCTAssertThrowsError(try session.insertText("late"))
        await encoder.release()
        let next = try await switcher.value
        XCTAssertEqual(next.source, destination)
        _ = try await coordinator.submit(next, encodedBytes: Data("new".utf8))
        try await drain(coordinator, source); try await drain(coordinator, destination)
        let old = await transport.bytes(), new = await second.bytes()
        XCTAssertEqual(old, Data("original".utf8)); XCTAssertEqual(new, Data("new".utf8))
        XCTAssertThrowsError(try session.insertText("later"))
        session.suspend() // Disposal of old responder cannot revoke its successor.
        _ = try await coordinator.submit(next, encodedBytes: Data("!".utf8))
        _ = try await coordinator.suspend(next)
    }
    func testHandoffCleanupUsesReservedBudgetBehindOriginalKeys() async throws {
        var limits = TerminalInputLimits(); limits.bytesPerConnection = 8; limits.totalBytes = 8
        let (coordinator, transport, source, _, session) = try await setup(limits: limits)
        try session.key(TerminalHardwareKey(usage: 4), phase: .down)
        await session.waitUntilSettled()
        let next = try await session.handoff(to: nil)
        XCTAssertNil(next.source)
        try await drain(coordinator, source)
        let bytes = await transport.bytes(); XCTAssertEqual(bytes, Data("<D:4><U:4>".utf8))
        _ = try await coordinator.suspend(next)
    }
    func testNativeQueueIncludesInFlightPayloadAndCapsBeforeCopy() async throws {
        let (coordinator, transport, source, encoder, session) = try await setup(maximumEvents: 2, maximumBytes: 16)
        await encoder.block(); try session.insertText("12345678"); await entered(encoder)
        try session.insertText("abcdefgh")
        XCTAssertEqual(session.accounting.pendingUTF8Bytes, 16)
        XCTAssertEqual(session.accounting.pendingEvents, 2)
        XCTAssertThrowsError(try session.insertText("x"))
        XCTAssertThrowsError(try session.setMarkedText("x", selectedRange: NSRange(location: 1, length: 0)))
        XCTAssertFalse(session.document.isMarked)
        await encoder.release(); await session.waitUntilSettled(); try await drain(coordinator, source)
        let bytes = await transport.bytes(); XCTAssertEqual(bytes, Data("12345678abcdefgh".utf8))
        session.suspend(); await session.waitUntilSuspended()
    }
    func testLocalFailureStopsSuffixAndExplicitRetryDoesNotDuplicateEarlierText() async throws {
        let (coordinator, transport, source, encoder, session) = try await setup()
        try session.insertText("first"); await session.waitUntilSettled()
        await encoder.fail(); try session.insertText("second"); try session.insertText("third")
        await session.waitUntilSettled()
        XCTAssertEqual(session.phase, .blocked); XCTAssertEqual(session.accounting.pendingEvents, 2)
        try await drain(coordinator, source)
        let before = await transport.bytes(); XCTAssertEqual(before, Data("first".utf8))
        try session.retryLocalAdmission(); await session.waitUntilSettled(); try await drain(coordinator, source)
        let after = await transport.bytes(); XCTAssertEqual(after, Data("firstsecondthird".utf8))
        session.suspend(); await session.waitUntilSuspended()
    }
    func testSuspensionDropsUnadmittedEncoderResultAndRetiresFocus() async throws {
        let (coordinator, transport, source, encoder, session) = try await setup()
        await encoder.block(); try session.insertText("secret"); await entered(encoder)
        try session.insertText("later"); session.suspend()
        XCTAssertEqual(session.phase, .suspending)
        XCTAssertThrowsError(try session.insertText("very late"))
        await encoder.release(); await session.waitUntilSuspended(); try await drain(coordinator, source)
        let bytes = await transport.bytes(); XCTAssertTrue(bytes.isEmpty)
        XCTAssertEqual(session.phase, .retired); XCTAssertEqual(session.accounting.pendingUTF8Bytes, 0)
        XCTAssertEqual(session.accounting.discardedUnadmittedEvents, 2)
        do { _ = try await coordinator.submit(session.originalFocus, encodedBytes: Data([1])); XCTFail("retired focus accepted") }
        catch let error as TerminalInputError { XCTAssertEqual(error, .stale) }
    }
    func testSuspensionDuringHandoffOwnsAndRetiresTheActualFocus() async throws {
        let (coordinator, _, _, encoder, session) = try await setup()
        let other = try KeyboardTestTransport(machine: "other"), source = try await coordinator.attach(other)
        await encoder.block()
        try session.insertText("pending")
        let switcher = Task { try await session.handoff(to: source) }
        await entered(encoder); session.suspend(); await encoder.release()
        do { _ = try await switcher.value; XCTFail("suspended handoff succeeded") } catch { }
        await session.waitUntilSuspended()
        let accounting = await coordinator.accounting(); XCTAssertEqual(accounting.streams, 0)
        XCTAssertEqual(session.phase, .retired)
    }
    func testRealCoordinatorCapacityFailureRetainsCompositionForRetry() async throws {
        var limits = TerminalInputLimits(); limits.bytesPerConnection = 16; limits.totalBytes = 16
        let (coordinator, transport, source, _, session) = try await setup(limits: limits)
        try session.insertText("1234567890123456"); await session.waitUntilSettled()
        try session.setMarkedText("候補", selectedRange: NSRange(location: 2, length: 0))
        try session.unmarkText(); await session.waitUntilSettled()
        XCTAssertEqual(session.phase, .blocked)
        try await drain(coordinator, source)
        try session.retryLocalAdmission(); await session.waitUntilSettled(); try await drain(coordinator, source)
        let bytes = await transport.bytes(); XCTAssertEqual(bytes, Data("1234567890123456候補".utf8))
        session.suspend(); await session.waitUntilSuspended()
    }
    func testCancelledNavigationDoesNotMoveFocusOrReplayInput() async throws {
        let (coordinator, transport, source, encoder, session) = try await setup()
        let other = try KeyboardTestTransport(machine: "other"), destination = try await coordinator.attach(other)
        await encoder.block(); try session.insertText("original")
        let switcher = Task { try await session.handoff(to: destination) }
        await entered(encoder); switcher.cancel(); await encoder.release()
        do { _ = try await switcher.value; XCTFail("cancelled navigation succeeded") } catch { }
        XCTAssertEqual(session.phase, .active)
        try session.insertText("stayed"); await session.waitUntilSettled()
        try await drain(coordinator, source); try await drain(coordinator, destination)
        let old = await transport.bytes(), new = await other.bytes()
        XCTAssertEqual(old, Data("originalstayed".utf8)); XCTAssertTrue(new.isEmpty)
        session.suspend(); await session.waitUntilSuspended()
    }
    func testOrphanedKeyReleasesAndRepeatsAreIgnoredInFreshContext() async throws {
        let (coordinator, transport, source, encoder, session) = try await setup()
        let key = try TerminalHardwareKey(usage: 4, characters: "a")
        try session.key(key, phase: .up); try session.key(key, phase: .repeated)
        await session.waitUntilSettled()
        let calls = await encoder.count(); XCTAssertEqual(calls, 0)
        try session.key(key, phase: .down); try session.key(key, phase: .repeated); try session.key(key, phase: .up)
        await session.waitUntilSettled(); try await drain(coordinator, source)
        let bytes = await transport.bytes(); XCTAssertEqual(bytes, Data("<D:4><R:4><U:4>".utf8))
        XCTAssertEqual(session.accounting.heldKeys, 0)
        session.suspend(); await session.waitUntilSuspended()
    }
    func testSourceLossRejectsCompositionAndHandoffWithoutNewHostInput() async throws {
        let (coordinator, transport, source, _, session) = try await setup()
        let other = try KeyboardTestTransport(machine: "other"), destination = try await coordinator.attach(other)
        try session.setMarkedText("unsent", selectedRange: NSRange(location: 6, length: 0))
        await session.waitUntilSettled(); try await coordinator.disconnect(source)
        do { _ = try await session.handoff(to: destination); XCTFail("lost source committed") } catch { }
        XCTAssertEqual(session.phase, .blocked)
        XCTAssertEqual(session.accounting.pendingEvents, 1)
        let old = await transport.bytes(), new = await other.bytes()
        XCTAssertTrue(old.isEmpty); XCTAssertTrue(new.isEmpty)
        session.suspend(); await session.waitUntilSuspended()
    }
    func testHeldKeyCountIsBoundedAndSuspensionReleasesAcceptedKeysOnly() async throws {
        let (coordinator, transport, source, _, session) = try await setup()
        for usage in 4..<37 { try session.key(TerminalHardwareKey(usage: UInt16(usage)), phase: .down) }
        await session.waitUntilSettled()
        XCTAssertEqual(session.phase, .blocked); XCTAssertEqual(session.accounting.heldKeys, 32)
        XCTAssertEqual(session.accounting.pendingEvents, 1)
        session.suspend(); await session.waitUntilSuspended(); try await drain(coordinator, source)
        let expected = (4..<36).map { "<D:\($0)>" }.joined() + (4..<36).map { "<U:\($0)>" }.joined()
        let bytes = await transport.bytes(); XCTAssertEqual(bytes, Data(expected.utf8))
    }
}
