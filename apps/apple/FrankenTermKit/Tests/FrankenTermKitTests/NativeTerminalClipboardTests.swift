#if canImport(UIKit) && canImport(MetalKit)
import UIKit
import XCTest
@testable import FrankenTermKit

/// Apple SDK tests. These must run in a UIKit test host; Linux excludes them.
/// The writer is injected so tests never read or overwrite the user's clipboard.
@MainActor
final class NativeTerminalClipboardTests: XCTestCase {
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
