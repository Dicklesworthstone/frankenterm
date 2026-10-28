import Foundation

/// A finite selection from one already resolved configuration revision. This
/// type opens no files, evaluates no configuration, and contacts no remote host.
/// Its public preview never carries private-key material.
public struct WorkspaceExportSelection: Sendable {
    public enum IdentityDecision: String, CaseIterable, Hashable, Sendable {
        case pending, includePrivateKey, keepReferenceOnly
    }
    public struct Identity: Identifiable, Sendable {
        public let fingerprint: String, algorithm: String
        public let credentialIDs: [String], routeIDs: [String]
        public var id: String { fingerprint }
    }
    public struct Review: Sendable {
        public let pendingIdentityCount: Int, missingApprovedKeyCount: Int
        public let includedIdentityCount: Int, omittedIdentityCount: Int
        public let blockedRouteIDs: [String], missingIdentityRouteIDs: [String]
        public let findings: [AirGapArchive.Portability]
        /// A narrower workspace can be deliberately exported. Required policies
        /// remain in its manifest and cannot be satisfied by this confirmation.
        public var canPrepare: Bool { pendingIdentityCount == 0 && missingApprovedKeyCount == 0 }
    }

    private let source: WorkspaceDocument
    private let sourceFindings: [AirGapArchive.Portability]
    public let workspace: WorkspaceDocument
    public let identities: [Identity]
    /// The original single-group API and multi-group preview expose the same
    /// validated dependency projection.
    public var document: WorkspaceDocument { workspace }
    public var orderedMachines: [WorkspaceDocument.Machine] {
        let machineMap = Dictionary(uniqueKeysWithValues: workspace.machines.map { ($0.id, $0) })
        var seen = Set<String>()
        return workspace.groups.flatMap(\.machineOrder).compactMap { id in
            guard seen.insert(id).inserted else { return nil }
            return machineMap[id]
        }
    }

    /// The selected groups before narrowing individual machines. Keep this
    /// order stable so removing a machine does not make its selection control
    /// disappear or change the meaning of another group's shared membership.
    public var availableMachines: [WorkspaceDocument.Machine] {
        let groupIDs = Set(workspace.groups.map(\.id))
        let machineMap = Dictionary(uniqueKeysWithValues: source.machines.map { ($0.id, $0) })
        var seen = Set<String>()
        return source.groups.filter { groupIDs.contains($0.id) }.flatMap(\.machineOrder).compactMap { id in
            guard seen.insert(id).inserted else { return nil }
            return machineMap[id]
        }
    }

    public init(workspace source: WorkspaceDocument, groupIDs: Set<String>,
                machineIDs: Set<String>? = nil,
                findings: [AirGapArchive.Portability] = []) throws {
        try self.init(source: source, groupIDs: groupIDs, machineIDs: machineIDs,
                      projectionBundleID: nil, findings: findings)
    }

    /// A single reviewed group retains its existing public API, including a new
    /// projection identity. Preparing its archive always issues another identity.
    public init(workspace source: WorkspaceDocument, groupID: String,
                findings: [AirGapArchive.Portability] = []) throws {
        try self.init(source: source, groupIDs: [groupID], machineIDs: nil,
                      projectionBundleID: UUID().uuidString, findings: findings)
    }

    private init(source: WorkspaceDocument, groupIDs: Set<String>, machineIDs: Set<String>?, projectionBundleID: String?,
                 findings: [AirGapArchive.Portability]) throws {
        self.source = source
        // Validate before filtering so an invalid source report cannot become
        // acceptable merely by selecting a group that hides its reference.
        let metadata = try AirGapArchive.validatedMetadata(workspace: source, portability: findings, annotations: [])
        let selection = try Self.project(source, groupIDs: groupIDs, machineIDs: machineIDs)
        let projected = try projectionBundleID.map { try selection.reissuingBundle(id: $0) } ?? selection
        workspace = projected
        let retainedEntities = Set([projected.id] + projected.groups.map(\.id) + projected.machines.map(\.id)
            + projected.routes.map(\.id) + projected.credentials.map(\.id) + projected.hostTrust.map(\.id) + projected.themes.map(\.id))
        sourceFindings = metadata.portability.filter { retainedEntities.contains($0.entityID) }
        var orderedFingerprints: [String] = [], references: [String: [String]] = [:]
        var algorithms: [String: String] = [:]
        for credential in projected.credentials {
            if references[credential.fingerprint] == nil { orderedFingerprints.append(credential.fingerprint) }
            references[credential.fingerprint, default: []].append(credential.id)
            algorithms[credential.fingerprint] = credential.algorithm
        }
        identities = try orderedFingerprints.map { fingerprint in
            guard let ids = references[fingerprint], let algorithm = algorithms[fingerprint] else { throw FTError.stale }
            let refs = Set(ids)
            return Identity(fingerprint: fingerprint, algorithm: algorithm,
                            credentialIDs: ids, routeIDs: projected.routes.filter {
                                $0.credentialIDs.contains(where: refs.contains)
                            }.map(\.id))
        }
    }

    /// Missing private material is an explicit omission, not a permanent deny:
    /// the destination may already hold the key or permit an interactive method.
    /// Actual route readiness is resolved against the phone's reviewed vault.
    public func review(decisions: [String: IdentityDecision], loadedFingerprints: Set<String>) throws -> Review {
        let selected = Set(identities.map(\.fingerprint))
        guard Set(decisions.keys).isSubset(of: selected), loadedFingerprints.isSubset(of: selected) else {
            throw FTError.malformed("export.identity_selection")
        }
        let included = Set(identities.filter {
            decisions[$0.fingerprint] == .includePrivateKey && loadedFingerprints.contains($0.fingerprint)
        }.map(\.fingerprint))
        var findings = sourceFindings
        var positions = Dictionary(uniqueKeysWithValues: findings.enumerated().map {
            ([$0.element.entityID, $0.element.setting], $0.offset)
        })
        func add(_ finding: AirGapArchive.Portability) {
            let key = [finding.entityID, finding.setting]
            if let index = positions[key] {
                if finding.disposition > findings[index].disposition { findings[index] = finding }
            } else { positions[key] = findings.count; findings.append(finding) }
        }
        let omitted = identities.filter { decisions[$0.fingerprint] == .keepReferenceOnly }
        for identity in omitted {
            for credentialID in identity.credentialIDs {
                add(.init(entityID: credentialID, setting: "private_identity", disposition: 0,
                    reason: "Private material was deliberately omitted. This public reference is preserved; use a matching identity already on the phone or import it separately."))
            }
        }
        let supportedCredentialCapabilities: Set<String> = [WorkspaceDocument.userCertificateCapability]
        for credential in workspace.credentials where !Set(credential.requiredCapabilities).isSubset(of: supportedCredentialCapabilities)
            || !credential.requiredExtensions.isEmpty {
            // This candidate's restriction stays mandatory in the manifest.
            // The annotation is informational: another ordered candidate or a
            // permitted interactive method may still satisfy the route.
            add(.init(entityID: credential.id, setting: "credential_policy", disposition: 0,
                reason: "This credential reference has a required capability or extension that is unavailable in this build. Transferring or finding its private key does not satisfy that policy."))
        }
        let routeMap = Dictionary(uniqueKeysWithValues: workspace.routes.map { ($0.id, $0) })
        let trustMap = Dictionary(uniqueKeysWithValues: workspace.hostTrust.map { ($0.id, $0) })
        let credentials = Dictionary(uniqueKeysWithValues: workspace.credentials.map { ($0.id, $0) })
        let destinations = Set(workspace.machines.flatMap(\.routeIDs))
        let sourceBlocked = WorkspacePortabilityPolicy.blockedRouteIDs(in: workspace,
            requiredEntityIDs: Set(sourceFindings.filter { $0.disposition == 1 }.map(\.entityID)))
        var revocations: [String: Set<String>] = [:]
        var requirements: [String: Set<String>] = [:]
        for trust in workspace.hostTrust {
            revocations[trust.canonicalTrustScope, default: []].formUnion(trust.revokedKeys.map(\.fingerprint))
            requirements[trust.canonicalTrustScope, default: []].formUnion(trust.requiredPolicyRequirements)
        }
        var blocked: [String] = [], missingIdentity: [String] = []
        let certificateTime = Date()
        for route in workspace.routes {
            var required = Set<String>(), needsIdentity = false, lacksPins = false, deniesPins = false, certificateBlocked = false
            var visited = Set<String>()
            func inspect(_ candidate: WorkspaceDocument.Route, transit: Bool) {
                guard visited.insert(candidate.id).inserted else { return }
                required.formUnion(candidate.requiredCapabilities)
                required.formUnion(candidate.requiredExtensions.map { "extension:" + $0 })
                if let trust = trustMap[candidate.trustID] {
                    required.formUnion(requirements[trust.canonicalTrustScope] ?? [])
                    if trust.pins.isEmpty { lacksPins = true }
                    else if trust.pins.allSatisfy({ (revocations[trust.canonicalTrustScope] ?? []).contains($0.fingerprint) }) { deniesPins = true }
                }
                if candidate.authenticationMethods == [.publicKey] {
                    let candidates = candidate.credentialIDs.compactMap { credentials[$0] }
                    let permitted = candidates.filter {
                        Set($0.requiredCapabilities).isSubset(of: supportedCredentialCapabilities) && $0.requiredExtensions.isEmpty
                    }
                    if !candidates.isEmpty, permitted.isEmpty {
                        required.formUnion(candidates.flatMap(\.requiredCapabilities))
                        required.formUnion(candidates.flatMap(\.requiredExtensions).map { "extension:" + $0 })
                    } else {
                        let usable = permitted.filter {
                            $0.permitsCertificate(user: candidate.user, transit: transit, routeMode: candidate.mode, now: certificateTime)
                        }
                        if !permitted.isEmpty, usable.isEmpty { certificateBlocked = true }
                        if !permitted.contains(where: { included.contains($0.fingerprint) }) { needsIdentity = true }
                    }
                }
                for id in candidate.jumpRoutes { if let hop = routeMap[id] { inspect(hop, transit: true) } }
            }
            inspect(route, transit: !destinations.contains(route.id))
            var reasons: [String] = []
            let lacksSessionPolicy = destinations.contains(route.id)
                && ((route.mode == "direct_ssh" && route.sessionCreation == nil)
                    || (route.mode == "frankenterm_mux" && route.attachPolicy == nil))
            if lacksSessionPolicy { reasons.append("Choose an explicit session creation or attachment policy before opening this destination.") }
            if sourceBlocked.contains(route.id) { reasons.append("A required source setting remains unresolved for this route or a dependency.") }
            if !required.isEmpty {
                let names = required.sorted(), sample = names.prefix(8).joined(separator: ", ")
                let remainder = names.count > 8 ? " and \(names.count - 8) more" : ""
                reasons.append("Required policy is preserved but unavailable in this build: \(sample)\(remainder).")
            }
            if deniesPins { reasons.append("Every positive host key for this route or a jump host is revoked. Positive trust cannot override that denial.") }
            if lacksPins { reasons.append("This route or a jump host has no positive host key. Review and verify host identity on the phone before authentication.") }
            if certificateBlocked { reasons.append("No selected user certificate currently permits this account and connection. The certificate is preserved; renew it or choose an authorized candidate.") }
            if needsIdentity {
                missingIdentity.append(route.id)
                reasons.append("Public-key authentication for this route or a jump host needs a matching identity already on the phone or a separate key import.")
            }
            // Time validity is evaluated again on the phone. Do not persist an
            // immutable required finding for a not-yet-valid certificate.
            let requiredBlock = !required.isEmpty || deniesPins || lacksSessionPolicy || sourceBlocked.contains(route.id)
            if requiredBlock || certificateBlocked { blocked.append(route.id) }
            if !reasons.isEmpty {
                add(.init(entityID: route.id, setting: "route_readiness", disposition: requiredBlock ? 1 : 0,
                                      reason: reasons.joined(separator: " ")))
            }
        }
        guard findings.count <= 1024 else { throw FTError.limit("archive.metadata") }
        return .init(pendingIdentityCount: identities.filter { decisions[$0.fingerprint, default: .pending] == .pending }.count,
                     missingApprovedKeyCount: identities.filter {
                         decisions[$0.fingerprint] == .includePrivateKey && !loadedFingerprints.contains($0.fingerprint)
                     }.count,
                     includedIdentityCount: included.count, omittedIdentityCount: omitted.count,
                     blockedRouteIDs: blocked, missingIdentityRouteIDs: missingIdentity, findings: findings)
    }

    /// An ordinary document cannot carry the archive's portability records.
    /// Preserve every required source restriction as an inactive route marker,
    /// and issue a separate identity from any encrypted export of this snapshot.
    public func publicDocument(bundleID: String) throws -> WorkspaceDocument {
        guard bundleID != source.bundleID, bundleID != workspace.bundleID else { throw FTError.malformed("export.bundle_identity") }
        let blocked = WorkspacePortabilityPolicy.blockedRouteIDs(in: workspace,
            requiredEntityIDs: Set(sourceFindings.filter { $0.disposition == 1 }.map(\.entityID)))
        guard !blocked.isEmpty else { return try workspace.reissuingBundle(id: bundleID) }
        var root = try StrictJSON.decode(workspace.raw).object()
        var document = try root.required("workspace").object()
        let routes = try document.required("routes").array().map { value -> JSONValue in
            var route = try value.object()
            if try blocked.contains(route.text("id")) {
                let existing = try route["required_capabilities"]?.array().map { try $0.string() } ?? []
                route["required_capabilities"] = .array(Set(existing).union(["source.unresolved_required_policy"]).sorted().map(JSONValue.string))
            }
            return .object(route)
        }
        document["routes"] = .array(routes); root["workspace"] = .object(document)
        root["bundle_id"] = .string(bundleID)
        return try WorkspaceDocument(data: StrictJSON.encode(.object(root)))
    }

    /// Revalidates the real private components and every explicit choice before
    /// construction. A caller cannot authorize a key by forging the preview.
    /// A source alias outside this group may supply the identical selected key;
    /// the archive binds it to a retained reference, exactly once per fingerprint.
    public func archive<C: AirGapCryptography>(decisions: [String: IdentityDecision], keys: [PrivateKeyRecord],
                                              bundleID: String, using crypto: C) throws -> AirGapArchive {
        guard bundleID != source.bundleID, bundleID != workspace.bundleID else { throw FTError.malformed("export.bundle_identity") }
        _ = try AirGapArchive(workspace: source, keys: keys, using: crypto)
        let sourceReferences = Dictionary(uniqueKeysWithValues: source.credentials.map { ($0.id, $0.fingerprint) })
        var material: [String: PrivateKeyRecord] = [:]
        for key in keys {
            guard let fingerprint = sourceReferences[key.credentialID],
                  decisions[fingerprint] == .includePrivateKey,
                  identities.contains(where: { $0.fingerprint == fingerprint }) else {
                throw FTError.malformed("export.unapproved_key")
            }
            material[fingerprint] = key
        }
        let reviewed = try review(decisions: decisions, loadedFingerprints: Set(material.keys))
        guard reviewed.canPrepare else { throw FTError.malformed("export.identity_decision_required") }
        let selectedKeys = try identities.compactMap { identity -> PrivateKeyRecord? in
            guard decisions[identity.fingerprint] == .includePrivateKey else { return nil }
            guard let key = material[identity.fingerprint], let reference = identity.credentialIDs.first else { throw FTError.stale }
            return PrivateKeyRecord(credentialID: reference, container: key.container, publicBlob: key.publicBlob,
                certificates: Self.certificates(for: identity.fingerprint, in: workspace))
        }
        return try AirGapArchive(workspace: workspace.reissuingBundle(id: bundleID), keys: selectedKeys,
                                 portability: reviewed.findings, using: crypto)
    }

    /// Validates all supplied material before projecting it. A key explicitly
    /// supplied through an unselected alias may serve the same selected public
    /// identity; unrelated private material is excluded from the return value.
    public func selectedKeys<C: AirGapCryptography>(from keys: [PrivateKeyRecord], using crypto: C) throws -> [PrivateKeyRecord] {
        _ = try AirGapArchive(workspace: source, keys: keys, using: crypto)
        let references = Dictionary(uniqueKeysWithValues: source.credentials.map { ($0.id, $0) })
        return try keys.compactMap { key in
            guard let original = references[key.credentialID] else { throw FTError.malformed("export.key_reference") }
            guard let selected = workspace.credentials.first(where: {
                $0.algorithm == original.algorithm && $0.fingerprint == original.fingerprint
            }) else { return nil }
            return PrivateKeyRecord(credentialID: selected.id, container: key.container,
                                    publicBlob: key.publicBlob, certificates: Self.certificates(for: selected.fingerprint, in: workspace))
        }
    }

    private static func certificates(for fingerprint: String, in workspace: WorkspaceDocument) -> [Data] {
        var seen = Set<Data>()
        return workspace.credentials.filter { $0.fingerprint == fingerprint }.compactMap { credential in
            guard let blob = credential.userCertificate?.blob, seen.insert(blob).inserted else { return nil }
            return blob
        }
    }

    /// This explicit API call approves the supplied keys for this selection and
    /// omits all other identities. The native UI uses the per-identity decision
    /// API so each include/omit choice is individually visible before sealing.
    public func archive<C: AirGapCryptography>(including keys: [PrivateKeyRecord], using crypto: C) throws -> AirGapArchive {
        let selected = try selectedKeys(from: keys, using: crypto)
        let selectedIDs = Set(selected.map(\.credentialID))
        let included = Set(workspace.credentials.filter { selectedIDs.contains($0.id) }.map(\.fingerprint))
        let choices = Dictionary(uniqueKeysWithValues: identities.map {
            ($0.fingerprint, included.contains($0.fingerprint) ? IdentityDecision.includePrivateKey : .keepReferenceOnly)
        })
        return try archive(decisions: choices, keys: selected, bundleID: UUID().uuidString, using: crypto)
    }

    private static func project(_ source: WorkspaceDocument, groupIDs: Set<String>,
                                machineIDs requestedMachineIDs: Set<String>?) throws -> WorkspaceDocument {
        guard !groupIDs.isEmpty, groupIDs.isSubset(of: Set(source.groups.map(\.id))) else {
            throw FTError.malformed("export.group_selection")
        }
        let groups = source.groups.filter { groupIDs.contains($0.id) }
        let availableMachineIDs = Set(groups.flatMap(\.machineOrder))
        guard !availableMachineIDs.isEmpty else { throw FTError.malformed("export.empty_group_selection") }
        let machineIDs = requestedMachineIDs ?? availableMachineIDs
        guard !machineIDs.isEmpty, machineIDs.isSubset(of: availableMachineIDs) else {
            throw FTError.malformed("export.machine_selection")
        }
        let machines = source.machines.filter { machineIDs.contains($0.id) }
        let routeMap = Dictionary(uniqueKeysWithValues: source.routes.map { ($0.id, $0) })
        var routeIDs = Set<String>()
        func includeRoute(_ id: String) {
            guard routeIDs.insert(id).inserted, let route = routeMap[id] else { return }
            for hop in route.jumpRoutes { includeRoute(hop) }
        }
        for machine in machines { for route in machine.routeIDs { includeRoute(route) } }
        let routes = source.routes.filter { routeIDs.contains($0.id) }
        let credentialIDs = Set(routes.flatMap(\.credentialIDs))
        let trustIDs = Set(routes.map(\.trustID))
        let trustScopes = Set(source.hostTrust.filter { trustIDs.contains($0.id) }.map(\.canonicalTrustScope))
        // Retaining only directly referenced pins could erase an applicable
        // negative record or mandatory policy stored under another trust alias.
        let retainedTrustIDs = Set(source.hostTrust.filter { trustScopes.contains($0.canonicalTrustScope) }.map(\.id))
        let themeIDs = Set(machines.map(\.themeID))
        var root = try StrictJSON.decode(source.raw).object()
        var document = try root.required("workspace").object()
        func retain(_ field: String, _ ids: Set<String>) throws {
            let values = try document.required(field).array()
            document[field] = .array(try values.filter { value in ids.contains(try value.object().text("id")) })
        }
        try retain("groups", groupIDs); try retain("machines", machineIDs); try retain("routes", routeIDs)
        try retain("credentials", credentialIDs); try retain("host_trust", retainedTrustIDs); try retain("themes", themeIDs)
        // Prune only membership. Group identity, order, auto-connect and any
        // required policy stay intact, including a deliberately retained empty
        // group. No unselected machine can survive through another group.
        document["groups"] = .array(try document.required("groups").array().map { value in
            var group = try value.object()
            group["machine_order"] = .array(try group.required("machine_order").array().filter {
                machineIDs.contains(try $0.string())
            })
            return .object(group)
        })
        let nonemptyGroups = groups.filter { $0.machineOrder.contains(where: machineIDs.contains) }
        guard let initialGroup = nonemptyGroups.first(where: { $0.id == source.initialGroupID })
                ?? nonemptyGroups.first(where: { $0.machineOrder.contains(source.initialMachineID) && machineIDs.contains(source.initialMachineID) })
                ?? nonemptyGroups.first,
              let initialMachine = initialGroup.machineOrder.contains(source.initialMachineID) && machineIDs.contains(source.initialMachineID)
                ? source.initialMachineID : initialGroup.machineOrder.first(where: machineIDs.contains) else { throw FTError.stale }
        var preferences = try document.required("preferences").object()
        preferences["initial_group_ref"] = .string(initialGroup.id)
        preferences["initial_machine_ref"] = .string(initialMachine)
        document["preferences"] = .object(preferences); root["workspace"] = .object(document)
        return try WorkspaceDocument(data: JSONValue.object(root).encoded())
    }
}
