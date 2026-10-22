import XCTest
@testable import FrankenTermKit

final class TerminalMuxControlTests: XCTestCase {
    private func pane(_ id: Int) -> TerminalMuxPane {
        .init(paneId: id, tabId: id, windowId: 0, tabTitle: "", title: "p\(id)", workspace: "default",
              workingDir: nil, size: .init(rows: 24, cols: 80), isActive: id == 1, isZoomed: false)
    }
    private func results(_ count: Int, error: String? = nil) -> TerminalMuxSearchResults {
        .init(search: 1, matches: (0..<count).map { _ in
            try! JSONDecoder().decode(TerminalMuxSearchMatch.self, from: Data(
                #"{"row":5,"startColumn":1,"endRow":5,"endColumn":4,"text":"a match"}"#.utf8))
        }, searchedRows: 100, error: error)
    }

    func testASearchFillsInPaneByPaneAndAReplacedOneIsIgnored() {
        let control = TerminalMuxControl()
        control.publish(panes: [pane(1), pane(2), pane(3)], shown: 1)
        final class Wakes: @unchecked Sendable { let lock = NSLock(); var count = 0 }
        let wakes = Wakes()
        control.setWaker { wakes.lock.withLock { wakes.count += 1 } }
        let query = TerminalMuxControl.SearchQuery(text: "needle", caseSensitive: false, regex: false)
        let first = control.search(query, panes: [1, 2, 3])
        XCTAssertEqual(wakes.lock.withLock { wakes.count }, 1, "the transport is woken to send it")
        XCTAssertEqual(control.takeSearch()?.panes, [1, 2, 3])
        XCTAssertNil(control.takeSearch(), "a search is sent once")

        control.deliver(search: first, pane: 2, results: results(2))
        XCTAssertEqual(control.current.search?.pending, [1, 3])
        XCTAssertEqual(control.current.search?.results[2]?.matches.count, 2)

        // A pane that closes stops being waited for.
        control.remove(pane: 3)
        XCTAssertEqual(control.current.search?.pending, [1])

        // A newer search replaces it; the old one's late answers are dropped.
        let second = control.search(query, panes: [1])
        control.deliver(search: first, pane: 1, results: results(9))
        XCTAssertEqual(control.current.search?.id, second)
        XCTAssertNil(control.current.search?.results[1])
        control.deliver(search: second, pane: 1, results: results(0, error: "regex parse error"))
        XCTAssertEqual(control.current.search?.pending, [])
        XCTAssertEqual(control.current.search?.results[1]?.error, "regex parse error")

        control.clearSearch()
        XCTAssertNil(control.current.search)
        XCTAssertNil(control.takeSearch())
    }

    func testContextIsReadOnceAndOnlyItsOwnAnswerLands() throws {
        let control = TerminalMuxControl()
        control.readContext(pane: 4, row: 100, before: 2, after: 3)
        let request = try XCTUnwrap(control.takeContext())
        XCTAssertEqual(request.pane, 4)
        XCTAssertEqual(request.first, 98)
        XCTAssertEqual(request.count, 6)
        XCTAssertNil(control.takeContext())
        XCTAssertNil(control.current.context?.lines)

        control.deliver(context: UUID(), lines: [])
        XCTAssertNil(control.current.context?.lines, "another request's rows are ignored")
        let line = try JSONDecoder().decode(TerminalMuxContextLine.self, from: Data(#"{"row":100,"text":"hit"}"#.utf8))
        control.deliver(context: request.id, lines: [line])
        XCTAssertEqual(control.current.context?.lines, [line])
    }
}
