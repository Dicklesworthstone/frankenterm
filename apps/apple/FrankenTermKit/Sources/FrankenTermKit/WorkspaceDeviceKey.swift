import Foundation
#if canImport(CryptoKit)
import CryptoKit
#elseif canImport(Crypto)
import Crypto
#endif

/// Public metadata for an independently generated phone identity. The private
/// container belongs exclusively to the workspace's protected credential vault.
/// Generation is not enrollment and never grants a route permission to use it.
public struct WorkspaceDeviceKey: Codable, Equatable, Identifiable, Sendable {
    public let id: String
    public let fingerprint: String
    public let publicBlob: Data
    public var algorithm: String { "ssh-ed25519" }
    public var authorizedKey: String { "\(algorithm) \(publicBlob.base64EncodedString()) \(id)" }

    func validate() throws {
        _ = try WorkspaceDocument.identifier(id)
        guard id.hasPrefix("phone-key-"),
              let suffix = UUID(uuidString: String(id.dropFirst("phone-key-".count))),
              id == "phone-key-" + suffix.uuidString.lowercased() else {
            throw WorkspaceStoreError.corruptStore
        }
        let identity = try SSHPublicIdentity(blob: publicBlob)
        guard identity.algorithm == algorithm, identity.fingerprint == fingerprint else {
            throw WorkspaceStoreError.corruptStore
        }
    }

    var credentialJSON: JSONValue {
        .object(["id": .string(id), "algorithm": .string(algorithm),
                 "public_fingerprint": .string(fingerprint), "private_material": .string("not_included")])
    }

    /// Canonical OpenSSH container accepted by the existing independently
    /// validated importer and SSH signer. No private serialization is exposed
    /// by the store API, or included in its public catalog/review/receipt.
    static func generate() throws -> (key: Self, container: Data) {
        try Task.checkCancellation()
        let privateKey = Curve25519.Signing.PrivateKey()
        var seed = privateKey.rawRepresentation
        defer { seed.resetBytes(in: 0..<seed.count) }
        let point = privateKey.publicKey.rawRepresentation
        let string = SSHUserAuthenticationChallenge.string
        let blob = string(Data("ssh-ed25519".utf8)) + string(point)
        let identity = try SSHPublicIdentity(blob: blob)
        let id = "phone-key-" + UUID().uuidString.lowercased()
        let check = UInt32.random(in: UInt32.min...UInt32.max)
        var inner = Data()
        defer { inner.resetBytes(in: 0..<inner.count) }
        inner.appendBE(UInt64(check), bytes: 4)
        inner.appendBE(UInt64(check), bytes: 4)
        inner += string(Data("ssh-ed25519".utf8)) + string(point)
        var secret = seed + point
        defer { secret.resetBytes(in: 0..<secret.count) }
        inner += string(secret) + string(Data(id.utf8))
        for pad in 1...(8 - inner.count % 8) { inner.append(UInt8(pad)) }
        var container = Data("openssh-key-v1\0".utf8)
        defer { container.resetBytes(in: 0..<container.count) }
        container += string(Data("none".utf8)) + string(Data("none".utf8)) + string(Data())
        container.appendBE(1, bytes: 4)
        container += string(blob) + string(inner)
        guard try SSHUserAuthenticationSigner.publicIdentity(privateContainer: container) == identity else {
            throw SSHAuthenticationError.identityMismatch
        }
        try Task.checkCancellation()
        return (.init(id: id, fingerprint: identity.fingerprint, publicBlob: blob), container)
    }
}

/// A phone-local choice, bound to the exact imported route and every transit
/// route's authentication and host-trust policy. Reimport never upgrades this
/// choice to a different endpoint or silently resumes the Mac's copied key.
public struct WorkspaceDeviceKeyAssignment: Codable, Equatable, Sendable {
    public let routeID: String
    public let credentialID: String
    let sourceWitness: Data
}

public struct WorkspaceDeviceKeyReceipt: Sendable {
    public let workspace: InstalledWorkspace
    public let key: WorkspaceDeviceKey
    public let cleanupPending: Bool
}

enum WorkspaceDeviceKeyOverlay {
    static let keyLimit = 16
    static let assignmentLimit = 256
    static let reviewCapability = "phone_device_key_review_required"

    static func canAssign(_ route: WorkspaceDocument.Route, in document: WorkspaceDocument) -> Bool {
        guard route.authenticationMethods.contains(.publicKey),
              route.mode == "direct_ssh" || route.mode == "frankenterm_mux" else { return false }
        // A plain generated identity cannot fulfill a required certificate or
        // silently shed mandatory policy attached to the imported identity.
        let references = document.credentials.filter { route.credentialIDs.contains($0.id) }
        return references.allSatisfy {
            $0.userCertificate == nil && $0.requiredCapabilities.isEmpty && $0.requiredExtensions.isEmpty
        }
    }

    static func witness(routeID: String, in document: WorkspaceDocument) throws -> Data {
        try PolicyIndex(document: document).witness(routeID: routeID)
    }

    /// One bounded policy index for a fleet-sized atomic selection. Creating
    /// metadata here does not publish it or grant access to a protected key.
    static func assignments(routeIDs: Set<String>, credentialID: String,
                            in document: WorkspaceDocument) throws -> [WorkspaceDeviceKeyAssignment] {
        guard routeIDs.count <= assignmentLimit else { throw FTError.limit("device_key.assignments") }
        let index = try PolicyIndex(document: document)
        return try routeIDs.sorted().map { routeID in
            guard let route = index.typed[routeID], canAssign(route, in: document) else {
                throw FTError.unsupported("device_key.route_policy")
            }
            return .init(routeID: routeID, credentialID: credentialID,
                         sourceWitness: try index.witness(routeID: routeID))
        }
    }

    /// Parse the bounded source once for a whole fleet, rather than once per
    /// assigned route during every local-policy reload and authentication check.
    private struct PolicyIndex {
        let document: WorkspaceDocument
        let root: [String: JSONValue]
        let workspace: [String: JSONValue]
        let routes: [String: JSONValue], credentials: [String: JSONValue], trusts: [String: JSONValue]
        let typed: [String: WorkspaceDocument.Route]
        let contexts: [String: [[String]]]

        init(document: WorkspaceDocument) throws {
            self.document = document
            root = try StrictJSON.decode(document.raw).object()
            workspace = try root.required("workspace").object()
            func indexed(_ name: String, workspace: [String: JSONValue]) throws -> [String: JSONValue] {
                try Dictionary(uniqueKeysWithValues: workspace.required(name).array().map { value in
                    (try value.object().required("id").string(), value)
                })
            }
            routes = try indexed("routes", workspace: workspace)
            credentials = try indexed("credentials", workspace: workspace)
            trusts = try indexed("host_trust", workspace: workspace)
            typed = Dictionary(uniqueKeysWithValues: document.routes.map { ($0.id, $0) })
            // A route can be a transit account behind several different
            // prefixes. Its credential choice affects every actual use, not
            // only the dependencies declared on that route in isolation.
            var actual: [String: Set<[String]>] = [:]
            let destinations = Set(document.machines.flatMap(\.routeIDs))
            for destinationID in destinations.sorted() {
                guard let destination = typed[destinationID],
                      destination.mode == "direct_ssh" || destination.mode == "frankenterm_mux" else { continue }
                let path = try document.orderedJumpRoutes(forRouteID: destinationID).map(\.id) + [destinationID]
                for index in path.indices {
                    actual[path[index], default: []].insert(Array(path.prefix(index + 1)))
                }
            }
            contexts = actual.mapValues { $0.sorted { $0.lexicographicallyPrecedes($1) } }
        }

        func witness(routeID: String) throws -> Data {
        var pending = [routeID], visited = Set<String>()
        let declared = try document.orderedJumpRoutes(forRouteID: routeID).map(\.id) + [routeID]
        let actual = contexts[routeID] ?? []
        // Keep existing direct-route witnesses stable. Distinct incoming
        // prefixes add explicit context, including an exposed standalone use.
        // Adding another machine that reaches the same prefix is cosmetic.
        let hasContext = actual.contains { $0 != declared }
        if hasContext { pending += actual.flatMap { $0 } }
        var routePolicy: [String: JSONValue] = [:], credentialPolicy: [String: JSONValue] = [:]
        var trustPolicy: [String: JSONValue] = [:]
        while let id = pending.popLast() {
            guard let route = typed[id], let raw = routes[id], let trust = trusts[route.trustID] else {
                throw WorkspaceStoreError.staleReview
            }
            guard visited.insert(id).inserted else { continue }
            guard visited.count <= 256 else { throw FTError.limit("device_key.route_graph") }
            routePolicy[id] = raw; trustPolicy[route.trustID] = trust
            for credentialID in route.credentialIDs {
                guard let credential = credentials[credentialID] else { throw WorkspaceStoreError.corruptStore }
                credentialPolicy[credentialID] = credential
            }
            pending += route.jumpRoutes
        }
        // A positive pin cannot override another record's same-scope denial or
        // mandatory policy. Endpoint-wide revocations also survive alias changes.
        let selectedTrusts = document.hostTrust.filter { trustPolicy[$0.id] != nil }
        for trust in document.hostTrust where selectedTrusts.contains(where: {
            $0.canonicalTrustScope == trust.canonicalTrustScope || ($0.host == trust.host && $0.port == trust.port)
        }) {
            trustPolicy[trust.id] = trusts[trust.id]
        }
        var identity: [String: JSONValue] = [
            "exporter": .string(document.exporterID), "workspace": .string(document.id),
            "routes": .object(routePolicy), "credentials": .object(credentialPolicy), "trust": .object(trustPolicy),
            "required_capabilities": root["required_capabilities"] ?? .array([]),
            "source_kind": .string(document.sourceKind ?? ""),
            "root_extensions": root["extensions"] ?? .object([:]),
            "workspace_extensions": workspace["extensions"] ?? .object([:])
        ]
        if hasContext {
            identity["authentication_contexts"] = .array(actual.map { .array($0.map(JSONValue.string)) })
        }
        return Data(SHA256.hash(data: try StrictJSON.encode(.object(identity))))
        }
    }

    static func reviewRequiredRouteIDs(imported: WorkspaceDocument,
                                      assignments: [WorkspaceDeviceKeyAssignment]) throws -> Set<String> {
        guard !assignments.isEmpty else { return [] }
        let index = try PolicyIndex(document: imported)
        var result = Set<String>()
        for assignment in assignments {
            guard let route = index.typed[assignment.routeID] else {
                result.insert(assignment.routeID); continue
            }
            let current = try index.witness(routeID: assignment.routeID)
            if assignment.sourceWitness != current || !canAssign(route, in: imported) {
                result.insert(assignment.routeID)
            }
        }
        return result
    }

    static func document(imported: WorkspaceDocument, keys: [WorkspaceDeviceKey],
                         assignments: [WorkspaceDeviceKeyAssignment]) throws -> WorkspaceDocument {
        guard keys.count <= keyLimit, assignments.count <= assignmentLimit,
              Set(keys.map(\.id)).count == keys.count,
              Set(keys.map(\.fingerprint)).count == keys.count,
              Set(assignments.map(\.routeID)).count == assignments.count else { throw WorkspaceStoreError.corruptStore }
        if keys.isEmpty && assignments.isEmpty { return imported }
        for key in keys { try key.validate() }
        let keyIDs = Set(keys.map(\.id))
        guard Set(imported.credentials.map(\.id)).isDisjoint(with: keyIDs),
              imported.credentials.count + keys.count <= 256 else { throw FTError.limit("device_key.credentials") }
        let assigned = Dictionary(uniqueKeysWithValues: assignments.map { ($0.routeID, $0) })
        for assignment in assignments {
            _ = try WorkspaceDocument.identifier(assignment.routeID)
            guard keyIDs.contains(assignment.credentialID),
                  assignment.sourceWitness.count == 32 else { throw WorkspaceStoreError.corruptStore }
        }
        let needsReview = try reviewRequiredRouteIDs(imported: imported, assignments: assignments)
        var root = try StrictJSON.decode(imported.raw).object()
        var workspace = try root.required("workspace").object()
        workspace["credentials"] = .array(try workspace.required("credentials").array() + keys.map(\.credentialJSON))
        workspace["routes"] = .array(try workspace.required("routes").array().map { value in
            var route = try value.object()
            let id = try route.required("id").string()
            guard let assignment = assigned[id] else { return value }
            if !needsReview.contains(id) {
                // This deliberate choice replaces the source's key candidates;
                // a refused device key must not fall back to a shared Mac key.
                route["credential_refs"] = .array([.string(assignment.credentialID)])
            } else {
                var required = try route["required_capabilities"]?.array() ?? []
                if !required.contains(.string(reviewCapability)) { required.append(.string(reviewCapability)) }
                route["required_capabilities"] = .array(required)
            }
            return .object(route)
        })
        root["workspace"] = .object(workspace)
        return try WorkspaceDocument(data: StrictJSON.encode(.object(root)))
    }
}

public extension InstalledWorkspace {
    /// The authenticated source's immutable document. Local credential choices
    /// must never alter the accepted envelope/bundle identity or import review.
    func importedDocument() throws -> WorkspaceDocument { try WorkspaceDocument(data: manifest) }

    func deviceKeyAssignableRouteIDs() throws -> [String] {
        let imported = try importedDocument()
        return imported.routes.filter { WorkspaceDeviceKeyOverlay.canAssign($0, in: imported) }.map(\.id)
    }

    func deviceKeyAssignmentNeedsReview(routeID: String) throws -> Bool {
        guard let assignment = deviceKeyAssignments.first(where: { $0.routeID == routeID }) else { return false }
        let imported = try importedDocument()
        guard let route = imported.routes.first(where: { $0.id == routeID }) else { return true }
        let expected = try WorkspaceDeviceKeyOverlay.witness(routeID: routeID, in: imported)
        return !WorkspaceDeviceKeyOverlay.canAssign(route, in: imported)
            || assignment.sourceWitness != expected
    }
}

