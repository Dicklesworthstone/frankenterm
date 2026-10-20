import Foundation

public enum OfflineRestorePhase: String, Sendable {
    case idle, candidate, collecting, locked, documentReady, preparingReview, review, committing, complete, recoveryRequired
}

public enum OfflineRestoreSource: String, Sendable { case optical, encryptedFile, workspaceDocument }

/// Public preview deliberately carries no private containers or transfer code.
public struct OfflineRestoreReview: Sendable {
    public let token: UUID
    public let document: WorkspaceDocument
    public let includedFingerprints: [String]
    public let findings: [InstalledWorkspace.PortabilityFinding]
    public let inactiveSettingCount: Int
    public let expectedRevision: UUID?
    public let removesMachines: Bool
    public let isRollback: Bool
    public let preservedHostRevocationCount: Int
    public let preservedHostRequirementCount: Int
    public let firstAcceptedRevision: UUID?
    public let routeReadiness: [WorkspaceDocument.RouteReadiness]
    public var requiresRollbackApproval: Bool { isRollback }
}

public struct OfflineRestoreStatus: Sendable {
    public let generation: UUID
    public let phase: OfflineRestorePhase
    public let isSuspended: Bool
    public let source: OfflineRestoreSource?
    public let identity: OpticalFrame.Identity?
    public let comparison: String?
    public let verifiedPages: Int, verifiedBytes: Int, independentParts: Int
    public let incompletePageIndices: [Int]
    public let review: OfflineRestoreReview?
    public let receipt: WorkspaceImportReceipt?
}

/// The application shares one instance across its scanner and document importer.
/// Callers MUST bound observation admission (not launch one Task per video frame).
/// No method can connect, provision hosts, run a command, or activate auto-connect.
public actor OfflineRestore<C: AirGapCryptography> {
    private let crypto: C
    private let store: WorkspaceStore
    private var generation = UUID()
    private var phase = OfflineRestorePhase.idle
    private var suspended = false
    private var candidate: OpticalFrame?
    private var receiver: OpticalReceiver?
    private var identity: OpticalFrame.Identity?
    private var envelope: Data?
    private var source: OfflineRestoreSource?
    private var documentData: Data?
    private var documentDigest: Data?
    private var archive: AirGapArchive?
    private var review: OfflineRestoreReview?
    private var receipt: WorkspaceImportReceipt?
    private var pendingNamespace: (exporter: String, workspace: String)?

    public init(using crypto: C, store: WorkspaceStore) { self.crypto = crypto; self.store = store }

    private var importDigest: Data? { identity?.envelopeDigest ?? documentDigest }
    private var reviewFallback: OfflineRestorePhase { source == .workspaceDocument ? .documentReady : .locked }

    public func status() -> OfflineRestoreStatus {
        let comparison = importDigest.map { digest in
            let chars = Array(digest.ftHex.prefix(20))
            return stride(from: 0, to: chars.count, by: 4).map { String(chars[$0..<$0 + 4]) }.joined(separator: "-")
        }
        return .init(generation: generation, phase: phase, isSuspended: suspended, source: source, identity: identity,
                     comparison: comparison, verifiedPages: receiver?.verifiedPages ?? (envelope == nil ? 0 : identity?.pageCount ?? 0),
                     verifiedBytes: receiver?.verifiedBytes ?? envelope?.count ?? 0,
                     independentParts: receiver?.independentParts ?? 0,
                     incompletePageIndices: receiver?.incompletePageIndices ?? [],
                     review: suspended ? nil : review, receipt: suspended ? nil : receipt)
    }

    /// First observation only offers a candidate. It does not allocate a fountain
    /// decoder, accept a source identity, or trust its self-asserted exporter ID.
    @discardableResult
    public func observe(_ text: String, generation token: UUID) throws -> OfflineRestoreStatus {
        try check(token)
        guard [.idle, .candidate, .collecting].contains(phase) else { return status() }
        let frame = try OpticalFrame(text: text)
        if phase == .idle {
            candidate = frame; identity = frame.identity; source = .optical; phase = .candidate
        } else if phase == .candidate {
            guard frame.identity == candidate?.identity else { throw FTError.stale }
        } else {
            guard var active = receiver else { throw FTError.malformed("restore.receiver") }
            do { try active.receive(frame, using: crypto) }
            catch { receiver = active; throw error }
            receiver = active
            if active.isComplete {
                envelope = try active.envelope(using: crypto)
                receiver = nil; candidate = nil; phase = .locked
            }
        }
        return status()
    }

    @discardableResult
    public func selectCandidate(generation token: UUID, digest: Data) throws -> OfflineRestoreStatus {
        try check(token)
        guard phase == .candidate, let candidate, candidate.identity.envelopeDigest == digest else { throw FTError.stale }
        var selected = OpticalReceiver(selecting: candidate)
        try selected.receive(candidate, using: crypto)
        receiver = selected; self.candidate = nil; phase = .collecting
        if selected.isComplete {
            envelope = try selected.envelope(using: crypto); receiver = nil; phase = .locked
        }
        return status()
    }

    /// An explicit file choice replaces no active scan. Reset first to change sources.
    @discardableResult
    public func acceptEncryptedFile(_ data: Data, generation token: UUID) throws -> OfflineRestoreStatus {
        try check(token)
        guard phase == .idle else { throw WorkspaceStoreError.busy }
        let header = try FTAGHeader(envelope: data)
        identity = .init(transferID: header.transferID, envelopeDigest: crypto.sha256(data), envelopeLength: data.count)
        envelope = data; source = .encryptedFile; phase = .locked
        return status()
    }

    /// Ordinary public workspace files use the same review and journal as FTAG.
    /// They have no transfer code and confer no authenticated source identity.
    @discardableResult
    public func acceptWorkspaceDocument(_ data: Data, generation token: UUID) async throws -> OfflineRestoreStatus {
        try check(token)
        guard phase == .idle else { throw WorkspaceStoreError.busy }
        let document = try WorkspaceDocument(data: data)
        let decoded = try AirGapArchive(workspace: document, keys: [], using: crypto)
        documentData = document.raw
        documentDigest = crypto.sha256(Data("FrankenTerm/ftworkspace/v1/document\0".utf8) + data)
        source = .workspaceDocument
        return try await prepareReview(decoded, generation: token)
    }

    @discardableResult
    public func reviewWorkspaceDocument(generation token: UUID) async throws -> OfflineRestoreStatus {
        try check(token)
        guard phase == .documentReady, let documentData else { throw FTError.stale }
        let decoded = try AirGapArchive(workspace: WorkspaceDocument(data: documentData), keys: [], using: crypto)
        return try await prepareReview(decoded, generation: token)
    }

    /// Repair is deliberate and page-local. Verified pages survive, while queued
    /// callbacks from before the reset cannot immediately poison the new decoder.
    @discardableResult
    public func resetIncompletePage(_ page: Int, generation token: UUID) throws -> OfflineRestoreStatus {
        try check(token)
        guard phase == .collecting, var active = receiver,
              active.incompletePageIndices.contains(page) else { throw FTError.stale }
        active.resetIncompletePage(page)
        receiver = active; generation = UUID()
        return status()
    }

    @discardableResult
    public func restartPage(_ page: Int, generation token: UUID) throws -> OfflineRestoreStatus {
        try resetIncompletePage(page, generation: token)
    }

    /// Cryptography and key parsing run on this actor, not on the native UI actor.
    /// A wrong code leaves ciphertext intact for a retry. It never stages a key.
    @discardableResult
    public func unlock(enteredCode: String, generation token: UUID) async throws -> OfflineRestoreStatus {
        try check(token)
        guard phase == .locked, let envelope else { throw FTError.stale }
        let code = try TransferCode(entered: enteredCode, using: crypto)
        let decoded = try FTAG.open(envelope, code: code, using: crypto)
        return try await prepareReview(decoded, generation: token)
    }

    private func prepareReview(_ decoded: AirGapArchive, generation token: UUID) async throws -> OfflineRestoreStatus {
        guard let digest = importDigest else { throw FTError.stale }
        phase = .preparingReview
        do {
            let preview = try await store.previewImport(decoded, envelopeDigest: digest, transferID: identity?.transferID)
            try check(token)
            let prior = preview.assessment
            let refs = Dictionary(uniqueKeysWithValues: decoded.workspace.credentials.map { ($0.id, $0.fingerprint) })
            archive = decoded
            review = .init(token: UUID(), document: decoded.workspace,
                           includedFingerprints: decoded.keys.compactMap { refs[$0.credentialID] },
                           findings: decoded.portability.map { .init(entityID: $0.entityID, setting: $0.setting, disposition: $0.disposition, reason: $0.reason) },
                           inactiveSettingCount: decoded.annotations.count, expectedRevision: prior.existingWorkspace?.revision,
                           removesMachines: prior.removesMachines, isRollback: prior.requiresRollbackApproval,
                           preservedHostRevocationCount: prior.preservedHostRevocationCount,
                           preservedHostRequirementCount: prior.preservedHostRequirementCount,
                           firstAcceptedRevision: prior.firstAcceptedRevision,
                           routeReadiness: preview.routes)
            phase = .review
            return status()
        } catch {
            if generation == token, phase == .preparingReview { phase = reviewFallback }
            throw error
        }
    }

    /// Publication is owned to settlement even when the presenting scene goes away.
    /// The opaque review token binds confirmation to the decrypted candidate and
    /// expected saved revision. No approval can be decoded out of a QR payload.
    @discardableResult
    public func commit(reviewToken: UUID, generation token: UUID, sourceAndContentsConfirmed: Bool,
                       allowMachineRemoval: Bool = false, allowRollback: Bool = false) async throws -> OfflineRestoreStatus {
        try check(token)
        guard sourceAndContentsConfirmed else { throw WorkspaceStoreError.reviewRequired }
        guard phase == .review, let review, review.token == reviewToken, let archive, let digest = importDigest else { throw FTError.stale }
        guard !review.removesMachines || allowMachineRemoval else { throw WorkspaceStoreError.removalNeedsApproval }
        guard !review.isRollback || allowRollback else { throw WorkspaceStoreError.rollbackNeedsApproval }
        phase = .committing
        pendingNamespace = (archive.workspace.exporterID, archive.workspace.id)
        self.archive = nil; self.review = nil
        do {
            let result = try await store.install(archive, envelopeDigest: digest,
                expectedRevision: review.expectedRevision, sourceAndContentsConfirmed: true, allowMachineRemoval: allowMachineRemoval,
                allowRollback: allowRollback, transferID: identity?.transferID)
            receipt = result; phase = .complete
            envelope = nil; documentData = nil; receiver = nil; candidate = nil; pendingNamespace = nil
            return status()
        } catch {
            if let reason = error as? WorkspaceStoreError,
               [.reviewRequired, .staleReview, .removalNeedsApproval, .rollbackNeedsApproval,
                .conflictingExport, .importHistoryFull, .revokedIdentity].contains(reason) {
                pendingNamespace = nil; phase = reviewFallback
                throw error
            }
            // A storage error may follow publication. Require readback before the
            // UI offers another attempt, rather than claiming a failed import.
            phase = .recoveryRequired
            throw error
        }
    }

    @discardableResult
    public func commit(reviewToken: UUID, generation token: UUID, sourceAndContentsConfirmed: Bool,
                       allowMachineRemoval: Bool = false, allowHistoricalRollback: Bool) async throws -> OfflineRestoreStatus {
        try await commit(reviewToken: reviewToken, generation: token, sourceAndContentsConfirmed: sourceAndContentsConfirmed,
                         allowMachineRemoval: allowMachineRemoval, allowRollback: allowHistoricalRollback)
    }

    @discardableResult
    public func reconcile(generation token: UUID) async throws -> OfflineRestoreStatus {
        try check(token)
        guard phase == .recoveryRequired, let digest = importDigest, let pendingNamespace else { throw FTError.stale }
        // Reserve the transition across the store await, fencing concurrent resets.
        phase = .committing
        do {
            if let recovered = try await store.recoverImport(exporterID: pendingNamespace.exporter,
                workspaceID: pendingNamespace.workspace, envelopeDigest: digest, transferID: identity?.transferID) {
                receipt = recovered
                phase = .complete; envelope = nil; documentData = nil; self.pendingNamespace = nil
            } else { phase = reviewFallback; self.pendingNamespace = nil }
            return status()
        } catch { phase = .recoveryRequired; throw error }
    }

    /// Keeps ciphertext progress, but clears decrypted review and retires every
    /// pre-suspension observation token. A commit already owned by storage settles.
    @discardableResult
    public func suspend() -> OfflineRestoreStatus {
        suspended = true; generation = UUID(); archive = nil; review = nil
        if [.review, .preparingReview].contains(phase) { phase = reviewFallback }
        return status()
    }
    @discardableResult
    public func resume() -> OfflineRestoreStatus {
        guard suspended else { return status() }
        suspended = false; generation = UUID()
        return status()
    }
    @discardableResult
    public func reset() throws -> OfflineRestoreStatus {
        guard phase != .committing, phase != .recoveryRequired else { throw WorkspaceStoreError.recoveryRequired }
        generation = UUID(); phase = .idle; suspended = false
        candidate = nil; receiver = nil; identity = nil; envelope = nil; archive = nil; review = nil; receipt = nil; pendingNamespace = nil
        source = nil; documentData = nil; documentDigest = nil
        return status()
    }
    private func check(_ token: UUID) throws {
        try Task.checkCancellation()
        guard !suspended, generation == token else { throw FTError.stale }
    }
}
