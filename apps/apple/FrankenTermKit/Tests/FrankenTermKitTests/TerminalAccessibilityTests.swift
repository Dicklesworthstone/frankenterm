import Foundation
import XCTest
@testable import FrankenTermKit

private actor AccessibilityTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    init() throws {
        identity = try .init(workspaceID: "reading", machineID: "host", routeID: "ssh",
            routeRevision: 1, transportID: UUID(), channelID: UUID())
    }
    func writeSome(_ bytes: Data) -> TerminalTransportWrite { .failed }
    func close() {}
}

@MainActor
final class TerminalAccessibilityTests: XCTestCase {
    private func source() async throws -> TerminalSource {
        try await TerminalInputCoordinator().attach(AccessibilityTransport())
    }

    private func cell(_ text: String, column: Int = 0, width: Int = 1,
                      concealed: Bool = false) -> TerminalRenderCell {
        .init(column: column, width: width, text: text, flags: concealed ? 1 << 10 : 0,
              foreground: .default, background: .default, underline: .default, linkID: 0)
    }

    private func frame(_ source: TerminalSource, rows: [[TerminalRenderCell]],
                       columns: Int = 8, wraps: Set<Int> = [], revision: UInt64 = 1,
                       geometry: UInt64 = 1, cursor: Int? = nil,
                       history: Bool = false, retained: Bool = false,
                       coherentWraps: Bool = true) -> TerminalRenderFrame {
        let context = TerminalRowCopyContext(source: source, geometryRevision: geometry,
            terminalSequence: revision, alternateScreen: false)
        return .init(source: source, sequence: revision, geometryRevision: geometry,
            paletteRevision: 1, fontRevision: 1, revision: revision, rows: rows.count,
            columns: columns, viewportFirst: 100, flags: retained ? 9 : 1,
            cursor: .init(stableRow: Int64(100 + (cursor ?? 0)), column: 0,
                          shape: 1, width: 1, visible: cursor != nil),
            lines: rows.enumerated().map { index, cells in
                .init(stableIndex: Int64(100 + index), sequence: revision, cells: cells,
                      wrapsToNext: wraps.contains(index), copyContext: coherentWraps ? context : nil)
            }, palette: Array(repeating: .init(0, 0, 0), count: 265), links: [],
            history: .init(first: 90, live: history ? 110 : 100))
    }

    func testLogicalLinesKeepWideGraphemesAndSpacesAtProvenWraps() async throws {
        let source = try await source()
        let capture = frame(source, rows: [
            [cell("A"), cell("界", column: 1, width: 2), cell(" ", column: 3)],
            [cell("e\u{301}"), cell("👩‍💻", column: 1, width: 2)],
            [cell("Next")], [], []
        ], columns: 4, wraps: [0], cursor: 2)
        let snapshot = try TerminalAccessibilitySnapshot(frame: capture)
        XCTAssertEqual(snapshot.lines.map(\.text), ["A界 e\u{301}👩‍💻", "Next"])
        XCTAssertEqual(snapshot.lines[0].viewportRows, 0...1)
        XCTAssertEqual(snapshot.lines[0].firstRow, 100)
        XCTAssertEqual(snapshot.lines[0].lastRow, 101)
        XCTAssertEqual(snapshot.cursorLineIndex, 1)
        XCTAssertEqual(snapshot.textByteCount, snapshot.lines.reduce(0) { $0 + $1.text.utf8.count })
    }

    func testUnknownWrapsStaySeparateAndConcealedTextIsNeverRead() async throws {
        let source = try await source()
        let capture = frame(source, rows: [
            [cell("go"), cell("secret", column: 2, width: 2, concealed: true)],
            [cell("next")], []
        ], columns: 4, wraps: [0], cursor: 2, coherentWraps: false)
        let snapshot = try TerminalAccessibilitySnapshot(frame: capture)
        XCTAssertEqual(snapshot.lines.map(\.text), ["go", "next", ""])
        XCTAssertEqual(snapshot.lines[2].spokenText, "Blank line")
        XCTAssertEqual(snapshot.cursorLineIndex, 2)
        XCTAssertEqual(snapshot.newestOutputIndex, 1)
        XCTAssertFalse(snapshot.lines.map(\.spokenText).joined().contains("secret"))
    }

    func testWideCharacterMovedToNextRowJoinsWithoutAddingSpacer() async throws {
        let source = try await source()
        let capture = frame(source, rows: [
            [cell("a"), cell("b", column: 1), cell("c", column: 2)],
            [cell("界", width: 2), cell("d", column: 2)]
        ], columns: 4, wraps: [0])
        XCTAssertEqual(try TerminalAccessibilitySnapshot(frame: capture).lines.map(\.text), ["abc界d"])
    }

    func testBudgetsFavorRecentLinesAndNeverSplitAnEmojiOrCombiningCharacter() async throws {
        let source = try await source()
        let capture = frame(source, rows: [[cell("old")], [cell("older")], [cell("e\u{301}👩‍💻z")]])
        let snapshot = try TerminalAccessibilitySnapshot(frame: capture, maximumLines: 2,
            maximumTextBytes: 8, maximumLineBytes: 5)
        XCTAssertEqual(snapshot.lines.map(\.text), ["older", "e\u{301}"])
        XCTAssertEqual(snapshot.omittedLines, 1)
        XCTAssertEqual(snapshot.textByteCount, 8)
        XCTAssertTrue(snapshot.lines[1].truncated)
        XCTAssertTrue(snapshot.lines[1].spokenText.contains("not included"))
        XCTAssertThrowsError(try TerminalAccessibilitySnapshot(frame: capture, maximumLines: 513))
        XCTAssertThrowsError(try TerminalAccessibilitySnapshot(frame: capture, maximumTextBytes: 0))
        XCTAssertThrowsError(try TerminalAccessibilitySnapshot(frame: capture, maximumLineBytes: 4097))
    }

    func testOutputFloodDoesNotReplaceTheReadingSnapshotUntilRefreshOrExit() async throws {
        let source = try await source()
        var reader = TerminalAccessibilityReading(source: source)
        let first = try reader.capture(frame(source, rows: [[cell("first")]]))
        reader.setReading(true)
        for revision in UInt64(2)...100 {
            let current = try reader.capture(frame(source, rows: [[cell("changed")]], revision: revision))
            XCTAssertEqual(current, first)
        }
        let current = frame(source, rows: [[cell("latest")]], revision: 101)
        XCTAssertEqual(try reader.capture(current, refresh: true).lines[0].text, "latest")
        reader.setReading(false)
        XCTAssertEqual(try reader.capture(frame(source, rows: [[cell("returned")]], revision: 102)).lines[0].text, "returned")
        reader.discard()
        XCTAssertNil(reader.snapshot)
        XCTAssertFalse(reader.isReading)
    }

    func testSourceAndRevisionCannotBeChangedUnderAReadingFocus() async throws {
        let source = try await source(), other = try await self.source()
        var reader = TerminalAccessibilityReading(source: source)
        _ = try reader.capture(frame(source, rows: [[cell("original")]], revision: 2))
        reader.setReading(true)
        XCTAssertThrowsError(try reader.capture(frame(other, rows: [[cell("different host")]], revision: 3))) {
            XCTAssertEqual($0 as? TerminalRenderError, .staleSource)
        }
        XCTAssertThrowsError(try reader.capture(frame(source, rows: [[cell("old")]]))) {
            XCTAssertEqual($0 as? TerminalRenderError, .staleFrame)
        }
        XCTAssertEqual(reader.snapshot?.lines.first?.text, "original")
    }

    func testHistoryIsNamedAndNeverClaimsAnOffscreenCursor() async throws {
        let source = try await source()
        let snapshot = try TerminalAccessibilitySnapshot(frame: frame(source,
            rows: [[cell("history")], []], cursor: 1, history: true, retained: true))
        XCTAssertFalse(snapshot.isLive)
        XCTAssertTrue(snapshot.isRetained)
        XCTAssertNil(snapshot.cursorLineIndex)
        XCTAssertEqual(snapshot.lines.count, 1)
    }

    func testLineNavigationStopsAtEachBoundaryWithoutWraparound() async throws {
        let source = try await source()
        let snapshot = try TerminalAccessibilitySnapshot(frame: frame(source,
            rows: [[cell("one")], [cell("two")], [cell("three")]]))
        XCTAssertEqual(snapshot.nextLine(after: nil, forward: true), 0)
        XCTAssertEqual(snapshot.nextLine(after: nil, forward: false), 2)
        XCTAssertEqual(snapshot.nextLine(after: 0, forward: true), 1)
        XCTAssertEqual(snapshot.nextLine(after: 2, forward: false), 1)
        XCTAssertNil(snapshot.nextLine(after: 0, forward: false))
        XCTAssertNil(snapshot.nextLine(after: 2, forward: true))
        XCTAssertNil(snapshot.nextLine(after: 100, forward: true))
    }
}
