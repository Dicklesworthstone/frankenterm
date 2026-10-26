import Foundation

/// Authentication and terminal creation are separate decisions. A manual
/// route can be prepared in the background, but only a deliberate terminal
/// action may open its channel. The actual authenticated route decides.
public enum TerminalSessionCreation: String, Hashable, Sendable {
    case onSelect = "on_select", manual, existingOnly = "existing_only"

    public func permitsOpening(explicit: Bool) -> Bool { explicit || self != .manual }
}

public enum TerminalConnectionIntentError: Error, Equatable, Sendable {
    case invalidInventory, invalidGroup, unknownMachine, notApproved
}

/// Scene-local connection intent for one immutable workspace revision. An
/// inventory or a navigation selection alone does not approve networking.
/// Background preparation belongs to the active, opted-in group; explicit
/// Connect and terminal actions apply to one machine. No imported preference
/// can clear a user disconnect or authorize a replacement terminal after loss.
public struct TerminalConnectionIntent: Sendable {
    public struct Machine: Equatable, Sendable {
        public let id: String, autoConnect: Bool
        public init(id: String, autoConnect: Bool) { self.id = id; self.autoConnect = autoConnect }
    }
    public struct Group: Equatable, Sendable {
        public let id: String, machineIDs: [String], autoConnect: Bool
        public init(id: String, machineIDs: [String], autoConnect: Bool) {
            self.id = id; self.machineIDs = machineIDs; self.autoConnect = autoConnect
        }
    }
    public enum Opening: Equatable, Sendable { case none, onSelect, explicit }

    public private(set) var group: Group?
    public private(set) var selectedMachineID: String?
    public private(set) var disconnected: Set<String> = []
    public private(set) var requiresNewSession: Set<String> = []
    /// Machines whose attachment to existing mux panes was lost. Selecting one
    /// may attach to those panes again, which creates no PTY; a new shell
    /// still needs New Session.
    private var reattachable: Set<String> = []
    private let inventory: [String], known: Set<String>, optedIn: Set<String>
    private var approvedConnections: Set<String> = []
    private var startedConnections: Set<String> = []
    private var explicitTerminals: Set<String> = []

    public init(machines: [Machine], selectedMachineID: String? = nil, group: Group? = nil) throws {
        let ids = machines.map(\.id), unique = Set(ids)
        guard ids.count <= 128, unique.count == ids.count, !ids.contains(where: \.isEmpty),
              selectedMachineID.map(unique.contains) ?? true else { throw TerminalConnectionIntentError.invalidInventory }
        inventory = ids; known = unique
        optedIn = Set(machines.filter(\.autoConnect).map(\.id))
        self.selectedMachineID = selectedMachineID
        try setGroup(group)
    }

    /// Changing groups changes only unsent warming intent. Existing owners
    /// remain owned by their scene, and explicit requests keep their scope.
    public mutating func setGroup(_ value: Group?) throws {
        if let value {
            let ids = Set(value.machineIDs)
            guard !value.id.isEmpty, value.machineIDs.count <= 128,
                  ids.count == value.machineIDs.count, ids.isSubset(of: known) else {
                throw TerminalConnectionIntentError.invalidGroup
            }
        }
        group = value
    }

    /// A committed selection promotes approved work and may open an already
    /// prepared on-select route. It never revives a disconnected machine.
    public mutating func select(_ machineID: String?) throws {
        if let machineID { try check(machineID) }
        selectedMachineID = machineID
    }

    /// Connect authorizes this machine's authentication. A manual route still
    /// requires a distinct Open Terminal action, even after Connect succeeds.
    public mutating func approveConnection(_ machineID: String) throws {
        try check(machineID)
        approvedConnections.insert(machineID); disconnected.remove(machineID)
    }

    /// Open Terminal / New Session is stronger than Connect. A failed opening
    /// consumes no automatic retry privilege; the scene owns each attempt.
    public mutating func requestTerminal(_ machineID: String) throws {
        try approveConnection(machineID)
        explicitTerminals.insert(machineID)
    }

    /// Called only after the scene reserves an actual connection owner. Later
    /// group changes cannot revoke that owner's bounded reconnect permission.
    public mutating func connectionStarted(_ machineID: String) throws {
        try check(machineID)
        guard shouldPrepare(machineID) || permitsReconnect(machineID) else {
            throw TerminalConnectionIntentError.notApproved
        }
        startedConnections.insert(machineID)
    }

    public mutating func terminalOpened(_ machineID: String) throws {
        try check(machineID)
        explicitTerminals.remove(machineID); requiresNewSession.remove(machineID); reattachable.remove(machineID)
    }

    /// A lost or normally ended terminal is not a request for another PTY.
    /// Reauthentication may prepare a transport, but only New Session removes
    /// this gate. Merely returning to its selected chip never does. A lost
    /// attachment to existing mux panes may also attach to those panes again
    /// on selection, since that creates no PTY; the gate still holds for any
    /// route that would start a shell.
    public mutating func terminalEnded(_ machineID: String, reattachesExistingPanes: Bool = false) throws {
        try check(machineID)
        explicitTerminals.remove(machineID); requiresNewSession.insert(machineID)
        if reattachesExistingPanes { reattachable.insert(machineID) } else { reattachable.remove(machineID) }
    }

    public mutating func disconnect(_ machineID: String) throws {
        try check(machineID)
        disconnected.insert(machineID); approvedConnections.remove(machineID)
        startedConnections.remove(machineID); explicitTerminals.remove(machineID); reattachable.remove(machineID)
    }

    public func shouldPrepare(_ machineID: String) -> Bool {
        guard known.contains(machineID), !disconnected.contains(machineID) else { return false }
        if approvedConnections.contains(machineID) { return true }
        guard let group, group.autoConnect, optedIn.contains(machineID) else { return false }
        return group.machineIDs.contains(machineID)
    }

    public func permitsReconnect(_ machineID: String) -> Bool {
        startedConnections.contains(machineID) && !disconnected.contains(machineID)
    }

    /// Only the actual authenticated route is supplied here. In particular,
    /// an on-select preferred route cannot authorize a manual fallback route.
    public func opening(_ machineID: String, creation: TerminalSessionCreation) -> Opening {
        guard known.contains(machineID), !disconnected.contains(machineID),
              startedConnections.contains(machineID) || shouldPrepare(machineID) else { return .none }
        if explicitTerminals.contains(machineID), creation.permitsOpening(explicit: true) { return .explicit }
        guard selectedMachineID == machineID, creation.permitsOpening(explicit: false),
              !requiresNewSession.contains(machineID)
                || (creation == .existingOnly && reattachable.contains(machineID)) else { return .none }
        return .onSelect
    }

    public func connectionOrder(available: Set<String>) throws -> [String] {
        guard available.isSubset(of: known) else { throw TerminalConnectionIntentError.unknownMachine }
        let eligible = available.filter(shouldPrepare)
        let groupOrder = group?.machineIDs ?? []
        let members = Set(groupOrder)
        let order = groupOrder + inventory.filter { !members.contains($0) }
        return try TerminalConnectionOrder.prioritize(order, selected: selectedMachineID, including: eligible)
    }

    private func check(_ machineID: String) throws {
        guard known.contains(machineID) else { throw TerminalConnectionIntentError.unknownMachine }
    }
}
