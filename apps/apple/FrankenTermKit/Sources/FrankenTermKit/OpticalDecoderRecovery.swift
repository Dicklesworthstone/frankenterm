import Foundation

public enum OpticalDecoderRecoveryError: Error, Equatable, Sendable, LocalizedError {
    case consentRequired, savedProgressExists, staleReview, staleWriter, differentTransfer, corruptStore, wrongNamespace
    public var errorDescription: String? {
        switch self {
        case .consentRequired: "Saving partial scan progress requires your approval."
        case .savedProgressExists: "Review the saved scan before continuing or discarding it."
        case .staleReview: "Saved progress changed. Review it again before loading or discarding it."
        case .staleWriter: "This save belongs to an earlier scan or saving decision."
        case .differentTransfer: "This progress belongs to another encrypted transfer."
        case .corruptStore: "Saved partial progress could not be validated. It was not replaced."
        case .wrongNamespace: "Partial scan recovery needs its own protected storage directory."
        }
    }
}

/// Metadata for a precise stored ciphertext snapshot, not decryption or import
/// authority. Unfinished equations are counted separately from verified pages.
public struct OpticalDecoderRecoveryReview: Identifiable, Sendable {
    public let id: UUID
    public let identity: OpticalFrame.Identity
    public let verifiedPages: Int, verifiedBytes: Int, independentParts: Int, receivedParts: Int
    public let incompletePageIndices: [Int]
    fileprivate let witness: Data
}

/// Single-writer consent plus the exact predecessor expected by the next save.
/// Each successful save returns a new session. Never serialize this capability.
public struct OpticalDecoderSaveSession: Sendable {
    public let review: OpticalDecoderRecoveryReview
    fileprivate let epoch: UUID
}

/// Opt-in persistence for FTDC's incomplete reconstruction state. Use a dedicated
/// directory, distinct from both WorkspaceStore and verified-only FTSC recovery.
/// The existing file adapter supplies atomic publication, file/directory fsync,
/// restrictive permissions, iOS complete protection and backup exclusion.
///
/// Writes compare the exact predecessor under the persistence lease. They do not
/// merge incomplete equations: a deliberate page repair must be able to discard
/// suspect equations without a delayed save restoring them. No camera frame,
/// unlock code, recipient secret, decrypted archive or approval is stored here.
public actor OpticalDecoderRecovery<C: AirGapCryptography> {
    public static var maximumBytes: Int { OpticalReceiver.decoderCheckpointByteLimit + 92 }
    private static var namespace: Data { Data("FrankenTerm/OpticalDecoderRecovery/v1\n".utf8) }
    private static var domain: Data { Data("FrankenTerm/FTDR/v1/recovery\0".utf8) }
    private let persistence: any WorkspacePersistence
    private let crypto: C

    public init(directory: URL, using crypto: C) throws {
        persistence = try FileWorkspacePersistence(directory: directory)
        self.crypto = crypto
    }
    public init(persistence: any WorkspacePersistence, using crypto: C) {
        self.persistence = persistence; self.crypto = crypto
    }

    private struct Record {
        let epoch: UUID, parent: Data, payload: Data?
        let receiver: OpticalReceiver?
        let bytes: Data, witness: Data
    }

    /// Read-only, including an unused directory or a discarded scan.
    public func review() throws -> OpticalDecoderRecoveryReview? {
        try Task.checkCancellation()
        return try persistence.withExclusiveAccess { try read().flatMap(makeReview) }
    }

    /// Begin only in an empty/tombstoned store. Existing progress must be reviewed
    /// and resumed or explicitly discarded; a fresh scan never replaces it.
    public func beginSaving(_ checkpoint: Data, confirmedByUser: Bool) throws -> OpticalDecoderSaveSession {
        guard confirmedByUser else { throw OpticalDecoderRecoveryError.consentRequired }
        try Task.checkCancellation()
        let receiver = try OpticalReceiver(decoderCheckpoint: checkpoint, using: crypto)
        return try persistence.withExclusiveAccess {
            let previous = try read()
            guard previous?.payload == nil else { throw OpticalDecoderRecoveryError.savedProgressExists }
            try Task.checkCancellation()
            try persistence.write(Self.namespace, to: .journal)
            let record = try write(epoch: UUID(), parent: previous?.witness ?? Data(count: 32),
                                   payload: checkpoint, receiver: receiver)
            return try session(record)
        }
    }

    /// A reopened process obtains fresh consent for this exact saved snapshot.
    /// Rotate the epoch before issuing permission; old owners cannot write again.
    public func continueSaving(_ reviewed: OpticalDecoderRecoveryReview,
                               confirmedByUser: Bool) throws -> OpticalDecoderSaveSession {
        guard confirmedByUser else { throw OpticalDecoderRecoveryError.consentRequired }
        try Task.checkCancellation()
        return try persistence.withExclusiveAccess {
            let previous = try require(reviewed)
            try persistence.write(Self.namespace, to: .journal)
            return try session(write(epoch: UUID(), parent: previous.witness,
                                     payload: previous.payload, receiver: previous.receiver))
        }
    }

    /// Compare-and-swap the complete snapshot, allowing an intentional decrease
    /// in partial rank after page repair. Success includes durability. A failure
    /// may have published: retry the SAME checkpoint with the SAME predecessor.
    /// Only that exact retry may reaffirm an already-published successor.
    public func save(_ checkpoint: Data, after prior: OpticalDecoderSaveSession) throws -> OpticalDecoderSaveSession {
        try Task.checkCancellation()
        let receiver = try OpticalReceiver(decoderCheckpoint: checkpoint, using: crypto)
        guard receiver.identity == prior.review.identity else { throw OpticalDecoderRecoveryError.differentTransfer }
        return try persistence.withExclusiveAccess {
            guard let current = try read(), current.epoch == prior.epoch,
                  current.receiver?.identity == prior.review.identity else { throw OpticalDecoderRecoveryError.staleWriter }
            if current.witness == prior.review.witness {
                if current.payload == checkpoint {
                    try persistence.write(current.bytes, to: .catalog)
                    return try session(current)
                }
                return try session(write(epoch: current.epoch, parent: current.witness,
                                         payload: checkpoint, receiver: receiver))
            }
            guard current.parent == prior.review.witness, current.payload == checkpoint else {
                throw OpticalDecoderRecoveryError.staleWriter
            }
            // Readback after a failed fsync is not durability. Reaffirm exactly
            // the observed successor before reporting an idempotent success.
            try persistence.write(current.bytes, to: .catalog)
            return try session(current)
        }
    }

    /// Returns ciphertext only. Pass it to OfflineRestore.acceptDecoderCheckpoint
    /// with that restore's current generation; ordinary offline/auth checks apply.
    public func load(_ reviewed: OpticalDecoderRecoveryReview) throws -> Data {
        try Task.checkCancellation()
        return try persistence.withExclusiveAccess {
            guard let payload = try require(reviewed).payload else { throw OpticalDecoderRecoveryError.staleReview }
            return payload
        }
    }

    /// Revoke the entire writer epoch while retaining its newest saved progress.
    /// No deletion and no merge: a queued old save cannot resurrect partial rows.
    public func stopSaving(_ session: OpticalDecoderSaveSession) throws {
        try Task.checkCancellation()
        try persistence.withExclusiveAccess {
            guard let current = try read(), current.epoch == session.epoch, current.payload != nil else {
                throw OpticalDecoderRecoveryError.staleWriter
            }
            _ = try write(epoch: UUID(), parent: current.witness, payload: current.payload, receiver: current.receiver)
        }
    }

    /// A tombstone fences old writers, including in other actor instances. It
    /// releases logical ciphertext retention, not forensic copies or backups.
    public func discard(_ reviewed: OpticalDecoderRecoveryReview, confirmedByUser: Bool) throws {
        guard confirmedByUser else { throw OpticalDecoderRecoveryError.consentRequired }
        try Task.checkCancellation()
        try persistence.withExclusiveAccess {
            guard let current = try read() else { throw OpticalDecoderRecoveryError.staleReview }
            if current.payload == nil, current.parent == reviewed.witness {
                try persistence.write(current.bytes, to: .catalog)
                return
            }
            guard current.epoch == reviewed.id, current.witness == reviewed.witness,
                  current.receiver?.identity == reviewed.identity else { throw OpticalDecoderRecoveryError.staleReview }
            _ = try write(epoch: UUID(), parent: current.witness, payload: nil, receiver: nil)
        }
    }

    private func require(_ reviewed: OpticalDecoderRecoveryReview) throws -> Record {
        guard let current = try read(), current.epoch == reviewed.id, current.witness == reviewed.witness,
              current.receiver?.identity == reviewed.identity else { throw OpticalDecoderRecoveryError.staleReview }
        return current
    }
    private func makeReview(_ record: Record) -> OpticalDecoderRecoveryReview? {
        guard let receiver = record.receiver else { return nil }
        return .init(id: record.epoch, identity: receiver.identity, verifiedPages: receiver.verifiedPages,
                     verifiedBytes: receiver.verifiedBytes, independentParts: receiver.independentParts,
                     receivedParts: receiver.receivedParts, incompletePageIndices: receiver.incompletePageIndices,
                     witness: record.witness)
    }
    private func session(_ record: Record) throws -> OpticalDecoderSaveSession {
        guard let review = makeReview(record) else { throw OpticalDecoderRecoveryError.corruptStore }
        return .init(review: review, epoch: record.epoch)
    }

    private func read() throws -> Record? {
        let marker = try persistence.read(.journal, limit: Self.namespace.count + 1)
        let bytes = try persistence.read(.catalog, limit: Self.maximumBytes)
        guard let marker else {
            guard bytes == nil else { throw OpticalDecoderRecoveryError.wrongNamespace }
            return nil
        }
        guard marker == Self.namespace else { throw OpticalDecoderRecoveryError.wrongNamespace }
        guard let bytes else { return nil } // interrupted first publication
        guard bytes.count >= 92, bytes.count <= Self.maximumBytes else { throw OpticalDecoderRecoveryError.corruptStore }
        let body = Data(bytes.dropLast(32))
        guard crypto.sha256(Self.domain + body) == Data(bytes.suffix(32)) else { throw OpticalDecoderRecoveryError.corruptStore }
        var reader = ByteCursor(body)
        guard try reader.take(4) == Data("FTDR".utf8), try reader.uint(2) == 1 else {
            throw OpticalDecoderRecoveryError.corruptStore
        }
        let state = try reader.uint(2), id = [UInt8](try reader.take(16))
        let parent = try reader.take(32), length = try reader.uint(4)
        guard state <= 1, length <= OpticalReceiver.decoderCheckpointByteLimit,
              length == reader.remaining, (state == 0) == (length == 0) else {
            throw OpticalDecoderRecoveryError.corruptStore
        }
        let payload = state == 0 ? nil : try reader.take(Int(length))
        let receiver = try payload.map { try OpticalReceiver(decoderCheckpoint: $0, using: crypto) }
        let epoch = UUID(uuid: (id[0],id[1],id[2],id[3],id[4],id[5],id[6],id[7],
                               id[8],id[9],id[10],id[11],id[12],id[13],id[14],id[15]))
        return .init(epoch: epoch, parent: parent, payload: payload, receiver: receiver,
                     bytes: bytes, witness: crypto.sha256(bytes))
    }

    private func write(epoch: UUID, parent: Data, payload: Data?, receiver: OpticalReceiver?) throws -> Record {
        guard parent.count == 32, (payload == nil) == (receiver == nil),
              (payload?.count ?? 0) <= OpticalReceiver.decoderCheckpointByteLimit else {
            throw OpticalDecoderRecoveryError.corruptStore
        }
        var bytes = Data("FTDR".utf8)
        bytes.appendBE(1, bytes: 2); bytes.appendBE(payload == nil ? 0 : 1, bytes: 2)
        let id = epoch.uuid
        bytes.append(contentsOf: [id.0,id.1,id.2,id.3,id.4,id.5,id.6,id.7,id.8,id.9,id.10,id.11,id.12,id.13,id.14,id.15])
        bytes += parent; bytes.appendBE(UInt64(payload?.count ?? 0), bytes: 4)
        if let payload { bytes += payload }
        let digest = crypto.sha256(Self.domain + bytes)
        guard digest.count == 32 else { throw OpticalDecoderRecoveryError.corruptStore }
        bytes += digest
        try persistence.write(bytes, to: .catalog)
        return .init(epoch: epoch, parent: parent, payload: payload, receiver: receiver,
                     bytes: bytes, witness: crypto.sha256(bytes))
    }
}
