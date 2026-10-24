import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class TerminalDiagnosticExcerptTests: XCTestCase {
    private func context(_ label: String = "Build host\nMachine build · Route ssh · Pane 17") throws -> TerminalDiagnosticExcerpt.Context {
        try .init(destinationLabel: label, receivedAt: Date(timeIntervalSince1970: 0))
    }
    private func document(_ rows: [String]) throws -> TerminalCopyDocument {
        try TerminalCopyDocument(rows: rows.map { row in
            row.enumerated().map { index, character in
                .init(column: index, width: 1, text: String(character))
            }
        }, columns: 128)
    }
    private func excerpt(_ text: String = "token=private") throws -> TerminalDiagnosticExcerpt {
        let document = try document([text])
        return try TerminalDiagnosticExcerpt(document: document,
            selection: NSRange(location: 0, length: document.utf16Count), context: context())
    }

    func testOnlyExplicitSelectionIsRetainedAndCanonicalCellsStayWhole() async throws {
        let document = try TerminalCopyDocument(rows: [[
            .init(column: 0, width: 1, text: "X"), .init(column: 1, width: 2, text: "👩🏽‍💻"),
            .init(column: 3, width: 1, text: "e\u{301}"), .init(column: 4, width: 1, text: "secret", concealed: true),
            .init(column: 5, width: 1, text: "Y")
        ]], columns: 8)
        let selected = try TerminalDiagnosticExcerpt(document: document, selection: NSRange(location: 2, length: 1), context: context())
        XCTAssertEqual(selected.text, "👩🏽‍💻")
        let review = try selected.prepareReview()
        XCTAssertEqual(review.text, "👩🏽‍💻")
        XCTAssertFalse(review.text.contains("X"))
        XCTAssertFalse(review.text.contains("Y"))
        XCTAssertFalse(review.text.contains("secret"))
        XCTAssertFalse(String(reflecting: selected).contains("👩"))
        XCTAssertFalse(String(reflecting: review).contains("👩"))

        let whole = try TerminalDiagnosticExcerpt(document: document,
            selection: NSRange(location: 0, length: document.utf16Count), context: context())
        XCTAssertEqual(whole.text, "X👩🏽‍💻e\u{301} Y", "concealed text stays masked even in an explicitly selected whole viewport")
    }

    func testEmptyInvalidOrOverflowingSelectionNeverFallsBackToWholeDocument() async throws {
        let document = try document(["unselected private output"])
        for range in [NSRange(location: 0, length: 0), NSRange(location: document.utf16Count, length: 0),
                      NSRange(location: -1, length: 1), NSRange(location: 0, length: -1),
                      NSRange(location: 1, length: Int.max), NSRange(location: Int.max, length: 1)] {
            XCTAssertThrowsError(try TerminalDiagnosticExcerpt(document: document, selection: range, context: context()))
        }
    }

    func testWhitespaceAndHardLineBreaksAreSharedExactlyWithoutAddedContextOrReturn() async throws {
        let document = try document(["  ERROR a=1", " next  "])
        let excerpt = try TerminalDiagnosticExcerpt(document: document,
            selection: NSRange(location: 2, length: document.utf16Count - 2), context: context())
        XCTAssertFalse(excerpt.includesContext)
        let review = try excerpt.prepareReview()
        var offered: [String] = []
        XCTAssertTrue(offered.isEmpty)
        try excerpt.share(reviewID: review.id) { offered.append($0) }
        XCTAssertEqual(offered, ["ERROR a=1\n next  "])
        XCTAssertEqual(excerpt.phase, .retired)
        XCTAssertEqual(excerpt.text, "")
    }

    func testContextIsOptionalAndUsesOriginalSourceAndViewportReceiptTime() async throws {
        let excerpt = try excerpt("failure")
        let first = try excerpt.prepareReview()
        XCTAssertEqual(first.text, "failure")
        try excerpt.resumeEditing(reviewID: first.id)
        try excerpt.setIncludesContext(true)
        let second = try excerpt.prepareReview()
        XCTAssertEqual(second.text,
            "FrankenTerm diagnostic excerpt\nBuild host\nMachine build · Route ssh · Pane 17\nViewport received: 1970-01-01T00:00:00Z\n\nfailure")
        XCTAssertEqual(excerpt.context.receivedAt, Date(timeIntervalSince1970: 0))
        XCTAssertThrowsError(try excerpt.setIncludesContext(false), "an already displayed preview is immutable")
        var shared = ""
        try excerpt.share(reviewID: second.id) { shared = $0 }
        XCTAssertEqual(shared, second.text)
    }

    func testRedactionRemovesChosenTextFromPreviewAndShare() async throws {
        let excerpt = try excerpt()
        let replacement = try excerpt.redact(NSRange(location: 6, length: 7))
        XCTAssertEqual(replacement, NSRange(location: 6, length: "[redacted]".utf16.count))
        XCTAssertEqual(excerpt.text, "token=[redacted]")
        let review = try excerpt.prepareReview()
        XCTAssertEqual(review.text, "token=[redacted]")
        var shared = ""
        try excerpt.share(reviewID: review.id) { shared = $0 }
        XCTAssertEqual(shared, review.text)
        XCTAssertFalse(shared.contains("private"))
    }

    func testRedactionExpandsPartialEmojiAndCombiningSelectionsToWholeGraphemes() async throws {
        let excerpt = try excerpt("A👩🏽‍💻e\u{301}Z")
        _ = try excerpt.redact(NSRange(location: 2, length: 1))
        XCTAssertEqual(excerpt.text, "A[redacted]e\u{301}Z")
        _ = try excerpt.redact(NSRange(location: 12, length: 1))
        XCTAssertEqual(excerpt.text, "A[redacted][redacted]Z")
        XCTAssertFalse(excerpt.text.contains("\u{FFFD}"))
    }

    func testBadRedactionRangesPreserveTheAcceptedExcerpt() async throws {
        let excerpt = try excerpt()
        for range in [NSRange(location: 0, length: 0), NSRange(location: -1, length: 1),
                      NSRange(location: 1, length: -1), NSRange(location: 13, length: 1),
                      NSRange(location: Int.max, length: 1), NSRange(location: 1, length: Int.max)] {
            XCTAssertThrowsError(try excerpt.redact(range)) { error in
                XCTAssertEqual(error as? TerminalDiagnosticExcerptError, .invalidRange)
            }
            XCTAssertEqual(excerpt.text, "token=private")
        }
    }

    func testUTF16NativeEditingAllowsScalarIMEChangesButRejectsHalfSurrogatesAndOverflow() async throws {
        let excerpt = try excerpt("A🦀e\u{301}Z")
        let proposed = try excerpt.proposedText(replacing: NSRange(location: 4, length: 1), with: "\u{308}")
        XCTAssertEqual(proposed, "A🦀e\u{308}Z")
        XCTAssertEqual(excerpt.text, "A🦀e\u{301}Z", "a proposed edit has not yet been committed by UIKit")
        try excerpt.update(proposed)
        for range in [NSRange(location: 2, length: 1), NSRange(location: 1, length: 1),
                      NSRange(location: -1, length: 0), NSRange(location: 0, length: -1),
                      NSRange(location: Int.max, length: 0), NSRange(location: 1, length: Int.max)] {
            XCTAssertThrowsError(try excerpt.proposedText(replacing: range, with: "x")) { error in
                XCTAssertEqual(error as? TerminalDiagnosticExcerptError, .invalidRange)
            }
        }
        XCTAssertEqual(try excerpt.proposedText(replacing: NSRange(location: proposed.utf16.count, length: 0), with: "!"), proposed + "!")
    }

    func testUTF8CapacityIsCheckedBeforeEditingOrRedactionWithoutPartialTruncation() async throws {
        let excerpt = try excerpt()
        let almost = String(repeating: "界", count: 43_690)
        try excerpt.update(almost)
        XCTAssertEqual(excerpt.utf8Bytes, 131_070)
        XCTAssertThrowsError(try excerpt.proposedText(replacing: NSRange(location: almost.utf16.count, length: 0), with: "界"))
        XCTAssertEqual(excerpt.text, almost)
        let full = try excerpt.proposedText(replacing: NSRange(location: almost.utf16.count, length: 0), with: "ab")
        try excerpt.update(full)
        XCTAssertEqual(excerpt.utf8Bytes, TerminalDiagnosticExcerpt.maximumUTF8Bytes)
        XCTAssertThrowsError(try excerpt.update(full + "x"))
        XCTAssertThrowsError(try excerpt.redact(NSRange(location: 0, length: 1)), "a longer redaction marker must also fit")
        XCTAssertEqual(excerpt.text, full)
        let removed = try excerpt.proposedText(replacing: NSRange(location: 0, length: full.utf16.count), with: "")
        XCTAssertEqual(removed, "")
        try excerpt.update(removed)
        XCTAssertThrowsError(try excerpt.prepareReview()) { error in
            XCTAssertEqual(error as? TerminalDiagnosticExcerptError, .empty)
        }
    }

    func testTextControlsAreRejectedWhileOrdinaryTabsAndLineBreaksRemainEditable() async throws {
        let excerpt = try excerpt("kept")
        for value in ["\0", "\u{1B}[31m", "\u{7F}", "\u{85}", "\u{9B}31m"] {
            XCTAssertThrowsError(try excerpt.update(value))
            XCTAssertThrowsError(try excerpt.proposedText(replacing: NSRange(location: 0, length: 4), with: value))
            XCTAssertEqual(excerpt.text, "kept")
        }
        try excerpt.update("\tline one\r\nline two\n")
        XCTAssertEqual(try excerpt.prepareReview().text, "\tline one\r\nline two\n")
    }

    func testEditingAgainInvalidatesTheOldApprovalAndRequiresANewExactPreview() async throws {
        let excerpt = try excerpt()
        let first = try excerpt.prepareReview()
        XCTAssertThrowsError(try excerpt.update("late"))
        XCTAssertThrowsError(try excerpt.redact(NSRange(location: 0, length: 1)))
        XCTAssertThrowsError(try excerpt.prepareReview())
        XCTAssertThrowsError(try excerpt.resumeEditing(reviewID: UUID()))
        try excerpt.resumeEditing(reviewID: first.id)
        _ = try excerpt.redact(NSRange(location: 6, length: 7))
        let second = try excerpt.prepareReview()
        XCTAssertNotEqual(first.id, second.id)
        var shared: [String] = []
        XCTAssertThrowsError(try excerpt.share(reviewID: first.id) { shared.append($0) })
        XCTAssertTrue(shared.isEmpty)
        try excerpt.share(reviewID: second.id) { shared.append($0) }
        XCTAssertEqual(shared, ["token=[redacted]"])
        XCTAssertThrowsError(try excerpt.share(reviewID: second.id) { shared.append($0) })
        XCTAssertEqual(shared.count, 1)
    }

    func testFailedAndReentrantShareConsumesApprovalWithoutReplay() async throws {
        let excerpt = try excerpt("reviewed")
        let review = try excerpt.prepareReview()
        var offers = 0
        XCTAssertThrowsError(try excerpt.share(reviewID: review.id) { value in
            offers += 1
            XCTAssertEqual(value, "reviewed")
            XCTAssertEqual(excerpt.phase, .retired)
            XCTAssertEqual(excerpt.text, "")
            XCTAssertThrowsError(try excerpt.share(reviewID: review.id) { _ in offers += 1 })
            throw TerminalDiagnosticExcerptError.unavailable
        })
        XCTAssertEqual(offers, 1)
        XCTAssertThrowsError(try excerpt.share(reviewID: review.id) { _ in offers += 1 })
        XCTAssertEqual(offers, 1)
    }

    func testRetirementClearsEditableAndReviewedTextAndRejectsLateCallbacks() async throws {
        for reviewing in [false, true] {
            let excerpt = try excerpt()
            let reviewID = reviewing ? try excerpt.prepareReview().id : UUID()
            excerpt.retire()
            XCTAssertEqual(excerpt.text, "")
            XCTAssertEqual(excerpt.phase, .retired)
            XCTAssertThrowsError(try excerpt.update("late callback"))
            XCTAssertThrowsError(try excerpt.proposedText(replacing: NSRange(location: 0, length: 0), with: "late"))
            XCTAssertThrowsError(try excerpt.setIncludesContext(true))
            XCTAssertThrowsError(try excerpt.redact(NSRange(location: 0, length: 1)))
            XCTAssertThrowsError(try excerpt.prepareReview())
            XCTAssertThrowsError(try excerpt.resumeEditing(reviewID: reviewID))
            XCTAssertThrowsError(try excerpt.share(reviewID: reviewID) { _ in XCTFail("retired excerpt was offered") })
        }
    }

    func testContextAndCombinedOutputHaveIndependentFiniteBounds() async throws {
        for label in ["", String(repeating: "x", count: 1025), "bad\0label", "bad\u{1B}label", "bad\tlabel", "bad\u{7F}label"] {
            XCTAssertThrowsError(try context(label)) { error in
                XCTAssertEqual(error as? TerminalDiagnosticExcerptError, .invalidContext)
            }
        }
        for date in [Date(timeIntervalSince1970: .nan), Date(timeIntervalSince1970: .infinity),
                     Date.distantPast.addingTimeInterval(-1), Date.distantFuture.addingTimeInterval(1)] {
            XCTAssertThrowsError(try TerminalDiagnosticExcerpt.Context(destinationLabel: "machine", receivedAt: date))
        }
        let document = try document(["x"])
        let excerpt = try TerminalDiagnosticExcerpt(document: document, selection: NSRange(location: 0, length: 1),
            context: context(String(repeating: "界", count: 341)))
        try excerpt.update(String(repeating: "x", count: TerminalDiagnosticExcerpt.maximumUTF8Bytes))
        try excerpt.setIncludesContext(true)
        let review = try excerpt.prepareReview()
        XCTAssertLessThanOrEqual(review.text.utf8.count, TerminalDiagnosticExcerpt.maximumSharedUTF8Bytes)
        XCTAssertTrue(review.text.hasSuffix(excerpt.text))
    }
}
