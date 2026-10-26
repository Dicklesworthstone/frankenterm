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

/// A link under a cell: one the program attached to its text (OSC 8), or a
/// `scheme://` address written in it, found by the desktop's default rules.
/// Its destination is shown before anything opens; only web and mail
/// addresses open, others (files, settings, other apps) can be copied.
public struct TerminalLink: Equatable, Sendable {
    public let destination: String
    /// The first and last cells it covers.
    public let start: TerminalGridPoint, end: TerminalGridPoint

    public var openableURL: URL? {
        guard let url = URL(string: destination, encodingInvalidCharacters: true),
              let scheme = url.scheme?.lowercased(), ["http", "https", "mailto"].contains(scheme) else { return nil }
        return url
    }

    /// The desktop's default rules (frankenterm-config `default_hyperlink_rules`)
    /// with the capture each one links, minus the bare e-mail rule: a prompt's
    /// user@host would take the tap that brings up the keyboard.
    private static let rules: [(NSRegularExpression, Int)] = [
        (#"\((\w+://\S+)\)"#, 1), (#"\[(\w+://\S+)\]"#, 1), (#"<(\w+://\S+)>"#, 1),
        (#"\b\w+://[^\s()]*\(\S*\)(?=\s|$|[^_/a-zA-Z0-9-])"#, 0), (#"\b\w+://\S+[_/a-zA-Z0-9-]"#, 0),
    ].map { (try! NSRegularExpression(pattern: $0.0), $0.1) }
    /// Rows joined on each side of the tapped one when looking for an address.
    static let maximumJoinedRows = 8

    /// The link at `point`, if any. An address runs on into the next row
    /// where the terminal recorded the row's wrap; a row that only fills its
    /// width ends it.
    public static func at(_ point: TerminalGridPoint, in frame: TerminalRenderFrame) -> TerminalLink? {
        let columns = frame.columns
        guard columns > 0, let index = frame.lines.firstIndex(where: { $0.stableIndex == point.row }) else { return nil }
        let row = frame.lines[index]
        let owners = TerminalTextSelection.cellOwners(row, columns: columns)
        guard owners.indices.contains(point.column) else { return nil }
        if let owner = owners[point.column], row.cells[owner].linkID > 0,
           frame.links.indices.contains(row.cells[owner].linkID - 1) {
            let id = row.cells[owner].linkID
            var first = point.column, last = point.column
            func linked(_ column: Int) -> Bool {
                owners.indices.contains(column) && owners[column].map { row.cells[$0].linkID == id } == true
            }
            while linked(first - 1) { first -= 1 }
            while linked(last + 1) { last += 1 }
            return TerminalLink(destination: frame.links[id - 1],
                                start: .init(row: row.stableIndex, column: first),
                                end: .init(row: row.stableIndex, column: last))
        }
        func continues(_ at: Int) -> Bool {
            at + 1 < frame.lines.count && frame.lines[at].wrapsToNext
                && frame.lines[at + 1].stableIndex == frame.lines[at].stableIndex + 1
        }
        var first = index, last = index
        while first > 0, index - first < maximumJoinedRows, continues(first - 1) { first -= 1 }
        while last - index < maximumJoinedRows, continues(last) { last += 1 }
        // The joined text, with the cell each UTF-16 unit came from.
        var text = "", cells = [TerminalGridPoint]()
        for line in frame.lines[first...last] {
            var column = 0
            for cell in line.cells where cell.column >= column && cell.column < columns {
                let gap = cell.column - column
                text += String(repeating: " ", count: gap)
                cells += (column..<cell.column).map { TerminalGridPoint(row: line.stableIndex, column: $0) }
                let shown = cell.invisible || cell.text.isEmpty ? " " : cell.text
                text += shown
                cells += repeatElement(TerminalGridPoint(row: line.stableIndex, column: cell.column), count: shown.utf16.count)
                column = cell.column + max(cell.width, 1)
            }
        }
        let tapped = TerminalGridPoint(row: point.row, column: owners[point.column].map { row.cells[$0].column } ?? point.column)
        guard let at = cells.firstIndex(of: tapped) else { return nil }
        let whole = NSRange(location: 0, length: (text as NSString).length)
        for (rule, capture) in rules {
            for match in rule.matches(in: text, range: whole) {
                let range = match.range(at: capture)
                guard range.location != NSNotFound, NSLocationInRange(at, range) else { continue }
                return TerminalLink(destination: (text as NSString).substring(with: range),
                                    start: cells[range.location], end: cells[NSMaxRange(range) - 1])
            }
        }
        return nil
    }
}
