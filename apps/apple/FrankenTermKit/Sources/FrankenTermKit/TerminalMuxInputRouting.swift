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
    private var destination: Destination?
    private var ready = false, retired = false
    private var revision: UInt64 = 0
    private var observer: (@Sendable (State) -> Void)?
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
            guard let selected = destination, next[selected.paneID] != selected.registration else { return nil }
            destination = nil; ready = false
            return advance()
        }
        if let (handler, state) = notification { handler?(state) }
    }
    public func remove(pane: Int) {
        let notification = lock.withLock { () -> ((@Sendable (State) -> Void)?, State)? in
            registrations[pane] = nil
            guard !retired, destination?.paneID == pane else { return nil }
            destination = nil; ready = false
            return advance()
        }
        if let (handler, state) = notification { handler?(state) }
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
            destination = selected; ready = false
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
            retired = true; ready = false; destination = nil; registrations.removeAll()
            let notification = advance()
            observer = nil
            return notification
        }
        if let (handler, state) = notification { handler?(state) }
    }
    private func advance() -> ((@Sendable (State) -> Void)?, State) {
        if revision == UInt64.max {
            retired = true; ready = false; destination = nil; registrations.removeAll()
        } else { revision += 1 }
        return (observer, snapshot())
    }
}
