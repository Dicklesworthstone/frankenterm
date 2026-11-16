import Foundation

/// Two local machine slots, separate from committed keyboard navigation. A
/// split never creates a connection, selects a mux pane or grants input.
public struct TerminalSplitLayout: Equatable, Sendable {
    public private(set) var machineIDs: [String] = []
    public init() {}
    public var isSplit: Bool { machineIDs.count == 2 }

    /// Each column retains at least 360 points. Narrow iPad windows and phones
    /// show only the focused terminal while preserving the user's pair.
    public static func supports(width: Double, isPad: Bool) -> Bool {
        isPad && width.isFinite && width >= 720
    }

    public mutating func open(first: String, second: String, available: [String]) throws {
        guard first != second, available.contains(first), available.contains(second) else {
            throw TerminalNavigationError.unavailable
        }
        machineIDs = [first, second]
    }

    /// A focus transfer within the pair leaves both slots in place. Ordinary
    /// machine navigation replaces only the slot the person was using. Group
    /// changes cannot keep an out-of-group companion alive on screen.
    public mutating func reconcile(previous: String?, selected: String?, available: [String]) {
        guard isSplit else { return }
        guard let selected, available.contains(selected), machineIDs.allSatisfy(available.contains) else {
            close(); return
        }
        guard !machineIDs.contains(selected) else { return }
        guard let previous, let index = machineIDs.firstIndex(of: previous) else { close(); return }
        machineIDs[index] = selected
    }

    public func visibleMachineIDs(selected: String?, expanded: Bool) -> [String] {
        guard let selected else { return [] }
        return expanded && machineIDs.contains(selected) ? machineIDs : [selected]
    }

    public mutating func close() { machineIDs = [] }
}
