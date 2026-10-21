#if canImport(UIKit) && canImport(MetalKit)
import UIKit

/// An explicit, source-bound clipboard surface for a live terminal. Copy uses a
/// frozen canonical viewport, not OCR, accessibility text, or shell commands.
/// Paste delegates actual encoding/admission to the original native responder.
@MainActor
public final class NativeTerminalClipboardController: NSObject {
    private struct Destination: Equatable, Sendable {
        let keyboardID: UUID
        let focus: TerminalFocus
    }
    private weak var terminal: NativeTerminalView?
    private let source: TerminalSource, destinationLabel: String
    private let review = TerminalClipboardReview<Destination>()
    private var frame: Data?
    private var active = false, retired = false
    private var generation = UUID()
    private var copyTask: Task<TerminalCopyDocument, Error>?
    private var overlay: UIView?
    private var textView: UITextView?
    private var panelID: UUID?
    public var onFailure: (@MainActor (Error) -> Void)?
    public var isPresenting: Bool { overlay != nil || copyTask != nil }

    public init(terminal: NativeTerminalView, destinationLabel: String) throws {
        guard !destinationLabel.isEmpty, destinationLabel.utf8.count <= 1024,
              terminal.onPasteReview == nil else { throw TerminalClipboardError.unavailable }
        self.terminal = terminal; source = terminal.source; self.destinationLabel = destinationLabel
        super.init()
        terminal.onPasteReview = { [weak self] candidate in self?.offerPaste(candidate) }
        NotificationCenter.default.addObserver(self, selector: #selector(applicationDeactivated),
            name: UIApplication.willResignActiveNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(sceneDeactivated(_:)),
            name: UIScene.willDeactivateNotification, object: nil)
    }
    deinit { NotificationCenter.default.removeObserver(self) }
    @objc private func applicationDeactivated() { setActive(false) }
    @objc private func sceneDeactivated(_ note: Notification) {
        if let scene = note.object as? UIScene, scene === terminal?.window?.windowScene { setActive(false) }
    }

    /// Keep only one received full frame while selected. Admission here is not
    /// a claim that Metal has presented it. The copy panel explicitly labels its
    /// immutable text as a received-viewport snapshot, not a display screenshot.
    public func receiveFrame(_ bytes: Data, source: TerminalSource) throws {
        guard source == self.source else { throw TerminalInputError.stale }
        guard !retired, active else { return }
        guard bytes.count <= 8 * 1024 * 1024 else { throw TerminalClipboardError.capacity }
        frame = bytes
    }
    public func setActive(_ active: Bool) {
        guard !retired else { return }
        self.active = active
        if !active { dismiss(); frame = nil }
    }
    /// A selection document is read-only and may outlive the keyboard that
    /// resigned to select it. A paste approval never survives that retirement.
    public func inputRetired() {
        if review.candidate != nil { dismiss() }
    }
    private var canPresent: Bool {
        !retired && active && terminal?.window?.windowScene?.activationState == .foregroundActive
            && UIApplication.shared.applicationState == .active
    }

    /// Call only from a user Copy/Select Output action. One process-wide capture
    /// permit stays owned until synchronous decoding really settles, even after
    /// cancellation. Repeated taps cannot enqueue an unbounded decoder backlog.
    public func requestCopy() {
        guard canPresent, !isPresenting else { return }
        guard let bytes = frame else { onFailure?(TerminalClipboardError.unavailable); return }
        do {
            let permit = try ClipboardCaptureReservation()
            let source = self.source, token = UUID(); generation = token
            let job = Task.detached(priority: .userInitiated) {
                defer { permit.release() }
                try Task.checkCancellation()
                let frame = try TerminalRenderFrame.decode(bytes, expectedSource: source)
                // Never expose a partial synchronized-output transaction as the
                // completed viewport. Try again after the remote update ends.
                guard frame.flags & 8 == 0 else { throw TerminalClipboardError.unavailable }
                let document = try TerminalCopyDocument(rows: frame.lines.map { row in
                    row.cells.map { .init(column: $0.column, width: $0.width, text: $0.text, concealed: $0.invisible) }
                }, columns: frame.columns)
                try Task.checkCancellation()
                return document
            }
            copyTask = job
            Task { [weak self] in
                let result = await job.result
                guard let self else { return }
                self.copyTask = nil
                guard self.generation == token, self.canPresent else { return }
                switch result {
                case let .success(document): self.showCopy(document)
                case let .failure(error): self.onFailure?(error)
                }
            }
        } catch { onFailure?(error) }
    }

    /// The menu/button is a deliberate user paste action. No foreground or
    /// output callback ever invokes this path or reads the system pasteboard.
    public func requestPaste() {
        guard canPresent, !isPresenting, let terminal else { return }
        guard terminal.input.isEditable, terminal.input.session.acceptsPaste else {
            onFailure?(TerminalClipboardError.unavailable); return
        }
        terminal.input.paste(nil)
    }

    private func offerPaste(_ candidate: TerminalPasteReview) {
        guard canPresent, !isPresenting, let terminal,
              candidate.focus.source == source, candidate.keyboardID == terminal.input.session.id,
              candidate.focus == terminal.input.session.originalFocus else {
            terminal?.cancelPaste(id: candidate.id); return
        }
        do {
            let destination = Destination(keyboardID: candidate.keyboardID, focus: candidate.focus)
            try review.offer(id: candidate.id, destination: destination, text: candidate.text)
            let preview = PassiveClipboardTextView()
            preview.isEditable = false; preview.isSelectable = false
            preview.text = candidate.text
            preview.font = .monospacedSystemFont(ofSize: 14, weight: .regular)
            preview.accessibilityLabel = "Text to paste"
            let warning = candidate.containsLineBreaks
                ? "Contains line breaks. A shell may execute commands when this is pasted. No extra Return is added."
                : "Paste exactly this text into the identified terminal. No extra Return is added."
            let id = candidate.id
            showPanel(id: id, title: "Review Paste", detail: "\(destinationLabel)\n\(candidate.utf8Bytes) UTF-8 bytes. \(warning)",
                      text: preview, actions: [
                button("Cancel") { [weak self] in self?.cancelPanel(id) },
                button("Paste to This Terminal") { [weak self] in self?.confirm(id) }
            ])
        } catch { terminal.cancelPaste(id: candidate.id); review.retire(); onFailure?(error) }
    }
    private func confirm(_ id: UUID) {
        guard canPresent, panelID == id, let terminal else { cancelPanel(id); return }
        let destination = Destination(keyboardID: terminal.input.session.id, focus: terminal.input.session.originalFocus)
        do {
            try review.confirm(id: id, destination: destination) { _ in try terminal.confirmPaste(id: id) }
            dismiss()
        } catch {
            terminal.cancelPaste(id: id); dismiss(); onFailure?(error)
        }
    }
    private func showCopy(_ document: TerminalCopyDocument) {
        guard canPresent, overlay == nil else { return }
        // Inspection is read-only. Retire the live responder before covering
        // it, so hardware text cannot go to a shell hidden behind the reader.
        // Only explicit return-to-terminal focus activation creates a new one.
        terminal?.input.retire()
        let text = NativeTerminalCopyTextView(document: document)
        text.onFailure = { [weak self] error in self?.onFailure?(error) }
        let id = UUID()
        showPanel(id: id, title: "Select Output", detail: "\(destinationLabel)\nFrozen received viewport; live output continues. Screen-row breaks are preserved. Copies stay on this device and expire after two minutes.",
                  text: text, actions: [
            button("Select All") { [weak text] in text?.selectAll(nil) },
            button("Copy Selection") { [weak text] in text?.copy(nil) },
            button("Done") { [weak self] in self?.cancelPanel(id) }
        ])
    }
    private func cancelPanel(_ id: UUID) {
        guard panelID == id else { return }
        dismiss()
    }
    private func button(_ title: String, action: @escaping @MainActor () -> Void) -> UIButton {
        let button = UIButton(type: .system)
        var configuration = UIButton.Configuration.bordered(); configuration.title = title
        button.configuration = configuration
        button.heightAnchor.constraint(greaterThanOrEqualToConstant: 44).isActive = true
        button.addAction(UIAction { _ in action() }, for: .touchUpInside)
        return button
    }
    private func showPanel(id: UUID, title: String, detail: String, text: UITextView, actions: [UIButton]) {
        guard let terminal, canPresent, overlay == nil else { return }
        let panel = UIView(); panel.backgroundColor = .systemBackground
        panel.accessibilityViewIsModal = true
        let heading = UILabel(); heading.text = title; heading.font = .preferredFont(forTextStyle: .headline)
        heading.adjustsFontForContentSizeCategory = true
        let explanation = UILabel(); explanation.text = detail; explanation.numberOfLines = 0
        explanation.font = .preferredFont(forTextStyle: .caption1); explanation.adjustsFontForContentSizeCategory = true
        let buttons = UIStackView(arrangedSubviews: actions); buttons.axis = .vertical; buttons.spacing = 4
        let scroll = UIScrollView()
        let stack = UIStackView(arrangedSubviews: [heading, explanation, text, buttons])
        stack.axis = .vertical; stack.spacing = 8
        text.dataDetectorTypes = []; text.writingToolsBehavior = .none
        text.backgroundColor = .secondarySystemBackground; text.layer.cornerRadius = 8
        text.setContentHuggingPriority(.defaultLow, for: .vertical)
        heading.setContentCompressionResistancePriority(.required, for: .vertical)
        explanation.setContentCompressionResistancePriority(.required, for: .vertical)
        panel.translatesAutoresizingMaskIntoConstraints = false; stack.translatesAutoresizingMaskIntoConstraints = false
        scroll.translatesAutoresizingMaskIntoConstraints = false
        terminal.addSubview(panel); panel.addSubview(scroll); scroll.addSubview(stack)
        // Controls remain reachable in a short landscape viewport or with a
        // large accessibility font. Never compress the confirmation buttons to
        // zero height just to fit the still-visible software keyboard.
        let preferredTextHeight = text.heightAnchor.constraint(equalTo: panel.heightAnchor, multiplier: 0.5)
        preferredTextHeight.priority = .defaultHigh
        NSLayoutConstraint.activate([
            panel.topAnchor.constraint(equalTo: terminal.topAnchor), panel.bottomAnchor.constraint(equalTo: terminal.bottomAnchor),
            panel.leadingAnchor.constraint(equalTo: terminal.leadingAnchor), panel.trailingAnchor.constraint(equalTo: terminal.trailingAnchor),
            scroll.topAnchor.constraint(equalTo: panel.safeAreaLayoutGuide.topAnchor),
            scroll.bottomAnchor.constraint(equalTo: panel.safeAreaLayoutGuide.bottomAnchor),
            scroll.leadingAnchor.constraint(equalTo: panel.leadingAnchor), scroll.trailingAnchor.constraint(equalTo: panel.trailingAnchor),
            stack.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor, constant: 8),
            stack.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor, constant: -8),
            stack.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 12),
            stack.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -12),
            stack.widthAnchor.constraint(equalTo: scroll.frameLayoutGuide.widthAnchor, constant: -24),
            text.heightAnchor.constraint(greaterThanOrEqualToConstant: 120), preferredTextHeight
        ])
        overlay = panel; textView = text; panelID = id
        UIAccessibility.post(notification: .screenChanged, argument: heading)
    }
    private func dismiss() {
        generation = UUID(); copyTask?.cancel()
        if let candidate = review.candidate { terminal?.cancelPaste(id: candidate.id) }
        review.retire()
        _ = textView?.resignFirstResponder()
        (textView as? NativeTerminalCopyTextView)?.clearSnapshot()
        textView?.text = ""; textView = nil
        overlay?.removeFromSuperview(); overlay = nil; panelID = nil
    }
    public func retire() {
        guard !retired else { return }
        retired = true; active = false; dismiss(); frame = nil
        terminal?.onPasteReview = nil
        onFailure = nil
    }
    public func waitUntilSettled() async {
        if let copyTask { _ = await copyTask.result }
    }
}

/// The preview must not replace/resign the responder whose exact paste request
/// it is reviewing. This is an inline review, not a modal presentation or editor.
@MainActor
private final class PassiveClipboardTextView: UITextView {
    override var canBecomeFirstResponder: Bool { false }
}

/// The only transcript copy path, shared by the explicit button and UIKit's
/// selection menu / hardware Copy command. No cut, paste, share or URL detector
/// is installed on this read-only snapshot. Tests inject a non-system writer.
@MainActor
final class NativeTerminalCopyTextView: UITextView {
    private var document: TerminalCopyDocument?
    private let writer: @MainActor (String) -> Void
    var onFailure: (@MainActor (Error) -> Void)?
    init(document: TerminalCopyDocument, writer: @escaping @MainActor (String) -> Void = { text in
        UIPasteboard.general.setItems([["public.utf8-plain-text": text]],
            options: [.localOnly: true, .expirationDate: Date().addingTimeInterval(120)])
    }) {
        self.document = document; self.writer = writer
        super.init(frame: .zero, textContainer: nil)
        text = document.text; isEditable = false; isSelectable = true
        font = .monospacedSystemFont(ofSize: 14, weight: .regular)
        dataDetectorTypes = []; writingToolsBehavior = .none
        textDragInteraction?.isEnabled = false
        accessibilityLabel = "Captured terminal output"
    }
    @available(*, unavailable)
    required init?(coder: NSCoder) { nil }
    override func canPerformAction(_ action: Selector, withSender sender: Any?) -> Bool {
        guard document != nil else { return false }
        if action == #selector(copy(_:)) { return selectedRange.length > 0 }
        if action == #selector(selectAll(_:)) { return !text.isEmpty }
        return false
    }
    override func copy(_ sender: Any?) {
        guard let document else { return }
        do {
            let range = try document.normalizedRange(selectedRange)
            let value = try document.selectedText(in: range)
            selectedRange = range
            writer(value)
        } catch { onFailure?(error) }
    }
    func clearSnapshot() { document = nil; text = ""; onFailure = nil }
}

/// Only one outstanding full-frame decode across the process, including work
/// cancelled while Foundation is still decoding. There is no waiting queue.
private final class ClipboardCaptureReservation: @unchecked Sendable {
    private final class Ledger: @unchecked Sendable { let lock = NSLock(); var busy = false }
    private static let ledger = Ledger()
    private let lock = NSLock()
    private var owned = true
    init() throws {
        guard Self.ledger.lock.withLock({
            if Self.ledger.busy { return false }; Self.ledger.busy = true; return true
        }) else { owned = false; throw TerminalClipboardError.unavailable }
    }
    func release() {
        let release = lock.withLock { let value = owned; owned = false; return value }
        if release { Self.ledger.lock.withLock { Self.ledger.busy = false } }
    }
    deinit { release() }
}
#endif
