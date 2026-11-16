import Foundation

/// Local display arithmetic only. It never proposes a different terminal grid.
/// Whole-pixel ceilings keep every cell inside the drawable after rasterization.
public struct TerminalGridFit: Equatable, Sendable {
    public let scale: Double
    public let maximumCellWidth: Double, maximumCellHeight: Double

    public init(columns: Int, rows: Int, cellWidth: Double, cellHeight: Double,
                pixelWidth: Int, pixelHeight: Int) throws {
        guard (1...512).contains(columns), (1...512).contains(rows),
              columns * rows <= TerminalSessionGeometry.maximumShownCells,
              (1...32768).contains(pixelWidth), (1...32768).contains(pixelHeight),
              cellWidth.isFinite, cellHeight.isFinite, cellWidth > 0, cellHeight > 0,
              cellWidth <= 256, cellHeight <= 512,
              pixelWidth >= columns, pixelHeight >= rows else { throw TerminalRenderError.capacity }
        maximumCellWidth = floor(Double(pixelWidth) / Double(columns))
        maximumCellHeight = floor(Double(pixelHeight) / Double(rows))
        scale = min(1, maximumCellWidth / ceil(cellWidth), maximumCellHeight / ceil(cellHeight))
    }

    public func contains(cellWidth: Double, cellHeight: Double) -> Bool {
        cellWidth.isFinite && cellHeight.isFinite && cellWidth >= 1 && cellHeight >= 1
            && cellWidth <= maximumCellWidth && cellHeight <= maximumCellHeight
    }
}
