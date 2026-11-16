import Foundation

public enum OpticalScanRecoveryError: Error, Equatable, Sendable, LocalizedError {
    case consentRequired, wrongNamespace, differentTransfer, conflictingPage, staleReview, stalePermit, corruptStore
    public var errorDescription: String? {
        switch self {
        case .consentRequired: "Saving encrypted scan progress requires your approval."
        case .wrongNamespace: "Scan recovery needs its own protected storage directory."
        case .differentTransfer: "Another transfer has saved progress. Review and discard it before saving this transfer."
        case .conflictingPage: "Saved and received pages disagree. Neither copy was replaced."
        case .staleReview: "Saved scan progress changed. Review it again before resuming or discarding it."
        case .stalePermit: "Saving permission for this scan has ended. Approve saving again."
        case .corruptStore: "Saved scan progress could not be validated. It was not imported or replaced."
        }
    }
}

/// Opaque consent for one save epoch and exact encrypted transfer. Discarding
/// saved progress or granting a new epoch invalidates earlier writers, including
/// those belonging to another recovery actor. It grants no import authority.
public struct OpticalScanSavePermit: Sendable {
    fileprivate let epoch: UUID
    fileprivate let identity: OpticalFrame.Identity
}

/// A precise read-only snapshot. No code, private identity, or decoded setting
/// enters this record. The private witness prevents an old review from clearing
/// or loading a newer saved version, even if its envelope identity is unchanged.
public struct OpticalScanRecoveryReview: Identifiable, Sendable {
    public let id: UUID
    public let identity: OpticalFrame.Identity
    public let verifiedPages: Int, verifiedBytes: Int
    public let incompletePageIndices: [Int]
    fileprivate let witness: Data
}

/// One opt-in encrypted scan cache, separate from workspace and import storage.
/// All filesystem operations stay on this actor and within the existing atomic,
/// synchronized persistence lease. No operation starts a camera or a connection.
/// The directory initializer uses complete iOS file protection and exclusion
/// from backup through FileWorkspacePersistence. Injected persistence must supply
/// the same contract. Neither the filename nor a directory is taken from a QR.
public actor OpticalScanRecovery<C: AirGapCryptography> {
    private let persistence: any WorkspacePersistence
    private let crypto: C
    private static var namespace: Data { Data("FrankenTerm/OpticalScanRecovery/v1\n".utf8) }
    private static var domain: Data { Data("FrankenTerm/FTSR/v1/recovery\0".utf8) }
    private static var maximumBytes: Int { OpticalScanCheckpoint.maximumBytes + 60 }

    public init(directory: URL, using crypto: C) throws {
        persistence = try FileWorkspacePersistence(directory: directory)
        self.crypto = crypto
    }
    public init(persistence: any WorkspacePersistence, using crypto: C) {
        self.persistence = persistence; self.crypto = crypto
    }

    private struct Record {
        let epoch: UUID
        let checkpoint: OpticalScanCheckpoint?
        let witness: Data
    }

    /// This lookup is read-only, including the never-used and discarded cases.
    public func review() throws -> OpticalScanRecoveryReview? {
        try Task.checkCancellation()
        return try persistence.withExclusiveAccess { try read().flatMap(makeReview) }
    }

    /// Explicit opt-in starts a new writer epoch. Existing pages of this same
    /// transfer are merged, never regressed. Another transfer cannot be replaced
    /// by this operation, even with consent; discard its exact review first.
    public func beginSaving(_ checkpoint: OpticalScanCheckpoint,
                            confirmedByUser: Bool) throws -> OpticalScanSavePermit {
        guard confirmedByUser else { throw OpticalScanRecoveryError.consentRequired }
        try Task.checkCancellation()
        return try persistence.withExclusiveAccess {
            let previous = try read()
            let merged = try merge(previous?.checkpoint, checkpoint)
            let epoch = UUID()
            // Reaffirm the marker too. An earlier write can have published it
            // before reporting an fsync error. Success means durability, not just
            // that the bytes happened to be visible on readback.
            try persistence.write(Self.namespace, to: .journal)
            try write(epoch: epoch, checkpoint: merged)
            return OpticalScanSavePermit(epoch: epoch, identity: merged.identity)
        }
    }

    /// At most a page completion should call this, never every camera frame.
    /// A write error may have published. Retrying with the SAME permit reads
    /// and merges actual state and reaffirms durability; it never loses pages.
    @discardableResult
    public func save(_ checkpoint: OpticalScanCheckpoint,
                     permit: OpticalScanSavePermit) throws -> OpticalScanRecoveryReview {
        try Task.checkCancellation()
        return try persistence.withExclusiveAccess {
            guard let previous = try read(), previous.epoch == permit.epoch,
                  let stored = previous.checkpoint, stored.identity == permit.identity,
                  checkpoint.identity == permit.identity else { throw OpticalScanRecoveryError.stalePermit }
            let merged = try merge(stored, checkpoint)
            let written = try write(epoch: previous.epoch, checkpoint: merged)
            guard let review = makeReview(written) else { throw OpticalScanRecoveryError.corruptStore }
            return review
        }
    }

    /// Loading does not adopt a source or decrypt it. OfflineRestore must check
    /// its own generation, offline exclusion and recipient-key requirements.
    public func load(_ reviewed: OpticalScanRecoveryReview) throws -> OpticalScanCheckpoint {
        try Task.checkCancellation()
        return try persistence.withExclusiveAccess {
            guard let current = try read(), current.epoch == reviewed.id,
                  current.witness == reviewed.witness, let checkpoint = current.checkpoint,
                  checkpoint.identity == reviewed.identity else { throw OpticalScanRecoveryError.staleReview }
            return checkpoint
        }
    }

    /// Replace the reviewed cache with a small tombstone, rather than removing
    /// the namespace and permitting a delayed writer to recreate the old scan.
    /// This releases ciphertext from the logical store; it is not secure erase.
    public func discard(_ reviewed: OpticalScanRecoveryReview) throws {
        try Task.checkCancellation()
        try persistence.withExclusiveAccess {
            guard let current = try read(), current.epoch == reviewed.id,
                  current.witness == reviewed.witness, current.checkpoint != nil else {
                throw OpticalScanRecoveryError.staleReview
            }
            _ = try write(epoch: UUID(), checkpoint: nil)
        }
    }

    private func makeReview(_ record: Record) -> OpticalScanRecoveryReview? {
        guard let checkpoint = record.checkpoint else { return nil }
        return .init(id: record.epoch, identity: checkpoint.identity,
                     verifiedPages: checkpoint.verifiedPages, verifiedBytes: checkpoint.verifiedBytes,
                     incompletePageIndices: checkpoint.incompletePageIndices, witness: record.witness)
    }

    private func merge(_ stored: OpticalScanCheckpoint?, _ incoming: OpticalScanCheckpoint) throws -> OpticalScanCheckpoint {
        guard let stored else { return incoming }
        guard stored.identity == incoming.identity else { throw OpticalScanRecoveryError.differentTransfer }
        var pages = stored.pages, digests = stored.pageDigests
        for (index, page) in incoming.pages {
            if let old = pages[index], old != page || digests[index] != incoming.pageDigests[index] {
                throw OpticalScanRecoveryError.conflictingPage
            }
            pages[index] = page; digests[index] = incoming.pageDigests[index]
        }
        return try OpticalScanCheckpoint(identity: stored.identity, pages: pages, pageDigests: digests, using: crypto)
    }

    private func read() throws -> Record? {
        let marker = try persistence.read(.journal, limit: Self.namespace.count + 1)
        let bytes = try persistence.read(.catalog, limit: Self.maximumBytes)
        if marker == nil {
            guard bytes == nil else { throw OpticalScanRecoveryError.wrongNamespace }
            return nil
        }
        guard marker == Self.namespace else { throw OpticalScanRecoveryError.wrongNamespace }
        guard let bytes else { return nil } // interrupted first publication
        guard bytes.count >= 60, bytes.count <= Self.maximumBytes else { throw OpticalScanRecoveryError.corruptStore }
        let body = Data(bytes.dropLast(32))
        guard crypto.sha256(Self.domain + body) == Data(bytes.suffix(32)) else {
            throw OpticalScanRecoveryError.corruptStore
        }
        var cursor = ByteCursor(body)
        guard try cursor.take(4) == Data("FTSR".utf8), try cursor.uint(2) == 1 else {
            throw OpticalScanRecoveryError.corruptStore
        }
        let state = try cursor.uint(2)
        let id = [UInt8](try cursor.take(16))
        let epoch = UUID(uuid: (id[0],id[1],id[2],id[3],id[4],id[5],id[6],id[7],
                               id[8],id[9],id[10],id[11],id[12],id[13],id[14],id[15]))
        let length = try cursor.uint(4)
        guard state <= 1, length <= OpticalScanCheckpoint.maximumBytes,
              length == cursor.remaining, (state == 0) == (length == 0) else {
            throw OpticalScanRecoveryError.corruptStore
        }
        let checkpoint = state == 0 ? nil : try OpticalScanCheckpoint(data: cursor.take(Int(length)), using: crypto)
        return Record(epoch: epoch, checkpoint: checkpoint, witness: crypto.sha256(bytes))
    }

    @discardableResult
    private func write(epoch: UUID, checkpoint: OpticalScanCheckpoint?) throws -> Record {
        let payload = try checkpoint?.encoded(using: crypto) ?? Data()
        var bytes = Data("FTSR".utf8)
        bytes.appendBE(1, bytes: 2); bytes.appendBE(checkpoint == nil ? 0 : 1, bytes: 2)
        let id = epoch.uuid
        bytes.append(contentsOf: [id.0,id.1,id.2,id.3,id.4,id.5,id.6,id.7,
                                  id.8,id.9,id.10,id.11,id.12,id.13,id.14,id.15])
        bytes.appendBE(UInt64(payload.count), bytes: 4); bytes += payload
        let checksum = crypto.sha256(Self.domain + bytes)
        guard checksum.count == 32 else { throw OpticalScanRecoveryError.corruptStore }
        bytes += checksum
        guard bytes.count <= Self.maximumBytes else { throw FTError.limit("scan.recovery_bytes") }
        try persistence.write(bytes, to: .catalog)
        return Record(epoch: epoch, checkpoint: checkpoint, witness: crypto.sha256(bytes))
    }
}
