import Foundation
import XCTest
@testable import FrankenTermKit

private func queryPane(_ id: Int) -> TerminalMuxPane {
    .init(paneId: id, tabId: id, windowId: 0, tabTitle: "", title: "p\(id)", workspace: "default",
          workingDir: nil, size: .init(rows: 24, cols: 80), isActive: id == 1, isZoomed: false)
}
private func queryResults(_ count: Int, request: UInt64 = 1, text: String = "a match", error: String? = nil) -> TerminalMuxSearchResults {
    .init(search: request, matches: (0..<count).map {
        .init(row: Int64($0 + 5), startColumn: 1, endRow: Int64($0 + 5), endColumn: 4, text: text)
    }, searchedRows: 100, error: error)
}
private let queryNeedle = TerminalMuxControl.SearchQuery(text: "needle", caseSensitive: false, regex: false)

final class TerminalMuxControlTests: XCTestCase {
    func testASearchFillsInPaneByPaneAndAReplacedOneIsIgnored() {
        let control = TerminalMuxControl()
        control.publish(panes: [queryPane(1), queryPane(2), queryPane(3)], shown: 1)
        final class Wakes: @unchecked Sendable { let lock = NSLock(); var count = 0 }
        let wakes = Wakes()
        control.setWaker { wakes.lock.withLock { wakes.count += 1 } }
        let first = control.search(queryNeedle, panes: [1, 2, 3])
        XCTAssertEqual(wakes.lock.withLock { wakes.count }, 1, "the transport is woken to send it")
        XCTAssertEqual(control.takeSearch()?.panes, [1, 2, 3])
        XCTAssertNil(control.takeSearch(), "a search is sent once")
        control.deliver(search: first, pane: 2, results: queryResults(2))
        XCTAssertEqual(control.current.search?.pending, [1, 3])
        XCTAssertEqual(control.current.search?.results[2]?.matches.count, 2)
        control.remove(pane: 3)
        XCTAssertEqual(control.current.search?.pending, [1])
        let second = control.search(queryNeedle, panes: [1])
        control.deliver(search: first, pane: 1, results: queryResults(9))
        XCTAssertEqual(control.current.search?.id, second)
        XCTAssertNil(control.current.search?.results[1])
        control.deliver(search: second, pane: 1, results: queryResults(0, error: "regex parse error"))
        XCTAssertEqual(control.current.search?.pending, [])
        XCTAssertEqual(control.current.search?.results[1]?.error, "regex parse error")
        control.clearSearch()
        XCTAssertNil(control.current.search)
        XCTAssertNil(control.takeSearch())
    }

    func testContextIsReadOnceAndOnlyItsOwnAnswerLands() throws {
        let control = TerminalMuxControl()
        control.publish(panes: [queryPane(4)], shown: 4)
        control.readContext(pane: 4, row: 100, before: 2, after: 3)
        let request = try XCTUnwrap(control.takeContext())
        XCTAssertEqual(request.pane, 4); XCTAssertEqual(request.first, 98); XCTAssertEqual(request.count, 6)
        XCTAssertNil(control.takeContext()); XCTAssertNil(control.current.context?.lines)
        control.deliver(context: UUID(), lines: [])
        XCTAssertNil(control.current.context?.lines, "another request's rows are ignored")
        let line = TerminalMuxContextLine(row: 100, text: "hit")
        control.deliver(context: request.id, lines: [line])
        XCTAssertEqual(control.current.context?.lines, [line])
    }

    func testReentrantClearCannotLeaveAnObsoleteSearchOrContextQueued() {
        let control = TerminalMuxControl()
        control.publish(panes: [queryPane(1)], shown: 1)
        control.observe { state in
            if state.search != nil { control.clearSearch() }
            if state.context != nil { control.clearContext() }
        }
        _ = control.search(queryNeedle, panes: [1])
        XCTAssertNil(control.current.search); XCTAssertNil(control.takeSearch())
        control.readContext(pane: 1, row: 5)
        XCTAssertNil(control.current.context); XCTAssertNil(control.takeContext())
    }

    func testInvalidQueryAndPaneSetsNeverProduceWorkOrRetainOversizedText() {
        let control = TerminalMuxControl()
        control.publish(panes: (0..<129).map(queryPane), shown: 0)
        for panes in [[], [-1], [0, 0], [130], Array(0..<129)] {
            _ = control.search(queryNeedle, panes: panes)
            XCTAssertNotNil(control.current.search?.issue); XCTAssertNil(control.takeSearch())
            XCTAssertTrue(control.current.search?.pending.isEmpty == true)
        }
        for text in ["", String(repeating: "é", count: 2049)] {
            _ = control.search(.init(text: text, caseSensitive: false, regex: false), panes: [0])
            XCTAssertNil(control.takeSearch()); XCTAssertEqual(control.current.search?.query.text, "")
        }
        _ = control.search(.init(text: String(repeating: "é", count: 2048), caseSensitive: true, regex: true), panes: [0])
        XCTAssertEqual(control.takeSearch()?.query.text.utf8.count, 4096)
    }

    func testForeignPaneDuplicateAndStaleAnswersCannotReplaceResults() throws {
        let control = TerminalMuxControl()
        control.publish(panes: [queryPane(1), queryPane(2)], shown: 1)
        let id = control.search(queryNeedle, panes: [1])
        let before = control.current.revision
        control.deliver(search: id, pane: 2, results: queryResults(1))
        XCTAssertEqual(control.current.revision, before)
        control.deliver(search: id, pane: 1, results: queryResults(1))
        let answer = try XCTUnwrap(control.current.search)
        control.deliver(search: id, pane: 1, results: queryResults(8))
        XCTAssertEqual(control.current.search, answer)
    }

    func testResultCountAndTextBudgetsAreSharedAcrossAllPanes() throws {
        let control = TerminalMuxControl()
        control.publish(panes: (1...10).map(queryPane), shown: 1)
        var id = control.search(queryNeedle, panes: Array(1...10))
        for pane in 1...5 { control.deliver(search: id, pane: pane, results: queryResults(128)) }
        var search = try XCTUnwrap(control.current.search)
        XCTAssertEqual(search.results.values.reduce(0) { $0 + $1.matches.count }, 512)
        XCTAssertTrue(search.limitReached); XCTAssertTrue(search.pending.isEmpty); XCTAssertNotNil(search.issue)
        id = control.search(queryNeedle, panes: Array(1...10))
        let text = String(repeating: "x", count: 16 * 1024)
        for pane in 1...5 { control.deliver(search: id, pane: pane, results: queryResults(8, text: text)) }
        search = try XCTUnwrap(control.current.search)
        XCTAssertEqual(search.results.values.reduce(0) { sum, value in sum + value.matches.reduce(0) { $0 + $1.text.utf8.count } }, 512 * 1024)
        XCTAssertTrue(search.limitReached); XCTAssertTrue(search.pending.isEmpty)
    }

    func testMalformedMatchCoordinatesAndOversizedErrorsAreBounded() throws {
        let control = TerminalMuxControl(); control.publish(panes: [queryPane(1)], shown: 1)
        let bad: [TerminalMuxSearchMatch] = [
            .init(row: -1, startColumn: 0, endRow: 0, endColumn: 1, text: "x"),
            .init(row: 1, startColumn: 5, endRow: 1, endColumn: 4, text: "x"),
            .init(row: 3, startColumn: 0, endRow: 2, endColumn: 0, text: "x"),
            .init(row: 1, startColumn: Int.max, endRow: 1, endColumn: Int.max, text: "x")
        ]
        for match in bad {
            let id = control.search(queryNeedle, panes: [1])
            control.deliver(search: id, pane: 1, results: .init(search: 1, matches: [match], searchedRows: 20, error: nil))
            XCTAssertNotNil(control.current.search?.results[1]?.error)
            XCTAssertTrue(control.current.search?.results[1]?.matches.isEmpty == true)
        }
        let id = control.search(queryNeedle, panes: [1])
        control.deliver(search: id, pane: 1, results: queryResults(0, error: String(repeating: "x", count: 20_000)))
        XCTAssertLessThan(try XCTUnwrap(control.current.search?.results[1]?.error).utf8.count, 512)
        let duplicateID = control.search(queryNeedle, panes: [1]), match = queryResults(1).matches[0]
        control.deliver(search: duplicateID, pane: 1, results: .init(search: 1, matches: [match, match], searchedRows: 20, error: nil))
        XCTAssertNotNil(control.current.search?.results[1]?.error)
    }

    func testContextIntegerExtremesAndWrongRowsCannotCrashOrLeakIntoReader() throws {
        let control = TerminalMuxControl(); control.publish(panes: [queryPane(1)], shown: 1)
        for (row, before, after) in [(Int64.min, 0, 0), (.max, 0, 0), (0, Int.max, 0), (0, 0, Int.max), (1, -1, 0), (1, 128, 128)] {
            control.readContext(pane: 1, row: row, before: before, after: after)
            XCTAssertNil(control.takeContext()); XCTAssertNotNil(control.current.context?.error)
        }
        control.readContext(pane: 1, row: 0)
        let request = try XCTUnwrap(control.takeContext())
        XCTAssertEqual(request.first, 0); XCTAssertEqual(request.count, 13)
        control.deliver(context: request.id, lines: [.init(row: .max, text: "wrong row")])
        XCTAssertNotNil(control.current.context?.error); XCTAssertTrue(control.current.context?.lines?.isEmpty == true)
        for lines in [[TerminalMuxContextLine(row: 5, text: "a"), .init(row: 4, text: "b")],
                      [.init(row: 5, text: String(repeating: "x", count: 16_385))],
                      [.init(row: 5, text: "a"), .init(row: 5, text: "b")]] {
            control.readContext(pane: 1, row: 5)
            let request = try XCTUnwrap(control.takeContext())
            control.deliver(context: request.id, lines: lines)
            XCTAssertNotNil(control.current.context?.error)
        }
    }

    func testContextHasATotalTextBudgetAndNeverReplacesItsFirstAnswer() throws {
        let control = TerminalMuxControl(); control.publish(panes: [queryPane(1)], shown: 1)
        control.readContext(pane: 1, row: 12)
        let request = try XCTUnwrap(control.takeContext())
        control.deliver(context: request.id, lines: (0..<9).map { .init(row: Int64($0), text: String(repeating: "x", count: 16_384)) })
        let context = try XCTUnwrap(control.current.context)
        XCTAssertNotNil(context.error); XCTAssertTrue(context.lines?.isEmpty == true)
        control.deliver(context: request.id, lines: [.init(row: 12, text: "replacement")])
        XCTAssertEqual(control.current.context, context)
    }

    func testInventoryReplacementExpiresSearchContextAndNumericPaneReuse() throws {
        let control = TerminalMuxControl(); control.publish(panes: [queryPane(1), queryPane(2)], shown: 1)
        let id = control.search(queryNeedle, panes: [1, 2])
        control.deliver(search: id, pane: 1, results: queryResults(1))
        control.readContext(pane: 1, row: 5, searchID: id)
        let context = try XCTUnwrap(control.takeContext())
        control.publish(panes: [queryPane(2)], shown: 2)
        control.publish(panes: [queryPane(1), queryPane(2)], shown: 1)
        control.deliver(context: context.id, lines: [.init(row: 5, text: "old instance")])
        XCTAssertNotNil(control.current.context?.error)
        control.readContext(pane: 1, row: 5, searchID: id)
        XCTAssertNil(control.takeContext()); XCTAssertNotNil(control.current.context?.error)
        control.deliver(search: id, pane: 1, results: queryResults(1))
        XCTAssertTrue(control.current.search?.results[1]?.matches.isEmpty == true)
    }

    func testCancellingAndClosingPreserveHitsButSettleSpinnersHonestly() {
        let control = TerminalMuxControl(); control.publish(panes: [queryPane(1), queryPane(2)], shown: 1)
        let id = control.search(queryNeedle, panes: [1, 2])
        control.deliver(search: id, pane: 1, results: queryResults(1))
        control.cancelSearch(UUID()); XCTAssertEqual(control.current.search?.pending, [2])
        control.cancelSearch(id)
        XCTAssertTrue(control.current.search?.pending.isEmpty == true)
        XCTAssertEqual(control.current.search?.results[1]?.matches.count, 1)
        XCTAssertNotNil(control.current.search?.issue); XCTAssertNil(control.takeSearch())
        _ = control.search(queryNeedle, panes: [1]); control.readContext(pane: 1, row: 5)
        control.endQueries()
        XCTAssertTrue(control.current.search?.pending.isEmpty == true)
        XCTAssertNotNil(control.current.search?.issue); XCTAssertNotNil(control.current.context?.error)
        XCTAssertNil(control.takeSearch()); XCTAssertNil(control.takeContext())
    }
}

private final class QueryClient: TerminalMuxClient, @unchecked Sendable {
    struct Call { let id: UInt64, pane: Int, text: String?, first: Int64?, count: Int? }
    var calls: [Call] = []
    var sequence: UInt64 = 0
    var fail = false, reuseID = false
    func receive(_ bytes: Data) throws {}
    func channelClosed() throws {}
    func outgoing(limit: Int) throws -> Data { Data() }
    func nextEvent() throws -> TerminalMuxEvent? { nil }
    func refreshPanes() throws {}
    func show(pane: Int) throws {}
    func hide(pane: Int) throws {}
    func write(pane: Int, bytes: Data) throws {}
    func paste(pane: Int, text: String) throws {}
    func resize(pane: Int, rows: Int, columns: Int) throws {}
    func restoreSize(pane: Int) throws {}
    private func next() throws -> UInt64 {
        if fail { throw TerminalMuxError.unavailable }
        if !reuseID { sequence += 1 }
        return sequence
    }
    func search(pane: Int, text: String, caseSensitive: Bool, regex: Bool) throws -> UInt64 {
        let id = try next(); calls.append(.init(id: id, pane: pane, text: text, first: nil, count: nil)); return id
    }
    func context(pane: Int, firstRow: Int64, count: Int) throws -> UInt64 {
        let id = try next(); calls.append(.init(id: id, pane: pane, text: nil, first: firstRow, count: count)); return id
    }
}

final class TerminalMuxQueryDriverTests: XCTestCase {
    private func fixture(_ count: Int = 10) -> (TerminalMuxControl, QueryClient, TerminalMuxQueryDriver) {
        let control = TerminalMuxControl(), client = QueryClient()
        control.publish(panes: (1...count).map(queryPane), shown: 1)
        return (control, client, TerminalMuxQueryDriver(client: client, control: control))
    }

    func testAllPanesAreScheduledInOrderWithOnlyThreeOutstanding() {
        let (control, client, driver) = fixture(128), now = ContinuousClock.now
        _ = control.search(queryNeedle, panes: Array(1...128))
        driver.step(now: now)
        XCTAssertEqual(client.calls.map(\.pane), [1, 2, 3])
        for index in 0..<128 {
            XCTAssertLessThanOrEqual(driver.outstandingCount, 3)
            let request = client.calls[index]
            driver.receive(pane: request.pane, results: queryResults(1, request: request.id), now: now)
            driver.step(now: now)
        }
        XCTAssertEqual(client.calls.map(\.pane), Array(1...128))
        XCTAssertEqual(driver.outstandingCount, 0)
        XCTAssertTrue(control.current.search?.pending.isEmpty == true)
        XCTAssertEqual(control.current.search?.results.count, 128)
    }

    func testTenThousandReplacementsCannotForgetOrReplenishOutstandingWork() {
        let (control, client, driver) = fixture(), now = ContinuousClock.now
        for index in 0..<10_000 {
            _ = control.search(.init(text: "query-\(index)", caseSensitive: false, regex: false), panes: Array(1...10))
            driver.step(now: now)
        }
        XCTAssertEqual(client.calls.count, 3); XCTAssertEqual(driver.outstandingCount, 3)
        let old = client.calls[0]
        driver.receive(pane: old.pane, results: queryResults(1, request: old.id), now: now)
        XCTAssertTrue(control.current.search?.results.isEmpty == true)
        driver.step(now: now)
        XCTAssertEqual(client.calls.count, 4); XCTAssertEqual(client.calls.last?.text, "query-9999")
        XCTAssertEqual(driver.outstandingCount, 3)
    }

    func testTimeoutSettlesTheUIButRetainsRemoteDebtUntilAnActualReply() {
        let (control, client, driver) = fixture(), now = ContinuousClock.now
        _ = control.search(queryNeedle, panes: Array(1...10)); driver.step(now: now)
        driver.step(now: now + .seconds(10))
        XCTAssertEqual(driver.outstandingCount, 3); XCTAssertEqual(client.calls.count, 3)
        XCTAssertNotNil(control.current.search?.results[1]?.error)
        driver.step(now: now + .seconds(30))
        XCTAssertTrue(control.current.search?.pending.isEmpty == true); XCTAssertNotNil(control.current.search?.issue)
        XCTAssertEqual(driver.waitTimeout(maximum: .seconds(100), now: now + .seconds(30)), .seconds(100))
        _ = control.search(queryNeedle, panes: [1]); driver.step(now: now + .seconds(31))
        XCTAssertEqual(client.calls.count, 3)
        let old = client.calls[0]
        driver.receive(pane: old.pane, results: queryResults(1, request: old.id), now: now + .seconds(31))
        driver.step(now: now + .seconds(31))
        XCTAssertEqual(client.calls.count, 4)
        XCTAssertTrue(control.current.search?.results.isEmpty == true)
    }

    func testResponseAtTheDeadlineDoesNotPublishATimedOutHit() {
        let (control, client, driver) = fixture(1), now = ContinuousClock.now
        _ = control.search(queryNeedle, panes: [1]); driver.step(now: now)
        driver.receive(pane: 1, results: queryResults(1, request: client.calls[0].id), now: now + .seconds(10))
        XCTAssertTrue(control.current.search?.results[1]?.matches.isEmpty == true)
        XCTAssertNotNil(control.current.search?.results[1]?.error)
        XCTAssertEqual(driver.outstandingCount, 0)
    }

    func testContextHasItsOwnSlotAndResponsesRequireBothRequestAndPane() throws {
        let (control, client, driver) = fixture(), now = ContinuousClock.now
        _ = control.search(queryNeedle, panes: Array(1...10)); driver.step(now: now)
        control.readContext(pane: 1, row: 5); driver.step(now: now)
        XCTAssertEqual(client.calls.count, 4); XCTAssertEqual(driver.outstandingCount, 4)
        let context = try XCTUnwrap(client.calls.last)
        driver.receive(pane: 2, context: .init(request: context.id, lines: [.init(row: 5, text: "wrong pane")]), now: now)
        XCTAssertNotNil(control.current.context?.error); XCTAssertTrue(control.current.context?.lines?.isEmpty == true)
        XCTAssertTrue(control.current.search?.pending.isEmpty == true)
        _ = control.search(queryNeedle, panes: [1]); driver.step(now: now)
        XCTAssertEqual(client.calls.count, 4); XCTAssertNotNil(control.current.search?.issue)
    }

    func testAReplacedContextWaitsForTheActualPriorReadAndDoesNotShowItsRows() throws {
        let (control, client, driver) = fixture(), now = ContinuousClock.now
        control.readContext(pane: 1, row: 5); driver.step(now: now)
        let old = try XCTUnwrap(client.calls.last)
        control.readContext(pane: 2, row: 8); driver.step(now: now)
        XCTAssertEqual(client.calls.count, 1)
        driver.receive(pane: 1, context: .init(request: old.id, lines: [.init(row: 5, text: "old")]), now: now)
        XCTAssertNil(control.current.context?.lines)
        driver.step(now: now)
        let current = try XCTUnwrap(client.calls.last)
        XCTAssertEqual(current.pane, 2)
        driver.receive(pane: 2, context: .init(request: current.id, lines: [.init(row: 8, text: "new")]), now: now)
        XCTAssertEqual(control.current.context?.lines?.map(\.text), ["new"])
    }

    func testCancelledSearchDropsUnsentWorkButNotAcceptedReservations() {
        let (control, client, driver) = fixture(), now = ContinuousClock.now
        let id = control.search(queryNeedle, panes: Array(1...10)); driver.step(now: now)
        control.cancelSearch(id); driver.step(now: now)
        for call in client.calls { driver.receive(pane: call.pane, results: queryResults(1, request: call.id), now: now) }
        driver.step(now: now)
        XCTAssertEqual(client.calls.count, 3); XCTAssertEqual(driver.outstandingCount, 0)
        XCTAssertTrue(control.current.search?.results.isEmpty == true)
    }

    func testQueryAdmissionFailureAndReusedIDsDisableOnlyFurtherQueries() {
        for failing in [true, false] {
            let (control, client, driver) = fixture(), now = ContinuousClock.now
            client.fail = failing; client.reuseID = !failing
            _ = control.search(queryNeedle, panes: Array(1...10)); driver.step(now: now)
            XCTAssertNotNil(control.current.search?.issue)
            XCTAssertTrue(control.current.search?.pending.isEmpty == true)
            let sent = client.calls.count
            client.fail = false; client.reuseID = false
            _ = control.search(queryNeedle, panes: [1]); driver.step(now: now)
            XCTAssertEqual(client.calls.count, sent)
            XCTAssertFalse(control.inputRouting.current.retired)
        }
    }

    func testWrongResponseKindAndUnknownResponsesCannotFreeRequestSlots() {
        let (control, client, driver) = fixture(), now = ContinuousClock.now
        _ = control.search(queryNeedle, panes: [1, 2, 3]); driver.step(now: now)
        driver.receive(pane: 1, results: queryResults(1, request: 10_000), now: now)
        XCTAssertEqual(driver.outstandingCount, 3)
        driver.receive(pane: 1, context: .init(request: client.calls[0].id, lines: []), now: now)
        XCTAssertEqual(driver.outstandingCount, 3)
        XCTAssertNotNil(control.current.search?.issue)
    }

    func testRemovedPaneCannotReceiveAnOldReplyAfterReappearing() {
        let (control, client, driver) = fixture(), now = ContinuousClock.now
        _ = control.search(queryNeedle, panes: [1]); driver.step(now: now)
        control.remove(pane: 1); control.publish(panes: [queryPane(1)], shown: 1)
        driver.receive(pane: 1, results: queryResults(1, request: client.calls[0].id), now: now)
        XCTAssertTrue(control.current.search?.results[1]?.matches.isEmpty == true)
        XCTAssertNotNil(control.current.search?.results[1]?.error)
        XCTAssertEqual(driver.outstandingCount, 0)
    }

    func testContextTimeoutWhileWaitingForASlotEndsWithoutBusyLoop() {
        let (control, client, driver) = fixture(), now = ContinuousClock.now
        control.readContext(pane: 1, row: 5); driver.step(now: now)
        control.readContext(pane: 2, row: 8); driver.step(now: now + .seconds(1))
        driver.step(now: now + .seconds(11))
        XCTAssertEqual(client.calls.count, 1); XCTAssertEqual(driver.outstandingCount, 1)
        XCTAssertNotNil(control.current.context?.error)
        XCTAssertEqual(driver.waitTimeout(maximum: .seconds(20), now: now + .seconds(11)), .seconds(20))
    }

    func testRetirementSettlesAllQueriesAndNeverRestartsDispatch() {
        let (control, client, driver) = fixture(), now = ContinuousClock.now
        _ = control.search(queryNeedle, panes: [1, 2, 3]); control.readContext(pane: 1, row: 5)
        driver.step(now: now); driver.retire(); driver.retire()
        XCTAssertEqual(driver.outstandingCount, 0)
        XCTAssertNotNil(control.current.search?.issue); XCTAssertNotNil(control.current.context?.error)
        let calls = client.calls.count
        _ = control.search(queryNeedle, panes: [1]); driver.step(now: now)
        XCTAssertEqual(client.calls.count, calls)
        XCTAssertNotNil(control.current.search?.issue)
        XCTAssertTrue(control.current.search?.pending.isEmpty == true)
        control.readContext(pane: 1, row: 5)
        XCTAssertNotNil(control.current.context?.error); XCTAssertNil(control.takeContext())
    }

    func testPhoneSizesSurviveAcrossConnectionsPerMachineAndClearWhenRestored() throws {
        let suite = "frankenterm.tests.phoneSizes.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let desktop = TerminalMuxSize(rows: 50, cols: 180, pixelWidth: 1440, pixelHeight: 800, dpi: 144)
        let first = TerminalMuxPhoneSizes(workspace: "w", machine: "ts1", defaults: defaults)
        first.set(pane: 4, desktop: desktop, rows: 40, columns: 54)
        first.set(pane: 9, desktop: desktop, rows: 40, columns: 54)
        first.set(pane: 9, desktop: nil, rows: 0, columns: 0)

        // The next connection to the same machine sees what was left.
        let next = TerminalMuxPhoneSizes(workspace: "w", machine: "ts1", defaults: defaults)
        XCTAssertEqual(next.records, [4: .init(desktop: desktop, rows: 40, columns: 54)])
        XCTAssertTrue(TerminalMuxPhoneSizes(workspace: "w", machine: "ts2", defaults: defaults).records.isEmpty)
        next.removeAll()
        XCTAssertTrue(first.records.isEmpty)

        for pane in 0..<(TerminalMuxPhoneSizes.maximumRecords + 10) {
            first.set(pane: pane, desktop: desktop, rows: 40, columns: 54)
        }
        XCTAssertEqual(first.records.count, TerminalMuxPhoneSizes.maximumRecords, "bounded")
    }

    func testANewConnectionComesBackToTheShownPaneOnlyWhileItIsStillThere() throws {
        let suite = "frankenterm.tests.shownPane.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        func pane(_ id: Int, tab: Int, active: Bool = false) -> TerminalMuxPane {
            TerminalMuxPane(paneId: id, tabId: tab, windowId: 0, tabTitle: "", title: "p\(id)", workspace: "default",
                            workingDir: nil, size: .init(rows: 24, cols: 80), isActive: active, isZoomed: false)
        }
        let listed = [pane(1, tab: 0, active: true), pane(4, tab: 2), pane(7, tab: 3)]
        // A v0.13 server, which does not name its run: the numbers decide.
        let first = TerminalMuxShownPane(workspace: "w", machine: "ts1", defaults: defaults)
        // Nothing remembered: the desktop's active pane, and nothing to say.
        XCTAssertEqual(first.choose(from: listed, incarnation: nil).pane, listed[0])
        XCTAssertFalse(first.choose(from: listed, incarnation: nil).lost)
        first.set(listed[1], incarnation: nil)

        // The next connection to the same machine comes back to it.
        let next = TerminalMuxShownPane(workspace: "w", machine: "ts1", defaults: defaults)
        XCTAssertEqual(next.record, .init(listed[1], incarnation: nil))
        XCTAssertEqual(next.choose(from: listed, incarnation: nil).pane, listed[1])
        XCTAssertFalse(next.choose(from: listed, incarnation: nil).lost)
        XCTAssertNil(TerminalMuxShownPane(workspace: "w", machine: "ts2", defaults: defaults).record)

        // Gone, or its number in another tab (a restarted server): the active
        // pane, and the screen says the one being viewed closed.
        for panes in [[listed[0], listed[2]], [listed[0], pane(4, tab: 9), listed[2]]] {
            let choice = next.choose(from: panes, incarnation: nil)
            XCTAssertEqual(choice.pane, listed[0]); XCTAssertTrue(choice.lost); XCTAssertFalse(choice.restarted)
        }
        XCTAssertNil(next.choose(from: [], incarnation: nil).pane)
        // A pane that closed while shown is forgotten: nothing to say later.
        next.set(nil, incarnation: nil)
        XCTAssertNil(first.record)
        XCTAssertFalse(first.choose(from: [listed[0]], incarnation: nil).lost)

        let control = TerminalMuxControl()
        XCTAssertNil(control.current.notice)
        control.recordNotice("The pane you were viewing closed while you were away.")
        XCTAssertEqual(control.current.notice, "The pane you were viewing closed while you were away.")
    }

    func testInputAConnectionLeftUnconfirmedIsToldOnceOnlyForThatPane() throws {
        let suite = "frankenterm.tests.unconfirmedInput.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        func pane(_ id: Int, tab: Int) -> TerminalMuxPane {
            TerminalMuxPane(paneId: id, tabId: tab, windowId: 0, tabTitle: "", title: "p\(id)", workspace: "default",
                            workingDir: nil, size: .init(rows: 24, cols: 80), isActive: id == 1, isZoomed: false)
        }
        let listed = [pane(1, tab: 0), pane(4, tab: 2)]
        let ending = TerminalMuxShownPane(workspace: "w", machine: "ts1", defaults: defaults)
        // Nothing remembered: nothing to mark.
        ending.markInputUnconfirmed()
        XCTAssertNil(ending.record)
        ending.set(listed[1], incarnation: nil)
        XCTAssertFalse(ending.choose(from: listed, incarnation: nil).inputUnconfirmed)
        ending.markInputUnconfirmed()

        // The next connection comes back to that pane and is told; showing it
        // clears the mark, so the one after is not.
        let next = TerminalMuxShownPane(workspace: "w", machine: "ts1", defaults: defaults)
        let choice = next.choose(from: listed, incarnation: nil)
        XCTAssertEqual(choice.pane, listed[1]); XCTAssertFalse(choice.lost); XCTAssertTrue(choice.inputUnconfirmed)
        // Gone, or its number in another tab: only that it closed.
        for panes in [[listed[0]], [listed[0], pane(4, tab: 9)]] {
            let other = next.choose(from: panes, incarnation: nil)
            XCTAssertTrue(other.lost); XCTAssertFalse(other.inputUnconfirmed)
        }
        next.set(listed[1], incarnation: nil)
        XCTAssertFalse(TerminalMuxShownPane(workspace: "w", machine: "ts1", defaults: defaults)
            .choose(from: listed, incarnation: nil).inputUnconfirmed)
    }

    func testAPaneShownBeforeTheServerRestartedIsNotTheOneUnderItsNumbersNow() throws {
        let suite = "frankenterm.tests.restartedMux.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        func pane(_ id: Int) -> TerminalMuxPane {
            TerminalMuxPane(paneId: id, tabId: id, windowId: 0, tabTitle: "", title: "p\(id)", workspace: "default",
                            workingDir: nil, size: .init(rows: 24, cols: 80), isActive: id == 0, isZoomed: false)
        }
        let listed = [pane(0), pane(1)]
        let run = Data(repeating: 0x1c, count: 16), restarted = Data(repeating: 0x2d, count: 16)
        let shown = TerminalMuxShownPane(workspace: "w", machine: "mac", defaults: defaults)
        shown.set(listed[1], incarnation: run)
        shown.markInputUnconfirmed()
        XCTAssertEqual(shown.record?.incarnation, run, "marking input keeps the run")

        // The same run: the same pane, and the input it never confirmed.
        var choice = shown.choose(from: listed, incarnation: run)
        XCTAssertEqual(choice.pane, listed[1]); XCTAssertFalse(choice.lost); XCTAssertFalse(choice.restarted)
        XCTAssertTrue(choice.inputUnconfirmed)
        // Another run lists the same numbers: the desktop's active pane, told as a
        // restart. The earlier run's input is not this pane's to report.
        choice = shown.choose(from: listed, incarnation: restarted)
        XCTAssertEqual(choice.pane, listed[0]); XCTAssertTrue(choice.lost); XCTAssertTrue(choice.restarted)
        XCTAssertFalse(choice.inputUnconfirmed)
        // A server that does not say, or a pane remembered from one: the numbers decide.
        XCTAssertEqual(shown.choose(from: listed, incarnation: nil).pane, listed[1])
        shown.set(listed[1], incarnation: nil)
        choice = shown.choose(from: listed, incarnation: restarted)
        XCTAssertEqual(choice.pane, listed[1]); XCTAssertFalse(choice.restarted)
        // A record kept before runs were named reads as not saying.
        defaults.set(Data(#"{"paneId":1,"tabId":1,"windowId":0,"inputUnconfirmed":false}"#.utf8),
                     forKey: "frankenterm.mux.shownPane.w.mac")
        XCTAssertEqual(shown.record, .init(listed[1], incarnation: nil))
    }

    func testAnAttachedSessionsFailureReasonReachesTheScreenBounded() {
        let control = TerminalMuxControl()
        final class Seen: @unchecked Sendable { let lock = NSLock(); var failures: [String?] = [] }
        let seen = Seen()
        control.observe { state in seen.lock.withLock { seen.failures.append(state.failure) } }
        XCTAssertNil(control.current.failure)
        control.recordFailure("malformed mux frame: a pane size no terminal has")
        XCTAssertEqual(control.current.failure, "malformed mux frame: a pane size no terminal has")
        XCTAssertEqual(seen.lock.withLock { seen.failures.last }, "malformed mux frame: a pane size no terminal has")
        control.recordFailure(String(repeating: "x", count: 4096))
        XCTAssertEqual(control.current.failure, "The server reported an oversized error message.")
    }

    func testUnconfirmedInputIsStateTheScreenReadsNotANoticeAndIsToldOnce() {
        let control = TerminalMuxControl()
        final class Seen: @unchecked Sendable { let lock = NSLock(); var states: [TerminalMuxControl.State] = [] }
        let seen = Seen()
        control.observe { state in seen.lock.withLock { seen.states.append(state) } }
        let before = seen.lock.withLock { seen.states.count }
        XCTAssertFalse(control.current.inputUnconfirmed)
        control.recordInputUnconfirmed()
        control.recordInputUnconfirmed()
        XCTAssertTrue(control.current.inputUnconfirmed)
        XCTAssertNil(control.current.notice, "the screen's delivery notice says it, not a second notice")
        XCTAssertEqual(seen.lock.withLock { seen.states.count }, before + 1, "a repeat changes nothing")
    }
}

final class TerminalMuxNavigationRequestTests: XCTestCase {
    private func pane(_ id: Int) -> TerminalMuxPane {
        .init(paneId: id, tabId: 4, windowId: 2, tabTitle: "work", title: "pane-\(id)", workspace: "default",
              workingDir: nil, size: .init(rows: 24, cols: 80), isActive: id == 10, isZoomed: false)
    }
    private func fixture() throws -> TerminalMuxControl {
        let control = TerminalMuxControl()
        try control.inputRouting.replaceInventory([10, 20, 30])
        let selected = try control.inputRouting.select(pane: 10)
        control.inputRouting.didApplyOutput(selected)
        control.publish(panes: [10, 20, 30].map(pane), shown: 10)
        control.setVisible(true)
        return control
    }

    func testQueuedRequestKeepsExactCandidateAndDoesNotSelectItEarly() throws {
        let control = try fixture()
        let plan = try XCTUnwrap(control.inputRouting.navigation(forward: true))
        try control.request(plan)
        XCTAssertEqual(control.current.shown, 10)
        XCTAssertEqual(control.inputRouting.current.destination?.paneID, 10)
        XCTAssertTrue(control.inputRouting.current.ready)
        XCTAssertEqual(control.takeRequest(), 20)
        XCTAssertNil(control.takeRequest())
        XCTAssertNil(control.current.rejectedNavigation)
    }

    func testReorderAfterQueueRejectsInsteadOfAcquiringANewNeighbour() throws {
        let control = try fixture()
        let plan = try XCTUnwrap(control.inputRouting.navigation(forward: true))
        try control.request(plan)
        try control.inputRouting.replaceInventory([10, 30, 20])
        control.publish(panes: [10, 30, 20].map(pane), shown: 10)
        XCTAssertNil(control.takeRequest())
        XCTAssertEqual(control.current.rejectedNavigation, plan)
        XCTAssertEqual(control.current.shown, 10)
        try control.inputRouting.replaceInventory([10, 20, 30])
        XCTAssertThrowsError(try control.request(plan))
        XCTAssertNil(control.takeRequest())
    }

    func testRemovalRejectsExactWaitingRequestAndReuseDoesNotRestoreIt() throws {
        let control = try fixture()
        let plan = try control.inputRouting.navigation(pane: 20)
        try control.request(plan)
        control.inputRouting.remove(pane: 20); control.remove(pane: 20)
        XCTAssertEqual(control.current.rejectedNavigation, plan)
        XCTAssertNil(control.takeRequest())
        try control.inputRouting.replaceInventory([10, 20, 30])
        control.publish(panes: [10, 20, 30].map(pane), shown: 10)
        XCTAssertThrowsError(try control.request(plan))
        XCTAssertNil(control.takeRequest())
        let fresh = try control.inputRouting.navigation(pane: 20)
        try control.request(fresh)
        XCTAssertNil(control.current.rejectedNavigation)
        XCTAssertEqual(control.takeRequest(), 20)
    }

    func testSourceRoundTripBeforeTransportConsumeExpiresPlan() throws {
        let control = try fixture()
        let plan = try control.inputRouting.navigation(pane: 20)
        try control.request(plan)
        try control.inputRouting.select(pane: 30)
        try control.inputRouting.select(pane: 10)
        XCTAssertNil(control.takeRequest())
        XCTAssertEqual(control.current.rejectedNavigation, plan)
    }

    func testInvalidAndForeignRequestsCannotOverwriteAnAlreadyQueuedCandidate() throws {
        let control = try fixture(), other = try fixture()
        let intended = try control.inputRouting.navigation(pane: 20)
        try control.request(intended)
        let foreign = try other.inputRouting.navigation(pane: 30)
        XCTAssertThrowsError(try control.request(foreign))
        control.request(pane: -1)
        XCTAssertEqual(control.takeRequest(), 20)
        XCTAssertNil(control.current.rejectedNavigation)
    }

    func testHideRevokesQueuedGestureAndShowDoesNotReplayIt() throws {
        let control = try fixture()
        let plan = try control.inputRouting.navigation(pane: 20)
        try control.request(plan)
        control.setVisible(false)
        XCTAssertEqual(control.current.rejectedNavigation, plan)
        XCTAssertNil(control.takeRequest())
        XCTAssertThrowsError(try control.request(plan))
        control.setVisible(true)
        XCTAssertNil(control.takeRequest())
        try control.request(control.inputRouting.navigation(pane: 30))
        XCTAssertEqual(control.takeRequest(), 30)
    }

    func testConnectionEndRevokesPendingAndFutureRequests() throws {
        let control = try fixture()
        let plan = try control.inputRouting.navigation(pane: 20)
        try control.request(plan)
        control.endQueries()
        XCTAssertEqual(control.current.rejectedNavigation, plan)
        XCTAssertNil(control.takeRequest())
        XCTAssertThrowsError(try control.request(plan))
        control.request(pane: 30)
        XCTAssertNil(control.takeRequest())
    }

    func testReentrantObserverCanHideBeforeWakeWithoutDeadlockOrResurrection() throws {
        final class Count: @unchecked Sendable {
            let lock = NSLock()
            var calls = 0
        }
        let control = try fixture(), count = Count()
        let plan = try control.inputRouting.navigation(pane: 20)
        control.setWaker { count.lock.withLock { count.calls += 1 } }
        let revision = control.current.revision
        // The initial observation reads state only. A new queued publication
        // reenters hiding after the control released its critical section.
        control.observe { state in
            if state.revision != revision, state.visible { control.setVisible(false) }
        }
        try control.request(plan)
        XCTAssertFalse(control.current.visible)
        XCTAssertEqual(control.current.rejectedNavigation, plan)
        XCTAssertNil(control.takeRequest())
        XCTAssertGreaterThanOrEqual(count.lock.withLock { count.calls }, 1)
    }

    func testReentrantRejectionObserverCanQueueFreshPlanWithoutOldTakeErasingIt() throws {
        let control = try fixture()
        let old = try control.inputRouting.navigation(pane: 20)
        try control.request(old)
        try control.inputRouting.replaceInventory([10, 30, 20])
        let fresh = try control.inputRouting.navigation(pane: 30)
        control.observe { state in
            if state.rejectedNavigation == old { try? control.request(fresh) }
        }
        XCTAssertNil(control.takeRequest())
        XCTAssertEqual(control.takeRequest(), 30)
        XCTAssertNil(control.takeRequest())
        XCTAssertNil(control.current.rejectedNavigation)
    }

    func testMetadataRefreshDoesNotCancelCapturedCandidate() throws {
        let control = try fixture()
        let plan = try control.inputRouting.navigation(pane: 20)
        try control.request(plan)
        for index in 0..<100 {
            try control.inputRouting.replaceInventory([10, 20, 30])
            control.publish(panes: [10, 20, 30].map(pane), shown: 10)
            control.retitle(pane: 20, title: "new-title-\(index)")
            control.setMouseGrabbed(index % 2 == 0)
        }
        XCTAssertEqual(control.takeRequest(), 20)
        XCTAssertNil(control.current.rejectedNavigation)
    }

    func testQueuedNavigationCancelsOnlyUnsentPointers() throws {
        let control = try fixture(), source = try control.inputRouting.capture()
        control.setMouseGrabbed(true)
        try control.click(column: 1, row: 1, destination: source)
        XCTAssertEqual(control.inputRouting.pendingPointerCount, 1)
        let plan = try control.inputRouting.navigation(pane: 20)
        try control.request(plan)
        XCTAssertEqual(control.inputRouting.pendingPointerCount, 0)
        XCTAssertThrowsError(try control.click(column: 1, row: 1, destination: source))
        XCTAssertNoThrow(try control.inputRouting.validateDelivery(source))
        XCTAssertEqual(control.takeRequest(), 20)
    }

    func testTenThousandReplacementsRetainOnlyTheLatestExactTarget() throws {
        let control = try fixture()
        for index in 0..<10_000 {
            try control.request(control.inputRouting.navigation(pane: index % 2 == 0 ? 20 : 30))
        }
        XCTAssertEqual(control.takeRequest(), 30)
        XCTAssertNil(control.takeRequest())
        XCTAssertEqual(control.current.shown, 10)
    }

    func testOutputOffScreenIsUnreadUntilShownAndARedrawJustAfterLeavingIsNot() {
        let control = TerminalMuxControl(settle: .milliseconds(200))
        final class Flips: @unchecked Sendable { let lock = NSLock(); var values: [Bool] = [] }
        let flips = Flips()
        control.observeUnread { value in flips.lock.withLock { flips.values.append(value) } }
        control.publish(panes: [queryPane(1), queryPane(2), queryPane(3)], shown: 1)
        control.setVisible(true)
        control.noteActivity(pane: 1)
        XCTAssertEqual(control.current.unread, [], "the pane on screen is read as it comes")
        control.noteActivity(pane: 2)
        control.noteActivity(pane: 9)
        XCTAssertEqual(control.current.unread, [2], "a pane not listed is ignored")
        control.noteActivity(pane: 3)
        XCTAssertEqual(control.current.unread, [2, 3])

        // Pane 1 leaves the screen: its redraw at its own size is not news.
        control.publish(shown: 2)
        XCTAssertEqual(control.current.unread, [3], "the pane put on screen is read")
        control.noteActivity(pane: 1)
        XCTAssertEqual(control.current.unread, [3])
        Thread.sleep(forTimeInterval: 0.3)
        control.noteActivity(pane: 1)
        XCTAssertEqual(control.current.unread, [1, 3])

        // The machine leaves the screen: its shown pane settles the same way.
        control.setVisible(false)
        control.noteActivity(pane: 2)
        XCTAssertEqual(control.current.unread, [1, 3])
        Thread.sleep(forTimeInterval: 0.3)
        control.noteActivity(pane: 2)
        XCTAssertEqual(control.current.unread, [1, 2, 3])
        control.setVisible(true)
        XCTAssertEqual(control.current.unread, [1, 3], "back on screen, its shown pane is read")

        control.remove(pane: 3)
        XCTAssertEqual(control.current.unread, [1])
        control.publish(panes: [queryPane(1), queryPane(2)], shown: 1)
        XCTAssertEqual(control.current.unread, [])
        XCTAssertEqual(flips.lock.withLock { flips.values }, [false, true, false],
                       "the off-screen signal changes only when the first pane turns unread and the last is read")
    }
}
