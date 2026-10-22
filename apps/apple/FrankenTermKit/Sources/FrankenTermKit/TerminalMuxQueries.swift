import Foundation

/// Serialized by its owning mux transport, not an independent executor. Query
/// replacement cancels unsent work; it cannot cancel an RPC the server already
/// owns. Such requests remain charged, even after timeout, until their response
/// arrives or the connection closes. Rapid typing cannot reset this budget.
public final class TerminalMuxQueryDriver {
    public static let maximumSearchesInFlight = 3
    public static let requestTimeout: Duration = .seconds(10)
    public static let searchTimeout: Duration = .seconds(30)
    private let client: any TerminalMuxClient
    private let control: TerminalMuxControl
    private struct Search {
        let value: TerminalMuxControl.Search
        let deadline: ContinuousClock.Instant
        var next = 0
    }
    private struct Context {
        let id: UUID, pane: Int, first: Int64, count: Int
        let deadline: ContinuousClock.Instant
    }
    private enum Kind { case search, context }
    private struct Request {
        let kind: Kind, id: UUID, pane: Int
        let deadline: ContinuousClock.Instant
        var expired = false
    }
    private var search: Search?, context: Context?
    private var outstanding: [UInt64: Request] = [:]
    private var lastRequest: UInt64 = 0
    private var fault: String?
    private var retired = false

    public init(client: any TerminalMuxClient, control: TerminalMuxControl) {
        self.client = client; self.control = control
    }
    public var outstandingCount: Int { outstanding.count }

    /// The existing network wait uses this deadline; no task per pane, query,
    /// replacement, timeout or reply is required. Expired RPCs stay charged but
    /// cease contributing deadlines, so an unanswered server cannot spin.
    public func waitTimeout(maximum: Duration, now: ContinuousClock.Instant = .now) -> Duration {
        var result = maximum
        let deadlines = outstanding.values.filter { !$0.expired }.map(\.deadline)
            + [search?.deadline, context?.deadline].compactMap { $0 }
        for deadline in deadlines { result = min(result, now.duration(to: deadline)) }
        return max(.nanoseconds(1), result)
    }

    /// Admit at most three search jobs and one row-read per connection. All
    /// requests use this client's monotonically increasing lookup IDs (the
    /// existing native mux contract); an ambiguous admission disables only
    /// query dispatch, never the terminal's input or rendering.
    public func step(now: ContinuousClock.Instant = .now) {
        guard !retired else { return }
        if let wanted = control.takeSearch() { search = Search(value: wanted, deadline: now + Self.searchTimeout) }
        if let wanted = control.takeContext() {
            context = Context(id: wanted.id, pane: wanted.pane, first: wanted.first,
                              count: wanted.count, deadline: now + Self.requestTimeout)
        }
        expire(now: now)
        var current = control.current
        if current.search?.id != search?.value.id || current.search?.pending.isEmpty != false { search = nil }
        if current.context?.id != context?.id || current.context?.lines != nil { context = nil }
        if let fault {
            if let search { control.finishSearch(search.value.id, reason: fault); self.search = nil }
            if let context { control.deliver(context: context.id, lines: [], error: fault); self.context = nil }
            return
        }

        // Reserve a separate context slot: three long history searches cannot
        // prevent the reader from opening a result already received.
        if let wanted = context, !outstanding.values.contains(where: { $0.kind == .context }) {
            current = control.current
            if current.context?.id == wanted.id, current.context?.lines == nil {
                do {
                    let id = try client.context(pane: wanted.pane, firstRow: wanted.first, count: wanted.count)
                    try retain(id, request: Request(kind: .context, id: wanted.id, pane: wanted.pane, deadline: wanted.deadline))
                    context = nil
                } catch { disableQueries() }
            }
        }
        guard fault == nil else { return }
        var capacity = Self.maximumSearchesInFlight - outstanding.values.filter { $0.kind == .search }.count
        while capacity > 0, var wanted = search, wanted.next < wanted.value.panes.count {
            let pane = wanted.value.panes[wanted.next]
            wanted.next += 1; search = wanted
            // An observer may have replaced the query or removed a pane during
            // a prior completion. Recheck before EACH synchronous admission.
            current = control.current
            guard current.search?.id == wanted.value.id else { search = nil; break }
            guard current.search?.pending.contains(pane) == true else { continue }
            do {
                let query = wanted.value.query
                let id = try client.search(pane: pane, text: query.text,
                                           caseSensitive: query.caseSensitive, regex: query.regex)
                try retain(id, request: Request(kind: .search, id: wanted.value.id, pane: pane,
                                               deadline: min(wanted.deadline, now + Self.requestTimeout)))
                capacity -= 1
            } catch { disableQueries(); break }
        }
    }

    private func retain(_ id: UInt64, request: Request) throws {
        guard id > lastRequest, outstanding.count < Self.maximumSearchesInFlight + 1 else {
            throw TerminalMuxError.failed("Invalid query request identity.")
        }
        lastRequest = id; outstanding[id] = request
    }

    public func receive(pane: Int, results: TerminalMuxSearchResults, now: ContinuousClock.Instant = .now) {
        guard !retired else { return }
        expire(now: now)
        guard let request = outstanding[results.search] else { return }
        guard request.kind == .search, request.pane == pane else { disableQueries(); return }
        outstanding[results.search] = nil
        guard !request.expired, fault == nil else { return }
        control.deliver(search: request.id, pane: pane, results: results)
    }

    public func receive(pane: Int, context: TerminalMuxContext, now: ContinuousClock.Instant = .now) {
        guard !retired else { return }
        expire(now: now)
        guard let request = outstanding[context.request] else { return }
        guard request.kind == .context, request.pane == pane else { disableQueries(); return }
        outstanding[context.request] = nil
        guard !request.expired, fault == nil else { return }
        control.deliver(context: request.id, lines: context.lines)
    }

    private func expire(now: ContinuousClock.Instant) {
        // Mark before callbacks: synchronous UI reentry cannot notify twice.
        for id in Array(outstanding.keys) {
            guard var request = outstanding[id], !request.expired, now >= request.deadline else { continue }
            request.expired = true; outstanding[id] = request
            switch request.kind {
            case .search:
                control.deliver(search: request.id, pane: request.pane,
                    results: .init(search: id, matches: [], searchedRows: 0,
                                   error: "This pane did not answer before the search deadline."))
            case .context:
                control.deliver(context: request.id, lines: [], error: "The requested rows did not arrive before the deadline.")
            }
        }
        if let wanted = search, now >= wanted.deadline {
            search = nil
            control.finishSearch(wanted.value.id, reason: "Search timed out. Results are incomplete; earlier requests may still be finishing.")
        }
        if let wanted = context, now >= wanted.deadline {
            context = nil
            control.deliver(context: wanted.id, lines: [], error: "The row reader is still waiting for an earlier request to finish.")
        }
    }

    private func disableQueries() {
        let reason = "The mux query could not be safely dispatched or matched. Reconnect to search again."
        fault = reason
        let current = control.current
        if let search = current.search { control.finishSearch(search.id, reason: reason) }
        if let context = current.context, context.lines == nil { control.deliver(context: context.id, lines: [], error: reason) }
        search = nil; context = nil
        // Existing IDs remain charged until real responses or retirement.
    }

    public func retire() {
        guard !retired else { return }
        retired = true; search = nil; context = nil; outstanding.removeAll()
        control.endQueries()
    }
}
