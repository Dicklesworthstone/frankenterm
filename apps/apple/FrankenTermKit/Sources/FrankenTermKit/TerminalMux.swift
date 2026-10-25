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
    /// Where the match is in `text`, as UTF-8 offsets the client computed
    /// from the rows' cells (so wide characters before it do not shift it).
    public let textStart: Int?, textEnd: Int?
    public var id: String { "\(row):\(startColumn):\(endRow):\(endColumn)" }

    public init(row: Int64, startColumn: Int, endRow: Int64, endColumn: Int, text: String,
                textStart: Int? = nil, textEnd: Int? = nil) {
        self.row = row; self.startColumn = startColumn; self.endRow = endRow; self.endColumn = endColumn
        self.text = text; self.textStart = textStart; self.textEnd = textEnd
    }

    /// The matched part of `text`, when its offsets are present and fall on
    /// character boundaries.
    public var matchedRange: Range<String.Index>? {
        let utf8 = text.utf8
        guard let textStart, let textEnd, 0 <= textStart, textStart <= textEnd, textEnd <= utf8.count,
              let lower = utf8.index(utf8.startIndex, offsetBy: textStart).samePosition(in: text),
              let upper = utf8.index(utf8.startIndex, offsetBy: textEnd).samePosition(in: text) else { return nil }
        return lower..<upper
    }
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

/// A pane size as the mux server keeps it, pixels and dpi included.
public struct TerminalMuxSize: Codable, Equatable, Sendable {
    public let rows: Int, cols: Int, pixelWidth: Int, pixelHeight: Int, dpi: Int
    public init(rows: Int, cols: Int, pixelWidth: Int, pixelHeight: Int, dpi: Int) {
        self.rows = rows; self.cols = cols; self.pixelWidth = pixelWidth; self.pixelHeight = pixelHeight; self.dpi = dpi
    }
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
    /// The phone's size (rows x columns) is in effect for the pane, and
    /// `desktop` is the size to give back; or, without `desktop`, no longer.
    case phoneSize(paneID: Int, desktop: TerminalMuxSize?, rows: Int, columns: Int)
    /// The pane's program asked for the mouse (a tap is then a click), or stopped.
    case mouseGrab(paneID: Int, grabbed: Bool)
    /// A pane not on screen has new output.
    case activity(paneID: Int)
    /// The session is over; the channel must close.
    case failed(String)
}

/// The panes a phone left at its own size on one machine, kept across
/// connections: when a connection drops before giving a pane the desktop's
/// size back, the next one does.
public final class TerminalMuxPhoneSizes: @unchecked Sendable {
    public struct Record: Codable, Equatable, Sendable {
        public let desktop: TerminalMuxSize
        public let rows: Int, columns: Int
    }
    /// More panes than any machine has; older entries beyond it are dropped.
    public static let maximumRecords = 256
    private let lock = NSLock()
    private let defaults: UserDefaults
    private let key: String

    public init(workspace: String, machine: String, defaults: UserDefaults = .standard) {
        self.defaults = defaults
        key = "frankenterm.mux.phoneSizes.\(workspace).\(machine)"
    }

    public var records: [Int: Record] { lock.withLock { load() } }

    public func set(pane: Int, desktop: TerminalMuxSize?, rows: Int, columns: Int) {
        lock.withLock {
            var all = load()
            if let desktop { all[pane] = Record(desktop: desktop, rows: rows, columns: columns) }
            else { all[pane] = nil }
            store(all)
        }
    }

    public func removeAll() { lock.withLock { defaults.removeObject(forKey: key) } }

    private func load() -> [Int: Record] {
        guard let data = defaults.data(forKey: key),
              let decoded = try? JSONDecoder().decode([String: Record].self, from: data) else { return [:] }
        return Dictionary(decoded.compactMap { key, value in Int(key).map { ($0, value) } },
                          uniquingKeysWith: { first, _ in first })
    }

    private func store(_ all: [Int: Record]) {
        guard !all.isEmpty else { defaults.removeObject(forKey: key); return }
        let kept = all.sorted { $0.key < $1.key }.suffix(Self.maximumRecords)
        let encoded = Dictionary(kept.map { (String($0.key), $0.value) }, uniquingKeysWith: { first, _ in first })
        if let data = try? JSONEncoder().encode(encoded) { defaults.set(data, forKey: key) }
    }
}

/// The pane the phone was showing on one machine, kept across connections
/// and launches: a connection that dropped while the phone was away comes
/// back to that pane, not to whichever one the desktop has active.
///
/// A pane is the same one only under the same pane, tab and window numbers.
/// A running server never reuses a pane number; one that restarted would have
/// to rebuild the same layout to repeat all three. Nothing typed before is
/// ever replayed into the pane a new connection shows.
public final class TerminalMuxShownPane: @unchecked Sendable {
    public struct Record: Codable, Equatable, Sendable {
        public let paneId: Int, tabId: Int, windowId: Int
        /// The connection ended before the server confirmed input sent to it.
        public var inputUnconfirmed = false
        public init(_ pane: TerminalMuxPane) { paneId = pane.paneId; tabId = pane.tabId; windowId = pane.windowId }
        func names(_ pane: TerminalMuxPane) -> Bool {
            pane.paneId == paneId && pane.tabId == tabId && pane.windowId == windowId
        }
    }
    private let lock = NSLock()
    private let defaults: UserDefaults
    private let key: String

    public init(workspace: String, machine: String, defaults: UserDefaults = .standard) {
        self.defaults = defaults
        key = "frankenterm.mux.shownPane.\(workspace).\(machine)"
    }

    public var record: Record? {
        lock.withLock { defaults.data(forKey: key).flatMap { try? JSONDecoder().decode(Record.self, from: $0) } }
    }

    /// Remembers the pane now shown, or that none is (it closed).
    public func set(_ pane: TerminalMuxPane?) {
        let next = pane.map(Record.init)
        lock.withLock {
            let stored = defaults.data(forKey: key).flatMap { try? JSONDecoder().decode(Record.self, from: $0) }
            guard stored != next else { return }
            if let next, let data = try? JSONEncoder().encode(next) { defaults.set(data, forKey: key) }
            else { defaults.removeObject(forKey: key) }
        }
    }

    /// The connection is ending before the server confirmed input sent to the
    /// remembered pane; the next connection that shows it says so.
    public func markInputUnconfirmed() {
        lock.withLock {
            guard var stored = defaults.data(forKey: key).flatMap({ try? JSONDecoder().decode(Record.self, from: $0) }),
                  !stored.inputUnconfirmed else { return }
            stored.inputUnconfirmed = true
            if let data = try? JSONEncoder().encode(stored) { defaults.set(data, forKey: key) }
        }
    }

    /// The pane a new connection shows from the server's list: the remembered
    /// one while it is still there, else the desktop's active pane. `lost`
    /// says a remembered pane is gone, and `inputUnconfirmed` that the last
    /// connection ended before the pane confirmed input; the screen should
    /// tell either.
    public func choose(from panes: [TerminalMuxPane]) -> (pane: TerminalMuxPane?, lost: Bool, inputUnconfirmed: Bool) {
        let fallback = panes.first(where: \.isActive) ?? panes.first
        guard let record else { return (fallback, false, false) }
        if let same = panes.first(where: record.names) { return (same, false, record.inputUnconfirmed) }
        return (fallback, true, false)
    }
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
    /// A size `restoreSize` gave back is not answered yet: the server may not
    /// have applied it, and closing the channel now can lose it on the way.
    func restoring() throws -> Bool
    /// Input sent to the pane is not answered yet: the server may not have
    /// written it, and closing the channel now can lose it on the way. After
    /// the connection ended, some of it may never have arrived.
    func unansweredInput(pane: Int) throws -> Bool
    /// Searches the pane's scrollback on the server, newest rows first; a
    /// `.searchResults` event with the returned id answers.
    func search(pane: Int, text: String, caseSensitive: Bool, regex: Bool) throws -> UInt64
    /// The text of the pane's rows `firstRow..<firstRow+count`; a `.context`
    /// event with the returned id answers.
    func context(pane: Int, firstRow: Int64, count: Int) throws -> UInt64
    /// Turns a mouse wheel `steps` notches (negative toward older output) over
    /// the pane's screen cell: wheel events for a program that asked for the
    /// mouse, arrow keys for a full-screen one, nothing for others.
    func wheel(pane: Int, steps: Int, column: Int, row: Int) throws
    /// After the pane list arrives: takes over a pane an earlier connection
    /// left at the phone's size, if it still has exactly that size, so that
    /// `restoreSize` gives it `desktop`. True when taken over.
    func adoptPhoneSize(pane: Int, desktop: TerminalMuxSize, rows: Int, columns: Int) throws -> Bool
    /// Clicks the left button on the pane's screen cell, for a program that
    /// asked for the mouse.
    func click(pane: Int, column: Int, row: Int) throws
}

extension TerminalMuxClient {
    /// A client without a mouse ignores clicks.
    public func click(pane: Int, column: Int, row: Int) throws {}
    /// A client without a wheel ignores it.
    public func wheel(pane: Int, steps: Int, column: Int, row: Int) throws {}
    /// A client that keeps no sizes takes over none.
    public func adoptPhoneSize(pane: Int, desktop: TerminalMuxSize, rows: Int, columns: Int) throws -> Bool { false }
    /// A client that keeps no sizes waits on no restore.
    public func restoring() throws -> Bool { false }
    /// A client that tracks no answers waits on none.
    public func unansweredInput(pane: Int) throws -> Bool { false }
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
        /// Why the mux server ended an attached session (a reply this app
        /// could not read, say), when it was not the connection dropping.
        public var failure: String?
        /// What changed while the phone was away that the screen should say,
        /// such as the pane it was showing having closed.
        public var notice: String?
        /// The shown pane's program has asked for the mouse: a tap clicks.
        public var mouseGrabbed = false
        /// Panes with output the phone has not had on screen since it arrived.
        /// One leaves this set when it comes on screen.
        public var unread: Set<Int> = []
        /// A captured request expired before the transport could apply it. The
        /// original presenter can stop waiting without selecting a substitute.
        public var rejectedNavigation: TerminalMuxInputRouting.NavigationPlan?
    }
    private let lock = NSLock()
    private var state = State()
    private var requested: TerminalMuxInputRouting.NavigationPlan?
    private var pendingSearch: Search?
    private var pendingContext: (id: UUID, first: Int64, count: Int)?
    private var waker: (@Sendable () -> Void)?
    private var observer: (@Sendable (State) -> Void)?
    private var unreadObserver: (@Sendable (Bool) -> Void)?
    /// When each pane last left the screen. Giving it its own size back makes
    /// its program redraw, which is not new output.
    private var leftScreen: [Int: ContinuousClock.Instant] = [:]
    private let settle: Duration
    private var queriesEnded = false

    public static let maximumSearchPanes = 128
    public static let maximumSearchMatches = 512
    public static let maximumSearchTextBytes = 512 * 1024
    public static let maximumMatchesPerPane = 128
    public static let maximumContextLines = 256
    public static let maximumContextTextBytes = 128 * 1024
    private static let maximumLineBytes = 16 * 1024

    /// `settle`: how long after a pane leaves the screen its output is taken
    /// for the redraw at its own size rather than news.
    public init(settle: Duration = .seconds(3)) { self.settle = settle }

    public var current: State { lock.withLock { state } }

    /// The screen's change handler, called with each new state.
    public func observe(_ handler: @escaping @Sendable (State) -> Void) {
        let snapshot = lock.withLock { observer = handler; return state }
        handler(snapshot)
    }

    /// Called with whether any pane is unread, each time that changes. Unlike
    /// `observe`, it is for a machine off screen: it changes at most twice for
    /// each time a pane comes on screen or closes.
    public func observeUnread(_ handler: @escaping @Sendable (Bool) -> Void) {
        let unread = lock.withLock { unreadObserver = handler; return !state.unread.isEmpty }
        handler(unread)
    }

    /// The transport saw output on a pane. It is unread unless it is on
    /// screen or only just left it.
    public func noteActivity(pane: Int) {
        let (notify, snapshot, flipped) = lock.withLock { () -> ((@Sendable (State) -> Void)?, State, (@Sendable (Bool) -> Void)?) in
            guard !(state.visible && state.shown == pane), !state.unread.contains(pane),
                  state.panes.contains(where: { $0.paneId == pane }),
                  leftScreen[pane].map({ ContinuousClock.now - $0 >= settle }) ?? true else { return (nil, state, nil) }
            state.unread.insert(pane); state.revision = UUID()
            return (observer, state, state.unread.count == 1 ? unreadObserver : nil)
        }
        notify?(snapshot); flipped?(true)
    }

    /// Under lock: the pane on screen (`shown` while `visible`) is read; one
    /// that leaves the screen starts settling. Returns the unread observer
    /// when the last unread pane was read.
    private func moveScreen(shown: Int?, visible: Bool) -> (@Sendable (Bool) -> Void)? {
        let old = state.visible ? state.shown : nil
        let new = visible ? shown : nil
        if let old, old != new { leftScreen[old] = .now }
        guard let new, state.unread.remove(new) != nil, state.unread.isEmpty else { return nil }
        return unreadObserver
    }

    /// Capture an immediate picker request. Gesture callers capture earlier and
    /// use request(_:) after their existing source-bound keyboard handoff.
    public func request(pane: Int) {
        guard let plan = try? inputRouting.navigation(pane: pane) else { return }
        try? request(plan)
    }

    public func request(_ plan: TerminalMuxInputRouting.NavigationPlan) throws {
        let (wake, notify, snapshot) = try lock.withLock {
            guard !queriesEnded, state.visible,
                  state.panes.contains(where: { $0.paneId == plan.paneID }) else { throw TerminalInputError.stale }
            try inputRouting.validateNavigation(plan)
            inputRouting.discardPointers()
            requested = plan; state.rejectedNavigation = nil; state.revision = UUID()
            return (waker, observer, state)
        }
        notify?(snapshot); wake?()
    }

    /// The view supplies its immutable displayed visit. Neither a delayed
    /// gesture nor a blocked native writer may resolve a new target later.
    public func scroll(steps: Int, column: Int, row: Int,
                       destination: TerminalMuxInputRouting.Destination) throws {
        let admission = try lock.withLock {
            guard !queriesEnded, state.visible, requested == nil,
                  state.shown == destination.paneID else { throw TerminalInputError.stale }
            return try inputRouting.admitPointer(.wheel(steps: steps, column: column, row: row), destination: destination)
        }
        admission.wake?()
    }

    public func click(column: Int, row: Int,
                      destination: TerminalMuxInputRouting.Destination) throws {
        let admission = try lock.withLock {
            guard !queriesEnded, state.visible, state.mouseGrabbed, requested == nil,
                  state.shown == destination.paneID else { throw TerminalInputError.stale }
            return try inputRouting.admitPointer(.click(column: column, row: row), destination: destination)
        }
        admission.wake?()
    }

    /// Whether the shown pane's program has the mouse (transport side).
    public func setMouseGrabbed(_ grabbed: Bool) {
        update { state in
            guard state.mouseGrabbed != grabbed else { return false }
            inputRouting.discardPointers()
            state.mouseGrabbed = grabbed
            return true
        }
    }

    /// Whether the machine is on screen. Only a visible machine's pane is
    /// sized to the phone and drawn; a hidden one returns to its own size.
    public func setVisible(_ visible: Bool) {
        let (wake, notify, snapshot, read) = lock.withLock { () -> ((@Sendable () -> Void)?, (@Sendable (State) -> Void)?, State, (@Sendable (Bool) -> Void)?) in
            guard state.visible != visible else { return (nil, nil, state, nil) }
            if !visible {
                inputRouting.discardPointers()
                if let requested { state.rejectedNavigation = requested; self.requested = nil }
            }
            let read = moveScreen(shown: state.shown, visible: visible)
            state.visible = visible; state.revision = UUID(); return (waker, observer, state, read)
        }
        notify?(snapshot)
        read?(false)
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

    /// The attached session is ending for `reason`; the screen shows it.
    public func recordFailure(_ reason: String) {
        update { state in
            state.failure = Self.boundedError(reason)
            return true
        }
    }

    /// Something the screen should say once about this attachment.
    public func recordNotice(_ notice: String) {
        update { state in
            state.notice = notice
            return true
        }
    }

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
            if let requested { state.rejectedNavigation = requested; self.requested = nil }
            inputRouting.discardPointers()
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

    public func setWaker(_ waker: (@Sendable () -> Void)?) {
        lock.withLock { self.waker = waker }
        inputRouting.setPointerWaker(waker)
    }

    public func takeRequest() -> Int? {
        let (pane, notify, snapshot) = lock.withLock { () -> (Int?, (@Sendable (State) -> Void)?, State) in
            guard let plan = requested else { return (nil, nil, state) }
            requested = nil
            // The sole transport owns inventory and selection; it calls this
            // and selects synchronously, with no actor suspension in between.
            if !queriesEnded, state.visible,
               state.panes.contains(where: { $0.paneId == plan.paneID }),
               (try? inputRouting.validateNavigation(plan)) != nil {
                return (plan.paneID, nil, state)
            }
            state.rejectedNavigation = plan; state.revision = UUID()
            return (nil, observer, state)
        }
        notify?(snapshot)
        return pane
    }

    public func publish(panes: [TerminalMuxPane]? = nil, shown: Int?) {
        let (notify, snapshot, read) = lock.withLock { () -> ((@Sendable (State) -> Void)?, State, (@Sendable (Bool) -> Void)?) in
            let hadUnread = !state.unread.isEmpty
            if let panes {
                let existing = Set(panes.map(\.paneId))
                for old in state.panes where !existing.contains(old.paneId) { removeQueryPane(old.paneId) }
                state.panes = panes
                state.unread.formIntersection(existing)
                leftScreen = leftScreen.filter { existing.contains($0.key) }
            }
            _ = moveScreen(shown: shown, visible: state.visible)
            state.shown = shown
            state.revision = UUID()
            return (observer, state, hadUnread && state.unread.isEmpty ? unreadObserver : nil)
        }
        notify?(snapshot)
        read?(false)
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
        let (notify, snapshot, read) = lock.withLock { () -> ((@Sendable (State) -> Void)?, State, (@Sendable (Bool) -> Void)?) in
            state.panes.removeAll { $0.paneId == pane }
            if state.shown == pane { state.shown = nil }
            removeQueryPane(pane)
            if requested?.paneID == pane {
                state.rejectedNavigation = requested; requested = nil
            }
            let read = state.unread.remove(pane) != nil && state.unread.isEmpty
            leftScreen[pane] = nil
            state.revision = UUID()
            return (observer, state, read ? unreadObserver : nil)
        }
        notify?(snapshot)
        read?(false)
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
