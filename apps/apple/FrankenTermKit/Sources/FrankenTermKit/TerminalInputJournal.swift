import Foundation

/// Content-free observations of input accepted by a direct-SSH transport. These
/// are evidence about transport disposition, never proof of command execution.
/// The union is idempotent if a retained outcome batch is delivered again.
public struct TerminalInputEvidence: OptionSet, Hashable, Sendable {
    public let rawValue: UInt8
    public init(rawValue: UInt8) { self.rawValue = rawValue }
    public static let transportAccepted = Self(rawValue: 1 << 0)
    public static let acceptedPrefix = Self(rawValue: 1 << 1)
    public static let unknownTransportOutcome = Self(rawValue: 1 << 2)
    public static let knownUnsent = Self(rawValue: 1 << 3)
    public var mayHaveRemoteEffect: Bool {
        !intersection([.transportAccepted, .acceptedPrefix, .unknownTransportOutcome]).isEmpty
    }
    var isValid: Bool { rawValue & ~UInt8(15) == 0 }

    static func observations(_ outcomes: [TerminalInputOutcome], source: TerminalSource) throws -> Self {
        guard outcomes.count <= 256 else { throw TerminalSessionJournalError.capacity }
        var evidence: Self = []
        for outcome in outcomes {
            guard outcome.receipt.source == source, outcome.originalBytes > 0,
                  outcome.originalBytes <= 8 * 1024 * 1024,
                  (0...outcome.originalBytes).contains(outcome.acceptedOriginalBytes),
                  (0...65536).contains(outcome.acceptedCleanupBytes) else {
                throw TerminalSessionJournalError.invalidEpoch
            }
            if outcome.acceptedOriginalBytes > 0 || outcome.acceptedCleanupBytes > 0 { evidence.insert(.acceptedPrefix) }
            switch outcome.disposition {
            case .transportAccepted:
                guard outcome.acceptedOriginalBytes == outcome.originalBytes,
                      outcome.acceptedCleanupBytes == 0 else { throw TerminalSessionJournalError.corruptStore }
                evidence.insert(.transportAccepted)
            case .cancelledBeforeWrite, .definitelyNotWritten:
                guard outcome.acceptedOriginalBytes == 0, outcome.acceptedCleanupBytes == 0 else {
                    throw TerminalSessionJournalError.corruptStore
                }
                evidence.insert(.knownUnsent)
            case .cancelledAfterPrefix:
                guard outcome.acceptedOriginalBytes > 0 || outcome.acceptedCleanupBytes > 0 else {
                    throw TerminalSessionJournalError.corruptStore
                }
                evidence.insert(.acceptedPrefix)
            case .connectionLost(let unknown):
                if unknown { evidence.insert(.unknownTransportOutcome) }
            }
        }
        return evidence
    }
}
