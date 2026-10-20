import Foundation

public enum TerminalKeyboardError: Error, Equatable, Sendable {
    case invalidRange, capacity, unavailable, blocked, encoding, transitionInProgress
}

/// The local UITextInput document contains only uncommitted marked text. It is
/// not a mirror of the terminal, an input-history buffer, or a command editor.
public struct TerminalMarkedText: Sendable, CustomDebugStringConvertible {
    public private(set) var text = ""
    public private(set) var selection = NSRange(location: 0, length: 0)
    public private(set) var isMarked = false
    public let maximumUTF8Bytes: Int
    public var utf16Count: Int { text.utf16.count }
    public var debugDescription: String { "TerminalMarkedText([redacted])" }

    public init(maximumUTF8Bytes: Int = 16_384) throws {
        guard (1...65_536).contains(maximumUTF8Bytes) else { throw TerminalKeyboardError.capacity }
        self.maximumUTF8Bytes = maximumUTF8Bytes
    }
    public static func valid(_ range: NSRange, in text: String) -> Bool {
        let units = text.utf16
        guard range.location >= 0, range.length >= 0, range.location <= units.count,
              range.length <= units.count - range.location else { return false }
        func boundary(_ offset: Int) -> Bool {
            guard offset > 0, offset < units.count else { return true }
            let index = units.index(units.startIndex, offsetBy: offset)
            return !((0xD800...0xDBFF).contains(units[units.index(before: index)])
                && (0xDC00...0xDFFF).contains(units[index]))
        }
        return boundary(range.location) && boundary(range.location + range.length)
    }
    public func substring(_ range: NSRange) throws -> String {
        guard Self.valid(range, in: text) else { throw TerminalKeyboardError.invalidRange }
        return (text as NSString).substring(with: range)
    }
    public mutating func select(_ range: NSRange) throws {
        guard Self.valid(range, in: text) else { throw TerminalKeyboardError.invalidRange }
        selection = range
    }
    public mutating func mark(_ value: String, selectedRange: NSRange) throws {
        guard value.utf8.count <= maximumUTF8Bytes else { throw TerminalKeyboardError.capacity }
        guard Self.valid(selectedRange, in: value) else { throw TerminalKeyboardError.invalidRange }
        text = value; selection = selectedRange; isMarked = true
    }
    public mutating func replace(_ range: NSRange, with value: String) throws {
        guard isMarked, Self.valid(range, in: text) else { throw TerminalKeyboardError.invalidRange }
        let removed = try substring(range).utf8.count
        guard value.utf8.count <= maximumUTF8Bytes - (text.utf8.count - removed) else {
            throw TerminalKeyboardError.capacity
        }
        text = (text as NSString).replacingCharacters(in: range, with: value)
        selection = NSRange(location: range.location + value.utf16.count, length: 0)
    }
    /// Deletes a whole extended grapheme, not a UTF-16 code unit or half a ZWJ.
    /// Marked-but-empty text still consumes Backspace locally until unmarked.
    public mutating func deleteBackward() throws {
        guard isMarked else { throw TerminalKeyboardError.unavailable }
        if selection.length > 0 { try replace(selection, with: ""); return }
        guard selection.location > 0 else { return }
        let cluster = (text as NSString).rangeOfComposedCharacterSequence(at: selection.location - 1)
        try replace(cluster, with: "")
    }
    public mutating func clear() { text = ""; selection = NSRange(location: 0, length: 0); isMarked = false }
}

public struct TerminalKeyModifiers: OptionSet, Hashable, Sendable {
    public let rawValue: UInt8
    public init(rawValue: UInt8) { self.rawValue = rawValue }
    public static let shift = Self(rawValue: 1)
    public static let control = Self(rawValue: 2)
    public static let alt = Self(rawValue: 4)
    public static let command = Self(rawValue: 8)
    public static let capsLock = Self(rawValue: 16)
}
public enum TerminalKeyPhase: Sendable { case down, repeated, up, press }
public struct TerminalHardwareKey: Hashable, Sendable, CustomDebugStringConvertible {
    public var debugDescription: String { "TerminalHardwareKey([redacted])" }
    /// USB HID keyboard/keypad usage, not a platform virtual-keycode.
    public let usage: UInt16
    public let modifiers: TerminalKeyModifiers
    public let characters: String
    public init(usage: UInt16, modifiers: TerminalKeyModifiers = [], characters: String = "") throws {
        guard usage > 0, usage <= 0xE7, characters.utf8.count <= 256,
              modifiers.rawValue & ~UInt8(31) == 0 else { throw TerminalKeyboardError.encoding }
        self.usage = usage; self.modifiers = modifiers; self.characters = characters
    }
}
public enum TerminalKeyboardIntent: Sendable, CustomDebugStringConvertible {
    public var debugDescription: String { "TerminalKeyboardIntent([redacted])" }
    case text(String)
    case key(TerminalHardwareKey, TerminalKeyPhase)
    case releaseAll([TerminalHardwareKey])
}

/// Implement using the canonical terminal encoder and its source-owned mode
/// state. This method is encoding ONLY: no writer, local echo, or network effect.
/// It must honor the byte cap, avoid MainActor/long terminal locks, and cooperate
/// with cancellation. No fallback ANSI encoder is supplied by the Swift UI.
public protocol TerminalKeyboardEncoding: Sendable {
    func encode(_ intent: TerminalKeyboardIntent, source: TerminalSource, maximumBytes: Int) async throws -> Data
    var supportsPaste: Bool { get }
    /// The canonical source owner applies its newline policy and samples its
    /// current bracketed-paste mode in the same ordering domain as encode.
    /// The UI must never choose a bracket flag or implement an ANSI fallback.
    func preparePaste(_ text: String, source: TerminalSource, maximumBytes: Int) async throws -> TerminalPreparedPaste
}
public struct TerminalPreparedPaste: Sendable, CustomDebugStringConvertible {
    public let text: String
    public let bracketed: Bool
    public var debugDescription: String { "TerminalPreparedPaste([redacted])" }
    public init(text: String, bracketed: Bool) { self.text = text; self.bracketed = bracketed }
}
public extension TerminalKeyboardEncoding {
    var supportsPaste: Bool { false }
    func preparePaste(_ text: String, source: TerminalSource, maximumBytes: Int) async throws -> TerminalPreparedPaste {
        throw TerminalKeyboardError.unavailable
    }
}

public enum TerminalKeyboardPhase: Equatable, Sendable { case active, handingOff, blocked, suspending, retired }
public struct TerminalKeyboardAccounting: Equatable, Sendable {
    public let pendingEvents: Int, pendingUTF8Bytes: Int, compositionUTF8Bytes: Int, heldKeys: Int
    public let admittedEvents: UInt64, discardedUnadmittedEvents: UInt64
}

/// One immutable responder context per focus generation. Synchronous UIKit
/// callbacks enter a bounded FIFO; ONE owned pump performs the async encoding
/// and coordinator admission. Nothing is resolved through a current-pane pointer.
@MainActor
public final class TerminalKeyboardSession: CustomDebugStringConvertible {
    public nonisolated var debugDescription: String { "TerminalKeyboardSession([redacted])" }
    public let id = UUID()
    public let originalFocus: TerminalFocus
    public private(set) var document: TerminalMarkedText
    public private(set) var phase: TerminalKeyboardPhase = .active
    public private(set) var lastFailure: TerminalKeyboardError?
    /// Content-free receipts; consuming these must not discard the downstream
    /// coordinator's effect/uncertainty ledger. There is no receipt history here.
    public var onAdmission: (@MainActor (TerminalInputReceipt) -> Void)?
    public var onStateChange: (@MainActor () -> Void)?
    public var acceptsInput: Bool { phase == .active }
    public var acceptsPaste: Bool { acceptsInput && !document.isMarked && encoder.supportsPaste }

    private enum Event {
        case beginComposition, commitComposition(String), text(String), key(TerminalHardwareKey, TerminalKeyPhase)
        case paste(TerminalPasteReview)
        var charge: Int {
            switch self {
            case .beginComposition: 0
            case .commitComposition(let text), .text(let text): text.utf8.count
            case .key(let key, _): key.characters.utf8.count + 16
            case .paste(let review): review.utf8Bytes
            }
        }
    }
    private let coordinator: TerminalInputCoordinator
    private let encoder: any TerminalKeyboardEncoding
    private let maximumEvents: Int, maximumBytes: Int, maximumEncodedBytes: Int
    private var focus: TerminalFocus
    private var composition: TerminalComposition?
    private var queue: [Event] = []
    private var flight: Event?
    private var held: [UInt16: TerminalHardwareKey] = [:]
    private var worker: Task<Void, Never>?
    private var transition: Task<TerminalFocus, Error>?
    private var shutdown: Task<Void, Never>?
    private var ownsFocus = true
    private var admitted: UInt64 = 0, discarded: UInt64 = 0
    private var stateObservers: [UUID: @MainActor () -> Void] = [:]

    public init(focus: TerminalFocus, coordinator: TerminalInputCoordinator,
                encoder: any TerminalKeyboardEncoding, maximumEvents: Int = 128,
                maximumBytes: Int = 32_768, maximumEncodedBytes: Int = 32_768) throws {
        guard focus.source != nil else { throw TerminalKeyboardError.unavailable }
        guard (2...512).contains(maximumEvents), (16...131_072).contains(maximumBytes),
              (16...131_072).contains(maximumEncodedBytes) else { throw TerminalKeyboardError.capacity }
        originalFocus = focus; self.focus = focus; self.coordinator = coordinator; self.encoder = encoder
        self.maximumEvents = maximumEvents; self.maximumBytes = maximumBytes; self.maximumEncodedBytes = maximumEncodedBytes
        document = try TerminalMarkedText(maximumUTF8Bytes: min(16_384, maximumBytes))
    }

    public var accounting: TerminalKeyboardAccounting {
        .init(pendingEvents: queue.count + (flight == nil ? 0 : 1),
              pendingUTF8Bytes: queue.reduce(0) { $0 + $1.charge } + (flight?.charge ?? 0),
              compositionUTF8Bytes: document.text.utf8.count, heldKeys: held.count,
              admittedEvents: admitted, discardedUnadmittedEvents: discarded)
    }
    private func requireActive() throws { guard acceptsInput else { throw TerminalKeyboardError.unavailable } }
    /// Native responders observe lifecycle without replacing the application's
    /// callback. A bounded snapshot permits removal/suspension from a callback.
    public func observeState(_ observer: @escaping @MainActor () -> Void) throws -> UUID {
        guard stateObservers.count < 8 else { throw TerminalKeyboardError.capacity }
        let token = UUID(); stateObservers[token] = observer; return token
    }
    public func removeStateObserver(_ token: UUID) { stateObservers[token] = nil }
    private func notifyStateChange() {
        let callbacks = Array(stateObservers.values)
        onStateChange?()
        for callback in callbacks { callback() }
    }
    private func append(_ event: Event) throws {
        let usage = accounting
        guard usage.pendingEvents < maximumEvents, event.charge <= maximumBytes - usage.pendingUTF8Bytes else {
            throw TerminalKeyboardError.capacity
        }
        queue.append(event); schedule()
    }
    public func setMarkedText(_ text: String?, selectedRange: NSRange) throws {
        try requireActive()
        var candidate = document
        try candidate.mark(text ?? "", selectedRange: selectedRange)
        if !document.isMarked { try append(.beginComposition) }
        document = candidate
    }
    public func select(_ range: NSRange) throws { try requireActive(); try document.select(range) }
    public func replace(_ range: NSRange, with text: String) throws {
        try requireActive()
        if document.isMarked { try document.replace(range, with: text) }
        else {
            guard range == NSRange(location: 0, length: 0) else { throw TerminalKeyboardError.invalidRange }
            try insertText(text)
        }
    }
    public func insertText(_ text: String) throws {
        try requireActive()
        if document.isMarked { try append(.commitComposition(text)); document.clear() }
        else if !text.isEmpty { try append(.text(text)) }
    }
    /// This commits the current local candidate ONCE. It does not synthesize an
    /// insertText callback, an Enter, or text matching/deduplication heuristics.
    public func unmarkText() throws {
        try requireActive()
        guard document.isMarked else { return }
        try append(.commitComposition(document.text)); document.clear()
    }
    public func deleteBackward() throws {
        try requireActive()
        if document.isMarked { try document.deleteBackward() }
        else { try key(TerminalHardwareKey(usage: 0x2A), phase: .press) }
    }
    public func key(_ key: TerminalHardwareKey, phase keyPhase: TerminalKeyPhase) throws {
        try requireActive()
        // A physical key already released by beginComposition can still produce
        // its delayed UIKit key-up. Keep it ordered; process ignores it once its
        // original held-key ownership has been settled.
        guard !document.isMarked || keyPhase == .up else { throw TerminalInputError.compositionPending }
        try append(.key(key, keyPhase))
    }
    public func paste(_ review: TerminalPasteReview) throws {
        try requireActive()
        guard acceptsPaste else { throw TerminalKeyboardError.unavailable }
        guard review.keyboardID == id, review.focus == originalFocus else { throw TerminalInputError.stale }
        try append(.paste(review))
    }

    private func schedule() {
        guard worker == nil else { return }
        worker = Task { [weak self] in await self?.pump() }
    }
    private func encode(_ intent: TerminalKeyboardIntent, maximum: Int? = nil) async throws -> Data {
        guard let source = originalFocus.source else { throw TerminalKeyboardError.unavailable }
        let cap = maximum ?? maximumEncodedBytes
        let bytes = try await encoder.encode(intent, source: source, maximumBytes: cap)
        guard bytes.count <= cap else { throw TerminalKeyboardError.capacity }
        return bytes
    }
    private func checkLivePump() throws {
        try Task.checkCancellation()
        guard phase == .active || phase == .handingOff else { throw TerminalKeyboardError.unavailable }
    }
    private func process(_ event: Event) async throws {
        try checkLivePump()
        var receipt: TerminalInputReceipt?
        switch event {
        case .beginComposition:
            let keys = held.values.sorted { $0.usage < $1.usage }
            if !keys.isEmpty {
                let bytes = try await encode(.releaseAll(keys), maximum: 1024)
                try checkLivePump()
                let cleanup = try await coordinator.cleanup(originalFocus, encodedBytes: bytes)
                // Only clear captured keys after their release has passed the
                // reserved admission boundary. Failure leaves this event/held
                // state blocked on the original source for retry or suspension.
                held.removeAll()
                if let cleanup { onAdmission?(cleanup) }
                try checkLivePump()
            }
            composition = try await coordinator.beginComposition(originalFocus)
        case .commitComposition(let text):
            guard let token = composition else { throw TerminalKeyboardError.encoding }
            let bytes = text.isEmpty ? Data() : try await encode(.text(text))
            try checkLivePump()
            guard text.isEmpty || !bytes.isEmpty else { throw TerminalKeyboardError.encoding }
            receipt = try await coordinator.commitComposition(token, encodedBytes: bytes)
            composition = nil
        case .text(let text):
            let bytes = try await encode(.text(text))
            try checkLivePump()
            guard !bytes.isEmpty else { throw TerminalKeyboardError.encoding }
            receipt = try await coordinator.submit(originalFocus, encodedBytes: bytes)
        case .key(let key, let keyPhase):
            // A release/repeat from an old UIKit responder must not manufacture
            // a key on a fresh context. Ordinary repeated down callbacks remain
            // valid; native code marks subsequent observed downs as repeats.
            if (keyPhase == .up || keyPhase == .repeated) && held[key.usage] == nil { return }
            if keyPhase == .down && held[key.usage] == nil && held.count >= 32 { throw TerminalKeyboardError.capacity }
            let bytes = try await encode(.key(key, keyPhase))
            try checkLivePump()
            if !bytes.isEmpty { receipt = try await coordinator.submit(originalFocus, encodedBytes: bytes) }
            switch keyPhase {
            case .down, .repeated: held[key.usage] = key
            case .up: held[key.usage] = nil
            case .press: break
            }
        case .paste(let review):
            guard let source = originalFocus.source else { throw TerminalKeyboardError.unavailable }
            let prepared = try await encoder.preparePaste(review.text, source: source, maximumBytes: maximumEncodedBytes)
            try checkLivePump()
            guard prepared.text.utf8.count <= maximumEncodedBytes else { throw TerminalKeyboardError.capacity }
            receipt = try await coordinator.paste(originalFocus, text: prepared.text, bracketed: prepared.bracketed)
        }
        if admitted < UInt64.max { admitted += 1 }
        if let receipt { onAdmission?(receipt) }
    }
    private func pump() async {
        while !queue.isEmpty && (phase == .active || phase == .handingOff) {
            let event = queue.removeFirst(); flight = event
            do { try await process(event); flight = nil }
            catch {
                if phase != .suspending {
                    phase = .blocked
                    lastFailure = (error as? TerminalKeyboardError) ?? (error is TerminalInputError ? .blocked : .encoding)
                    // Retain the unadmitted head and suffix. The operator may
                    // retry a local rejection or discard, never silently replay.
                } else { if discarded < UInt64.max { discarded += 1 }; flight = nil }
                break
            }
        }
        worker = nil; notifyStateChange()
    }
    public func waitUntilSettled() async { if let worker { await worker.value } }
    /// Only retries a synchronous coordinator admission/side-effect-free encoder
    /// rejection. It NEVER retries a transport write or a remote unknown outcome.
    public func retryLocalAdmission() throws {
        guard phase == .blocked, transition == nil, let failed = flight else { throw TerminalKeyboardError.unavailable }
        queue.insert(failed, at: 0); flight = nil; lastFailure = nil; phase = .active; schedule()
    }

    /// Drains local callbacks, settles marked text, and places held-key cleanup
    /// before the coordinator's focus barrier. Does not wait for network writes.
    /// After success this object is permanently retired; create a NEW responder
    /// with the returned focus. Late callbacks cannot target that new responder.
    public func handoff(to source: TerminalSource?) async throws -> TerminalFocus {
        try requireActive()
        try unmarkText()
        phase = .handingOff
        let job = Task<TerminalFocus, Error> { [self] in
            if let worker { await worker.value }
            guard phase == .handingOff else { throw TerminalKeyboardError.blocked }
            try Task.checkCancellation()
            let keys = held.values.sorted { $0.usage < $1.usage }
            let cleanup = keys.isEmpty ? Data() : try await encode(.releaseAll(keys), maximum: 1024)
            try Task.checkCancellation()
            guard phase == .handingOff else { throw TerminalKeyboardError.unavailable }
            let next = try await coordinator.changeFocus(focus, to: source, cleanup: cleanup)
            // The actor operation may have linearized before suspension. Keep
            // ownership of its result so shutdown can retire the actual token.
            focus = next; held.removeAll()
            guard phase == .handingOff, !Task.isCancelled else {
                // Explicit suspension waits for this transition and owns its
                // resulting token. A caller-cancelled handoff has no recipient;
                // retire that token here rather than leak an invisible stream.
                if phase != .suspending {
                    _ = try? await coordinator.suspend(next)
                    ownsFocus = false; phase = .retired; notifyStateChange()
                }
                throw TerminalKeyboardError.unavailable
            }
            ownsFocus = false; phase = .retired; notifyStateChange()
            return next
        }
        transition = job
        do {
            let result = try await withTaskCancellationHandler {
                try await job.value
            } onCancel: { job.cancel() }
            // Cancellation can race the job's final synchronous return too.
            if Task.isCancelled {
                _ = try? await coordinator.suspend(result)
                transition = nil
                throw CancellationError()
            }
            transition = nil; return result
        } catch {
            transition = nil
            if phase == .handingOff { phase = .active; lastFailure = .blocked; notifyStateChange() }
            throw error
        }
    }

    /// Freeze immediately without waiting for UIKit/encoding/I/O. Unadmitted
    /// callbacks are discarded, admitted receipts retain their original source.
    /// The session owner MUST call this on OS deactivation or view disposal.
    public func suspend() {
        guard ownsFocus, phase != .suspending, phase != .retired else { return }
        phase = .suspending; document.clear()
        let count = UInt64(queue.count)
        discarded = discarded > UInt64.max - count ? UInt64.max : discarded + count
        queue.removeAll(); worker?.cancel(); transition?.cancel()
        let oldWorker = worker, oldTransition = transition
        shutdown = Task { [self] in
            if let oldWorker { await oldWorker.value }
            if let oldTransition { _ = try? await oldTransition.value }
            // A blocked head was never admitted; release it at explicit discard.
            if flight != nil { if discarded < UInt64.max { discarded += 1 }; flight = nil }
            var cleanup = Data()
            let keys = held.values.sorted { $0.usage < $1.usage }
            if !keys.isEmpty {
                do { cleanup = try await encode(.releaseAll(keys), maximum: 1024) }
                catch { lastFailure = .encoding }
            }
            do {
                if try await !coordinator.suspend(focus, cleanup: cleanup) { lastFailure = .capacity }
            } catch { lastFailure = .blocked }
            held.removeAll(); composition = nil; ownsFocus = false; phase = .retired
            notifyStateChange()
        }
        notifyStateChange()
    }
    public func waitUntilSuspended() async { if let shutdown { await shutdown.value } }
}
