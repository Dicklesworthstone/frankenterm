import Foundation
import XCTest
@testable import FrankenTermKit

final class TerminalClipboardTests: XCTestCase {
    private typealias Cell = TerminalCopyDocument.Cell

    func testCanonicalColumnsDoNotDuplicateWideGraphemesOrInventPadding() throws {
        let document = try TerminalCopyDocument(rows: [[
            Cell(column: 1, width: 2, text: "界"), Cell(column: 3, width: 1, text: "e\u{301}"),
            Cell(column: 5, width: 2, text: "👩🏽‍💻"), Cell(column: 7, width: 1, text: "!")
        ]], columns: 10)
        XCTAssertEqual(document.text, " 界e\u{301} 👩🏽‍💻!")
        XCTAssertEqual(document.physicalRows, 1)
        XCTAssertEqual(document.utf8Bytes, document.text.utf8.count)
        XCTAssertEqual(document.utf16Count, (document.text as NSString).length)
    }

    func testConcealedCellsNeverReachDocumentOrClipboard() throws {
        let document = try TerminalCopyDocument(rows: [[
            Cell(column: 0, width: 1, text: "secret", concealed: true),
            Cell(column: 1, width: 2, text: "👩🏽‍💻", concealed: true),
            Cell(column: 3, width: 1, text: "x")
        ]], columns: 4)
        XCTAssertEqual(document.text, "   x")
        XCTAssertEqual(try document.selectedText(in: NSRange(location: 0, length: 3)), "   ")
        XCTAssertFalse(document.debugDescription.contains("secret"))
    }

    func testPhysicalLineBreaksAndRepresentedWhitespaceArePreserved() throws {
        let document = try TerminalCopyDocument(rows: [
            [Cell(column: 0, width: 1, text: "a"), Cell(column: 1, width: 1, text: " ")],
            [], [Cell(column: 2, width: 1, text: "b")], []
        ], columns: 4)
        XCTAssertEqual(document.text, "a \n\n  b\n")
        XCTAssertEqual(document.physicalRows, 4)
    }

    func testSelectionExpandsEveryUTF16UnitInsideCanonicalGraphemes() throws {
        for grapheme in ["e\u{301}", "👨‍👩‍👧‍👦", "🇺🇸", "𝄞", "क्‍ष", "a" + String(repeating: "\u{301}", count: 100)] {
            let document = try TerminalCopyDocument(rows: [[Cell(column: 0, width: 1, text: "x"),
                Cell(column: 1, width: 2, text: grapheme), Cell(column: 3, width: 1, text: "z")]], columns: 4)
            for unit in 0..<grapheme.utf16.count {
                let selection = NSRange(location: 1 + unit, length: 1)
                XCTAssertEqual(try document.selectedText(in: selection), grapheme)
                XCTAssertEqual(try document.normalizedRange(selection), NSRange(location: 1, length: grapheme.utf16.count))
            }
        }
    }

    func testSelectionAcrossRowsAndGapsKeepsBoundaries() throws {
        let document = try TerminalCopyDocument(rows: [[Cell(column: 0, width: 2, text: "🦀")],
            [Cell(column: 2, width: 1, text: "é")]], columns: 4)
        XCTAssertEqual(document.text, "🦀\n  é")
        XCTAssertEqual(try document.selectedText(in: NSRange(location: 1, length: 3)), "🦀\n ")
        XCTAssertEqual(try document.selectedText(in: NSRange(location: 2, length: 1)), "\n")
        XCTAssertEqual(try document.selectedText(in: NSRange(location: 3, length: 2)), "  ")
        XCTAssertEqual(try document.selectedText(in: NSRange(location: 4, length: 2)), " é")
    }

    func testEverySelectionMatchesIndependentCanonicalUnitExpansion() throws {
        let cells: [Cell] = [Cell(column: 0, width: 1, text: "a"), Cell(column: 2, width: 2, text: "😀"),
            Cell(column: 4, width: 1, text: "e\u{301}"), Cell(column: 6, width: 2, text: "界")]
        let document = try TerminalCopyDocument(rows: [cells, [], cells], columns: 8)
        let expected = "a 😀e\u{301} 界\n\na 😀e\u{301} 界" as NSString
        // Explicit independently assembled source-cell spans, not values read
        // back from the production document's private map.
        let spans = [NSRange(location: 0, length: 1), NSRange(location: 2, length: 2),
                     NSRange(location: 4, length: 2), NSRange(location: 7, length: 1),
                     NSRange(location: 10, length: 1), NSRange(location: 12, length: 2),
                     NSRange(location: 14, length: 2), NSRange(location: 17, length: 1)]
        XCTAssertEqual(document.text, expected as String)
        for start in 0..<expected.length {
            for end in (start + 1)...expected.length {
                var low = start, high = end
                for span in spans where NSIntersectionRange(span, NSRange(location: start, length: end - start)).length > 0 {
                    low = min(low, span.location); high = max(high, span.location + span.length)
                }
                let value = try document.selectedText(in: NSRange(location: start, length: end - start))
                XCTAssertEqual(value, expected.substring(with: NSRange(location: low, length: high - low)))
            }
        }
    }

    func testMalformedRangesCannotOverflowOrTrap() throws {
        let document = try TerminalCopyDocument(rows: [[Cell(column: 0, width: 1, text: "x")]], columns: 1)
        for range in [NSRange(location: -1, length: 1), NSRange(location: 0, length: -1),
                      NSRange(location: Int.max, length: 1), NSRange(location: 1, length: Int.max),
                      NSRange(location: NSNotFound, length: Int.max), NSRange(location: 0, length: 2)] {
            XCTAssertThrowsError(try document.selectedText(in: range))
        }
        XCTAssertThrowsError(try document.selectedText(in: NSRange(location: 1, length: 0)))
    }

    func testGeometryAndControlInjectionAreRefused() {
        let invalidRows: [[Cell]] = [
            [Cell(column: -1, width: 1, text: "a")], [Cell(column: Int.max, width: 2, text: "a")],
            [Cell(column: 0, width: Int.max, text: "a")], [Cell(column: 0, width: 0, text: "a")],
            [Cell(column: 3, width: 2, text: "a")],
            [Cell(column: 1, width: 2, text: "a"), Cell(column: 2, width: 1, text: "b")]
        ]
        for rows in invalidRows { XCTAssertThrowsError(try TerminalCopyDocument(rows: [rows], columns: 4)) }
        for text in ["\u{1B}[31m", "\n", "\t", "\0", "\u{85}", "\u{7F}"] {
            XCTAssertThrowsError(try TerminalCopyDocument(rows: [[Cell(column: 0, width: 1, text: text)]], columns: 1))
        }
        for columns in [Int.min, 0, 513, Int.max] { XCTAssertThrowsError(try TerminalCopyDocument(rows: [[]], columns: columns)) }
        XCTAssertThrowsError(try TerminalCopyDocument(rows: [], columns: 1))
        XCTAssertThrowsError(try TerminalCopyDocument(rows: Array(repeating: [], count: 513), columns: 1))
        XCTAssertThrowsError(try TerminalCopyDocument(rows: Array(repeating: [], count: 129), columns: 512))
    }

    func testByteLimitsCountNewlinesGapsAndMaskingBeforeGrowth() throws {
        let cell = Cell(column: 2, width: 1, text: "界")
        XCTAssertEqual(try TerminalCopyDocument(rows: [[cell], []], columns: 3, maximumUTF8Bytes: 6).text, "  界\n")
        XCTAssertThrowsError(try TerminalCopyDocument(rows: [[cell], []], columns: 3, maximumUTF8Bytes: 5))
        XCTAssertThrowsError(try TerminalCopyDocument(rows: [[cell]], columns: 3, maximumUTF8Bytes: 4))
        XCTAssertThrowsError(try TerminalCopyDocument(rows: [[Cell(column: 0, width: 1,
            text: String(repeating: "x", count: 4097))]], columns: 1))
        for limit in [0, 131_073, Int.max] { XCTAssertThrowsError(try TerminalCopyDocument(rows: [[]], columns: 1, maximumUTF8Bytes: limit)) }
        let masked = try TerminalCopyDocument(rows: [[Cell(column: 0, width: 2, text: String(repeating: "x", count: 4096), concealed: true)]], columns: 2, maximumUTF8Bytes: 2)
        XCTAssertEqual(masked.text, "  ")
    }

    func testMaximumGridAndImmutableSnapshot() throws {
        var rows = Array(repeating: (0..<512).map { Cell(column: $0, width: 1, text: "x") }, count: 128)
        let document = try TerminalCopyDocument(rows: rows, columns: 512)
        rows[0] = []
        XCTAssertEqual(document.utf8Bytes, 65_536 + 127)
        XCTAssertEqual(try document.selectedText(in: NSRange(location: 0, length: 512)), String(repeating: "x", count: 512))
        XCTAssertEqual(document.text.last, "x")
    }
}

@MainActor
final class TerminalClipboardReviewTests: XCTestCase {
    func testConfirmationIsExactDestinationAndSingleUse() async throws {
        let owner = TerminalClipboardReview<String>(), id = UUID()
        try owner.offer(id: id, destination: "machine-a:keyboard-1", text: "echo hi\n")
        var actual: [String] = []
        XCTAssertThrowsError(try owner.confirm(id: id, destination: "machine-b:keyboard-1") { actual.append($0.text) })
        XCTAssertThrowsError(try owner.confirm(id: UUID(), destination: "machine-a:keyboard-1") { actual.append($0.text) })
        XCTAssertNotNil(owner.candidate)
        try owner.confirm(id: id, destination: "machine-a:keyboard-1") { actual.append($0.text) }
        XCTAssertNil(owner.candidate)
        XCTAssertThrowsError(try owner.confirm(id: id, destination: "machine-a:keyboard-1") { actual.append($0.text) })
        XCTAssertEqual(actual, ["echo hi\n"])
    }
    func testStaleCancelCannotDiscardNewerOffer() async throws {
        let owner = TerminalClipboardReview<String>(), old = UUID(), current = UUID()
        try owner.offer(id: old, destination: "a", text: "old")
        owner.cancel(id: old)
        try owner.offer(id: current, destination: "a", text: "new")
        owner.cancel(id: old)
        XCTAssertEqual(owner.candidate?.id, current)
        owner.retire()
        XCTAssertNil(owner.candidate)
    }
    func testRefusedAdmissionDoesNotReplayAndReentryCannotConfirmTwice() async throws {
        enum Rejected: Error { case blocked }
        let owner = TerminalClipboardReview<String>(), id = UUID()
        try owner.offer(id: id, destination: "a", text: "command")
        XCTAssertThrowsError(try owner.confirm(id: id, destination: "a") { _ in throw Rejected.blocked })
        XCTAssertNil(owner.candidate)
        let next = UUID()
        try owner.offer(id: next, destination: "a", text: "command2")
        try owner.confirm(id: next, destination: "a") { _ in
            XCTAssertThrowsError(try owner.confirm(id: next, destination: "a") { _ in XCTFail("duplicate") })
            try owner.offer(id: UUID(), destination: "b", text: "other")
        }
        XCTAssertEqual(owner.candidate?.text, "other")
    }
    func testHostileAndOversizedOffersAreRefusedWithoutReplacingCurrent() async throws {
        let owner = TerminalClipboardReview<String>()
        for text in ["", "\u{1B}[201~danger", "\0", "\u{85}", String(repeating: "a", count: 32_769)] {
            XCTAssertThrowsError(try owner.offer(id: UUID(), destination: "a", text: text))
            XCTAssertNil(owner.candidate)
        }
        try owner.offer(id: UUID(), destination: "a", text: "界\t\n")
        XCTAssertThrowsError(try owner.offer(id: UUID(), destination: "b", text: "other"))
        XCTAssertEqual(owner.candidate?.text, "界\t\n")
        XCTAssertEqual(owner.debugDescription, "TerminalClipboardReview([redacted])")
    }
}
