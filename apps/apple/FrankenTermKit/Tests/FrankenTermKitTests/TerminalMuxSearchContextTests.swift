import XCTest
@testable import FrankenTermKit

final class TerminalMuxSearchContextTests: XCTestCase {
    func testOnlyTheCompleteOriginalRowsCanIdentifyTheMatch() {
        let match = TerminalMuxSearchMatch(row: 9, startColumn: 1, endRow: 11, endColumn: 2,
                                           text: "α\n\n終")
        let rows: [TerminalMuxContextLine] = [
            .init(row: 8, text: "before"), .init(row: 9, text: "α"),
            .init(row: 10, text: ""), .init(row: 11, text: "終"), .init(row: 12, text: "after")
        ]
        XCTAssertTrue(match.isPresent(in: rows))
        XCTAssertFalse(match.isPresent(in: Array(rows.dropLast(2))))
        XCTAssertFalse(match.isPresent(in: rows.filter { $0.row != 10 }))
        XCTAssertFalse(match.isPresent(in: rows.map { $0.row == 11 ? .init(row: 11, text: "changed") : $0 }))
    }

    func testReflowOrDifferentCellTextAtTheSameCoordinatesCannotReplaceCapturedOutput() {
        let match = TerminalMuxSearchMatch(row: 42, startColumn: 0, endRow: 42, endColumn: 1, text: "é")
        XCTAssertTrue(match.isPresent(in: [.init(row: 42, text: "é")]))
        XCTAssertFalse(match.isPresent(in: [.init(row: 41, text: "é")]))
        XCTAssertFalse(match.isPresent(in: [.init(row: 42, text: "e\u{301}")]))
        XCTAssertFalse(match.isPresent(in: []))
    }
}
