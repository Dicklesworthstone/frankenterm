#if canImport(UIKit) && canImport(MetalKit)
import UIKit
import XCTest
@testable import FrankenTermKit

private actor CommandEditorTestTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    init() throws {
        identity = try .init(workspaceID: "workspace", machineID: "machine", routeID: "route", routeRevision: 1,
                             transportID: UUID(), channelID: UUID())
    }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        XCTFail("a local editor must not write to its terminal")
        return .failed
    }
    func close() async {}
}

private actor ClipboardInspectionTestEncoder: TerminalKeyboardEncoding {
    func encode(_ intent: TerminalKeyboardIntent, source: TerminalSource, maximumBytes: Int) async throws -> Data {
        if case .releaseAll = intent { return Data() }
        throw TerminalKeyboardError.encoding
    }
}

/// Apple SDK tests. These must run in a UIKit test host; Linux excludes them.
/// The writer is injected so tests never read or overwrite the user's clipboard.
@MainActor
final class NativeTerminalClipboardTests: XCTestCase {
    private func commandDraft() async throws -> TerminalCommandDraft<TerminalCommandDraftDestination> {
        let coordinator = try TerminalInputCoordinator()
        let source = try await coordinator.attach(CommandEditorTestTransport())
        return TerminalCommandDraft(destination: .init(source: source))
    }

    func testLocalCommandEditorKeepsReturnAndIMETextOutOfPasteAdmission() async throws {
        let draft = try await commandDraft()
        let view = NativeTerminalCommandTextView(draft: draft)
        XCTAssertTrue(view.isEditable)
        XCTAssertTrue(view.isSelectable)
        XCTAssertEqual(view.autocapitalizationType, .none)
        XCTAssertEqual(view.autocorrectionType, .no)
        XCTAssertEqual(view.smartQuotesType, .no)
        XCTAssertEqual(view.smartDashesType, .no)
        XCTAssertEqual(view.smartInsertDeleteType, .no)
        XCTAssertEqual(view.writingToolsBehavior, .none)
        XCTAssertEqual(view.returnKeyType, .default)
        let value = "printf '候補 👩🏽‍💻'\n"
        XCTAssertTrue(view.textView(view, shouldChangeTextIn: NSRange(location: 0, length: 0), replacementText: value))
        view.text = value; view.textViewDidChange(view)
        XCTAssertEqual(draft.text, value)
        XCTAssertEqual(draft.phase, .editing)
        XCTAssertTrue(view.textView(view, shouldChangeTextIn: NSRange(location: value.utf16.count, length: 0), replacementText: "\n"))
        XCTAssertEqual(draft.text, value, "a delegate permission alone does not edit or submit")
        draft.retire(); view.clearDraft()
        XCTAssertEqual(view.text, "")
        XCTAssertFalse(view.isEditable)
        XCTAssertFalse(view.textView(view, shouldChangeTextIn: NSRange(location: 0, length: 0), replacementText: "late"))
    }

    func testCommandEditorRefusesUnsafeReplacementAndRestoresOversizedNativeDocument() async throws {
        let draft = try await commandDraft()
        try draft.update("kept")
        let view = NativeTerminalCommandTextView(draft: draft)
        var failures = 0
        view.onFailure = { _ in failures += 1 }
        XCTAssertFalse(view.textView(view, shouldChangeTextIn: NSRange(location: 0, length: 4), replacementText: "\u{1B}[201~"))
        XCTAssertEqual(draft.text, "kept")
        view.text = String(repeating: "界", count: 11_000)
        view.textViewDidChange(view)
        XCTAssertEqual(view.text, "kept")
        XCTAssertEqual(view.selectedRange, NSRange(location: 4, length: 0))
        XCTAssertEqual(draft.text, "kept")
        XCTAssertEqual(failures, 2)
        draft.retire(); view.clearDraft()
    }

    func testNativeCopyUsesWholeCanonicalCellAndCustomWriter() async throws {
        let document = try TerminalCopyDocument(rows: [[
            .init(column: 0, width: 1, text: "x"), .init(column: 1, width: 2, text: "👩🏽‍💻"),
            .init(column: 3, width: 1, text: "secret", concealed: true)
        ]], columns: 4)
        var copied: [String] = []
        let view = NativeTerminalCopyTextView(document: document) { copied.append($0) }
        view.selectedRange = NSRange(location: 1, length: "👩🏽‍💻".utf16.count)
        view.copy(nil)
        XCTAssertEqual(copied, ["👩🏽‍💻"])
        XCTAssertEqual(view.text, "x👩🏽‍💻 ")
        XCTAssertFalse(view.isEditable)
        XCTAssertTrue(view.isSelectable)
        XCTAssertEqual(view.dataDetectorTypes, [])
        XCTAssertEqual(view.textDragInteraction?.isEnabled, false)
        XCTAssertEqual(view.writingToolsBehavior, .none)
    }

    func testRetirementRevokesMenuAndRetainedCopyCallback() async throws {
        let document = try TerminalCopyDocument(rows: [[.init(column: 0, width: 1, text: "x")]], columns: 1)
        var copies = 0
        let view = NativeTerminalCopyTextView(document: document) { _ in copies += 1 }
        view.selectedRange = NSRange(location: 0, length: 1)
        XCTAssertTrue(view.canPerformAction(#selector(UIResponderStandardEditActions.copy(_:)), withSender: nil))
        XCTAssertFalse(view.canPerformAction(#selector(UIResponderStandardEditActions.paste(_:)), withSender: nil))
        XCTAssertFalse(view.canPerformAction(#selector(UIResponderStandardEditActions.cut(_:)), withSender: nil))
        view.clearSnapshot()
        view.copy(nil)
        XCTAssertEqual(copies, 0)
        XCTAssertEqual(view.text, "")
        XCTAssertFalse(view.canPerformAction(#selector(UIResponderStandardEditActions.copy(_:)), withSender: nil))
        XCTAssertFalse(view.canPerformAction(#selector(UIResponderStandardEditActions.selectAll(_:)), withSender: nil))
    }

    func testEmptySelectionDoesNotWriteClipboard() async throws {
        let document = try TerminalCopyDocument(rows: [[.init(column: 0, width: 1, text: "x")]], columns: 1)
        var copies = 0, failures = 0
        let view = NativeTerminalCopyTextView(document: document) { _ in copies += 1 }
        view.onFailure = { _ in failures += 1 }
        view.selectedRange = NSRange(location: 0, length: 0)
        view.copy(nil)
        XCTAssertEqual(copies, 0)
        XCTAssertEqual(failures, 1)
    }

    func testDiagnosticSelectionRequiresAnExplicitRangeAndNeverWritesClipboard() async throws {
        let document = try TerminalCopyDocument(rows: [[
            .init(column: 0, width: 1, text: "X"), .init(column: 1, width: 2, text: "👩🏽‍💻"),
            .init(column: 3, width: 1, text: "Y")
        ]], columns: 4)
        let context = try TerminalDiagnosticExcerpt.Context(destinationLabel: "machine", receivedAt: Date())
        var copies = 0
        let view = NativeTerminalCopyTextView(document: document) { _ in copies += 1 }
        view.selectedRange = NSRange(location: 0, length: 0)
        XCTAssertThrowsError(try view.makeDiagnosticExcerpt(context: context))
        view.selectedRange = NSRange(location: 2, length: 1)
        let excerpt = try view.makeDiagnosticExcerpt(context: context)
        XCTAssertEqual(excerpt.text, "👩🏽‍💻")
        XCTAssertEqual(view.selectedRange, NSRange(location: 1, length: "👩🏽‍💻".utf16.count))
        XCTAssertEqual(copies, 0)
        view.clearSnapshot()
        XCTAssertThrowsError(try view.makeDiagnosticExcerpt(context: context))
        XCTAssertEqual(excerpt.text, "👩🏽‍💻", "the explicit selection is independent of a released viewport")
        excerpt.retire()
    }

    func testNativeDiagnosticEditorRedactsWholeGraphemesWithoutOfferingUnreviewedShare() async throws {
        let document = try TerminalCopyDocument(rows: [[
            .init(column: 0, width: 1, text: "A"), .init(column: 1, width: 2, text: "👩🏽‍💻")
        ]], columns: 3)
        let excerpt = try TerminalDiagnosticExcerpt(document: document,
            selection: NSRange(location: 0, length: document.utf16Count),
            context: .init(destinationLabel: "machine", receivedAt: Date()))
        let view = NativeTerminalDiagnosticTextView(excerpt: excerpt)
        XCTAssertTrue(view.isEditable)
        XCTAssertEqual(view.autocapitalizationType, .none)
        XCTAssertEqual(view.autocorrectionType, .no)
        XCTAssertEqual(view.smartQuotesType, .no)
        XCTAssertEqual(view.smartDashesType, .no)
        XCTAssertEqual(view.smartInsertDeleteType, .no)
        XCTAssertEqual(view.writingToolsBehavior, .none)
        XCTAssertEqual(view.dataDetectorTypes, [])
        XCTAssertEqual(view.textDragInteraction?.isEnabled, false)
        XCTAssertFalse(view.canPerformAction(NSSelectorFromString("_share:"), withSender: nil))
        XCTAssertFalse(view.canPerformAction(NSSelectorFromString("_lookup:"), withSender: nil))
        view.selectedRange = NSRange(location: 2, length: 1)
        XCTAssertFalse(view.canPerformAction(#selector(UIResponderStandardEditActions.cut(_:)), withSender: nil))
        XCTAssertFalse(view.canPerformAction(#selector(UIResponderStandardEditActions.copy(_:)), withSender: nil))
        view.copy(nil); view.cut(nil)
        XCTAssertEqual(view.text, "A👩🏽‍💻", "direct edit selectors cannot bypass the protected clipboard path")
        XCTAssertEqual(excerpt.text, "A👩🏽‍💻")
        view.redactSelection()
        XCTAssertEqual(view.text, "A[redacted]")
        XCTAssertEqual(excerpt.text, "A[redacted]")
        XCTAssertEqual(view.selectedRange, NSRange(location: 1, length: "[redacted]".utf16.count))
        XCTAssertEqual(excerpt.phase, .editing)
        excerpt.retire(); view.clearExcerpt()
        XCTAssertEqual(view.text, "")
        XCTAssertFalse(view.isEditable)
        XCTAssertFalse(view.canPerformAction(#selector(UIResponderStandardEditActions.copy(_:)), withSender: nil))
        XCTAssertFalse(view.textView(view, shouldChangeTextIn: NSRange(location: 0, length: 0), replacementText: "late"))
    }

    func testDiagnosticEditorRefusesControlsAndRestoresAnOversizedNativeDocument() async throws {
        let document = try TerminalCopyDocument(rows: [[.init(column: 0, width: 1, text: "kept")]], columns: 1)
        let excerpt = try TerminalDiagnosticExcerpt(document: document,
            selection: NSRange(location: 0, length: document.utf16Count),
            context: .init(destinationLabel: "machine", receivedAt: Date()))
        let view = NativeTerminalDiagnosticTextView(excerpt: excerpt)
        var failures = 0
        view.onFailure = { _ in failures += 1 }
        XCTAssertFalse(view.textView(view, shouldChangeTextIn: NSRange(location: 0, length: 4), replacementText: "\u{1B}[31m"))
        view.text = String(repeating: "界", count: 44_000)
        view.textViewDidChange(view)
        XCTAssertEqual(view.text, "kept")
        XCTAssertEqual(excerpt.text, "kept")
        XCTAssertEqual(view.selectedRange, NSRange(location: 4, length: 0))
        XCTAssertEqual(failures, 2)
        XCTAssertTrue(view.textView(view, shouldChangeTextIn: NSRange(location: 4, length: 0), replacementText: "\nnext"))
        view.text = "kept\nnext"; view.textViewDidChange(view)
        XCTAssertEqual(excerpt.text, "kept\nnext")
        excerpt.retire(); view.clearExcerpt()
    }

    func testReviewedNativeShareItemIsExactAnchoredAndRevokedOnRetirement() async throws {
        let document = try TerminalCopyDocument(rows: [[.init(column: 0, width: 1, text: "private")]], columns: 1)
        let excerpt = try TerminalDiagnosticExcerpt(document: document,
            selection: NSRange(location: 0, length: document.utf16Count),
            context: .init(destinationLabel: "machine", receivedAt: Date()))
        _ = try excerpt.redact(NSRange(location: 0, length: 7))
        let review = try excerpt.prepareReview()
        var offered: NativeTerminalDiagnosticShareItem?
        try excerpt.share(reviewID: review.id) { offered = try NativeTerminalDiagnosticShareItem(text: $0) }
        let item = try XCTUnwrap(offered)
        let anchor = UIView(frame: CGRect(x: 10, y: 20, width: 120, height: 44))
        let activity = item.makeActivityController(anchor: anchor)
        XCTAssertEqual(item.activityViewControllerPlaceholderItem(activity) as? String, "")
        XCTAssertEqual(item.activityViewController(activity, itemForActivityType: nil) as? String, "[redacted]")
        XCTAssertEqual(item.activityViewController(activity, dataTypeIdentifierForActivityType: nil), "public.utf8-plain-text")
        XCTAssertEqual(activity.excludedActivityTypes, [.copyToPasteboard])
        if let popover = activity.popoverPresentationController {
            XCTAssertTrue(popover.sourceView === anchor)
            XCTAssertEqual(popover.sourceRect, anchor.bounds)
        } else if UIDevice.current.userInterfaceIdiom == .pad {
            XCTFail("an iPad share needs an anchored popover")
        }
        item.retire()
        XCTAssertNil(item.activityViewController(activity, itemForActivityType: .mail))
        XCTAssertThrowsError(try NativeTerminalDiagnosticShareItem(text: ""))
        XCTAssertThrowsError(try NativeTerminalDiagnosticShareItem(text: String(repeating: "x", count: TerminalDiagnosticExcerpt.maximumSharedUTF8Bytes + 1)))
    }

    func testInspectionRefusesPendingBlockedAndMarkedInputWithoutDiscardingIt() async throws {
        let coordinator = try TerminalInputCoordinator()
        let source = try await coordinator.attach(CommandEditorTestTransport())
        let encoder = ClipboardInspectionTestEncoder()
        func keyboard() async throws -> TerminalKeyboardSession {
            let stream = try await coordinator.makeStream(sceneID: UUID())
            let focus = try await coordinator.changeFocus(stream, to: source)
            return try TerminalKeyboardSession(focus: focus, coordinator: coordinator, encoder: encoder)
        }
        let pending = try await keyboard()
        XCTAssertNoThrow(try NativeTerminalClipboardController.validateInputForInspection(pending))
        try pending.insertText("not yet admitted")
        XCTAssertThrowsError(try NativeTerminalClipboardController.validateInputForInspection(pending)) { error in
            XCTAssertEqual(error as? TerminalClipboardError, .pendingInput)
        }
        XCTAssertEqual(pending.accounting.discardedUnadmittedEvents, 0)
        await pending.waitUntilSettled()
        XCTAssertEqual(pending.phase, .blocked)
        XCTAssertThrowsError(try NativeTerminalClipboardController.validateInputForInspection(pending))
        XCTAssertEqual(pending.accounting.pendingEvents, 1)
        XCTAssertEqual(pending.accounting.discardedUnadmittedEvents, 0)
        pending.suspend(); await pending.waitUntilSuspended()
        XCTAssertNoThrow(try NativeTerminalClipboardController.validateInputForInspection(pending),
                         "read-only history inspection does not require live input or a caret")
        let marked = try await keyboard()
        try marked.setMarkedText("候補", selectedRange: NSRange(location: 2, length: 0))
        XCTAssertThrowsError(try NativeTerminalClipboardController.validateInputForInspection(marked))
        XCTAssertEqual(marked.document.text, "候補")
        XCTAssertEqual(marked.accounting.discardedUnadmittedEvents, 0)
        marked.suspend(); await marked.waitUntilSuspended()
    }
}
#endif
