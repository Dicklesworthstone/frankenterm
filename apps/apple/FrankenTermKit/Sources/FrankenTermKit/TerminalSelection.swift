import Foundation

/// A cell by stable row, so a selection stays on the same text while output
/// scrolls it up and while earlier history is shown.
public struct TerminalGridPoint: Comparable, Hashable, Sendable {
    public let row: Int64, column: Int
    public init(row: Int64, column: Int) { self.row = row; self.column = column }
    public static func < (a: Self, b: Self) -> Bool { (a.row, a.column) < (b.row, b.column) }
}

/// Selected cells from `start` through `end` in reading order: the rest of
/// the first row, every row between, and the last row up to `end`.
public struct TerminalTextSelection: Equatable, Sendable {
    public let start: TerminalGridPoint, end: TerminalGridPoint
    /// The most rows one selection spans; longer drags stop growing.
    public static let maximumRows: Int64 = 4096

    public init(_ a: TerminalGridPoint, _ b: TerminalGridPoint) {
        start = min(a, b); end = max(a, b)
    }

    /// The columns of `row` this selection covers in a grid `columns` wide.
    public func columns(onRow row: Int64, width columns: Int) -> ClosedRange<Int>? {
        guard columns > 0, row >= start.row, row <= end.row else { return nil }
        let first = max(0, row == start.row ? start.column : 0)
        let last = min(columns - 1, row == end.row ? end.column : columns - 1)
        return first <= last ? first...last : nil
    }

    /// Characters that end a word: a double-tap or long press on a path, URL,
    /// flag or identifier selects all of it, but not the quotes around it.
    static let delimiters: Set<Character> = [" ", "\t", "\"", "'", "`", "(", ")", "[", "]", "{", "}",
                                              "<", ">", "|", ";", ","]

    /// The word around `column`: the run of cells that are neither blank nor a
    /// delimiter. On a blank or a delimiter, just that cell.
    public static func word(at column: Int, in row: TerminalRenderRow, columns: Int) -> TerminalTextSelection {
        let owners = cellOwners(row, columns: columns)
        let clamped = min(max(column, 0), max(columns - 1, 0))
        func isWord(_ index: Int) -> Bool {
            guard owners.indices.contains(index), let owner = owners[index] else { return false }
            let cell = row.cells[owner]
            return !cell.text.isEmpty && !cell.invisible && !cell.text.allSatisfy { delimiters.contains($0) }
        }
        let point = { (column: Int) in TerminalGridPoint(row: row.stableIndex, column: column) }
        guard isWord(clamped) else {
            // A wide character occupies both of its columns.
            let span = owners.indices.contains(clamped) ? owners[clamped].map { row.cells[$0] } : nil
            return .init(point(span?.column ?? clamped), point((span.map { $0.column + $0.width } ?? clamped + 1) - 1))
        }
        var first = clamped, last = clamped
        while isWord(first - 1) { first -= 1 }
        while isWord(last + 1) { last += 1 }
        return .init(point(first), point(last))
    }

    /// Which cell (by index into `row.cells`) draws each column; nil for gaps.
    static func cellOwners(_ row: TerminalRenderRow, columns: Int) -> [Int?] {
        var owners = [Int?](repeating: nil, count: max(columns, 0))
        for (index, cell) in row.cells.enumerated() where cell.column >= 0 && cell.column < owners.count {
            for column in cell.column..<min(cell.column + max(cell.width, 1), owners.count) { owners[column] = index }
        }
        return owners
    }

    /// Canonical soft wraps join only adjacent, complete selected boundaries
    /// from the same source/screen/geometry capture. Hard breaks, absent rows
    /// and unknown metadata retain a newline. Trailing blanks at a joined wrap
    /// are meaningful text; other row ends are trimmed as before. A partly
    /// selected wide grapheme is included whole; concealed cells stay masked.
    /// Invalid or oversized snapshots return no text, never a partial command.
    public func text(rows: [Int64: TerminalRenderRow], columns: Int) -> String {
        guard (1...512).contains(columns) else { return "" }
        let distance = end.row.subtractingReportingOverflow(start.row)
        guard !distance.overflow, distance.partialValue < Self.maximumRows else { return "" }
        let maximumBytes = 1_048_576
        var result = "", usedBytes = 0
        var previous: (row: TerminalRenderRow, range: ClosedRange<Int>)?
        for offset in 0..<Self.maximumRows {
            let step = start.row.addingReportingOverflow(offset)
            guard !step.overflow, step.partialValue <= end.row else { break }
            let row = step.partialValue
            guard let range = self.columns(onRow: row, width: columns), let source = rows[row],
                  source.stableIndex == row else { continue }
            guard source.cells.count <= columns else { return "" }
            if let previous, !(previous.range.upperBound == columns - 1 && range.lowerBound == 0
                && previous.row.joins(source, columns: columns)) {
                guard usedBytes < maximumBytes else { return "" }
                result.append("\n"); usedBytes += 1
            }
            var line = "", column = range.lowerBound, lineBytes = 0
            for cell in source.cells.sorted(by: { $0.column < $1.column }) {
                guard cell.column >= 0, cell.column < columns, (1...2).contains(cell.width),
                      cell.width <= columns - cell.column, cell.text.utf8.count <= 4096 else { return "" }
                let cellEnd = cell.column + cell.width - 1
                guard cellEnd >= range.lowerBound, cell.column <= range.upperBound else { continue }
                let gap = max(0, cell.column - column)
                let visible = cell.invisible || cell.text.isEmpty
                    ? String(repeating: " ", count: cell.width)
                    : String(String.UnicodeScalarView(cell.text.unicodeScalars.filter {
                        $0.value >= 32 && !(127...159).contains($0.value)
                    }))
                let count = visible.utf8.count
                guard gap <= maximumBytes - usedBytes - lineBytes,
                      count <= maximumBytes - usedBytes - lineBytes - gap else { return "" }
                line.append(String(repeating: " ", count: gap)); line.append(visible)
                lineBytes += gap + count; column = cell.column + cell.width
            }
            // Preserve spaces before a proven continuation, but never join a
            // wrap whose next row is outside this selection or its row budget.
            let next = row.addingReportingOverflow(1)
            let joinsNext = offset + 1 < Self.maximumRows && !next.overflow && next.partialValue <= end.row
                && range.upperBound == columns - 1
                && self.columns(onRow: next.partialValue, width: columns)?.lowerBound == 0
                && rows[next.partialValue].map { source.joins($0, columns: columns) } == true
            if !joinsNext { while line.last == " " { line.removeLast() } }
            result.append(line); usedBytes += line.utf8.count
            previous = (source, range)
        }
        return result
    }
}
