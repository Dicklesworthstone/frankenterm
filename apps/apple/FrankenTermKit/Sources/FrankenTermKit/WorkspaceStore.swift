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
    public var errorDescription: String? {
        switch self {
        case .reviewRequired: return "Confirm the source, comparison value, and import contents first."
        case .staleReview: return "The saved workspace changed. Review the import again."
        case .removalNeedsApproval: return "This import removes machines. Explicit replacement approval is required."
        case .revokedIdentity: return "This import includes a locally revoked identity."
        case .conflictingExport: return "This export reuses a previously accepted bundle or transfer identity with different contents. Create a new export on the source device."
        case .rollbackNeedsApproval: return "This recording restores a previously accepted workspace version. Review and approve that replacement explicitly."
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

/// Computed from locally accepted envelopes, never from a claimed export date or
/// sequence. A previous recording is not necessarily older than an unseen export.
public struct WorkspaceImportAssessment: Sendable {
    public let existingWorkspace: InstalledWorkspace?
    public let firstAcceptedRevision: UUID?
    public let requiresRollbackApproval: Bool
    public let removesMachines: Bool
}

/// One app-owned actor shared by every scene. There are no suspension points
/// inside a transaction, and the persistence lease also fences other instances.
public actor WorkspaceStore {
    private struct AcceptedImport: Codable {
        let exporterID: String
        let workspaceID: String
        let bundleID: String
        // Version-one catalogs did not retain transfer IDs. Migration preserves
        // their known bundle/digest bindings, then learns an ID only from an
        // authenticated rescan with the exact already accepted envelope digest.
        var transferID: Data?
        let envelopeDigest: Data
        let firstAcceptedRevision: UUID
    }
    private struct Catalog: Codable {
        var version = 2
        var workspaces: [InstalledWorkspace] = []
        var revokedFingerprints: Set<String> = []
        var acceptedImports: [AcceptedImport] = []

        init() {}
        private enum CodingKeys: String, CodingKey {
            case version, workspaces, revokedFingerprints, acceptedImports
        }
        init(from decoder: any Decoder) throws {
            let fields = try decoder.container(keyedBy: CodingKeys.self)
            version = try fields.decode(Int.self, forKey: .version)
            workspaces = try fields.decode([InstalledWorkspace].self, forKey: .workspaces)
            revokedFingerprints = try fields.decode(Set<String>.self, forKey: .revokedFingerprints)
            if version == 1 {
                guard !fields.contains(.acceptedImports) else { throw WorkspaceStoreError.corruptStore }
                acceptedImports = []
            } else {
                acceptedImports = try fields.decode([AcceptedImport].self, forKey: .acceptedImports)
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
    // This is an admission bound, not an eviction policy. Never silently discard
    // an accepted identity to make a new import fit.
    static let importHistoryLimit = 4096

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

    /// A UI snapshot binds credential availability to the same saved revisions
    /// under one lease. Counting saved bindings alone would show a revoked or
    /// removed Keychain identity as available after lock, relaunch, or key loss.
    public func loadWithCredentialAvailability() throws -> [(workspace: InstalledWorkspace, credentialIDs: Set<String>)] {
        try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            return try catalog.workspaces.map { workspace in
                (workspace, try availableCredentials(workspace, catalog: catalog))
            }
        }
    }

    /// Authentication precedes this call. Assessment and the saved revision are
    /// obtained under the same persistence lease; install rechecks both before
    /// staging any new credential so a stale UI cannot grant replacement rights.
    public func assessImport(_ document: WorkspaceDocument, envelopeDigest: Data,
                             transferID: Data) throws -> WorkspaceImportAssessment {
        guard envelopeDigest.count == 32, transferID.count == 16 else { throw WorkspaceStoreError.reviewRequired }
        return try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            return try assessment(document, envelopeDigest: envelopeDigest, transferID: transferID, catalog: catalog)
        }
    }

    /// The approval is a local UI decision, never a flag decoded from a package.
    /// expectedRevision is captured when previewing this exact exporter/workspace.
    public func install(_ archive: AirGapArchive, envelopeDigest: Data, transferID: Data,
                        expectedRevision: UUID?, sourceAndContentsConfirmed: Bool,
                        allowMachineRemoval: Bool = false, allowHistoricalRollback: Bool = false) throws -> WorkspaceImportReceipt {
        guard sourceAndContentsConfirmed, envelopeDigest.count == 32, transferID.count == 16 else { throw WorkspaceStoreError.reviewRequired }
        try Task.checkCancellation()
        return try persistence.withExclusiveAccess {
            var catalog = try readCatalog()
            try recover(catalog: &catalog)
            let document = archive.workspace
            let review = try assessment(document, envelopeDigest: envelopeDigest, transferID: transferID, catalog: catalog)
            let index = try catalog.workspaces.firstIndex {
                let prior = try $0.document()
                return prior.exporterID == document.exporterID && prior.id == document.id
            }
            let old = index.map { catalog.workspaces[$0] }
            guard old?.revision == expectedRevision else { throw WorkspaceStoreError.staleReview }
            guard !review.requiresRollbackApproval || allowHistoricalRollback else { throw WorkspaceStoreError.rollbackNeedsApproval }
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
                    if archive.keys.allSatisfy({ references[$0.credentialID].map(fingerprints.contains) == true }) {
                        // Finish a legacy catalog's migration even for a fully
                        // idempotent rescan; do not lose the newly verified ID.
                        if let accepted = acceptanceIndex(document, catalog: catalog), catalog.acceptedImports[accepted].transferID == nil {
                            catalog.acceptedImports[accepted].transferID = transferID
                            try persistence.write(encode(catalog, limit: Self.catalogLimit), to: .catalog)
                        }
                        return receipt(old, requested: document.credentials.map(\.id),
                                       available: Set(available.map(\.credentialID)), cleanupPending: false)
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
            if let index { catalog.workspaces[index] = installed } else { catalog.workspaces.append(installed) }
            if let accepted = acceptanceIndex(document, catalog: catalog) {
                catalog.acceptedImports[accepted].transferID = transferID
            } else {
                catalog.acceptedImports.append(AcceptedImport(exporterID: document.exporterID, workspaceID: document.id,
                    bundleID: document.bundleID, transferID: transferID, envelopeDigest: envelopeDigest, firstAcceptedRevision: revision))
            }
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
            guard fingerprint.utf8.count == 50, fingerprint.hasPrefix("SHA256:"),
                  catalog.revokedFingerprints.count < 4096 || catalog.revokedFingerprints.contains(fingerprint)
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
        Set(try workspace.bindings.filter {
            if catalog.revokedFingerprints.contains($0.fingerprint) { return false }
            return try vault.fingerprint(account: $0.account) == $0.fingerprint
        }.map(\.credentialID))
    }

    private func acceptanceIndex(_ document: WorkspaceDocument, catalog: Catalog) -> Int? {
        // Bundle identities belong to their exporter, independent of the current
        // workspace placement. A bundle cannot later be rebound to another one.
        catalog.acceptedImports.firstIndex { $0.exporterID == document.exporterID && $0.bundleID == document.bundleID }
    }

    private func assessment(_ document: WorkspaceDocument, envelopeDigest: Data, transferID: Data,
                            catalog: Catalog) throws -> WorkspaceImportAssessment {
        let acceptedIndex = acceptanceIndex(document, catalog: catalog)
        let accepted = acceptedIndex.map { catalog.acceptedImports[$0] }
        if let accepted {
            guard accepted.workspaceID == document.id, accepted.envelopeDigest == envelopeDigest,
                  accepted.transferID == nil || accepted.transferID == transferID else { throw WorkspaceStoreError.conflictingExport }
        }
        // FTAG transfer IDs and envelope digests bind the whole package, even if
        // a conflicting document claims a different exporter or workspace name.
        for previous in catalog.acceptedImports where previous.transferID == transferID || previous.envelopeDigest == envelopeDigest {
            guard previous.exporterID == document.exporterID, previous.workspaceID == document.id,
                  previous.bundleID == document.bundleID, previous.envelopeDigest == envelopeDigest,
                  previous.transferID == nil || previous.transferID == transferID else { throw WorkspaceStoreError.conflictingExport }
        }
        guard accepted != nil || catalog.acceptedImports.count < Self.importHistoryLimit else { throw WorkspaceStoreError.importHistoryFull }
        let old = try catalog.workspaces.first {
            let prior = try $0.document()
            return prior.exporterID == document.exporterID && prior.id == document.id
        }
        let oldIDs = try old.map { Set(try $0.document().machines.map(\.id)) } ?? []
        let newIDs = Set(document.machines.map(\.id))
        return .init(existingWorkspace: old, firstAcceptedRevision: accepted?.firstAcceptedRevision,
                     requiresRollbackApproval: accepted != nil && old?.envelopeDigest != envelopeDigest,
                     removesMachines: !oldIDs.isSubset(of: newIDs))
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
        guard var catalog = try? JSONDecoder().decode(Catalog.self, from: data), [1, 2].contains(catalog.version),
              catalog.workspaces.count <= 32, catalog.revokedFingerprints.count <= 4096,
              catalog.acceptedImports.count <= Self.importHistoryLimit,
              Set(catalog.workspaces.map(\.id)).count == catalog.workspaces.count,
              Set(catalog.workspaces.map(\.revision)).count == catalog.workspaces.count else { throw WorkspaceStoreError.corruptStore }
        var namespaces = Set<[String]>()
        for workspace in catalog.workspaces {
            let document = try workspace.document()
            guard workspace.connectLater, workspace.envelopeDigest.count == 32, workspace.bindings.count <= 256,
                  workspace.inactiveSettings.count <= 1024, workspace.findings.count <= 1024,
                  namespaces.insert([document.exporterID, document.id]).inserted else { throw WorkspaceStoreError.corruptStore }
            let refs = Dictionary(uniqueKeysWithValues: document.credentials.map { ($0.id, $0.fingerprint) })
            guard Set(workspace.bindings.map(\.credentialID)).count == workspace.bindings.count else { throw WorkspaceStoreError.corruptStore }
            for binding in workspace.bindings {
                guard refs[binding.credentialID] == binding.fingerprint, Self.validAccount(binding.account) else {
                    throw WorkspaceStoreError.corruptStore
                }
            }
            if catalog.version == 1 {
                catalog.acceptedImports.append(AcceptedImport(exporterID: document.exporterID, workspaceID: document.id,
                    bundleID: document.bundleID, transferID: nil, envelopeDigest: workspace.envelopeDigest,
                    firstAcceptedRevision: workspace.revision))
            }
        }
        catalog.version = 2
        var bundles = Set<[String]>(), transfers = Set<Data>(), digests = Set<Data>()
        for accepted in catalog.acceptedImports {
            guard (try? WorkspaceDocument.identifier(accepted.exporterID)) != nil,
                  (try? WorkspaceDocument.identifier(accepted.workspaceID)) != nil,
                  (try? WorkspaceDocument.identifier(accepted.bundleID)) != nil,
                  accepted.envelopeDigest.count == 32,
                  bundles.insert([accepted.exporterID, accepted.bundleID]).inserted,
                  digests.insert(accepted.envelopeDigest).inserted else { throw WorkspaceStoreError.corruptStore }
            if let transfer = accepted.transferID {
                guard transfer.count == 16, transfers.insert(transfer).inserted else { throw WorkspaceStoreError.corruptStore }
            }
        }
        for workspace in catalog.workspaces {
            let document = try workspace.document()
            guard let index = acceptanceIndex(document, catalog: catalog),
                  catalog.acceptedImports[index].workspaceID == document.id,
                  catalog.acceptedImports[index].envelopeDigest == workspace.envelopeDigest else { throw WorkspaceStoreError.corruptStore }
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
