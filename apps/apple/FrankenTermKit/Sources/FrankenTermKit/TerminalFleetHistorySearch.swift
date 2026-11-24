import Foundation

/// One bounded search of the current group's already-connected mux machines.
/// The caller retains the exact connection owners and advances this coordinator
/// while its panel is visible. It never connects, refreshes inventory, selects
/// panes, or writes terminal input. Each existing mux transport owns its RPCs.
@MainActor
public final class TerminalFleetHistorySearch {
    public static let maximumMachines = 128
    public static let maximumConcurrentMachines = 3
    public static let maximumMatches = 512
    public static let maximumTextBytes = 512 * 1024
    public static let machineTimeout: Duration = .seconds(32)
    public static let queryTimeout: Duration = .seconds(90)

    public enum Failure: Error, Equatable, Sendable, LocalizedError {
        case capacity, invalidQuery, stale
        public var errorDescription: String? {
            switch self {
            case .capacity: "The navigation group exceeds this search's machine or metadata limit."
            case .invalidQuery: "Enter between 1 and 4,096 UTF-8 bytes to search."
            case .stale: "This search source or pane changed. Search again before showing the pane."
            }
        }
    }

    /// Availability is frozen when the query starts. A later connection never
    /// silently joins it. isCurrent must check the exact controller and control,
    /// not just a machine ID or a connection's current display label.
    public struct Source {
        public let machineID: String, machineName: String
        fileprivate let sessionID: UUID?
        fileprivate let control: TerminalMuxControl?
        fileprivate let unavailable: String?
        fileprivate let isCurrent: @MainActor () -> Bool

        public init(machineID: String, machineName: String, sessionID: UUID,
                    control: TerminalMuxControl, isCurrent: @escaping @MainActor () -> Bool) {
            self.machineID = machineID; self.machineName = machineName
            self.sessionID = sessionID; self.control = control
            unavailable = nil; self.isCurrent = isCurrent
        }

        public init(machineID: String, machineName: String, unavailable: String) {
            self.machineID = machineID; self.machineName = machineName
            sessionID = nil; control = nil
            self.unavailable = unavailable.utf8.count <= 512 ? unavailable : "This machine is unavailable."
            isCurrent = { false }
        }
    }

    public enum Status: String, Equatable, Sendable {
        case queued, searching, complete, partial, notSearched, unavailable
        public var label: String {
            switch self {
            case .queued: "Waiting"
            case .searching: "Searching"
            case .complete: "Searched"
            case .partial: "Incomplete"
            case .notSearched: "Not searched"
            case .unavailable: "Unavailable"
            }
        }
    }

    public struct Coverage: Identifiable, Equatable, Sendable {
        public let id: String, machineName: String
        public fileprivate(set) var status: Status
        public fileprivate(set) var totalPanes = 0
        public fileprivate(set) var searchedPanes = 0
        public fileprivate(set) var failedPanes = 0
        public fileprivate(set) var searchedRows = 0
        public fileprivate(set) var issue: String?
    }

    public struct Hit: Identifiable, Sendable {
        public let id: UUID
        public let machineID: String, machineName: String, paneTitle: String
        public let paneID: Int
        public let match: TerminalMuxSearchMatch
        /// Retained text remains readable after a pane closes, but its exact
        /// navigation capability can only expire, never point at a replacement.
        public fileprivate(set) var canNavigate = true
        fileprivate let sessionID: UUID
        fileprivate let plan: TerminalMuxInputRouting.NavigationPlan
    }

    public struct NavigationTarget: Sendable {
        public let machineID: String, machineName: String, paneTitle: String
        public let sessionID: UUID
        public let plan: TerminalMuxInputRouting.NavigationPlan
    }

    private struct CapturedPane {
        let id: Int, title: String
        let plan: TerminalMuxInputRouting.NavigationPlan
    }

    private final class Entry {
        let source: Source
        var coverage: Coverage
        var request: UUID?
        var panes: [CapturedPane] = []
        var harvested = Set<Int>()
        var deadline: ContinuousClock.Instant?

        init(_ source: Source) {
            self.source = source
            coverage = Coverage(id: source.machineID, machineName: source.machineName,
                status: source.control == nil ? .unavailable : .queued, issue: source.unavailable)
        }
    }

    public let id = UUID()
    public private(set) var query: TerminalMuxControl.SearchQuery?
    public private(set) var hits: [Hit] = []
    public private(set) var issue: String?
    public private(set) var revision = UUID()
    public private(set) var isRetired = false
    public var coverage: [Coverage] { entries.map(\.coverage) }
    public var isRunning: Bool { entries.contains { $0.coverage.status == .queued || $0.coverage.status == .searching } }

    private var entries: [Entry]
    private var textBytes = 0
    private let isCurrent: @MainActor () -> Bool
    private let deadline: ContinuousClock.Instant
    private var lastNow: ContinuousClock.Instant
    private var accepting = true

    public init(query: TerminalMuxControl.SearchQuery, sources: [Source],
                at now: ContinuousClock.Instant = .now,
                isCurrent: @escaping @MainActor () -> Bool = { true }) throws {
        guard (1...4096).contains(query.text.utf8.count) else { throw Failure.invalidQuery }
        guard sources.count <= Self.maximumMachines,
              Set(sources.map(\.machineID)).count == sources.count,
              sources.allSatisfy({ (1...128).contains($0.machineID.utf8.count)
                  && $0.machineName.utf8.count <= 1024 }) else { throw Failure.capacity }
        self.query = query; entries = sources.map(Entry.init); self.isCurrent = isCurrent
        lastNow = now; deadline = now.advanced(by: Self.queryTimeout)
        advance(at: now)
    }

    /// A monotonic, deterministic step. Polling this snapshot leaves each
    /// controller's existing presentation observer and query driver in place.
    /// Continue stepping after completion to expire retired sources and panes.
    public func advance(at now: ContinuousClock.Instant = .now) {
        guard !isRetired else { return }
        guard isCurrent() else { retire(); return }
        guard now >= lastNow else { stop(reason: "The search clock changed. Search again."); return }
        lastNow = now
        for entry in entries where entry.source.control != nil && entry.coverage.status != .unavailable {
            if !entry.source.isCurrent() { invalidate(entry) }
        }
        for index in hits.indices where hits[index].canNavigate {
            guard let entry = entries.first(where: { $0.source.machineID == hits[index].machineID }),
                  let control = entry.source.control else { continue }
            if (try? control.inputRouting.validateNavigation(hits[index].plan)) == nil {
                hits[index].canNavigate = false; revision = UUID()
            }
        }
        guard accepting else { return }
        if now >= deadline && isRunning {
            stop(reason: "The 90-second workspace search deadline was reached. Unfinished machines were not fully searched.")
            return
        }
        for entry in entries where entry.request != nil {
            guard let control = entry.source.control, let request = entry.request,
                  let search = control.current.search, search.id == request else {
                finish(entry, status: .partial, reason: "Another search replaced this machine's request.")
                continue
            }
            if let deadline = entry.deadline, now >= deadline {
                finish(entry, status: .partial, reason: "This machine did not finish before its search deadline.")
                continue
            }
            if !collect(search, from: entry, control: control) {
                stop(reason: "The workspace result limit was reached. Narrow the query to search the remaining output.")
                return
            }
            if search.pending.isEmpty {
                let complete = search.issue == nil && !search.limitReached && entry.coverage.failedPanes == 0
                    && entry.coverage.searchedPanes == entry.coverage.totalPanes
                finish(entry, status: complete ? .complete : .partial,
                       reason: search.issue ?? (complete ? nil : entry.coverage.issue ?? "Some panes did not finish searching."))
            }
        }
        if (hits.count == Self.maximumMatches || textBytes == Self.maximumTextBytes) && isRunning {
            stop(reason: "The workspace result limit was reached. Narrow the query to search the remaining output.")
            return
        }
        var active = entries.filter { $0.request != nil }.count
        for entry in entries where entry.coverage.status == .queued {
            guard active < Self.maximumConcurrentMachines else { break }
            start(entry, at: now)
            if entry.request != nil { active += 1 }
        }
    }

    /// Keep the matches already shown while cancelling only this owner's
    /// requests. Already-sent RPCs remain the mux driver's bounded responsibility.
    public func stop() { stop(reason: "Search stopped. Machines marked incomplete or not searched may contain more matches.") }

    /// A dismissed panel/query/scene releases its captured output and authority.
    /// Old replies cannot repopulate the retired owner.
    public func retire() {
        guard !isRetired else { return }
        for entry in entries { release(entry) }
        hits.removeAll(); entries.removeAll(); query = nil; textBytes = 0
        issue = nil; accepting = false; isRetired = true; revision = UUID()
    }

    public func hit(_ id: UUID) -> Hit? {
        guard !isRetired, isCurrent(), let hit = hits.first(where: { $0.id == id }),
              let entry = entries.first(where: { $0.source.machineID == hit.machineID }),
              entry.source.isCurrent() else { return nil }
        return hit
    }

    /// Returns a candidate for the scene's existing input/navigation handoff.
    /// The scene must revalidate after every suspension before showing it.
    public func navigationTarget(_ id: UUID) throws -> NavigationTarget {
        guard let hit = hit(id), hit.canNavigate,
              let entry = entries.first(where: { $0.source.machineID == hit.machineID }),
              entry.source.sessionID == hit.sessionID, let control = entry.source.control else { throw Failure.stale }
        do { try control.inputRouting.validateNavigation(hit.plan) }
        catch { throw Failure.stale }
        return NavigationTarget(machineID: hit.machineID, machineName: hit.machineName, paneTitle: hit.paneTitle,
                                sessionID: hit.sessionID, plan: hit.plan)
    }

    private func start(_ entry: Entry, at now: ContinuousClock.Instant) {
        guard let query, let control = entry.source.control, entry.source.isCurrent() else {
            invalidate(entry); return
        }
        let state = control.current
        guard !state.panes.isEmpty else {
            finish(entry, status: .notSearched, reason: "No discovered mux panes were available when this machine's turn began.")
            return
        }
        guard state.panes.count <= TerminalMuxControl.maximumSearchPanes else {
            finish(entry, status: .notSearched, reason: "This machine has more than 128 panes. Search a smaller scope on that machine.")
            return
        }
        // Capture the pane identity before requesting its output. Merely
        // reusing its numeric ID cannot revive this plan or relabel a reply.
        let ordered = state.panes.filter { $0.paneId == state.shown }
            + state.panes.filter { $0.paneId != state.shown }
        do {
            entry.panes = try ordered.map { pane in
                let title = pane.title.utf8.count <= 2048 && !pane.title.isEmpty ? pane.title : "Pane \(pane.paneId)"
                return CapturedPane(id: pane.paneId, title: title,
                                    plan: try control.inputRouting.navigation(pane: pane.paneId))
            }
        } catch {
            finish(entry, status: .notSearched, reason: "The pane inventory changed before this machine could be searched.")
            return
        }
        guard let request = control.searchIfIdle(query, panes: entry.panes.map(\.id)) else {
            finish(entry, status: .notSearched, reason: "Another search or output reader is using this connection.")
            return
        }
        entry.request = request; entry.deadline = now.advanced(by: Self.machineTimeout)
        entry.coverage.totalPanes = entry.panes.count; entry.coverage.status = .searching
        revision = UUID()
    }

    /// Returns false before retaining anything beyond the aggregate byte/count
    /// budget. Per-machine replies have already passed the mux's wire bounds.
    private func collect(_ search: TerminalMuxControl.Search, from entry: Entry,
                         control: TerminalMuxControl) -> Bool {
        for pane in entry.panes where !entry.harvested.contains(pane.id) {
            guard let result = search.results[pane.id] else { continue }
            entry.harvested.insert(pane.id); revision = UUID()
            if let error = result.error {
                entry.coverage.failedPanes += 1; entry.coverage.issue = entry.coverage.issue ?? error
                continue
            }
            guard (try? control.inputRouting.validateNavigation(pane.plan)) != nil,
                  let sessionID = entry.source.sessionID else {
                entry.coverage.failedPanes += 1
                entry.coverage.issue = entry.coverage.issue ?? "A pane closed or changed during its search. Its reply was discarded."
                continue
            }
            entry.coverage.searchedPanes += 1
            let sum = entry.coverage.searchedRows.addingReportingOverflow(result.searchedRows)
            entry.coverage.searchedRows = sum.overflow ? Int.max : sum.partialValue
            for match in result.matches {
                let bytes = match.text.utf8.count
                guard hits.count < Self.maximumMatches, bytes <= Self.maximumTextBytes - textBytes else { return false }
                hits.append(Hit(id: UUID(), machineID: entry.source.machineID, machineName: entry.source.machineName,
                    paneTitle: pane.title, paneID: pane.id, match: match, sessionID: sessionID, plan: pane.plan))
                textBytes += bytes
            }
        }
        return true
    }

    private func release(_ entry: Entry) {
        if let request = entry.request { entry.source.control?.clearSearch(request) }
        entry.request = nil; entry.deadline = nil; entry.panes.removeAll(); entry.harvested.removeAll()
    }

    private func finish(_ entry: Entry, status: Status, reason: String?) {
        release(entry)
        entry.coverage.status = status
        if let reason { entry.coverage.issue = reason }
        revision = UUID()
    }

    private func invalidate(_ entry: Entry) {
        release(entry)
        hits.removeAll { $0.machineID == entry.source.machineID }
        textBytes = hits.reduce(0) { $0 + $1.match.text.utf8.count }
        entry.coverage.status = .unavailable
        entry.coverage.searchedPanes = 0; entry.coverage.searchedRows = 0
        entry.coverage.issue = "This connection or group membership changed. Its captured matches were discarded."
        revision = UUID()
    }

    private func stop(reason: String) {
        guard !isRetired, accepting else { return }
        accepting = false; issue = reason
        for entry in entries {
            if entry.coverage.status == .queued {
                finish(entry, status: .notSearched, reason: reason)
            } else if entry.coverage.status == .searching {
                finish(entry, status: .partial, reason: reason)
            }
        }
        revision = UUID()
    }
}
