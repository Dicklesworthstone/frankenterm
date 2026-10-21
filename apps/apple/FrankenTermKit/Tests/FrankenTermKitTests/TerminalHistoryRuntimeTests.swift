import Foundation
import XCTest
@testable import FrankenTermKit

private actor HistoryRuntimeGate {
    private var open = false
    private var waiters: [CheckedContinuation<Void, Never>] = []
    func wait() async {
        if open { return }
        await withCheckedContinuation { waiters.append($0) }
    }
    func release() {
        open = true
        let pending = waiters; waiters.removeAll()
        for waiter in pending { waiter.resume() }
    }
}

private actor HistoryRuntimeTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    private var written = Data()
    init() throws {
        identity = try .init(workspaceID: "history-workspace", machineID: "machine", routeID: "direct",
            routeRevision: 1, transportID: UUID(), channelID: UUID())
    }
    func writeSome(_ bytes: Data) -> TerminalTransportWrite { written += bytes; return .accepted(bytes.count) }
    func close() {}
    func bytes() -> Data { written }
}

private actor HistoryRuntimeDelivery: TerminalModelDelivery {
    let entered = HistoryRuntimeGate()
    let hold: HistoryRuntimeGate?
    private var frames: [TerminalRenderFrame] = []
    init(hold: HistoryRuntimeGate? = nil) { self.hold = hold }
    func present(_ presentation: TerminalModelPresentation, source: TerminalSource) async throws {
        if case let .frame(bytes) = presentation {
            frames.append(try TerminalRenderFrame.decode(bytes, expectedSource: source))
        }
        await entered.release()
        await hold?.wait()
    }
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) {}
    func presented() -> [TerminalRenderFrame] { frames }
}

/// Independent semantic model fixture. It writes documented FTSF fields,
/// including stable row/geometry witnesses, and never invokes a production
/// encoder. Appending a line is a test mutation, not an ANSI parser claim.
private final class HistoryRuntimeModel: TerminalSessionModel, @unchecked Sendable {
    struct Cell: Sendable {
        let column: Int, width: Int, text: String
        var concealed = false
    }
    struct Row: Sendable {
        var sequence: UInt64 = 1
        var cells: [Cell]
        init(_ text: String) {
            cells = text.enumerated().map { Cell(column: $0.offset, width: 1, text: String($0.element)) }
        }
        init(cells: [Cell]) { self.cells = cells }
    }
    let source: TerminalSource
    private let lock = NSLock()
    private var rows: [Row]
    private let height: Int, columns: Int
    private var first: Int64 = 100
    private var sequence: UInt64 = 1, geometry: UInt64 = 1
    private var alternate = false, synchronized = false, reportsHistory = true, closed = false
    private var wrongTop = false
    private var captures: [Int64?] = [], feeds = 0

    init(source: TerminalSource, rows: [Row], height: Int = 2, columns: Int = 32) {
        self.source = source; self.rows = rows; self.height = height; self.columns = columns
    }
    func change(geometry: UInt64? = nil, alternate: Bool? = nil, synchronized: Bool? = nil,
                reportsHistory: Bool? = nil, first: Int64? = nil, wrongTop: Bool? = nil) {
        lock.lock(); defer { lock.unlock() }
        if let geometry { self.geometry = geometry }
        if let alternate { self.alternate = alternate }
        if let synchronized { self.synchronized = synchronized }
        if let reportsHistory { self.reportsHistory = reportsHistory }
        if let first { self.first = first }
        if let wrongTop { self.wrongTop = wrongTop }
    }
    func replace(_ index: Int, with row: Row, preserveSequence: Bool = false) {
        lock.lock(); defer { lock.unlock() }
        var row = row
        if preserveSequence { row.sequence = rows[index].sequence }
        else { sequence += 1; row.sequence = sequence }
        rows[index] = row
    }
    func activity() -> (captures: [Int64?], feeds: Int, closed: Bool) {
        lock.lock(); defer { lock.unlock() }
        return (captures, feeds, closed)
    }
    func feed(_ bytes: Data) {
        lock.lock(); defer { lock.unlock() }
        sequence += 1; feeds += 1
        var row = Row(String(decoding: bytes, as: UTF8.self)); row.sequence = sequence
        rows.append(row)
    }
    func resize(_ geometry: TerminalSessionGeometry) {
        lock.lock(); self.geometry += 1; lock.unlock()
    }
    func encode(_ intent: TerminalKeyboardIntent, maximumBytes: Int) -> Data { Data([0x55]) }
    func preparePaste(_ text: String, maximumBytes: Int) -> TerminalPreparedPaste { .init(text: text, bracketed: false) }
    func copyReply() -> TerminalModelReply? { nil }
    func settleReply(sequence: UInt64, disposition: TerminalModelReplyDisposition) {}
    func setFontRevision(_ revision: UInt64) {}
    func setBasePalette(_ rgba: [UInt8]) {}
    func close() { lock.lock(); closed = true; lock.unlock() }

    func captureFrame(viewportTop: Int64?) throws -> Data {
        lock.lock(); defer { lock.unlock() }
        guard !closed else { throw TerminalModelError.closed }
        captures.append(viewportTop)
        let live = first + Int64(rows.count - height)
        let top = wrongTop ? live : viewportTop ?? live
        guard top >= first, top <= live else { throw TerminalModelError.viewportUnavailable }
        let offset = Int(top - first), page = Array(rows[offset..<(offset + height)])
        let cells = page.flatMap(\.cells)
        let cellsOffset = 256 + height * 24, textOffset = cellsOffset + cells.count * 48
        let textBytes = cells.reduce(0) { $0 + $1.text.utf8.count }, paletteOffset = textOffset + textBytes
        var data = Data(repeating: 0, count: paletteOffset + 265 * 4)
        func put(_ value: UInt64, at: Int, bytes: Int = 4) {
            for index in 0..<bytes { data[at + index] = UInt8(truncatingIfNeeded: value >> (index * 8)) }
        }
        func signed(_ value: Int64, at: Int) { put(UInt64(bitPattern: value), at: at, bytes: 8) }
        func uuid(_ value: UUID, at: Int) {
            var value = value.uuid
            withUnsafeBytes(of: &value) { data.replaceSubrange(at..<(at + 16), with: $0) }
        }
        data.replaceSubrange(0..<4, with: "FTSF".utf8)
        put(1, at: 4, bytes: 2); put(256, at: 6, bytes: 2); put(UInt64(data.count), at: 8)
        put(1 | (reportsHistory ? 16 : 0) | (alternate ? 4 : 0) | (synchronized ? 8 : 0), at: 12)
        uuid(source.identity.channelID, at: 16); uuid(source.identity.transportID, at: 32)
        for (at, value) in [(48, source.identity.routeRevision), (56, sequence), (64, geometry),
                            (72, UInt64(1)), (80, 1), (88, UInt64(captures.count))] {
            put(value, at: at, bytes: 8)
        }
        for (at, value) in [(96, height), (100, columns), (128, height), (132, cells.count),
                            (136, textBytes), (140, 256), (144, cellsOffset), (148, textOffset),
                            (152, paletteOffset), (156, 265), (164, textOffset), (172, 1)] {
            put(UInt64(value), at: at)
        }
        signed(top, at: 104); signed(live, at: 112)
        if reportsHistory { signed(first, at: 176); signed(live, at: 184) }
        var firstCell = 0
        for (index, row) in page.enumerated() {
            let at = 256 + index * 24
            signed(top + Int64(index), at: at); put(row.sequence, at: at + 8, bytes: 8)
            put(UInt64(firstCell), at: at + 16); put(UInt64(row.cells.count), at: at + 20)
            firstCell += row.cells.count
        }
        var textPosition = 0
        for (index, cell) in cells.enumerated() {
            let at = cellsOffset + index * 48, length = cell.text.utf8.count
            put(UInt64(cell.column), at: at); put(UInt64(cell.width), at: at + 4)
            put(UInt64(textPosition), at: at + 8); put(UInt64(length), at: at + 12)
            put(cell.concealed ? 1 << 10 : 0, at: at + 16)
            data.replaceSubrange((textOffset + textPosition)..<(textOffset + textPosition + length), with: cell.text.utf8)
            textPosition += length
        }
        return data
    }
}

@MainActor
final class TerminalHistoryRuntimeTests: XCTestCase {
    private typealias Row = HistoryRuntimeModel.Row
    private typealias Cell = HistoryRuntimeModel.Cell
    private struct Harness {
        let coordinator: TerminalInputCoordinator, source: TerminalSource
        let transport: HistoryRuntimeTransport, model: HistoryRuntimeModel
        let runtime: TerminalModelRuntime, delivery: HistoryRuntimeDelivery
    }
    private func harness(rows: [Row] = [Row("old error"), Row("middle"), Row("new error"), Row("prompt")],
                         height: Int = 2, columns: Int = 32, hold: HistoryRuntimeGate? = nil) async throws -> Harness {
        let coordinator = try TerminalInputCoordinator(), transport = try HistoryRuntimeTransport()
        let source = try await coordinator.attach(transport)
        let model = HistoryRuntimeModel(source: source, rows: rows, height: height, columns: columns)
        let delivery = HistoryRuntimeDelivery(hold: hold)
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: delivery)
        return .init(coordinator: coordinator, source: source, transport: transport,
                     model: model, runtime: runtime, delivery: delivery)
    }
    private func search(_ h: Harness, _ query: String = "error") async throws -> TerminalHistorySearchResult<TerminalSource> {
        try await TerminalHistorySearch.search(query: query, source: h.source) {
            try await h.runtime.captureSearchPage(top: $0, source: h.source)
        }
    }
    private func expect<E: Error & Equatable>(_ error: E, _ operation: () async throws -> Void,
                                             file: StaticString = #filePath, line: UInt = #line) async {
        do { try await operation(); XCTFail("Expected refusal", file: file, line: line) }
        catch let actual { XCTAssertEqual(actual as? E, error, file: file, line: line) }
    }

    func testCanonicalSearchReadsDoNotPublishNavigateOrSendInput() async throws {
        let h = try await harness()
        try await h.runtime.setViewport(top: 101, source: h.source)
        let bookmark = try await h.runtime.captureHistoryBookmark(source: h.source)
        let result = try await search(h)
        XCTAssertEqual(result.matches.map(\.row), [102, 100])
        XCTAssertEqual(result.scannedRows, 4); XCTAssertEqual(result.completion, .complete)
        XCTAssertEqual(bookmark.viewportTop, 101)
        let after = try await h.runtime.captureHistoryBookmark(source: h.source)
        XCTAssertEqual(after.viewportTop, 101)
        let presented = await h.delivery.presented(), sent = await h.transport.bytes()
        XCTAssertEqual(presented.map(\.viewportFirst), [101])
        XCTAssertTrue(sent.isEmpty); XCTAssertEqual(h.model.activity().feeds, 0)
        try await h.runtime.close()
    }

    func testCanonicalCellsKeepWideAndCombiningBoundsAndSuppressConcealedText() async throws {
        let h = try await harness(rows: [Row(cells: [
            Cell(column: 0, width: 2, text: "界"), Cell(column: 2, width: 1, text: "e\u{301}"),
            Cell(column: 3, width: 2, text: "secret", concealed: true), Cell(column: 5, width: 2, text: "👩🏽‍💻")
        ])], height: 1)
        let page = try await h.runtime.captureSearchPage(top: nil, source: h.source)
        XCTAssertEqual(page.rows[0].cells.map(\.text), ["界", "e\u{301}", "", "👩🏽‍💻"])
        let match = try await search(h, "界e\u{301}")
        XCTAssertEqual(match.matches.first?.columns, 0..<3)
        let hidden = try await search(h, "secret")
        XCTAssertTrue(hidden.matches.isEmpty)
        try await h.runtime.close()
    }

    func testRevealAndReturnUseTheExactHistoryOrLiveBookmark() async throws {
        let h = try await harness()
        let live = try await h.runtime.captureHistoryBookmark(source: h.source)
        XCTAssertNil(live.viewportTop)
        try await h.runtime.setViewport(top: 101, source: h.source)
        let history = try await h.runtime.captureHistoryBookmark(source: h.source)
        let result = try await search(h)
        let match = try XCTUnwrap(result.matches.last)
        try await h.runtime.revealSearchMatch(match, source: h.source)
        try await h.runtime.restoreHistoryBookmark(history, source: h.source)
        try await h.runtime.receive(.init(output: Data("new output".utf8)), source: h.source)
        try await h.runtime.restoreHistoryBookmark(live, source: h.source)
        let frames = await h.delivery.presented()
        XCTAssertEqual(frames.map(\.viewportFirst), [101, 100, 101, 101, 103])
        let current = try await h.runtime.captureHistoryBookmark(source: h.source)
        XCTAssertNil(current.viewportTop, "returning live follows the new live row, not the old live index")
        let sent = await h.transport.bytes(); XCTAssertTrue(sent.isEmpty)
        try await h.runtime.close()
    }

    func testStaleMatchesNeverMoveTheViewportEvenIfSequenceIsReused() async throws {
        for mutation in ["bytes", "row", "geometry", "screen", "evicted"] {
            let h = try await harness()
            let result = try await search(h)
            let match = try XCTUnwrap(result.matches.last)
            switch mutation {
            case "bytes": h.model.replace(0, with: Row("old other"), preserveSequence: true)
            case "row": h.model.replace(0, with: Row("old error"))
            case "geometry": h.model.change(geometry: 2)
            case "screen": h.model.change(alternate: true)
            default: h.model.change(first: 101)
            }
            await expect(TerminalHistorySearchError.staleResult) {
                try await h.runtime.revealSearchMatch(match, source: h.source)
            }
            let frames = await h.delivery.presented(); XCTAssertTrue(frames.isEmpty, mutation)
            let bookmark = try await h.runtime.captureHistoryBookmark(source: h.source)
            XCTAssertNil(bookmark.viewportTop, mutation)
            try await h.runtime.close()
        }
    }

    func testExpiredOrChangedBookmarkDoesNotJumpToAReplacementRow() async throws {
        for mutation in ["row", "geometry", "screen", "evicted"] {
            let h = try await harness()
            try await h.runtime.setViewport(top: 100, source: h.source)
            let bookmark = try await h.runtime.captureHistoryBookmark(source: h.source)
            try await h.runtime.setViewport(top: nil, source: h.source)
            switch mutation {
            case "row": h.model.replace(0, with: Row("replacement"))
            case "geometry": h.model.change(geometry: 2)
            case "screen": h.model.change(alternate: true)
            default: h.model.change(first: 101)
            }
            await expect(TerminalHistorySearchError.staleResult) {
                try await h.runtime.restoreHistoryBookmark(bookmark, source: h.source)
            }
            let current = try await h.runtime.captureHistoryBookmark(source: h.source)
            XCTAssertNil(current.viewportTop, mutation)
            let frames = await h.delivery.presented(); XCTAssertEqual(frames.count, 2)
            try await h.runtime.close()
        }
    }

    func testForeignSourceAndAnotherRuntimeCannotReuseResultsOrBookmarks() async throws {
        let h = try await harness(), other = try await harness()
        let bookmark = try await h.runtime.captureHistoryBookmark(source: h.source)
        let result = try await search(h)
        let match = try XCTUnwrap(result.matches.first)
        await expect(TerminalInputError.stale) {
            _ = try await h.runtime.captureSearchPage(top: nil, source: other.source)
        }
        await expect(TerminalHistorySearchError.staleResult) {
            try await other.runtime.revealSearchMatch(match, source: other.source)
        }
        await expect(TerminalHistorySearchError.staleResult) {
            try await other.runtime.restoreHistoryBookmark(bookmark, source: other.source)
        }
        let replacement = TerminalModelRuntime(model: HistoryRuntimeModel(source: h.source, rows: [Row("x")], height: 1),
            coordinator: h.coordinator, delivery: h.delivery)
        await expect(TerminalHistorySearchError.staleResult) {
            try await replacement.restoreHistoryBookmark(bookmark, source: h.source)
        }
        try await h.runtime.close(); try await other.runtime.close(); try await replacement.close()
    }

    func testUnavailablePagesAndIncorrectProviderViewportsDoNotEndTheSession() async throws {
        for mutation in ["sync", "range", "wrongTop"] {
            let h = try await harness()
            if mutation == "sync" { h.model.change(synchronized: true) }
            if mutation == "range" { h.model.change(reportsHistory: false) }
            if mutation == "wrongTop" { h.model.change(wrongTop: true) }
            await expect(mutation == "wrongTop" ? TerminalHistorySearchError.invalidPage : .unavailable) {
                _ = try await h.runtime.captureSearchPage(top: 100, source: h.source)
            }
            h.model.change(synchronized: false, reportsHistory: true, wrongTop: false)
            try await h.runtime.receive(.init(output: Data("still live".utf8)), source: h.source)
            let page = try await h.runtime.captureSearchPage(top: nil, source: h.source)
            XCTAssertEqual(page.live, 103)
            let accounting = await h.runtime.accounting(); XCTAssertFalse(accounting.closing)
            try await h.runtime.close()
        }
    }

    func testOneQueuedReadIsBoundedAndCancellationSettlesBeforeTheNextRead() async throws {
        let hold = HistoryRuntimeGate(), h = try await harness(hold: hold)
        let receive = Task { try await h.runtime.receive(.init(output: Data("arrived".utf8)), source: h.source) }
        await h.delivery.entered.wait()
        let read = Task { try await h.runtime.captureSearchPage(top: nil, source: h.source) }
        while await h.runtime.accounting().waitingHistoryRequests == 0 { await Task.yield() }
        await expect(TerminalModelError.busy) { _ = try await h.runtime.captureOutputDocument(source: h.source) }
        read.cancel()
        await hold.release()
        try await receive.value
        do { _ = try await read.value; XCTFail("Cancelled read resumed") } catch { XCTAssertTrue(error is CancellationError) }
        let page = try await h.runtime.captureSearchPage(top: nil, source: h.source)
        XCTAssertEqual(page.live, 103)
        let accounting = await h.runtime.accounting(); XCTAssertEqual(accounting.waitingHistoryRequests, 0)
        try await h.runtime.close()
    }

    func testCloseRejectsAWaitingSearchAndJoinsPublicationBeforeModelDestruction() async throws {
        let hold = HistoryRuntimeGate(), h = try await harness(hold: hold)
        let receive = Task { try await h.runtime.receive(.init(output: Data("arrived".utf8)), source: h.source) }
        await h.delivery.entered.wait()
        let read = Task { try await h.runtime.captureSearchPage(top: nil, source: h.source) }
        while await h.runtime.accounting().waitingHistoryRequests == 0 { await Task.yield() }
        await h.runtime.requestClose()
        do { _ = try await read.value; XCTFail("Closed runtime resumed history") }
        catch { XCTAssertEqual(error as? TerminalModelError, .closed) }
        XCTAssertFalse(h.model.activity().closed)
        await hold.release(); try await receive.value; try await h.runtime.close()
        XCTAssertTrue(h.model.activity().closed)
        await expect(TerminalModelError.closed) { _ = try await h.runtime.captureOutputDocument(source: h.source) }
    }

    func testRevealPublishesBeforeWaitingOutputCanChangeItsMatchedRow() async throws {
        let hold = HistoryRuntimeGate(), h = try await harness(hold: hold)
        let result = try await search(h)
        let match = try XCTUnwrap(result.matches.last)
        let reveal = Task { try await h.runtime.revealSearchMatch(match, source: h.source) }
        await h.delivery.entered.wait()
        let receive = Task { try await h.runtime.receive(.init(output: Data("later output".utf8)), source: h.source) }
        await Task.yield()
        XCTAssertEqual(h.model.activity().feeds, 0, "publication retains the same canonical transaction as validation")
        let first = await h.delivery.presented()
        XCTAssertEqual(first.first?.viewportFirst, 100)
        XCTAssertTrue(first.first?.readableText.contains("old error") == true)
        await hold.release(); try await reveal.value; try await receive.value
        XCTAssertEqual(h.model.activity().feeds, 1)
        try await h.runtime.close()
    }

    func testReceivedOutputAfterEOFIsBoundedReadOnlyAndConcealedTextIsAbsent() async throws {
        let h = try await harness(rows: [Row("earlier"), Row(cells: [
            Cell(column: 0, width: 2, text: "界"), Cell(column: 2, width: 2, text: "secret", concealed: true),
            Cell(column: 4, width: 1, text: "!")
        ])])
        try await h.runtime.setViewport(top: 100, source: h.source)
        try await h.runtime.receive(.init(output: Data("finished".utf8), endOfOutput: true), source: h.source)
        let before = await h.delivery.presented().count
        let document = try await h.runtime.captureOutputDocument(source: h.source)
        XCTAssertEqual(document.text, "界  !\nfinished")
        XCTAssertEqual(try document.selectedText(in: NSRange(location: 0, length: document.utf16Count)), document.text)
        let selected = try await h.runtime.captureOutputDocument(source: h.source, liveViewport: false)
        XCTAssertEqual(selected.text, "earlier\n界  !", "a search preview copies its selected canonical rows")
        let after = await h.delivery.presented().count, sent = await h.transport.bytes()
        XCTAssertEqual(after, before); XCTAssertTrue(sent.isEmpty)
        let accounting = await h.runtime.accounting(); XCTAssertTrue(accounting.endOfOutput)
        try await h.runtime.close()
        XCTAssertEqual(document.text, "界  !\nfinished", "the bounded snapshot survives only as read-only text")
    }

    func testReceivedOutputRefusesIncompleteSyncAndOverBudgetText() async throws {
        let h = try await harness()
        h.model.change(synchronized: true)
        await expect(TerminalClipboardError.unavailable) { _ = try await h.runtime.captureOutputDocument(source: h.source) }
        h.model.change(synchronized: false, reportsHistory: false)
        let document = try await h.runtime.captureOutputDocument(source: h.source)
        XCTAssertEqual(document.text, "new error\nprompt", "copy does not require a search history capability")
        try await h.runtime.close()

        let cell = "e" + String(repeating: "\u{301}", count: 2000)
        let rows = (0..<9).map { _ in Row(cells: (0..<4).map { Cell(column: $0, width: 1, text: cell) }) }
        let large = try await harness(rows: rows, height: 9)
        await expect(TerminalClipboardError.capacity) { _ = try await large.runtime.captureOutputDocument(source: large.source) }
        let accounting = await large.runtime.accounting(); XCTAssertFalse(accounting.closing)
        try await large.runtime.close()
    }

    func testInspectionSearchRevealAndDoneRestoreTheOriginalViewportExactlyOnce() async throws {
        let h = try await harness()
        try await h.runtime.setViewport(top: 101, source: h.source)
        var preparations = 0
        var closes: [(UUID, TerminalHistoryBookmark?, String?)] = []
        let inspection = TerminalHistoryInspection(runtime: h.runtime, source: h.source, destination: "Machine",
            prepare: { preparations += 1 }, onClose: { closes.append(($0, $1, $2)) })
        inspection.query = "error"
        inspection.search()
        await inspection.waitUntilSettled()
        XCTAssertEqual(preparations, 1)
        XCTAssertEqual(inspection.result?.matches.map(\.row), [102, 100])
        XCTAssertNil(inspection.failure)
        let before = await h.delivery.presented()
        XCTAssertEqual(before.map(\.viewportFirst), [101], "search does not change the shown viewport")

        let match = try XCTUnwrap(inspection.result?.matches.last)
        inspection.reveal(match)
        await inspection.waitUntilSettled()
        XCTAssertEqual(inspection.selectedMatchID, match.id)
        XCTAssertEqual(inspection.preview?.text, "old error\nmiddle")
        let selected = try await h.runtime.captureHistoryBookmark(source: h.source)
        XCTAssertEqual(selected.viewportTop, 100)

        await inspection.close()
        await inspection.close(restoreViewport: false)
        let returned = try await h.runtime.captureHistoryBookmark(source: h.source)
        XCTAssertEqual(returned.viewportTop, 101)
        XCTAssertEqual(closes.count, 1)
        XCTAssertEqual(closes.first?.0, inspection.id)
        XCTAssertNil(closes.first?.1); XCTAssertNil(closes.first?.2)
        XCTAssertFalse(inspection.isPresented); XCTAssertFalse(inspection.isBusy)
        XCTAssertTrue(inspection.query.isEmpty)
        XCTAssertNil(inspection.result); XCTAssertNil(inspection.preview); XCTAssertNil(inspection.previewID)
        XCTAssertNil(inspection.selectedMatchID)
        let sent = await h.transport.bytes(); XCTAssertTrue(sent.isEmpty)
        try await h.runtime.close()
    }

    func testInspectionKeepsARevealedMatchAndReturnsOnlyItsOriginalBookmark() async throws {
        let h = try await harness()
        var closes: [(UUID, TerminalHistoryBookmark?, String?)] = []
        let inspection = TerminalHistoryInspection(runtime: h.runtime, source: h.source, destination: "Machine",
            prepare: {}, onClose: { closes.append(($0, $1, $2)) })
        inspection.query = "old error"; inspection.search()
        await inspection.waitUntilSettled()
        inspection.reveal(try XCTUnwrap(inspection.result?.matches.first))
        await inspection.waitUntilSettled()
        await inspection.close(restoreViewport: false)
        let retained = try await h.runtime.captureHistoryBookmark(source: h.source)
        XCTAssertEqual(retained.viewportTop, 100, "Show in Terminal preserves the validated selection")
        XCTAssertEqual(closes.count, 1); XCTAssertNil(closes.first?.2)
        let original = try XCTUnwrap(closes.first?.1)
        XCTAssertEqual(original.source, h.source); XCTAssertNil(original.viewportTop)
        try await h.runtime.receive(.init(output: Data("later".utf8)), source: h.source)
        try await h.runtime.restoreHistoryBookmark(original, source: h.source)
        let returned = try await h.runtime.captureHistoryBookmark(source: h.source)
        XCTAssertNil(returned.viewportTop, "the returned bookmark follows live output")
        let captures = h.model.activity().captures.count
        inspection.search(); inspection.retire(); await inspection.close()
        XCTAssertEqual(h.model.activity().captures.count, captures, "a closed owner never starts another read")
        XCTAssertEqual(closes.count, 1)
        try await h.runtime.close()
    }

    func testInspectionRepeatedRevealReplacesTheImmutablePreviewWhenContextChanges() async throws {
        let h = try await harness()
        let inspection = TerminalHistoryInspection(runtime: h.runtime, source: h.source, destination: "Machine",
            prepare: {}, onClose: { _, _, _ in })
        inspection.query = "old error"; inspection.search()
        await inspection.waitUntilSettled()
        let match = try XCTUnwrap(inspection.result?.matches.first)
        inspection.reveal(match); await inspection.waitUntilSettled()
        let first = try XCTUnwrap(inspection.previewID)
        XCTAssertEqual(inspection.preview?.text, "old error\nmiddle")
        h.model.replace(1, with: Row("later context"))
        inspection.reveal(match); await inspection.waitUntilSettled()
        XCTAssertEqual(inspection.selectedMatchID, match.id)
        XCTAssertEqual(inspection.preview?.text, "old error\nlater context")
        XCTAssertNotEqual(inspection.previewID, first, "updated context must replace the native view's immutable copy document")
        await inspection.close()
        XCTAssertNil(inspection.previewID)
        try await h.runtime.close()
    }

    func testInspectionLeavesExpiredResultContextVisibleAndDoesNotRetargetIt() async throws {
        let h = try await harness()
        let inspection = TerminalHistoryInspection(runtime: h.runtime, source: h.source, destination: "Machine",
            prepare: {}, onClose: { _, _, _ in })
        inspection.query = "old error"; inspection.search()
        await inspection.waitUntilSettled()
        let match = try XCTUnwrap(inspection.result?.matches.first)
        h.model.replace(0, with: Row("unrelated"), preserveSequence: true)
        inspection.reveal(match)
        await inspection.waitUntilSettled()
        XCTAssertEqual(inspection.failure, TerminalHistorySearchError.staleResult.errorDescription)
        XCTAssertEqual(inspection.result?.matches.first?.id, match.id)
        XCTAssertEqual(inspection.result?.matches.first?.excerpt, match.excerpt)
        XCTAssertNil(inspection.preview); XCTAssertNil(inspection.selectedMatchID)
        let stillLive = try await h.runtime.captureHistoryBookmark(source: h.source)
        XCTAssertNil(stillLive.viewportTop)
        let frames = await h.delivery.presented(); XCTAssertTrue(frames.isEmpty)
        await inspection.close()
        try await h.runtime.close()
    }

    func testInspectionReportsAnExpiredReturnBookmarkWithoutSelectingAnotherRow() async throws {
        let h = try await harness()
        try await h.runtime.setViewport(top: 101, source: h.source)
        var closes: [(UUID, TerminalHistoryBookmark?, String?)] = []
        let inspection = TerminalHistoryInspection(runtime: h.runtime, source: h.source, destination: "Machine",
            prepare: {}, onClose: { closes.append(($0, $1, $2)) })
        inspection.query = "old error"; inspection.search()
        await inspection.waitUntilSettled()
        inspection.reveal(try XCTUnwrap(inspection.result?.matches.first))
        await inspection.waitUntilSettled()
        h.model.replace(1, with: Row("replacement"))
        await inspection.close()
        let selected = try await h.runtime.captureHistoryBookmark(source: h.source)
        XCTAssertEqual(selected.viewportTop, 100)
        XCTAssertEqual(closes.count, 1); XCTAssertNil(closes.first?.1)
        XCTAssertEqual(closes.first?.2, TerminalHistorySearchError.staleResult.errorDescription)
        XCTAssertFalse(inspection.isPresented)
        try await h.runtime.close()
    }

    func testInspectionCancellationKeepsTheOnlyReaderSlotUntilTheModelSettles() async throws {
        let hold = HistoryRuntimeGate(), h = try await harness(hold: hold)
        let receive = Task { try await h.runtime.receive(.init(output: Data("arrived".utf8)), source: h.source) }
        await h.delivery.entered.wait()
        var preparations = 0
        let inspection = TerminalHistoryInspection(runtime: h.runtime, source: h.source, destination: "Machine",
            prepare: { preparations += 1 }, onClose: { _, _, _ in })
        inspection.query = "error"; inspection.search()
        while await h.runtime.accounting().waitingHistoryRequests == 0 { await Task.yield() }
        inspection.cancelSearch()
        inspection.query = "middle"; inspection.search()
        XCTAssertTrue(inspection.isBusy, "cancellation cannot release an operation still waiting on the model")
        XCTAssertEqual(preparations, 1, "repeated submission cannot create another reader")
        let queued = await h.runtime.accounting()
        XCTAssertEqual(queued.waitingHistoryRequests, 1)
        await hold.release(); try await receive.value
        await inspection.waitUntilSettled()
        XCTAssertFalse(inspection.isBusy); XCTAssertNil(inspection.result); XCTAssertNil(inspection.failure)
        inspection.search(); await inspection.waitUntilSettled()
        XCTAssertEqual(preparations, 2)
        XCTAssertEqual(inspection.result?.matches.map(\.row), [101])
        await inspection.close()
        try await h.runtime.close()
    }

    func testInspectionRetiredBeforePresentationNeverStartsWorkOrReturnsAuthority() async throws {
        let h = try await harness()
        var preparations = 0
        var closes: [(UUID, TerminalHistoryBookmark?, String?)] = []
        let inspection = TerminalHistoryInspection(runtime: h.runtime, source: h.source, destination: "Machine",
            prepare: { preparations += 1 }, onClose: { closes.append(($0, $1, $2)) })
        inspection.query = "error"; inspection.retire()
        XCTAssertFalse(inspection.isPresented, "a sheet mounted later can immediately observe retirement")
        XCTAssertTrue(inspection.isUnavailable); XCTAssertTrue(inspection.query.isEmpty)
        inspection.search(); inspection.retire()
        await inspection.waitUntilSettled(); await inspection.close(restoreViewport: false)
        XCTAssertEqual(preparations, 0); XCTAssertTrue(h.model.activity().captures.isEmpty)
        XCTAssertEqual(closes.count, 1); XCTAssertEqual(closes.first?.0, inspection.id)
        XCTAssertNil(closes.first?.1); XCTAssertNil(closes.first?.2)
        XCTAssertNil(inspection.result); XCTAssertNil(inspection.preview); XCTAssertFalse(inspection.isBusy)
        try await h.runtime.close()
    }

    func testInspectionRetirementClearsImmediatelyAndJoinsACancelledCanonicalRead() async throws {
        let hold = HistoryRuntimeGate(), h = try await harness(hold: hold)
        let receive = Task { try await h.runtime.receive(.init(output: Data("arrived".utf8)), source: h.source) }
        await h.delivery.entered.wait()
        var closes: [(UUID, TerminalHistoryBookmark?, String?)] = []
        let inspection = TerminalHistoryInspection(runtime: h.runtime, source: h.source, destination: "Machine",
            prepare: {}, onClose: { closes.append(($0, $1, $2)) })
        inspection.query = "error"; inspection.search()
        while await h.runtime.accounting().waitingHistoryRequests == 0 { await Task.yield() }
        let captures = h.model.activity().captures.count
        inspection.retire()
        XCTAssertFalse(inspection.isPresented); XCTAssertTrue(inspection.isUnavailable)
        XCTAssertTrue(inspection.query.isEmpty); XCTAssertNil(inspection.result); XCTAssertNil(inspection.preview)
        var joined = false
        let join = Task { await inspection.waitUntilSettled(); joined = true }
        await Task.yield()
        XCTAssertFalse(joined); XCTAssertTrue(inspection.isBusy); XCTAssertTrue(closes.isEmpty)
        inspection.search(); inspection.retire()
        await hold.release(); try await receive.value; await join.value
        XCTAssertTrue(joined); XCTAssertFalse(inspection.isBusy)
        XCTAssertEqual(closes.count, 1); XCTAssertNil(closes.first?.1); XCTAssertNil(closes.first?.2)
        XCTAssertNil(inspection.result); XCTAssertNil(inspection.preview); XCTAssertNil(inspection.failure)
        XCTAssertEqual(h.model.activity().captures.count, captures, "cancelled work cannot repopulate the retired inspection")
        try await h.runtime.close()
        XCTAssertTrue(h.model.activity().closed)
    }

    func testInspectionDoneJoinsCancelledRevealThenRestoresTheOriginalLiveViewport() async throws {
        let hold = HistoryRuntimeGate(), h = try await harness(hold: hold)
        var closes: [(UUID, TerminalHistoryBookmark?, String?)] = []
        let inspection = TerminalHistoryInspection(runtime: h.runtime, source: h.source, destination: "Machine",
            prepare: {}, onClose: { closes.append(($0, $1, $2)) })
        inspection.query = "old error"; inspection.search()
        await inspection.waitUntilSettled()
        inspection.reveal(try XCTUnwrap(inspection.result?.matches.first))
        await h.delivery.entered.wait()
        var closed = false
        let close = Task { await inspection.close(); closed = true }
        while !inspection.isClosing { await Task.yield() }
        XCTAssertFalse(closed); XCTAssertTrue(inspection.isBusy)
        let revealed = await h.delivery.presented()
        XCTAssertEqual(revealed.map(\.viewportFirst), [100])
        await hold.release(); await close.value
        let frames = await h.delivery.presented()
        XCTAssertEqual(frames.map(\.viewportFirst), [100, 102], "a cancelled publication still needs its original position restored")
        let original = try await h.runtime.captureHistoryBookmark(source: h.source)
        XCTAssertNil(original.viewportTop)
        XCTAssertTrue(closed); XCTAssertFalse(inspection.isBusy); XCTAssertNil(inspection.preview)
        XCTAssertEqual(closes.count, 1); XCTAssertNil(closes.first?.1); XCTAssertNil(closes.first?.2)
        try await h.runtime.close()
    }
}
