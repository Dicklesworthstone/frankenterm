import Foundation

public enum TerminalDiagnosticExcerptError: Error, Equatable, Sendable, LocalizedError {
    case capacity, unsafeText, invalidRange, empty, invalidContext, unavailable
    public var errorDescription: String? {
        switch self {
        case .capacity: "An excerpt can contain at most 131,072 UTF-8 bytes. The last edit was not accepted."
        case .unsafeText: "Use ordinary text, tabs, and line breaks in the excerpt. Terminal control characters cannot be shared."
        case .invalidRange: "Select the text again before editing or redacting it."
        case .empty: "Select some output, or keep some edited text, before sharing."
        case .invalidContext: "The captured excerpt's source or receipt time could not be validated."
        case .unavailable: "This excerpt is no longer available. Select the output again."
        }
    }
}

/// An explicit selection from one frozen, canonical copy document. This owner
/// retains only that selection, never the surrounding viewport or a transcript.
/// Editing and redaction are local; a separate immutable preview must be
/// confirmed before an exporter receives any text. No file or clipboard access.
@MainActor
public final class TerminalDiagnosticExcerpt: CustomDebugStringConvertible {
    public struct Context: Sendable {
        public let destinationLabel: String, receivedAt: Date
        fileprivate let header: String

        /// The label belongs to the controller which captured the viewport.
        /// This is the phone's receipt time, not a claim about when a command
        /// ran or when the remote host originally produced a selected line.
        public init(destinationLabel: String, receivedAt: Date) throws {
            guard !destinationLabel.isEmpty, destinationLabel.utf8.count <= 1024,
                  !destinationLabel.unicodeScalars.contains(where: {
                      ($0.value < 32 && $0.value != 10) || (127...159).contains($0.value)
                  }), receivedAt.timeIntervalSinceReferenceDate.isFinite,
                  receivedAt >= .distantPast, receivedAt <= .distantFuture else {
                throw TerminalDiagnosticExcerptError.invalidContext
            }
            let formatter = ISO8601DateFormatter()
            formatter.timeZone = TimeZone(secondsFromGMT: 0)
            formatter.formatOptions = [.withInternetDateTime]
            let timestamp = formatter.string(from: receivedAt)
            let header = "FrankenTerm diagnostic excerpt\n\(destinationLabel)\nViewport received: \(timestamp)\n\n"
            guard !timestamp.isEmpty, header.utf8.count <= 2048 else {
                throw TerminalDiagnosticExcerptError.invalidContext
            }
            self.destinationLabel = destinationLabel; self.receivedAt = receivedAt; self.header = header
        }
    }
    public struct Review: Sendable, CustomDebugStringConvertible {
        public let id: UUID, text: String
        public var debugDescription: String { "TerminalDiagnosticExcerpt.Review([redacted])" }
    }
    public enum Phase: Equatable, Sendable { case editing, reviewing, retired }
    public static let maximumUTF8Bytes = 131_072
    public static let maximumSharedUTF8Bytes = maximumUTF8Bytes + 2048
    public static let redactionMarker = "[redacted]"
    public let id = UUID(), context: Context
    public private(set) var text: String
    public private(set) var includesContext = false
    public private(set) var phase: Phase = .editing
    private var review: Review?
    public var utf8Bytes: Int { text.utf8.count }
    public nonisolated var debugDescription: String { "TerminalDiagnosticExcerpt([redacted])" }

    public init(document: TerminalCopyDocument, selection: NSRange, context: Context) throws {
        // No empty-selection fallback to a viewport or transcript. The copy
        // document expands partially selected canonical cells before extraction.
        let value = try document.selectedText(in: selection)
        try Self.validate(value)
        guard !value.isEmpty else { throw TerminalDiagnosticExcerptError.empty }
        self.context = context; text = value
    }

    public func update(_ value: String) throws {
        guard phase == .editing else { throw TerminalDiagnosticExcerptError.unavailable }
        try Self.validate(value)
        text = value
    }

    public func setIncludesContext(_ include: Bool) throws {
        guard phase == .editing else { throw TerminalDiagnosticExcerptError.unavailable }
        includesContext = include
    }

    /// Validate replacement bounds and size before constructing a larger native
    /// document. Scalar-aligned IME edits remain possible within a grapheme.
    public func proposedText(replacing range: NSRange, with replacement: String) throws -> String {
        guard phase == .editing else { throw TerminalDiagnosticExcerptError.unavailable }
        guard TerminalMarkedText.valid(range, in: text) else { throw TerminalDiagnosticExcerptError.invalidRange }
        let units = Array(text.utf16), end = range.location + range.length
        let removed = String(decoding: units[range.location..<end], as: UTF16.self).utf8.count
        guard replacement.utf8.count <= Self.maximumUTF8Bytes - (utf8Bytes - removed) else {
            throw TerminalDiagnosticExcerptError.capacity
        }
        try Self.validate(replacement)
        var result = Array(units[..<range.location])
        result.append(contentsOf: replacement.utf16)
        result.append(contentsOf: units[end...])
        return String(decoding: result, as: UTF16.self)
    }

    /// Redaction removes whole native graphemes, including a partially selected
    /// surrogate pair or combining sequence. The returned selection highlights
    /// the replacement marker; the removed value is not kept by this owner.
    @discardableResult
    public func redact(_ range: NSRange) throws -> NSRange {
        guard phase == .editing else { throw TerminalDiagnosticExcerptError.unavailable }
        let count = text.utf16.count
        guard range.location >= 0, range.length > 0, range.location < count,
              range.length <= count - range.location else { throw TerminalDiagnosticExcerptError.invalidRange }
        let end = range.location + range.length
        var offset = 0, lower = range.location, upper = end
        for character in text {
            let next = offset + character.utf16.count
            if offset < end && next > range.location { lower = min(lower, offset); upper = max(upper, next) }
            offset = next
            if offset >= end { break }
        }
        text = try proposedText(replacing: NSRange(location: lower, length: upper - lower), with: Self.redactionMarker)
        return NSRange(location: lower, length: Self.redactionMarker.utf16.count)
    }

    public func prepareReview() throws -> Review {
        guard phase == .editing else { throw TerminalDiagnosticExcerptError.unavailable }
        guard !text.isEmpty else { throw TerminalDiagnosticExcerptError.empty }
        let value = (includesContext ? context.header : "") + text
        guard value.utf8.count <= Self.maximumSharedUTF8Bytes else { throw TerminalDiagnosticExcerptError.capacity }
        let candidate = Review(id: UUID(), text: value)
        review = candidate; phase = .reviewing
        return candidate
    }

    public func resumeEditing(reviewID: UUID) throws {
        guard phase == .reviewing, review?.id == reviewID else { throw TerminalDiagnosticExcerptError.unavailable }
        review = nil; phase = .editing
    }

    /// Consume before calling platform code. Failed, reentrant or repeated share
    /// actions cannot replay a decision or retrieve an earlier unredacted value.
    public func share(reviewID: UUID, offer: (String) throws -> Void) throws {
        guard phase == .reviewing, let review, review.id == reviewID else {
            throw TerminalDiagnosticExcerptError.unavailable
        }
        let value = review.text
        retire()
        try offer(value)
    }

    public func retire() { text = ""; review = nil; phase = .retired }

    private static func validate(_ text: String) throws {
        guard text.utf8.count <= maximumUTF8Bytes else { throw TerminalDiagnosticExcerptError.capacity }
        guard !text.unicodeScalars.contains(where: {
            ($0.value < 32 && ![9, 10, 13].contains($0.value)) || (127...159).contains($0.value)
        }) else { throw TerminalDiagnosticExcerptError.unsafeText }
    }
}
