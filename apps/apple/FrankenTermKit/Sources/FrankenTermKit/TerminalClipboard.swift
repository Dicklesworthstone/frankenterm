import Foundation

public enum TerminalClipboardError: Error, Equatable, Sendable, LocalizedError {
    case invalidGeometry, capacity, invalidRange, emptySelection, unavailable
    public var errorDescription: String? {
        switch self {
        case .invalidGeometry: "The terminal snapshot could not be validated."
        case .capacity: "This viewport is too large to copy. Select a smaller viewport."
        case .invalidRange: "The selected text range is no longer valid."
        case .emptySelection: "Select some output to copy."
        case .unavailable: "The terminal is updating. Try copying again after its update finishes."
        }
    }
}

/// An immutable, bounded selection document, never a terminal parser or input
/// source. Its producer supplies canonical cell positions; Unicode display
/// width is not recalculated. Concealed cells are replaced before retention.
/// Canonical soft-wrap boundaries are joined only within one coherent capture.
/// Rows without that metadata remain separate; text length never implies a wrap.
public struct TerminalCopyDocument: Sendable, CustomDebugStringConvertible {
    public struct Cell: Sendable {
        public let column: Int, width: Int, text: String, concealed: Bool
        public init(column: Int, width: Int, text: String, concealed: Bool = false) {
            self.column = column; self.width = width; self.text = text; self.concealed = concealed
        }
    }
    public let text: String
    public let physicalRows: Int
    private let atomicRanges: [NSRange]
    public var debugDescription: String { "TerminalCopyDocument([redacted])" }
    public var utf8Bytes: Int { text.utf8.count }
    public var utf16Count: Int { text.utf16.count }

    public init(rows: [[Cell]], columns: Int, maximumUTF8Bytes: Int = 131_072) throws {
        guard (1...512).contains(rows.count) else { throw TerminalClipboardError.invalidGeometry }
        try self.init(rows: rows, columns: columns, joinsPrevious: Array(repeating: false, count: rows.count),
                      maximumUTF8Bytes: maximumUTF8Bytes)
    }

    /// Copy one validated semantic viewport. The frame supplies source and
    /// geometry witnesses together with canonical soft-wrap metadata, so a
    /// wrapped command or URL stays one line while real newlines remain intact.
    public init(frame: TerminalRenderFrame, maximumUTF8Bytes: Int = 131_072) throws {
        guard frame.flags & 8 == 0 else { throw TerminalClipboardError.unavailable }
        guard frame.lines.count == frame.rows, (1...512).contains(frame.rows) else {
            throw TerminalClipboardError.invalidGeometry
        }
        let joins = frame.lines.indices.map { index in
            index > 0 && frame.lines[index - 1].joins(frame.lines[index], columns: frame.columns)
        }
        try self.init(rows: frame.lines.map { row in
            row.cells.map { Cell(column: $0.column, width: $0.width, text: $0.text, concealed: $0.invisible) }
        }, columns: frame.columns, joinsPrevious: joins, maximumUTF8Bytes: maximumUTF8Bytes)
    }

    private init(rows: [[Cell]], columns: Int, joinsPrevious: [Bool], maximumUTF8Bytes: Int) throws {
        guard (1...512).contains(columns), (1...512).contains(rows.count),
              rows.count <= 65_536 / columns, joinsPrevious.count == rows.count else {
            throw TerminalClipboardError.invalidGeometry
        }
        guard (1...131_072).contains(maximumUTF8Bytes) else { throw TerminalClipboardError.capacity }
        var result = "", ranges: [NSRange] = [], usedBytes = 0, usedUTF16 = 0
        var cells = 0
        for (rowIndex, row) in rows.enumerated() {
            guard row.count <= columns, row.count <= 65_536 - cells else { throw TerminalClipboardError.capacity }
            cells += row.count
            if rowIndex > 0 && !joinsPrevious[rowIndex] {
                guard usedBytes < maximumUTF8Bytes else { throw TerminalClipboardError.capacity }
                result.append("\n"); usedBytes += 1; usedUTF16 += 1
            }
            var column = 0
            for cell in row {
                guard cell.column >= column, cell.column < columns, (1...2).contains(cell.width),
                      cell.width <= columns - cell.column else { throw TerminalClipboardError.invalidGeometry }
                guard cell.text.utf8.count <= 4096 else { throw TerminalClipboardError.capacity }
                guard !cell.text.unicodeScalars.contains(where: {
                    $0.value < 32 || (127...159).contains($0.value)
                }) else { throw TerminalClipboardError.invalidGeometry }
                let gap = cell.column - column
                let value = cell.concealed || cell.text.isEmpty ? String(repeating: " ", count: cell.width) : cell.text
                let count = value.utf8.count
                guard gap <= maximumUTF8Bytes - usedBytes,
                      count <= maximumUTF8Bytes - usedBytes - gap else { throw TerminalClipboardError.capacity }
                result.append(String(repeating: " ", count: gap)); usedBytes += gap; usedUTF16 += gap
                // A canonical cell is indivisible for copying, even when its
                // grapheme uses many UTF-16 units or Core Text glyphs. Masked
                // whitespace is not atomic: it contains no original text.
                if !cell.concealed, !cell.text.isEmpty {
                    ranges.append(NSRange(location: usedUTF16, length: value.utf16.count))
                }
                result.append(value); usedBytes += count; usedUTF16 += value.utf16.count
                column = cell.column + cell.width
            }
            // Preserve represented trailing blanks. Unrepresented right-margin
            // cells add no text, and a width-two grapheme is not duplicated.
        }
        text = result; atomicRanges = ranges; physicalRows = rows.count
    }

    /// NSTextView/UITextView offsets are UTF-16, not byte or grid coordinates.
    /// Expand a partially selected canonical cell rather than copying half an
    /// emoji, combining sequence, or source-owned grapheme. Bounds precede sums.
    public func normalizedRange(_ range: NSRange) throws -> NSRange {
        let length = utf16Count
        guard range.location >= 0, range.length >= 0, range.location <= length,
              range.length <= length - range.location else { throw TerminalClipboardError.invalidRange }
        guard range.length > 0 else { throw TerminalClipboardError.emptySelection }
        var start = range.location, end = start + range.length
        func firstEnding(after position: Int) -> Int {
            var low = 0, high = atomicRanges.count
            while low < high {
                let middle = low + (high - low) / 2, value = atomicRanges[middle]
                if value.location + value.length <= position { low = middle + 1 } else { high = middle }
            }
            return low
        }
        let first = firstEnding(after: start)
        if first < atomicRanges.count {
            let cell = atomicRanges[first]
            if cell.location < end { start = min(start, cell.location) }
        }
        let last = firstEnding(after: end - 1)
        if last < atomicRanges.count {
            let cell = atomicRanges[last]
            if cell.location < end { end = max(end, cell.location + cell.length) }
        }
        return NSRange(location: start, length: end - start)
    }

    public func selectedText(in range: NSRange) throws -> String {
        let normalized = try normalizedRange(range)
        return (text as NSString).substring(with: normalized)
    }
}

/// A single-use local decision. The destination is the immutable responder
/// witness captured on offer, not whatever machine happens to be selected later.
/// No text is retained after confirm/cancel/retirement. Even failed confirmation
/// consumes the local decision; a new native paste action must offer a fresh one.
@MainActor
public final class TerminalClipboardReview<Destination: Equatable & Sendable>: CustomDebugStringConvertible {
    public struct Candidate: Sendable {
        public let id: UUID, destination: Destination, text: String
    }
    public private(set) var candidate: Candidate?
    public nonisolated var debugDescription: String { "TerminalClipboardReview([redacted])" }
    public init() {}

    public func offer(id: UUID, destination: Destination, text: String) throws {
        guard candidate == nil else { throw TerminalClipboardError.unavailable }
        guard !text.isEmpty else { throw TerminalClipboardError.emptySelection }
        guard text.utf8.count <= 32_768 else { throw TerminalClipboardError.capacity }
        guard !text.unicodeScalars.contains(where: {
            ($0.value < 32 && ![9, 10, 13].contains($0.value)) || (127...159).contains($0.value)
        }) else { throw TerminalClipboardError.invalidGeometry }
        candidate = Candidate(id: id, destination: destination, text: text)
    }
    public func cancel(id: UUID) {
        if candidate?.id == id { candidate = nil }
    }
    public func retire() { candidate = nil }
    public func confirm(id: UUID, destination: Destination,
                        admit: (Candidate) throws -> Void) throws {
        guard let candidate, candidate.id == id, candidate.destination == destination else {
            throw TerminalClipboardError.unavailable
        }
        // Clear BEFORE invoking client code: reentry cannot confirm twice, and
        // neither failure nor a reentrant new offer revives this old decision.
        self.candidate = nil
        try admit(candidate)
    }
}
