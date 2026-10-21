import Foundation

public enum TerminalNavigationError: Error, Equatable, Sendable {
    case invalidInventory, capacity, stale, unavailable, busy, exhausted
}

/// A tab ID alone is not a session identity. A replacement SSH channel or mux
/// registration must have a new incarnation, even if its tab/name is reused.
public struct TerminalNavigationPaneID: Hashable, Sendable {
    public let id: UUID
    public let incarnation: UUID
    public init(id: UUID, incarnation: UUID) {
        self.id = id; self.incarnation = incarnation
    }
}

public struct TerminalNavigationPane: Equatable, Sendable, Identifiable {
    public let id: TerminalNavigationPaneID
    public let title: String
    public init(id: TerminalNavigationPaneID, title: String) throws {
        guard title.utf8.count <= 4096 else { throw TerminalNavigationError.capacity }
        self.id = id; self.title = title
    }
}

/// The array order is the user's navigation order, never discovery timing or a
/// numeric remote pane ID. An offline machine with no panes stays in the ring.
public struct TerminalNavigationMachine: Equatable, Sendable, Identifiable {
    public let id: String
    public let name: String
    public let panes: [TerminalNavigationPane]
    public init(id: String, name: String, panes: [TerminalNavigationPane]) throws {
        guard !id.isEmpty, id.utf8.count <= 128, name.utf8.count <= 4096,
              id.utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0)
                  || (97...122).contains($0) || [45, 46, 58, 95].contains($0) }) else {
            throw TerminalNavigationError.invalidInventory
        }
        guard panes.count <= 256 else { throw TerminalNavigationError.capacity }
        guard Set(panes.map(\.id)).count == panes.count else { throw TerminalNavigationError.invalidInventory }
        self.id = id; self.name = name; self.panes = panes
    }
}

public struct TerminalNavigationSelection: Equatable, Sendable {
    public let machineID: String
    public let paneID: TerminalNavigationPaneID?
    public init(machineID: String, paneID: TerminalNavigationPaneID?) {
        self.machineID = machineID; self.paneID = paneID
    }
}

public struct TerminalNavigationViewport: Equatable, Sendable {
    /// nil follows live output. This is a local viewing preference, not a PTY size.
    public let topRow: Int64?
    public let fontPointSize: Double
    public init(topRow: Int64? = nil, fontPointSize: Double = 15) throws {
        guard topRow != Int64.min, fontPointSize.isFinite, (6...48).contains(fontPointSize) else {
            throw TerminalNavigationError.unavailable
        }
        self.topRow = topRow; self.fontPointSize = fontPointSize
    }
}

public enum TerminalNavigationAxis: Equatable, Sendable { case machine, pane }
public enum TerminalNavigationDirection: Equatable, Sendable { case previous, next }

/// An immutable preview. Only commit AFTER the source-bound keyboard handoff
/// succeeds. A visual preview grants no input, resize, or connection authority.
public struct TerminalNavigationPlan: Equatable, Sendable {
    fileprivate let owner: UUID
    fileprivate let topology: UInt64
    fileprivate let focus: UInt64
    public let source: TerminalNavigationSelection?
    public let destination: TerminalNavigationSelection
    public let axis: TerminalNavigationAxis
    public let wraps: Bool
}

/// One model per scene and active workspace/group. It has no transport, parser,
/// input queue, or implicit connection action. Inventory changes invalidate old
/// previews; they never redirect an ongoing gesture to a new neighboring pane.
public struct TerminalNavigation: Sendable {
    private let owner = UUID()
    private var topology: UInt64 = 1, focus: UInt64 = 1
    public private(set) var machines: [TerminalNavigationMachine]
    public private(set) var selection: TerminalNavigationSelection?
    private var remembered: [String: TerminalNavigationPaneID] = [:]
    private var viewports: [TerminalNavigationPaneID: TerminalNavigationViewport] = [:]

    public init(machines: [TerminalNavigationMachine], initialMachineID: String? = nil) throws {
        try Self.validate(machines)
        self.machines = machines
        if let initialMachineID {
            guard let machine = machines.first(where: { $0.id == initialMachineID }) else {
                throw TerminalNavigationError.invalidInventory
            }
            selection = .init(machineID: machine.id, paneID: machine.panes.first?.id)
        } else if let first = machines.first {
            selection = .init(machineID: first.id, paneID: first.panes.first?.id)
        }
        if let selection, let pane = selection.paneID { remembered[selection.machineID] = pane }
    }

    private static func validate(_ machines: [TerminalNavigationMachine]) throws {
        guard machines.count <= 128 else { throw TerminalNavigationError.capacity }
        guard Set(machines.map(\.id)).count == machines.count else { throw TerminalNavigationError.invalidInventory }
        var identities = Set<TerminalNavigationPaneID>(), count = 0
        for machine in machines {
            count += machine.panes.count
            guard count <= 4096 else { throw TerminalNavigationError.capacity }
            for pane in machine.panes {
                guard identities.insert(pane.id).inserted else { throw TerminalNavigationError.invalidInventory }
            }
        }
    }

    /// Returns true when the old selected source disappeared. The caller must
    /// seal that source's input immediately. No replacement source is selected:
    /// a surviving machine becomes an empty view until an explicit selection.
    @discardableResult
    public mutating func replaceInventory(_ incoming: [TerminalNavigationMachine]) throws -> Bool {
        try Self.validate(incoming)
        guard incoming != machines else { return false }
        guard topology < UInt64.max, focus < UInt64.max else { throw TerminalNavigationError.exhausted }
        let old = selection
        let map = Dictionary(uniqueKeysWithValues: incoming.map { ($0.id, $0) })
        let panes = Set(incoming.flatMap { $0.panes.map(\.id) })
        if let selected = selection {
            if let machine = map[selected.machineID] {
                if let pane = selected.paneID, !machine.panes.contains(where: { $0.id == pane }) {
                    selection = .init(machineID: selected.machineID, paneID: nil)
                }
            } else { selection = nil }
        }
        remembered = remembered.filter { machine, pane in map[machine]?.panes.contains(where: { $0.id == pane }) == true }
        viewports = viewports.filter { panes.contains($0.key) }
        machines = incoming; topology += 1
        if old != selection { focus += 1 }
        return old != selection
    }

    /// Invalidates in-progress async work without forgetting what was viewed.
    /// Scene suspension must also retire its actual keyboard/focus owner.
    public mutating func invalidatePlans() throws {
        guard focus < UInt64.max else { throw TerminalNavigationError.exhausted }
        focus += 1
    }

    private func destination(_ machine: TerminalNavigationMachine) -> TerminalNavigationSelection {
        let pane = remembered[machine.id].flatMap { remembered in
            machine.panes.first(where: { $0.id == remembered })?.id
        } ?? machine.panes.first?.id
        return .init(machineID: machine.id, paneID: pane)
    }
    private func plan(_ destination: TerminalNavigationSelection, axis: TerminalNavigationAxis,
                      wraps: Bool = false) -> TerminalNavigationPlan? {
        guard destination != selection else { return nil }
        return .init(owner: owner, topology: topology, focus: focus, source: selection,
                     destination: destination, axis: axis, wraps: wraps)
    }

    public func plan(machineID: String) throws -> TerminalNavigationPlan? {
        guard let machine = machines.first(where: { $0.id == machineID }) else { throw TerminalNavigationError.unavailable }
        return plan(destination(machine), axis: .machine)
    }
    public func plan(paneID: TerminalNavigationPaneID) throws -> TerminalNavigationPlan? {
        guard let machine = machines.first(where: { $0.panes.contains(where: { $0.id == paneID }) }) else {
            throw TerminalNavigationError.unavailable
        }
        return plan(.init(machineID: machine.id, paneID: paneID),
                    axis: selection?.machineID == machine.id ? .pane : .machine)
    }
    public func plan(axis: TerminalNavigationAxis, direction: TerminalNavigationDirection,
                     wrap: Bool = true) throws -> TerminalNavigationPlan? {
        guard let selection, let machineIndex = machines.firstIndex(where: { $0.id == selection.machineID }) else {
            return nil
        }
        func neighbor(_ index: Int, count: Int) -> (Int, Bool)? {
            guard count > 1 else { return nil }
            let next = index + (direction == .next ? 1 : -1)
            if next >= 0, next < count { return (next, false) }
            return wrap ? (next < 0 ? count - 1 : 0, true) : nil
        }
        switch axis {
        case .machine:
            guard let (index, wraps) = neighbor(machineIndex, count: machines.count) else { return nil }
            return plan(destination(machines[index]), axis: axis, wraps: wraps)
        case .pane:
            let panes = machines[machineIndex].panes
            guard let selected = selection.paneID, let index = panes.firstIndex(where: { $0.id == selected }) else {
                // A vanished pane is not silently replaced. A picker explicitly
                // selects a surviving source; a stale swipe cannot do so.
                return nil
            }
            guard let (next, wraps) = neighbor(index, count: panes.count) else { return nil }
            return plan(.init(machineID: selection.machineID, paneID: panes[next].id), axis: axis, wraps: wraps)
        }
    }

    public func validate(_ plan: TerminalNavigationPlan) throws {
        guard plan.owner == owner, plan.topology == topology, plan.focus == focus,
              plan.source == selection,
              let machine = machines.first(where: { $0.id == plan.destination.machineID }),
              plan.destination.paneID == nil || machine.panes.contains(where: { $0.id == plan.destination.paneID }) else {
            throw TerminalNavigationError.stale
        }
    }
    @discardableResult
    public mutating func commit(_ plan: TerminalNavigationPlan) throws -> TerminalNavigationSelection {
        try validate(plan)
        guard focus < UInt64.max else { throw TerminalNavigationError.exhausted }
        selection = plan.destination; focus += 1
        if let pane = plan.destination.paneID { remembered[plan.destination.machineID] = pane }
        return plan.destination
    }

    /// A delayed pan/zoom callback cannot overwrite another pane's preferences.
    public mutating func remember(_ viewport: TerminalNavigationViewport,
                                  for selected: TerminalNavigationSelection) throws {
        guard selected == selection, let pane = selected.paneID else { throw TerminalNavigationError.stale }
        viewports[pane] = viewport
    }
    public func viewport(for pane: TerminalNavigationPaneID) -> TerminalNavigationViewport? { viewports[pane] }
}

/// Gesture classification is independent of UIKit so finger-count, direction,
/// interruption and settling rules can be tested. Distances are logical points;
/// times are monotonic seconds from one gesture's start, never wall clock time.
public struct TerminalNavigationGesture: Sendable {
    public enum State: Equatable, Sendable {
        case possible, dragging(TerminalNavigationAxis), cancelled, finished
    }
    public private(set) var state: State = .possible
    private var touches: Int
    private var originX: Double = 0, originY: Double = 0
    private var lastTime: Double = 0
    private var displacement: Double = 0
    public init(touches: Int) throws {
        guard (1...2).contains(touches) else { throw TerminalNavigationError.unavailable }
        self.touches = touches
    }
    public mutating func update(touches: Int, x: Double, y: Double, elapsed: Double) -> State {
        guard state != .cancelled, state != .finished else { return state }
        guard x.isFinite, y.isFinite, elapsed.isFinite, elapsed >= lastTime,
              (1...2).contains(touches) else { state = .cancelled; return state }
        lastTime = elapsed
        if touches != self.touches {
            guard state == .possible, self.touches == 1, touches == 2 else {
                state = .cancelled; return state
            }
            // A second finger changes the recognizer's centroid. Do not count
            // that discontinuity as deliberate two-finger translation.
            self.touches = 2; originX = x; originY = y; displacement = 0
            return state
        }
        let dx = x - originX, dy = y - originY
        displacement = dx
        if state == .possible {
            if abs(dy) >= 12, abs(dy) > abs(dx) { state = .cancelled; return state }
            guard abs(dx) >= 12, abs(dx) >= 1.4 * abs(dy) else { return state }
            // Allow a brief second-finger arrival before one-finger ownership.
            guard touches == 2 || elapsed >= 0.08 else { return state }
            state = .dragging(touches == 1 ? .machine : .pane)
        }
        return state
    }
    public mutating func finish(width: Double, velocityX: Double) -> (TerminalNavigationAxis, TerminalNavigationDirection)? {
        defer { state = .finished }
        guard case let .dragging(axis) = state, width.isFinite, width > 0, width <= 16384,
              velocityX.isFinite else { return nil }
        let projected = displacement + min(2400, max(-2400, velocityX)) * 0.12
        let threshold = max(28, min(120, width * 0.22))
        // A short reverse flick cancels rather than selecting an unseen opposite
        // neighbor. Tiny/no-motion gestures never become velocity-only switches.
        guard abs(displacement) >= 12, abs(projected) >= threshold,
              projected.sign == displacement.sign else { return nil }
        return (axis, projected < 0 ? .next : .previous)
    }
    public mutating func cancel() { state = .cancelled }
}
