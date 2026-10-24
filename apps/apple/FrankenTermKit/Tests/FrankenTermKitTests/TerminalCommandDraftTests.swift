import Foundation
import XCTest
@testable import FrankenTermKit

private actor CommandDraftTestTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    private var output = Data()
    init() throws {
        identity = try .init(workspaceID: "workspace", machineID: "build", routeID: "ssh", routeRevision: 1,
                             transportID: UUID(), channelID: UUID())
    }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        let count = min(3, bytes.count)
        output.append(bytes.prefix(count))
        return .accepted(count)
    }
    func close() async {}
    func bytes() -> Data { output }
}

/// A synthetic mode provider, not a second terminal encoder. Real keyboard and
/// coordinator owners below perform admission, delimiting and partial delivery.
private actor CommandDraftTestEncoder: TerminalKeyboardEncoding {
    nonisolated let supportsPaste = true
    private var bracketed = false
    private var prepared: [String] = []
    func setBracketed(_ value: Bool) { bracketed = value }
    func preparations() -> [String] { prepared }
    func encode(_ intent: TerminalKeyboardIntent, source: TerminalSource, maximumBytes: Int) async throws -> Data {
        if case .releaseAll = intent { return Data() }
        throw TerminalKeyboardError.encoding
    }
    func preparePaste(_ text: String, source: TerminalSource, maximumBytes: Int) async throws -> TerminalPreparedPaste {
        guard text.utf8.count <= maximumBytes else { throw TerminalKeyboardError.capacity }
        prepared.append(text)
        return TerminalPreparedPaste(text: text, bracketed: bracketed)
    }
}

@MainActor
final class TerminalCommandDraftTests: XCTestCase {
    private func source() async throws -> TerminalSource {
        let coordinator = try TerminalInputCoordinator()
        return try await coordinator.attach(CommandDraftTestTransport())
    }

    func testOpeningRefusesQueuedBlockedAndMarkedInputWithoutDiscardingIt() async throws {
        let coordinator = try TerminalInputCoordinator(), transport = try CommandDraftTestTransport()
        let source = try await coordinator.attach(transport), encoder = CommandDraftTestEncoder()
        func keyboard() async throws -> TerminalKeyboardSession {
            let stream = try await coordinator.makeStream(sceneID: UUID())
            let focus = try await coordinator.changeFocus(stream, to: source)
            return try TerminalKeyboardSession(focus: focus, coordinator: coordinator, encoder: encoder)
        }
        let pending = try await keyboard()
        XCTAssertNoThrow(try TerminalCommandDraft<Int>.validateOpening(keyboard: pending))
        XCTAssertThrowsError(try TerminalCommandDraft<Int>.validateOpening(keyboard: pending, liveViewport: false)) { error in
            XCTAssertEqual(error as? TerminalCommandDraftError, .historyViewport)
        }
        XCTAssertEqual(pending.phase, .active)
        XCTAssertEqual(pending.accounting.pendingEvents, 0)
        try pending.insertText("not yet admitted")
        XCTAssertThrowsError(try TerminalCommandDraft<Int>.validateOpening(keyboard: pending)) { error in
            XCTAssertEqual(error as? TerminalCommandDraftError, .pendingInput)
        }
        XCTAssertEqual(pending.accounting.pendingEvents, 1)
        XCTAssertEqual(pending.accounting.discardedUnadmittedEvents, 0)
        // This test encoder refuses text, leaving a real blocked head which
        // opening the composer must not discard or silently retry.
        await pending.waitUntilSettled()
        XCTAssertEqual(pending.phase, .blocked)
        XCTAssertThrowsError(try TerminalCommandDraft<Int>.validateOpening(keyboard: pending))
        XCTAssertEqual(pending.accounting.pendingEvents, 1)
        XCTAssertEqual(pending.accounting.discardedUnadmittedEvents, 0)
        pending.suspend(); await pending.waitUntilSuspended()

        let marked = try await keyboard()
        try marked.setMarkedText("候補", selectedRange: NSRange(location: 2, length: 0))
        XCTAssertThrowsError(try TerminalCommandDraft<Int>.validateOpening(keyboard: marked)) { error in
            XCTAssertEqual(error as? TerminalCommandDraftError, .compositionPending)
        }
        XCTAssertEqual(marked.document.text, "候補")
        XCTAssertTrue(marked.document.isMarked)
        XCTAssertEqual(marked.accounting.discardedUnadmittedEvents, 0)
        marked.suspend(); await marked.waitUntilSuspended()
        let output = await transport.bytes()
        XCTAssertTrue(output.isEmpty)
    }

    func testLocalDraftPreservesMultilineUnicodeAndReturnExactly() async throws {
        let draft = TerminalCommandDraft(destination: "fixed-terminal")
        let text = "printf '%s\\n' '👩🏽‍💻 e\u{301} 界'\n\tprintf done\r\n"
        try draft.update(text)
        XCTAssertEqual(draft.text, text)
        XCTAssertTrue(draft.containsLineBreaks)
        XCTAssertEqual(draft.utf8Bytes, text.utf8.count)
        XCTAssertFalse(String(reflecting: draft).contains("printf"))
        try draft.prepareReview(destination: "fixed-terminal")
        var offers: [String] = []
        try draft.offerReview(destination: "fixed-terminal") { offers.append($0) }
        XCTAssertEqual(offers, [text], "offering neither trims nor appends Return")
        XCTAssertEqual(draft.phase, .retired)
        XCTAssertEqual(draft.text, "")
    }

    func testEmptyDraftCanBeEditedButCannotBeginReview() async throws {
        let draft = TerminalCommandDraft(destination: 7)
        try draft.update("")
        XCTAssertThrowsError(try draft.prepareReview(destination: 7)) { error in
            XCTAssertEqual(error as? TerminalCommandDraftError, .empty)
        }
        XCTAssertEqual(draft.phase, .editing)
        try draft.update("pwd")
        try draft.prepareReview(destination: 7)
        XCTAssertEqual(draft.text, "pwd")
    }

    func testPreparingFreezesEditsAndNeedsAnExplicitReturnBeforeRetry() async throws {
        let draft = TerminalCommandDraft(destination: "original")
        try draft.update("first")
        try draft.prepareReview(destination: "original")
        XCTAssertThrowsError(try draft.update("late edit"))
        XCTAssertThrowsError(try draft.proposedText(replacing: NSRange(location: 0, length: 0), with: "late"))
        XCTAssertThrowsError(try draft.prepareReview(destination: "original"))
        XCTAssertEqual(draft.text, "first")
        try draft.resumeEditing(destination: "original")
        try draft.update("revised\ncommand")
        try draft.prepareReview(destination: "original")
        var offered = ""
        try draft.offerReview(destination: "original") { offered = $0 }
        XCTAssertEqual(offered, "revised\ncommand")
    }

    func testRetirementClearsTextAndRejectsDelayedReviewOrEditing() async throws {
        for preparing in [false, true] {
            let draft = TerminalCommandDraft(destination: "original")
            try draft.update("private command")
            if preparing { try draft.prepareReview(destination: "original") }
            let delayedFocus = Task { @MainActor in
                await Task.yield()
                XCTAssertThrowsError(try draft.offerReview(destination: "original") { _ in
                    XCTFail("retired text reached the paste review")
                })
                XCTAssertThrowsError(try draft.resumeEditing(destination: "original"))
            }
            draft.retire()
            try await delayedFocus.value
            XCTAssertEqual(draft.text, "")
            XCTAssertThrowsError(try draft.update("late native callback"))
            XCTAssertThrowsError(try draft.prepareReview(destination: "original"))
        }
    }

    func testFailedAndReentrantOfferCannotReplayConsumedText() async throws {
        enum Refusal: Error { case expected }
        let draft = TerminalCommandDraft(destination: 1)
        try draft.update("only once")
        try draft.prepareReview(destination: 1)
        XCTAssertThrowsError(try draft.offerReview(destination: 1) { value in
            XCTAssertEqual(value, "only once")
            XCTAssertEqual(draft.text, "")
            XCTAssertEqual(draft.phase, .retired)
            XCTAssertThrowsError(try draft.offerReview(destination: 1) { _ in XCTFail("duplicate offer") })
            throw Refusal.expected
        })
        XCTAssertThrowsError(try draft.offerReview(destination: 1) { _ in XCTFail("failed offer replayed") })
        XCTAssertThrowsError(try draft.resumeEditing(destination: 1))
        XCTAssertEqual(draft.text, "")
    }

    func testWholeDocumentAndReplacementBoundsCountUTF8BeforeMutation() async throws {
        let draft = TerminalCommandDraft(destination: 1)
        let limit = TerminalCommandDraft<Int>.maximumUTF8Bytes
        let exact = String(repeating: "界", count: limit / 3) + "ab"
        XCTAssertEqual(exact.utf8.count, limit)
        try draft.update(exact)
        for extra in ["a", "界", "🦀"] {
            XCTAssertThrowsError(try draft.update(exact + extra))
            XCTAssertThrowsError(try draft.proposedText(replacing: NSRange(location: exact.utf16.count, length: 0), with: extra))
            XCTAssertEqual(draft.text, exact)
        }
        let replacement = String(repeating: "x", count: limit)
        let proposed = try draft.proposedText(replacing: NSRange(location: 0, length: exact.utf16.count), with: replacement)
        XCTAssertEqual(proposed, replacement)
        XCTAssertEqual(draft.text, exact, "validation does not pre-commit a UIKit edit")
        try draft.update(proposed)
        XCTAssertEqual(draft.text, replacement)
    }

    func testUnsafeControlCharactersCannotEnterLocalDraftOrReplacement() async throws {
        let draft = TerminalCommandDraft(destination: 1)
        try draft.update("kept")
        let forbidden = Array(0...31).filter { ![9, 10, 13].contains($0) } + Array(127...159)
        for scalar in forbidden {
            let value = "prefix" + String(UnicodeScalar(scalar)!) + "suffix"
            XCTAssertThrowsError(try draft.update(value))
            XCTAssertThrowsError(try draft.proposedText(replacing: NSRange(location: 0, length: 4), with: value))
            XCTAssertEqual(draft.text, "kept")
        }
        try draft.update("tab\tLF\nCR\r")
        XCTAssertTrue(draft.containsLineBreaks)
    }

    func testNativeUTF16EditsRejectOverflowAndHalfSurrogatesButAllowIMEUpdates() async throws {
        let draft = TerminalCommandDraft(destination: 1)
        try draft.update("A🦀e\u{301}Z")
        for range in [NSRange(location: -1, length: 0), NSRange(location: 0, length: -1),
                      NSRange(location: Int.max, length: 0), NSRange(location: 1, length: Int.max),
                      NSRange(location: 2, length: 0), NSRange(location: 1, length: 1),
                      NSRange(location: 2, length: 1)] {
            XCTAssertThrowsError(try draft.proposedText(replacing: range, with: "x"))
            XCTAssertEqual(draft.text, "A🦀e\u{301}Z")
        }
        XCTAssertEqual(try draft.proposedText(replacing: NSRange(location: 1, length: 2), with: "👩🏽‍💻"),
                       "A👩🏽‍💻e\u{301}Z")
        XCTAssertEqual(try draft.proposedText(replacing: NSRange(location: 4, length: 1), with: "\u{308}"),
                       "A🦀e\u{308}Z", "native IME owns scalar edits inside its marked grapheme")
        try draft.update("候補")
        XCTAssertEqual(try draft.proposedText(replacing: NSRange(location: 0, length: 2), with: "確定"), "確定")
    }

    func testChangedDestinationRetiresEvenBeforeKeyboardRestoration() async throws {
        let draft = TerminalCommandDraft(destination: "original")
        try draft.update("private")
        XCTAssertThrowsError(try draft.prepareReview(destination: "other"))
        XCTAssertEqual(draft.text, "")
        XCTAssertEqual(draft.phase, .retired)
        XCTAssertThrowsError(try draft.prepareReview(destination: "original"))
    }

    func testSourceOwnerAndReconnectedChannelCannotInheritDraft() async throws {
        let transport = try CommandDraftTestTransport()
        let originalOwner = try TerminalInputCoordinator(), otherOwner = try TerminalInputCoordinator()
        let original = try await originalOwner.attach(transport)
        let sameIdentityOtherOwner = try await otherOwner.attach(transport)
        let reconnected = try await source()
        XCTAssertEqual(original.identity, sameIdentityOtherOwner.identity)
        for changed in [sameIdentityOtherOwner, reconnected] {
            let destination = TerminalCommandDraftDestination(source: original)
            let draft = TerminalCommandDraft(destination: destination)
            try draft.update("private")
            try draft.prepareReview(destination: destination)
            XCTAssertThrowsError(try draft.offerReview(destination: .init(source: changed)) { _ in
                XCTFail("draft moved to another source")
            })
            XCTAssertEqual(draft.text, "")
            XCTAssertEqual(draft.phase, .retired)
        }
    }

    func testMuxVisitAndRegistrationCannotBeReusedByNumericPaneID() async throws {
        let source = try await source(), routing = TerminalMuxInputRouting()
        try routing.replaceInventory([4, 9])
        let original = try routing.select(pane: 4)
        let destination = TerminalCommandDraftDestination(source: source, mux: original)
        _ = try routing.select(pane: 9)
        let revisited = try routing.select(pane: 4)
        routing.remove(pane: 4)
        try routing.replaceInventory([4, 9])
        let reused = try routing.select(pane: 4)
        let otherRouting = TerminalMuxInputRouting()
        try otherRouting.replaceInventory([4])
        let otherConnection = try otherRouting.select(pane: 4)
        for changed in [revisited, reused, otherConnection] {
            XCTAssertEqual(changed.paneID, original.paneID)
            let draft = TerminalCommandDraft(destination: destination)
            try draft.update("private")
            try draft.prepareReview(destination: destination)
            XCTAssertThrowsError(try draft.offerReview(destination: .init(source: source, mux: changed)) { _ in
                XCTFail("a repeated pane number revived the draft")
            })
            XCTAssertEqual(draft.phase, .retired)
            XCTAssertEqual(draft.text, "")
        }
    }

    func testRealPasteAdmissionRequiresFreshReviewAndNeverAppendsReturn() async throws {
        for bracketed in [false, true] {
            let coordinator = try TerminalInputCoordinator(), transport = try CommandDraftTestTransport()
            let source = try await coordinator.attach(transport)
            let encoder = CommandDraftTestEncoder()
            let oldStream = try await coordinator.makeStream(sceneID: UUID())
            let oldFocus = try await coordinator.changeFocus(oldStream, to: source)
            let originalKeyboard = try TerminalKeyboardSession(focus: oldFocus, coordinator: coordinator, encoder: encoder)
            let destination = TerminalCommandDraftDestination(source: source)
            let draft = TerminalCommandDraft(destination: destination)
            // Opening the native editor ends direct-terminal input first.
            originalKeyboard.suspend()
            await originalKeyboard.waitUntilSuspended()
            let text = "printf '%s' '界 e\u{301}'\n\tprintf done"
            try draft.update(text)
            try draft.prepareReview(destination: destination)
            let before = try await coordinator.drainOne(source)
            XCTAssertEqual(before, .idle)

            let freshStream = try await coordinator.makeStream(sceneID: oldFocus.sceneID)
            let freshFocus = try await coordinator.changeFocus(freshStream, to: source)
            let keyboard = try TerminalKeyboardSession(focus: freshFocus, coordinator: coordinator, encoder: encoder)
            XCTAssertNotEqual(keyboard.id, originalKeyboard.id)
            let pasteDecision = TerminalClipboardReview<TerminalFocus>()
            var offered: TerminalPasteReview?
            try draft.offerReview(destination: destination) { value in
                let review = try TerminalPasteReview(text: value, keyboardID: keyboard.id, focus: freshFocus)
                try pasteDecision.offer(id: review.id, destination: review.focus, text: review.text)
                offered = review
            }
            let noInput = try await coordinator.drainOne(source)
            let noPreparation = await encoder.preparations()
            XCTAssertEqual(noInput, .idle)
            XCTAssertEqual(noPreparation, [], "Review is not paste admission")
            let review = try XCTUnwrap(offered)
            XCTAssertThrowsError(try originalKeyboard.paste(review))
            XCTAssertThrowsError(try pasteDecision.confirm(id: review.id, destination: oldFocus) { _ in
                XCTFail("retired focus approved the draft")
            })
            // Mode is sampled only by the existing keyboard/encoder path at
            // explicit confirmation, after local drafting and review.
            await encoder.setBracketed(bracketed)
            try pasteDecision.confirm(id: review.id, destination: freshFocus) { _ in try keyboard.paste(review) }
            XCTAssertThrowsError(try pasteDecision.confirm(id: review.id, destination: freshFocus) { _ in
                XCTFail("duplicate confirmation")
            })
            await keyboard.waitUntilSettled()
            var settled = false
            for _ in 0..<128 {
                if try await coordinator.drainOne(source) == .idle { settled = true; break }
            }
            XCTAssertTrue(settled)
            let expected = bracketed ? "\u{1B}[200~" + text + "\u{1B}[201~" : text
            let output = await transport.bytes(), preparations = await encoder.preparations()
            XCTAssertEqual(output, Data(expected.utf8))
            XCTAssertEqual(preparations, [text])
            keyboard.suspend(); await keyboard.waitUntilSuspended()
        }
    }
}
