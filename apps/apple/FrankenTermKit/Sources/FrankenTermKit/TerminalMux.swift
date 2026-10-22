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
    public var id: String { "\(row):\(startColumn)" }
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
    }
    /// Rows around one match, for reading it in place.
    public struct Context: Equatable, Sendable {
        public let id: UUID
        public let pane: Int, row: Int64
        public var lines: [TerminalMuxContextLine]?
    }
    public struct State: Equatable, Sendable {
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
            state.visible = visible; return (waker, observer, state)
        }
        notify?(snapshot)
        wake?()
    }

    /// Searches `panes` (the machine's, by id); `current.search` fills in as
    /// each answers. A new search replaces the last one.
    @discardableResult
    public func search(_ query: SearchQuery, panes: [Int]) -> UUID {
        let id = UUID()
        let search = Search(id: id, query: query, panes: panes, pending: Set(panes))
        update { state in state.search = search }
        lock.withLock { pendingSearch = search }
        wake()
        return id
    }

    public func clearSearch() {
        lock.withLock { pendingSearch = nil; pendingContext = nil }
        update { state in state.search = nil; state.context = nil }
    }

    /// Reads the rows around `row` of `pane` into `current.context`.
    public func readContext(pane: Int, row: Int64, before: Int = 12, after: Int = 12) {
        let id = UUID(), first = row - Int64(max(before, 0))
        update { state in state.context = Context(id: id, pane: pane, row: row, lines: nil) }
        lock.withLock { pendingContext = (id, first, max(before, 0) + max(after, 0) + 1) }
        wake()
    }

    public func clearContext() {
        lock.withLock { pendingContext = nil }
        update { state in state.context = nil }
    }

    private func update(_ change: (inout State) -> Void) {
        let (notify, snapshot) = lock.withLock { () -> ((@Sendable (State) -> Void)?, State) in
            change(&state); return (observer, state)
        }
        notify?(snapshot)
    }

    private func wake() { lock.withLock { waker }?() }

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
            guard state.search?.id == id else { return }
            state.search?.results[pane] = results
            state.search?.pending.remove(pane)
        }
    }

    public func deliver(context id: UUID, lines: [TerminalMuxContextLine]) {
        update { state in
            guard state.context?.id == id else { return }
            state.context?.lines = lines
        }
    }

    public func setWaker(_ waker: (@Sendable () -> Void)?) { lock.withLock { self.waker = waker } }

    public func takeRequest() -> Int? {
        lock.withLock { defer { requested = nil }; return requested }
    }

    public func publish(panes: [TerminalMuxPane]? = nil, shown: Int?) {
        let (notify, snapshot) = lock.withLock { () -> ((@Sendable (State) -> Void)?, State) in
            if let panes { state.panes = panes }
            state.shown = shown
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
            return (observer, state)
        }
        notify?(snapshot)
    }

    public func remove(pane: Int) {
        let (notify, snapshot) = lock.withLock { () -> ((@Sendable (State) -> Void)?, State) in
            state.panes.removeAll { $0.paneId == pane }
            if state.shown == pane { state.shown = nil }
            state.search?.pending.remove(pane)
            return (observer, state)
        }
        notify?(snapshot)
    }
}
