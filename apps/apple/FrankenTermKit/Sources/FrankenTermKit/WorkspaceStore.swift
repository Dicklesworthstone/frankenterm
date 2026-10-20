import Foundation
#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif

public enum WorkspaceStorageSlot: String, Sendable { case catalog, journal }

/// A successful write is atomic and durable. A thrown write may have published;
/// callers MUST reconcile by reading, not delete the candidate's credentials.
/// Exclusive access covers the whole journal/vault/catalog transaction.
public protocol WorkspacePersistence: Sendable {
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data?
    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws
    func removeJournal() throws
}

/// Implementations are synchronous, used only on the store actor, never MainActor.
/// create must not replace an existing item or relax its access policy.
public protocol WorkspaceCredentialVault: Sendable {
    func create(account: String, privateContainer: Data, fingerprint: String) throws
    func fingerprint(account: String) throws -> String?
    func remove(account: String) throws
}

public enum WorkspaceStoreError: Error, Equatable, Sendable, LocalizedError {
    case reviewRequired, staleReview, removalNeedsApproval, revokedIdentity
    case conflictingExport, rollbackNeedsApproval, importHistoryFull
    case corruptStore, busy, recoveryRequired
    public static var immutableExportConflict: Self { .conflictingExport }
    public var errorDescription: String? {
        switch self {
        case .reviewRequired: return "Confirm the source, comparison value, and import contents first."
        case .staleReview: return "The saved workspace changed. Review the import again."
        case .removalNeedsApproval: return "This import removes machines. Explicit replacement approval is required."
        case .revokedIdentity: return "This import includes a locally revoked identity."
        case .conflictingExport: return "This export reuses a previously accepted bundle or transfer ID with different contents. Create a new export on the source."
        case .rollbackNeedsApproval: return "This is an earlier accepted export. Review and approve restoring its older settings."
        case .importHistoryFull: return "The protected import history is full. New exports cannot be accepted without forgetting replay protection. Existing accepted recordings remain available."
        case .corruptStore: return "Workspace storage could not be validated. No credentials were removed."
        case .busy: return "Another workspace transaction is active."
        case .recoveryRequired: return "The import outcome needs storage recovery. Do not assume it was rolled back."
        }
    }
}

public struct InstalledWorkspace: Codable, Sendable, Identifiable {
    public struct Binding: Codable, Equatable, Sendable {
        public let credentialID: String, fingerprint: String, account: String
    }
    public struct InactiveSetting: Codable, Sendable {
        public let entityID: String, setting: String, original: Data
    }
    public struct PortabilityFinding: Codable, Sendable {
        public let entityID: String, setting: String, disposition: UInt64, reason: String
    }
    public let id: UUID
    public let revision: UUID
    public let manifest: Data
    public let envelopeDigest: Data
    public let bindings: [Binding]
    public let inactiveSettings: [InactiveSetting]
    public let findings: [PortabilityFinding]
    /// Restoring NEVER executes imported auto-connect preferences, even on relaunch.
    public let connectLater: Bool
    public func document() throws -> WorkspaceDocument { try WorkspaceDocument(data: manifest) }
}

public struct WorkspaceImportReceipt: Sendable {
    public let workspace: InstalledWorkspace
    public let missingCredentialIDs: [String]
    public let cleanupPending: Bool
}

public struct WorkspaceReimportReview: Sendable {
    public let expectedRevision: UUID?
    public let removesMachines: Bool
    public let isRollback: Bool
    public let preservedHostRevocationCount: Int
    public let preservedHostRequirementCount: Int
    public let firstAcceptedRevision: UUID?
}

public struct WorkspaceImportAssessment: Sendable {
    public let existingWorkspace: InstalledWorkspace?
    public let firstAcceptedRevision: UUID?
    public let requiresRollbackApproval: Bool
    public let removesMachines: Bool
    public let preservedHostRevocationCount: Int
    public let preservedHostRequirementCount: Int
}

/// One revision-consistent snapshot of actual protected-key availability and
/// mandatory local trust policy for the native browser and future transports.
public struct WorkspaceLocalState: Sendable {
    public let workspace: InstalledWorkspace
    public let credentialIDs: Set<String>
    public let revokedHostKeysByTrustScope: [String: Set<WorkspaceDocument.HostKey>]
    public let requiredHostCapabilitiesByTrustScope: [String: Set<String>]
    public let routes: [WorkspaceDocument.RouteReadiness]
}

/// One app-owned actor shared by every scene. There are no suspension points
/// inside a transaction, and the persistence lease also fences other instances.
public actor WorkspaceStore {
    private struct AcceptedImport: Codable {
        let exporterID: String
        let workspaceID: String
        let bundleID: String
        var transferID: Data?
        let envelopeDigest: Data
        let firstAcceptedRevision: UUID
    }
    private struct HostRevocation: Codable, Hashable {
        let host: String
        let port: UInt16
        let key: WorkspaceDocument.HostKey
    }
    private struct ScopedHostRevocation: Codable, Hashable {
        let workspaceID: UUID
        let scope: String
        let key: WorkspaceDocument.HostKey
    }
    private struct HostRequirement: Codable, Hashable {
        let workspaceID: UUID
        let scope: String
        let capability: String
    }
    private struct Catalog: Codable {
        var version = 4
        var workspaces: [InstalledWorkspace] = []
        var revokedFingerprints: Set<String> = []
        var acceptedImports: [AcceptedImport] = []
        var hostRevocations: Set<HostRevocation> = []
        var scopedHostRevocations: Set<ScopedHostRevocation> = []
        var hostRequirements: Set<HostRequirement> = []
        var needsSanitizedPublication = false

        init() {}
        private enum CodingKeys: String, CodingKey {
            case version, workspaces, revokedFingerprints, acceptedImports, hostRevocations, scopedHostRevocations, hostRequirements
        }
        init(from decoder: Decoder) throws {
            let fields = try decoder.container(keyedBy: CodingKeys.self)
            let storedVersion = try fields.decode(Int.self, forKey: .version)
            guard (1...4).contains(storedVersion) else { throw WorkspaceStoreError.corruptStore }
            workspaces = try fields.decode([InstalledWorkspace].self, forKey: .workspaces)
            revokedFingerprints = try fields.decode(Set<String>.self, forKey: .revokedFingerprints)
            guard workspaces.count <= 32, revokedFingerprints.count <= 4096 else { throw WorkspaceStoreError.corruptStore }
            if storedVersion == 1 {
                guard !fields.contains(.acceptedImports), !fields.contains(.hostRevocations),
                      !fields.contains(.scopedHostRevocations), !fields.contains(.hostRequirements) else { throw WorkspaceStoreError.corruptStore }
                // A versioned storage migration, never a permissive document
                // import path. Earlier catalogs used a single public-key candidate.
                workspaces = try workspaces.map(WorkspaceStore.migrateVersionOneWorkspace)
                for workspace in workspaces {
                    let document = try workspace.document()
                    acceptedImports.append(.init(exporterID: document.exporterID, workspaceID: document.id,
                        bundleID: document.bundleID, transferID: nil, envelopeDigest: workspace.envelopeDigest,
                        firstAcceptedRevision: workspace.revision))
                }
            } else {
                // Version two is already published. Preserve every historical
                // identity and its original accepted revision during migration.
                acceptedImports = try fields.decode([AcceptedImport].self, forKey: .acceptedImports)
            }
            guard acceptedImports.count <= WorkspaceStore.importHistoryLimit else { throw WorkspaceStoreError.corruptStore }
            // Version three already published endpoint-wide negative trust.
            // Keep its exact representation and decisions; the new fields add
            // alias-scoped denials and mandatory policy without narrowing it.
            if storedVersion >= 3 {
                hostRevocations = try fields.decode(Set<HostRevocation>.self, forKey: .hostRevocations)
            } else if fields.contains(.hostRevocations) { throw WorkspaceStoreError.corruptStore }
            if storedVersion == 4 {
                scopedHostRevocations = try fields.decode(Set<ScopedHostRevocation>.self, forKey: .scopedHostRevocations)
                hostRequirements = try fields.decode(Set<HostRequirement>.self, forKey: .hostRequirements)
            } else {
                guard !fields.contains(.scopedHostRevocations), !fields.contains(.hostRequirements) else { throw WorkspaceStoreError.corruptStore }
                for workspace in workspaces {
                    for trust in try workspace.document().hostTrust {
                        for key in trust.revokedKeys {
                            if storedVersion < 3 { hostRevocations.insert(.init(host: trust.host, port: trust.port, key: key)) }
                            scopedHostRevocations.insert(.init(workspaceID: workspace.id, scope: trust.canonicalTrustScope, key: key))
                            guard hostRevocations.count <= WorkspaceStore.hostRevocationLimit,
                                  scopedHostRevocations.count <= WorkspaceStore.scopedHostRevocationLimit else { throw WorkspaceStoreError.corruptStore }
                        }
                        for capability in trust.requiredPolicyRequirements {
                            hostRequirements.insert(.init(workspaceID: workspace.id, scope: trust.canonicalTrustScope, capability: capability))
                            guard hostRequirements.count <= WorkspaceStore.hostRequirementLimit else { throw WorkspaceStoreError.corruptStore }
                        }
                    }
                }
            }
            guard hostRevocations.count <= WorkspaceStore.hostRevocationLimit,
                  scopedHostRevocations.count <= WorkspaceStore.scopedHostRevocationLimit,
                  hostRequirements.count <= WorkspaceStore.hostRequirementLimit else { throw WorkspaceStoreError.corruptStore }
            needsSanitizedPublication = storedVersion != 4
            // Older catalogs could retain opaque desktop annotations. Reuse the
            // wire validator so neither migration nor re-export can preserve an
            // untyped private value in this public configuration store.
            for index in workspaces.indices {
                let saved = workspaces[index], document = try saved.document()
                let portability = saved.findings.map {
                    AirGapArchive.Portability(entityID: $0.entityID, setting: $0.setting, disposition: $0.disposition, reason: $0.reason)
                }
                let annotations = saved.inactiveSettings.map {
                    AirGapArchive.Annotation(entityID: $0.entityID, setting: $0.setting, original: $0.original)
                }
                let metadata = try AirGapArchive.validatedMetadata(workspace: document, portability: portability, annotations: annotations)
                if saved.manifest != document.raw || metadata.portability != portability || metadata.annotations != annotations {
                    needsSanitizedPublication = true
                    workspaces[index] = .init(id: saved.id, revision: saved.revision, manifest: document.raw,
                        envelopeDigest: saved.envelopeDigest, bindings: saved.bindings,
                        inactiveSettings: metadata.annotations.map { .init(entityID: $0.entityID, setting: $0.setting, original: $0.original) },
                        findings: metadata.portability.map { .init(entityID: $0.entityID, setting: $0.setting, disposition: $0.disposition, reason: $0.reason) },
                        connectLater: saved.connectLater)
                }
            }
        }
    }
    private struct Journal: Codable {
        var version = 1
        let transaction: UUID
        let targetRevision: UUID
        let newAccounts: [String]
        let retiredAccounts: [String]
    }
    private let persistence: any WorkspacePersistence
    private let vault: any WorkspaceCredentialVault
    private static let catalogLimit = 16 * 1024 * 1024
    private static let journalLimit = 128 * 1024
    static let importHistoryLimit = 4096
    static let hostRevocationLimit = 4096
    private static let scopedHostRevocationLimit = 16384
    private static let hostRequirementLimit = 16384

    public init(persistence: any WorkspacePersistence, vault: any WorkspaceCredentialVault) {
        self.persistence = persistence; self.vault = vault
    }

    public func load() throws -> [InstalledWorkspace] {
        try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            return catalog.workspaces
        }
    }

    public func loadWithCredentialAvailability() throws -> [(workspace: InstalledWorkspace, credentialIDs: Set<String>)] {
        try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            return try catalog.workspaces.map { ($0, try availableCredentials($0, catalog: catalog)) }
        }
    }

    public func loadWithLocalPolicy() throws -> [WorkspaceLocalState] {
        try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            return try catalog.workspaces.map { workspace in
                let document = try workspace.document()
                let available = try availableCredentials(workspace, catalog: catalog)
                return .init(workspace: workspace, credentialIDs: available,
                      revokedHostKeysByTrustScope: hostRevocations(workspace.id, document: document, catalog: catalog),
                      requiredHostCapabilitiesByTrustScope: hostRequirements(workspace.id, catalog: catalog),
                      routes: try readiness(document: document, workspaceID: workspace.id, available: available,
                          findings: workspace.findings, catalog: catalog, support: .offlineOnly))
            }
        }
    }

    /// All readiness decisions describe the same durable revision and actual
    /// protected-key availability, including persisted negative trust.
    public func loadWithReadiness(support: WorkspaceDocument.RouteSupport = .offlineOnly) throws
        -> [(workspace: InstalledWorkspace, credentialIDs: Set<String>, routes: [WorkspaceDocument.RouteReadiness])] {
        try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            return try catalog.workspaces.map { saved in
                let available = try availableCredentials(saved, catalog: catalog)
                return (saved, available, try readiness(document: saved.document(), workspaceID: saved.id,
                    available: available, findings: saved.findings, catalog: catalog, support: support))
            }
        }
    }

    /// Evaluate included and reusable local identities with the replacement
    /// decision under one lease. A public document has no transfer ID or keys.
    public func previewImport(_ archive: AirGapArchive, envelopeDigest: Data, transferID: Data? = nil,
                              support: WorkspaceDocument.RouteSupport = .offlineOnly) throws
        -> (assessment: WorkspaceImportAssessment, routes: [WorkspaceDocument.RouteReadiness]) {
        try validateIdentity(envelopeDigest, transferID: transferID)
        return try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            let document = archive.workspace
            let review = try assessment(document, envelopeDigest: envelopeDigest, transferID: transferID, catalog: catalog)
            var fingerprints = Set<String>()
            if let old = review.existingWorkspace {
                let available = try availableCredentials(old, catalog: catalog)
                fingerprints.formUnion(try old.document().credentials.filter { available.contains($0.id) }.map(\.fingerprint))
            }
            let included = Set(archive.keys.map(\.credentialID))
            fingerprints.formUnion(document.credentials.filter { included.contains($0.id) }.map(\.fingerprint))
            let available = Set(document.credentials.filter { fingerprints.contains($0.fingerprint) }.map(\.id))
            return (review, try readiness(document: document, workspaceID: review.existingWorkspace?.id,
                available: available,
                findings: archive.portability.map { .init(entityID: $0.entityID, setting: $0.setting, disposition: $0.disposition, reason: $0.reason) },
                catalog: catalog, support: support))
        }
    }

    public func assessImport(_ document: WorkspaceDocument, envelopeDigest: Data,
                             transferID: Data) throws -> WorkspaceImportAssessment {
        try validateIdentity(envelopeDigest, transferID: transferID)
        return try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            return try assessment(document, envelopeDigest: envelopeDigest, transferID: transferID, catalog: catalog)
        }
    }

    /// Review uses the same durable history checked again by install. An old
    /// recording cannot become a new export merely because another one replaced it.
    public func preview(_ document: WorkspaceDocument, envelopeDigest: Data, transferID: Data? = nil) throws -> WorkspaceReimportReview {
        try validateIdentity(envelopeDigest, transferID: transferID)
        return try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            let review = try assessment(document, envelopeDigest: envelopeDigest, transferID: transferID, catalog: catalog)
            return .init(expectedRevision: review.existingWorkspace?.revision, removesMachines: review.removesMachines,
                         isRollback: review.requiresRollbackApproval,
                         preservedHostRevocationCount: review.preservedHostRevocationCount,
                         preservedHostRequirementCount: review.preservedHostRequirementCount,
                         firstAcceptedRevision: review.firstAcceptedRevision)
        }
    }

    public func install(_ archive: AirGapArchive, envelopeDigest: Data, transferID: Data,
                        expectedRevision: UUID?, sourceAndContentsConfirmed: Bool,
                        allowMachineRemoval: Bool = false, allowHistoricalRollback: Bool = false) throws -> WorkspaceImportReceipt {
        try install(archive, envelopeDigest: envelopeDigest, expectedRevision: expectedRevision,
                    sourceAndContentsConfirmed: sourceAndContentsConfirmed, allowMachineRemoval: allowMachineRemoval,
                    allowRollback: allowHistoricalRollback, transferID: transferID)
    }

    /// The approval is a local UI decision, never a flag decoded from a package.
    /// expectedRevision is captured when previewing this exact exporter/workspace.
    public func install(_ archive: AirGapArchive, envelopeDigest: Data,
                        expectedRevision: UUID?, sourceAndContentsConfirmed: Bool,
                        allowMachineRemoval: Bool = false, allowRollback: Bool = false,
                        transferID: Data? = nil) throws -> WorkspaceImportReceipt {
        guard sourceAndContentsConfirmed, envelopeDigest.count == 32 else { throw WorkspaceStoreError.reviewRequired }
        try validateIdentity(envelopeDigest, transferID: transferID)
        try Task.checkCancellation()
        return try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            let document = archive.workspace
            let index = try catalog.workspaces.firstIndex {
                let prior = try $0.document()
                return prior.exporterID == document.exporterID && prior.id == document.id
            }
            let old = index.map { catalog.workspaces[$0] }
            guard old?.revision == expectedRevision else { throw WorkspaceStoreError.staleReview }
            let rollback = try checkHistory(document, digest: envelopeDigest, transferID: transferID, old: old, catalog: catalog)
            guard !rollback || allowRollback else { throw WorkspaceStoreError.rollbackNeedsApproval }
            var historyUpdated = false
            if let transferID, let historyIndex = catalog.acceptedImports.firstIndex(where: {
                $0.exporterID == document.exporterID && $0.bundleID == document.bundleID && $0.transferID == nil
            }) {
                catalog.acceptedImports[historyIndex].transferID = transferID
                historyUpdated = true
            }
            guard !document.credentials.contains(where: { catalog.revokedFingerprints.contains($0.fingerprint) }) else {
                throw WorkspaceStoreError.revokedIdentity
            }
            if let old {
                let oldIDs = Set(try old.document().machines.map(\.id))
                guard allowMachineRemoval || oldIDs.isSubset(of: Set(document.machines.map(\.id))) else {
                    throw WorkspaceStoreError.removalNeedsApproval
                }
                if old.envelopeDigest == envelopeDigest {
                    guard old.manifest == document.raw else { throw WorkspaceStoreError.corruptStore }
                    let available = try old.bindings.filter { try vault.fingerprint(account: $0.account) == $0.fingerprint }
                    let fingerprints = Set(available.map(\.fingerprint))
                    let references = Dictionary(uniqueKeysWithValues: document.credentials.map { ($0.id, $0.fingerprint) })
                    if !historyUpdated, archive.keys.allSatisfy({ references[$0.credentialID].map(fingerprints.contains) == true }) {
                        return receipt(old, requested: document.credentials.map(\.id),
                                       available: Set(document.credentials.filter { fingerprints.contains($0.fingerprint) }.map(\.id)), cleanupPending: false)
                    }
                }
            } else if catalog.workspaces.count >= 32 { throw FTError.limit("store.workspaces") }

            let transaction = UUID(), revision = UUID()
            let prefix = "ft-import:\(transaction.uuidString):"
            let references = Dictionary(uniqueKeysWithValues: document.credentials.map { ($0.id, $0.fingerprint) })
            var byFingerprint: [String: String] = [:]
            for binding in old?.bindings ?? [] where try vault.fingerprint(account: binding.account) == binding.fingerprint {
                byFingerprint[binding.fingerprint] = binding.account
            }
            var creations: [(account: String, material: Data, fingerprint: String)] = []
            for key in archive.keys {
                guard let fingerprint = references[key.credentialID] else { throw FTError.malformed("store.key_reference") }
                if byFingerprint[fingerprint] == nil {
                    let account = prefix + String(creations.count)
                    byFingerprint[fingerprint] = account
                    creations.append((account, key.container, fingerprint))
                }
            }
            // A shared public identity can satisfy several manifest aliases without
            // making duplicate Keychain items or weakening route-specific policy.
            let bindings = document.credentials.compactMap { credential -> InstalledWorkspace.Binding? in
                guard let account = byFingerprint[credential.fingerprint] else { return nil }
                return .init(credentialID: credential.id, fingerprint: credential.fingerprint, account: account)
            }
            let installed = InstalledWorkspace(id: old?.id ?? UUID(), revision: revision, manifest: document.raw,
                envelopeDigest: envelopeDigest, bindings: bindings,
                inactiveSettings: archive.annotations.map { .init(entityID: $0.entityID, setting: $0.setting, original: $0.original) },
                findings: archive.portability.map { .init(entityID: $0.entityID, setting: $0.setting, disposition: $0.disposition, reason: $0.reason) },
                connectLater: true)
            if acceptedImport(document, catalog: catalog) == nil {
                guard catalog.acceptedImports.count < Self.importHistoryLimit else { throw WorkspaceStoreError.importHistoryFull }
                catalog.acceptedImports.append(.init(exporterID: document.exporterID, workspaceID: document.id,
                    bundleID: document.bundleID, transferID: transferID, envelopeDigest: envelopeDigest, firstAcceptedRevision: revision))
            }
            // Preserve endpoint-wide negative decisions as well as mandatory
            // policy tied to this workspace's effective trust aliases. Neither
            // renamed routes nor older recordings can erase accepted denials.
            for trust in document.hostTrust {
                for key in trust.revokedKeys {
                    catalog.hostRevocations.insert(.init(host: trust.host, port: trust.port, key: key))
                    catalog.scopedHostRevocations.insert(.init(workspaceID: installed.id, scope: trust.canonicalTrustScope, key: key))
                    guard catalog.hostRevocations.count <= Self.hostRevocationLimit,
                          catalog.scopedHostRevocations.count <= Self.scopedHostRevocationLimit else { throw FTError.limit("store.host_revocations") }
                }
                for capability in trust.requiredPolicyRequirements {
                    catalog.hostRequirements.insert(.init(workspaceID: installed.id, scope: trust.canonicalTrustScope, capability: capability))
                    guard catalog.hostRequirements.count <= Self.hostRequirementLimit else { throw FTError.limit("store.host_requirements") }
                }
            }
            if let index { catalog.workspaces[index] = installed } else { catalog.workspaces.append(installed) }
            let next = try encode(catalog, limit: Self.catalogLimit)
            let retained = Set(bindings.map(\.account))
            let retired = Set(old?.bindings.map(\.account) ?? []).subtracting(retained).sorted()
            let journal = Journal(transaction: transaction, targetRevision: revision,
                                  newAccounts: creations.map(\.account), retiredAccounts: retired)
            // Record the full intended ownership set BEFORE the first Keychain call.
            // Even an insert that throws after taking effect is recoverable.
            try persistence.write(encode(journal, limit: Self.journalLimit), to: .journal)
            do {
                for (account, material, fingerprint) in creations {
                    try Task.checkCancellation()
                    try vault.create(account: account, privateContainer: material, fingerprint: fingerprint)
                }
                for binding in bindings {
                    guard try vault.fingerprint(account: binding.account) == binding.fingerprint else { throw FTError.storage }
                }
                try Task.checkCancellation()
                try persistence.write(next, to: .catalog)
            } catch {
                // Neither errors nor task cancellation prove publication failed.
                // Read the actual durable catalog before considering key cleanup.
                do {
                    var actual = try readCatalog()
                    try recover(catalog: &actual)
                } catch { throw WorkspaceStoreError.recoveryRequired }
                throw error
            }
            // After publication, cancellation cannot turn success into rollback.
            // Failed journal cleanup is reported without repeating key insertion.
            var cleanupPending = false
            do { try recover(catalog: &catalog) } catch { cleanupPending = true }
            return receipt(installed, requested: document.credentials.map(\.id),
                           available: Set(bindings.map(\.credentialID)), cleanupPending: cleanupPending)
        }
    }

    /// Local revocation persists before any cleanup. It is not remote revocation.
    /// Transport integrations must also cancel signer grants and affected sessions.
    public func revokeLocally(fingerprint: String) throws {
        try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            _ = try WorkspaceDocument.fingerprint(fingerprint)
            guard catalog.revokedFingerprints.count < 4096 || catalog.revokedFingerprints.contains(fingerprint)
            else { throw FTError.limit("store.revocations") }
            catalog.revokedFingerprints.insert(fingerprint)
            try persistence.write(encode(catalog, limit: Self.catalogLimit), to: .catalog)
        }
    }

    public func availableCredentialIDs(workspaceID: UUID) throws -> Set<String> {
        try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            guard let workspace = catalog.workspaces.first(where: { $0.id == workspaceID }) else { throw FTError.stale }
            return try availableCredentials(workspace, catalog: catalog)
        }
    }

    private func availableCredentials(_ workspace: InstalledWorkspace, catalog: Catalog) throws -> Set<String> {
        let fingerprints = Set(try workspace.bindings.filter {
            if catalog.revokedFingerprints.contains($0.fingerprint) { return false }
            return try vault.fingerprint(account: $0.account) == $0.fingerprint
        }.map(\.fingerprint))
        return Set(try workspace.document().credentials.filter { fingerprints.contains($0.fingerprint) }.map(\.id))
    }

    /// Reconcile the complete accepted identity under the storage lease. Matching
    /// old workspace bytes alone cannot prove a migrated transfer-ID binding was
    /// published; the history and its durable journal must agree as well.
    public func recoverImport(exporterID: String, workspaceID: String, envelopeDigest: Data,
                              transferID: Data? = nil) throws -> WorkspaceImportReceipt? {
        try validateIdentity(envelopeDigest, transferID: transferID)
        return try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            guard let workspace = try catalog.workspaces.first(where: {
                let document = try $0.document()
                return document.exporterID == exporterID && document.id == workspaceID && $0.envelopeDigest == envelopeDigest
            }) else { return nil }
            let document = try workspace.document()
            guard catalog.acceptedImports.contains(where: {
                $0.exporterID == document.exporterID && $0.workspaceID == document.id && $0.bundleID == document.bundleID && $0.envelopeDigest == envelopeDigest
                    && (transferID == nil || $0.transferID == transferID)
            }) else { return nil }
            let available = try availableCredentials(workspace, catalog: catalog)
            return receipt(workspace, requested: document.credentials.map(\.id), available: available, cleanupPending: false)
        }
    }

    /// Durable local denial is independent of incoming pins and bundle freshness.
    /// Future transport owners must also retire attempts using this trust scope.
    public func revokeHostLocally(workspaceID: UUID, trustID: String, key: WorkspaceDocument.HostKey) throws {
        try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            guard let workspace = catalog.workspaces.first(where: { $0.id == workspaceID }),
                  let trust = try workspace.document().hostTrust.first(where: { $0.id == trustID }) else { throw FTError.stale }
            catalog.hostRevocations.insert(.init(host: trust.host, port: trust.port, key: key))
            catalog.scopedHostRevocations.insert(.init(workspaceID: workspaceID, scope: trust.canonicalTrustScope, key: key))
            guard catalog.hostRevocations.count <= Self.hostRevocationLimit,
                  catalog.scopedHostRevocations.count <= Self.scopedHostRevocationLimit else { throw FTError.limit("store.host_revocations") }
            try persistence.write(encode(catalog, limit: Self.catalogLimit), to: .catalog)
        }
    }

    /// Imported positive pins cannot erase an endpoint-wide local rejection.
    public func revokeHostKeyLocally(host: String, port: UInt16, algorithm: String, fingerprint: String) throws {
        let entry = HostRevocation(host: try WorkspaceDocument.host(host), port: try WorkspaceDocument.port(UInt64(port)),
                                  key: try WorkspaceDocument.HostKey(algorithm: algorithm, fingerprint: fingerprint))
        try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            guard catalog.hostRevocations.count < Self.hostRevocationLimit || catalog.hostRevocations.contains(entry) else {
                throw FTError.limit("store.host_revocations")
            }
            catalog.hostRevocations.insert(entry)
            try persistence.write(encode(catalog, limit: Self.catalogLimit), to: .catalog)
        }
    }

    /// A transport must consult durable local policy before using imported trust.
    /// It still owns cryptographic certificate/signature checks and qualification
    /// of its supported algorithms and capabilities.
    public func verifyHostKey(workspaceID: UUID, routeID: String, presented: WorkspaceDocument.HostKey,
                              certificateAuthority: WorkspaceDocument.HostKey? = nil,
                              support: WorkspaceDocument.RouteSupport = .offlineOnly) throws -> WorkspaceDocument.HostVerificationDecision {
        try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            guard let saved = catalog.workspaces.first(where: { $0.id == workspaceID }) else { throw FTError.stale }
            let document = try saved.document()
            guard let route = document.routes.first(where: { $0.id == routeID }),
                  let trust = document.hostTrust.first(where: { $0.id == route.trustID }) else { throw FTError.stale }
            let rejected = hostRevocations(workspaceID, document: document, catalog: catalog)
            let fingerprints = Set((rejected[trust.canonicalTrustScope] ?? []).map(\.fingerprint))
            if fingerprints.contains(presented.fingerprint) || certificateAuthority.map({ fingerprints.contains($0.fingerprint) }) == true {
                return .revoked
            }
            if WorkspacePortabilityPolicy.blockedRouteIDs(in: document,
                requiredEntityIDs: Set(saved.findings.filter { $0.disposition == 1 }.map(\.entityID))).contains(routeID) {
                return .unsupportedPolicy
            }
            return try document.verifyHostKey(forRouteID: routeID, presented: presented,
                certificateAuthority: certificateAuthority, reviewedTrustIDs: Set(document.hostTrust.map(\.id)),
                support: support, revokedHostKeysByTrustScope: rejected,
                requiredHostCapabilitiesByTrustScope: hostRequirements(workspaceID, catalog: catalog))
        }
    }

    public func routeReadiness(workspaceID: UUID,
                               support: WorkspaceDocument.RouteSupport = .offlineOnly) throws -> [WorkspaceDocument.RouteReadiness] {
        try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            guard let saved = catalog.workspaces.first(where: { $0.id == workspaceID }) else { throw FTError.stale }
            return try readiness(document: saved.document(), workspaceID: workspaceID,
                available: availableCredentials(saved, catalog: catalog), findings: saved.findings, catalog: catalog, support: support)
        }
    }

    private func readiness(document: WorkspaceDocument, workspaceID: UUID?, available: Set<String>,
                           findings: [InstalledWorkspace.PortabilityFinding], catalog: Catalog,
                           support: WorkspaceDocument.RouteSupport) throws -> [WorkspaceDocument.RouteReadiness] {
        let blocked = WorkspacePortabilityPolicy.blockedRouteIDs(in: document,
            requiredEntityIDs: Set(findings.filter { $0.disposition == 1 }.map(\.entityID)))
        let rejected = hostRevocations(workspaceID, document: document, catalog: catalog)
        let required = workspaceID.map { hostRequirements($0, catalog: catalog) } ?? [:]
        let trustIDs = Set(document.hostTrust.map(\.id))
        return try document.routes.map { route in
            let state = try document.readiness(forRouteID: route.id, availableCredentialIDs: available,
                reviewedTrustIDs: trustIDs, revokedCredentialFingerprints: catalog.revokedFingerprints,
                support: support, revokedHostKeysByTrustScope: rejected, requiredHostCapabilitiesByTrustScope: required)
            var issues = state.issues
            if blocked.contains(route.id), !issues.contains(.unsupportedRequirement) { issues.append(.unsupportedRequirement) }
            return .init(routeID: route.id, usableCredentialIDs: state.usableCredentialIDs, issues: issues)
        }
    }

    public func revokedHostKeys(workspaceID: UUID) throws -> [String: Set<WorkspaceDocument.HostKey>] {
        try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            guard let saved = catalog.workspaces.first(where: { $0.id == workspaceID }) else { throw FTError.stale }
            return hostRevocations(workspaceID, document: try saved.document(), catalog: catalog)
        }
    }

    public func requiredHostCapabilities(workspaceID: UUID) throws -> [String: Set<String>] {
        try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            guard catalog.workspaces.contains(where: { $0.id == workspaceID }) else { throw FTError.stale }
            return hostRequirements(workspaceID, catalog: catalog)
        }
    }

    private func hostRevocations(_ workspaceID: UUID?, document: WorkspaceDocument,
                                 catalog: Catalog) -> [String: Set<WorkspaceDocument.HostKey>] {
        var result: [String: Set<WorkspaceDocument.HostKey>] = [:]
        for revocation in catalog.scopedHostRevocations where revocation.workspaceID == workspaceID {
            result[revocation.scope, default: []].insert(revocation.key)
        }
        for trust in document.hostTrust {
            result[trust.canonicalTrustScope, default: []].formUnion(catalog.hostRevocations.filter {
                $0.host == trust.host && $0.port == trust.port
            }.map(\.key))
        }
        return result
    }

    private func hostRequirements(_ workspaceID: UUID, catalog: Catalog) -> [String: Set<String>] {
        var result: [String: Set<String>] = [:]
        for requirement in catalog.hostRequirements where requirement.workspaceID == workspaceID {
            result[requirement.scope, default: []].insert(requirement.capability)
        }
        return result
    }

    private static func validHostRequirement(_ value: String) -> Bool {
        let identifier = value.hasPrefix("extension:") ? String(value.dropFirst(10)) : value
        return (try? WorkspaceDocument.identifier(identifier)) != nil
    }

    private func validateIdentity(_ digest: Data, transferID: Data?) throws {
        guard digest.count == 32, transferID == nil || transferID?.count == 16 else { throw WorkspaceStoreError.reviewRequired }
    }

    private func matchingWorkspace(_ document: WorkspaceDocument, in catalog: Catalog) throws -> InstalledWorkspace? {
        try catalog.workspaces.first {
            let saved = try $0.document()
            return saved.exporterID == document.exporterID && saved.id == document.id
        }
    }

    private func acceptedImport(_ document: WorkspaceDocument, catalog: Catalog) -> AcceptedImport? {
        catalog.acceptedImports.first { $0.exporterID == document.exporterID && $0.bundleID == document.bundleID }
    }

    private func assessment(_ document: WorkspaceDocument, envelopeDigest: Data, transferID: Data?,
                            catalog: Catalog) throws -> WorkspaceImportAssessment {
        let old = try matchingWorkspace(document, in: catalog)
        let rollback = try checkHistory(document, digest: envelopeDigest, transferID: transferID, old: old, catalog: catalog)
        let oldIDs = try old.map { Set(try $0.document().machines.map(\.id)) } ?? []
        let revoked = hostRevocations(old?.id, document: document, catalog: catalog)
        return .init(existingWorkspace: old, firstAcceptedRevision: acceptedImport(document, catalog: catalog)?.firstAcceptedRevision,
                     requiresRollbackApproval: rollback, removesMachines: !oldIDs.isSubset(of: Set(document.machines.map(\.id))),
                     preservedHostRevocationCount: revoked.values.reduce(0) { $0 + $1.count },
                     preservedHostRequirementCount: catalog.hostRequirements.filter { $0.workspaceID == old?.id }.count)
    }

    private func checkHistory(_ document: WorkspaceDocument, digest: Data, transferID: Data?,
                              old: InstalledWorkspace?, catalog: Catalog) throws -> Bool {
        guard !document.credentials.contains(where: { catalog.revokedFingerprints.contains($0.fingerprint) }) else {
            throw WorkspaceStoreError.revokedIdentity
        }
        // Reusing an existing protected identity must enforce the same binding
        // as an included private key, even when this archive carries only refs.
        let incoming = Dictionary(grouping: document.credentials, by: \.fingerprint)
        for saved in catalog.workspaces {
            let prior = try saved.document()
            let credentials = Dictionary(uniqueKeysWithValues: prior.credentials.map { ($0.id, $0) })
            for binding in saved.bindings {
                guard let previous = credentials[binding.credentialID] else { throw WorkspaceStoreError.corruptStore }
                for candidate in incoming[binding.fingerprint] ?? [] {
                    guard candidate.algorithm == previous.algorithm else { throw FTError.malformed("store.credential_identity") }
                }
            }
        }
        let accepted = acceptedImport(document, catalog: catalog)
        if let accepted {
            guard accepted.workspaceID == document.id, accepted.envelopeDigest == digest,
                  accepted.transferID == nil || transferID == nil || accepted.transferID == transferID else {
                throw WorkspaceStoreError.immutableExportConflict
            }
        }
        for prior in catalog.acceptedImports where prior.envelopeDigest == digest || (transferID != nil && prior.transferID == transferID) {
            guard prior.envelopeDigest == digest, prior.exporterID == document.exporterID,
                  prior.workspaceID == document.id, prior.bundleID == document.bundleID,
                  prior.transferID == nil || transferID == nil || prior.transferID == transferID else {
                throw WorkspaceStoreError.immutableExportConflict
            }
        }
        guard accepted != nil || catalog.acceptedImports.count < Self.importHistoryLimit else { throw WorkspaceStoreError.importHistoryFull }
        return accepted != nil && old?.envelopeDigest != digest
    }

    private static func migrateVersionOneWorkspace(_ saved: InstalledWorkspace) throws -> InstalledWorkspace {
        var root = try StrictJSON.decode(saved.manifest).object()
        var workspace = try root.required("workspace").object()
        var changed = false
        let routes = try workspace.required("routes").array().map { value -> JSONValue in
            var route = try value.object()
            if let singular = route.removeValue(forKey: "credential_ref") {
                guard route["credential_refs"] == nil, route["authentication_methods"] == nil else { throw WorkspaceStoreError.corruptStore }
                _ = try WorkspaceDocument.identifier(singular.string())
                route["credential_refs"] = .array([singular])
                route["authentication_methods"] = .array([.string("publickey")])
                changed = true
            }
            return .object(route)
        }
        let trusts = try workspace.required("host_trust").array().map { value -> JSONValue in
            var trust = try value.object()
            if trust["revoked_keys"] == nil { trust["revoked_keys"] = .array([]); changed = true }
            return .object(trust)
        }
        let manifest: Data
        if changed {
            workspace["routes"] = .array(routes); workspace["host_trust"] = .array(trusts)
            root["workspace"] = .object(workspace)
            manifest = try WorkspaceDocument(data: StrictJSON.encode(.object(root))).raw
        } else { manifest = try WorkspaceDocument(data: saved.manifest).raw }
        return .init(id: saved.id, revision: saved.revision, manifest: manifest, envelopeDigest: saved.envelopeDigest,
                     bindings: saved.bindings, inactiveSettings: saved.inactiveSettings, findings: saved.findings, connectLater: saved.connectLater)
    }

    private func receipt(_ workspace: InstalledWorkspace, requested: [String], available: Set<String>, cleanupPending: Bool) -> WorkspaceImportReceipt {
        // The manifest was validated before publication; no fallible Keychain
        // operation is allowed to disguise a committed transaction as rollback.
        let missing = requested.filter { !available.contains($0) }
        return .init(workspace: workspace, missingCredentialIDs: missing, cleanupPending: cleanupPending)
    }
    private func encode<T: Encodable>(_ value: T, limit: Int) throws -> Data {
        let result = try JSONEncoder().encode(value)
        guard result.count <= limit else { throw FTError.limit("store.bytes") }
        return result
    }
    private func readCatalog() throws -> Catalog {
        guard let data = try persistence.read(.catalog, limit: Self.catalogLimit) else { return Catalog() }
        guard let catalog = try? JSONDecoder().decode(Catalog.self, from: data), catalog.version == 4,
              catalog.workspaces.count <= 32, catalog.revokedFingerprints.count <= 4096,
              catalog.revokedFingerprints.allSatisfy({ (try? WorkspaceDocument.fingerprint($0)) != nil }),
              catalog.acceptedImports.count <= Self.importHistoryLimit, catalog.hostRevocations.count <= Self.hostRevocationLimit,
              catalog.scopedHostRevocations.count <= Self.scopedHostRevocationLimit,
              catalog.hostRequirements.count <= Self.hostRequirementLimit,
              Set(catalog.workspaces.map(\.id)).count == catalog.workspaces.count,
              Set(catalog.workspaces.map(\.revision)).count == catalog.workspaces.count else { throw WorkspaceStoreError.corruptStore }
        var namespaces = Set<[String]>()
        var accountIdentities: [String: [String]] = [:]
        let workspaceIDs = Set(catalog.workspaces.map(\.id))
        var bundleIDs = Set<[String]>(), transferIDs = Set<Data>(), digests = Set<Data>()
        for export in catalog.acceptedImports {
            guard (try? WorkspaceDocument.identifier(export.exporterID)) != nil,
                  (try? WorkspaceDocument.identifier(export.workspaceID)) != nil,
                  (try? WorkspaceDocument.identifier(export.bundleID)) != nil,
                  export.envelopeDigest.count == 32, digests.insert(export.envelopeDigest).inserted,
                  export.transferID == nil || export.transferID?.count == 16,
                  bundleIDs.insert([export.exporterID, export.bundleID]).inserted else { throw WorkspaceStoreError.corruptStore }
            if let transferID = export.transferID, !transferIDs.insert(transferID).inserted { throw WorkspaceStoreError.corruptStore }
        }
        for revocation in catalog.hostRevocations {
            guard revocation.port > 0, (try? WorkspaceDocument.host(revocation.host)) == revocation.host else {
                throw WorkspaceStoreError.corruptStore
            }
        }
        for revocation in catalog.scopedHostRevocations {
            guard workspaceIDs.contains(revocation.workspaceID), !revocation.scope.isEmpty,
                  revocation.scope.utf8.count <= FTLimit.text + 64 else { throw WorkspaceStoreError.corruptStore }
        }
        for requirement in catalog.hostRequirements {
            guard workspaceIDs.contains(requirement.workspaceID), !requirement.scope.isEmpty,
                  requirement.scope.utf8.count <= FTLimit.text + 64,
                  Self.validHostRequirement(requirement.capability) else { throw WorkspaceStoreError.corruptStore }
        }
        for workspace in catalog.workspaces {
            let document = try workspace.document()
            guard workspace.connectLater, workspace.envelopeDigest.count == 32, workspace.bindings.count <= 256,
                  workspace.inactiveSettings.count <= 1024, workspace.findings.count <= 1024,
                  namespaces.insert([document.exporterID, document.id]).inserted else { throw WorkspaceStoreError.corruptStore }
            guard catalog.acceptedImports.contains(where: {
                $0.exporterID == document.exporterID && $0.workspaceID == document.id && $0.bundleID == document.bundleID && $0.envelopeDigest == workspace.envelopeDigest
            }) else { throw WorkspaceStoreError.corruptStore }
            for trust in document.hostTrust {
                guard trust.revokedKeys.allSatisfy({
                    catalog.hostRevocations.contains(.init(host: trust.host, port: trust.port, key: $0)) &&
                    catalog.scopedHostRevocations.contains(.init(workspaceID: workspace.id, scope: trust.canonicalTrustScope, key: $0))
                }) else {
                    throw WorkspaceStoreError.corruptStore
                }
                guard trust.requiredPolicyRequirements.allSatisfy({ catalog.hostRequirements.contains(.init(workspaceID: workspace.id, scope: trust.canonicalTrustScope, capability: $0)) }) else {
                    throw WorkspaceStoreError.corruptStore
                }
            }
            let refs = Dictionary(uniqueKeysWithValues: document.credentials.map { ($0.id, $0.fingerprint) })
            guard Set(workspace.bindings.map(\.credentialID)).count == workspace.bindings.count else { throw WorkspaceStoreError.corruptStore }
            for binding in workspace.bindings {
                guard refs[binding.credentialID] == binding.fingerprint, Self.validAccount(binding.account) else {
                    throw WorkspaceStoreError.corruptStore
                }
                guard let credential = document.credentials.first(where: { $0.id == binding.credentialID }) else { throw WorkspaceStoreError.corruptStore }
                let identity = [credential.algorithm, binding.fingerprint]
                if let previous = accountIdentities[binding.account], previous != identity { throw WorkspaceStoreError.corruptStore }
                accountIdentities[binding.account] = identity
            }
        }
        // Publish only after all history, ownership and policy invariants above
        // pass. Failure never authorizes deleting journal-owned credentials.
        if catalog.needsSanitizedPublication {
            try persistence.write(encode(catalog, limit: Self.catalogLimit), to: .catalog)
        }
        return catalog
    }
    private static func validAccount(_ account: String, transaction: UUID? = nil) -> Bool {
        let parts = account.split(separator: ":", omittingEmptySubsequences: false)
        guard parts.count == 3, parts[0] == "ft-import", let id = UUID(uuidString: String(parts[1])),
              parts[1] == Substring(id.uuidString), let number = Int(parts[2]), (0..<256).contains(number),
              parts[2] == Substring(String(number)) else { return false }
        return transaction == nil || transaction == id
    }
    private func recover(catalog: inout Catalog) throws {
        guard let data = try persistence.read(.journal, limit: Self.journalLimit) else { return }
        guard let journal = try? JSONDecoder().decode(Journal.self, from: data), journal.version == 1,
              journal.newAccounts.count <= 256, Set(journal.newAccounts).count == journal.newAccounts.count,
              journal.retiredAccounts.count <= 256, Set(journal.retiredAccounts).count == journal.retiredAccounts.count,
              journal.newAccounts.allSatisfy({ Self.validAccount($0, transaction: journal.transaction) }),
              journal.retiredAccounts.allSatisfy({ Self.validAccount($0) }) else {
            throw WorkspaceStoreError.corruptStore
        }
        let reachable = Set(catalog.workspaces.flatMap { $0.bindings.map(\.account) })
        // Readback alone is NOT durability: a write may have renamed the file
        // before its fsync failed. Reaffirm the selected catalog durably before
        // deleting either retired or abandoned keys, or removing its journal.
        // Failure here retains both generations and the recovery authority.
        try persistence.write(encode(catalog, limit: Self.catalogLimit), to: .catalog)
        for account in Set(journal.newAccounts + journal.retiredAccounts) where !reachable.contains(account) {
            try vault.remove(account: account)
        }
        // Referenced keys survive even when publication happened before an error.
        // Unknown/corrupt catalog state fails above, before this deletion path.
        try persistence.removeJournal()
    }
}

/// Private application-support storage, no imported filenames or arbitrary paths.
/// File descriptors stay inside one synchronous call; no borrowed buffers escape.
/// rename + file/directory fsync give the store's old-or-new publication contract.
public struct FileWorkspacePersistence: WorkspacePersistence {
    private let root: URL
    public init(directory: URL) throws {
        guard directory.isFileURL else { throw FTError.storage }
        root = directory.standardizedFileURL
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true,
                                                attributes: [.posixPermissions: 0o700])
        #if os(iOS)
        try FileManager.default.setAttributes([.protectionKey: FileProtectionType.complete], ofItemAtPath: root.path)
        #endif
        var url = root
        var values = URLResourceValues(); values.isExcludedFromBackup = true
        try url.setResourceValues(values)
    }
    public func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T {
        let fd = root.appendingPathComponent("store.lock").path.withCString { open($0, O_CREAT | O_RDWR | O_CLOEXEC | O_NOFOLLOW, 0o600) }
        guard fd >= 0 else { throw FTError.storage }
        defer { _ = close(fd) }
        guard flock(fd, LOCK_EX | LOCK_NB) == 0 else { throw WorkspaceStoreError.busy }
        defer { _ = flock(fd, LOCK_UN) }
        return try body()
    }
    public func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? {
        guard limit > 0, limit <= 16 * 1024 * 1024 else { throw FTError.limit("store.read_limit") }
        let url = path(slot)
        let fd = url.path.withCString { open($0, O_RDONLY | O_CLOEXEC | O_NOFOLLOW) }
        if fd < 0 {
            if errno == ENOENT { return nil }
            throw FTError.storage
        }
        let handle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
        defer { try? handle.close() }
        var result = Data()
        while let chunk = try handle.read(upToCount: min(65536, limit + 1 - result.count)), !chunk.isEmpty {
            result += chunk
            guard result.count <= limit else { throw FTError.limit("store.read") }
        }
        return result
    }
    public func write(_ data: Data, to slot: WorkspaceStorageSlot) throws {
        guard data.count <= (slot == .journal ? 128 * 1024 : 16 * 1024 * 1024) else { throw FTError.limit("store.write") }
        let destination = path(slot)
        #if os(iOS)
        try data.write(to: destination, options: [.atomic, .completeFileProtection])
        #else
        try data.write(to: destination, options: .atomic)
        #endif
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: destination.path)
        let handle = try FileHandle(forWritingTo: destination)
        defer { try? handle.close() }
        try handle.synchronize()
        try synchronizeDirectory()
    }
    public func removeJournal() throws {
        do { try FileManager.default.removeItem(at: path(.journal)) }
        catch CocoaError.fileNoSuchFile { return }
        try synchronizeDirectory()
    }
    private func path(_ slot: WorkspaceStorageSlot) -> URL { root.appendingPathComponent(slot.rawValue + ".json") }
    private func synchronizeDirectory() throws {
        let fd = root.path.withCString { open($0, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW) }
        guard fd >= 0 else { throw FTError.storage }
        defer { _ = close(fd) }
        guard fsync(fd) == 0 else { throw FTError.storage }
    }
}
