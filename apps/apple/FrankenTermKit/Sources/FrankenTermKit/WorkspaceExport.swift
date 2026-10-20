import Foundation

/// A reviewed group and the complete public configuration it depends on.
/// Selection never reads files, resolves SSH configuration, or opens a network connection.
public struct WorkspaceExportSelection: Sendable {
    public let document: WorkspaceDocument
    public let orderedMachines: [WorkspaceDocument.Machine]
    private let source: WorkspaceDocument

    public init(workspace: WorkspaceDocument, groupID: String) throws {
        guard let group = workspace.groups.first(where: { $0.id == groupID }),
              let firstMachine = group.machineOrder.first else {
            throw FTError.malformed("export.group_selection")
        }
        let machineIDs = Set(group.machineOrder)
        let machines = workspace.machines.filter { machineIDs.contains($0.id) }
        let routeMap = Dictionary(uniqueKeysWithValues: workspace.routes.map { ($0.id, $0) })
        var routeIDs = Set(machines.flatMap(\.routeIDs))
        var pending = Array(routeIDs)
        while let id = pending.popLast() {
            guard let route = routeMap[id] else { throw FTError.malformed("export.route_reference") }
            for hop in route.jumpRoutes where routeIDs.insert(hop).inserted { pending.append(hop) }
        }
        let routes = workspace.routes.filter { routeIDs.contains($0.id) }
        let credentialIDs = Set(routes.flatMap(\.credentialIDs))
        let trustIDs = Set(routes.map(\.trustID))
        let themeIDs = Set(machines.map(\.themeID))

        // Keep the validated JSON subtrees intact, including ordered candidate
        // arrays, policy, and exact numeric tokens. The UI's reduced display
        // properties are never used to reconstruct exported records.
        var root = try StrictJSON.decode(workspace.raw).object()
        var content = try root.required("workspace").object()
        var preferences = try content.required("preferences").object()
        func retain(_ field: String, ids: Set<String>) throws {
            let records = try content.required(field).array()
            content[field] = .array(try records.filter { ids.contains(try $0.object().text("id")) })
        }
        try retain("groups", ids: [groupID])
        try retain("machines", ids: machineIDs)
        try retain("routes", ids: routeIDs)
        try retain("credentials", ids: credentialIDs)
        try retain("host_trust", ids: trustIDs)
        try retain("themes", ids: themeIDs)
        preferences["initial_group_ref"] = .string(groupID)
        preferences["initial_machine_ref"] = .string(machineIDs.contains(workspace.initialMachineID)
            ? workspace.initialMachineID : firstMachine)
        content["preferences"] = .object(preferences)
        root["workspace"] = .object(content)
        root["bundle_id"] = .string(UUID().uuidString)
        document = try WorkspaceDocument(data: StrictJSON.encode(.object(root)))
        let machineMap = Dictionary(uniqueKeysWithValues: document.machines.map { ($0.id, $0) })
        orderedMachines = try group.machineOrder.map {
            guard let machine = machineMap[$0] else { throw FTError.malformed("export.machine_reference") }
            return machine
        }
        source = workspace
    }

    /// Only selected identities survive. A key loaded using an alias from
    /// another group is rebound to a retained alias of the same validated
    /// algorithm and canonical fingerprint, without duplicating key material.
    public func selectedKeys<C: AirGapCryptography>(from keys: [PrivateKeyRecord], using crypto: C) throws -> [PrivateKeyRecord] {
        // Validate the caller's entire loaded set before taking a subset. This
        // rejects forged public metadata, duplicate identities and stale IDs.
        _ = try AirGapArchive(workspace: source, keys: keys, using: crypto)
        let references = Dictionary(uniqueKeysWithValues: source.credentials.map { ($0.id, $0) })
        return try keys.compactMap { key in
            guard let original = references[key.credentialID] else {
                throw FTError.malformed("export.key_reference")
            }
            guard let selected = document.credentials.first(where: {
                $0.algorithm == original.algorithm && $0.fingerprint == original.fingerprint
            }) else { return nil }
            return PrivateKeyRecord(credentialID: selected.id, container: key.container,
                                    publicBlob: key.publicBlob, certificates: key.certificates)
        }
    }

    /// Each call prepares a new immutable export, including a fresh bundle ID.
    /// Playback and saved files must reuse the resulting FTAG bytes; resealing
    /// an old archive would conflict with the recipient's replay protections.
    public func archive<C: AirGapCryptography>(including keys: [PrivateKeyRecord], using crypto: C) throws -> AirGapArchive {
        let selected = try selectedKeys(from: keys, using: crypto)
        var root = try StrictJSON.decode(document.raw).object()
        root["bundle_id"] = .string(UUID().uuidString)
        let fresh = try WorkspaceDocument(data: StrictJSON.encode(.object(root)))
        return try AirGapArchive(workspace: fresh, keys: selected, using: crypto)
    }
}
