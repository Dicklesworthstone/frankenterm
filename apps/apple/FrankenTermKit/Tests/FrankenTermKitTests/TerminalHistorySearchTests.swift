import Foundation
import XCTest
@testable import FrankenTermKit

private actor SearchPages {
    let source: UUID, rows: [TerminalSearchRow], height: Int, base: Int64
    var requests: [Int64?] = []
    var failAfterFirst = false, changeGeometry = false, changeSource = false, rewindSequence = false
    init(source: UUID, rows: [TerminalSearchRow], height: Int, base: Int64 = 0) {
        self.source = source; self.rows = rows; self.height = height; self.base = base
    }
    func change(_ kind: String) {
        failAfterFirst = kind == "unavailable"; changeGeometry = kind == "geometry"
        changeSource = kind == "source"; rewindSequence = kind == "sequence"
    }
    func read(_ top: Int64?) throws -> TerminalSearchPage<UUID> {
        requests.append(top)
        if requests.count > 1, failAfterFirst { throw TerminalHistorySearchError.unavailable }
        let live = base + Int64(rows.count - height), start = top ?? live
        let offset = Int(start - base)
        return try TerminalSearchPage(source: requests.count > 1 && changeSource ? UUID() : source,
            geometryRevision: requests.count > 1 && changeGeometry ? 2 : 1,
            sequence: requests.count > 1 && rewindSequence ? 8 : 10, alternateScreen: false,
            columns: 512, first: base, live: live, top: start,
            rows: Array(rows[offset..<(offset + height)]))
    }
}

@MainActor
final class TerminalHistorySearchTests: XCTestCase {
    private typealias Cell = TerminalSearchCell
    private func row(_ text: String, sequence: UInt64 = 1) throws -> TerminalSearchRow {
        try TerminalSearchRow(sequence: sequence, cells: text.enumerated().map {
            try Cell(column: $0.offset, width: 1, text: String($0.element))
        })
    }
    private func page(_ cells: [Cell], source: UUID, sequence: UInt64 = 1,
                      rowSequence: UInt64 = 1, geometry: UInt64 = 1, alternate: Bool = false) throws -> TerminalSearchPage<UUID> {
        try .init(source: source, geometryRevision: geometry, sequence: sequence, alternateScreen: alternate,
                  columns: 20, first: 0, live: 0, top: 0, rows: [.init(sequence: rowSequence, cells: cells)])
    }
    private func run(_ text: [String], query: String, height: Int = 2,
                     limits: TerminalHistorySearchLimits = .init(), base: Int64 = 0) async throws
        -> (TerminalHistorySearchResult<UUID>, SearchPages) {
        let source = UUID(), rows = try text.map { try row($0) }
        let pages = SearchPages(source: source, rows: rows, height: height, base: base)
        return (try await TerminalHistorySearch.search(query: query, source: source, limits: limits) { try await pages.read($0) }, pages)
    }

    func testSearchesAllResidentPagesNewestFirstWithoutTailDuplicates() async throws {
        let (result, pages) = try await run((0..<7).map { "error \($0)" }, query: "error", height: 3)
        XCTAssertEqual(result.matches.map(\.row), [6,5,4,3,2,1,0])
        XCTAssertEqual(result.scannedRows, 7); XCTAssertEqual(result.pages, 3)
        XCTAssertEqual(result.completion, .complete)
        let requests = await pages.requests
        XCTAssertEqual(requests, [nil, 1, 0])
        XCTAssertEqual(result.firstRow, 0); XCTAssertEqual(result.lastRow, 6)
    }
    func testOverlappingMatchesAreAllFoundAndOrderedWithinEachRow() async throws {
        let (result, _) = try await run(["aaaaa"], query: "aaa", height: 1)
        XCTAssertEqual(result.matches.map(\.columns), [0..<3,1..<4,2..<5])
        XCTAssertEqual(result.scannedBytes, 5)
    }
    func testRegexMetacharactersAreLiteralAndCaseIsSignificant() async throws {
        let (result, _) = try await run([".* ERROR error .*"] , query: ".*", height: 1)
        XCTAssertEqual(result.matches.map(\.columns), [0..<2,15..<17])
        let (lower, _) = try await run(["ERROR error"], query: "error", height: 1)
        XCTAssertEqual(lower.matches.map(\.columns), [6..<11])
    }
    func testUnicodeMatchesUseCanonicalCellOccupancyNotStringWidth() async throws {
        let source = UUID()
        let cells = try [Cell(column: 0, width: 2, text: "界"), Cell(column: 2, width: 1, text: "e\u{301}"),
                         Cell(column: 3, width: 2, text: "👩🏽‍💻"), Cell(column: 6, width: 1, text: "!")]
        let snapshot = try page(cells, source: source)
        for (query, range) in [("界e\u{301}",0..<3), ("\u{301}",2..<3), ("🏽",3..<5), (" !",5..<7)] {
            let result = try await TerminalHistorySearch.search(query: query, source: source) { _ in snapshot }
            XCTAssertEqual(result.matches.map(\.columns), [range], query)
            try TerminalHistorySearch.validate(XCTUnwrap(result.matches.first), in: snapshot)
        }
    }
    func testConcealedTextIsDiscardedAndCannotBridgeAMatch() async throws {
        let source = UUID()
        let secret = try Cell(column: 1, width: 2, text: "password", concealed: true)
        XCTAssertEqual(secret.text, "")
        let snapshot = try page([Cell(column: 0, width: 1, text: "a"), secret,
                                 Cell(column: 3, width: 1, text: "b")], source: source)
        for query in ["password", "a  b", "  "] {
            let result = try await TerminalHistorySearch.search(query: query, source: source) { _ in snapshot }
            XCTAssertTrue(result.matches.isEmpty)
        }
        let result = try await TerminalHistorySearch.search(query: "a", source: source) { _ in snapshot }
        XCTAssertEqual(result.matches.first?.excerpt, "a  b")
    }
    func testDoesNotJoinPhysicalRowsOrInventUnrepresentedMargin() async throws {
        let (result, _) = try await run(["hello", "world"], query: "helloworld")
        XCTAssertTrue(result.matches.isEmpty)
        let (spaces, _) = try await run(["x"], query: "x ", height: 1)
        XCTAssertTrue(spaces.matches.isEmpty)
    }
    func testExactlyAtMatchCapIsCompleteButExtraMatchReportsTruncation() async throws {
        var limits = TerminalHistorySearchLimits(); limits.maximumMatches = 2
        let (exact, _) = try await run(["xx"], query: "x", height: 1, limits: limits)
        XCTAssertEqual(exact.completion, .complete); XCTAssertEqual(exact.matches.count, 2)
        let (extra, _) = try await run(["xxx"], query: "x", height: 1, limits: limits)
        XCTAssertEqual(extra.completion, .limitReached); XCTAssertEqual(extra.matches.count, 2)
    }
    func testRowByteAndPageBudgetsReportIncompleteSearch() async throws {
        var limits = TerminalHistorySearchLimits(); limits.maximumRows = 2
        let (rows, _) = try await run(["x", "x", "x"], query: "x", height: 1, limits: limits)
        XCTAssertEqual(rows.completion, .limitReached); XCTAssertEqual(rows.scannedRows, 2)
        limits = .init(); limits.maximumBytes = 2
        let (bytes, _) = try await run(["xx", "xx"], query: "x", height: 1, limits: limits)
        XCTAssertEqual(bytes.completion, .limitReached); XCTAssertEqual(bytes.scannedBytes, 2)
        limits = .init(); limits.maximumPages = 1
        let (pages, provider) = try await run(["x", "x"], query: "x", height: 1, limits: limits)
        XCTAssertEqual(pages.completion, .limitReached); XCTAssertEqual(pages.pages, 1)
        let calls = await provider.requests; XCTAssertEqual(calls.count, 1)
    }
    func testChangedHistoryAndGeometryPreserveOnlyExplicitlyPartialResults() async throws {
        for kind in ["unavailable", "geometry", "sequence"] {
            let source = UUID(), pages = SearchPages(source: source, rows: try [row("x"),row("x")], height: 1)
            await pages.change(kind)
            let result = try await TerminalHistorySearch.search(query: "x", source: source) { try await pages.read($0) }
            XCTAssertEqual(result.completion, .historyChanged, kind)
            XCTAssertEqual(result.matches.map(\.row), [1], kind)
        }
    }
    func testForeignSourceIsNotAcceptedAsPartialSuccess() async throws {
        let source = UUID(), pages = SearchPages(source: source, rows: try [row("x"),row("x")], height: 1)
        await pages.change("source")
        do {
            _ = try await TerminalHistorySearch.search(query: "x", source: source) { try await pages.read($0) }
            XCTFail("foreign page accepted")
        } catch { XCTAssertEqual(error as? TerminalHistorySearchError, .invalidPage) }
    }
    func testFrozenStartRangeExcludesNewRowsAndSupportsNegativeStableRows() async throws {
        let (result, _) = try await run(["x","x","x"], query: "x", height: 2, base: -10)
        XCTAssertEqual(result.matches.map(\.row), [-8,-9,-10])
        XCTAssertEqual(result.firstRow, -10); XCTAssertEqual(result.lastRow, -8)
    }
    func testStableRowArithmeticNearBothInt64Boundaries() async throws {
        for base in [Int64.min + 1, Int64.max - 2] {
            let (result, _) = try await run(["x","x","x"], query: "x", height: 2, base: base)
            XCTAssertEqual(result.matches.map(\.row), [base + 2, base + 1, base])
            XCTAssertEqual(result.completion, .complete)
        }
    }
    func testValidationRejectsChangedBytesEvenWithReusedSequence() async throws {
        let source = UUID(), original = try page([Cell(column: 0, width: 1, text: "x")], source: source)
        let result = try await TerminalHistorySearch.search(query: "x", source: source) { _ in original }
        let match = try XCTUnwrap(result.matches.first)
        try TerminalHistorySearch.validate(match, in: original)
        for changed in [
            try page([Cell(column: 0, width: 1, text: "y")], source: source),
            try page([Cell(column: 0, width: 1, text: "x")], source: UUID()),
            try page([Cell(column: 0, width: 1, text: "x")], source: source, geometry: 2),
            try page([Cell(column: 0, width: 1, text: "x")], source: source, alternate: true),
            try page([Cell(column: 0, width: 1, text: "x")], source: source, sequence: 2, rowSequence: 2),
            try page([Cell(column: 0, width: 1, text: "x", concealed: true)], source: source)
        ] {
            XCTAssertThrowsError(try TerminalHistorySearch.validate(match, in: changed)) {
                XCTAssertEqual($0 as? TerminalHistorySearchError, .staleResult)
            }
        }
    }
    func testValidationAllowsUnrelatedOutputWithoutPretendingAnAtomicSnapshot() async throws {
        let source = UUID(), original = try page([Cell(column: 0, width: 1, text: "x")], source: source)
        let result = try await TerminalHistorySearch.search(query: "x", source: source) { _ in original }
        let later = try page([Cell(column: 0, width: 1, text: "x")], source: source, sequence: 99)
        try TerminalHistorySearch.validate(XCTUnwrap(result.matches.first), in: later)
    }
    func testLongGraphemeExcerptRemainsBoundedWithoutSplittingTheCell() async throws {
        let source = UUID(), long = "x" + String(repeating: "\u{301}", count: 1000)
        let snapshot = try page([Cell(column: 0, width: 1, text: long)], source: source)
        let result = try await TerminalHistorySearch.search(query: "x", source: source) { _ in snapshot }
        XCTAssertEqual(result.matches.first?.excerpt, "[Match in a long terminal cell]")
        XCTAssertEqual(result.matches.first?.columns, 0..<1)
    }
    func testInvalidQueriesAreRejectedBeforeCallingTheProvider() async throws {
        for query in ["", "x\ny", "\u{1b}", "\u{85}", String(repeating: "é", count: 513)] {
            do {
                _ = try await TerminalHistorySearch.search(query: query, source: UUID()) { _ in
                    throw TerminalHistorySearchError.invalidPage
                }
                XCTFail("invalid query accepted")
            } catch { XCTAssertEqual(error as? TerminalHistorySearchError, .invalidQuery) }
        }
    }
    func testPageRejectsOverflowOverlapAndOutOfOrderCellGeometry() throws {
        let source = UUID(), valid = try row("x")
        XCTAssertThrowsError(try TerminalSearchPage(source: source, geometryRevision: 1, sequence: 1,
            alternateScreen: false, columns: 20, first: 0, live: Int64.max, top: 0, rows: [valid, valid]))
        XCTAssertThrowsError(try page([Cell(column: 2, width: 2, text: "界"), Cell(column: 3, width: 1, text: "x")], source: source))
        XCTAssertThrowsError(try page([Cell(column: 19, width: 2, text: "界")], source: source))
        XCTAssertThrowsError(try Cell(column: 0, width: 1, text: "\u{1b}"))
    }
    func testPreCancelledSearchDoesNotCallProvider() async throws {
        let source = UUID(), pages = SearchPages(source: source, rows: try [row("x")], height: 1)
        let job = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await TerminalHistorySearch.search(query: "x", source: source) { try await pages.read($0) }
        }
        do { _ = try await job.value; XCTFail("cancelled search succeeded") }
        catch { XCTAssertTrue(error is CancellationError) }
        let calls = await pages.requests; XCTAssertTrue(calls.isEmpty)
    }
    func testCancellationAfterUncooperativeProviderReturnsDiscardsItsResult() async throws {
        let gate = SearchGate(), source = UUID()
        let snapshot = try page([Cell(column: 0, width: 1, text: "x")], source: source)
        let job = Task {
            try await TerminalHistorySearch.search(query: "x", source: source) { _ in
                await gate.park(); return snapshot
            }
        }
        await gate.entered(); job.cancel(); await gate.release()
        do { _ = try await job.value; XCTFail("late result published") }
        catch { XCTAssertTrue(error is CancellationError) }
    }
    func testDifferentialLiteralSearchAgainstIndependentNaiveOracle() async throws {
        var seed: UInt64 = 0x52a3_991d
        func next() -> UInt64 { seed = seed &* 6364136223846793005 &+ 1; return seed }
        for _ in 0..<200 {
            let characters = (0..<100).map { _ in UInt8(97 + next() % 4) }
            let queryBytes = (0..<(1 + Int(next() % 9))).map { _ in UInt8(97 + next() % 4) }
            let text = String(decoding: characters, as: UTF8.self), query = String(decoding: queryBytes, as: UTF8.self)
            let expected = (0...(characters.count - queryBytes.count)).filter {
                Array(characters[$0..<($0 + queryBytes.count)]) == queryBytes
            }.map { $0..<($0 + queryBytes.count) }
            let (result, _) = try await run([text], query: query, height: 1)
            XCTAssertEqual(result.matches.map(\.columns), expected)
            XCTAssertEqual(result.completion, .complete)
        }
    }
}

private actor SearchGate {
    private var parked: CheckedContinuation<Void, Never>?
    private var observers: [CheckedContinuation<Void, Never>] = []
    func park() async {
        await withCheckedContinuation { continuation in
            parked = continuation
            let saved = observers; observers.removeAll(); saved.forEach { $0.resume() }
        }
    }
    func entered() async {
        if parked != nil { return }
        await withCheckedContinuation { observers.append($0) }
    }
    func release() { let saved = parked; parked = nil; saved?.resume() }
}
