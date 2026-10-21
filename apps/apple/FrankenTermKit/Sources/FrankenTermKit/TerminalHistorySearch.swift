import Foundation

public enum TerminalHistorySearchError: Error, Equatable, Sendable, LocalizedError {
    case invalidQuery, invalidPage, capacity, unavailable, staleResult, busy
    public var errorDescription: String? {
        switch self {
        case .invalidQuery: "Enter up to 1,024 UTF-8 bytes of text on one line. Search is literal and case-sensitive."
        case .invalidPage: "The terminal supplied an invalid history page."
        case .capacity: "The local history search limit was reached."
        case .unavailable: "This history is no longer available. Search again."
        case .staleResult: "The matched output changed or expired. Search again; nothing was sent to the terminal."
        case .busy: "Another history search is still finishing."
        }
    }
}

/// Semantic cells supplied by the canonical terminal, never parsed ANSI or
/// inferred Unicode widths. Concealed text is discarded at this boundary.
public struct TerminalSearchCell: Sendable, CustomDebugStringConvertible {
    public let column: Int, width: Int, text: String, concealed: Bool
    public var debugDescription: String { "TerminalSearchCell([redacted])" }
    public init(column: Int, width: Int, text: String, concealed: Bool = false) throws {
        guard (0..<512).contains(column), (1...2).contains(width) else { throw TerminalHistorySearchError.invalidPage }
        guard text.utf8.count <= 4096 else { throw TerminalHistorySearchError.capacity }
        guard !text.unicodeScalars.contains(where: { $0.value < 32 || (127...159).contains($0.value) }) else {
            throw TerminalHistorySearchError.invalidPage
        }
        self.column = column; self.width = width; self.concealed = concealed
        self.text = concealed ? "" : text
    }
}

public struct TerminalSearchRow: Sendable, CustomDebugStringConvertible {
    public let sequence: UInt64, cells: [TerminalSearchCell]
    public var debugDescription: String { "TerminalSearchRow([redacted])" }
    public init(sequence: UInt64, cells: [TerminalSearchCell]) throws {
        guard cells.count <= 512 else { throw TerminalHistorySearchError.capacity }
        self.sequence = sequence; self.cells = cells
    }
}

/// A complete, bounded viewport. first/live delimit legal viewport START rows,
/// not the final line of output. The exact source and geometry generation remain
/// attached even when a machine title, row number or screen is reused.
public struct TerminalSearchPage<Source: Equatable & Sendable>: Sendable, CustomDebugStringConvertible {
    public let source: Source
    public let geometryRevision: UInt64, sequence: UInt64
    public let alternateScreen: Bool
    public let columns: Int
    public let first: Int64, live: Int64, top: Int64
    public let rows: [TerminalSearchRow]
    public var debugDescription: String { "TerminalSearchPage([redacted])" }
    public init(source: Source, geometryRevision: UInt64, sequence: UInt64, alternateScreen: Bool,
                columns: Int, first: Int64, live: Int64, top: Int64, rows: [TerminalSearchRow]) throws {
        guard geometryRevision > 0, (1...512).contains(columns), (1...512).contains(rows.count),
              rows.count <= 65_536 / columns, first != Int64.min, first <= top, top <= live,
              live <= Int64.max - Int64(rows.count - 1) else { throw TerminalHistorySearchError.invalidPage }
        var bytes = 0
        for row in rows {
            guard row.sequence <= sequence else { throw TerminalHistorySearchError.invalidPage }
            var end = 0
            for cell in row.cells {
                guard cell.column >= end, cell.column < columns, cell.width <= columns - cell.column else {
                    throw TerminalHistorySearchError.invalidPage
                }
                let count = cell.text.utf8.count
                guard count <= 1_048_576 - bytes else { throw TerminalHistorySearchError.capacity }
                bytes += count; end = cell.column + cell.width
            }
        }
        self.source = source; self.geometryRevision = geometryRevision; self.sequence = sequence
        self.alternateScreen = alternateScreen; self.columns = columns
        self.first = first; self.live = live; self.top = top; self.rows = rows
    }
}

public struct TerminalHistorySearchLimits: Sendable {
    public var maximumRows = 4096
    public var maximumBytes = 4 * 1024 * 1024
    public var maximumMatches = 128
    public var maximumPages = 256
    public init() {}
    fileprivate func validate() throws {
        guard (1...8192).contains(maximumRows), (1...8_388_608).contains(maximumBytes),
              (1...128).contains(maximumMatches), (1...512).contains(maximumPages) else {
            throw TerminalHistorySearchError.capacity
        }
    }
}

public struct TerminalHistoryMatch<Source: Equatable & Sendable>: Sendable, Identifiable, CustomDebugStringConvertible {
    public let id: UUID
    public let source: Source
    public let row: Int64, rowSequence: UInt64, geometryRevision: UInt64
    public let alternateScreen: Bool, columns: Range<Int>, excerpt: String
    fileprivate let gridColumns: Int, byteRange: Range<Int>, needle: [UInt8]
    public var debugDescription: String { "TerminalHistoryMatch([redacted])" }
}

public struct TerminalHistorySearchResult<Source: Equatable & Sendable>: Sendable, CustomDebugStringConvertible {
    public enum Completion: Equatable, Sendable { case complete, limitReached, historyChanged }
    public let matches: [TerminalHistoryMatch<Source>]
    public let completion: Completion
    public let scannedRows: Int, scannedBytes: Int, pages: Int
    /// The range observed at start. Output arriving later is outside this search.
    public let firstRow: Int64, lastRow: Int64
    public var debugDescription: String { "TerminalHistorySearchResult([redacted])" }
}

/// Literal UTF-8 KMP, newest physical rows first. No regex engine, hidden
/// normalization, cross-row joining, retained transcript or per-row tasks.
/// Results are observations, not an atomic transcript snapshot; revealing one
/// requires revalidating its exact source, geometry, row sequence AND matched
/// bytes. Only a read-only canonical snapshot provider is accepted here.
public enum TerminalHistorySearch {
    public static func search<Source: Equatable & Sendable>(query: String, source: Source,
        limits: TerminalHistorySearchLimits = .init(),
        read: @Sendable (Int64?) async throws -> TerminalSearchPage<Source>) async throws -> TerminalHistorySearchResult<Source> {
        try limits.validate()
        let needle = try queryBytes(query)
        let prefix = prefixTable(needle)
        try Task.checkCancellation()
        var page: TerminalSearchPage<Source>? = try await read(nil)
        try Task.checkCancellation()
        guard let initial = page, initial.source == source, initial.top == initial.live else {
            throw TerminalHistorySearchError.invalidPage
        }
        let first = initial.first, last = initial.live + Int64(initial.rows.count - 1)
        let geometry = initial.geometryRevision, alternate = initial.alternateScreen
        let columns = initial.columns, height = initial.rows.count
        var before: Int64? = nil, requestedTop = initial.live
        var sequence = initial.sequence
        var matches: [TerminalHistoryMatch<Source>] = []
        var scannedRows = 0, scannedBytes = 0, pages = 0
        func result(_ completion: TerminalHistorySearchResult<Source>.Completion) -> TerminalHistorySearchResult<Source> {
            .init(matches: matches, completion: completion, scannedRows: scannedRows,
                  scannedBytes: scannedBytes, pages: pages, firstRow: first, lastRow: last)
        }
        while let current = page {
            try Task.checkCancellation()
            guard current.source == source, current.top == requestedTop else { throw TerminalHistorySearchError.invalidPage }
            guard current.geometryRevision == geometry, current.alternateScreen == alternate,
                  current.columns == columns, current.rows.count == height,
                  current.sequence >= sequence else { return result(.historyChanged) }
            sequence = current.sequence; pages += 1
            for index in current.rows.indices.reversed() {
                try Task.checkCancellation()
                let rowIndex = current.top + Int64(index)
                if rowIndex > last || before.map({ rowIndex >= $0 }) == true { continue }
                if rowIndex < first { continue }
                guard scannedRows < limits.maximumRows else { return result(.limitReached) }
                let row = current.rows[index]
                let flat: FlatRow
                do { flat = try flatten(row, maximumBytes: min(1_048_576, limits.maximumBytes - scannedBytes)) }
                catch TerminalHistorySearchError.capacity { return result(.limitReached) }
                scannedRows += 1; scannedBytes += flat.bytes.count
                var matched = 0, spanIndex = 0
                for offset in flat.bytes.indices {
                    if offset & 4095 == 0 { try Task.checkCancellation() }
                    while spanIndex < flat.spans.count, offset >= flat.spans[spanIndex].bytes.upperBound { spanIndex += 1 }
                    guard spanIndex < flat.spans.count else { throw TerminalHistorySearchError.invalidPage }
                    if !flat.spans[spanIndex].searchable { matched = 0; continue }
                    while matched > 0, needle[matched] != flat.bytes[offset] { matched = prefix[matched - 1] }
                    if needle[matched] == flat.bytes[offset] { matched += 1 }
                    if matched == needle.count {
                        // Keep one extra-match observation, not an extra retained
                        // result, to distinguish exactly-at-cap from truncation.
                        guard matches.count < limits.maximumMatches else { return result(.limitReached) }
                        let range = (offset + 1 - needle.count)..<(offset + 1)
                        let covered = try flat.covering(range)
                        matches.append(.init(id: UUID(), source: source, row: rowIndex, rowSequence: row.sequence,
                            geometryRevision: geometry, alternateScreen: alternate, columns: covered.columns,
                            excerpt: flat.excerpt(around: covered.indices), gridColumns: columns,
                            byteRange: range, needle: needle))
                        matched = prefix[matched - 1]
                    }
                }
            }
            if current.top <= first { return result(.complete) }
            guard pages < limits.maximumPages else { return result(.limitReached) }
            before = current.top
            // Avoid signed subtraction overflow even for very old stable rows.
            let stepped = current.top.subtractingReportingOverflow(Int64(height))
            requestedTop = stepped.overflow ? first : max(first, stepped.partialValue)
            page = nil
            await Task.yield()
            try Task.checkCancellation()
            do { page = try await read(requestedTop) }
            catch TerminalHistorySearchError.unavailable { return result(.historyChanged) }
        }
        throw TerminalHistorySearchError.invalidPage
    }

    /// Call under the canonical owner's SAME transaction that selects the
    /// viewport. This validates content, not network or input authority.
    public static func validate<Source>(_ match: TerminalHistoryMatch<Source>,
        in page: TerminalSearchPage<Source>) throws where Source: Equatable & Sendable {
        guard page.source == match.source, page.geometryRevision == match.geometryRevision,
              page.alternateScreen == match.alternateScreen, page.columns == match.gridColumns,
              match.row >= page.top else { throw TerminalHistorySearchError.staleResult }
        let distance = match.row.subtractingReportingOverflow(page.top)
        guard !distance.overflow, distance.partialValue < Int64(page.rows.count) else {
            throw TerminalHistorySearchError.staleResult
        }
        let row = page.rows[Int(distance.partialValue)]
        guard row.sequence == match.rowSequence else { throw TerminalHistorySearchError.staleResult }
        let flat = try flatten(row, maximumBytes: 1_048_576)
        guard match.byteRange.upperBound <= flat.bytes.count,
              flat.bytes[match.byteRange].elementsEqual(match.needle) else { throw TerminalHistorySearchError.staleResult }
        let covered = try flat.covering(match.byteRange)
        guard covered.columns == match.columns,
              flat.spans[covered.indices].allSatisfy(\.searchable) else { throw TerminalHistorySearchError.staleResult }
    }

    private static func queryBytes(_ query: String) throws -> [UInt8] {
        guard !query.isEmpty, query.utf8.count <= 1024,
              !query.unicodeScalars.contains(where: { $0.value < 32 || (127...159).contains($0.value) }) else {
            throw TerminalHistorySearchError.invalidQuery
        }
        return Array(query.utf8)
    }
    private static func prefixTable(_ bytes: [UInt8]) -> [Int] {
        var table = [Int](repeating: 0, count: bytes.count), prefix = 0
        for index in bytes.indices.dropFirst() {
            while prefix > 0, bytes[index] != bytes[prefix] { prefix = table[prefix - 1] }
            if bytes[index] == bytes[prefix] { prefix += 1 }
            table[index] = prefix
        }
        return table
    }
    private struct Span { let bytes: Range<Int>, columns: Range<Int>, searchable: Bool }
    private struct FlatRow {
        let bytes: [UInt8], spans: [Span]
        func covering(_ range: Range<Int>) throws -> (indices: ClosedRange<Int>, columns: Range<Int>) {
            guard !range.isEmpty, range.lowerBound >= 0, range.upperBound <= bytes.count,
                  let first = spans.firstIndex(where: { $0.bytes.contains(range.lowerBound) }),
                  let last = spans.lastIndex(where: { $0.bytes.contains(range.upperBound - 1) }) else {
                throw TerminalHistorySearchError.invalidPage
            }
            return (first...last, spans[first].columns.lowerBound..<spans[last].columns.upperBound)
        }
        func excerpt(around indices: ClosedRange<Int>) -> String {
            var first = indices.lowerBound, last = indices.upperBound
            let ownBytes = spans[last].bytes.upperBound - spans[first].bytes.lowerBound
            guard ownBytes <= 512 else { return "[Match in a long terminal cell]" }
            for _ in 0..<12 {
                if first > 0, spans[last].bytes.upperBound - spans[first - 1].bytes.lowerBound <= 512 { first -= 1 }
                if last + 1 < spans.count, spans[last + 1].bytes.upperBound - spans[first].bytes.lowerBound <= 512 { last += 1 }
            }
            let text = String(decoding: bytes[spans[first].bytes.lowerBound..<spans[last].bytes.upperBound], as: UTF8.self)
            return (first > 0 ? "…" : "") + text + (last + 1 < spans.count ? "…" : "")
        }
    }
    private static func flatten(_ row: TerminalSearchRow, maximumBytes: Int) throws -> FlatRow {
        var bytes: [UInt8] = [], spans: [Span] = [], column = 0
        func append(_ value: [UInt8], columns: Range<Int>, searchable: Bool) throws {
            guard value.count <= maximumBytes - bytes.count else { throw TerminalHistorySearchError.capacity }
            let start = bytes.count; bytes.append(contentsOf: value)
            spans.append(.init(bytes: start..<bytes.count, columns: columns, searchable: searchable))
        }
        for cell in row.cells {
            try Task.checkCancellation()
            while column < cell.column {
                try append([32], columns: column..<(column + 1), searchable: true); column += 1
            }
            let value = cell.text.isEmpty ? [UInt8](repeating: 32, count: cell.width) : Array(cell.text.utf8)
            try append(value, columns: cell.column..<(cell.column + cell.width), searchable: !cell.concealed)
            column = cell.column + cell.width
        }
        return FlatRow(bytes: bytes, spans: spans)
    }
}
