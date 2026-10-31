import XCTest
@testable import FrankenTermKit

final class TerminalLocalViewportTests: XCTestCase {
    func testWideDesktopGridKeepsReadableCellsWhileEveryCornerIsReachable() throws {
        let first = try TerminalLocalViewport(columns: 240, rows: 100, cellWidth: 9, cellHeight: 18,
            width: 390, height: 612)
        XCTAssertEqual(first.cellWidth, 9); XCTAssertEqual(first.cellHeight, 18)
        XCTAssertEqual(first.visibleColumns, 0..<44); XCTAssertEqual(first.visibleRows, 0..<34)
        let far = try XCTUnwrap(first.panning(byX: -100_000, y: -100_000))
        let last = try TerminalLocalViewport(columns: first.columns, rows: first.rows,
            cellWidth: first.cellWidth, cellHeight: first.cellHeight, width: first.width, height: first.height,
            requestedOffset: far)
        XCTAssertEqual(last.visibleColumns.upperBound, 240); XCTAssertEqual(last.visibleRows.upperBound, 100)
        XCTAssertEqual(last.originX + 240 * 9, 390, accuracy: 0.000001)
        XCTAssertEqual(last.originY + 100 * 18, 612, accuracy: 0.000001)
        XCTAssertEqual(last.cell(atX: 389, y: 611), .init(column: 239, row: 99))
        XCTAssertEqual(last.panning(byX: 100_000, y: 100_000), .zero)
    }

    func testCellHitTestsAndSelectionRectanglesRoundTripThroughFractionalOffsets() throws {
        for offset in [TerminalLocalViewport.Offset.zero, .init(columns: 8.25, rows: 3.5), .init(columns: 1000, rows: 1000)] {
            let view = try TerminalLocalViewport(columns: 120, rows: 80, cellWidth: 7.5, cellHeight: 16.5,
                width: 390, height: 480, requestedOffset: offset)
            for row in view.visibleRows {
                for column in view.visibleColumns {
                    let full = try XCTUnwrap(view.rectangle(column: column, row: row))
                    guard let visible = view.clipped(full) else { continue }
                    XCTAssertEqual(view.cell(atX: visible.x + visible.width / 2, y: visible.y + visible.height / 2),
                                   .init(column: column, row: row))
                }
            }
        }
    }

    func testPaddingIsNotARealCellAndSelectionClampingIsExplicit() throws {
        let view = try TerminalLocalViewport(columns: 20, rows: 10, cellWidth: 8, cellHeight: 16, width: 390, height: 600,
            requestedOffset: .init(columns: 200, rows: 200))
        XCTAssertEqual(view.offset, .zero)
        XCTAssertNil(view.cell(atX: 161, y: 10)); XCTAssertNil(view.cell(atX: 10, y: 161))
        XCTAssertNil(view.cell(atX: -1, y: 0)); XCTAssertNil(view.cell(atX: 0, y: 600))
        XCTAssertEqual(view.cell(atX: -.greatestFiniteMagnitude, y: .greatestFiniteMagnitude, clamping: true),
                       .init(column: 0, row: 9))
        XCTAssertNil(view.cell(atX: .infinity, y: 2, clamping: true))
    }

    func testWindowAndFontChangesClampTheSameCellAnchorWithoutInventingNewRows() throws {
        let anchor = TerminalLocalViewport.Offset(columns: 75, rows: 60)
        let phone = try TerminalLocalViewport(columns: 120, rows: 80, cellWidth: 8, cellHeight: 16,
            width: 320, height: 320, requestedOffset: anchor)
        XCTAssertEqual(phone.offset, anchor)
        let largerText = try TerminalLocalViewport(columns: 120, rows: 80, cellWidth: 12, cellHeight: 24,
            width: 320, height: 320, requestedOffset: phone.offset)
        XCTAssertEqual(largerText.offset, anchor)
        let wideWindow = try TerminalLocalViewport(columns: 120, rows: 80, cellWidth: 8, cellHeight: 16,
            width: 960, height: 1280, requestedOffset: largerText.offset)
        XCTAssertEqual(wideWindow.offset, .zero)
        XCTAssertEqual(wideWindow.visibleColumns, 0..<120); XCTAssertEqual(wideWindow.visibleRows, 0..<80)
        let smallerGrid = try TerminalLocalViewport(columns: 80, rows: 24, cellWidth: 8, cellHeight: 16,
            width: 320, height: 320, requestedOffset: anchor)
        XCTAssertEqual(smallerGrid.offset, .init(columns: 40, rows: 4))
    }

    func testRevealCursorMovesMinimallyAndKeepsWideGlyphTogether() throws {
        let view = try TerminalLocalViewport(columns: 100, rows: 50, cellWidth: 10, cellHeight: 20,
            width: 400, height: 400, requestedOffset: .init(columns: 20, rows: 10))
        XCTAssertEqual(view.revealing(column: 30, row: 15), view.offset)
        XCTAssertEqual(view.revealing(column: 60, row: 30, cellWidth: 2), .init(columns: 22, rows: 11))
        XCTAssertEqual(view.revealing(column: 0, row: 0), .zero)
        XCTAssertEqual(view.revealing(column: 99, row: 49), .init(columns: 60, rows: 30))
        XCTAssertNil(view.revealing(column: 99, row: 49, cellWidth: 2))
        XCTAssertNil(view.revealing(column: 100, row: 0))
    }

    func testAdmissionIsBoundedAndRejectsNonfiniteGeometry() throws {
        for invalid in [Double.nan, .infinity, -.infinity, .leastNonzeroMagnitude, 0, -1] {
            XCTAssertThrowsError(try TerminalLocalViewport(columns: 80, rows: 24, cellWidth: invalid,
                cellHeight: 16, width: 390, height: 600))
            XCTAssertThrowsError(try TerminalLocalViewport(columns: 80, rows: 24, cellWidth: 8,
                cellHeight: 16, width: invalid, height: 600))
        }
        XCTAssertThrowsError(try TerminalLocalViewport(columns: 512, rows: 512,
            cellWidth: 8, cellHeight: 16, width: 390, height: 600))
        XCTAssertThrowsError(try TerminalLocalViewport(columns: 80, rows: 24,
            cellWidth: 8, cellHeight: 16, width: 390, height: 600, requestedOffset: .init(columns: .nan)))
        let tall = try TerminalLocalViewport(columns: 128, rows: 512, cellWidth: 256, cellHeight: 512,
            width: 390, height: 600, requestedOffset: .init(columns: 512, rows: 512))
        XCTAssertEqual(tall.visibleRows.upperBound, 512)
        XCTAssertEqual(tall.cell(atX: 389, y: 599), .init(column: 127, row: 511))
        XCTAssertNil(tall.panning(byX: .infinity, y: 0))
    }

    func testExistingShaderOriginCanReachTheLastRowWithoutAnOversizedDrawable() throws {
        let view = try TerminalLocalViewport(columns: 128, rows: 512, cellWidth: 256, cellHeight: 512,
            width: 390, height: 600, requestedOffset: .init(columns: 512, rows: 512))
        var uniforms = TerminalRenderUniforms()
        uniforms.rows = 512; uniforms.columns = 128; uniforms.viewport = (390, 600)
        uniforms.cellSize = (256, 512); uniforms.origin = (Float(view.originX), Float(view.originY))
        let packed = try TerminalRenderPacking.uniforms(uniforms)
        let bits = packed[60..<64].enumerated().reduce(UInt32(0)) { $0 | UInt32($1.element) << ($1.offset * 8) }
        XCTAssertEqual(Float(bitPattern: bits), -261_544)
        uniforms.origin = (0, -262_145)
        XCTAssertThrowsError(try TerminalRenderPacking.uniforms(uniforms))
        uniforms.origin = (.nan, 0)
        XCTAssertThrowsError(try TerminalRenderPacking.uniforms(uniforms))
        uniforms.origin = (0, 0); uniforms.cursorThickness = 32769
        XCTAssertThrowsError(try TerminalRenderPacking.uniforms(uniforms))
    }
}
