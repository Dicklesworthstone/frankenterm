import Foundation

/// Reconstructs one encrypted transfer from explicitly selected recordings.
/// This owns ciphertext and fountain equations only: it never accepts an
/// unlock code, recipient secret, decoded archive, or destination workspace.
/// The caller owns the bounded native reader and must end its lease after
/// cancellation before allowing another recording to start.
public struct OpticalRecordingRecovery: Sendable {
    public struct Recording: Equatable, Sendable {
        private let witness = UUID()
        fileprivate init() {}
    }

    public struct Progress: Equatable, Sendable {
        public let identity: OpticalFrame.Identity?
        public let verifiedBytes: Int
        public let verifiedPages: Int
        public let receivedParts: Int
        public let isComplete: Bool
        public var totalBytes: Int { identity?.envelopeLength ?? 0 }
        public var totalPages: Int { identity?.pageCount ?? 0 }
        public var totalParts: Int { identity?.sourcePartCount ?? 0 }
    }

    private var receiver: OpticalReceiver?
    private var recording: Recording?

    public init() {}

    public var progress: Progress {
        .init(identity: receiver?.identity, verifiedBytes: receiver?.verifiedBytes ?? 0,
              verifiedPages: receiver?.verifiedPages ?? 0, receivedParts: receiver?.receivedParts ?? 0,
              isComplete: receiver?.isComplete ?? false)
    }

    public mutating func beginRecording() throws -> Recording {
        try Task.checkCancellation()
        guard recording == nil else { throw WorkspaceStoreError.busy }
        let lease = Recording()
        recording = lease
        return lease
    }

    /// Every valid FTQR1 symbol in a decoded image must identify the selected
    /// transfer. Check the complete image before admitting any of its symbols;
    /// simultaneous or later competing identities never switch or mix sources.
    /// Unrelated QR text, transfer codes and unreadable wrappers are ignored.
    @discardableResult
    public mutating func observe<C: AirGapCryptography>(_ texts: [String],
        recording lease: Recording, using crypto: C
    ) throws -> Progress {
        try Task.checkCancellation()
        guard recording == lease else { throw FTError.stale }
        guard texts.count <= OpticalRecordingLimits.maximumCodesPerFrame else {
            throw FTError.limit("recording.qr_count")
        }
        let frames = texts.compactMap { text -> OpticalFrame? in
            guard text.utf8.count <= FTLimit.qrText, text.hasPrefix("FTQR1/") else { return nil }
            return try? OpticalFrame(text: text)
        }
        guard let first = frames.first else { return progress }
        let selected = receiver?.identity ?? first.identity
        guard frames.allSatisfy({ $0.identity == selected }) else {
            throw FTError.malformed("recording.ambiguous_sources")
        }
        var candidate = receiver ?? OpticalReceiver(selecting: first)
        for frame in frames {
            do { try candidate.receive(frame, using: crypto) }
            catch {
                // Keep verified pages, but retire the failed page's equations
                // so a subsequent good recording can repair that same page.
                candidate.resetIncompletePage(frame.pageIndex)
                receiver = candidate
                throw error
            }
        }
        if candidate.isComplete {
            do { _ = try candidate.envelope(using: crypto) }
            catch {
                // Page hashes alone cannot establish an envelope. In
                // particular, a forged full digest or transfer ID must never
                // leave progress looking complete or become a replay source.
                receiver = nil
                throw error
            }
        }
        try Task.checkCancellation()
        receiver = candidate
        return progress
    }

    /// Ending a reader retains accepted ciphertext, including partial page
    /// equations. An obsolete reader's cleanup cannot retire a newer lease.
    public mutating func endRecording(_ lease: Recording) {
        if recording == lease { recording = nil }
    }

    /// Raw bytes become available only after reader cleanup and all ordinary
    /// page, whole-envelope digest and FTAG header/transfer-ID checks. This is
    /// not AEAD authentication; the receiving phone still needs its original
    /// transfer code or matching recipient key and its normal source review.
    public func envelope<C: AirGapCryptography>(using crypto: C) throws -> Data {
        try Task.checkCancellation()
        guard recording == nil else { throw WorkspaceStoreError.busy }
        guard let receiver else { throw FTError.malformed("qr.incomplete") }
        return try receiver.envelope(using: crypto)
    }

    /// Explicitly abandon the selected transfer. Late callbacks from its
    /// native reader are rejected even if a new recording has already begun.
    public mutating func discard() {
        receiver = nil
        recording = nil
    }
}
