import Foundation

/// A bounded, immutable index of already-known machines and mux panes. Building
/// or searching it performs no discovery, authentication, terminal creation, or
/// rendering. A result is a navigation candidate, never permission to send input.
public struct TerminalFleetOverview: Identifiable, Sendable {
    public enum Status: String, CaseIterable, Sendable {
        case live, ready, connecting, attention, disconnected, unavailable
        public var label: String {
            switch self {
            case .live: "Connected"
            case .ready: "Signed in; terminal not open"
            case .connecting: "Connecting"
            case .attention: "Needs attention"
            case .disconnected: "Disconnected"
            case .unavailable: "Unavailable"
            }
        }
    }
    public enum Scope: String, CaseIterable, Sendable { case all, machines, panes }
    public enum Filter: String, CaseIterable, Sendable { case all, connected, attention }
    public enum Failure: Error, Equatable, Sendable, LocalizedError {
        case capacity, invalidInventory, invalidQuery, stale
        public var errorDescription: String? {
            switch self {
            case .capacity: "The discovered workspace exceeds this overview's local size limit."
            case .invalidInventory: "The discovered machine or pane inventory is inconsistent. Refresh the overview."
            case .invalidQuery: "Use at most 512 UTF-8 bytes and 16 search terms. Search is literal, not a regular expression."
            case .stale: "This result is no longer current. Refresh the overview and choose again."
            }
        }
    }
    public struct Pane: Sendable {
        public let plan: TerminalMuxInputRouting.NavigationPlan
        public let title: String, tabTitle: String, workingDirectory: String
        public init(plan: TerminalMuxInputRouting.NavigationPlan, title: String,
                    tabTitle: String = "", workingDirectory: String = "") {
            self.plan = plan; self.title = title; self.tabTitle = tabTitle; self.workingDirectory = workingDirectory
        }
    }
    public struct Machine: Sendable {
        public let id: String, name: String, endpoint: String
        public let status: Status
        public let sessionID: UUID?
        public let panes: [Pane]
        public let selectedPaneID: Int?
        public init(id: String, name: String, endpoint: String, status: Status,
                    sessionID: UUID? = nil, panes: [Pane] = [], selectedPaneID: Int? = nil) {
            self.id = id; self.name = name; self.endpoint = endpoint; self.status = status
            self.sessionID = sessionID; self.panes = panes; self.selectedPaneID = selectedPaneID
        }
    }
    public struct Query: Hashable, Sendable {
        public var text: String
        public var scope: Scope
        public var filter: Filter
        public init(text: String = "", scope: Scope = .all, filter: Filter = .all) {
            self.text = text; self.scope = scope; self.filter = filter
        }
    }
    /// Only this exact snapshot can resolve a choice. Identical names, row
    /// positions, or pane numbers in a replacement index do not revive it.
    public struct Choice: Hashable, Sendable {
        fileprivate let owner: UUID, offset: Int
    }
    public enum Target: Equatable, Sendable {
        case machine(String)
        case pane(machineID: String, sessionID: UUID, plan: TerminalMuxInputRouting.NavigationPlan)
    }
    public struct Row: Identifiable, Sendable {
        public let id: Choice
        public let machineID: String, machineName: String, title: String, detail: String
        public let status: Status
        public let paneID: Int?
        public let selected: Bool
    }
    public struct Results: Sendable {
        public let rows: [Row]
        /// Count before the display limit, after all query and scope filters.
        public let matchingCount: Int
        public var limited: Bool { matchingCount > rows.count }
    }
    private struct IndexedRow: Sendable {
        let row: Row, target: Target
        let machineSearch: String, paneSearch: String
    }
    public static let maximumMachines = 128
    public static let maximumPanes = 4096
    public static let maximumTextBytes = 2 * 1024 * 1024
    public static let maximumResults = 512
    public let id: UUID
    public let machineCount: Int, paneCount: Int
    private let entries: [IndexedRow]

    public init(machines: [Machine], selectedMachineID: String? = nil) throws {
        guard machines.count <= Self.maximumMachines else { throw Failure.capacity }
        let owner = UUID()
        var entries: [IndexedRow] = [], identifiers = Set<String>()
        var paneCount = 0, textBytes = 0, indexedBytes = 0
        func charge(_ value: String, maximum: Int) throws {
            let count = value.utf8.count
            guard count <= maximum, count <= Self.maximumTextBytes - textBytes else { throw Failure.capacity }
            textBytes += count
        }
        func index(_ value: String) throws -> String {
            let folded = Self.fold(value)
            guard folded.utf8.count <= Self.maximumTextBytes - indexedBytes else { throw Failure.capacity }
            indexedBytes += folded.utf8.count
            return folded
        }
        for machine in machines {
            try Task.checkCancellation()
            guard !machine.id.isEmpty, identifiers.insert(machine.id).inserted else { throw Failure.invalidInventory }
            try charge(machine.id, maximum: 128)
            try charge(machine.name, maximum: 1024)
            try charge(machine.endpoint, maximum: 2048)
            guard machine.panes.count <= Self.maximumPanes - paneCount else { throw Failure.capacity }
            guard machine.panes.isEmpty || (machine.sessionID != nil && machine.status == .live) else {
                throw Failure.invalidInventory
            }
            let machineSearch = try index(machine.id + "\n" + machine.name + "\n" + machine.endpoint)
            let machineRow = Row(id: Choice(owner: owner, offset: entries.count), machineID: machine.id,
                machineName: machine.name, title: machine.name, detail: machine.endpoint,
                status: machine.status, paneID: nil, selected: machine.id == selectedMachineID)
            entries.append(IndexedRow(row: machineRow, target: .machine(machine.id),
                machineSearch: machineSearch, paneSearch: ""))
            var paneIDs = Set<Int>()
            for pane in machine.panes {
                try Task.checkCancellation()
                guard pane.plan.paneID >= 0, paneIDs.insert(pane.plan.paneID).inserted,
                      let sessionID = machine.sessionID else { throw Failure.invalidInventory }
                try charge(pane.title, maximum: 2048)
                try charge(pane.tabTitle, maximum: 2048)
                try charge(pane.workingDirectory, maximum: 4096)
                let fallback = "Pane \(pane.plan.paneID)"
                let title = pane.title.trimmingCharacters(in: .whitespacesAndNewlines)
                let detail = [pane.tabTitle, pane.workingDirectory].filter { !$0.isEmpty }.joined(separator: " · ")
                let paneSearch = try index(fallback + "\n" + pane.title + "\n" + pane.tabTitle + "\n" + pane.workingDirectory)
                let row = Row(id: Choice(owner: owner, offset: entries.count), machineID: machine.id,
                    machineName: machine.name, title: title.isEmpty ? fallback : title, detail: detail,
                    status: machine.status, paneID: pane.plan.paneID,
                    selected: machine.id == selectedMachineID && machine.selectedPaneID == pane.plan.paneID)
                entries.append(IndexedRow(row: row,
                    target: .pane(machineID: machine.id, sessionID: sessionID, plan: pane.plan),
                    machineSearch: machineSearch, paneSearch: paneSearch))
            }
            paneCount += machine.panes.count
        }
        id = owner; self.machineCount = machines.count; self.paneCount = paneCount; self.entries = entries
    }

    /// All terms must match the machine's name/ID/endpoint or the pane's
    /// title/tab/directory. Literal, case/diacritic-insensitive search preserves
    /// imported machine order and the captured remote pane order. Status filters
    /// are explicit; disconnected machines are never silently skipped.
    public func search(_ query: Query = Query(), limit: Int = 256) throws -> Results {
        guard (1...Self.maximumResults).contains(limit) else { throw Failure.capacity }
        guard query.text.utf8.count <= 512 else { throw Failure.invalidQuery }
        let terms = Self.fold(query.text).split(whereSeparator: \.isWhitespace).map(String.init)
        guard terms.count <= 16 else { throw Failure.invalidQuery }
        try Task.checkCancellation()
        var rows: [Row] = [], count = 0
        for entry in entries {
            try Task.checkCancellation()
            let row = entry.row
            if query.scope == .machines && row.paneID != nil { continue }
            if query.scope == .panes && row.paneID == nil { continue }
            if query.filter == .connected && row.status != .live && row.status != .ready { continue }
            if query.filter == .attention && row.status != .attention { continue }
            guard terms.allSatisfy({ entry.machineSearch.contains($0) || entry.paneSearch.contains($0) }) else { continue }
            count += 1
            if rows.count < limit { rows.append(row) }
        }
        return Results(rows: rows, matchingCount: count)
    }

    public func resolve(_ choice: Choice) throws -> Target {
        guard choice.owner == id, entries.indices.contains(choice.offset) else { throw Failure.stale }
        return entries[choice.offset].target
    }

    /// A pane choice is tied to the actual controller and the routing owner's
    /// captured registration/visit/order. Revalidate again after any UI await;
    /// this check alone is not an input handoff or a remote authorization.
    public func validatePane(_ choice: Choice, sessionID: UUID,
                             routing: TerminalMuxInputRouting) throws -> TerminalMuxInputRouting.NavigationPlan {
        guard case let .pane(_, expected, plan) = try resolve(choice), expected == sessionID else {
            throw Failure.stale
        }
        do { try routing.validateNavigation(plan) }
        catch { throw Failure.stale }
        return plan
    }

    private static func fold(_ text: String) -> String {
        text.folding(options: [.caseInsensitive, .diacriticInsensitive], locale: Locale(identifier: "en_US_POSIX"))
    }
}
