import Foundation

/// A local viewing position, issued by one runtime. It contains no input or
/// transport authority and cannot be restored in another source or runtime.
/// History can expire while search is open; restoration validates its original
/// geometry, screen and first-row witness rather than selecting reused rows.
public struct TerminalHistoryBookmark: Sendable {
    public let source: TerminalSource
    /// nil means the live screen, not a frozen row at the former live position.
    public let viewportTop: Int64?
    let owner: UUID
    let geometryRevision: UInt64
    let alternateScreen: Bool
    let rowSequence: UInt64
}

extension TerminalSearchPage where Source == TerminalSource {
    /// Adapts a complete canonical FTSF viewport. Search neither parses terminal
    /// escape sequences nor reconstructs cell widths from Unicode text. A
    /// synchronized-output transaction still in progress is not a completed
    /// search page. Producers without a retained-range witness are unavailable.
    public init(frame: TerminalRenderFrame) throws {
        guard frame.flags & 8 == 0, let history = frame.history else {
            throw TerminalHistorySearchError.unavailable
        }
        guard frame.lines.count == frame.rows else { throw TerminalHistorySearchError.invalidPage }
        var rows: [TerminalSearchRow] = []
        rows.reserveCapacity(frame.lines.count)
        for (index, line) in frame.lines.enumerated() {
            let row = frame.viewportFirst.addingReportingOverflow(Int64(index))
            guard !row.overflow, line.stableIndex == row.partialValue else {
                throw TerminalHistorySearchError.invalidPage
            }
            rows.append(try TerminalSearchRow(sequence: line.sequence, cells: line.cells.map {
                try TerminalSearchCell(column: $0.column, width: $0.width,
                                       text: $0.text, concealed: $0.invisible)
            }))
        }
        try self.init(source: frame.source, geometryRevision: frame.geometryRevision,
            sequence: frame.sequence, alternateScreen: frame.alternateScreen,
            columns: frame.columns, first: history.first, live: history.live,
            top: frame.viewportFirst, rows: rows)
    }
}
