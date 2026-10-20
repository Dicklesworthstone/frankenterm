import Foundation

/// Maps unresolved required conditions to the routes that depend on them.
/// Archive and persistence callers select their required findings before calling;
/// optional annotations and inactive preferences do not grant or remove authority.
public enum WorkspacePortabilityPolicy {
    public static func blockedRouteIDs(in workspace: WorkspaceDocument, requiredEntityIDs: Set<String>) -> Set<String> {
        guard !requiredEntityIDs.isEmpty else { return [] }
        let allRoutes = Set(workspace.routes.map(\.id))
        let knownEntities = Set([workspace.id] + workspace.groups.map(\.id) + workspace.machines.map(\.id)
            + workspace.routes.map(\.id) + workspace.credentials.map(\.id) + workspace.hostTrust.map(\.id)
            + workspace.themes.map(\.id))
        // An unresolved scope cannot safely be interpreted as an unrelated one.
        // Retain its preview finding and block all routes until it is understood.
        guard requiredEntityIDs.isSubset(of: knownEntities), !requiredEntityIDs.contains(workspace.id) else { return allRoutes }

        var affected: [String: Set<String>] = [:]
        var dependents: [String: Set<String>] = [:]
        for route in workspace.routes {
            affected[route.id, default: []].insert(route.id)
            affected[route.trustID, default: []].insert(route.id)
            for credentialID in route.credentialIDs { affected[credentialID, default: []].insert(route.id) }
            for hopID in route.jumpRoutes { dependents[hopID, default: []].insert(route.id) }
        }
        let machines = Dictionary(uniqueKeysWithValues: workspace.machines.map { ($0.id, $0) })
        for machine in workspace.machines {
            affected[machine.id, default: []].formUnion(machine.routeIDs)
            affected[machine.themeID, default: []].formUnion(machine.routeIDs)
        }
        for group in workspace.groups {
            for machineID in group.machineOrder {
                if let machine = machines[machineID] { affected[group.id, default: []].formUnion(machine.routeIDs) }
            }
        }
        var blocked = Set(requiredEntityIDs.flatMap { affected[$0] ?? [] })
        var pending = Array(blocked)
        // Propagate toward destinations, including hops used by other hops.
        // A blocked machine does not disable an otherwise independent hop merely
        // because its route uses that hop; the dependency goes the other way.
        while let routeID = pending.popLast() {
            for dependent in dependents[routeID] ?? [] where blocked.insert(dependent).inserted {
                pending.append(dependent)
            }
        }
        return blocked
    }
}
