import Foundation
#if canImport(CryptoKit)
import CryptoKit
#elseif canImport(Crypto)
import Crypto
#endif

/// A public host identity explicitly verified on this phone. The source's
/// immutable manifest stays unchanged. A pin fills only an unconfigured trust
/// record with the exact same source policy; it never replaces imported pins.
public struct WorkspaceHostKeyPin: Codable, Equatable, Sendable {
    public let trustID: String
    public let key: WorkspaceDocument.HostKey
    let sourceWitness: Data
}

public extension InstalledWorkspace {
    /// Only a currently active phone choice can be replaced on this phone.
    /// Imported pins and local choices whose source policy changed are never
    /// returned, even when their old metadata remains in protected storage.
    func phoneHostKeyPin(forRouteID routeID: String) throws -> WorkspaceHostKeyPin? {
        let imported = try importedDocument()
        guard let route = imported.routes.first(where: { $0.id == routeID }),
              let trust = imported.hostTrust.first(where: { $0.id == route.trustID }), trust.pins.isEmpty,
              let pin = hostKeyPins.first(where: { $0.trustID == trust.id }),
              pin.sourceWitness == (try WorkspaceHostKeyOverlay.witness(trustID: trust.id, in: imported)) else { return nil }
        // Validate the complete overlay rather than treating a stored record as
        // proof that it still controls the effective host trust.
        let effective = try document()
        guard effective.hostTrust.first(where: { $0.id == trust.id })?.pins == [pin.key] else { return nil }
        return pin
    }
}

public enum WorkspaceHostKeyPinError: Error, Equatable, Sendable, LocalizedError {
    case alreadyConfigured, unchangedKey, revoked, unsupportedPolicy, unsupportedRoute, jumpNotReady
    public var errorDescription: String? {
        switch self {
        case .alreadyConfigured:
            "This host already has a trusted key. Replacing a phone-verified key needs a separate review; imported trust must be updated in its source workspace."
        case .unchangedKey:
            "The server still presents the host key already verified on this phone. No replacement is needed."
        case .revoked:
            "This host key is blocked on this phone or revoked by the workspace. It cannot be trusted again here."
        case .unsupportedPolicy:
            "This route has a required policy that this build cannot satisfy. Verifying a host key cannot remove that requirement."
        case .unsupportedRoute:
            "This route cannot perform a bounded SSH host-key check with this build."
        case .jumpNotReady:
            "Verify each jump host and supply its permitted sign-in method before checking the destination's host key."
        }
    }
}

/// Short-lived, store-issued permission to authenticate only the already
/// trusted transit hosts for first-use verification or explicit phone-pin
/// replacement. It grants no account authentication at that destination, no
/// shell, and no durable connection approval. The issuing store rechecks the
/// complete path and any prior phone pin on every use.
public struct WorkspaceHostKeyProbe: Equatable, Sendable {
    public let id: UUID
    public let workspaceID: UUID
    public let expectedRevision: UUID
    public let routeID: String
    let previousPin: WorkspaceHostKeyPin?
    let owner: UUID
    let expires: ContinuousClock.Instant
}

/// A store-issued review for one current route and one observed public key.
/// It neither approves a connection nor grants access to a private credential.
public struct WorkspaceHostKeyPinReview: Identifiable, Sendable {
    public let id: UUID
    public let workspaceID: UUID
    public let expectedRevision: UUID
    public let workspaceName: String
    public let routeID: String
    public let trustID: String
    public let host: String
    public let port: UInt16
    public let key: WorkspaceDocument.HostKey
    /// The exact phone-verified key this review replaces; nil is first use.
    public var previousKey: WorkspaceDocument.HostKey? { previousPin?.key }
    let previousPin: WorkspaceHostKeyPin?
    let sourceWitness: Data
    let owner: UUID
}

enum WorkspaceHostKeyOverlay {
    static let limit = 256

    static func witness(trustID: String, in imported: WorkspaceDocument) throws -> Data {
        try PolicyIndex(imported).witness(trustID: trustID)
    }

    private struct PolicyIndex {
        let imported: WorkspaceDocument
        let root: [String: JSONValue]
        let workspace: [String: JSONValue]
        let trusts: [String: JSONValue]

        init(_ imported: WorkspaceDocument) throws {
            self.imported = imported
            root = try StrictJSON.decode(imported.raw).object()
            workspace = try root.required("workspace").object()
            trusts = try Dictionary(uniqueKeysWithValues: workspace.required("host_trust").array().map {
                (try $0.object().text("id"), $0)
            })
        }

        func witness(trustID: String) throws -> Data {
            guard let trust = trusts[trustID] else { throw WorkspaceStoreError.staleReview }
            let policy: JSONValue = .object([
                "exporter": .string(imported.exporterID), "workspace": .string(imported.id),
                "source_kind": .string(imported.sourceKind ?? ""), "trust": trust,
                "required_capabilities": root["required_capabilities"] ?? .array([]),
                "root_extensions": root["extensions"] ?? .object([:]),
                "workspace_extensions": workspace["extensions"] ?? .object([:])
            ])
            return Data(SHA256.hash(data: try StrictJSON.encode(policy)))
        }
    }

    /// Device-key choices are applied first. Their witnesses continue to use
    /// the imported host policy; adding a local host pin does not change which
    /// user identity was deliberately selected for that route.
    static func document(effective: WorkspaceDocument, imported: WorkspaceDocument,
                         pins: [WorkspaceHostKeyPin]) throws -> WorkspaceDocument {
        guard pins.count <= limit, Set(pins.map(\.trustID)).count == pins.count else {
            throw WorkspaceStoreError.corruptStore
        }
        guard !pins.isEmpty else { return effective }
        for pin in pins {
            _ = try WorkspaceDocument.identifier(pin.trustID)
            _ = try WorkspaceDocument.HostKey(algorithm: pin.key.algorithm, fingerprint: pin.key.fingerprint)
            guard pin.sourceWitness.count == 32 else { throw WorkspaceStoreError.corruptStore }
        }
        let local = Dictionary(uniqueKeysWithValues: pins.map { ($0.trustID, $0) })
        let importedTrust = Dictionary(uniqueKeysWithValues: imported.hostTrust.map { ($0.id, $0) })
        let policy = try PolicyIndex(imported)
        var root = try StrictJSON.decode(effective.raw).object()
        var workspace = try root.required("workspace").object()
        workspace["host_trust"] = .array(try workspace.required("host_trust").array().map { value in
            var trust = try value.object()
            let id = try trust.text("id")
            guard let pin = local[id], let original = importedTrust[id], original.pins.isEmpty,
                  pin.sourceWitness == (try policy.witness(trustID: id)) else { return value }
            trust["pins"] = .array([.object([
                "algorithm": .string(pin.key.algorithm), "fingerprint": .string(pin.key.fingerprint)
            ])])
            return .object(trust)
        })
        root["workspace"] = .object(workspace)
        return try WorkspaceDocument(data: StrictJSON.encode(.object(root)))
    }
}
