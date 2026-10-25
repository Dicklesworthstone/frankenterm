#if canImport(UIKit)
import UIKit
import XCTest
@testable import FrankenTermKit

private actor UIKitInputTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    private var output = Data()
    init() throws {
        identity = try TerminalConnectionIdentity(workspaceID: "work", machineID: "machine", routeID: "route",
                                                  routeRevision: 1, transportID: UUID(), channelID: UUID())
    }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite { output += bytes; return .accepted(bytes.count) }
    func close() async {}
    func bytes() -> Data { output }
}
private struct UIKitInputEncoder: TerminalKeyboardEncoding {
    var supportsPaste: Bool { true }
    func encode(_ intent: TerminalKeyboardIntent, source: TerminalSource, maximumBytes: Int) async throws -> Data {
        let value: String
        switch intent {
        case .text(let text): value = text
        case .key(let key, _): value = "<key:\(key.usage)>"
        case .releaseAll: value = ""
        }
        guard value.utf8.count <= maximumBytes else { throw TerminalKeyboardError.capacity }
        return Data(value.utf8)
    }
    func preparePaste(_ text: String, source: TerminalSource, maximumBytes: Int) async throws -> TerminalPreparedPaste {
        guard text.utf8.count <= maximumBytes else { throw TerminalKeyboardError.capacity }
        return TerminalPreparedPaste(text: text, bracketed: true)
    }
}
@MainActor
private final class NativeInputDelegate: NSObject, UITextInputDelegate {
    var calls: [String] = []
    func textWillChange(_ textInput: (any UITextInput)?) { calls.append("textWillChange") }
    func textDidChange(_ textInput: (any UITextInput)?) { calls.append("textDidChange") }
    func selectionWillChange(_ textInput: (any UITextInput)?) { calls.append("selectionWillChange") }
    func selectionDidChange(_ textInput: (any UITextInput)?) { calls.append("selectionDidChange") }
    // Required since iOS 18.4; the terminal input view publishes no conversation context.
    func conversationContext(_ context: UIConversationContext?, didChange textInput: (any UITextInput)?) {
        calls.append("conversationContextDidChange")
    }
}

/// These tests require the real UIKit SDK. Linux deliberately excludes them;
/// portable coordination tests are not a substitute for this native boundary.
@MainActor
final class NativeTerminalInputTests: XCTestCase {
    private func setup() async throws
        -> (TerminalInputCoordinator, UIKitInputTransport, TerminalSource, NativeTerminalInputView) {
        let coordinator = try TerminalInputCoordinator()
        let transport = try UIKitInputTransport()
        let source = try await coordinator.attach(transport)
        let stream = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(stream, to: source)
        let session = try TerminalKeyboardSession(focus: focus, coordinator: coordinator, encoder: UIKitInputEncoder())
        let view = try NativeTerminalInputView(session: session)
        view.frame = CGRect(x: 0, y: 0, width: 320, height: 480)
        try view.updateCaret(NativeTerminalCaret(source: source, revision: 1, rect: CGRect(x: 20, y: 20, width: 2, height: 20)))
        return (coordinator, transport, source, view)
    }
    private func drain(_ coordinator: TerminalInputCoordinator, _ source: TerminalSource) async throws {
        for _ in 0..<10_000 {
            if try await coordinator.drainOne(source) == .idle { return }
        }
        XCTFail("native test transport failed to drain within its bound")
    }
    /// A key-row button by its accessibility label.
    private func accessoryButton(_ view: NativeTerminalInputView, _ label: String) -> UIButton? {
        func find(_ root: UIView) -> UIButton? {
            if let button = root as? UIButton, button.accessibilityLabel == label { return button }
            return root.subviews.lazy.compactMap(find).first
        }
        return view.inputAccessoryView.flatMap(find)
    }
    func testRealUITextInputCompositionSendsOnlyFinalCandidateOnce() async throws {
        let (coordinator, transport, source, view) = try await setup()
        view.setMarkedText("ni", selectedRange: NSRange(location: 2, length: 0))
        view.setMarkedText("你", selectedRange: NSRange(location: 1, length: 0))
        await view.session.waitUntilSettled(); try await drain(coordinator, source)
        let before = await transport.bytes(); XCTAssertTrue(before.isEmpty)
        view.unmarkText(); view.unmarkText()
        await view.session.waitUntilSettled(); try await drain(coordinator, source)
        let after = await transport.bytes(); XCTAssertEqual(after, Data("你".utf8))
        XCTAssertNil(view.markedTextRange)
        view.retire(); await view.session.waitUntilSuspended()
    }
    func testSoftwareReturnAndTabUseCanonicalKeysOutsideComposition() async throws {
        let (coordinator, transport, source, view) = try await setup()
        view.insertText("hello"); view.insertText("\n"); view.insertText("\t")
        view.setMarkedText("candidate", selectedRange: NSRange(location: 9, length: 0))
        view.insertText("final")
        await view.session.waitUntilSettled(); try await drain(coordinator, source)
        let output = await transport.bytes(); XCTAssertEqual(output, Data("hello<key:40><key:43>final".utf8))
        view.retire(); await view.session.waitUntilSuspended()
    }
    func testUTF16RangesRejectSurrogateHalvesAndForeignResponderPositions() async throws {
        let (_, _, _, view) = try await setup()
        let (_, _, _, other) = try await setup()
        view.setMarkedText("🦀e\u{301}", selectedRange: NSRange(location: 4, length: 0))
        XCTAssertNil(view.position(from: view.beginningOfDocument, offset: 1))
        XCTAssertNil(view.position(from: view.endOfDocument, offset: Int.max))
        let middle = try XCTUnwrap(view.position(from: view.beginningOfDocument, offset: 2))
        let first = try XCTUnwrap(view.textRange(from: view.beginningOfDocument, to: middle))
        XCTAssertEqual(view.text(in: first), "🦀")
        XCTAssertNil(other.text(in: first))
        XCTAssertNil(other.textRange(from: view.beginningOfDocument, to: other.endOfDocument))
        view.deleteBackward(); XCTAssertEqual(view.session.document.text, "🦀")
        view.deleteBackward(); XCTAssertEqual(view.session.document.text, "")
        XCTAssertNotNil(view.markedTextRange)
        view.retire(); other.retire()
        await view.session.waitUntilSuspended(); await other.session.waitUntilSuspended()
    }
    func testSourceAndRevisionBoundCaretRejectsStaleLayout() async throws {
        let (_, _, source, view) = try await setup()
        let (_, _, foreign, other) = try await setup()
        let rect = CGRect(x: 40, y: 50, width: 2, height: 20)
        try view.updateCaret(NativeTerminalCaret(source: source, revision: 2, rect: rect))
        try view.updateCaret(NativeTerminalCaret(source: source, revision: 2, rect: rect))
        XCTAssertEqual(view.caretRect(for: view.beginningOfDocument), rect)
        XCTAssertThrowsError(try view.updateCaret(NativeTerminalCaret(source: source, revision: 2,
            rect: rect.offsetBy(dx: 10, dy: 0))))
        XCTAssertThrowsError(try view.updateCaret(NativeTerminalCaret(source: source, revision: 1, rect: .zero)))
        XCTAssertThrowsError(try view.updateCaret(NativeTerminalCaret(source: foreign, revision: 3, rect: rect)))
        XCTAssertThrowsError(try view.updateCaret(NativeTerminalCaret(source: source, revision: 3,
            rect: CGRect(x: CGFloat.nan, y: 0, width: 2, height: 20))))
        view.retire(); other.retire()
        await view.session.waitUntilSuspended(); await other.session.waitUntilSuspended()
    }
    func testInvalidatedGeometryPreservesCompositionAndRequiresANewerMatchingProjection() async throws {
        let (coordinator, transport, source, view) = try await setup()
        let (_, _, foreign, other) = try await setup()
        let rendered = CGRect(x: 40, y: 50, width: 2, height: 20)
        try view.updateCaret(NativeTerminalCaret(source: source, revision: 4, rect: rendered))
        let candidate = "🦀e\u{301}"
        view.setMarkedText(candidate, selectedRange: NSRange(location: 4, length: 0))
        await view.session.waitUntilSettled()
        let range = try XCTUnwrap(view.textRange(from: view.beginningOfDocument, to: view.endOfDocument))
        let delegate = NativeInputDelegate(); view.inputDelegate = delegate

        XCTAssertThrowsError(try view.invalidateRenderedCaret(source: foreign))
        XCTAssertTrue(view.isEditable); XCTAssertTrue(view.canBecomeFirstResponder)
        try view.invalidateRenderedCaret(source: source)
        try view.invalidateRenderedCaret(source: source)
        XCTAssertEqual(delegate.calls, ["selectionWillChange", "selectionDidChange"])
        XCTAssertFalse(view.isEditable); XCTAssertFalse(view.canBecomeFirstResponder)
        XCTAssertTrue(view.session.acceptsInput, "temporary projection loss does not retire the focus")
        XCTAssertTrue(view.session.document.isMarked); XCTAssertEqual(view.session.document.text, candidate)
        XCTAssertNotNil(view.markedTextRange)
        XCTAssertEqual(view.caretRect(for: view.beginningOfDocument), .zero)
        XCTAssertEqual(view.firstRect(for: range), .zero)
        XCTAssertTrue(view.selectionRects(for: range).isEmpty)
        XCTAssertNil(view.closestPosition(to: CGPoint(x: 40, y: 50)))

        // These may be delayed UIKit callbacks from the now-invalid projection.
        // None may commit, replace, delete, select, or discard the pending IME candidate.
        view.unmarkText(); view.insertText("late"); view.deleteBackward()
        view.replace(range, withText: "replacement")
        view.setMarkedText("late candidate", selectedRange: NSRange(location: 0, length: 0))
        view.selectedTextRange = range
        await view.session.waitUntilSettled(); try await drain(coordinator, source)
        let before = await transport.bytes(); XCTAssertTrue(before.isEmpty)
        XCTAssertEqual(view.session.document.text, candidate)
        XCTAssertEqual(view.session.document.selection, NSRange(location: 4, length: 0))
        XCTAssertEqual(delegate.calls, ["selectionWillChange", "selectionDidChange"])

        for revision: UInt64 in [1, 4] {
            XCTAssertThrowsError(try view.updateCaret(NativeTerminalCaret(source: source, revision: revision,
                                                                          rect: rendered))) {
                XCTAssertEqual($0 as? TerminalInputError, .stale)
            }
        }
        XCTAssertThrowsError(try view.updateCaret(NativeTerminalCaret(source: foreign, revision: 5, rect: rendered)))
        XCTAssertFalse(view.isEditable); XCTAssertEqual(view.caretRect(for: view.beginningOfDocument), .zero)
        try view.updateCaret(NativeTerminalCaret(source: source, revision: 5, rect: rendered.offsetBy(dx: 10, dy: 0)))
        XCTAssertTrue(view.isEditable); XCTAssertTrue(view.canBecomeFirstResponder)
        XCTAssertEqual(view.session.document.text, candidate)
        XCTAssertGreaterThan(view.caretRect(for: view.beginningOfDocument).height, 0)
        view.unmarkText(); view.unmarkText()
        await view.session.waitUntilSettled(); try await drain(coordinator, source)
        let after = await transport.bytes(); XCTAssertEqual(after, Data(candidate.utf8))
        XCTAssertNil(view.markedTextRange)
        view.retire(); other.retire()
        await view.session.waitUntilSuspended(); await other.session.waitUntilSuspended()
    }
    /// A resize or new text size redraws the live screen: keys typed meanwhile
    /// still reach the shell, and so does what an input method commits. With
    /// history shown nothing is sent, composition included.
    func testTypingWhileTheLiveScreenIsRedrawnIsSent() async throws {
        let (coordinator, transport, source, view) = try await setup()
        var live = true
        view.onInputWithoutProjection = { live }
        try view.updateCaret(NativeTerminalCaret(source: source, revision: 2,
                                                  rect: CGRect(x: 30, y: 40, width: 2, height: 20)))
        try view.invalidateRenderedCaret(source: source)
        XCTAssertFalse(view.isEditable)
        view.insertText("lsx"); view.deleteBackward(); view.insertText("\n")
        // The key row's keys are keys too.
        let left = try XCTUnwrap(accessoryButton(view, "Left arrow"))
        XCTAssertTrue(left.isEnabled, "the key row stays usable while the screen is redrawn")
        left.sendActions(for: .touchUpInside)
        // A conversion the input method commits with its own text, then a
        // candidate committed as it stands; only the committed text is sent.
        view.setMarkedText("か", selectedRange: NSRange(location: 1, length: 0))
        view.setMarkedText("かん", selectedRange: NSRange(location: 2, length: 0))
        XCTAssertTrue(view.session.document.isMarked, "the candidate is kept while the caret is redrawn")
        view.insertText("漢")
        view.setMarkedText("ü", selectedRange: NSRange(location: 1, length: 0))
        view.unmarkText()
        await view.session.waitUntilSettled(); try await drain(coordinator, source)
        let sent = await transport.bytes()
        XCTAssertEqual(String(decoding: sent, as: UTF8.self), "lsx<key:42><key:40><key:80>漢ü")
        XCTAssertFalse(view.session.document.isMarked)

        live = false
        view.insertText("pwd\n"); left.sendActions(for: .touchUpInside)
        view.setMarkedText("ä", selectedRange: NSRange(location: 1, length: 0)); view.unmarkText()
        await view.session.waitUntilSettled(); try await drain(coordinator, source)
        let after = await transport.bytes()
        XCTAssertEqual(after, sent, "with history shown, typing is not sent")
        XCTAssertFalse(view.session.document.isMarked, "nor is a candidate kept")
        view.retire()
        await view.session.waitUntilSuspended()
    }
    func testInvalidatedGeometryRevokesPasteReviewAndBlocksOutgoingInputUntilRestored() async throws {
        let (coordinator, transport, source, view) = try await setup()
        var offered: [TerminalPasteReview] = []
        view.onPasteReview = { offered.append($0) }
        view.requestPasteReview(text: "echo before")
        let oldReview = try XCTUnwrap(offered.first)
        XCTAssertNotNil(view.pendingPaste)
        try view.invalidateRenderedCaret(source: source)
        XCTAssertNil(view.pendingPaste)
        XCTAssertFalse(view.canPerformAction(#selector(NativeTerminalInputView.paste(_:)), withSender: nil))
        XCTAssertThrowsError(try view.confirmPaste(id: oldReview.id))
        view.requestPasteReview(text: "blocked")
        view.paste(nil) // The geometry guard must return before accessing the clipboard.
        view.insertText("blocked"); view.insertText("\n"); view.deleteBackward()
        await view.session.waitUntilSettled(); try await drain(coordinator, source)
        let before = await transport.bytes(); XCTAssertTrue(before.isEmpty)
        XCTAssertEqual(offered.count, 1); XCTAssertNil(view.pendingPaste)

        try view.updateCaret(NativeTerminalCaret(source: source, revision: 2,
                                                  rect: CGRect(x: 30, y: 40, width: 2, height: 20)))
        XCTAssertTrue(view.isEditable)
        XCTAssertThrowsError(try view.confirmPaste(id: oldReview.id), "restoring geometry does not revive a cancelled review")
        view.requestPasteReview(text: "echo after\npwd")
        XCTAssertEqual(offered.count, 2)
        let freshReview = try XCTUnwrap(offered.last)
        XCTAssertNotEqual(freshReview.id, oldReview.id)
        try view.confirmPaste(id: freshReview.id)
        await view.session.waitUntilSettled(); try await drain(coordinator, source)
        let after = await transport.bytes()
        XCTAssertEqual(after, Data("\u{1B}[200~echo after\npwd\u{1B}[201~".utf8))
        view.retire(); await view.session.waitUntilSuspended()
    }
    func testRetirementNotifiesDelegateAndLateUIKitCallbacksCannotCommit() async throws {
        let (coordinator, transport, source, view) = try await setup()
        let delegate = NativeInputDelegate(); view.inputDelegate = delegate
        view.setMarkedText("private draft", selectedRange: NSRange(location: 13, length: 0))
        XCTAssertTrue(delegate.calls.isEmpty, "UIKit-initiated mutations must not recurse through its delegate")
        view.retire(); view.unmarkText(); view.insertText("late"); view.deleteBackward()
        await view.session.waitUntilSuspended(); try await drain(coordinator, source)
        let output = await transport.bytes(); XCTAssertTrue(output.isEmpty)
        XCTAssertEqual(delegate.calls, ["textWillChange", "selectionWillChange", "selectionDidChange", "textDidChange"])
        XCTAssertFalse(view.canBecomeFirstResponder)
        XCTAssertFalse(view.hasText)
    }
    func testPasteNeedsSourceBoundConfirmationAndDoesNotAppendReturn() async throws {
        let (coordinator, transport, source, view) = try await setup()
        var offered: TerminalPasteReview?
        view.onPasteReview = { offered = $0 }
        view.requestPasteReview(text: "echo one\necho two")
        let review = try XCTUnwrap(offered)
        XCTAssertEqual(review.focus, view.session.originalFocus)
        XCTAssertEqual(view.session.accounting.pendingEvents, 0)
        XCTAssertThrowsError(try view.confirmPaste(id: UUID()))
        try view.confirmPaste(id: review.id)
        XCTAssertThrowsError(try view.confirmPaste(id: review.id))
        await view.session.waitUntilSettled(); try await drain(coordinator, source)
        let output = await transport.bytes()
        XCTAssertEqual(output, Data("\u{1B}[200~echo one\necho two\u{1B}[201~".utf8))
        view.retire(); await view.session.waitUntilSuspended()
    }
    func testPreparingAndShowingReviewBlockOrdinaryInputButKeepExplicitPasteFocus() async throws {
        let (coordinator, transport, source, view) = try await setup()
        let focus = view.session.originalFocus
        var offered: TerminalPasteReview?
        view.onPasteReview = { offered = $0 }
        // The session installs this hold before requesting the replacement
        // native responder, even before the draft has offered a paste candidate.
        view.setReviewingPaste(true)
        XCTAssertFalse(view.isEditable)
        XCTAssertTrue(view.canReviewPaste)
        XCTAssertTrue(view.canBecomeFirstResponder)
        view.insertText("\n"); view.insertText("\t"); view.insertText("hidden command")
        view.deleteBackward()
        view.setMarkedText("候補", selectedRange: NSRange(location: 2, length: 0)); view.unmarkText()
        XCTAssertFalse(view.session.document.isMarked)
        XCTAssertEqual(view.session.accounting.pendingEvents, 0)
        view.requestPasteReview(text: "reviewed\ntext")
        let review = try XCTUnwrap(offered)
        XCTAssertEqual(review.focus, focus)
        view.insertText("\n"); view.insertText("hidden suffix"); view.deleteBackward()
        await view.session.waitUntilSettled(); try await drain(coordinator, source)
        let before = await transport.bytes(); XCTAssertTrue(before.isEmpty)
        try view.confirmPaste(id: review.id)
        view.insertText("\n")
        await view.session.waitUntilSettled(); try await drain(coordinator, source)
        let confirmed = await transport.bytes()
        XCTAssertEqual(confirmed, Data("\u{1B}[200~reviewed\ntext\u{1B}[201~".utf8))
        XCTAssertFalse(view.isEditable, "panel ownership outlives the consumed candidate")
        view.setReviewingPaste(false)
        XCTAssertTrue(view.isEditable)
        XCTAssertEqual(view.session.originalFocus, focus)
        view.insertText("visible")
        await view.session.waitUntilSettled(); try await drain(coordinator, source)
        let after = await transport.bytes()
        XCTAssertEqual(after, confirmed + Data("visible".utf8))
        view.retire(); await view.session.waitUntilSuspended()
    }
    func testPendingReviewBlocksInputUntilCancellationAndCannotSurviveRetirement() async throws {
        let (coordinator, transport, source, view) = try await setup()
        view.onPasteReview = { _ in }
        view.requestPasteReview(text: "unconfirmed")
        let review = try XCTUnwrap(view.pendingPaste)
        // Even an adapter that has not installed a separate panel hold cannot
        // expose the hidden shell while its native candidate is pending.
        view.setReviewingPaste(false)
        XCTAssertFalse(view.isEditable)
        view.insertText("\n"); view.deleteBackward()
        view.cancelPaste(id: review.id)
        XCTAssertTrue(view.isEditable)
        XCTAssertThrowsError(try view.confirmPaste(id: review.id))
        view.setReviewingPaste(true)
        view.requestPasteReview(text: "also unconfirmed")
        let retiredReview = try XCTUnwrap(view.pendingPaste)
        view.retire(); view.setReviewingPaste(false)
        XCTAssertFalse(view.isEditable)
        XCTAssertFalse(view.canReviewPaste)
        XCTAssertThrowsError(try view.confirmPaste(id: retiredReview.id))
        await view.session.waitUntilSuspended(); try await drain(coordinator, source)
        let output = await transport.bytes(); XCTAssertTrue(output.isEmpty)
    }
    func testUnsafePasteIsRejectedWithoutCreatingAReview() async throws {
        let (_, _, _, view) = try await setup()
        var reviews = 0, failures = 0
        view.onPasteReview = { _ in reviews += 1 }
        view.onFailure = { _ in failures += 1 }
        view.requestPasteReview(text: "\u{1B}[201~execute\n")
        view.requestPasteReview(text: String(repeating: "x", count: 32_769))
        XCTAssertEqual(reviews, 0); XCTAssertEqual(failures, 2); XCTAssertNil(view.pendingPaste)
        view.retire(); await view.session.waitUntilSuspended()
    }

    /// One window holding the terminal that has the keyboard and a holder.
    private func keyboardWindow(_ view: NativeTerminalInputView,
                                deadline: Duration = .seconds(5)) -> (UIWindow, TerminalKeyboardHolder) {
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 320, height: 480))
        window.makeKeyAndVisible()
        let holder = TerminalKeyboardHolder(deadline: deadline)
        window.addSubview(holder); window.addSubview(view)
        view.keyboardHolder = holder
        XCTAssertTrue(view.becomeFirstResponder())
        return (window, holder)
    }

    func testAHandoffKeepsTheKeyboardUpUntilTheNextTerminalTakesIt() async throws {
        let (coordinator, transport, source, first) = try await setup()
        let (window, holder) = keyboardWindow(first)
        defer { window.isHidden = true }
        let between = try await first.session.handoff(to: nil)
        XCTAssertTrue(holder.isHolding, "between terminals the keyboard stays up")
        XCTAssertFalse(first.isFirstResponder)
        holder.insertText("x"); holder.deleteBackward()
        holder.setMarkedText("か", selectedRange: NSRange(location: 1, length: 0))
        XCTAssertEqual(holder.text, "", "the holder keeps no keys, composed or not")
        XCTAssertNil(holder.markedTextRange)

        let focus = try await coordinator.changeFocus(between, to: source)
        let session = try TerminalKeyboardSession(focus: focus, coordinator: coordinator, encoder: UIKitInputEncoder())
        let second = try NativeTerminalInputView(session: session)
        second.frame = window.bounds
        try second.updateCaret(NativeTerminalCaret(source: source, revision: 1, rect: CGRect(x: 20, y: 20, width: 2, height: 20)))
        window.addSubview(second)
        XCTAssertTrue(second.becomeFirstResponder())
        XCTAssertFalse(holder.isHolding, "the next terminal took the keyboard")
        XCTAssertFalse(holder.canBecomeFirstResponder)
        second.insertText("y")
        await session.waitUntilSettled(); try await drain(coordinator, source)
        let sent = await transport.bytes()
        XCTAssertEqual(sent, Data("y".utf8), "keys typed into the holder reach no terminal")
        second.retire(); await session.waitUntilSuspended()
    }

    func testASuspendedTerminalLetsTheKeyboardGoAndAnUnclaimedHoldEnds() async throws {
        let (_, _, _, first) = try await setup()
        let (window, holder) = keyboardWindow(first)
        defer { window.isHidden = true }
        first.session.suspend()
        await first.session.waitUntilSuspended()
        XCTAssertFalse(holder.isHolding, "a suspension is not a handoff: the keyboard goes down")
        XCTAssertFalse(first.isFirstResponder)

        let (_, _, _, other) = try await setup()
        let (otherWindow, shortHolder) = keyboardWindow(other, deadline: .milliseconds(100))
        defer { otherWindow.isHidden = true }
        _ = try await other.session.handoff(to: nil)
        XCTAssertTrue(shortHolder.isHolding)
        for _ in 0..<100 where shortHolder.isHolding { try await Task.sleep(for: .milliseconds(20)) }
        XCTAssertFalse(shortHolder.isHolding, "no terminal took it: the keyboard goes down at the deadline")
        XCTAssertFalse(shortHolder.isFirstResponder)
    }

    func testATappedLinkShowsWhereItGoesBeforeAnythingOpens() async throws {
        let coordinator = try TerminalInputCoordinator()
        let source = try await coordinator.attach(try UIKitInputTransport())
        let focus = try await coordinator.changeFocus(try await coordinator.makeStream(sceneID: UUID()), to: source)
        let view = try await NativeTerminalView(session: try TerminalKeyboardSession(
            focus: focus, coordinator: coordinator, encoder: UIKitInputEncoder()))
        let at = TerminalGridPoint(row: 0, column: 0)
        let cell = CGRect(x: 40, y: 60, width: 8, height: 16)
        let web = view.linkSheet(for: .init(destination: "https://example.com/pr/1?x=端末", start: at, end: at), from: cell)
        XCTAssertEqual(web.message, "https://example.com/pr/1?x=端末", "the exact destination, not a label")
        XCTAssertEqual(web.actions.map(\.title), ["Open", "Copy Link", "Cancel"])
        XCTAssertEqual(web.popoverPresentationController?.sourceRect, cell, "on iPad it points at the tapped cell")
        let file = view.linkSheet(for: .init(destination: "file:///etc/hosts", start: at, end: at), from: cell)
        XCTAssertEqual(file.message, "file:///etc/hosts")
        XCTAssertEqual(file.actions.map(\.title), ["Copy Link", "Cancel"], "only web and mail addresses open")
    }
}
#endif
