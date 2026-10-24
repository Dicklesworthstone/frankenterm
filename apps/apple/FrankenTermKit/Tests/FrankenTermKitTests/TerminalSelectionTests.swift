import XCTest
@testable import FrankenTermKit

final class TerminalSelectionTests: XCTestCase {
    private func cell(_ column: Int, _ text: String, width: Int = 1, concealed: Bool = false) -> TerminalRenderCell {
        .init(column: column, width: width, text: text, flags: concealed ? 1 << 10 : 0, foreground: .default,
              background: .default, underline: .default, linkID: 0)
    }
    /// One cell per character of `text` from column 0, spaces as gaps.
    private func row(_ stable: Int64, _ text: String) -> TerminalRenderRow {
        let cells = text.enumerated().compactMap { index, character in
            character == " " ? nil : cell(index, String(character))
        }
        return TerminalRenderRow(stableIndex: stable, sequence: 1, cells: cells)
    }
    private func point(_ row: Int64, _ column: Int) -> TerminalGridPoint { .init(row: row, column: column) }

    func testAPressSelectsTheWholePathOrUrlButNotItsQuotes() {
        let line = row(5, #"cat "/var/log/syslog" https://ex.com/a?b=1"#)
        let path = TerminalTextSelection.word(at: 9, in: line, columns: 60)
        XCTAssertEqual(path, .init(point(5, 5), point(5, 19)))
        XCTAssertEqual(path.text(rows: [5: line], columns: 60), "/var/log/syslog")
        let url = TerminalTextSelection.word(at: 30, in: line, columns: 60)
        XCTAssertEqual(url.text(rows: [5: line], columns: 60), "https://ex.com/a?b=1")
        // On a blank, just that cell.
        XCTAssertEqual(TerminalTextSelection.word(at: 3, in: line, columns: 60), .init(point(5, 3), point(5, 3)))
    }

    func testAWideCharacterSelectsBothItsColumns() {
        let line = TerminalRenderRow(stableIndex: 0, sequence: 1, cells: [cell(0, "界", width: 2), cell(2, " "), cell(3, "x")])
        XCTAssertEqual(TerminalTextSelection.word(at: 1, in: line, columns: 10), .init(point(0, 0), point(0, 1)))
        // Half a wide character selected copies all of it.
        XCTAssertEqual(TerminalTextSelection(point(0, 1), point(0, 3)).text(rows: [0: line], columns: 10), "界 x")
    }

    func testRowsReadAsOnScreenWithHiddenCellsAndTrailingBlanksHandled() {
        let first = row(10, "$ echo secret")
        let second = TerminalRenderRow(stableIndex: 11, sequence: 1, cells: [cell(0, "p"), cell(1, "w", concealed: true), cell(4, "d")])
        let third = row(12, "done      ")
        let selection = TerminalTextSelection(point(12, 1), point(10, 2))
        XCTAssertEqual(selection.start, point(10, 2), "ends are put in reading order")
        XCTAssertEqual(selection.text(rows: [10: first, 11: second, 12: third], columns: 20),
                       "echo secret\np   d\ndo")
        XCTAssertEqual(selection.columns(onRow: 11, width: 20), 0...19)
        XCTAssertEqual(selection.columns(onRow: 12, width: 20), 0...1)
        XCTAssertNil(selection.columns(onRow: 13, width: 20))
    }

    func testRowsNeverSeenAreLeftOutRatherThanInvented() {
        let selection = TerminalTextSelection(point(0, 0), point(2, 3))
        XCTAssertEqual(selection.text(rows: [0: row(0, "top"), 2: row(2, "end!")], columns: 10), "top\nend!")
    }

    func testControlCharactersNeverReachTheClipboard() {
        let line = TerminalRenderRow(stableIndex: 0, sequence: 1, cells: [cell(0, "a"), cell(1, "\u{1b}"), cell(2, "b")])
        XCTAssertEqual(TerminalTextSelection(point(0, 0), point(0, 2)).text(rows: [0: line], columns: 5), "ab")
    }

    func testExtremeStableRowsCannotOverflowSelectionIteration() {
        for stable in [Int64.min, Int64.max] {
            let selected = TerminalTextSelection(point(stable, 0), point(stable, 3))
            XCTAssertEqual(selected.text(rows: [stable: row(stable, "edge")], columns: 4), "edge")
        }
        let lastTwo = TerminalTextSelection(point(Int64.max - 1, 0), point(Int64.max, 3))
        XCTAssertEqual(lastTwo.text(rows: [Int64.max - 1: row(Int64.max - 1, "last"),
                                          Int64.max: row(Int64.max, "edge")], columns: 4), "last\nedge")
        let impossible = TerminalTextSelection(point(Int64.min, 0), point(Int64.max, 3))
        XCTAssertEqual(impossible.text(rows: [Int64.min: row(Int64.min, "edge")], columns: 4), "")
    }

    func testOversizedSelectionsReturnNoPartialCommand() {
        let selected = TerminalTextSelection(point(0, 0), point(TerminalTextSelection.maximumRows, 3))
        XCTAssertEqual(selected.text(rows: [0: row(0, "echo")], columns: 4), "")
        let mismatched = TerminalTextSelection(point(0, 0), point(0, 3))
        XCTAssertEqual(mismatched.text(rows: [0: row(1, "echo")], columns: 4), "")
    }
}
