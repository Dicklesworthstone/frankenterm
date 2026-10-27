import Foundation

public enum TerminalRouteAttemptPlanError: Error, Equatable, Sendable {
    case invalidRoutes, invalidCredentials, invalidPreference, staleAttempt
}

/// A selected SSH method, not a credential identifier disguised as a password
/// option. Only methods expressly permitted by the imported route can appear.
public enum WorkspaceSSHAuthenticationChoice: Hashable, Sendable {
    case publicKey(String)
    case password
    case keyboardInteractive

    public var credentialID: String? {
        if case let .publicKey(id) = self { return id }
        return nil
    }

    public var requiresForegroundPrompt: Bool {
        switch self {
        case .publicKey: false
        case .password, .keyboardInteractive: true
        }
    }

    /// Choose the first usable permitted method in the saved order. Exhausting
    /// keys never silently downgrades to a password, and keyboard-interactive is
    /// deliberately distinct from the SSH password method.
    public static func choices(for route: WorkspaceDocument.Route,
                               usableCredentialIDs: [String]) -> [Self] {
        for method in route.authenticationMethods {
            switch method {
            case .publicKey:
                let keys = usableCredentialIDs.filter { route.credentialIDs.contains($0) }
                if !keys.isEmpty { return keys.map(Self.publicKey) }
            case .password: return [.password]
            case .keyboardInteractive: return [.keyboardInteractive]
            }
        }
        return []
    }
}

/// The ordered, bounded choices for one machine connection. This value never
/// opens a connection or authorizes a key: every issued attempt still needs the
/// normal current workspace, host-trust and protected-signer checks.
public struct TerminalRouteAttemptPlan: Sendable {
    public struct Route: Hashable, Sendable {
        public let routeID: String
        public let authenticationChoices: [WorkspaceSSHAuthenticationChoice]
        public var credentialIDs: [String] { authenticationChoices.compactMap(\.credentialID) }

        public init(routeID: String, credentialIDs: [String]) {
            self.init(routeID: routeID, authenticationChoices: credentialIDs.map(WorkspaceSSHAuthenticationChoice.publicKey))
        }
        public init(routeID: String, authenticationChoices: [WorkspaceSSHAuthenticationChoice]) {
            self.routeID = routeID; self.authenticationChoices = authenticationChoices
        }
    }

    /// A previously authenticated choice may be preferred on automatic
    /// reconnect, within the same immutable workspace revision. It grants no
    /// authentication authority and must still belong to the supplied routes.
    public struct Selection: Hashable, Sendable {
        public let routeID: String
        public let authentication: WorkspaceSSHAuthenticationChoice
        public var credentialID: String? { authentication.credentialID }

        public init(routeID: String, credentialID: String) {
            self.init(routeID: routeID, authentication: .publicKey(credentialID))
        }
        public init(routeID: String, authentication: WorkspaceSSHAuthenticationChoice) {
            self.routeID = routeID; self.authentication = authentication
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
            (1...10).contains(route.authenticationChoices.count)
                && route.credentialIDs.count <= 8
                && Set(route.authenticationChoices).count == route.authenticationChoices.count
                && route.credentialIDs.allSatisfy({ !$0.isEmpty })
        }) else { throw TerminalRouteAttemptPlanError.invalidCredentials }
        var ordered = routes
        if let preferred {
            guard let index = ordered.firstIndex(where: { $0.routeID == preferred.routeID }),
                  ordered[index].authenticationChoices.contains(preferred.authentication) else {
                throw TerminalRouteAttemptPlanError.invalidPreference
            }
            let selected = ordered.remove(at: index)
            let choices = [preferred.authentication] + selected.authenticationChoices.filter { $0 != preferred.authentication }
            ordered.insert(.init(routeID: selected.routeID, authenticationChoices: choices), at: 0)
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
            // A refusal proves only that this unsigned key was not accepted.
            // It never authorizes a password prompt or a retry of a password.
            guard case .publicKey = attempt.selection.authentication,
                  credentialIndex + 1 < routes[routeIndex].authenticationChoices.count,
                  case .publicKey = routes[routeIndex].authenticationChoices[credentialIndex + 1] else {
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
        .init(selection: .init(routeID: route.routeID, authentication: route.authenticationChoices[credentialIndex]))
    }
}
