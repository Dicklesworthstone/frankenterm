#if canImport(UIKit)
import GameController
import UIKit
import SwiftUI

@MainActor
private final class TerminalTextPosition: UITextPosition {
    let owner: UUID
    let offset: Int
    init(owner: UUID, offset: Int) { self.owner = owner; self.offset = offset }
}

@MainActor
private final class TerminalTextRange: UITextRange {
    let owner: UUID
    let value: NSRange
    init(owner: UUID, value: NSRange) { self.owner = owner; self.value = value }
    override var start: UITextPosition { TerminalTextPosition(owner: owner, offset: value.location) }
    override var end: UITextPosition { TerminalTextPosition(owner: owner, offset: value.location + value.length) }
    override var isEmpty: Bool { value.length == 0 }
}

@MainActor
private final class TerminalSelectionRect: UITextSelectionRect {
    let value: CGRect
    let first: Bool, last: Bool
    init(_ value: CGRect, first: Bool, last: Bool) { self.value = value; self.first = first; self.last = last }
    override var rect: CGRect { value }
    override var writingDirection: NSWritingDirection { .natural }
    override var containsStart: Bool { first }
    override var containsEnd: Bool { last }
    override var isVertical: Bool { false }
}

/// Geometry comes from the same source as the displayed terminal. An old frame
/// cannot move this responder's IME candidate window onto a different machine.
public struct NativeTerminalCaret {
    public let source: TerminalSource
    /// Monotonic caret projection snapshot, advanced whenever cursor or local
    /// viewport geometry changes. Repeating an identical snapshot is harmless.
    public let revision: UInt64
    public let rect: CGRect
    public init(source: TerminalSource, revision: UInt64, rect: CGRect) {
        self.source = source; self.revision = revision; self.rect = rect
    }
}

/// An actual UIKit text-input client, containing ONLY the bounded uncommitted
/// composition. The terminal renderer remains a sibling/parent surface. One
/// instance belongs to one immutable keyboard session and is never retargeted.
@MainActor
public final class NativeTerminalInputView: UIView, UITextInput {
    public let session: TerminalKeyboardSession
    public var onFailure: (@MainActor (Error) -> Void)?
    public var onPasteReview: (@MainActor (TerminalPasteReview) -> Void)?
    public var onRetired: (@MainActor () -> Void)?
    /// Asked when a key arrives while no caret is drawn: true while the live
    /// screen is still the one shown, only being redrawn (a resize or new
    /// text size), so typing still reaches it.
    public var onInputWithoutProjection: (@MainActor () -> Bool)?
    public private(set) var accessoryModifiers = TerminalAccessoryModifiers()
    public private(set) var pendingPaste: TerminalPasteReview?

    public weak var inputDelegate: (any UITextInputDelegate)?
    public lazy var tokenizer: any UITextInputTokenizer = UITextInputStringTokenizer(textInput: self)
    public var markedTextStyle: [NSAttributedString.Key: Any]?
    public var selectionAffinity: UITextStorageDirection = .forward
    public var textInputView: UIView { self }
    public var isEditable: Bool { acceptsKeys && hasGeometry }
    /// Keys need no caret: the key row stays usable while the live screen is
    /// redrawn, and its keys are admitted like typing (see `typed`).
    private var acceptsKeys: Bool { session.acceptsInput && !retiring && !ordinaryInputBlocked }
    /// Review keeps its exact focus alive while withholding ordinary typing.
    /// Only confirmPaste may use this separate admission while a panel covers
    /// the terminal; a new source/geometry still invalidates the candidate.
    public var canReviewPaste: Bool { session.acceptsPaste && hasGeometry && !retiring }
    public var autocapitalizationType: UITextAutocapitalizationType = .none
    public var autocorrectionType: UITextAutocorrectionType = .no
    public var spellCheckingType: UITextSpellCheckingType = .no
    public var smartQuotesType: UITextSmartQuotesType = .no
    public var smartDashesType: UITextSmartDashesType = .no
    public var smartInsertDeleteType: UITextSmartInsertDeleteType = .no
    public var keyboardType: UIKeyboardType = .default
    public var keyboardAppearance: UIKeyboardAppearance = .default
    public var returnKeyType: UIReturnKeyType = .default
    public var enablesReturnKeyAutomatically = false
    public var isSecureTextEntry = false
    public var textContentType: UITextContentType! = nil
    public var writingToolsBehavior: UIWritingToolsBehavior = .none

    private let textStorage = NSTextStorage()
    private let layoutManager = NSLayoutManager()
    private let textContainer = NSTextContainer(size: .zero)
    private var caret = CGRect(x: 0, y: 0, width: 2, height: 20)
    private var geometryRevision: UInt64 = 0
    private var hasGeometry = false
    private var compositionOrigin = CGPoint.zero
    private var font = UIFont.monospacedSystemFont(ofSize: 15, weight: .regular)
    private var foreground = UIColor.label
    private var stateObservation: UUID?
    private var observedPhase: TerminalKeyboardPhase = .active
    private var physicalKeys: [UInt16: TerminalHardwareKey] = [:]
    private var retiring = false
    private var enteredWindow = false
    private var keyboardRequested = false
    private var reviewingPaste = false
    private var ordinaryInputBlocked: Bool { reviewingPaste || pendingPaste != nil }
    private var controlButton: UIButton?
    private var altButton: UIButton?
    private var terminalButtons: [UIButton] = []
    private var dismissButton: UIButton?
    private weak var accessoryBar: UIView?
    private var theme: TerminalChromeTheme?
    private lazy var accessory = makeAccessory()
    /// Copy (⌘C or the edit menu) of the terminal's own selection, which the
    /// owning view keeps; this responder's document holds only composition.
    public var canCopySelection: (@MainActor () -> Bool)?
    public var copySelection: (@MainActor () -> Void)?

    public init(session: TerminalKeyboardSession) throws {
        self.session = session
        super.init(frame: .zero)
        isOpaque = false; backgroundColor = .clear
        accessibilityLabel = "Terminal keyboard"
        accessibilityHint = "Types into the selected terminal."
        textContainer.lineFragmentPadding = 0
        layoutManager.addTextContainer(textContainer)
        textStorage.addLayoutManager(layoutManager)
        stateObservation = try session.observeState { [weak self] in self?.stateChanged() }
        NotificationCenter.default.addObserver(self, selector: #selector(sceneDeactivated(_:)),
                                               name: UIScene.willDeactivateNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(applicationDeactivated),
                                               name: UIApplication.willResignActiveNotification, object: nil)
        refreshComposition()
    }
    @available(*, unavailable)
    public required init?(coder: NSCoder) { fatalError("Use init(session:)") }
    deinit { NotificationCenter.default.removeObserver(self) }

    public override var canBecomeFirstResponder: Bool { session.acceptsInput && hasGeometry && !retiring }
    public override var inputAccessoryView: UIView? { accessory }
    // A remote shell may contain text even though our local composition is
    // empty. Keep the software Backspace enabled without mirroring that shell.
    public var hasText: Bool { session.acceptsInput && !ordinaryInputBlocked }

    /// Install before restoring first responder for a command's Review step.
    /// Pending paste candidates also impose this hold automatically. Releasing
    /// the panel's hold cannot revive a candidate cleared by source retirement.
    public func setReviewingPaste(_ reviewing: Bool) {
        reviewingPaste = reviewing
        if reviewing { accessoryModifiers.clear() }
        updateAccessory()
    }

    public func updateCaret(_ value: NativeTerminalCaret) throws {
        guard value.source == session.originalFocus.source, value.revision > 0,
              value.revision >= geometryRevision else { throw TerminalInputError.stale }
        let r = value.rect
        guard [r.minX, r.minY, r.width, r.height].allSatisfy({ $0.isFinite }),
              abs(r.minX) <= 1_000_000, abs(r.minY) <= 1_000_000,
              r.width > 0, r.width <= 4096, r.height > 0, r.height <= 4096 else {
            throw TerminalKeyboardError.invalidRange
        }
        guard value.revision > geometryRevision || (hasGeometry && r == caret) else { throw TerminalInputError.stale }
        let changed = !hasGeometry || caret != r
        if changed { inputDelegate?.selectionWillChange(self) }
        caret = r; geometryRevision = value.revision; hasGeometry = true
        updateAccessory(); setNeedsLayout(); setNeedsDisplay()
        if changed { inputDelegate?.selectionDidChange(self) }
    }
    /// A resize, font change or viewport without the live cursor invalidates
    /// the displayed projection. Preserve uncommitted composition, but stop
    /// new input and hide candidate geometry until a matching frame completes.
    /// Key-up cleanup remains available for keys admitted before invalidation.
    /// Ctrl and alt the user set stay set: a new text size or a look at
    /// history does not change what the next key should send.
    public func invalidateRenderedCaret(source: TerminalSource) throws {
        guard source == session.originalFocus.source else { throw TerminalInputError.stale }
        guard hasGeometry else { return }
        inputDelegate?.selectionWillChange(self)
        hasGeometry = false; pendingPaste = nil
        updateAccessory(); setNeedsLayout(); setNeedsDisplay()
        inputDelegate?.selectionDidChange(self)
    }
    public func updateAppearance(font: UIFont, foreground: UIColor) {
        guard font.pointSize.isFinite, (6...128).contains(font.pointSize) else { return }
        self.font = font; self.foreground = foreground; refreshComposition()
    }
    public func requestKeyboard(_ visible: Bool) {
        keyboardRequested = visible
        if visible, window != nil, !isFirstResponder { _ = becomeFirstResponder() }
        else if !visible, isFirstResponder { retire() }
    }
    /// The caller runs the session's acknowledged handoff BEFORE installing a
    /// new responder. This operation is for OS interruption/disposal, and never
    /// commits unsubmitted composition or sends a synthetic Return.
    public func retire() {
        guard !retiring else { return }
        retiring = true
        keyboardRequested = false
        inputDelegate?.textWillChange(self); inputDelegate?.selectionWillChange(self)
        accessoryModifiers.clear(); pendingPaste = nil; physicalKeys.removeAll()
        session.suspend()
        if let token = stateObservation { session.removeStateObserver(token); stateObservation = nil }
        refreshComposition()
        inputDelegate?.selectionDidChange(self); inputDelegate?.textDidChange(self)
        _ = super.resignFirstResponder()
        onRetired?()
    }
    public override func resignFirstResponder() -> Bool {
        let resigned = super.resignFirstResponder()
        if resigned { retire() }
        return resigned
    }
    public override func didMoveToWindow() {
        super.didMoveToWindow()
        if window != nil {
            enteredWindow = true
            if keyboardRequested { _ = becomeFirstResponder() }
        }
        else if enteredWindow { retire() }
    }
    @objc private func sceneDeactivated(_ notification: Notification) {
        guard let scene = notification.object as? UIScene, scene === window?.windowScene else { return }
        retire()
    }
    @objc private func applicationDeactivated() { retire() }
    private func stateChanged() {
        let changed = observedPhase != session.phase
        observedPhase = session.phase
        if session.phase == .retired || session.phase == .suspending { retire(); return }
        refreshComposition()
        if changed, let failure = session.lastFailure { onFailure?(failure) }
    }

    private func makePosition(_ offset: Int) -> TerminalTextPosition {
        TerminalTextPosition(owner: session.id, offset: offset)
    }
    private func makeRange(_ value: NSRange) -> TerminalTextRange {
        TerminalTextRange(owner: session.id, value: value)
    }
    private func offset(_ position: UITextPosition) -> Int? {
        guard let p = position as? TerminalTextPosition, p.owner == session.id,
              TerminalMarkedText.valid(NSRange(location: p.offset, length: 0), in: session.document.text) else { return nil }
        return p.offset
    }
    private func value(_ range: UITextRange) -> NSRange? {
        guard let r = range as? TerminalTextRange, r.owner == session.id,
              TerminalMarkedText.valid(r.value, in: session.document.text) else { return nil }
        return r.value
    }
    public var beginningOfDocument: UITextPosition { makePosition(0) }
    public var endOfDocument: UITextPosition { makePosition(session.document.utf16Count) }
    public var selectedTextRange: UITextRange? {
        get { makeRange(session.document.selection) }
        set {
            guard let newValue, let selection = value(newValue) else { return }
            mutate { try session.select(selection) }
        }
    }
    public var markedTextRange: UITextRange? {
        session.document.isMarked ? makeRange(NSRange(location: 0, length: session.document.utf16Count)) : nil
    }
    /// UIKit-originated mutations must NOT recursively notify its input delegate.
    /// Delegate notifications are used for external lifecycle/selection changes.
    private func mutate(_ operation: () throws -> Void) {
        guard !retiring, hasGeometry, !ordinaryInputBlocked else { return }
        do { try operation(); refreshComposition() }
        catch { onFailure?(error) }
    }
    /// Typing while no rendered projection exists (for example while history
    /// is shown) is still not sent; the owner may bring the live screen back.
    /// While the live screen is only being redrawn, committed text and keys
    /// are sent: they need no caret geometry. Composition still waits for it.
    private func typed(composing: Bool = false, _ operation: () throws -> Void) {
        guard !ordinaryInputBlocked else { return }
        if !retiring, !hasGeometry {
            let live = onInputWithoutProjection?() == true
            guard live, !composing, !session.document.isMarked else { return }
            do { try operation(); refreshComposition() }
            catch { onFailure?(error) }
            return
        }
        mutate(operation)
    }
    public func text(in range: UITextRange) -> String? {
        guard let range = value(range) else { return nil }
        return try? session.document.substring(range)
    }
    public func replace(_ range: UITextRange, withText text: String) {
        guard let range = value(range) else { return }
        if !session.document.isMarked, range == NSRange(location: 0, length: 0) {
            insertText(text); return
        }
        mutate { try session.replace(range, with: text) }
    }
    public func setMarkedText(_ markedText: String?, selectedRange: NSRange) {
        // Helper modifiers never silently transform a native language candidate.
        // Starting IME returns them to the visibly inactive state.
        typed(composing: true) {
            try session.setMarkedText(markedText, selectedRange: selectedRange)
            accessoryModifiers.clear()
        }
    }
    public func unmarkText() { mutate { try session.unmarkText() } }
    public func insertText(_ text: String) {
        typed {
            if !session.document.isMarked,
               !accessoryModifiers.modifiers.isEmpty || ["\n", "\r", "\t"].contains(text) {
                let key = try TerminalHardwareRouting.accessoryKey(text: text, modifiers: accessoryModifiers.modifiers)
                try session.key(key, phase: .press); accessoryModifiers.consume()
            } else { try session.insertText(text) }
        }
    }
    public func deleteBackward() {
        typed {
            if session.document.isMarked { try session.deleteBackward() }
            else {
                try session.key(TerminalHardwareKey(usage: 0x2A, modifiers: accessoryModifiers.modifiers), phase: .press)
                accessoryModifiers.consume()
            }
        }
    }
    public func textRange(from: UITextPosition, to: UITextPosition) -> UITextRange? {
        guard let start = offset(from), let end = offset(to) else { return nil }
        return makeRange(NSRange(location: min(start, end), length: abs(end - start)))
    }
    public func position(from: UITextPosition, offset delta: Int) -> UITextPosition? {
        guard let base = offset(from) else { return nil }
        let (next, overflow) = base.addingReportingOverflow(delta)
        guard !overflow, TerminalMarkedText.valid(NSRange(location: next, length: 0), in: session.document.text) else { return nil }
        return makePosition(next)
    }
    public func position(from: UITextPosition, in direction: UITextLayoutDirection, offset distance: Int) -> UITextPosition? {
        guard let base = offset(from), distance != Int.min else { return nil }
        switch direction {
        case .left: return position(from: from, offset: -distance)
        case .right: return position(from: from, offset: distance)
        case .up, .down:
            let rect = caretRect(for: makePosition(base))
            let delta = CGFloat(distance) * max(font.lineHeight, 1) * (direction == .up ? -1 : 1)
            guard delta.isFinite else { return nil }
            return closestPosition(to: CGPoint(x: rect.minX, y: rect.midY + delta))
        @unknown default: return nil
        }
    }
    public func compare(_ position: UITextPosition, to other: UITextPosition) -> ComparisonResult {
        guard let a = offset(position), let b = offset(other) else { return .orderedSame }
        return a == b ? .orderedSame : (a < b ? .orderedAscending : .orderedDescending)
    }
    public func offset(from: UITextPosition, to: UITextPosition) -> Int {
        guard let a = offset(from), let b = offset(to) else { return 0 }
        return b - a
    }
    public func position(within range: UITextRange, farthestIn direction: UITextLayoutDirection) -> UITextPosition? {
        guard let r = value(range) else { return nil }
        switch direction {
        case .left, .up: return makePosition(r.location)
        case .right, .down: return makePosition(r.location + r.length)
        @unknown default: return nil
        }
    }
    public func characterRange(byExtending position: UITextPosition, in direction: UITextLayoutDirection) -> UITextRange? {
        guard let index = offset(position) else { return nil }
        let text = session.document.text as NSString
        let backward = direction == .left || direction == .up
        guard backward ? index > 0 : index < text.length else { return nil }
        return makeRange(text.rangeOfComposedCharacterSequence(at: backward ? index - 1 : index))
    }
    public func baseWritingDirection(for position: UITextPosition, in direction: UITextStorageDirection) -> NSWritingDirection { .natural }
    public func setBaseWritingDirection(_ writingDirection: NSWritingDirection, for range: UITextRange) {
        // There is no terminal-side writing-direction command here. TextKit
        // derives the marked candidate's Unicode direction from its text.
    }

    private func refreshComposition() {
        let text = session.document.text
        textStorage.setAttributedString(NSAttributedString(string: text, attributes: [
            .font: font, .foregroundColor: foreground,
            .underlineStyle: NSUnderlineStyle.single.rawValue
        ]))
        if session.document.selection.length > 0 {
            textStorage.addAttribute(.backgroundColor, value: UIColor.systemBlue.withAlphaComponent(0.25),
                                     range: session.document.selection)
        }
        updateAccessory(); setNeedsLayout(); setNeedsDisplay()
    }
    public override func layoutSubviews() {
        super.layoutSubviews()
        let x = max(0, min(caret.minX, max(0, bounds.width - 44)))
        compositionOrigin = CGPoint(x: x, y: max(0, min(caret.minY, max(0, bounds.height - font.lineHeight))))
        textContainer.size = CGSize(width: max(44, bounds.width - x), height: .greatestFiniteMagnitude)
    }
    public override func draw(_ rect: CGRect) {
        guard hasGeometry, session.document.isMarked, !session.document.text.isEmpty else { return }
        layoutIfNeeded()
        let visible = rect.offsetBy(dx: -compositionOrigin.x, dy: -compositionOrigin.y)
        let glyphs = layoutManager.glyphRange(forBoundingRect: visible, in: textContainer)
        layoutManager.drawBackground(forGlyphRange: glyphs, at: compositionOrigin)
        layoutManager.drawGlyphs(forGlyphRange: glyphs, at: compositionOrigin)
    }
    public func caretRect(for position: UITextPosition) -> CGRect {
        guard let index = offset(position), hasGeometry else { return .zero }
        guard !session.document.text.isEmpty else { return caret }
        layoutIfNeeded(); layoutManager.ensureLayout(for: textContainer)
        let count = session.document.utf16Count
        let probe = min(index, count - 1)
        let glyph = layoutManager.glyphIndexForCharacter(at: probe)
        let fragment = layoutManager.lineFragmentRect(forGlyphAt: glyph, effectiveRange: nil)
        let slots = layoutManager.getLineFragmentInsertionPoints(forCharacterAt: probe,
            alternatePositions: false, inDisplayOrder: false, positions: nil, characterIndexes: nil)
        // Every insertion point belongs to this bounded composition document.
        guard slots > 0, slots <= count + 1 else { return caret }
        var positions = [CGFloat](repeating: 0, count: slots)
        var indexes = [Int](repeating: 0, count: slots)
        let actual = layoutManager.getLineFragmentInsertionPoints(forCharacterAt: probe,
            alternatePositions: false, inDisplayOrder: false, positions: &positions, characterIndexes: &indexes)
        if actual == slots, let match = indexes.firstIndex(of: index) {
            return CGRect(x: compositionOrigin.x + fragment.minX + positions[match],
                          y: compositionOrigin.y + fragment.minY, width: max(1, caret.width), height: fragment.height)
        }
        // A trailing hard line break has an empty final line fragment.
        if index == count, layoutManager.extraLineFragmentTextContainer === textContainer {
            let extra = layoutManager.extraLineFragmentRect
            return CGRect(x: compositionOrigin.x + extra.minX, y: compositionOrigin.y + extra.minY,
                          width: max(1, caret.width), height: max(font.lineHeight, extra.height))
        }
        return caret
    }
    private func rectangles(_ range: NSRange) -> [CGRect] {
        guard hasGeometry else { return [] }
        if range.length == 0 { return [caretRect(for: makePosition(range.location))] }
        layoutIfNeeded(); layoutManager.ensureLayout(for: textContainer)
        let glyphs = layoutManager.glyphRange(forCharacterRange: range, actualCharacterRange: nil)
        var result: [CGRect] = []
        layoutManager.enumerateEnclosingRects(forGlyphRange: glyphs,
            withinSelectedGlyphRange: NSRange(location: NSNotFound, length: 0), in: textContainer) { rect, stop in
                if result.count >= 512 { stop.pointee = true; return }
                result.append(rect.offsetBy(dx: self.compositionOrigin.x, dy: self.compositionOrigin.y))
            }
        return result
    }
    public func firstRect(for range: UITextRange) -> CGRect {
        guard let r = value(range) else { return .zero }
        return rectangles(r).first ?? .zero
    }
    public func selectionRects(for range: UITextRange) -> [UITextSelectionRect] {
        guard let r = value(range) else { return [] }
        let rects = rectangles(r)
        return rects.enumerated().map { TerminalSelectionRect($0.element, first: $0.offset == 0, last: $0.offset == rects.count - 1) }
    }
    public func closestPosition(to point: CGPoint) -> UITextPosition? {
        guard hasGeometry, point.x.isFinite, point.y.isFinite else { return nil }
        guard !session.document.text.isEmpty else { return makePosition(0) }
        layoutIfNeeded(); layoutManager.ensureLayout(for: textContainer)
        let local = CGPoint(x: point.x - compositionOrigin.x, y: point.y - compositionOrigin.y)
        var fraction: CGFloat = 0
        let index = min(layoutManager.characterIndex(for: local, in: textContainer,
                                                     fractionOfDistanceBetweenInsertionPoints: &fraction), session.document.utf16Count)
        if index == session.document.utf16Count { return makePosition(index) }
        let cluster = (session.document.text as NSString).rangeOfComposedCharacterSequence(at: index)
        return makePosition(fraction > 0.5 ? cluster.location + cluster.length : cluster.location)
    }
    public func closestPosition(to point: CGPoint, within range: UITextRange) -> UITextPosition? {
        guard let r = value(range), let closest = closestPosition(to: point), let p = offset(closest) else { return nil }
        return makePosition(max(r.location, min(p, r.location + r.length)))
    }
    public func characterRange(at point: CGPoint) -> UITextRange? {
        guard let p = closestPosition(to: point), let index = offset(p) else { return nil }
        if index == session.document.utf16Count { return makeRange(NSRange(location: index, length: 0)) }
        return makeRange((session.document.text as NSString).rangeOfComposedCharacterSequence(at: index))
    }

    private func modifiers(_ flags: UIKeyModifierFlags) -> TerminalKeyModifiers {
        var value: TerminalKeyModifiers = []
        if flags.contains(.shift) { value.insert(.shift) }
        if flags.contains(.control) { value.insert(.control) }
        if flags.contains(.alternate) { value.insert(.alt) }
        if flags.contains(.command) { value.insert(.command) }
        if flags.contains(.alphaShift) { value.insert(.capsLock) }
        return value
    }
    public override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        var unhandled = Set<UIPress>()
        for press in presses {
            guard let key = press.key, let usage = UInt16(exactly: key.keyCode.rawValue) else { unhandled.insert(press); continue }
            let mods = modifiers(key.modifierFlags)
            guard TerminalHardwareRouting.destination(usage: usage, modifiers: mods,
                                                       hasMarkedText: session.document.isMarked) == .terminal else {
                unhandled.insert(press); continue
            }
            typed {
                guard physicalKeys[usage] != nil || physicalKeys.count < 32 else { throw TerminalKeyboardError.capacity }
                let value = try TerminalHardwareKey(usage: usage, modifiers: mods.union(accessoryModifiers.modifiers),
                                                    characters: key.charactersIgnoringModifiers)
                try session.key(value, phase: physicalKeys[usage] == nil ? .down : .repeated)
                physicalKeys[usage] = value; accessoryModifiers.consume()
            }
        }
        if !unhandled.isEmpty { super.pressesBegan(unhandled, with: event) }
    }
    private func finishPresses(_ presses: Set<UIPress>) -> Set<UIPress> {
        var unhandled = Set<UIPress>()
        for press in presses {
            guard let key = press.key, let usage = UInt16(exactly: key.keyCode.rawValue) else { unhandled.insert(press); continue }
            if let captured = physicalKeys[usage] {
                do {
                    try session.key(captured, phase: .up)
                    physicalKeys[usage] = nil
                } catch {
                    // A rejected key-up cannot leave the captured key held in
                    // an otherwise active responder. Suspension owns bounded,
                    // source-bound releaseAll cleanup and stops new admission.
                    onFailure?(error); retire()
                }
            } else if TerminalHardwareRouting.destination(usage: usage, modifiers: modifiers(key.modifierFlags),
                                                           hasMarkedText: session.document.isMarked) == .textSystem {
                unhandled.insert(press)
            }
        }
        return unhandled
    }
    public override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let unhandled = finishPresses(presses)
        if !unhandled.isEmpty { super.pressesEnded(unhandled, with: event) }
    }
    public override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let unhandled = finishPresses(presses)
        if !unhandled.isEmpty { super.pressesCancelled(unhandled, with: event) }
    }

    public override func canPerformAction(_ action: Selector, withSender sender: Any?) -> Bool {
        if action == #selector(paste(_:)) {
            return canReviewPaste && !ordinaryInputBlocked && onPasteReview != nil && UIPasteboard.general.hasStrings
        }
        // Terminal transcript selection/copy belongs to the semantic renderer,
        // not this deliberately empty local text document.
        if action == #selector(copy(_:)) { return canCopySelection?() == true }
        return false
    }
    public override func copy(_ sender: Any?) { copySelection?() }
    public override func paste(_ sender: Any?) {
        guard canReviewPaste, !ordinaryInputBlocked, onPasteReview != nil,
              UIPasteboard.general.numberOfItems == 1, UIPasteboard.general.hasStrings else { return }
        // UIPasteboard has no length-only text read. Inspect immediately after
        // this deliberate read; never copy/retain an oversized value locally.
        guard let text = UIPasteboard.general.string else { return }
        requestPasteReview(text: text)
    }
    public func requestPasteReview(text: String) {
        guard canReviewPaste, let onPasteReview, pendingPaste == nil else { return }
        do {
            let review = try TerminalPasteReview(text: text, keyboardID: session.id, focus: session.originalFocus)
            pendingPaste = review; updateAccessory(); onPasteReview(review)
        } catch { onFailure?(error) }
    }
    public func confirmPaste(id: UUID) throws {
        guard !retiring, hasGeometry, let review = pendingPaste, review.id == id,
              review.keyboardID == session.id, review.focus == session.originalFocus else { throw TerminalInputError.stale }
        try session.paste(review); pendingPaste = nil; updateAccessory()
    }
    public func cancelPaste(id: UUID) {
        if pendingPaste?.id == id { pendingPaste = nil; updateAccessory() }
    }

    /// A row of keycaps over the keyboard in the terminal's own colors: the
    /// keys a terminal needs that the software keyboard lacks, and, pinned at
    /// the right edge, the key that puts the keyboard away.
    private func makeAccessory() -> UIInputView {
        let view = UIInputView(frame: CGRect(x: 0, y: 0, width: 0, height: 50), inputViewStyle: .default)
        view.autoresizingMask = [.flexibleWidth]
        accessoryBar = view
        let scroll = UIScrollView(); scroll.showsHorizontalScrollIndicator = false
        let row = UIStackView(); row.axis = .horizontal; row.spacing = 6
        scroll.translatesAutoresizingMaskIntoConstraints = false; row.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(scroll); scroll.addSubview(row)
        func key(_ title: String?, symbol: String? = nil, label: String, action: @escaping @MainActor () -> Void) -> UIButton {
            let value = UIButton(type: .system)
            var configuration = UIButton.Configuration.filled()
            configuration.title = title
            configuration.image = symbol.map { UIImage(systemName: $0) } ?? nil
            configuration.preferredSymbolConfigurationForImage = UIImage.SymbolConfiguration(pointSize: 15, weight: .semibold)
            configuration.titleTextAttributesTransformer = UIConfigurationTextAttributesTransformer { attributes in
                var attributes = attributes
                attributes.font = UIFont.systemFont(ofSize: 16, weight: .semibold)
                return attributes
            }
            configuration.contentInsets = NSDirectionalEdgeInsets(top: 6, leading: 11, bottom: 6, trailing: 11)
            configuration.background.cornerRadius = 8
            configuration.cornerStyle = .fixed
            value.configuration = configuration
            value.accessibilityLabel = label
            value.widthAnchor.constraint(greaterThanOrEqualToConstant: 42).isActive = true
            value.addAction(UIAction { _ in action() }, for: .touchUpInside)
            return value
        }
        let dismiss = key(nil, symbol: "keyboard.chevron.compact.down", label: "Hide keyboard") { [weak self] in
            self?.retire()
        }
        dismiss.accessibilityIdentifier = "terminal.keyboard.hide"
        dismiss.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(dismiss)
        dismissButton = dismiss
        NSLayoutConstraint.activate([
            dismiss.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor, constant: -6),
            dismiss.centerYAnchor.constraint(equalTo: view.centerYAnchor),
            dismiss.heightAnchor.constraint(equalToConstant: 38),
            scroll.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: dismiss.leadingAnchor, constant: -4),
            scroll.topAnchor.constraint(equalTo: view.topAnchor), scroll.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            row.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 6),
            row.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -6),
            row.centerYAnchor.constraint(equalTo: scroll.frameLayoutGuide.centerYAnchor),
            row.topAnchor.constraint(greaterThanOrEqualTo: scroll.contentLayoutGuide.topAnchor),
            row.heightAnchor.constraint(equalToConstant: 38)
        ])
        func add(_ button: UIButton) -> UIButton { row.addArrangedSubview(button); return button }
        controlButton = add(key("ctrl", label: "Control modifier") { [weak self] in
            guard let self, self.acceptsKeys, !self.session.document.isMarked else { return }
            self.accessoryModifiers.cycleControl(); self.updateAccessory()
        })
        altButton = add(key("alt", label: "Alt modifier") { [weak self] in
            guard let self, self.acceptsKeys, !self.session.document.isMarked else { return }
            self.accessoryModifiers.cycleAlt(); self.updateAccessory()
        })
        for (title, symbol, label, usage) in [("esc", nil, "Escape", UInt16(0x29)), ("tab", nil, "Tab", 0x2B),
                                              (nil, "arrow.left", "Left arrow", 0x50), (nil, "arrow.down", "Down arrow", 0x51),
                                              (nil, "arrow.up", "Up arrow", 0x52), (nil, "arrow.right", "Right arrow", 0x4F)] {
            terminalButtons.append(add(key(title, symbol: symbol, label: label) { [weak self] in
                guard let self else { return }
                self.typed {
                    try self.session.key(TerminalHardwareKey(usage: usage, modifiers: self.accessoryModifiers.modifiers), phase: .press)
                    self.accessoryModifiers.consume()
                }
            }))
        }
        // Characters a shell wants that hide on the keyboard's second pages.
        for character in ["~", "|", "/", "-"] {
            terminalButtons.append(add(key(character, label: character) { [weak self] in self?.insertText(character) }))
        }
        updateAccessory(); return view
    }

    /// Colors the key row from the terminal's theme; the row keeps the
    /// system's colors until the first frame names one.
    public func applyTheme(_ theme: TerminalChromeTheme) {
        guard self.theme != theme else { return }
        self.theme = theme
        updateAccessory()
    }

    private func updateAccessory() {
        let enabled = acceptsKeys && !session.document.isMarked
        accessoryBar?.backgroundColor = theme.map { $0.tint(0.06) } ?? .secondarySystemBackground
        let cap = theme.map { $0.tint(0.2) } ?? .secondarySystemFill
        let text = theme?.foreground ?? .label
        let accent = theme?.accent ?? .tintColor
        func style(_ button: UIButton?, active: Bool = false) {
            button?.configuration?.baseBackgroundColor = active ? accent : cap
            button?.configuration?.baseForegroundColor = active ? TerminalChromeTheme.label(on: accent) : text
        }
        for button in terminalButtons { button.isEnabled = enabled; style(button) }
        style(dismissButton)
        func update(_ button: UIButton?, _ state: TerminalAccessoryModifiers.State) {
            button?.isEnabled = enabled
            button?.isSelected = state != .off
            button?.accessibilityValue = switch state { case .off: "Off"; case .oneShot: "Next key"; case .locked: "Locked" }
            style(button, active: state != .off)
            // A locked modifier stays on until tapped again; show the lock.
            button?.configuration?.image = state == .locked ? UIImage(systemName: "lock.fill") : nil
            button?.configuration?.imagePlacement = .trailing
            button?.configuration?.imagePadding = 4
        }
        update(controlButton, accessoryModifiers.control); update(altButton, accessoryModifiers.alt)
    }
}

/// The shell supplies an authenticated session and source-bound renderer caret.
/// It must finish the old session's handoff before changing `session`. Disposal
/// instead follows interruption semantics and discards unsubmitted local input.
@MainActor
public struct NativeTerminalInputSurface: UIViewRepresentable {
    public let session: TerminalKeyboardSession
    public let caret: NativeTerminalCaret
    public var wantsKeyboard: Bool
    public var onFailure: @MainActor (Error) -> Void
    public var onPasteReview: (@MainActor (TerminalPasteReview) -> Void)?
    public var pasteDecision: NativeTerminalPasteDecision?
    @MainActor
    public final class Coordinator {
        private var sessionID: UUID?
        private var generation = UUID()
        private var callback: (@MainActor (Error) -> Void)?
        private var pending: Error?
        private var delivery: Task<Void, Never>?
        fileprivate func configure(sessionID: UUID, callback: @escaping @MainActor (Error) -> Void) {
            if self.sessionID != sessionID { invalidate(); self.sessionID = sessionID }
            self.callback = callback
        }
        fileprivate func report(_ error: Error, sessionID: UUID) {
            guard self.sessionID == sessionID else { return }
            if pending == nil { pending = error }
            guard delivery == nil else { return }
            let captured = generation
            // Exactly one coalesced delivery lives outside SwiftUI's update
            // callback. Repeated failed updates cannot create an error-task queue.
            delivery = Task { [weak self] in
                await Task.yield()
                guard let self, !Task.isCancelled, self.generation == captured else { return }
                let error = self.pending; self.pending = nil; self.delivery = nil
                if let error { self.callback?(error) }
            }
        }
        fileprivate func invalidate() {
            generation = UUID(); delivery?.cancel(); delivery = nil; pending = nil; callback = nil; sessionID = nil
        }
    }
    public init(session: TerminalKeyboardSession, caret: NativeTerminalCaret, wantsKeyboard: Bool,
                onFailure: @escaping @MainActor (Error) -> Void,
                onPasteReview: (@MainActor (TerminalPasteReview) -> Void)? = nil,
                pasteDecision: NativeTerminalPasteDecision? = nil) {
        self.session = session; self.caret = caret; self.wantsKeyboard = wantsKeyboard
        self.onFailure = onFailure; self.onPasteReview = onPasteReview
        self.pasteDecision = pasteDecision
    }
    public func makeCoordinator() -> Coordinator { Coordinator() }
    public func makeUIView(context: Context) -> NativeTerminalInputContainer { NativeTerminalInputContainer() }
    public func updateUIView(_ container: NativeTerminalInputContainer, context: Context) {
        let coordinator = context.coordinator, sessionID = session.id
        coordinator.configure(sessionID: sessionID, callback: onFailure)
        do {
            let input = try container.install(session)
            input.onFailure = { [weak coordinator] error in coordinator?.report(error, sessionID: sessionID) }
            input.onPasteReview = onPasteReview
            try input.updateCaret(caret)
            input.requestKeyboard(wantsKeyboard)
            if let pasteDecision, input.pendingPaste?.id == pasteDecision.id {
                switch pasteDecision {
                case .confirm(let id): try input.confirmPaste(id: id)
                case .cancel(let id): input.cancelPaste(id: id)
                }
            }
        } catch { coordinator.report(error, sessionID: sessionID) }
    }
    public static func dismantleUIView(_ container: NativeTerminalInputContainer, coordinator: Coordinator) {
        coordinator.invalidate(); container.input?.retire()
    }
}

public enum NativeTerminalPasteDecision {
    case confirm(UUID), cancel(UUID)
    fileprivate var id: UUID {
        switch self { case .confirm(let id), .cancel(let id): id }
    }
}

@MainActor
public final class NativeTerminalInputContainer: UIView {
    public private(set) var input: NativeTerminalInputView?
    fileprivate func install(_ session: TerminalKeyboardSession) throws -> NativeTerminalInputView {
        if let input, input.session.id == session.id { return input }
        if let input {
            guard input.session.phase == .retired else { throw TerminalKeyboardError.transitionInProgress }
            input.retire(); input.removeFromSuperview()
        }
        let replacement = try NativeTerminalInputView(session: session)
        replacement.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        replacement.frame = bounds; addSubview(replacement); input = replacement
        return replacement
    }
}

#if canImport(MetalKit)
/// A real terminal display and text responder for one authenticated source.
/// The session driver feeds canonical FTSF snapshots through `submit`; this
/// view neither opens a connection nor substitutes a local ANSI implementation.
/// The displayed frame supplies the responder's caret, in the same coordinates.
@MainActor
public final class NativeTerminalView: UIView {
    public let source: TerminalSource
    public let renderer: NativeTerminalRenderView
    public private(set) var input: NativeTerminalInputView
    public var onFailure: (@MainActor (Error) -> Void)?
    public var onPasteReview: (@MainActor (TerminalPasteReview) -> Void)? {
        didSet { bindPasteReview() }
    }
    /// Resigning or OS deactivation retires the exact input generation. A
    /// session owner can install a fresh focus on the SAME source once cleanup
    /// settles. The display and its authenticated channel remain independent.
    public var onInputRetired: (@MainActor () -> Void)?
    /// The owner applies this measured geometry to the canonical terminal and
    /// its SSH PTY, then submits the corresponding revised semantic frame.
    public var onViewportChange: (@MainActor (NativeTerminalViewport) -> Void)?
    /// The owner selects this history viewport on the canonical model (nil is
    /// the live screen) and submits the frame it produces. A one-finger
    /// vertical pan, the return-to-live control and typing over history ask.
    public var onHistoryViewportRequest: (@MainActor (Int64?) -> Void)?

    /// A drag over a screen without history (a full-screen program's): the
    /// owner turns that program's wheel `steps` notches (negative toward older
    /// output) over cell (column, row). Unset, such a drag does nothing.
    public var onRemoteScroll: (@MainActor (_ steps: Int, _ column: Int, _ row: Int) -> Void)?
    /// The program on screen has asked for the mouse: a tap clicks the cell
    /// under the finger through `onRemoteClick` instead of toggling the
    /// keyboard.
    public var remoteMouseGrabbed = false
    public var onRemoteClick: (@MainActor (_ column: Int, _ row: Int) -> Void)?
    /// Rows of drag per wheel notch: a notch scrolls a program about three rows.
    private static let rowsPerNotch = 3

    private let liveButton = UIButton(type: .system)
    private let panDelegate = NativeTerminalHistoryPanDelegate()
    private var panAnchor: (top: Int64, history: TerminalRenderHistory)?
    private var remoteScroll: (column: Int, row: Int, sent: Int)?
    private var lastHistoryRequest: Int64??
    private var lastCaret: NativeTerminalCaret?
    private var lastAppearance: (font: UIFont, foreground: UIColor)?
    private var retired = false
    private var wantsKeyboard = false
    private var hasActivated = false
    private var sceneActive = false

    /// A tap on the terminal while the keyboard is down: the owner brings a
    /// keyboard back (a tap while it is up puts it away).
    public var onKeyboardRequest: (@MainActor () -> Void)?
    /// The user changed the text size (pinch or ⌘+ / ⌘- / ⌘0); the owner keeps
    /// it for this terminal's later sessions and the others.
    public var onFontSizeChange: (@MainActor (Double) -> Void)?
    /// Each text-size change gives the renderer a new font revision; frames
    /// laid out for an older one are not drawn. The owner must hand this
    /// revision to the terminal model (TerminalModelRuntime.setFontRevision)
    /// so its next frames carry it.
    public var onFontRevision: (@MainActor (UInt64) -> Void)?
    /// Text sizes a pinch or key command can reach.
    public static let fontSizes: ClosedRange<Double> = 9...30
    /// The selected cells, by stable row; nil when nothing is selected.
    public private(set) var selection: TerminalTextSelection?
    public var fontPointSize: Double { renderer.configuration.fontPointSize }

    private let selectionOverlay = NativeTerminalSelectionOverlay()
    private let sizeBadge = UILabel()
    private lazy var editMenu = UIEditMenuInteraction(delegate: self)
    private let feedback = UISelectionFeedbackGenerator()
    /// Rows the selection has covered while on screen, so it copies whole
    /// even after they scroll away. Bounded by TerminalTextSelection.maximumRows.
    private var selectionRows: [Int64: TerminalRenderRow] = [:]
    /// What a long press or handle drag is extending, and from where.
    private enum SelectionDrag { case word(TerminalTextSelection), handle(fixed: TerminalGridPoint, offset: CGPoint) }
    private var drag: SelectionDrag?
    private var dragPoint = CGPoint.zero
    /// Where the drag began: only a finger that moved toward an edge scrolls.
    private var dragOrigin = CGPoint.zero
    /// The width the selection was made at; a new width rewraps its rows.
    private var selectionColumns: Int?
    /// Stable row numbers can be reused on another screen or geometry. Keep
    /// the capture's source witness as well as its width while selecting.
    private var selectionCopyContext: TerminalRowCopyContext?
    private let touchFilter = NativeTerminalTouchFilter()
    private var autoScroll: Timer?
    private var theme: TerminalChromeTheme?
    private var themedPalette: UInt64?
    private var momentum: CADisplayLink?
    private var momentumVelocity = 0.0, momentumTop = 0.0, momentumTime: CFTimeInterval = 0
    private var momentumHistory: TerminalRenderHistory?
    private var pinchStart: Double?
    private var pinchApplied: CFTimeInterval = 0
    /// The fingers' spacing changed enough to be a pinch, not a two-finger swipe.
    private var pinchEngaged = false

    public init(session: TerminalKeyboardSession, bundle: Bundle = .main,
                configuration: NativeTerminalRenderConfiguration = .init()) async throws {
        guard let source = session.originalFocus.source, session.acceptsInput else {
            throw TerminalKeyboardError.unavailable
        }
        let renderer = try await NativeTerminalRenderView(source: source, bundle: bundle,
                                                           configuration: configuration)
        guard !Task.isCancelled, session.acceptsInput else {
            renderer.retire()
            throw TerminalKeyboardError.unavailable
        }
        let input: NativeTerminalInputView
        do { input = try NativeTerminalInputView(session: session) }
        catch { renderer.retire(); throw error }
        self.source = source; self.renderer = renderer; self.input = input
        super.init(frame: .zero)
        clipsToBounds = true
        renderer.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        input.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        addSubview(renderer); addSubview(input); addSubview(selectionOverlay)
        bindInput()
        // One finger drags through history with momentum; a trackpad or
        // mouse wheel scrolls it too.
        let pan = UIPanGestureRecognizer(target: self, action: #selector(panned(_:)))
        pan.maximumNumberOfTouches = 1; pan.delegate = panDelegate
        pan.allowedScrollTypesMask = .all
        addGestureRecognizer(pan)
        // Press and hold selects a word; keep dragging to select more. A
        // selection handle's touches are its own drag's.
        let press = UILongPressGestureRecognizer(target: self, action: #selector(pressed(_:)))
        press.minimumPressDuration = 0.35; press.delegate = touchFilter
        addGestureRecognizer(press)
        // A tap clears a selection, else shows or hides the keyboard.
        let tap = UITapGestureRecognizer(target: self, action: #selector(tapped(_:)))
        tap.delegate = touchFilter
        addGestureRecognizer(tap)
        addGestureRecognizer(UIPinchGestureRecognizer(target: self, action: #selector(pinched(_:))))
        for handle in [selectionOverlay.startHandle, selectionOverlay.endHandle] {
            handle.addGestureRecognizer(UIPanGestureRecognizer(target: self, action: #selector(handleDragged(_:))))
        }
        addInteraction(editMenu)
        sizeBadge.font = .monospacedDigitSystemFont(ofSize: 17, weight: .semibold)
        sizeBadge.textAlignment = .center; sizeBadge.textColor = .white
        sizeBadge.backgroundColor = UIColor.black.withAlphaComponent(0.6)
        sizeBadge.layer.cornerRadius = 12; sizeBadge.layer.masksToBounds = true
        sizeBadge.alpha = 0; sizeBadge.isUserInteractionEnabled = false
        addSubview(sizeBadge)
        var button = UIButton.Configuration.filled()
        button.image = UIImage(systemName: "arrow.down.to.line"); button.cornerStyle = .capsule
        liveButton.configuration = button
        liveButton.accessibilityLabel = "Return to live output"
        liveButton.isHidden = true
        liveButton.addAction(UIAction { [weak self] _ in self?.returnToLiveOutput() }, for: .touchUpInside)
        addSubview(liveButton)
        renderer.onFrameAdmitted = { [weak self] frame in
            guard let self, !self.retired else { return }
            self.liveButton.isHidden = frame.isLive
            self.frameAdmitted(frame)
        }
        renderer.onFailure = { [weak self] error in
            guard let self, !self.retired else { return }
            self.onFailure?(error)
        }
        renderer.onAppearance = { [weak self] font, foreground in
            guard let self, !self.retired, self.sceneActive else { return }
            self.lastAppearance = (font, foreground)
            self.input.updateAppearance(font: font, foreground: foreground)
        }
        renderer.onCaretUnavailable = { [weak self] in
            guard let self, !self.retired else { return }
            self.lastCaret = nil
            do { try self.input.invalidateRenderedCaret(source: self.source) }
            catch { self.onFailure?(error) }
        }
        renderer.onViewportChange = { [weak self] viewport in
            guard let self, !self.retired, viewport.source == self.source else { return }
            self.onViewportChange?(viewport)
        }
        renderer.onCaret = { [weak self] caret in
            guard let self, !self.retired, caret.source == self.source else { return }
            self.lastCaret = caret
            guard self.sceneActive, self.input.session.acceptsInput else { return }
            do {
                try self.input.updateCaret(caret)
                self.input.requestKeyboard(self.wantsKeyboard)
            } catch { self.onFailure?(error) }
        }
        NotificationCenter.default.addObserver(self, selector: #selector(sceneStateChanged(_:)),
                                               name: UIScene.didActivateNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(sceneStateChanged(_:)),
                                               name: UIScene.willDeactivateNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(applicationStateChanged(_:)),
                                               name: UIApplication.didBecomeActiveNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(applicationStateChanged(_:)),
                                               name: UIApplication.willResignActiveNotification, object: nil)
    }
    @available(*, unavailable)
    public required init?(coder: NSCoder) { fatalError("Use init(session:bundle:configuration:)") }
    deinit { NotificationCenter.default.removeObserver(self) }

    private func bindInput() {
        input.onFailure = { [weak self] error in
            guard let self, !self.retired else { return }
            self.onFailure?(error)
        }
        bindPasteReview()
        input.canCopySelection = { [weak self] in self?.selection != nil }
        input.copySelection = { [weak self] in self?.copySelection() }
        if let theme { input.applyTheme(theme) }
        input.onRetired = { [weak self] in
            guard let self, !self.retired else { return }
            self.wantsKeyboard = false
            self.onInputRetired?()
        }
        input.onInputWithoutProjection = { [weak self] in
            guard let self, !self.retired, self.sceneActive, let frame = self.renderer.admittedFrame else { return false }
            // A resize or new text size redraws the live screen it shows.
            if frame.isLive { return true }
            self.returnToLiveOutput()
            return false
        }
    }

    /// Moves the shown rows by `rows` (negative is older history), within the
    /// history range of the last admitted frame.
    public func scrollHistory(byRows rows: Int) {
        guard !retired, let frame = renderer.admittedFrame, let history = frame.history else { return }
        let top = min(max(frame.viewportFirst + Int64(rows), history.first), history.live)
        // At either end there is nothing further to show, and no frame to ask for.
        guard top != frame.viewportFirst else { return }
        lastHistoryRequest = nil
        requestHistory(top: top, within: history)
    }
    public func returnToLiveOutput() {
        guard !retired else { return }
        stopMomentum()
        lastHistoryRequest = .some(nil)
        onHistoryViewportRequest?(nil)
    }
    private func requestHistory(top: Int64, within history: TerminalRenderHistory) {
        let clamped = min(max(top, history.first), history.live)
        let target = clamped == history.live ? nil : clamped
        guard lastHistoryRequest != .some(target) else { return }
        lastHistoryRequest = .some(target)
        onHistoryViewportRequest?(target)
    }
    @objc private func panned(_ pan: UIPanGestureRecognizer) {
        guard !retired else { return }
        let rowHeight = renderer.cellSize?.height ?? 0
        switch pan.state {
        case .began:
            stopMomentum()
            lastHistoryRequest = nil
            panAnchor = nil; remoteScroll = nil
            guard let frame = renderer.admittedFrame else { return }
            if let history = frame.history, history.first < history.live {
                panAnchor = (frame.viewportFirst, history)
            } else if onRemoteScroll != nil, let cell = renderer.cellSize, cell.width > 0, cell.height > 0 {
                // Nothing to scroll here: the program itself may scroll.
                let point = pan.location(in: self)
                remoteScroll = (min(max(Int(point.x / cell.width), 0), max(frame.columns - 1, 0)),
                                min(max(Int(point.y / cell.height), 0), max(frame.lines.count - 1, 0)), 0)
            }
        case .changed:
            if let remote = remoteScroll, rowHeight > 0 {
                // Dragging down shows older rows: the wheel turns up.
                let rows = Int((pan.translation(in: self).y / rowHeight).rounded(.towardZero))
                let notches = -rows / Self.rowsPerNotch
                if notches != remote.sent {
                    onRemoteScroll?(notches - remote.sent, remote.column, remote.row)
                    remoteScroll?.sent = notches
                }
                return
            }
            guard let anchor = panAnchor, rowHeight > 0 else { return }
            // Dragging down reveals older rows above, one row per row height.
            let moved = pan.translation(in: self).y / rowHeight
            requestHistory(top: anchor.top - Int64(moved.rounded(.towardZero)), within: anchor.history)
        case .ended:
            // A flick keeps going and slows like any iOS list.
            if let anchor = panAnchor, rowHeight > 0 {
                let top = Double(anchor.top) - Double(pan.translation(in: self).y / rowHeight)
                startMomentum(top: top, velocity: Double(pan.velocity(in: self).y / rowHeight), history: anchor.history)
            }
            panAnchor = nil; remoteScroll = nil
        default: panAnchor = nil; remoteScroll = nil
        }
    }

    // MARK: Momentum

    /// The display link's target, on the main run loop; it holds the view weakly.
    @MainActor
    private final class Ticker: NSObject {
        weak var view: NativeTerminalView?
        @objc func tick(_ link: CADisplayLink) { view?.momentumTick(link) }
    }

    /// `velocity` is rows per second, positive toward older rows.
    private func startMomentum(top: Double, velocity: Double, history: TerminalRenderHistory) {
        stopMomentum()
        guard abs(velocity) > 4 else { return }
        momentumTop = top; momentumVelocity = velocity; momentumHistory = history
        momentumTime = CACurrentMediaTime()
        let ticker = Ticker(); ticker.view = self
        let link = CADisplayLink(target: ticker, selector: #selector(Ticker.tick(_:)))
        link.add(to: .main, forMode: .common)
        momentum = link
    }

    private func stopMomentum() {
        momentum?.invalidate(); momentum = nil; momentumHistory = nil
    }

    fileprivate func momentumTick(_ link: CADisplayLink) {
        guard !retired, let history = momentumHistory else { stopMomentum(); return }
        let now = link.timestamp, elapsed = min(max(now - momentumTime, 0), 1.0 / 30)
        momentumTime = now
        momentumTop -= momentumVelocity * elapsed
        // UIScrollView's normal deceleration: 0.998 per millisecond.
        momentumVelocity *= pow(0.998, elapsed * 1000)
        let clamped = min(max(momentumTop, Double(history.first)), Double(history.live))
        requestHistory(top: Int64(clamped.rounded()), within: history)
        if abs(momentumVelocity) < 2 || clamped != momentumTop { stopMomentum() }
    }

    // MARK: Taps, text size

    @objc private func tapped(_ tap: UITapGestureRecognizer) {
        guard !retired else { return }
        stopMomentum()
        if selection != nil { clearSelection(); return }
        // A program that asked for the mouse takes the tap as a click; the
        // keyboard has its own button then.
        if remoteMouseGrabbed, let onRemoteClick, let cell = renderer.cellSize, cell.width > 0, cell.height > 0,
           let frame = renderer.admittedFrame, frame.isLive {
            let point = tap.location(in: self)
            onRemoteClick(min(max(Int(point.x / cell.width), 0), max(frame.columns - 1, 0)),
                          min(max(Int(point.y / cell.height), 0), max(frame.lines.count - 1, 0)))
            return
        }
        if !input.isFirstResponder { onKeyboardRequest?() }
        // With a hardware keyboard there is no on-screen keyboard to put
        // away, and resigning would only stop the typing.
        else if GCKeyboard.coalesced == nil { input.retire() }
    }

    @objc private func pinched(_ pinch: UIPinchGestureRecognizer) {
        guard !retired else { return }
        switch pinch.state {
        case .began:
            stopMomentum(); clearSelection()
            pinchStart = fontPointSize; pinchEngaged = false
        case .changed:
            guard let start = pinchStart, pinch.scale > 0 else { return }
            // Fingers swiping sideways drift a little apart or together; only
            // a clear change of spacing is a pinch.
            guard pinchEngaged || abs(log(Double(pinch.scale))) > 0.08 else { return }
            pinchEngaged = true
            let size = Self.clampedFontSize(start * Double(pinch.scale))
            showSizeBadge(size)
            // Each size change re-lays out the terminal (and resizes the remote
            // pane); a few per second keep the pinch live without a storm.
            let now = CACurrentMediaTime()
            if now - pinchApplied > 0.12 { pinchApplied = now; applyFontSize(size, notify: false) }
        case .ended:
            if pinchEngaged, let start = pinchStart, pinch.scale > 0 {
                applyFontSize(start * Double(pinch.scale), notify: true)
            }
            pinchStart = nil; pinchEngaged = false; hideSizeBadge()
        default:
            // Interrupted: keep the size shown so far, everywhere.
            if pinchEngaged { applyFontSize(fontPointSize, notify: true) }
            pinchStart = nil; pinchEngaged = false; hideSizeBadge()
        }
    }

    private static func clampedFontSize(_ size: Double) -> Double {
        let half = (size * 2).rounded() / 2
        return min(max(half, fontSizes.lowerBound), fontSizes.upperBound)
    }

    /// Shows this terminal's text at `size` points (half-point steps within
    /// `fontSizes`); the owner applies the cells that now fit, as on rotation.
    public func setFontPointSize(_ size: Double) { applyFontSize(size, notify: false) }

    private func applyFontSize(_ size: Double, notify: Bool) {
        guard !retired else { return }
        let clamped = Self.clampedFontSize(size)
        var configuration = renderer.configuration
        if configuration.fontPointSize != clamped {
            clearSelection()
            configuration.fontPointSize = clamped
            configuration.fontRevision += 1
            do {
                try renderer.updateConfiguration(configuration)
                onFontRevision?(configuration.fontRevision)
            } catch { onFailure?(error) }
        }
        if notify { onFontSizeChange?(clamped) }
    }

    private func showSizeBadge(_ size: Double) {
        sizeBadge.text = size == size.rounded() ? "\(Int(size)) pt" : String(format: "%.1f pt", size)
        bringSubviewToFront(sizeBadge)
        UIView.animate(withDuration: 0.12) { self.sizeBadge.alpha = 1 }
    }
    private func hideSizeBadge() {
        UIView.animate(withDuration: 0.4, delay: 0.5) { self.sizeBadge.alpha = 0 }
    }

    public override var keyCommands: [UIKeyCommand]? {
        [UIKeyCommand(title: "Bigger Text", action: #selector(biggerText), input: "+", modifierFlags: .command),
         UIKeyCommand(input: "=", modifierFlags: .command, action: #selector(biggerText)),
         UIKeyCommand(title: "Smaller Text", action: #selector(smallerText), input: "-", modifierFlags: .command),
         UIKeyCommand(title: "Actual Size", action: #selector(actualSize), input: "0", modifierFlags: .command)]
    }
    @objc public func biggerText() { applyFontSize(fontPointSize + 1, notify: true) }
    @objc public func smallerText() { applyFontSize(fontPointSize - 1, notify: true) }
    @objc public func actualSize() { applyFontSize(NativeTerminalRenderConfiguration().fontPointSize, notify: true) }

    // MARK: Selection

    /// The cell under `point`, by stable row; points past the edges clamp.
    private func gridPoint(at point: CGPoint) -> TerminalGridPoint? {
        guard let frame = renderer.admittedFrame, let cell = renderer.cellSize,
              cell.width > 0, cell.height > 0, !frame.lines.isEmpty else { return nil }
        let column = min(max(Int(point.x / cell.width), 0), frame.columns - 1)
        let index = min(max(Int(point.y / cell.height), 0), frame.lines.count - 1)
        return TerminalGridPoint(row: frame.lines[index].stableIndex, column: column)
    }

    @objc private func pressed(_ press: UILongPressGestureRecognizer) {
        guard !retired else { return }
        let point = press.location(in: self)
        dragPoint = point
        switch press.state {
        case .began:
            stopMomentum()
            guard let grid = gridPoint(at: point), let frame = renderer.admittedFrame,
                  let row = frame.lines.first(where: { $0.stableIndex == grid.row }) else { return }
            editMenu.dismissMenu()
            dragOrigin = point
            let word = TerminalTextSelection.word(at: grid.column, in: row, columns: frame.columns)
            drag = .word(word)
            setSelection(word)
            feedback.selectionChanged()
            startAutoScroll()
        case .changed:
            extendDrag()
        case .ended:
            finishDrag()
        default:
            finishDrag()
        }
    }

    @objc private func handleDragged(_ pan: UIPanGestureRecognizer) {
        guard !retired, let handle = pan.view as? NativeTerminalSelectionOverlay.SelectionHandle,
              let current = selection, let cell = renderer.cellSize, let frame = renderer.admittedFrame else { return }
        let point = pan.location(in: self)
        dragPoint = point
        switch pan.state {
        case .began:
            editMenu.dismissMenu()
            dragOrigin = point
            // Keep the finger's offset from the cell it grabbed, so the end
            // under the knob does not jump to the knob's position.
            let moving = handle.atStart ? current.start : current.end
            let index = frame.lines.firstIndex { $0.stableIndex == moving.row } ?? 0
            let center = CGPoint(x: (CGFloat(moving.column) + 0.5) * cell.width, y: (CGFloat(index) + 0.5) * cell.height)
            drag = .handle(fixed: handle.atStart ? current.end : current.start,
                           offset: CGPoint(x: center.x - point.x, y: center.y - point.y))
            startAutoScroll()
        case .changed: extendDrag()
        default: finishDrag()
        }
    }

    private func extendDrag() {
        guard let drag else { return }
        switch drag {
        case .word(let word):
            guard let grid = gridPoint(at: dragPoint) else { return }
            let next = grid < word.start ? TerminalTextSelection(grid, word.end)
                                         : TerminalTextSelection(word.start, max(grid, word.end))
            if next != selection { setSelection(next); feedback.selectionChanged() }
        case let .handle(fixed, offset):
            guard let grid = gridPoint(at: CGPoint(x: dragPoint.x + offset.x, y: dragPoint.y + offset.y)) else { return }
            let next = TerminalTextSelection(fixed, grid)
            if next != selection { setSelection(next); feedback.selectionChanged() }
        }
    }

    private func finishDrag() {
        autoScroll?.invalidate(); autoScroll = nil
        guard drag != nil else { return }
        drag = nil
        showEditMenu()
    }

    /// Holding a selection drag near the top or bottom edge scrolls history,
    /// and the selection follows the rows that pass under the finger.
    private func startAutoScroll() {
        autoScroll?.invalidate()
        autoScroll = Timer.scheduledTimer(withTimeInterval: 0.08, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.autoScrollTick() }
        }
    }
    private func autoScrollTick() {
        guard drag != nil, let cell = renderer.cellSize else { return }
        let edge = max(cell.height, 24)
        // A press that starts at an edge selects there; the finger has to
        // move toward the edge to scroll.
        if dragPoint.y < edge, dragPoint.y < dragOrigin.y - cell.height / 2 { scrollHistory(byRows: -1) }
        else if dragPoint.y > bounds.height - edge, dragPoint.y > dragOrigin.y + cell.height / 2 { scrollHistory(byRows: 1) }
    }

    private func setSelection(_ value: TerminalTextSelection?) {
        if value != nil {
            guard let frame = renderer.admittedFrame, frame.flags & 8 == 0 else { return }
        }
        selection = value
        if value == nil { selectionRows.removeAll(); selectionColumns = nil; selectionCopyContext = nil }
        else {
            if selectionColumns == nil {
                selectionColumns = renderer.admittedFrame?.columns
                selectionCopyContext = renderer.admittedFrame?.lines.first?.copyContext
            }
            captureSelectionRows()
        }
        drawSelection()
    }

    public func clearSelection() {
        guard selection != nil || drag != nil else { return }
        drag = nil; autoScroll?.invalidate(); autoScroll = nil
        editMenu.dismissMenu()
        setSelection(nil)
    }

    /// Selects every row on screen.
    public func selectScreen() {
        guard let frame = renderer.admittedFrame, frame.flags & 8 == 0,
              let first = frame.lines.first, let last = frame.lines.last else { return }
        setSelection(TerminalTextSelection(TerminalGridPoint(row: first.stableIndex, column: 0),
                                           TerminalGridPoint(row: last.stableIndex, column: frame.columns - 1)))
        showEditMenu()
    }

    /// Copies the selected text as it reads on screen, then clears the
    /// selection. Like the Select Output panel's copies, it stays on this
    /// device and expires after two minutes.
    public func copySelection() {
        guard let selection, let frame = renderer.admittedFrame, frame.flags & 8 == 0 else { return }
        captureSelectionRows()
        let text = selection.text(rows: selectionRows, columns: frame.columns)
        guard !text.isEmpty, text.utf8.count <= 1 << 20 else { return }
        UIPasteboard.general.setItems([["public.utf8-plain-text": text]],
            options: [.localOnly: true, .expirationDate: Date().addingTimeInterval(120)])
        UINotificationFeedbackGenerator().notificationOccurred(.success)
        clearSelection()
    }

    private func captureSelectionRows() {
        guard let selection, let frame = renderer.admittedFrame, frame.flags & 8 == 0 else { return }
        for row in frame.lines where row.stableIndex >= selection.start.row && row.stableIndex <= selection.end.row {
            selectionRows[row.stableIndex] = row
        }
        if selectionRows.count > Int(TerminalTextSelection.maximumRows) {
            selectionRows = selectionRows.filter { $0.key >= selection.start.row && $0.key <= selection.end.row }
        }
    }

    private func drawSelection() {
        selectionOverlay.show(selection, frame: renderer.admittedFrame, cell: renderer.cellSize,
                              tint: theme?.accent ?? tintColor)
    }

    private func showEditMenu() {
        guard selection != nil, let bounds = selectionOverlay.selectionBounds else { return }
        let visible = bounds.intersection(self.bounds)
        let anchor = visible.isNull ? CGPoint(x: self.bounds.midX, y: self.bounds.midY) : CGPoint(x: visible.midX, y: visible.minY)
        editMenu.presentEditMenu(with: UIEditMenuConfiguration(identifier: nil, sourcePoint: anchor))
    }

    /// Each admitted frame recolors the chrome when the palette changed, and
    /// moves a selection with its rows as output or scrolling shifts them.
    private func frameAdmitted(_ frame: TerminalRenderFrame) {
        if themedPalette != frame.paletteRevision || theme == nil, let next = TerminalChromeTheme(frame: frame) {
            themedPalette = frame.paletteRevision
            if next != theme {
                theme = next
                input.applyTheme(next)
                liveButton.configuration?.baseBackgroundColor = next.accent
                liveButton.configuration?.baseForegroundColor = TerminalChromeTheme.label(on: next.accent)
            }
        }
        guard selection != nil else { return }
        // A new source, screen or geometry no longer holds the selected cells,
        // even when it happens to have the same number of columns.
        if let columns = selectionColumns, columns != frame.columns { clearSelection(); return }
        if let context = selectionCopyContext,
           context.source != frame.source || context.geometryRevision != frame.geometryRevision
            || context.alternateScreen != frame.alternateScreen { clearSelection(); return }
        captureSelectionRows()
        if drag != nil { extendDrag() }
        drawSelection()
    }
    private func bindPasteReview() {
        guard !retired, onPasteReview != nil else {
            input.onPasteReview = nil
            if let pending = input.pendingPaste { input.cancelPaste(id: pending.id) }
            return
        }
        input.onPasteReview = { [weak self] review in
            guard let self, !self.retired, review.keyboardID == self.input.session.id else { return }
            self.onPasteReview?(review)
        }
    }
    public override func layoutSubviews() {
        super.layoutSubviews()
        renderer.frame = bounds; input.frame = bounds; selectionOverlay.frame = bounds
        liveButton.frame = CGRect(x: bounds.maxX - 60, y: bounds.maxY - 60, width: 48, height: 48)
        sizeBadge.frame = CGRect(x: bounds.midX - 50, y: bounds.midY - 20, width: 100, height: 40)
        drawSelection()
    }
    public override func didMoveToWindow() {
        super.didMoveToWindow()
        updateSceneActivity()
    }
    @objc private func sceneStateChanged(_ notification: Notification) {
        guard let scene = notification.object as? UIScene, scene === window?.windowScene else { return }
        // willDeactivate arrives before activationState necessarily changes.
        if notification.name == UIScene.willDeactivateNotification { setSceneActive(false) }
        else { updateSceneActivity() }
    }
    @objc private func applicationStateChanged(_ notification: Notification) {
        if notification.name == UIApplication.willResignActiveNotification { setSceneActive(false) }
        else { updateSceneActivity() }
    }
    private func updateSceneActivity() {
        setSceneActive(window?.windowScene?.activationState == .foregroundActive
                       && UIApplication.shared.applicationState == .active)
    }
    /// Inactivity stops frame submission and discards unsubmitted composition.
    /// Resumption never revives an old keyboard focus or reconnects a transport.
    public func setSceneActive(_ active: Bool) {
        guard !retired else { return }
        sceneActive = active
        renderer.setSceneActive(active)
        if active { hasActivated = true }
        else if hasActivated { wantsKeyboard = false; input.retire() }
    }
    public func submit(_ bytes: Data, source: TerminalSource) throws {
        guard !retired else { throw TerminalInputError.unavailable }
        guard source == self.source else { throw TerminalInputError.stale }
        try renderer.submit(bytes, source: source)
    }
    public func requestKeyboard(_ visible: Bool) {
        guard !retired else { return }
        wantsKeyboard = visible
        if !visible || sceneActive { input.requestKeyboard(visible) }
    }
    /// The caller owns the coordinator and creates the replacement focus only
    /// after the previous generation's asynchronous cleanup has settled.
    public func replaceInputSession(_ session: TerminalKeyboardSession) throws {
        guard !retired, session.acceptsInput else { throw TerminalKeyboardError.unavailable }
        guard session.originalFocus.source == source else { throw TerminalInputError.stale }
        guard input.session.phase == .retired else { throw TerminalKeyboardError.transitionInProgress }
        let replacement = try NativeTerminalInputView(session: session)
        if let appearance = lastAppearance {
            replacement.updateAppearance(font: appearance.font, foreground: appearance.foreground)
        }
        do { if let lastCaret { try replacement.updateCaret(lastCaret) } }
        catch { replacement.retire(); throw error }
        replacement.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        replacement.frame = bounds
        input.onFailure = nil; input.onPasteReview = nil; input.onRetired = nil; input.onInputWithoutProjection = nil
        input.removeFromSuperview(); input = replacement
        bindInput(); insertSubview(replacement, aboveSubview: renderer)
    }
    public func confirmPaste(id: UUID) throws {
        guard !retired else { throw TerminalInputError.unavailable }
        try input.confirmPaste(id: id)
    }
    public func cancelPaste(id: UUID) { input.cancelPaste(id: id) }
    /// Retains no authority to replay input on another transport. The session
    /// owner separately closes its channel and settles admitted input receipts.
    public func retire() {
        guard !retired else { return }
        clearSelection(); stopMomentum()
        onKeyboardRequest = nil; onFontSizeChange = nil; onFontRevision = nil; onRemoteScroll = nil; onRemoteClick = nil
        retired = true; wantsKeyboard = false; sceneActive = false; lastCaret = nil; lastAppearance = nil
        input.canCopySelection = nil; input.copySelection = nil
        input.onFailure = nil; input.onPasteReview = nil; input.onRetired = nil; input.onInputWithoutProjection = nil
        onHistoryViewportRequest = nil; liveButton.isHidden = true
        renderer.onFailure = nil; renderer.onCaret = nil; renderer.onAppearance = nil
        renderer.onCaretUnavailable = nil; renderer.onViewportChange = nil; onViewportChange = nil
        input.retire(); renderer.retire()
    }
}

/// The selection's menu: Copy, or select the whole screen.
extension NativeTerminalView: @preconcurrency UIEditMenuInteractionDelegate {
    public func editMenuInteraction(_ interaction: UIEditMenuInteraction, menuFor configuration: UIEditMenuConfiguration,
                                    suggestedActions: [UIMenuElement]) -> UIMenu? {
        guard selection != nil else { return nil }
        return UIMenu(children: [
            UIAction(title: "Copy", image: UIImage(systemName: "doc.on.doc")) { [weak self] _ in self?.copySelection() },
            UIAction(title: "Select All", image: UIImage(systemName: "selection.pin.in.out")) { [weak self] _ in
                self?.selectScreen()
            }
        ])
    }
    public func editMenuInteraction(_ interaction: UIEditMenuInteraction,
                                    targetRectFor configuration: UIEditMenuConfiguration) -> CGRect {
        selectionOverlay.selectionBounds?.intersection(bounds) ?? .null
    }
}

/// A selection handle's touches belong to its own drag: a press or tap on a
/// handle neither starts a new selection nor clears the current one.
@MainActor
private final class NativeTerminalTouchFilter: NSObject, UIGestureRecognizerDelegate {
    func gestureRecognizer(_ recognizer: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
        !(touch.view is NativeTerminalSelectionOverlay.SelectionHandle)
    }
}

/// Only a mostly vertical drag scrolls history; sideways drags stay free.
@MainActor
private final class NativeTerminalHistoryPanDelegate: NSObject, UIGestureRecognizerDelegate {
    func gestureRecognizerShouldBegin(_ recognizer: UIGestureRecognizer) -> Bool {
        guard let pan = recognizer as? UIPanGestureRecognizer, let view = pan.view else { return false }
        let velocity = pan.velocity(in: view)
        return abs(velocity.y) > abs(velocity.x)
    }
}
#endif
#endif
