import Foundation
import XCTest
@testable import FrankenTermTerminal

final class NativeMuxClientValidationTests: XCTestCase {
    func testNegativePaneIdentitiesThrowBeforeUnsignedConversion() throws {
        let client = try NativeMuxClient()
        for pane in [-1, Int.min] {
            XCTAssertThrowsError(try client.show(pane: pane))
            XCTAssertThrowsError(try client.hide(pane: pane))
            XCTAssertThrowsError(try client.write(pane: pane, bytes: Data([1])))
            XCTAssertThrowsError(try client.paste(pane: pane, text: "text"))
            XCTAssertThrowsError(try client.resize(pane: pane, rows: 24, columns: 80))
            XCTAssertThrowsError(try client.restoreSize(pane: pane))
            XCTAssertThrowsError(try client.search(pane: pane, text: "needle", caseSensitive: false, regex: false))
            XCTAssertThrowsError(try client.context(pane: pane, firstRow: 0, count: 1))
        }
    }

    func testContextRangeOverflowAndInvalidCountsAreRejected() throws {
        let client = try NativeMuxClient()
        for count in [Int.min, -1, 0, 1001, Int.max] {
            XCTAssertThrowsError(try client.context(pane: 1, firstRow: 0, count: count))
        }
        XCTAssertThrowsError(try client.context(pane: 1, firstRow: Int64.max, count: 1))
        XCTAssertThrowsError(try client.context(pane: 1, firstRow: Int64.max - 5, count: 6))
    }

    func testUTF8LimitsApplyBeforeAllocatingPayloadOrIssuingRequests() throws {
        let client = try NativeMuxClient()
        XCTAssertThrowsError(try client.paste(pane: 1, text: String(repeating: "🙂", count: 16_385)))
        XCTAssertThrowsError(try client.write(pane: 1, bytes: Data(count: 65_537)))
        XCTAssertThrowsError(try client.search(pane: 1, text: String(repeating: "🙂", count: 1025), caseSensitive: false, regex: false))
        XCTAssertThrowsError(try client.search(pane: 1, text: "", caseSensitive: false, regex: false))
    }
}
