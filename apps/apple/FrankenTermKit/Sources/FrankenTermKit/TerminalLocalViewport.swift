import Foundation

/// A clipped local view of an unchanged terminal grid. Coordinates use one
/// caller-selected unit (pixels for Metal, points for UIKit). Offsets use cells
/// so a local font change or window resize can preserve the place being read.
/// This value cannot request terminal history, send input, or resize a PTY.
public struct TerminalLocalViewport: Equatable, Sendable {
    public struct Offset: Equatable, Sendable {
        public var columns: Double, rows: Double
        public init(columns: Double = 0, rows: Double = 0) { self.columns = columns; self.rows = rows }
        public static let zero = Offset()
    }

    public struct Cell: Equatable, Sendable {
        public let column: Int, row: Int
    }

    public struct Rectangle: Equatable, Sendable {
        public let x: Double, y: Double, width: Double, height: Double
        public var maxX: Double { x + width }
        public var maxY: Double { y + height }
    }

    public let columns: Int, rows: Int
    public let cellWidth: Double, cellHeight: Double
    public let width: Double, height: Double
    public let offset: Offset
    public var originX: Double { -offset.columns * cellWidth }
    public var originY: Double { -offset.rows * cellHeight }
    public var maximumOffset: Offset {
        .init(columns: max(0, Double(columns) - width / cellWidth),
              rows: max(0, Double(rows) - height / cellHeight))
    }

    public init(columns: Int, rows: Int, cellWidth: Double, cellHeight: Double,
                width: Double, height: Double, requestedOffset: Offset = .zero) throws {
        guard (1...512).contains(columns), (1...512).contains(rows),
              columns * rows <= TerminalSessionGeometry.maximumShownCells,
              cellWidth.isFinite, cellHeight.isFinite, cellWidth >= 0.125, cellHeight >= 0.125,
              cellWidth <= 256, cellHeight <= 512,
              width.isFinite, height.isFinite, width >= 0.125, height >= 0.125, width <= 32768, height <= 32768,
              requestedOffset.columns.isFinite, requestedOffset.rows.isFinite else {
            throw TerminalRenderError.capacity
        }
        self.columns = columns; self.rows = rows; self.cellWidth = cellWidth; self.cellHeight = cellHeight
        self.width = width; self.height = height
        offset = .init(columns: min(max(0, requestedOffset.columns), max(0, Double(columns) - width / cellWidth)),
                       rows: min(max(0, requestedOffset.rows), max(0, Double(rows) - height / cellHeight)))
    }

    /// Pointer hit tests refuse padding and points outside the drawable.
    /// Selection drags may explicitly clamp to the original grid's boundary.
    public func cell(atX x: Double, y: Double, clamping: Bool = false) -> Cell? {
        guard x.isFinite, y.isFinite else { return nil }
        let column = (x - originX) / cellWidth, row = (y - originY) / cellHeight
        if !clamping {
            guard x >= 0, y >= 0, x < width, y < height,
                  column >= 0, row >= 0, column < Double(columns), row < Double(rows) else { return nil }
        }
        // Clamp before conversion, including extreme but finite drag values.
        return Cell(column: Int(min(max(0, column), Double(columns - 1)).rounded(.down)),
                    row: Int(min(max(0, row), Double(rows - 1)).rounded(.down)))
    }

    public func rectangle(column: Int, row: Int, columns count: Int = 1, rows rowCount: Int = 1) -> Rectangle? {
        guard column >= 0, row >= 0, count > 0, rowCount > 0,
              column < columns, row < rows, count <= columns - column, rowCount <= rows - row else { return nil }
        return .init(x: originX + Double(column) * cellWidth, y: originY + Double(row) * cellHeight,
                     width: Double(count) * cellWidth, height: Double(rowCount) * cellHeight)
    }

    public func clipped(_ rectangle: Rectangle) -> Rectangle? {
        let x = max(0, rectangle.x), y = max(0, rectangle.y)
        let right = min(width, rectangle.maxX), bottom = min(height, rectangle.maxY)
        guard right > x, bottom > y else { return nil }
        return .init(x: x, y: y, width: right - x, height: bottom - y)
    }

    public var visibleColumns: Range<Int> {
        Int(offset.columns.rounded(.down))..<min(columns, Int((offset.columns + width / cellWidth).rounded(.up)))
    }
    public var visibleRows: Range<Int> {
        Int(offset.rows.rounded(.down))..<min(rows, Int((offset.rows + height / cellHeight).rounded(.up)))
    }

    /// A finger moving right/down reveals cells to the left/above. This never
    /// interprets the motion as a remote wheel or a change of machine/pane.
    public func panning(byX x: Double, y: Double) -> Offset? {
        guard x.isFinite, y.isFinite else { return nil }
        let maximum = maximumOffset
        return .init(columns: min(max(0, offset.columns - x / cellWidth), maximum.columns),
                     rows: min(max(0, offset.rows - y / cellHeight), maximum.rows))
    }

    /// Move only enough to reveal the existing cursor/cell. The whole grid is
    /// unchanged; a two-cell glyph stays visible together when it can fit.
    public func revealing(column: Int, row: Int, cellWidth span: Int = 1) -> Offset? {
        guard column >= 0, row >= 0, column < columns, row < rows,
              (1...2).contains(span), span <= columns - column else { return nil }
        func reveal(_ start: Double, _ extent: Double, _ current: Double, _ visible: Double, _ maximum: Double) -> Double {
            let requested = extent > visible || start < current ? start
                : start + extent > current + visible ? start + extent - visible : current
            return min(max(0, requested), maximum)
        }
        let maximum = maximumOffset
        return .init(columns: reveal(Double(column), Double(span), offset.columns, width / cellWidth, maximum.columns),
                     rows: reveal(Double(row), 1, offset.rows, height / cellHeight, maximum.rows))
    }
}
