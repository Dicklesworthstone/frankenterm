import Foundation
import Dispatch

/// Local presentation policy only. Thermal and battery hints reduce how often
/// replaceable screens are prepared and presented; they never pause terminal
/// parsing, discard transport bytes, or change input or remote geometry.
struct TerminalRenderPowerPolicy: Equatable, Sendable {
    enum ThermalState: CaseIterable, Equatable, Sendable { case nominal, fair, serious, critical }

    let minimumFramesPerSecond: Float
    let maximumFramesPerSecond: Float
    let preferredFramesPerSecond: Float
    let animatesBlink: Bool

    init(thermalState: ThermalState, lowPower: Bool) {
        switch (thermalState, lowPower) {
        case (.critical, _):
            minimumFramesPerSecond = 10; maximumFramesPerSecond = 15; preferredFramesPerSecond = 15
        case (.serious, _):
            minimumFramesPerSecond = 15; maximumFramesPerSecond = 30; preferredFramesPerSecond = 30
        case (_, true):
            minimumFramesPerSecond = 15; maximumFramesPerSecond = 30; preferredFramesPerSecond = 30
        case (.fair, false):
            minimumFramesPerSecond = 30; maximumFramesPerSecond = 60; preferredFramesPerSecond = 60
        case (.nominal, false):
            minimumFramesPerSecond = 30; maximumFramesPerSecond = 120; preferredFramesPerSecond = 60
        }
        // Keep blinking text and the cursor visible at phase zero while idle.
        // Output and deliberate interaction can still request the next frame.
        animatesBlink = !lowPower && (thermalState == .nominal || thermalState == .fair)
    }
}

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

/// One bounded presentation-acquisition episode. Output arriving while the screen
/// is unavailable cannot create another retry or replenish the deadline. A
/// successful presentation, explicit retry, or changed presentation lifecycle
/// starts a new episode. The native owner wakes its existing display scheduler;
/// this value owns neither a task nor a rendering loop.
struct TerminalDrawableRetry: Sendable {
    enum Decision: Equatable, Sendable {
        case schedule(id: UUID, after: TimeInterval)
        case pending
        case exhausted
    }
    enum Wake: Equatable, Sendable { case ready, stale, exhausted }

    private static let delaysMilliseconds = [25, 50, 100, 200, 400, 800]
    private var deadline: ContinuousClock.Instant?
    private var nextDelay = 0
    private var pendingID: UUID?
    private(set) var isExhausted = false
    var isWaiting: Bool { pendingID != nil }

    mutating func unavailable(at now: ContinuousClock.Instant = ContinuousClock.now) -> Decision {
        guard !isExhausted else { return .exhausted }
        guard pendingID == nil else { return .pending }
        if deadline == nil { deadline = now.advanced(by: .seconds(2)) }
        guard nextDelay < Self.delaysMilliseconds.count,
              let deadline,
              now.advanced(by: .milliseconds(Self.delaysMilliseconds[nextDelay])) <= deadline else {
            isExhausted = true
            return .exhausted
        }
        let delay = Self.delaysMilliseconds[nextDelay]
        nextDelay += 1
        let id = UUID()
        pendingID = id
        return .schedule(id: id, after: Double(delay) / 1_000)
    }

    mutating func wake(_ id: UUID, at now: ContinuousClock.Instant = ContinuousClock.now) -> Wake {
        guard pendingID == id else { return .stale }
        pendingID = nil
        guard let deadline, now < deadline else {
            isExhausted = true
            return .exhausted
        }
        return .ready
    }

    mutating func reset() {
        deadline = nil; nextDelay = 0; pendingID = nil; isExhausted = false
    }
}
