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
}
#endif
