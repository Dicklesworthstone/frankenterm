import Foundation
import Dispatch

/// Nonblocking process-wide allocation admission. A resource and every in-flight
/// command that uses it retain the same lease; removing an idle cache reference
/// cannot release a reservation still held by a submitted command. Mutable state
/// is lock-protected and availability callbacks run outside that lock.
final class TerminalRenderResourceBudget: @unchecked Sendable {
    final class Lease: Sendable {
        private let budget: TerminalRenderResourceBudget
        fileprivate init(_ budget: TerminalRenderResourceBudget) { self.budget = budget }
        deinit { budget.release() }
    }

    let maximumResources: Int
    private let lock = NSLock()
    private var used = 0
    private var notificationPending = false
    private let onAvailability: @MainActor @Sendable () -> Void

    /// Capacity is a program-owned policy, never a count decoded from a frame.
    init(maximumResources: Int, onAvailability: @escaping @MainActor @Sendable () -> Void) {
        precondition((1...64).contains(maximumResources))
        self.maximumResources = maximumResources; self.onAvailability = onAvailability
    }

    var retainedResources: Int { lock.withLock { used } }
    var hasCapacity: Bool { lock.withLock { used < maximumResources } }

    func acquire() -> Lease? {
        let admitted = lock.withLock {
            guard used < maximumResources else { return false }
            used += 1; return true
        }
        return admitted ? Lease(self) : nil
    }

    private func release() {
        let notify = lock.withLock {
            precondition(used > 0)
            used -= 1
            guard !notificationPending else { return false }
            notificationPending = true; return true
        }
        if notify {
            // One queued wake, not one task per released texture/buffer. Clear
            // the latch before notifying so a release during a callback schedules
            // a later wake. Callers must recheck capacity after a refused attempt
            // too: a release may have happened before they became waiters.
            DispatchQueue.main.async { [self] in
                lock.withLock { notificationPending = false }
                onAvailability()
            }
        }
    }
}
