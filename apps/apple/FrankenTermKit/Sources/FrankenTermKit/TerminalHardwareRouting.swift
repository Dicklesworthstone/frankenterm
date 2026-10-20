import Foundation

/// UIKit must choose one path for a physical press. Printable keys belong to
/// the text system (including dead keys and IME); terminal control keys belong
/// to the shared encoder. Sending a printable press down both paths duplicates it.
public enum TerminalHardwareRouting {
    public enum Destination: Equatable, Sendable { case textSystem, terminal }

    public static func destination(usage: UInt16, modifiers: TerminalKeyModifiers,
                                   hasMarkedText: Bool) -> Destination {
        // Command shortcuts, the keyboard/language switch key, and standalone
        // modifiers remain UIKit's responsibility. IME owns navigation while
        // composing, including Return, Escape, and Backspace.
        guard !hasMarkedText, !modifiers.contains(.command),
              usage > 0, usage < 0xE0 else { return .textSystem }
        // Control-Space is the native keyboard/input-source switch shortcut.
        if usage == 0x2C, modifiers.contains(.control) { return .textSystem }
        if modifiers.contains(.control) || modifiers.contains(.alt) { return .terminal }
        switch usage {
        case 0x28...0x2B, 0x3A...0x45, 0x49...0x52, 0x58, 0x68...0x73:
            return .terminal
        default:
            return .textSystem
        }
    }

    /// A software keyboard has no physical HID usage. Essential ASCII helper
    /// chords use their logical key; other language input continues through IME.
    /// This maps key identities only; ANSI/Kitty encoding remains in the core.
    public static func accessoryKey(text: String, modifiers: TerminalKeyModifiers) throws -> TerminalHardwareKey {
        guard text.unicodeScalars.count == 1, let scalar = text.unicodeScalars.first,
              scalar.value < 128, !modifiers.contains(.command) else { throw TerminalKeyboardError.encoding }
        let value = scalar.value
        var effective = modifiers
        let usage: UInt16
        switch value {
        case 97...122: usage = UInt16(value - 97 + 4)
        case 65...90: usage = UInt16(value - 65 + 4); effective.insert(.shift)
        case 49...57: usage = UInt16(value - 49 + 0x1E)
        case 48: usage = 0x27
        case 10, 13: usage = 0x28
        case 9: usage = 0x2B
        case 32: usage = 0x2C
        default:
            let pairs: [(String, String, UInt16)] = [
                ("-", "_", 0x2D), ("=", "+", 0x2E), ("[", "{", 0x2F),
                ("]", "}", 0x30), ("\\", "|", 0x31), (";", ":", 0x33),
                ("'", "\"", 0x34), ("`", "~", 0x35), (",", "<", 0x36),
                (".", ">", 0x37), ("/", "?", 0x38), ("1", "!", 0x1E),
                ("2", "@", 0x1F), ("3", "#", 0x20), ("4", "$", 0x21),
                ("5", "%", 0x22), ("6", "^", 0x23), ("7", "&", 0x24),
                ("8", "*", 0x25), ("9", "(", 0x26), ("0", ")", 0x27)
            ]
            guard let pair = pairs.first(where: { $0.0 == text || $0.1 == text }) else {
                throw TerminalKeyboardError.encoding
            }
            usage = pair.2
            if pair.1 == text { effective.insert(.shift) }
        }
        return try TerminalHardwareKey(usage: usage, modifiers: effective, characters: text)
    }
}

/// A rejected event does not consume the one-shot latch. Focus retirement clears
/// both latches, so a helper modifier can never migrate to a different machine.
public struct TerminalAccessoryModifiers: Equatable, Sendable {
    public enum State: String, Sendable { case off, oneShot, locked }
    public private(set) var control: State = .off
    public private(set) var alt: State = .off
    public init() {}
    public var modifiers: TerminalKeyModifiers {
        var value: TerminalKeyModifiers = []
        if control != .off { value.insert(.control) }
        if alt != .off { value.insert(.alt) }
        return value
    }
    public mutating func cycleControl() { control = Self.next(control) }
    public mutating func cycleAlt() { alt = Self.next(alt) }
    public mutating func consume() {
        if control == .oneShot { control = .off }
        if alt == .oneShot { alt = .off }
    }
    public mutating func clear() { control = .off; alt = .off }
    private static func next(_ value: State) -> State {
        switch value { case .off: .oneShot; case .oneShot: .locked; case .locked: .off }
    }
}

/// One reviewed, bounded candidate tied to the responder which read it. The
/// native adapter never reads the pasteboard in response to output/foregrounding.
public struct TerminalPasteReview: Sendable, CustomDebugStringConvertible {
    public let id: UUID
    public let keyboardID: UUID
    public let focus: TerminalFocus
    public let text: String
    public var debugDescription: String { "TerminalPasteReview([redacted])" }
    public var containsLineBreaks: Bool { text.contains("\n") || text.contains("\r") }
    public var utf8Bytes: Int { text.utf8.count }

    public init(text: String, keyboardID: UUID, focus: TerminalFocus, maximumUTF8Bytes: Int = 32_768) throws {
        guard (1...131_072).contains(maximumUTF8Bytes), text.utf8.count <= maximumUTF8Bytes else {
            throw TerminalInputError.capacity
        }
        guard !text.isEmpty else { throw TerminalInputError.emptyInput }
        guard !text.unicodeScalars.contains(where: {
            ($0.value < 32 && ![9, 10, 13].contains($0.value)) || (127...159).contains($0.value)
        }) else { throw TerminalInputError.unsafePaste }
        guard focus.source != nil else { throw TerminalInputError.unavailable }
        id = UUID(); self.text = text; self.keyboardID = keyboardID; self.focus = focus
    }
}
