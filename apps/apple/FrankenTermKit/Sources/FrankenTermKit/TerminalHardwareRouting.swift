import Foundation

/// Phone-local physical-key choices. These never transform committed IME text,
/// pasted text, or the software keyboard's explicitly labelled helper keys.
public struct TerminalHardwarePreferences: Codable, Equatable, Sendable {
    public enum ModifierMapping: String, Codable, CaseIterable, Hashable, Sendable {
        case optionAsMeta
        case optionForCharacters
        case swapControlAndOption

        public var title: String {
            switch self {
            case .optionAsMeta: "Option as Meta"
            case .optionForCharacters: "Option Types Characters"
            case .swapControlAndOption: "Swap Control and Option"
            }
        }
        public var explanation: String {
            switch self {
            case .optionAsMeta: "Control sends terminal control keys. Option sends Meta shortcuts such as Option-B."
            case .optionForCharacters: "Option stays with the native keyboard layout for accents and other characters. Control still sends terminal control keys."
            case .swapControlAndOption: "The physical Option key sends Control, and the physical Control key sends Meta."
            }
        }
    }
    public enum Backspace: String, Codable, CaseIterable, Hashable, Sendable {
        case delete
        case controlBackspace

        public var title: String {
            switch self {
            case .delete: "Delete (DEL)"
            case .controlBackspace: "Control-Backspace (Ctrl-H)"
            }
        }
    }

    public var modifierMapping: ModifierMapping
    public var backspace: Backspace
    public init(modifierMapping: ModifierMapping = .optionAsMeta, backspace: Backspace = .delete) {
        self.modifierMapping = modifierMapping; self.backspace = backspace
    }
}

/// The immutable preference value is read once per new physical press. A held
/// key retains its admitted mapping even if another scene changes these choices.
/// All mutable state and UserDefaults access are serialized by the same lock.
public final class TerminalHardwarePreferencesStore: @unchecked Sendable {
    public static let shared = TerminalHardwarePreferencesStore()
    static let storageKey = "frankenterm.keyboard.hardware"
    private struct Stored: Codable {
        let version: Int
        let preferences: TerminalHardwarePreferences
    }
    private let lock = NSLock()
    private let defaults: UserDefaults
    private var preferences: TerminalHardwarePreferences

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        if let bytes = defaults.data(forKey: Self.storageKey), bytes.count <= 1024,
           let stored = try? JSONDecoder().decode(Stored.self, from: bytes), stored.version == 1 {
            preferences = stored.preferences
        } else { preferences = .init() }
    }

    public var current: TerminalHardwarePreferences { lock.withLock { preferences } }

    /// One small versioned value keeps a relaunch from observing a half-written
    /// combination. No workspace import can replace these device-local choices.
    public func save(_ preferences: TerminalHardwarePreferences) throws {
        let bytes = try JSONEncoder().encode(Stored(version: 1, preferences: preferences))
        try lock.withLock {
            guard bytes.count <= 1024 else { throw TerminalKeyboardError.capacity }
            defaults.set(bytes, forKey: Self.storageKey)
            self.preferences = preferences
        }
    }
}

/// UIKit must choose one path for a physical press. Printable keys belong to
/// the text system (including dead keys and IME); terminal control keys belong
/// to the shared encoder. Sending a printable press down both paths duplicates it.
public enum TerminalHardwareRouting {
    public enum Destination: Equatable, Sendable { case textSystem, terminal }

    public static func destination(usage: UInt16, modifiers: TerminalKeyModifiers,
                                   hasMarkedText: Bool,
                                   preferences: TerminalHardwarePreferences = .init()) -> Destination {
        // Command shortcuts, the keyboard/language switch key, and standalone
        // modifiers remain UIKit's responsibility. IME owns navigation while
        // composing, including Return, Escape, and Backspace.
        guard !hasMarkedText, !modifiers.contains(.command),
              usage > 0, usage < 0xE0 else { return .textSystem }
        // Control-Space is the native keyboard/input-source switch shortcut.
        if usage == 0x2C, modifiers.contains(.control) { return .textSystem }
        if preferences.modifierMapping == .optionForCharacters,
           modifiers.contains(.alt), !modifiers.contains(.control) { return .textSystem }
        if modifiers.contains(.control) || modifiers.contains(.alt) { return .terminal }
        switch usage {
        case 0x28...0x2B, 0x3A...0x45, 0x49...0x52, 0x58, 0x68...0x73:
            return .terminal
        default:
            return .textSystem
        }
    }

    /// Map semantic keys before the canonical terminal encoder chooses bytes.
    /// Physical system shortcuts are classified before this call. Accessory
    /// modifiers describe already-labelled terminal actions and are not swapped.
    public static func key(usage: UInt16, modifiers: TerminalKeyModifiers, characters: String,
                           preferences: TerminalHardwarePreferences,
                           accessoryModifiers: TerminalKeyModifiers = []) throws -> TerminalHardwareKey {
        guard destination(usage: usage, modifiers: modifiers, hasMarkedText: false,
                          preferences: preferences) == .terminal else { throw TerminalKeyboardError.unavailable }
        var mapped = modifiers
        if preferences.modifierMapping == .swapControlAndOption {
            mapped.subtract([.control, .alt])
            if modifiers.contains(.control) { mapped.insert(.alt) }
            if modifiers.contains(.alt) { mapped.insert(.control) }
        }
        mapped.formUnion(accessoryModifiers)
        if usage == 0x2A, preferences.backspace == .controlBackspace { mapped.insert(.control) }
        return try TerminalHardwareKey(usage: usage, modifiers: mapped, characters: characters)
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
