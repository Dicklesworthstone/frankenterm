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

/// A comparison of validated public configuration. Entity arrays are matched by
/// their stable IDs, never by array offsets, so a serializer reorder cannot make
/// one machine appear to have inherited another machine's credentials or trust.
/// Values remain exact bounded JSON (including integer tokens); no private key
/// material, credentials from the vault, or executable source data enters it.
public struct WorkspaceChangeReview: Sendable {
    public enum Entity: String, Sendable {
        case workspace, source, preferences, group, machine, route, credential, hostTrust, theme, portability
        public var title: String {
            switch self {
            case .workspace: "Workspace"
            case .source: "Claimed source"
            case .preferences: "Workspace preferences"
            case .group: "Group"
            case .machine: "Machine"
            case .route: "Connection route"
            case .credential: "Credential reference"
            case .hostTrust: "Host trust"
            case .theme: "Theme"
            case .portability: "Portability requirement"
            }
        }
    }
    public struct Change: Sendable {
        public let entity: Entity
        public let entityID: String
        public let field: String
        public let previous: JSONValue?
        public let incoming: JSONValue?
        public let affectsSecurity: Bool
        public var fieldTitle: String { field.replacingOccurrences(of: "_", with: " ") }

        /// Present the complete admitted value. Truncating host-pin arrays or
        /// candidate lists would hide exactly the differences being approved.
        public static func description(of value: JSONValue?) -> String {
            guard let value else { return "Not present" }
            if case .string(let text) = value { return text }
            // These values originate exclusively from validated StrictJSON.
            guard let bytes = try? StrictJSON.encode(value) else { return "Value unavailable; review the source document" }
            return String(decoding: bytes, as: UTF8.self)
        }
    }
    public let changes: [Change]
    public var securityChanges: [Change] { changes.filter(\.affectsSecurity) }
    public var requiresSecurityApproval: Bool { changes.contains(where: \.affectsSecurity) }

    public init(previous: WorkspaceDocument, incoming: WorkspaceDocument,
                previousPortability: [AirGapArchive.Portability] = [],
                incomingPortability: [AirGapArchive.Portability] = []) throws {
        guard previous.id == incoming.id, previous.exporterID == incoming.exporterID else {
            throw FTError.malformed("workspace.review_namespace")
        }
        let before = try StrictJSON.decode(previous.raw).object()
        let after = try StrictJSON.decode(incoming.raw).object()
        let oldWorkspace = try before.required("workspace").object()
        let newWorkspace = try after.required("workspace").object()
        var result: [Change] = []

        func append(entity: Entity, id: String, field: String, old: JSONValue?, new: JSONValue?) throws {
            guard old != new else { return }
            // Two admitted documents contain at most 65,536 JSON nodes. Each
            // reported field owns a disjoint subtree; never truncate a review.
            guard result.count < 65_536 else { throw FTError.limit("workspace.review_changes") }
            let security: Bool
            switch entity {
            case .route, .credential, .hostTrust: security = true
            case .portability:
                let priorDisposition = try old?.object()["disposition"]?.uint()
                let nextDisposition = try new?.object()["disposition"]?.uint()
                security = priorDisposition == 1 || nextDisposition == 1
            case .source: security = field == "kind" || field == "extensions"
            case .workspace: security = field == "required_capabilities" || field == "extensions"
            case .preferences: security = field == "connect_on_open" || field == "extensions"
            case .machine:
                security = ["route_refs", "preferred_route_ref", "auto_connect", "extensions"].contains(field)
            case .group:
                if field == "machine_order", let old, let new,
                   case .array(let oldIDs) = old, case .array(let newIDs) = new {
                    // Navigation order is cosmetic; changing active membership
                    // can change which machines an accepted group connects to.
                    security = Set(try oldIDs.map { try $0.string() }) != Set(try newIDs.map { try $0.string() })
                } else { security = ["machine_order", "auto_connect", "extensions"].contains(field) }
            case .theme: security = false
            }
            result.append(.init(entity: entity, entityID: id, field: field, previous: old, incoming: new,
                                affectsSecurity: security))
        }
        func compare(entity: Entity, id: String, old: [String: JSONValue], new: [String: JSONValue],
                     excluding: Set<String> = []) throws {
            for field in Set(old.keys).union(new.keys).subtracting(excluding).sorted() {
                try append(entity: entity, id: id, field: field, old: old[field], new: new[field])
            }
        }
        try compare(entity: .source, id: incoming.exporterID, old: before.required("source").object(),
                    new: after.required("source").object(), excluding: ["exporter_id"])
        try compare(entity: .workspace, id: incoming.id, old: before, new: after,
                    excluding: ["format", "version", "bundle_id", "source", "workspace"])
        let sections: [(String, Entity)] = [("groups", .group), ("machines", .machine), ("routes", .route),
            ("credentials", .credential), ("host_trust", .hostTrust), ("themes", .theme)]
        try compare(entity: .workspace, id: incoming.id, old: oldWorkspace, new: newWorkspace,
                    excluding: Set(sections.map(\.0)).union(["id", "preferences"]))
        try compare(entity: .preferences, id: incoming.id, old: oldWorkspace.required("preferences").object(),
                    new: newWorkspace.required("preferences").object())
        for (section, entity) in sections {
            func records(_ workspace: [String: JSONValue]) throws -> [String: [String: JSONValue]] {
                var indexed: [String: [String: JSONValue]] = [:]
                for value in try workspace.required(section).array() {
                    let record = try value.object(), id = try record.text("id")
                    guard indexed.updateValue(record, forKey: id) == nil else { throw FTError.malformed("workspace.review_id") }
                }
                return indexed
            }
            let old = try records(oldWorkspace), new = try records(newWorkspace)
            for id in Set(old.keys).union(new.keys).sorted() {
                try compare(entity: entity, id: id, old: old[id] ?? [:], new: new[id] ?? [:], excluding: ["id"])
            }
        }
        // Required omissions live in the authenticated archive, outside the
        // public manifest. Removing one can enable a route even when every JSON
        // field is unchanged, so its previous and incoming policy must be shown.
        func conditions(_ records: [AirGapArchive.Portability], document: WorkspaceDocument) throws
            -> [String: [String: JSONValue]] {
            let validated = try AirGapArchive.validatedMetadata(workspace: document, portability: records, annotations: []).portability
            var values: [String: [String: JSONValue]] = [:]
            for record in validated {
                values[record.entityID, default: [:]][record.setting] = .object([
                    "disposition": .number(String(record.disposition)), "reason": .string(record.reason)])
            }
            return values
        }
        let oldConditions = try conditions(previousPortability, document: previous)
        let newConditions = try conditions(incomingPortability, document: incoming)
        for entityID in Set(oldConditions.keys).union(newConditions.keys).sorted() {
            try compare(entity: .portability, id: entityID, old: oldConditions[entityID] ?? [:],
                        new: newConditions[entityID] ?? [:])
        }
        changes = result
    }
}
