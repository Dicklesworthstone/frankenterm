import Foundation

/// A bounded reading view of one coherent terminal capture. It is local text,
/// never an input buffer or a reason to fetch remote history. Keeping logical
/// lines separate lets assistive readers move through output without repeating
/// the whole screen whenever a program prints.
public struct TerminalAccessibilitySnapshot: Equatable, Sendable {
    public struct Line: Equatable, Sendable {
        public let firstRow: Int64, lastRow: Int64
        public let viewportRows: ClosedRange<Int>
        public let text: String
        public let truncated: Bool
        public var spokenText: String {
            (text.isEmpty ? "Blank line" : text) + (truncated ? ". More text in this line is not included." : "")
        }
    }

    public let source: TerminalSource
    public let revision: UInt64, geometryRevision: UInt64
    public let viewportRows: Int
    public let isLive: Bool, isRetained: Bool, alternateScreen: Bool
    public let lines: [Line]
    public let cursorLineIndex: Int?
    public let omittedLines: Int
    public let textByteCount: Int

    /// Favor the newest visible lines when the reading budget is exhausted.
    /// Text is capped while copying, at complete Character boundaries, rather
    /// than constructing an unbounded transcript and then truncating it.
    public init(frame: TerminalRenderFrame, maximumLines: Int = 128,
                maximumTextBytes: Int = 32 * 1024, maximumLineBytes: Int = 4096) throws {
        guard (1...512).contains(maximumLines), (1...65536).contains(maximumTextBytes),
              (1...4096).contains(maximumLineBytes),
              (1...512).contains(frame.rows), (1...512).contains(frame.columns),
              frame.lines.count == frame.rows else { throw TerminalRenderError.capacity }
        source = frame.source; revision = frame.revision; geometryRevision = frame.geometryRevision
        viewportRows = frame.rows; isLive = frame.isLive
        isRetained = frame.flags & 8 != 0; alternateScreen = frame.alternateScreen

        var groups: [ClosedRange<Int>] = []
        var first = 0
        for index in frame.lines.indices {
            if index + 1 == frame.lines.count || !frame.lines[index].joins(frame.lines[index + 1], columns: frame.columns) {
                groups.append(first...index); first = index + 1
            }
        }
        let cursorRow = frame.isLive && frame.cursor.visible ? frame.cursor.stableRow : nil
        // Blank rows below output are padding, except the current cursor line.
        while let last = groups.last, !last.contains(where: { frame.lines[$0].stableIndex == cursorRow }),
              last.allSatisfy({ row in
                  frame.lines[row].cells.allSatisfy { $0.invisible || $0.text.allSatisfy { $0 == " " } }
              }) {
            groups.removeLast()
        }

        var captured: [Line] = [], bytes = 0
        for group in groups.reversed() {
            guard captured.count < maximumLines, bytes < maximumTextBytes else { break }
            let limit = min(maximumLineBytes, maximumTextBytes - bytes)
            var text = "", used = 0, truncated = false
            func append(_ value: String) {
                guard !truncated else { return }
                for character in value {
                    let size = character.utf8.count
                    guard size <= limit - used else { truncated = true; return }
                    text.append(character); used += size
                }
            }
            for rowIndex in group {
                let row = frame.lines[rowIndex]
                var column = 0
                for cell in row.cells {
                    guard cell.column >= column, cell.column < frame.columns,
                          (1...2).contains(cell.width), cell.width <= frame.columns - cell.column,
                          cell.text.utf8.count <= 4096 else { throw TerminalRenderError.malformed }
                    append(String(repeating: " ", count: cell.column - column))
                    if cell.invisible || cell.text.isEmpty {
                        append(String(repeating: " ", count: cell.width))
                    } else {
                        append(cell.text)
                    }
                    column = cell.column + cell.width
                    if truncated { break }
                }
                if truncated { break }
            }
            // Spaces at a proven soft wrap stay inside the logical line. Only
            // its final trailing blanks are trimmed; wide cells occur once.
            while text.last == " " { text.removeLast(); used -= 1 }
            bytes += used
            captured.append(Line(firstRow: frame.lines[group.lowerBound].stableIndex,
                lastRow: frame.lines[group.upperBound].stableIndex, viewportRows: group,
                text: text, truncated: truncated))
        }
        let chronologicalLines = Array(captured.reversed())
        lines = chronologicalLines; textByteCount = bytes
        omittedLines = groups.count - chronologicalLines.count
        cursorLineIndex = cursorRow.flatMap { row in
            chronologicalLines.firstIndex { $0.firstRow <= row && row <= $0.lastRow }
        }
    }

    public var newestOutputIndex: Int? { lines.lastIndex { !$0.text.isEmpty } ?? lines.indices.last }

    /// Rotor navigation stops at the boundary; it never wraps into an unrelated
    /// source or sends cursor keys to the terminal.
    public func nextLine(after index: Int?, forward: Bool) -> Int? {
        guard !lines.isEmpty else { return nil }
        guard let index else { return forward ? lines.startIndex : lines.index(before: lines.endIndex) }
        guard lines.indices.contains(index) else { return nil }
        let next = index + (forward ? 1 : -1)
        return lines.indices.contains(next) ? next : nil
    }
}

/// Reading is deliberately stable during an output flood. A VoiceOver focus in
/// the transcript holds one snapshot; an explicit refresh, or leaving it and
/// returning, obtains the current capture. No transcript is persisted.
public struct TerminalAccessibilityReading: Sendable {
    public let source: TerminalSource
    public private(set) var snapshot: TerminalAccessibilitySnapshot?
    public private(set) var isReading = false

    public init(source: TerminalSource) { self.source = source }

    public mutating func setReading(_ value: Bool) { isReading = value }

    @discardableResult
    public mutating func capture(_ frame: TerminalRenderFrame, refresh: Bool = false) throws -> TerminalAccessibilitySnapshot {
        guard frame.source == source else { throw TerminalRenderError.staleSource }
        if let snapshot {
            guard frame.revision >= snapshot.revision else { throw TerminalRenderError.staleFrame }
            if !refresh && (isReading || snapshot.revision == frame.revision) { return snapshot }
        }
        let next = try TerminalAccessibilitySnapshot(frame: frame)
        snapshot = next
        return next
    }

    public mutating func discard() { snapshot = nil; isReading = false }
}
