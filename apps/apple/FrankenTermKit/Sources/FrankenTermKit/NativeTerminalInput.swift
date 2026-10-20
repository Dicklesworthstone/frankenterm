#if canImport(UIKit)
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
    public private(set) var accessoryModifiers = TerminalAccessoryModifiers()
    public private(set) var pendingPaste: TerminalPasteReview?

    public weak var inputDelegate: (any UITextInputDelegate)?
    public lazy var tokenizer: any UITextInputTokenizer = UITextInputStringTokenizer(textInput: self)
    public var markedTextStyle: [NSAttributedString.Key: Any]?
    public var selectionAffinity: UITextStorageDirection = .forward
    public var textInputView: UIView { self }
    public var isEditable: Bool { session.acceptsInput && hasGeometry && !retiring }
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
    private var controlButton: UIButton?
    private var altButton: UIButton?
    private var terminalButtons: [UIButton] = []
    private lazy var accessory = makeAccessory()

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
    public var hasText: Bool { session.acceptsInput }

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
    public func invalidateRenderedCaret(source: TerminalSource) throws {
        guard source == session.originalFocus.source else { throw TerminalInputError.stale }
        guard hasGeometry else { return }
        inputDelegate?.selectionWillChange(self)
        hasGeometry = false; pendingPaste = nil; accessoryModifiers.clear()
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
        guard !retiring, hasGeometry else { return }
        do { try operation(); refreshComposition() }
        catch { onFailure?(error) }
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
        mutate {
            try session.setMarkedText(markedText, selectedRange: selectedRange)
            accessoryModifiers.clear()
        }
    }
    public func unmarkText() { mutate { try session.unmarkText() } }
    public func insertText(_ text: String) {
        mutate {
            if !session.document.isMarked,
               !accessoryModifiers.modifiers.isEmpty || ["\n", "\r", "\t"].contains(text) {
                let key = try TerminalHardwareRouting.accessoryKey(text: text, modifiers: accessoryModifiers.modifiers)
                try session.key(key, phase: .press); accessoryModifiers.consume()
            } else { try session.insertText(text) }
        }
    }
    public func deleteBackward() {
        mutate {
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
            mutate {
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
            return !retiring && hasGeometry && session.acceptsPaste && onPasteReview != nil && pendingPaste == nil && UIPasteboard.general.hasStrings
        }
        // Terminal transcript selection/copy belongs to the semantic renderer,
        // not this deliberately empty local text document.
        return false
    }
    public override func paste(_ sender: Any?) {
        guard !retiring, hasGeometry, session.acceptsPaste, onPasteReview != nil, pendingPaste == nil,
              UIPasteboard.general.numberOfItems == 1, UIPasteboard.general.hasStrings else { return }
        // UIPasteboard has no length-only text read. Inspect immediately after
        // this deliberate read; never copy/retain an oversized value locally.
        guard let text = UIPasteboard.general.string else { return }
        requestPasteReview(text: text)
    }
    public func requestPasteReview(text: String) {
        guard !retiring, hasGeometry, session.acceptsPaste, let onPasteReview, pendingPaste == nil else { return }
        do {
            let review = try TerminalPasteReview(text: text, keyboardID: session.id, focus: session.originalFocus)
            pendingPaste = review; onPasteReview(review)
        } catch { onFailure?(error) }
    }
    public func confirmPaste(id: UUID) throws {
        guard !retiring, hasGeometry, let review = pendingPaste, review.id == id,
              review.keyboardID == session.id, review.focus == session.originalFocus else { throw TerminalInputError.stale }
        try session.paste(review); pendingPaste = nil
    }
    public func cancelPaste(id: UUID) {
        if pendingPaste?.id == id { pendingPaste = nil }
    }

    private func makeAccessory() -> UIInputView {
        let view = UIInputView(frame: CGRect(x: 0, y: 0, width: 0, height: 48), inputViewStyle: .keyboard)
        view.autoresizingMask = [.flexibleWidth]
        let scroll = UIScrollView(); scroll.showsHorizontalScrollIndicator = false
        let row = UIStackView(); row.axis = .horizontal; row.spacing = 5
        scroll.translatesAutoresizingMaskIntoConstraints = false; row.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(scroll); scroll.addSubview(row)
        NSLayoutConstraint.activate([
            scroll.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor),
            scroll.topAnchor.constraint(equalTo: view.topAnchor), scroll.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            row.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 6),
            row.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -6),
            row.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor, constant: 2),
            row.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor, constant: -2),
            row.heightAnchor.constraint(equalTo: scroll.frameLayoutGuide.heightAnchor, constant: -4)
        ])
        func button(_ title: String, label: String, action: @escaping @MainActor () -> Void) -> UIButton {
            let value = UIButton(type: .system)
            var configuration = UIButton.Configuration.gray(); configuration.title = title
            configuration.cornerStyle = .medium; value.configuration = configuration
            value.accessibilityLabel = label
            value.widthAnchor.constraint(greaterThanOrEqualToConstant: 44).isActive = true
            value.addAction(UIAction { _ in action() }, for: .touchUpInside)
            row.addArrangedSubview(value); return value
        }
        controlButton = button("Ctrl", label: "Control modifier") { [weak self] in
            guard let self, self.isEditable, !self.session.document.isMarked else { return }
            self.accessoryModifiers.cycleControl(); self.updateAccessory()
        }
        altButton = button("Alt", label: "Alt modifier") { [weak self] in
            guard let self, self.isEditable, !self.session.document.isMarked else { return }
            self.accessoryModifiers.cycleAlt(); self.updateAccessory()
        }
        for (title, label, usage) in [("Esc", "Escape", UInt16(0x29)), ("Tab", "Tab", 0x2B),
                                      ("←", "Left arrow", 0x50), ("↓", "Down arrow", 0x51),
                                      ("↑", "Up arrow", 0x52), ("→", "Right arrow", 0x4F)] {
            terminalButtons.append(button(title, label: label) { [weak self] in
                guard let self else { return }
                self.mutate {
                    try self.session.key(TerminalHardwareKey(usage: usage, modifiers: self.accessoryModifiers.modifiers), phase: .press)
                    self.accessoryModifiers.consume()
                }
            })
        }
        _ = button("⌄", label: "Dismiss keyboard") { [weak self] in self?.retire() }
        updateAccessory(); return view
    }
    private func updateAccessory() {
        let enabled = !retiring && hasGeometry && session.acceptsInput && !session.document.isMarked
        for button in terminalButtons { button.isEnabled = enabled }
        func update(_ button: UIButton?, _ state: TerminalAccessoryModifiers.State) {
            button?.isEnabled = enabled
            button?.isSelected = state != .off
            button?.accessibilityValue = switch state { case .off: "Off"; case .oneShot: "Next key"; case .locked: "Locked" }
            button?.configuration?.baseBackgroundColor = state == .off ? nil : UIColor.systemBlue
            button?.configuration?.subtitle = state == .locked ? "Locked" : nil
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

    private var lastCaret: NativeTerminalCaret?
    private var lastAppearance: (font: UIFont, foreground: UIColor)?
    private var retired = false
    private var wantsKeyboard = false
    private var hasActivated = false
    private var sceneActive = false

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
        addSubview(renderer); addSubview(input)
        bindInput()
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
        input.onRetired = { [weak self] in
            guard let self, !self.retired else { return }
            self.wantsKeyboard = false
            self.onInputRetired?()
        }
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
        renderer.frame = bounds; input.frame = bounds
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
        input.onFailure = nil; input.onPasteReview = nil; input.onRetired = nil
        input.removeFromSuperview(); input = replacement
        bindInput(); addSubview(replacement)
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
        retired = true; wantsKeyboard = false; sceneActive = false; lastCaret = nil; lastAppearance = nil
        input.onFailure = nil; input.onPasteReview = nil; input.onRetired = nil
        renderer.onFailure = nil; renderer.onCaret = nil; renderer.onAppearance = nil
        renderer.onCaretUnavailable = nil; renderer.onViewportChange = nil; onViewportChange = nil
        input.retire(); renderer.retire()
    }
}
#endif
#endif
