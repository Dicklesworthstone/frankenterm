import Foundation

public enum OfflineRestorePhase: String, Sendable {
    case idle, candidate, collecting, locked, documentReady, preparingReview, review, committing, complete, recoveryRequired
}

public enum OfflineRestoreSource: String, Sendable { case optical, encryptedFile, workspaceDocument }

/// Both public-file export and import compare the exact saved bytes. This is a
/// domain-separated checksum, not authentication of a self-asserted exporter.
public enum PublicWorkspaceIdentity {
    public static func digest<C: AirGapCryptography>(data: Data, using crypto: C) throws -> Data {
        guard !data.isEmpty, data.count <= FTLimit.manifest else { throw FTError.limit("workspace.identity_bytes") }
        return crypto.sha256(Data("FrankenTerm/ftworkspace/v1/document\0".utf8) + data)
    }
    public static func comparison<C: AirGapCryptography>(data: Data, using crypto: C) throws -> String {
        comparison(digest: try digest(data: data, using: crypto))
    }
    fileprivate static func comparison(digest: Data) -> String {
        let chars = Array(digest.ftHex.prefix(20))
        return stride(from: 0, to: chars.count, by: 4).map { String(chars[$0..<min($0 + 4, chars.count)]) }.joined(separator: "-")
    }
}

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
    public let existingWorkspaceName: String?
    public let changeReview: WorkspaceChangeReview?
    public let requiresSourceAssociation: Bool
    public var requiresSecurityApproval: Bool { changeReview?.requiresSecurityApproval == true }
    public var requiresRollbackApproval: Bool { isRollback }
}

public struct OfflineRestoreStatus: Sendable {
    public let generation: UUID
    public let phase: OfflineRestorePhase
    public let isSuspended: Bool
    public let source: OfflineRestoreSource?
    public let identity: OpticalFrame.Identity?
    public let comparison: String?
    public let encryptionMode: FTAGMode?
    /// Public request only. The private recipient key never leaves this actor.
    public let recipientRequest: RecipientRequest?
    public let requiresRecipientResume: Bool
    public let canUnlockRecipient: Bool
    public let verifiedPages: Int, verifiedBytes: Int, independentParts: Int
    /// Parts in hand out of those the transfer needs: the progress to show
    /// while scanning (see OpticalReceiver.receivedParts).
    public let receivedParts: Int, requiredParts: Int
    public let incompletePageIndices: [Int]
    public let review: OfflineRestoreReview?
    public let receipt: WorkspaceImportReceipt?
    /// After a scanned or encrypted transfer commits: what tells its sender
    /// it arrived (see TransferCompletion).
    public let arrival: TransferArrival?
}

/// The application shares one instance across its scanner and document importer.
/// Callers MUST bound observation admission (not launch one Task per video frame).
/// No method can connect, provision hosts, run a command, or activate auto-connect.
public actor OfflineRestore<C: AirGapCryptography> {
    private let crypto: C
    private let store: WorkspaceStore
    private let networkActivity: WorkspaceNetworkActivity
    private var offlineLease: WorkspaceNetworkLease?
    private var generation = UUID()
    private var phase = OfflineRestorePhase.idle
    private var suspended = false
    private var candidate: OpticalFrame?
    private var receiver: OpticalReceiver?
    private var identity: OpticalFrame.Identity?
    private var envelope: Data?
    private var encryptionMode: FTAGMode?
    private var recipientSession: RecipientSession?
    private var source: OfflineRestoreSource?
    private var documentData: Data?
    private var documentDigest: Data?
    private var archive: AirGapArchive?
    private var review: OfflineRestoreReview?
    private var receipt: WorkspaceImportReceipt?
    private var arrival: TransferArrival?
    private var pendingNamespace: (exporter: String, workspace: String)?

    public init(using crypto: C, store: WorkspaceStore,
                networkActivity: WorkspaceNetworkActivity = .shared) {
        self.crypto = crypto; self.store = store; self.networkActivity = networkActivity
    }

    /// Call before presenting a scanner or reading the selected local file.
    /// Existing connections must be closed deliberately by their owners; entering
    /// restore never disconnects a shell on the user's behalf. A completed
    /// restore has nothing left to read and gives up exclusion (so the device
    /// may tell the sender, or connect) until a reset and this call again.
    public func enterOfflineMode() throws {
        try Task.checkCancellation()
        if offlineLease == nil, phase != .complete { offlineLease = try networkActivity.beginOfflineRestore() }
    }

    private var importDigest: Data? { identity?.envelopeDigest ?? documentDigest }
    private var reviewFallback: OfflineRestorePhase { source == .workspaceDocument ? .documentReady : .locked }

    public func status() -> OfflineRestoreStatus {
        let comparison = importDigest.map { PublicWorkspaceIdentity.comparison(digest: $0) }
        return .init(generation: generation, phase: phase, isSuspended: suspended, source: source, identity: identity,
                     comparison: comparison, encryptionMode: encryptionMode, recipientRequest: recipientSession?.request,
                     requiresRecipientResume: suspended && recipientSession != nil,
                     canUnlockRecipient: !suspended && matchingRecipient(),
                     verifiedPages: receiver?.verifiedPages ?? (envelope == nil ? 0 : identity?.pageCount ?? 0),
                     verifiedBytes: receiver?.verifiedBytes ?? envelope?.count ?? 0,
                     independentParts: receiver?.independentParts ?? 0,
                     receivedParts: receiver?.receivedParts ?? (envelope == nil ? 0 : identity?.sourcePartCount ?? 0),
                     requiredParts: identity?.sourcePartCount ?? 0,
                     incompletePageIndices: receiver?.incompletePageIndices ?? [],
                     review: suspended ? nil : review, receipt: suspended ? nil : receipt,
                     arrival: phase == .complete ? arrival : nil)
    }

    private func matchingRecipient() -> Bool {
        guard let envelope, let recipientSession, let header = try? FTAGHeader(envelope: envelope),
              header.mode == .recipient else { return false }
        return header.modeHeader.prefix(16) == recipientSession.request.requestID
            && header.modeHeader.dropFirst(16).prefix(32) == recipientSession.request.digest(using: crypto)
    }

    /// At most one ephemeral request exists. A replacement requires an explicit
    /// reset, so a queued frame or view recreation cannot change the destination.
    @discardableResult
    public func createRecipientRequest(generation token: UUID,
        maximumEnvelopeBytes: Int = FTLimit.envelope) throws -> OfflineRestoreStatus where C: RecipientCryptography {
        try check(token)
        guard phase == .idle, recipientSession == nil else { throw WorkspaceStoreError.busy }
        var session = try RecipientSession(maximumEnvelopeBytes: maximumEnvelopeBytes, using: crypto)
        defer { session.retire() }
        try Task.checkCancellation()
        recipientSession = session
        generation = UUID()
        return status()
    }

    /// The key stays in process memory across a temporary lock/background event.
    /// Automatic foreground resume cannot reactivate it: this deliberate action
    /// retires old callbacks and permits scanning/decryption again. Process death
    /// loses the key, and reset/cancel drops this actor's owned session immediately.
    @discardableResult
    public func resumeRecipientRequest(generation token: UUID) throws -> OfflineRestoreStatus {
        try Task.checkCancellation()
        guard suspended, generation == token, recipientSession != nil else { throw FTError.stale }
        try enterOfflineMode()
        suspended = false; generation = UUID()
        return status()
    }

    /// First observation only offers a candidate. It does not allocate a fountain
    /// decoder, accept a source identity, or trust its self-asserted exporter ID.
    @discardableResult
    public func observe(_ text: String, generation token: UUID) throws -> OfflineRestoreStatus {
        try check(token)
        guard [.idle, .candidate, .collecting].contains(phase) else { return status() }
        let frame = try OpticalFrame(text: text)
        if let request = recipientSession?.request, frame.identity.envelopeLength > request.maximumEnvelopeBytes {
            throw FTError.limit("recipient.envelope_limit")
        }
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
                try finishOpticalEnvelope(from: active)
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
            try finishOpticalEnvelope(from: selected)
        }
        return status()
    }

    /// An explicit file choice replaces no active scan. Reset first to change sources.
    @discardableResult
    public func acceptEncryptedFile(_ data: Data, generation token: UUID) throws -> OfflineRestoreStatus {
        try check(token)
        guard phase == .idle else { throw WorkspaceStoreError.busy }
        let header = try FTAGHeader(envelope: data)
        try checkRecipientDestination(header, envelopeLength: data.count)
        identity = .init(transferID: header.transferID, envelopeDigest: crypto.sha256(data), envelopeLength: data.count)
        envelope = data; encryptionMode = header.mode; source = .encryptedFile; phase = .locked
        return status()
    }

    /// Ordinary public workspace files use the same review and journal as FTAG.
    /// They have no transfer code and confer no authenticated source identity.
    @discardableResult
    public func acceptWorkspaceDocument(_ data: Data, generation token: UUID) async throws -> OfflineRestoreStatus {
        try check(token)
        guard phase == .idle, recipientSession == nil else { throw WorkspaceStoreError.busy }
        let document = try WorkspaceDocument(data: data)
        let decoded = try AirGapArchive(workspace: document, keys: [], using: crypto)
        documentData = document.raw
        documentDigest = try PublicWorkspaceIdentity.digest(data: data, using: crypto)
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
        guard encryptionMode == .code, recipientSession == nil else { throw FTError.unsupported("envelope.requires_recipient_key") }
        let code = try TransferCode(entered: enteredCode, using: crypto)
        let decoded = try FTAG.open(envelope, code: code, using: crypto)
        return try await prepareReview(decoded, generation: token)
    }

    @discardableResult
    public func unlockRecipient(generation token: UUID) async throws -> OfflineRestoreStatus where C: RecipientCryptography {
        try check(token)
        guard phase == .locked, let envelope else { throw FTError.stale }
        let decoded = try decodeRecipientEnvelope(envelope)
        return try await prepareReview(decoded, generation: token)
    }

    /// A storage preview may suspend indefinitely. Keep its async frame free of
    /// a Session copy, so cancellation can retire the actor's owned key without
    /// another copy surviving in that suspended frame.
    private func decodeRecipientEnvelope(_ envelope: Data) throws -> AirGapArchive where C: RecipientCryptography {
        guard var session = recipientSession else { throw FTError.stale }
        defer { session.retire() }
        return try FTAG.open(envelope, recipient: session, using: crypto)
    }

    private func prepareReview(_ decoded: AirGapArchive, generation token: UUID) async throws -> OfflineRestoreStatus {
        let lease = try check(token)
        defer { withExtendedLifetime(lease) {} }
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
                           routeReadiness: preview.routes,
                           existingWorkspaceName: try prior.existingWorkspace?.document().name,
                           changeReview: prior.changeReview, requiresSourceAssociation: prior.requiresSourceAssociation)
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
                       allowMachineRemoval: Bool = false, allowRollback: Bool = false,
                       associateWithExistingWorkspace: Bool = false,
                       securityChangesConfirmed: Bool = false) async throws -> OfflineRestoreStatus {
        let lease = try check(token)
        defer { withExtendedLifetime(lease) {} }
        guard sourceAndContentsConfirmed else { throw WorkspaceStoreError.reviewRequired }
        guard phase == .review, let review, review.token == reviewToken, let archive, let digest = importDigest else { throw FTError.stale }
        guard !review.removesMachines || allowMachineRemoval else { throw WorkspaceStoreError.removalNeedsApproval }
        guard !review.isRollback || allowRollback else { throw WorkspaceStoreError.rollbackNeedsApproval }
        guard !review.requiresSourceAssociation || associateWithExistingWorkspace else {
            throw WorkspaceStoreError.sourceReplacementNeedsApproval
        }
        guard !review.requiresSecurityApproval || securityChangesConfirmed else {
            throw WorkspaceStoreError.securityChangesNeedApproval
        }
        phase = .committing
        pendingNamespace = (archive.workspace.exporterID, archive.workspace.id)
        self.archive = nil; self.review = nil
        do {
            let result = try await store.install(archive, envelopeDigest: digest,
                expectedRevision: review.expectedRevision, sourceAndContentsConfirmed: true, allowMachineRemoval: allowMachineRemoval,
                allowRollback: allowRollback, transferID: identity?.transferID,
                securityChangesConfirmed: securityChangesConfirmed,
                associateWithExistingWorkspace: associateWithExistingWorkspace)
            receipt = result; phase = .complete; offlineLease = nil
            if let identity {
                arrival = .init(envelopeDigest: identity.envelopeDigest, workspaceDigest: crypto.sha256(archive.workspace.raw))
            }
            envelope = nil; documentData = nil; receiver = nil; candidate = nil; pendingNamespace = nil
            retireRecipientSession()
            return status()
        } catch {
            if let reason = error as? WorkspaceStoreError,
               [.reviewRequired, .staleReview, .removalNeedsApproval, .rollbackNeedsApproval,
                .sourceReplacementNeedsApproval, .securityChangesNeedApproval,
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
                       allowMachineRemoval: Bool = false, allowHistoricalRollback: Bool,
                       associateWithExistingWorkspace: Bool = false,
                       securityChangesConfirmed: Bool = false) async throws -> OfflineRestoreStatus {
        try await commit(reviewToken: reviewToken, generation: token, sourceAndContentsConfirmed: sourceAndContentsConfirmed,
                         allowMachineRemoval: allowMachineRemoval, allowRollback: allowHistoricalRollback,
                         associateWithExistingWorkspace: associateWithExistingWorkspace,
                         securityChangesConfirmed: securityChangesConfirmed)
    }

    @discardableResult
    public func reconcile(generation token: UUID) async throws -> OfflineRestoreStatus {
        let lease = try check(token)
        defer { withExtendedLifetime(lease) {} }
        guard phase == .recoveryRequired, let digest = importDigest, let pendingNamespace else { throw FTError.stale }
        // Reserve the transition across the store await, fencing concurrent resets.
        phase = .committing
        do {
            if let recovered = try await store.recoverImport(exporterID: pendingNamespace.exporter,
                workspaceID: pendingNamespace.workspace, envelopeDigest: digest, transferID: identity?.transferID) {
                receipt = recovered
                phase = .complete; offlineLease = nil; envelope = nil; documentData = nil; self.pendingNamespace = nil
                retireRecipientSession()
            } else { phase = reviewFallback; self.pendingNamespace = nil }
            return status()
        } catch { phase = .recoveryRequired; throw error }
    }

    /// Keeps ciphertext progress, but clears decrypted review and retires every
    /// pre-suspension observation token. A commit already owned by storage settles.
    /// Backgrounding retains offline exclusion. Explicit dismissal releases this
    /// actor's lease; awaited storage work retains its own reference to settlement.
    @discardableResult
    public func suspend(discardRecipientRequest: Bool = false) -> OfflineRestoreStatus {
        suspended = true; generation = UUID(); archive = nil; review = nil
        if discardRecipientRequest { retireRecipientSession(); offlineLease = nil }
        if [.review, .preparingReview].contains(phase) { phase = reviewFallback }
        return status()
    }
    @discardableResult
    public func resume() -> OfflineRestoreStatus {
        guard suspended else { return status() }
        guard recipientSession == nil else { return status() }
        suspended = false; generation = UUID()
        return status()
    }
    @discardableResult
    public func reset() throws -> OfflineRestoreStatus {
        guard phase != .committing, phase != .recoveryRequired else { throw WorkspaceStoreError.recoveryRequired }
        generation = UUID(); phase = .idle; suspended = false
        candidate = nil; receiver = nil; identity = nil; envelope = nil; archive = nil; review = nil; receipt = nil; pendingNamespace = nil
        source = nil; documentData = nil; documentDigest = nil; encryptionMode = nil; arrival = nil
        retireRecipientSession()
        return status()
    }

    private func finishOpticalEnvelope(from completedReceiver: OpticalReceiver) throws {
        do {
            // Reconstruction also validates the outer digest and FTAG header.
            // Keep those checks inside the rejected-source cleanup boundary.
            let data = try completedReceiver.envelope(using: crypto)
            let header = try FTAGHeader(envelope: data)
            try checkRecipientDestination(header, envelopeLength: data.count)
            envelope = data; encryptionMode = header.mode
            receiver = nil; candidate = nil; phase = .locked
        } catch {
            // An invalid source, destination, or mode must not consume the
            // active phone request. Fence its queued callbacks before the phone
            // scans another encrypted response.
            candidate = nil; receiver = nil; identity = nil; source = nil
            envelope = nil; encryptionMode = nil; phase = .idle; generation = UUID()
            throw error
        }
    }

    private func checkRecipientDestination(_ header: FTAGHeader, envelopeLength: Int) throws {
        guard let request = recipientSession?.request else { return }
        guard header.mode == .recipient else { throw FTError.unsupported("envelope.requires_recipient_key") }
        guard envelopeLength <= request.maximumEnvelopeBytes else { throw FTError.limit("recipient.envelope_limit") }
        guard header.modeHeader.prefix(16) == request.requestID,
              header.modeHeader.dropFirst(16).prefix(32) == request.digest(using: crypto) else {
            throw FTError.authentication
        }
    }

    private func retireRecipientSession() {
        recipientSession?.retire()
        recipientSession = nil
    }
    @discardableResult
    private func check(_ token: UUID) throws -> WorkspaceNetworkLease {
        try Task.checkCancellation()
        guard !suspended, generation == token else { throw FTError.stale }
        try enterOfflineMode()
        guard let offlineLease else { throw FTError.stale }
        return offlineLease
    }
}
