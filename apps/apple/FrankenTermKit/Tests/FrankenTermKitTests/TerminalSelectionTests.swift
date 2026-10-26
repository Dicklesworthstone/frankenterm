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

private actor LinkTestTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    init() throws {
        identity = try TerminalConnectionIdentity(workspaceID: "w", machineID: "m", routeID: "r", routeRevision: 1,
                                                  transportID: UUID(), channelID: UUID())
    }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite { .failed }
    func close() async {}
}

final class TerminalLinkTests: XCTestCase {
    /// One cell per character of `text` from column 0, spaces as gaps; the
    /// characters at `linked` columns, spaces included, carry link 1.
    private func row(_ stable: Int64, _ text: String, linked: Range<Int> = 0..<0,
                     wraps: Bool = false) -> TerminalRenderRow {
        let cells = text.enumerated().compactMap { index, character -> TerminalRenderCell? in
            character == " " && !linked.contains(index) ? nil : .init(column: index, width: 1, text: String(character), flags: 0,
                foreground: .default, background: .default, underline: .default, linkID: linked.contains(index) ? 1 : 0)
        }
        return TerminalRenderRow(stableIndex: stable, sequence: 1, cells: cells, wrapsToNext: wraps)
    }
    private func frame(_ rows: [TerminalRenderRow], columns: Int, links: [String] = []) async throws -> TerminalRenderFrame {
        let source = try await TerminalInputCoordinator().attach(LinkTestTransport())
        return TerminalRenderFrame(source: source, sequence: 1, geometryRevision: 1, paletteRevision: 1, fontRevision: 1,
            revision: 1, rows: rows.count, columns: columns, viewportFirst: rows.first?.stableIndex ?? 0, flags: 0,
            cursor: .init(stableRow: 0, column: 0, shape: 0, width: 1, visible: true), lines: rows,
            palette: Array(repeating: .init(0, 0, 0, 255), count: 265), links: links, history: nil)
    }
    private func point(_ row: Int64, _ column: Int) -> TerminalGridPoint { .init(row: row, column: column) }

    func testAProgramsLinkIsFoundOnEachOfItsCells() async throws {
        let shown = try await frame([row(3, "see PR #1 now", linked: 4..<9)], columns: 40,
                                    links: ["https://example.com/pr/1"])
        for column in 4..<9 {
            let link = TerminalLink.at(point(3, column), in: shown)
            XCTAssertEqual(link, .init(destination: "https://example.com/pr/1", start: point(3, 4), end: point(3, 8)))
        }
        XCTAssertNil(TerminalLink.at(point(3, 1), in: shown), "plain text around it is not the link")
        XCTAssertNil(TerminalLink.at(point(3, 11), in: shown))
    }

    func testAnAddressInTheTextIsFoundByTheDesktopsRules() async throws {
        let shown = try await frame([
            row(0, "open https://ex.com/a?b=1, then (https://ex.com/x) now"),
            row(1, "cargo test -p main.rs; ubuntu@ip-10-0-0-1.ec2.internal:~$"),
        ], columns: 80)
        // Trailing punctuation and wrapping brackets are not part of it.
        XCTAssertEqual(TerminalLink.at(point(0, 9), in: shown)?.destination, "https://ex.com/a?b=1")
        XCTAssertEqual(TerminalLink.at(point(0, 9), in: shown)?.start, point(0, 5))
        XCTAssertEqual(TerminalLink.at(point(0, 9), in: shown)?.end, point(0, 24))
        XCTAssertNil(TerminalLink.at(point(0, 25), in: shown), "the comma after it")
        XCTAssertEqual(TerminalLink.at(point(0, 40), in: shown)?.destination, "https://ex.com/x")
        XCTAssertNil(TerminalLink.at(point(0, 32), in: shown), "the bracket before it")
        // A file name or a prompt's user@host is not taken for an address.
        for column in [15, 30, 40] { XCTAssertNil(TerminalLink.at(point(1, column), in: shown), "column \(column)") }
    }

    func testAWrappedAddressContinuesOnlyWhereTheRowContinues() async throws {
        let wrapped = try await frame([row(0, "go https://example.co", wraps: true), row(1, "m/pull/42 now")], columns: 21)
        let whole = TerminalLink(destination: "https://example.com/pull/42", start: point(0, 3), end: point(1, 8))
        XCTAssertEqual(TerminalLink.at(point(0, 6), in: wrapped), whole)
        XCTAssertEqual(TerminalLink.at(point(1, 2), in: wrapped), whole)
        // A row that only fills its width, with no wrap, ends it.
        let broken = try await frame([row(0, "go https://example.co"), row(1, "m/pull/42 now")], columns: 21)
        XCTAssertEqual(TerminalLink.at(point(0, 6), in: broken)?.destination, "https://example.co")
        XCTAssertNil(TerminalLink.at(point(1, 2), in: broken))
    }

    func testAnAddressCutAtTheFramesEdgeIsNotOffered() async throws {
        // Scrolled back: the row it wraps on into is below the frame.
        let edge = try await frame([row(0, "see below"), row(1, "go https://example.co", wraps: true)], columns: 21)
        XCTAssertNil(TerminalLink.at(point(1, 6), in: edge), "it may go on in the row below")
        // Even cut just after punctuation the rules leave off its end.
        let dotted = try await frame([row(1, "go https://ex.com/a.", wraps: true)], columns: 20)
        XCTAssertNil(TerminalLink.at(point(1, 6), in: dotted))
        // One that ends before the edge, or on a row that does not wrap, is whole.
        let ended = try await frame([row(1, "go https://ex.com/ to", wraps: true)], columns: 21)
        XCTAssertEqual(TerminalLink.at(point(1, 6), in: ended)?.destination, "https://ex.com/")
        let unwrapped = try await frame([row(1, "go https://example.co")], columns: 21)
        XCTAssertEqual(TerminalLink.at(point(1, 6), in: unwrapped)?.destination, "https://example.co")
    }

    func testOnlyWebAndMailAddressesOpen() {
        func link(_ destination: String) -> TerminalLink { .init(destination: destination, start: point(0, 0), end: point(0, 0)) }
        XCTAssertEqual(link("https://example.com/a b").openableURL?.scheme, "https")
        XCTAssertNotNil(link("http://example.com").openableURL)
        XCTAssertNotNil(link("mailto:ops@example.com").openableURL)
        for other in ["file:///etc/passwd", "prefs:root=General", "x-apple-reminder://1", "ssh://host", "javascript:alert(1)"] {
            XCTAssertNil(link(other).openableURL, other)
        }
    }
}
