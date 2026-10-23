import Foundation

/// A command keeps the whole source and exact optional mux visit, including
/// its private registration/visit witnesses. A repeated numeric pane ID or a
/// reconnect to the same named machine cannot make an old draft current again.
public struct TerminalCommandDraftDestination: Equatable, Sendable {
    public let source: TerminalSource
    public let mux: TerminalMuxInputRouting.Destination?
    public init(source: TerminalSource, mux: TerminalMuxInputRouting.Destination? = nil) {
        self.source = source; self.mux = mux
    }
}

public enum TerminalCommandDraftError: Error, Equatable, Sendable, LocalizedError {
    case capacity, unsafeText, invalidRange, empty, compositionPending, pendingInput, historyViewport, unavailable
    public var errorDescription: String? {
        switch self {
        case .capacity: "A command draft can contain at most 32,768 UTF-8 bytes. The last edit was not accepted."
        case .unsafeText: "The draft contains terminal control characters. Use ordinary text, tabs, and line breaks."
        case .invalidRange: "The editing position is no longer valid. Select the text again."
        case .empty: "Enter a command before reviewing it."
        case .compositionPending: "Finish or cancel the terminal's current text composition before opening a command draft."
        case .pendingInput: "Resolve the terminal's pending input before opening a command draft."
        case .historyViewport: "Return to live output before starting a command draft."
        case .unavailable: "The draft's terminal is no longer available for review."
        }
    }
}

/// Local editing only: this owner has no keyboard, encoder, transport or disk
/// access. Return remains part of the draft. The immutable destination belongs
/// to the view which opened it, never a later selected-machine lookup.
///
/// Preparing a review freezes edits while the app explicitly restores a fresh
/// native keyboard. Offering consumes the draft BEFORE invoking the existing
/// paste-review path; that separate decision is still required to admit input.
@MainActor
public final class TerminalCommandDraft<Destination: Equatable & Sendable>: CustomDebugStringConvertible {
    public enum Phase: Equatable, Sendable { case editing, preparingReview, retired }
    public static var maximumUTF8Bytes: Int { 32_768 }
    public let id = UUID()
    public let destination: Destination
    public private(set) var text = ""
    public private(set) var phase: Phase = .editing
    public var utf8Bytes: Int { text.utf8.count }
    public var containsLineBreaks: Bool { text.contains("\n") || text.contains("\r") }
    public nonisolated var debugDescription: String { "TerminalCommandDraft([redacted])" }

    public init(destination: Destination) { self.destination = destination }

    /// Opening an editor must not silently discard a direct-terminal event
    /// that its keyboard pump is still encoding/admitting, or an unresolved
    /// marked/blocked transaction. Read this immediately before retiring input.
    public static func validateOpening(keyboard: TerminalKeyboardSession, liveViewport: Bool = true) throws {
        guard liveViewport else { throw TerminalCommandDraftError.historyViewport }
        guard !keyboard.document.isMarked else { throw TerminalCommandDraftError.compositionPending }
        guard keyboard.accounting.pendingEvents == 0, keyboard.phase != .blocked else {
            throw TerminalCommandDraftError.pendingInput
        }
    }

    /// UIKit may publish complete marked-text changes rather than replacements.
    /// Validate before retaining; a refused edit preserves the last valid draft.
    public func update(_ value: String) throws {
        guard phase == .editing else { throw TerminalCommandDraftError.unavailable }
        try Self.validate(value)
        text = value
    }

    /// Bounds-check native UTF-16 edits, including surrogate pairs, before any
    /// replacement allocation. Scalar boundaries inside a marked grapheme are
    /// allowed so native IMEs can edit their own composition without a fake
    /// terminal editor or premature network commit.
    public func proposedText(replacing range: NSRange, with replacement: String) throws -> String {
        guard phase == .editing else { throw TerminalCommandDraftError.unavailable }
        guard TerminalMarkedText.valid(range, in: text) else { throw TerminalCommandDraftError.invalidRange }
        let units = Array(text.utf16), end = range.location + range.length
        let removed = String(decoding: units[range.location..<end], as: UTF16.self).utf8.count
        guard replacement.utf8.count <= Self.maximumUTF8Bytes - (utf8Bytes - removed) else {
            throw TerminalCommandDraftError.capacity
        }
        try Self.validate(replacement)
        // String's character-based replacement can expand a scalar range to
        // its whole grapheme in Foundation implementations. Preserve the exact
        // validated UTF-16 range instead; no endpoint splits a surrogate pair.
        var result = Array(units[..<range.location])
        result.append(contentsOf: replacement.utf16)
        result.append(contentsOf: units[end...])
        return String(decoding: result, as: UTF16.self)
    }

    public func prepareReview(destination: Destination) throws {
        try requireDestination(destination)
        guard phase == .editing else { throw TerminalCommandDraftError.unavailable }
        guard !text.isEmpty else { throw TerminalCommandDraftError.empty }
        phase = .preparingReview
    }

    /// A failed keyboard restoration may return to this same local editor for
    /// an explicit retry. Cancellation, scene loss and retargeting retire it.
    public func resumeEditing(destination: Destination) throws {
        try requireDestination(destination)
        guard phase == .preparingReview else { throw TerminalCommandDraftError.unavailable }
        phase = .editing
    }

    public func offerReview(destination: Destination, offer: (String) throws -> Void) throws {
        try requireDestination(destination)
        guard phase == .preparingReview else { throw TerminalCommandDraftError.unavailable }
        let value = text
        retire()
        try offer(value)
    }

    public func retire() { text = ""; phase = .retired }

    private func requireDestination(_ destination: Destination) throws {
        guard self.destination == destination else {
            retire()
            throw TerminalCommandDraftError.unavailable
        }
    }

    private static func validate(_ text: String) throws {
        guard text.utf8.count <= maximumUTF8Bytes else { throw TerminalCommandDraftError.capacity }
        guard !text.unicodeScalars.contains(where: {
            ($0.value < 32 && ![9, 10, 13].contains($0.value)) || (127...159).contains($0.value)
        }) else { throw TerminalCommandDraftError.unsafeText }
    }
}
