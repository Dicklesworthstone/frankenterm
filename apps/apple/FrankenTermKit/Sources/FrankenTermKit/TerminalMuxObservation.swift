import Foundation

/// The screen's sole mux observation owner. Producers set one dirty bit; they
/// never create UI tasks or retain historical pane lists/search results. One
/// retained worker reads current metadata and routing state on MainActor.
///
/// Metadata and routing use separate locks: their snapshots are not an atomic
/// input-authority grant. The consumer must retain its normal source, visit and
/// rendered-caret checks. Intermediate notifications may be coalesced, including
/// not-ready states; current routing remains authoritative for input admission.
@MainActor
public final class TerminalMuxObservation {
    public typealias Delivery = @MainActor (TerminalMuxControl.State, TerminalMuxInputRouting.State) -> Void
    public private(set) var isRetired = false
    public private(set) var isActive: Bool
    private let control: TerminalMuxControl
    private let wake: Wake
    private var delivery: Delivery?
    private var worker: Task<Void, Never>?
    private var lastMetadata: UUID?
    private var lastRouting: UInt64?

    /// Exclusive screen observation, just like the underlying observe APIs.
    /// Replacing an owner must retire the previous one first. Callbacks retain
    /// only a weak wake handle, never a controller, view, connection or snapshot.
    public init(control: TerminalMuxControl, initiallyActive: Bool = true,
                delivery: @escaping Delivery) {
        self.control = control; self.delivery = delivery; isActive = initiallyActive
        let wake = Wake(active: initiallyActive)
        self.wake = wake
        control.observe { [weak wake] _ in wake?.notify() }
        control.inputRouting.observe { [weak wake] _ in wake?.notify() }
        worker = Task { @MainActor [weak self, wake] in
            while await wake.signal.wait() {
                // Give a burst a chance to coalesce and let other MainActor work
                // run. This is one lifetime worker, not a task for each update.
                await Task.yield()
                guard !Task.isCancelled else { break }
                guard self != nil else { break }
                self?.deliverLatest()
            }
            wake.retire()
        }
    }

    deinit { wake.retire(); worker?.cancel() }

    /// Offscreen hosts keep running their SSH/parser/authority state, but do not
    /// wake MainActor for metadata. Returning publishes one fresh snapshot even
    /// when no network data arrives. It never reconnects or selects a pane.
    public func setActive(_ active: Bool) {
        guard !isRetired, isActive != active else { return }
        isActive = active
        if active { lastMetadata = nil; lastRouting = nil }
        wake.setActive(active)
    }

    /// Synchronous presentation fence. A callback already executing is on the
    /// same actor; queued or future callbacks cannot deliver after this returns.
    /// Retiring an old owner does not unregister a successor's callbacks.
    public func retire() {
        guard !isRetired else { return }
        isRetired = true; isActive = false
        delivery = nil; lastMetadata = nil; lastRouting = nil
        wake.retire(); worker?.cancel()
    }

    /// All callers join the same lifetime worker. No network or parser work is
    /// stopped, and no keyboard or connection authority is owned here.
    public func close() async {
        retire()
        if let worker { await worker.value }
        worker = nil
    }

    private func deliverLatest() {
        guard !isRetired, wake.takeDirty() else { return }
        let metadata = control.current
        let routing = control.inputRouting.current
        guard metadata.revision != lastMetadata || routing.revision != lastRouting else { return }
        // Record before calling out: a reentrant update schedules a successor;
        // a reentrant retire releases the consumer instead of restoring it.
        lastMetadata = metadata.revision; lastRouting = routing.revision
        delivery?(metadata, routing)
    }

    /// Only this bounded gate crosses producer executors. Disabling it does not
    /// drop the latest-state obligation; it prevents hidden-host wake storms.
    private final class Wake: @unchecked Sendable {
        let signal = TerminalSessionSignal()
        private let lock = NSLock()
        private var active: Bool
        private var dirty = false, retired = false
        init(active: Bool) { self.active = active }
        func notify() {
            let notify = lock.withLock {
                guard !retired else { return false }
                dirty = true
                return active
            }
            if notify { signal.notify() }
        }
        func setActive(_ value: Bool) {
            let notify = lock.withLock {
                guard !retired else { return false }
                active = value
                if value { dirty = true }
                return value
            }
            if notify { signal.notify() }
        }
        func takeDirty() -> Bool {
            lock.withLock {
                guard !retired, active, dirty else { return false }
                dirty = false
                return true
            }
        }
        func retire() {
            lock.withLock { retired = true; active = false; dirty = false }
            signal.finish()
        }
    }
}
