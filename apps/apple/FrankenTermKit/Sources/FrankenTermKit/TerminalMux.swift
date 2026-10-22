import Foundation

/// One pane of a machine's FrankenTerm mux, as its server lists it: the same
/// panes the desktop app shows, in the server's order.
public struct TerminalMuxPane: Decodable, Equatable, Sendable, Identifiable {
    public struct Size: Decodable, Equatable, Sendable {
        public let rows: Int, cols: Int
        public init(rows: Int, cols: Int) { self.rows = rows; self.cols = cols }
    }
    public let paneId: Int, tabId: Int, windowId: Int
    public let tabTitle: String, title: String, workspace: String
    public let workingDir: String?
    public let size: Size
    public let isActive: Bool, isZoomed: Bool
    public var id: Int { paneId }

    public init(paneId: Int, tabId: Int, windowId: Int, tabTitle: String, title: String, workspace: String,
                workingDir: String?, size: Size, isActive: Bool, isZoomed: Bool) {
        self.paneId = paneId; self.tabId = tabId; self.windowId = windowId; self.tabTitle = tabTitle
        self.title = title; self.workspace = workspace; self.workingDir = workingDir; self.size = size
        self.isActive = isActive; self.isZoomed = isZoomed
    }

    /// A short label for a pane switcher: the program's title, else the
    /// last component of its working directory.
    public var label: String {
        let trimmed = title.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty { return trimmed }
        if let dir = workingDir, let last = URL(string: dir)?.lastPathComponent, !last.isEmpty { return last }
        return "Pane \(paneId)"
    }
}

/// One scrollback match: from `startColumn` of `row` to `endColumn` of
/// `endRow` (cells), with the text of the rows it spans.
public struct TerminalMuxSearchMatch: Decodable, Equatable, Sendable, Identifiable {
    public let row: Int64, startColumn: Int, endRow: Int64, endColumn: Int
    public let text: String
    public var id: String { "\(row):\(startColumn):\(endRow):\(endColumn)" }
}

/// A pane's answer to a search: its newest matches, newest first.
public struct TerminalMuxSearchResults: Decodable, Equatable, Sendable {
    public let search: UInt64
    public let matches: [TerminalMuxSearchMatch]
    /// How many of the pane's newest rows were searched.
    public let searchedRows: Int
    public let error: String?
    public init(search: UInt64, matches: [TerminalMuxSearchMatch], searchedRows: Int, error: String?) {
        self.search = search; self.matches = matches; self.searchedRows = searchedRows; self.error = error
    }
}

public struct TerminalMuxContextLine: Decodable, Equatable, Sendable, Identifiable {
    public let row: Int64, text: String
    public var id: Int64 { row }
}

public struct TerminalMuxContext: Decodable, Equatable, Sendable {
    public let request: UInt64
    public let lines: [TerminalMuxContextLine]
}

public enum TerminalMuxEvent: Equatable, Sendable {
    case ready(serverVersion: String)
    case panes([TerminalMuxPane])
    /// Bytes for the pane's terminal, which must be rows x columns first.
    case output(paneID: Int, rows: Int, columns: Int, bytes: Data)
    case paneTitle(paneID: Int, title: String)
    case paneGone(paneID: Int)
    case searchResults(paneID: Int, TerminalMuxSearchResults)
    case context(paneID: Int, TerminalMuxContext)
    /// The session is over; the channel must close.
    case failed(String)
}

/// The protocol state of one connection to a machine's mux server, the same
/// server the desktop attaches to. It does no I/O: the owner passes in what
/// the channel delivered, sends what `outgoing` returns, and drains events.
public protocol TerminalMuxClient: AnyObject, Sendable {
    func receive(_ bytes: Data) throws
    func channelClosed() throws
    /// Up to `limit` bytes to send next; returned bytes count as taken.
    func outgoing(limit: Int) throws -> Data
    func nextEvent() throws -> TerminalMuxEvent?
    func refreshPanes() throws
    /// Draws the pane from scratch, then every change.
    func show(pane: Int) throws
    func hide(pane: Int) throws
    func write(pane: Int, bytes: Data) throws
    func paste(pane: Int, text: String) throws
    /// Resizes the remote pane, remembering its size before the first resize.
    func resize(pane: Int, rows: Int, columns: Int) throws
    func restoreSize(pane: Int) throws
    /// Searches the pane's scrollback on the server, newest rows first; a
    /// `.searchResults` event with the returned id answers.
    func search(pane: Int, text: String, caseSensitive: Bool, regex: Bool) throws -> UInt64
    /// The text of the pane's rows `firstRow..<firstRow+count`; a `.context`
    /// event with the returned id answers.
    func context(pane: Int, firstRow: Int64, count: Int) throws -> UInt64
}

public enum TerminalMuxError: Error, Equatable, Sendable, LocalizedError {
    /// Nothing answered at the machine's mux socket.
    case unavailable
    /// The mux server speaks another protocol or ended the session.
    case failed(String)
    /// The server lists no panes to attach to.
    case noPanes
    public var errorDescription: String? {
        switch self {
        case .unavailable: return "No FrankenTerm mux is running on this machine."
        case .failed(let reason): return "The machine's FrankenTerm mux ended the session: \(reason)"
        case .noPanes: return "The machine's FrankenTerm mux has no panes."
        }
    }
}

/// Shared between a mux-attached terminal's transport and its screen: the
/// machine's panes, the one on screen, and the user's requests. The transport
/// publishes; the screen requests a pane or reports visibility, and the
/// transport acts on its next service turn. Thread-safe; callbacks are
/// invoked outside the lock.
public final class TerminalMuxControl: @unchecked Sendable {
    /// Separate from titles/search state: exact input destinations are invalidated
    /// only by selection, registration loss and connection retirement.
    public let inputRouting = TerminalMuxInputRouting()
    public struct SearchQuery: Equatable, Sendable {
        public let text: String
        public let caseSensitive: Bool, regex: Bool
        public init(text: String, caseSensitive: Bool, regex: Bool) {
            self.text = text; self.caseSensitive = caseSensitive; self.regex = regex
        }
    }
    /// A search across some of the machine's panes, filling in as each answers.
    public struct Search: Equatable, Sendable {
        public let id: UUID
        public let query: SearchQuery
        public let panes: [Int]
        /// Panes that have not answered yet.
        public var pending: Set<Int>
        public var results: [Int: TerminalMuxSearchResults] = [:]
        /// A partial result must not be presented as an exhaustive negative.
        public var issue: String?
        public var limitReached = false
    }
    /// Rows around one match, for reading it in place.
    public struct Context: Equatable, Sendable {
        public let id: UUID
        public let pane: Int, row: Int64
        public var lines: [TerminalMuxContextLine]?
        public var error: String?
        public let firstRow: Int64, rowCount: Int
    }
    public struct State: Equatable, Sendable {
        /// Callbacks cross executors outside the lock. Consumers compare this
        /// witness with current.revision before applying a delayed snapshot.
        public var revision = UUID()
        public var panes: [TerminalMuxPane] = []
        public var shown: Int?
        public var visible = false
        public var search: Search?
        public var context: Context?
    }
    private let lock = NSLock()
    private var state = State()
    private var requested: Int?
    private var pendingSearch: Search?
    private var pendingContext: (id: UUID, first: Int64, count: Int)?
    private var waker: (@Sendable () -> Void)?
    private var observer: (@Sendable (State) -> Void)?
    private var queriesEnded = false

    public static let maximumSearchPanes = 128
    public static let maximumSearchMatches = 512
    public static let maximumSearchTextBytes = 512 * 1024
    public static let maximumMatchesPerPane = 128
    public static let maximumContextLines = 256
    public static let maximumContextTextBytes = 128 * 1024
    private static let maximumLineBytes = 16 * 1024

    public init() {}

    public var current: State { lock.withLock { state } }

    /// The screen's change handler, called with each new state.
    public func observe(_ handler: @escaping @Sendable (State) -> Void) {
        let snapshot = lock.withLock { observer = handler; return state }
        handler(snapshot)
    }

    /// Asks the transport to show this pane instead.
    public func request(pane: Int) {
        let wake = lock.withLock { () -> (@Sendable () -> Void)? in
            guard state.panes.contains(where: { $0.paneId == pane }) else { return nil }
            requested = pane; return waker
        }
        wake?()
    }

    /// Whether the machine is on screen. Only a visible machine's pane is
    /// sized to the phone and drawn; a hidden one returns to its own size.
    public func setVisible(_ visible: Bool) {
        let (wake, notify, snapshot) = lock.withLock { () -> ((@Sendable () -> Void)?, (@Sendable (State) -> Void)?, State) in
            guard state.visible != visible else { return (nil, nil, state) }
            state.visible = visible; state.revision = UUID(); return (waker, observer, state)
        }
        notify?(snapshot)
        wake?()
    }

    /// Searches `panes` (the machine's, by id); `current.search` fills in as
    /// each answers. A new search replaces the last one.
    @discardableResult
    public func search(_ query: SearchQuery, panes: [Int]) -> UUID {
        let id = UUID()
        let (notify, snapshot, wake) = lock.withLock { () -> ((@Sendable (State) -> Void)?, State, (@Sendable () -> Void)?) in
            let validText = (1...4096).contains(query.text.utf8.count)
            let validPanes = !panes.isEmpty && panes.count <= Self.maximumSearchPanes
                && Set(panes).count == panes.count && panes.allSatisfy { id in
                    id >= 0 && state.panes.contains { $0.paneId == id }
                }
            let storedQuery = validText ? query : SearchQuery(text: "", caseSensitive: query.caseSensitive, regex: query.regex)
            var search = Search(id: id, query: storedQuery, panes: validPanes ? panes : [], pending: [])
            if queriesEnded { search.issue = "This connection is closed. Reconnect to search again." }
            else if !validText { search.issue = "Enter between 1 and 4,096 UTF-8 bytes to search." }
            else if !validPanes { search.issue = "Choose at most 128 distinct, currently available panes." }
            else { search.pending = Set(panes) }
            state.search = search; state.context = nil; pendingContext = nil
            // Publish state and work in the SAME critical section. A reentrant
            // observer clearing/replacing this search cannot revive old work.
            pendingSearch = search.pending.isEmpty ? nil : search
            state.revision = UUID()
            return (observer, state, waker)
        }
        notify?(snapshot); wake?()
        return id
    }

    public func clearSearch() {
        let (notify, snapshot, wake) = lock.withLock {
            pendingSearch = nil; pendingContext = nil
            state.search = nil; state.context = nil; state.revision = UUID()
            return (observer, state, waker)
        }
        notify?(snapshot); wake?()
    }

    /// Preserve already received results while stopping unsent work. The wire
    /// driver still owns any sent request until its actual response or close.
    public func cancelSearch(_ id: UUID) {
        finishSearch(id, reason: "Search stopped. Results are incomplete.")
    }

    public func finishSearch(_ id: UUID, reason: String) {
        let (notify, snapshot, wake) = lock.withLock { () -> ((@Sendable (State) -> Void)?, State, (@Sendable () -> Void)?) in
            guard state.search?.id == id, state.search?.pending.isEmpty == false else { return (nil, state, nil) }
            state.search?.pending.removeAll()
            state.search?.issue = Self.boundedError(reason)
            if pendingSearch?.id == id { pendingSearch = nil }
            state.revision = UUID()
            return (observer, state, waker)
        }
        notify?(snapshot); wake?()
    }

    /// Reads the rows around `row` of `pane` into `current.context`.
    /// A result reader supplies searchID to refuse a replaced/expired hit.
    public func readContext(pane: Int, row: Int64, before: Int = 12, after: Int = 12, searchID: UUID? = nil) {
        let id = UUID()
        let (notify, snapshot, wake) = lock.withLock { () -> ((@Sendable (State) -> Void)?, State, (@Sendable () -> Void)?) in
            pendingContext = nil
            var problem: String?
            var first: Int64 = 0, count = 0
            if queriesEnded { problem = "This connection is closed. Reconnect to read its rows." }
            else if pane < 0 || !state.panes.contains(where: { $0.paneId == pane }) {
                problem = "This pane is no longer available."
            } else if let searchID, state.search?.id != searchID
                || state.search?.results[pane]?.matches.contains(where: { $0.row == row }) != true {
                problem = "This search result changed. Search again before reading its rows."
            } else if row < 0 || !(0..<Self.maximumContextLines).contains(before)
                || !(0..<Self.maximumContextLines).contains(after)
                || before + after >= Self.maximumContextLines {
                problem = "The requested context range is invalid or too large."
            } else {
                first = max(0, row - Int64(before))
                count = Int(row - first) + after + 1
                if first > Int64.max - Int64(count) { problem = "The requested context range is invalid or too large." }
            }
            state.context = Context(id: id, pane: pane, row: row, lines: problem == nil ? nil : [],
                                    error: problem, firstRow: first, rowCount: count)
            if problem == nil { pendingContext = (id, first, count) }
            state.revision = UUID()
            return (observer, state, waker)
        }
        notify?(snapshot); wake?()
    }

    public func clearContext() {
        let (notify, snapshot, wake) = lock.withLock {
            pendingContext = nil; state.context = nil; state.revision = UUID()
            return (observer, state, waker)
        }
        notify?(snapshot); wake?()
    }

    private func update(_ change: (inout State) -> Bool) {
        let (notify, snapshot) = lock.withLock { () -> ((@Sendable (State) -> Void)?, State) in
            guard change(&state) else { return (nil, state) }
            state.revision = UUID()
            return (observer, state)
        }
        notify?(snapshot)
    }

    // MARK: transport side

    /// The search to start, once.
    public func takeSearch() -> Search? { lock.withLock { defer { pendingSearch = nil }; return pendingSearch } }

    /// The rows to read, once: the context's id, pane, first row and count.
    public func takeContext() -> (id: UUID, pane: Int, first: Int64, count: Int)? {
        lock.withLock {
            defer { pendingContext = nil }
            guard let pending = pendingContext, let context = state.context, context.id == pending.id else { return nil }
            return (pending.id, context.pane, pending.first, pending.count)
        }
    }

    /// One pane's answer; answers to a replaced search are dropped.
    public func deliver(search id: UUID, pane: Int, results: TerminalMuxSearchResults) {
        update { state in
            guard var search = state.search, search.id == id, search.pending.contains(pane) else { return false }
            var remainingMatches = Self.maximumSearchMatches - search.results.values.reduce(0) { $0 + $1.matches.count }
            var remainingBytes = Self.maximumSearchTextBytes - search.results.values.reduce(0) { sum, value in
                sum + value.matches.reduce(0) { $0 + $1.text.utf8.count }
            }
            var matches: [TerminalMuxSearchMatch] = [], identifiers = Set<String>()
            var issue = results.error.map(Self.boundedError)
            if results.searchedRows < 0 { issue = "The server returned an invalid searched-row count." }
            else if issue == nil {
                for match in results.matches.prefix(Self.maximumMatchesPerPane) {
                    guard match.row >= 0, match.endRow >= match.row,
                          (0...65_535).contains(match.startColumn), (0...65_535).contains(match.endColumn),
                          match.endRow != match.row || match.endColumn >= match.startColumn,
                          identifiers.insert(match.id).inserted else {
                        issue = "The server returned invalid or duplicate match coordinates."; break
                    }
                    guard match.text.utf8.count <= Self.maximumLineBytes else {
                        issue = "A search result exceeded this device's text limit."; search.limitReached = true; break
                    }
                    guard remainingMatches > 0, match.text.utf8.count <= remainingBytes else {
                        search.limitReached = true; break
                    }
                    matches.append(match); remainingMatches -= 1; remainingBytes -= match.text.utf8.count
                }
                if results.matches.count > Self.maximumMatchesPerPane { search.limitReached = true }
            }
            if issue != nil { matches = [] }
            search.results[pane] = .init(search: results.search, matches: matches,
                searchedRows: max(0, results.searchedRows), error: issue)
            search.pending.remove(pane)
            if search.limitReached {
                search.issue = "The device's search-result limit was reached. Narrow the query or search fewer panes."
                search.pending.removeAll()
            }
            state.search = search
            return true
        }
    }

    public func deliver(context id: UUID, lines: [TerminalMuxContextLine], error: String? = nil) {
        update { state in
            guard var context = state.context, context.id == id, context.lines == nil else { return false }
            var problem = error.map(Self.boundedError), bytes = 0, previous: Int64?
            if problem == nil {
                if lines.count > context.rowCount || lines.count > Self.maximumContextLines {
                    problem = "The server returned too many context rows."
                } else {
                    for line in lines {
                        guard line.row >= context.firstRow, line.row - context.firstRow < Int64(context.rowCount),
                              previous == nil || line.row > previous!,
                              line.text.utf8.count <= Self.maximumLineBytes,
                              line.text.utf8.count <= Self.maximumContextTextBytes - bytes else {
                            problem = "The server returned invalid or oversized context rows."; break
                        }
                        previous = line.row; bytes += line.text.utf8.count
                    }
                }
            }
            context.lines = problem == nil ? lines : []; context.error = problem
            state.context = context
            return true
        }
    }

    private static func boundedError(_ text: String) -> String {
        text.utf8.count <= 512 ? text : "The server reported an oversized error message."
    }

    public func endQueries() {
        let (notify, snapshot) = lock.withLock {
            queriesEnded = true
            pendingSearch = nil; pendingContext = nil
            if state.search?.pending.isEmpty == false {
                state.search?.pending.removeAll(); state.search?.issue = "The connection closed before this search finished."
            }
            if state.context?.lines == nil {
                state.context?.lines = []; state.context?.error = "The connection closed before these rows arrived."
            }
            state.revision = UUID()
            return (observer, state)
        }
        notify?(snapshot)
    }

    public func setWaker(_ waker: (@Sendable () -> Void)?) { lock.withLock { self.waker = waker } }

    public func takeRequest() -> Int? {
        lock.withLock { defer { requested = nil }; return requested }
    }

    public func publish(panes: [TerminalMuxPane]? = nil, shown: Int?) {
        let (notify, snapshot) = lock.withLock { () -> ((@Sendable (State) -> Void)?, State) in
            if let panes {
                let existing = Set(panes.map(\.paneId))
                for old in state.panes where !existing.contains(old.paneId) { removeQueryPane(old.paneId) }
                state.panes = panes
            }
            state.shown = shown
            state.revision = UUID()
            return (observer, state)
        }
        notify?(snapshot)
    }

    public func retitle(pane: Int, title: String) {
        let (notify, snapshot) = lock.withLock { () -> ((@Sendable (State) -> Void)?, State) in
            guard let index = state.panes.firstIndex(where: { $0.paneId == pane }) else { return (nil, state) }
            let old = state.panes[index]
            state.panes[index] = TerminalMuxPane(paneId: old.paneId, tabId: old.tabId, windowId: old.windowId,
                tabTitle: old.tabTitle, title: title, workspace: old.workspace, workingDir: old.workingDir,
                size: old.size, isActive: old.isActive, isZoomed: old.isZoomed)
            state.revision = UUID()
            return (observer, state)
        }
        notify?(snapshot)
    }

    public func remove(pane: Int) {
        let (notify, snapshot) = lock.withLock { () -> ((@Sendable (State) -> Void)?, State) in
            state.panes.removeAll { $0.paneId == pane }
            if state.shown == pane { state.shown = nil }
            removeQueryPane(pane)
            if requested == pane { requested = nil }
            state.revision = UUID()
            return (observer, state)
        }
        notify?(snapshot)
    }

    /// Called under lock; removal permanently expires this search's reference,
    /// even if the same numeric pane identifier appears in a later inventory.
    private func removeQueryPane(_ pane: Int) {
        if state.search?.panes.contains(pane) == true {
            state.search?.pending.remove(pane)
            state.search?.results[pane] = .init(search: 0, matches: [], searchedRows: 0,
                                               error: "This pane closed before its search could be inspected.")
        }
        if state.context?.pane == pane {
            pendingContext = nil; state.context?.lines = []; state.context?.error = "This pane is no longer available."
        }
    }
}
