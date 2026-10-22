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

    /// The selected text as it reads on screen: each row's covered cells, with
    /// gaps and concealed (SGR 8) cells as spaces and trailing blanks dropped,
    /// rows joined by newlines. A wide character partly covered is included
    /// whole. Rows not in `rows` (never shown while selecting) are skipped.
    public func text(rows: [Int64: TerminalRenderRow], columns: Int) -> String {
        guard columns > 0 else { return "" }
        var lines: [String] = []
        var row = start.row
        let last = min(end.row, start.row + Self.maximumRows - 1)
        while row <= last {
            defer { row += 1 }
            guard let range = self.columns(onRow: row, width: columns), let source = rows[row] else { continue }
            var line = "", column = range.lowerBound
            for cell in source.cells.sorted(by: { $0.column < $1.column }) {
                let cellEnd = cell.column + max(cell.width, 1) - 1
                guard cellEnd >= range.lowerBound, cell.column <= range.upperBound else { continue }
                if cell.column > column { line.append(String(repeating: " ", count: cell.column - column)) }
                let visible = cell.invisible || cell.text.isEmpty
                    ? String(repeating: " ", count: max(cell.width, 1))
                    : String(String.UnicodeScalarView(cell.text.unicodeScalars.filter {
                        $0.value >= 32 && !(127...159).contains($0.value)
                    }))
                line.append(visible)
                column = cell.column + max(cell.width, 1)
            }
            while line.last == " " { line.removeLast() }
            lines.append(line)
        }
        return lines.joined(separator: "\n")
    }
}
