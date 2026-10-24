import Foundation

/// Retry accounting for a terminal whose transport dropped. The owner still
/// decides whether reconnecting is permitted and owns every timer/connection.
/// A brief successful handshake must not replenish the budget: a server that
/// repeatedly accepts and drops a shell otherwise creates an endless loop.
public struct TerminalReconnectPolicy: Sendable {
    public static let maximumAttempts = 3
    public static let stableConnectionDuration: Duration = .seconds(60)

    public private(set) var attemptsUsed = 0
    private var liveSince: ContinuousClock.Instant?

    public init() {}

    /// Inspecting or waiting for a retry does not consume it. Only the owner
    /// actually starting a new connection calls beginRetry().
    public var nextDelay: Duration? {
        guard liveSince == nil, attemptsUsed < Self.maximumAttempts else { return nil }
        switch attemptsUsed {
        case 0: return .seconds(1)
        case 1: return .seconds(2)
        default: return .seconds(4)
        }
    }

    @discardableResult
    public mutating func beginRetry() -> Bool {
        guard nextDelay != nil else { return false }
        attemptsUsed += 1
        return true
    }

    /// Repeated live-state notifications keep the first timestamp. A view
    /// attachment or settlement notification cannot extend the stability clock.
    public mutating func recordLive(at now: ContinuousClock.Instant = .now) {
        if liveSince == nil { liveSince = now }
    }

    public mutating func recordEnd(at now: ContinuousClock.Instant = .now) {
        if let liveSince, liveSince.duration(to: now) >= Self.stableConnectionDuration {
            attemptsUsed = 0
        }
        liveSince = nil
    }

    /// A deliberate user retry starts a new budget. Returning to the foreground,
    /// selecting a machine or cancelling a timer does not call this method.
    public mutating func resetForUserRequest() {
        attemptsUsed = 0
        liveSince = nil
    }
}
