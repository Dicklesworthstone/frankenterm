import Foundation

/// A fleet shortcut captures the session and, when present, exact mux
/// registration before any keyboard or machine handoff can suspend. Checking
/// the same machine name or numeric pane after that wait would be insufficient:
/// a reconnect or observed remove/recreate must retire the old shortcut.
/// This grants navigation only, never input or connection authority.
public struct TerminalFleetVisit: Sendable {
    public let machineID: String
    public let sessionID: UUID?
    public let pane: TerminalMuxInputRouting.NavigationPlan?

    public init(machineID: String, sessionID: UUID?, pane: TerminalMuxInputRouting.NavigationPlan? = nil) throws {
        guard !machineID.isEmpty, machineID.utf8.count <= 128, pane == nil || sessionID != nil else {
            throw TerminalNavigationError.invalidInventory
        }
        self.machineID = machineID; self.sessionID = sessionID; self.pane = pane
    }

    /// Validate after every suspended preparation, using the session currently
    /// installed for `machineID`. A pane plan is never rebuilt from its number.
    public func validate(sessionID: UUID?, routing: TerminalMuxInputRouting? = nil) throws {
        guard sessionID == self.sessionID else { throw TerminalNavigationError.stale }
        if let pane {
            guard let routing else { throw TerminalNavigationError.stale }
            try routing.validateNavigation(pane)
        }
    }
}
