import XCTest
@testable import FrankenTermKit

final class TerminalMuxSearchMatchTests: XCTestCase {
    private func match(_ json: String) throws -> TerminalMuxSearchMatch {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(TerminalMuxSearchMatch.self, from: Data(json.utf8))
    }

    func testTheClientsOffsetsMarkTheMatchAfterWideCharacters() throws {
        // "界界ab": the match "ab" starts at cell 4 but UTF-8 byte 6.
        let hit = try match(#"{"row":7,"start_column":4,"end_row":7,"end_column":6,"text":"界界ab\nnext","text_start":6,"text_end":8}"#)
        let range = try XCTUnwrap(hit.matchedRange)
        XCTAssertEqual(hit.text[range], "ab")
        XCTAssertEqual(String(MuxHighlightProbe.characters(of: hit)), "界界ab")
    }

    func testOffsetsMissingOrOffACharacterBoundaryAreNotTrusted() throws {
        XCTAssertNil(try match(#"{"row":1,"start_column":0,"end_row":1,"end_column":1,"text":"x"}"#).matchedRange)
        // Byte 1 is inside "界".
        XCTAssertNil(try match(#"{"row":1,"start_column":0,"end_row":1,"end_column":2,"text":"界","text_start":1,"text_end":3}"#).matchedRange)
        XCTAssertNil(try match(#"{"row":1,"start_column":0,"end_row":1,"end_column":2,"text":"ab","text_start":1,"text_end":9}"#).matchedRange)
    }
}

/// The first row of a match's text, as the result list shows it.
private enum MuxHighlightProbe {
    static func characters(of match: TerminalMuxSearchMatch) -> Substring {
        match.text.split(separator: "\n", omittingEmptySubsequences: false).first ?? ""
    }
}
