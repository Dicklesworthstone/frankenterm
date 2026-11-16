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
    /// The network changed while live: the end that follows starts a new budget.
    private var networkChangedWhileLive = false

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
        if networkChangedWhileLive { attemptsUsed = 0 }
        else if let liveSince, liveSince.duration(to: now) >= Self.stableConnectionDuration {
            attemptsUsed = 0
        }
        liveSince = nil; networkChangedWhileLive = false
    }

    /// A deliberate user retry starts a new budget. Returning to the foreground,
    /// selecting a machine or cancelling a timer does not call this method.
    public mutating func resetForUserRequest() {
        attemptsUsed = 0
        liveSince = nil; networkChangedWhileLive = false
    }

    /// The network came back or changed interfaces (another Wi-Fi, cellular,
    /// a VPN coming up). Attempts on the old path say nothing about this one,
    /// so a terminal that is not live (dropped, or a retry still in flight)
    /// gets a new budget, and a live one gets it when it next ends.
    public mutating func networkChanged() {
        if liveSince == nil { attemptsUsed = 0 } else { networkChangedWhileLive = true }
    }
}

/// The device's network path, reduced to what decides reconnecting: whether
/// it is usable, and through which interfaces.
public struct TerminalNetworkPath: Equatable, Sendable {
    public let usable: Bool
    public let interfaces: [String]

    public init(usable: Bool, interfaces: [String]) {
        self.usable = usable; self.interfaces = Set(interfaces).sorted()
    }

    /// A hint to retry, never proof that a transport survived: usable now,
    /// and unusable before or through other interfaces. The first path seen
    /// is only a baseline.
    public func isNewRoute(after previous: TerminalNetworkPath?) -> Bool {
        guard usable, let previous else { return false }
        return !previous.usable || previous.interfaces != interfaces
    }
}
