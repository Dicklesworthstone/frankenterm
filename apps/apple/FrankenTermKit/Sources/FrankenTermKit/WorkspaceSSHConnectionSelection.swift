import Foundation

public enum WorkspaceSSHConnectionSelectionError: Error, Equatable, Sendable, LocalizedError {
    case stale, routeUnavailable, authenticationUnavailable, hopMismatch

    public var errorDescription: String? {
        switch self {
        case .stale: "The workspace or machine changed. Review this connection choice again."
        case .routeUnavailable: "This SSH route is not ready under the current workspace and host-trust policy."
        case .authenticationUnavailable: "A selected sign-in method or key is no longer available for this account."
        case .hopMismatch: "The ordered jump-host path changed. Review every account's sign-in choice again."
        }
    }
}

/// A deliberate choice for one machine and immutable workspace revision. This
/// value is public metadata, not connection, host-trust, or signer authority.
/// It contains no password, MFA response, private key, or prompt response.
public struct WorkspaceSSHConnectionSelection: Hashable, Sendable {
    public struct Hop: Hashable, Sendable {
        public let routeID: String
        public let authentication: WorkspaceSSHAuthenticationChoice

        public init(routeID: String, authentication: WorkspaceSSHAuthenticationChoice) {
            self.routeID = routeID; self.authentication = authentication
        }
    }

    public let workspaceID: UUID
    public let expectedRevision: UUID
    public let machineID: String
    public let routeID: String
    public let authentication: WorkspaceSSHAuthenticationChoice
    /// One entry for each ordered occurrence, including repeated route IDs.
    /// A dictionary keyed by route ID would collapse distinct SSH namespaces.
    public let hops: [Hop]

    public init(workspaceID: UUID, expectedRevision: UUID, machineID: String, routeID: String,
                authentication: WorkspaceSSHAuthenticationChoice, hops: [Hop] = []) {
        self.workspaceID = workspaceID; self.expectedRevision = expectedRevision
        self.machineID = machineID; self.routeID = routeID
        self.authentication = authentication; self.hops = hops
    }
}

/// Exact current choices for a destination and each transit account. The caller
/// supplies a revision-consistent WorkspaceStore local-policy snapshot and its
/// actual backend support. A transport must rebuild this review from a fresh
/// store read before resolution/networking; this view alone grants no authority.
public struct WorkspaceSSHRouteOptions: Sendable {
    public struct Peer: Sendable {
        public let route: WorkspaceDocument.Route
        public let authenticationChoices: [WorkspaceSSHAuthenticationChoice]
    }

    public let workspaceID: UUID
    public let expectedRevision: UUID
    public let machineID: String
    public let machineName: String
    public let routeID: String
    public let destination: Peer
    public let hops: [Peer]

    public init(localState: WorkspaceLocalState, machineID: String, routeID: String,
                support: WorkspaceDocument.RouteSupport) throws {
        try Task.checkCancellation()
        let document = try localState.workspace.document()
        guard let machine = document.machines.first(where: { $0.id == machineID }),
              machine.routeIDs.contains(routeID),
              let route = document.routes.first(where: { $0.id == routeID }),
              (route.mode == "direct_ssh" && (route.sessionCreation == "on_select" || route.sessionCreation == "manual"))
                || (route.mode == "frankenterm_mux" && (route.attachPolicy ?? "existing_only") == "existing_only"),
              let ready = localState.routes.first(where: { $0.routeID == routeID }), ready.isLocallyReady else {
            throw WorkspaceSSHConnectionSelectionError.routeUnavailable
        }
        let chain = try document.orderedJumpRoutes(forRouteID: routeID)
        guard chain.count <= support.maximumJumpHops else {
            throw WorkspaceSSHConnectionSelectionError.routeUnavailable
        }
        func choices(_ route: WorkspaceDocument.Route, usable: [String]) throws -> [WorkspaceSSHAuthenticationChoice] {
            let choices = WorkspaceSSHAuthenticationChoice.permittedChoices(for: route,
                usableCredentialIDs: usable).filter { support.authenticationMethods.contains($0.method) }
            guard !choices.isEmpty else { throw WorkspaceSSHConnectionSelectionError.authenticationUnavailable }
            return choices
        }
        let destinationChoices = try choices(route, usable: ready.usableCredentialIDs)
        let peers = try chain.map { hop -> Peer in
            // A forwarding-only certificate or route need not permit a PTY or
            // the hop's standalone session policy. Evaluate its actual transit
            // use, with the same available keys and negative trust snapshot.
            let readiness = try document.readiness(forRouteID: hop.id,
                availableCredentialIDs: localState.credentialIDs,
                reviewedTrustIDs: Set(document.hostTrust.map(\.id)), support: support,
                revokedHostKeysByTrustScope: localState.revokedHostKeysByTrustScope,
                requiredHostCapabilitiesByTrustScope: localState.requiredHostCapabilitiesByTrustScope,
                transitForRouteID: routeID)
            guard readiness.isLocallyReady else { throw WorkspaceSSHConnectionSelectionError.routeUnavailable }
            return .init(route: hop, authenticationChoices: try choices(hop, usable: readiness.usableCredentialIDs))
        }
        workspaceID = localState.workspace.id
        expectedRevision = localState.workspace.revision
        self.machineID = machineID; machineName = machine.name; self.routeID = routeID
        destination = .init(route: route, authenticationChoices: destinationChoices)
        hops = peers
    }

    /// Freeze one method/key per account; no automatic alternative is appended.
    /// Choices are explicit even if a route happens to have only one option.
    public func select(authentication: WorkspaceSSHAuthenticationChoice,
                       hopAuthentications: [WorkspaceSSHAuthenticationChoice]) throws -> WorkspaceSSHConnectionSelection {
        guard hopAuthentications.count == hops.count else { throw WorkspaceSSHConnectionSelectionError.hopMismatch }
        let selection = WorkspaceSSHConnectionSelection(workspaceID: workspaceID, expectedRevision: expectedRevision,
            machineID: machineID, routeID: routeID, authentication: authentication,
            hops: zip(hops, hopAuthentications).map { .init(routeID: $0.route.id, authentication: $1) })
        try validate(selection)
        return selection
    }

    /// Revalidation never substitutes another route, key, password method, or
    /// hop. A caller must present the newly computed choices for a fresh review.
    public func validate(_ selection: WorkspaceSSHConnectionSelection) throws {
        try Task.checkCancellation()
        guard selection.workspaceID == workspaceID, selection.expectedRevision == expectedRevision,
              selection.machineID == machineID, selection.routeID == routeID else {
            throw WorkspaceSSHConnectionSelectionError.stale
        }
        guard selection.hops.count == hops.count,
              zip(selection.hops, hops).allSatisfy({ $0.routeID == $1.route.id }) else {
            throw WorkspaceSSHConnectionSelectionError.hopMismatch
        }
        guard destination.authenticationChoices.contains(selection.authentication),
              zip(selection.hops, hops).allSatisfy({ $1.authenticationChoices.contains($0.authentication) }) else {
            throw WorkspaceSSHConnectionSelectionError.authenticationUnavailable
        }
    }
}
