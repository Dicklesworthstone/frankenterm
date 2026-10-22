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

public enum TerminalMuxEvent: Equatable, Sendable {
    case ready(serverVersion: String)
    case panes([TerminalMuxPane])
    /// Bytes for the pane's terminal, which must be rows x columns first.
    case output(paneID: Int, rows: Int, columns: Int, bytes: Data)
    case paneTitle(paneID: Int, title: String)
    case paneGone(paneID: Int)
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
    public struct State: Equatable, Sendable {
        public var panes: [TerminalMuxPane] = []
        public var shown: Int?
        public var visible = false
    }
    private let lock = NSLock()
    private var state = State()
    private var requested: Int?
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

    // MARK: transport side

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
            return (observer, state)
        }
        notify?(snapshot)
    }
}
