import Foundation

/// Input ownership inside one authenticated mux connection. A local registration
/// survives title, geometry and ordering changes; an observed removal retires it.
/// This is NOT a server-provided birth identifier. The wire protocol must supply
/// one before unobserved numeric-ID reuse can be distinguished.
public final class TerminalMuxInputRouting: @unchecked Sendable {
    public struct Destination: Hashable, Sendable {
        public let paneID: Int
        fileprivate let owner: UUID, registration: UUID, visit: UUID
    }
    /// A frozen navigation candidate, not input authority. Both the source visit
    /// and the target registration must still exist when the handoff completes.
    /// The inventory witness prevents a drag from acquiring a new neighbour.
    public struct NavigationPlan: Equatable, Sendable {
        public let paneID: Int
        fileprivate let owner: UUID, inventory: UUID, registration: UUID
        fileprivate let source: Destination?
    }
    public enum PointerAction: Equatable, Sendable {
        case click(column: Int, row: Int)
        case wheel(steps: Int, column: Int, row: Int)
    }
    /// A gesture belongs to the exact displayed visit, not a mutable pane ID.
    /// Unlike admitted text, unsent gestures are cancelled when that visit ends.
    public struct PointerInput: Equatable, Sendable {
        public let destination: Destination
        public let action: PointerAction
    }
    public static let maximumPendingPointers = 8
    public static let maximumWheelSteps = 64
    public static let wheelStepsPerTurn = 8
    public struct State: Equatable, Sendable {
        public let revision: UInt64
        public let destination: Destination?
        /// A complete output event for this visit reached the canonical model.
        /// The native responder still needs its own rendered-caret permission.
        public let ready: Bool
        public let retired: Bool
    }
    private let lock = NSLock()
    private let owner = UUID()
    private var registrations: [Int: UUID] = [:]
    private var paneOrder: [Int] = []
    private var inventory = UUID()
    private var destination: Destination?
    private var ready = false, retired = false
    private var revision: UInt64 = 0
    private var observer: (@Sendable (State) -> Void)?
    private var pointers: [PointerInput] = []
    private var pointerWaker: (@Sendable () -> Void)?
    private static let maximumPanes = 4096

    public init() {}
    public var current: State { lock.withLock { snapshot() } }
    private func snapshot() -> State {
        State(revision: revision, destination: destination, ready: ready, retired: retired)
    }
    /// Observer callbacks run outside the lock and can arrive out of order.
    /// A UI executor must reject a snapshot older than current.revision.
    public func observe(_ handler: @escaping @Sendable (State) -> Void) {
        let state = lock.withLock { observer = handler; return snapshot() }
        handler(state)
    }
    public func removeObserver() { lock.withLock { observer = nil } }

    /// Install the existing coalescing transport signal, not a task-per-gesture.
    public func setPointerWaker(_ handler: (@Sendable () -> Void)?) {
        let wake = lock.withLock { () -> (@Sendable () -> Void)? in
            guard !retired else { return nil }
            pointerWaker = handler
            return pointers.isEmpty ? nil : handler
        }
        wake?()
    }
    public var hasPendingPointers: Bool { lock.withLock { !pointers.isEmpty } }
    public var pendingPointerCount: Int { lock.withLock { pointers.count } }

    /// Called synchronously by a source-matching native view. No route is looked
    /// up here on behalf of an old callback: it must supply its captured visit.
    /// Only adjacent, same-cell, same-direction wheel operations coalesce;
    /// clicks and reversals keep their order. A full queue rejects new input.
    @discardableResult
    public func enqueuePointer(_ action: PointerAction, destination selected: Destination) throws -> Bool {
        let admission = try admitPointer(action, destination: selected)
        admission.wake?()
        return admission.accepted
    }

    /// The control owner holds its visibility/mouse-mode lock through this
    /// admission. Return the wake rather than invoking it under either lock:
    /// hide or mode changes cannot slip between the guard and queue mutation,
    /// and a reentrant notification cannot deadlock the control owner.
    func admitPointer(_ action: PointerAction, destination selected: Destination)
        throws -> (accepted: Bool, wake: (@Sendable () -> Void)?) {
        let column: Int, row: Int
        switch action {
        case .click(let x, let y): column = x; row = y
        case .wheel(let steps, let x, let y):
            guard (-Self.maximumWheelSteps...Self.maximumWheelSteps).contains(steps) else {
                throw TerminalInputError.invalidLimits
            }
            if steps == 0 { return (false, nil) }
            column = x; row = y
        }
        guard (0..<512).contains(column), (0..<512).contains(row) else { throw TerminalInputError.invalidLimits }
        let wake = try lock.withLock { () -> (@Sendable () -> Void)? in
            guard !retired, ready, destination == selected,
                  registrations[selected.paneID] == selected.registration else { throw TerminalInputError.stale }
            if case .wheel(let steps, let x, let y) = action,
               let last = pointers.last, last.destination == selected,
               case .wheel(let previous, let oldX, let oldY) = last.action,
               oldX == x, oldY == y, (previous > 0) == (steps > 0),
               (-Self.maximumWheelSteps...Self.maximumWheelSteps).contains(previous + steps) {
                pointers[pointers.count - 1] = PointerInput(destination: selected,
                    action: .wheel(steps: previous + steps, column: x, row: y))
            } else {
                guard pointers.count < Self.maximumPendingPointers else { throw TerminalInputError.capacity }
                pointers.append(PointerInput(destination: selected, action: action))
            }
            return pointerWaker
        }
        return (true, wake)
    }

    /// The sole transport takes one operation only AFTER prior protocol egress
    /// drains. No await may intervene between taking it and encoding that exact
    /// target. A throwing/ambiguous encode is never retried automatically.
    /// Wheel batches are split without moving a later click ahead of their tail.
    public func takePointer() -> PointerInput? {
        lock.withLock {
            guard !retired, ready, let first = pointers.first,
                  first.destination == destination,
                  registrations[first.destination.paneID] == first.destination.registration else {
                pointers.removeAll(); return nil
            }
            if case .wheel(let steps, let column, let row) = first.action,
               steps > Self.wheelStepsPerTurn || steps < -Self.wheelStepsPerTurn {
                let taken = steps > 0 ? Self.wheelStepsPerTurn : -Self.wheelStepsPerTurn
                pointers[0] = PointerInput(destination: first.destination,
                    action: .wheel(steps: steps - taken, column: column, row: row))
                return PointerInput(destination: first.destination,
                    action: .wheel(steps: taken, column: column, row: row))
            }
            pointers.removeFirst()
            return first
        }
    }

    /// Hiding, opening an inspection or changing geometry cancels only unsent
    /// gestures. It cannot retract bytes already accepted by the native writer.
    public func discardPointers() { lock.withLock { pointers.removeAll() } }

    /// Validate the whole bounded inventory before replacing registrations.
    /// The transport calls this before publishing the same inventory to the UI.
    public func replaceInventory(_ panes: [Int]) throws {
        guard panes.count <= Self.maximumPanes, panes.allSatisfy({ $0 >= 0 }),
              Set(panes).count == panes.count else { throw TerminalInputError.invalidIdentity }
        let notification = try lock.withLock { () -> ((@Sendable (State) -> Void)?, State)? in
            guard !retired else { throw TerminalInputError.unavailable }
            var next: [Int: UUID] = [:]
            for pane in panes { next[pane] = registrations[pane] ?? UUID() }
            registrations = next
            if paneOrder != panes { paneOrder = panes; inventory = UUID() }
            guard let selected = destination, next[selected.paneID] != selected.registration else { return nil }
            destination = nil; ready = false; pointers.removeAll()
            return advance()
        }
        if let (handler, state) = notification { handler?(state) }
    }
    public func remove(pane: Int) {
        let notification = lock.withLock { () -> ((@Sendable (State) -> Void)?, State)? in
            if registrations.removeValue(forKey: pane) != nil {
                paneOrder.removeAll { $0 == pane }; inventory = UUID()
            }
            guard !retired, destination?.paneID == pane else { return nil }
            destination = nil; ready = false; pointers.removeAll()
            return advance()
        }
        if let (handler, state) = notification { handler?(state) }
    }

    /// Circular movement is confined to this machine. An empty, one-pane, or
    /// missing-selection ring has no neighbour, never an implicit machine move.
    public func navigation(forward: Bool) throws -> NavigationPlan? {
        try lock.withLock {
            guard !retired else { throw TerminalInputError.unavailable }
            guard paneOrder.count > 1, let source = destination,
                  let index = paneOrder.firstIndex(of: source.paneID) else { return nil }
            let next = (index + (forward ? 1 : paneOrder.count - 1)) % paneOrder.count
            return try navigationLocked(pane: paneOrder[next])
        }
    }

    /// Capture a picker or gesture's exact target before any async keyboard work.
    /// Titles, mouse-mode changes and output readiness do not invalidate it.
    public func navigation(pane: Int) throws -> NavigationPlan {
        try lock.withLock { try navigationLocked(pane: pane) }
    }
    private func navigationLocked(pane: Int) throws -> NavigationPlan {
        guard !retired, let registration = registrations[pane] else { throw TerminalInputError.unavailable }
        return NavigationPlan(paneID: pane, owner: owner, inventory: inventory,
                              registration: registration, source: destination)
    }

    /// Check again after keyboard settlement and when the sole transport takes
    /// the request. A -> B -> A, reordering, removal/reuse and a new connection
    /// cannot reinterpret an earlier drag. This grants no input readiness.
    public func validateNavigation(_ plan: NavigationPlan) throws {
        try lock.withLock {
            guard !retired, plan.owner == owner, plan.inventory == inventory,
                  plan.source == destination, registrations[plan.paneID] == plan.registration else {
                throw TerminalInputError.stale
            }
        }
    }

    /// Select is not an input acknowledgement. A new visit starts closed until
    /// its output has been consumed; A -> B -> A never revives A's old focus.
    @discardableResult
    public func select(pane: Int) throws -> Destination {
        let (selected, notification) = try lock.withLock { () -> (Destination, ((@Sendable (State) -> Void)?, State)?) in
            guard !retired, let registration = registrations[pane] else { throw TerminalInputError.unavailable }
            if let destination, destination.paneID == pane, destination.registration == registration {
                return (destination, nil)
            }
            let selected = Destination(paneID: pane, owner: owner, registration: registration, visit: UUID())
            destination = selected; ready = false; pointers.removeAll()
            let notification = advance()
            guard !retired else { throw TerminalInputError.sequenceExhausted }
            return (selected, notification)
        }
        if let (handler, state) = notification { handler?(state) }
        return selected
    }

    /// A focus may be prepared before the first output arrives, but actual input
    /// admission remains closed. Capturing does not change selection or readiness.
    public func capture() throws -> Destination {
        try lock.withLock {
            guard !retired, let destination else { throw TerminalInputError.unavailable }
            return destination
        }
    }
    public func validateAdmission(_ selected: Destination) throws {
        try lock.withLock {
            guard !retired, ready, destination == selected,
                  registrations[selected.paneID] == selected.registration else { throw TerminalInputError.stale }
        }
    }
    /// Previously admitted input and cleanup stay deliverable to their original
    /// live registration, even while another pane is shown. No current-pane lookup.
    public func validateDelivery(_ selected: Destination) throws {
        try lock.withLock {
            guard !retired, selected.owner == owner,
                  registrations[selected.paneID] == selected.registration else { throw TerminalInputError.stale }
        }
    }

    /// Called only by the transport after its pump's model-consumption callback.
    /// Delayed output from a previous visit, closed pane or connection is inert.
    public func didApplyOutput(_ selected: Destination) {
        let notification = lock.withLock { () -> ((@Sendable (State) -> Void)?, State)? in
            guard !retired, !ready, destination == selected,
                  registrations[selected.paneID] == selected.registration else { return nil }
            ready = true
            return advance()
        }
        if let (handler, state) = notification { handler?(state) }
    }

    /// Synchronous and idempotent: old writers cannot survive native teardown.
    public func retire() {
        let notification = lock.withLock { () -> ((@Sendable (State) -> Void)?, State)? in
            guard !retired else { return nil }
            retired = true; ready = false; destination = nil; registrations.removeAll(); paneOrder.removeAll()
            pointers.removeAll(); pointerWaker = nil
            let notification = advance()
            observer = nil
            return notification
        }
        if let (handler, state) = notification { handler?(state) }
    }
    private func advance() -> ((@Sendable (State) -> Void)?, State) {
        if revision == UInt64.max {
            retired = true; ready = false; destination = nil; registrations.removeAll(); paneOrder.removeAll()
            pointers.removeAll(); pointerWaker = nil
        } else { revision += 1 }
        return (observer, snapshot())
    }
}
