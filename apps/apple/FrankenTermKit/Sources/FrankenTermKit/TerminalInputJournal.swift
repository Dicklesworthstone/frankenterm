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

/// What an ended connection leaves known about the person's own typed or
/// pasted input, for them to read before typing it again. Classes, never
/// bytes; replies the terminal makes by itself are not theirs. Nothing here
/// is ever sent again.
public struct TerminalInputDelivery: Hashable, Sendable {
    /// Input that never left this phone. It was not sent.
    public private(set) var unsent = false
    /// Input that left this phone, but the connection ended before the
    /// machine answered it: it may or may not have arrived.
    public private(set) var unanswered = false
    /// Input stopped after only part of it was sent.
    public private(set) var stoppedPartway = false
    public init() {}
    public var isEmpty: Bool { !unsent && !unanswered && !stoppedPartway }

    /// Interrupted outcomes. Transport acceptance alone is not one: whoever
    /// saw the connection end reports unanswered input with `endedUnanswered`.
    public mutating func record(_ outcomes: [TerminalInputOutcome]) {
        for outcome in outcomes where outcome.kind == .encodedInput || outcome.kind == .paste {
            switch outcome.disposition {
            case .definitelyNotWritten: unsent = true
            case .connectionLost(let tailUnknown):
                if tailUnknown || outcome.acceptedOriginalBytes > 0 || outcome.acceptedCleanupBytes > 0 { unanswered = true }
                else { unsent = true }
            case .cancelledAfterPrefix: stoppedPartway = true
            case .transportAccepted, .cancelledBeforeWrite: break
            }
        }
    }

    /// The connection ended while input that left this phone had no answer.
    public mutating func endedUnanswered() { unanswered = true }

    public func message(machine: String) -> String? {
        guard !isEmpty else { return nil }
        var sentences: [String] = []
        if unanswered {
            sentences.append("Your last input left this phone, but the connection ended before \(machine) answered it. It may or may not have arrived.")
        }
        if unsent {
            sentences.append(unanswered ? "Input after it never left this phone and was not sent."
                                        : "Input typed as the connection ended never left this phone and was not sent.")
        }
        if stoppedPartway { sentences.append("Some input was stopped after only part of it was sent.") }
        sentences.append("Nothing is sent again by itself: check the screen before you type it again.")
        return sentences.joined(separator: " ")
    }
}
