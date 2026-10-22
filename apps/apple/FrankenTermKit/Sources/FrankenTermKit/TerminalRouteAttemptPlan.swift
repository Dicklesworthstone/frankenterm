import Foundation

public enum TerminalRouteAttemptPlanError: Error, Equatable, Sendable {
    case invalidRoutes, invalidCredentials, invalidPreference, staleAttempt
}

/// The ordered, bounded choices for one machine connection. This value never
/// opens a connection or authorizes a key: every issued attempt still needs the
/// normal current workspace, host-trust and protected-signer checks.
public struct TerminalRouteAttemptPlan: Sendable {
    public struct Route: Hashable, Sendable {
        public let routeID: String
        public let credentialIDs: [String]

        public init(routeID: String, credentialIDs: [String]) {
            self.routeID = routeID; self.credentialIDs = credentialIDs
        }
    }

    /// A previously authenticated choice may be preferred on automatic
    /// reconnect, within the same immutable workspace revision. It grants no
    /// authentication authority and must still belong to the supplied routes.
    public struct Selection: Hashable, Sendable {
        public let routeID: String, credentialID: String

        public init(routeID: String, credentialID: String) {
            self.routeID = routeID; self.credentialID = credentialID
        }
    }

    /// An exact outstanding attempt. A late or duplicate failure cannot consume
    /// a different key, even when another plan selected the same route and key.
    public struct Attempt: Equatable, Sendable {
        public let selection: Selection
        private let witness = UUID()

        fileprivate init(selection: Selection) { self.selection = selection }
    }

    public enum Failure: Sendable {
        /// The backend proved that the server refused the unsigned public-key
        /// offer before any signing challenge. Partial authentication, signer
        /// failure, or an ambiguous post-signature response must use .terminal.
        case credentialRefused
        /// The route never reached authentication. It may move to another
        /// route only if no server has already refused a key in this plan.
        case unreachable
        case terminal
    }

    public private(set) var current: Attempt?
    public private(set) var hasCredentialRefusal = false
    private var routes: [Route]
    private var routeIndex = 0, credentialIndex = 0

    public init(routes: [Route], preferring preferred: Selection? = nil) throws {
        guard (1...256).contains(routes.count),
              Set(routes.map(\.routeID)).count == routes.count,
              routes.allSatisfy({ !$0.routeID.isEmpty }) else {
            throw TerminalRouteAttemptPlanError.invalidRoutes
        }
        guard routes.allSatisfy({ route in
            (1...8).contains(route.credentialIDs.count)
                && Set(route.credentialIDs).count == route.credentialIDs.count
                && route.credentialIDs.allSatisfy({ !$0.isEmpty })
        }) else { throw TerminalRouteAttemptPlanError.invalidCredentials }
        var ordered = routes
        if let preferred {
            guard let index = ordered.firstIndex(where: { $0.routeID == preferred.routeID }),
                  ordered[index].credentialIDs.contains(preferred.credentialID) else {
                throw TerminalRouteAttemptPlanError.invalidPreference
            }
            let selected = ordered.remove(at: index)
            let keys = [preferred.credentialID] + selected.credentialIDs.filter { $0 != preferred.credentialID }
            ordered.insert(.init(routeID: selected.routeID, credentialIDs: keys), at: 0)
        }
        self.routes = ordered
        current = Self.attempt(route: ordered[0], credentialIndex: 0)
    }

    /// Call only after the exact failed connection has fully closed. A refused
    /// key never falls through to another route/trust scope, including when the
    /// next key's new transport fails. An unreachable route skips its other keys.
    @discardableResult
    public mutating func failed(_ failure: Failure, attempt: Attempt) throws -> Attempt? {
        guard current == attempt else { throw TerminalRouteAttemptPlanError.staleAttempt }
        switch failure {
        case .credentialRefused:
            hasCredentialRefusal = true
            guard credentialIndex + 1 < routes[routeIndex].credentialIDs.count else {
                current = nil; return nil
            }
            credentialIndex += 1
        case .unreachable:
            guard !hasCredentialRefusal, routeIndex + 1 < routes.count else {
                current = nil; return nil
            }
            routeIndex += 1; credentialIndex = 0
        case .terminal:
            current = nil; return nil
        }
        current = Self.attempt(route: routes[routeIndex], credentialIndex: credentialIndex)
        return current
    }

    /// Authentication ends the attempt sequence before a PTY, mux attachment or
    /// any input can be opened. Later channel failures are not key refusals.
    @discardableResult
    public mutating func connected(_ attempt: Attempt) throws -> Selection {
        guard current == attempt else { throw TerminalRouteAttemptPlanError.staleAttempt }
        current = nil
        return attempt.selection
    }

    public mutating func cancel() { current = nil }

    private static func attempt(route: Route, credentialIndex: Int) -> Attempt {
        .init(selection: .init(routeID: route.routeID, credentialID: route.credentialIDs[credentialIndex]))
    }
}
