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
    private struct CopyCapture: Sendable {
        let document: TerminalCopyDocument, receivedAt: Date
    }
    private weak var terminal: NativeTerminalView?
    private let source: TerminalSource, destinationLabel: String
    private let commandDestination: TerminalCommandDraftDestination
    private let review = TerminalClipboardReview<Destination>()
    private var frame: (bytes: Data, receivedAt: Date)?
    private var active = false, retired = false
    private var generation = UUID()
    private var copyTask: Task<CopyCapture, Error>?
    private var overlay: UIView?
    private var textView: UITextView?
    private var detailLabel: UILabel?
    private var panelID: UUID?
    private var panelGestures: [(UIGestureRecognizer, Bool)] = []
    private var commandDraft: TerminalCommandDraft<TerminalCommandDraftDestination>?
    private var commandTask: Task<Void, Never>?
    private var commandTaskID: UUID?
    private var diagnosticExcerpt: TerminalDiagnosticExcerpt?
    private weak var diagnosticContextButton: UIButton?
    private weak var diagnosticShareButton: UIButton?
    private var shareActivity: UIActivityViewController?
    private var shareItem: NativeTerminalDiagnosticShareItem?
    private var shareID: UUID?
    public var onFailure: (@MainActor (Error) -> Void)?
    /// The session owner restores a fresh keyboard only for this same source
    /// and mux visit. This callback must not connect, select or create a shell.
    public var onCommandDraftFocus: (@MainActor () async throws -> TerminalCommandDraftDestination)?
    public var isPresenting: Bool {
        overlay != nil || copyTask != nil || commandDraft != nil || commandTask != nil
            || diagnosticExcerpt != nil || shareActivity != nil
    }
    /// Editing owns the responder. Only the explicit, frozen Review transition
    /// may restore terminal focus; ordinary navigation remains blocked by
    /// isPresenting until the existing paste review takes ownership.
    public var blocksKeyboardActivation: Bool {
        isPresenting && commandDraft?.phase != .preparingReview
    }

    public init(terminal: NativeTerminalView, destinationLabel: String,
                commandDestination: TerminalCommandDraftDestination? = nil) throws {
        guard !destinationLabel.isEmpty, destinationLabel.utf8.count <= 1024,
              terminal.onPasteReview == nil else { throw TerminalClipboardError.unavailable }
        let destination = commandDestination ?? TerminalCommandDraftDestination(source: terminal.source)
        guard destination.source == terminal.source else { throw TerminalInputError.stale }
        self.commandDestination = destination
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
        frame = (bytes, Date())
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
    /// Called after replacement but BEFORE the session requests first responder.
    /// Restoring review focus must not briefly expose a covered shell to Return,
    /// IME or hardware/helper keys while the async focus callback completes.
    public func inputInstalled() {
        terminal?.input.setReviewingPaste(commandDraft?.phase == .preparingReview || review.candidate != nil)
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
        guard let capture = frame else { onFailure?(TerminalClipboardError.unavailable); return }
        do {
            guard let terminal else { throw TerminalClipboardError.unavailable }
            try Self.validateInputForInspection(terminal.input.session)
            let permit = try ClipboardCaptureReservation()
            let source = self.source, token = UUID(); generation = token
            let job = Task.detached(priority: .userInitiated) {
                defer { permit.release() }
                try Task.checkCancellation()
                let frame = try TerminalRenderFrame.decode(capture.bytes, expectedSource: source)
                // Never expose a partial synchronized-output transaction as the
                // completed viewport. Try again after the remote update ends.
                guard frame.flags & 8 == 0 else { throw TerminalClipboardError.unavailable }
                let document = try TerminalCopyDocument(frame: frame)
                try Task.checkCancellation()
                return CopyCapture(document: document, receivedAt: capture.receivedAt)
            }
            copyTask = job
            Task { [weak self] in
                let result = await job.result
                guard let self else { return }
                self.copyTask = nil
                guard self.generation == token, self.canPresent else { return }
                switch result {
                case let .success(capture): self.showCopy(capture)
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

    /// A local multiline editor, opened only by the user's Compose action.
    /// It reads no clipboard and sends no input. The native responder retires
    /// before the editor appears, including any uncommitted terminal IME text.
    public func requestCommandDraft() {
        guard canPresent, !isPresenting, let terminal, onCommandDraftFocus != nil else { return }
        do {
            try TerminalCommandDraft<TerminalCommandDraftDestination>.validateOpening(keyboard: terminal.input.session,
                liveViewport: terminal.renderer.admittedFrame?.isLive != false)
        }
        catch { onFailure?(error); return }
        let draft = TerminalCommandDraft(destination: commandDestination)
        commandDraft = draft
        terminal.input.retire()
        showCommandEditor(draft)
    }

    private func showCommandEditor(_ draft: TerminalCommandDraft<TerminalCommandDraftDestination>, problem: String? = nil) {
        guard canPresent, commandDraft === draft, draft.phase == .editing, overlay == nil else { return }
        let editor = NativeTerminalCommandTextView(draft: draft)
        editor.onChange = { [weak self, weak draft] in
            guard let draft else { return }
            self?.updateCommandDetail(draft)
        }
        editor.onFailure = { [weak self, weak draft] error in
            guard let draft else { return }
            self?.updateCommandDetail(draft, problem: error.localizedDescription)
        }
        showPanel(id: draft.id, title: "Compose Command", detail: "", text: editor, actions: [
            button("Discard Draft") { [weak self] in self?.cancelPanel(draft.id) },
            button("Review Draft…") { [weak self] in self?.prepareCommandReview(draft.id) }
        ])
        updateCommandDetail(draft, problem: problem)
        _ = editor.becomeFirstResponder()
    }

    private func updateCommandDetail(_ draft: TerminalCommandDraft<TerminalCommandDraftDestination>, problem: String? = nil) {
        guard commandDraft === draft, panelID == draft.id else { return }
        detailLabel?.text = "\(destinationLabel)\nLocal draft · \(draft.utf8Bytes) of 32,768 UTF-8 bytes. Return inserts a line break here. Review asks for confirmation before pasting; no extra Return is added."
            + (problem.map { "\n\($0)" } ?? "")
    }

    private func prepareCommandReview(_ id: UUID) {
        guard canPresent, commandTask == nil, let draft = commandDraft, draft.id == id,
              let editor = textView as? NativeTerminalCommandTextView,
              let terminal, let restoreFocus = onCommandDraftFocus else { return }
        do {
            // Native language entry commits only into the LOCAL editor here.
            editor.unmarkText()
            try draft.update(editor.text ?? "")
            try draft.prepareReview(destination: commandDestination)
        } catch { updateCommandDetail(draft, problem: error.localizedDescription); return }
        removePanel()
        let preview = PassiveClipboardTextView()
        preview.isEditable = false; preview.isSelectable = false; preview.text = draft.text
        preview.font = .monospacedSystemFont(ofSize: 14, weight: .regular)
        preview.accessibilityLabel = "Draft awaiting paste review"
        showPanel(id: id, title: "Preparing Review", detail: "\(destinationLabel)\nRestoring this terminal's keyboard. No draft text has been sent.",
                  text: preview, actions: [button("Cancel") { [weak self] in self?.cancelPanel(id) }])
        let taskID = UUID(); commandTaskID = taskID
        commandTask = Task { [weak self, weak terminal] in
            guard let self, let terminal else { draft.retire(); return }
            defer {
                if self.commandTaskID == taskID { self.commandTask = nil; self.commandTaskID = nil }
            }
            do {
                try Task.checkCancellation()
                let restoredDestination = try await restoreFocus()
                try Task.checkCancellation()
                guard self.canPresent, self.commandDraft === draft, self.terminal === terminal,
                      terminal.source == self.source, terminal.input.canReviewPaste else {
                    throw TerminalCommandDraftError.unavailable
                }
                self.removePanel()
                try draft.offerReview(destination: restoredDestination) { text in
                    // Transfer ownership synchronously, with no event-loop gap:
                    // only the existing source/focus-bound paste path may offer
                    // the next decision. This does not confirm or admit it.
                    self.commandDraft = nil; self.commandTask = nil; self.commandTaskID = nil
                    terminal.input.requestPasteReview(text: text)
                    guard let candidate = self.review.candidate,
                          candidate.id == terminal.input.pendingPaste?.id else {
                        if let pending = terminal.input.pendingPaste { terminal.cancelPaste(id: pending.id) }
                        throw TerminalCommandDraftError.unavailable
                    }
                }
            } catch {
                guard !(error is CancellationError), !Task.isCancelled, self.canPresent else {
                    if self.commandDraft === draft { self.dismiss() }
                    return
                }
                if self.commandDraft === draft, draft.phase == .preparingReview {
                    // An unavailable caret/keyboard leaves the draft editable
                    // for another explicit Review. It is never retried by a
                    // foreground, output, or focus callback.
                    try? draft.resumeEditing(destination: self.commandDestination)
                    self.removePanel()
                    terminal.input.retire()
                    self.showCommandEditor(draft, problem: "Return to live output if needed, then review again. \(error.localizedDescription)")
                } else if self.commandDraft === draft {
                    self.dismiss(); self.onFailure?(error)
                } else if self.commandDraft == nil {
                    self.dismiss(); self.onFailure?(error)
                }
            }
        }
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
            terminal.input.setReviewingPaste(true)
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
        } catch {
            terminal.cancelPaste(id: candidate.id); review.retire()
            terminal.input.setReviewingPaste(false); onFailure?(error)
        }
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
    private func showCopy(_ capture: CopyCapture) {
        guard canPresent, overlay == nil, let terminal else { return }
        // Capture runs away from the main actor. A key/composition may have
        // arrived since its preflight; never discard it when inspection opens.
        do { try Self.validateInputForInspection(terminal.input.session) }
        catch { onFailure?(error); return }
        // Inspection is read-only. Retire the live responder before covering
        // it, so hardware text cannot go to a shell hidden behind the reader.
        // Only explicit return-to-terminal focus activation creates a new one.
        terminal.input.retire()
        let text = NativeTerminalCopyTextView(document: capture.document)
        text.onFailure = { [weak self] error in self?.onFailure?(error) }
        let id = UUID(), receivedAt = capture.receivedAt
        showPanel(id: id, title: "Select Output", detail: "\(destinationLabel)\nFrozen received viewport; live output continues. Soft wraps are joined and hard line breaks preserved. Select the exact text to copy or share. Copies stay on this device and expire after two minutes.",
                  text: text, actions: [
            button("Select All") { [weak text] in text?.selectAll(nil) },
            button("Copy Selection") { [weak text] in text?.copy(nil) },
            button("Share Selection…") { [weak self] in self?.beginDiagnosticExcerpt(id, receivedAt: receivedAt) },
            button("Done") { [weak self] in self?.cancelPanel(id) }
        ])
    }

    static func validateInputForInspection(_ keyboard: TerminalKeyboardSession) throws {
        guard !keyboard.document.isMarked, keyboard.accounting.pendingEvents == 0,
              keyboard.phase != .blocked else { throw TerminalClipboardError.pendingInput }
    }

    private func beginDiagnosticExcerpt(_ id: UUID, receivedAt: Date) {
        guard canPresent, panelID == id, diagnosticExcerpt == nil,
              let text = textView as? NativeTerminalCopyTextView else { return }
        do {
            let context = try TerminalDiagnosticExcerpt.Context(destinationLabel: destinationLabel, receivedAt: receivedAt)
            let excerpt = try text.makeDiagnosticExcerpt(context: context)
            diagnosticExcerpt = excerpt
            // Drop the unselected viewport before editing. Later frames cannot
            // change the excerpt, its original label, or its receipt time.
            removePanel()
            showDiagnosticEditor(excerpt)
        } catch { onFailure?(error) }
    }

    private func showDiagnosticEditor(_ excerpt: TerminalDiagnosticExcerpt) {
        guard canPresent, diagnosticExcerpt === excerpt, excerpt.phase == .editing, overlay == nil else { return }
        let editor = NativeTerminalDiagnosticTextView(excerpt: excerpt)
        editor.onChange = { [weak self, weak excerpt] in
            guard let excerpt else { return }
            self?.updateDiagnosticDetail(excerpt)
        }
        editor.onFailure = { [weak self, weak excerpt] error in
            guard let excerpt else { return }
            self?.updateDiagnosticDetail(excerpt, problem: error.localizedDescription)
        }
        let context = button("Include Context: Off") { [weak self, weak excerpt] in
            guard let self, let excerpt, self.diagnosticExcerpt === excerpt else { return }
            do { try excerpt.setIncludesContext(!excerpt.includesContext); self.updateDiagnosticDetail(excerpt) }
            catch { self.updateDiagnosticDetail(excerpt, problem: error.localizedDescription) }
        }
        context.accessibilityHint = "Include the original machine, route, pane when present, and viewport receipt time in the shared text."
        showPanel(id: excerpt.id, title: "Prepare Diagnostic Excerpt", detail: "", text: editor, actions: [
            button("Redact Selected Text") { [weak editor] in editor?.redactSelection() },
            context,
            button("Review Share…") { [weak self] in self?.prepareDiagnosticReview(excerpt.id) },
            button("Cancel") { [weak self] in self?.cancelPanel(excerpt.id) }
        ])
        diagnosticContextButton = context
        updateDiagnosticDetail(excerpt)
        _ = editor.becomeFirstResponder()
    }

    private func updateDiagnosticDetail(_ excerpt: TerminalDiagnosticExcerpt, problem: String? = nil) {
        guard diagnosticExcerpt === excerpt, panelID == excerpt.id else { return }
        let context = excerpt.includesContext ? "On" : "Off"
        diagnosticContextButton?.configuration?.title = "Include Context: \(context)"
        diagnosticContextButton?.accessibilityValue = context
        detailLabel?.text = "\(destinationLabel)\nOnly your selected excerpt is included. Edit it or select private text and tap Redact Selected Text. No automatic secret detection is performed. \(excerpt.utf8Bytes) of 131,072 UTF-8 bytes. Context adds the original source and viewport receipt time."
            + (problem.map { "\n\($0)" } ?? "")
    }

    private func prepareDiagnosticReview(_ id: UUID) {
        guard canPresent, panelID == id, let excerpt = diagnosticExcerpt, excerpt.id == id,
              let editor = textView as? NativeTerminalDiagnosticTextView else { return }
        do {
            editor.unmarkText()
            try excerpt.update(editor.text ?? "")
            let review = try excerpt.prepareReview()
            let reviewID = review.id
            removePanel()
            let preview = PassiveClipboardTextView()
            preview.isEditable = false; preview.isSelectable = false; preview.text = review.text
            preview.font = .monospacedSystemFont(ofSize: 14, weight: .regular)
            preview.accessibilityLabel = "Complete text to share"
            let share = button("Share…") { [weak self] in self?.presentDiagnosticShare(id, reviewID: reviewID) }
            share.accessibilityIdentifier = "terminal.excerpt.share"
            let context = excerpt.includesContext ? "Original source and viewport receipt time are included." : "Source context is not included."
            showPanel(id: id, title: "Review Diagnostic Share", detail: "\(destinationLabel)\nThis is the complete plain-text item. \(review.text.utf8.count) UTF-8 bytes. \(context) Share opens the system chooser; nothing is sent to the terminal.", text: preview, actions: [
                button("Edit Excerpt") { [weak self, weak excerpt] in
                    guard let self, let excerpt, self.canPresent, self.diagnosticExcerpt === excerpt,
                          self.shareActivity == nil else { return }
                    do {
                        try excerpt.resumeEditing(reviewID: reviewID)
                        self.removePanel(); self.showDiagnosticEditor(excerpt)
                    } catch { self.onFailure?(error) }
                },
                share,
                button("Cancel") { [weak self] in self?.cancelPanel(id) }
            ])
            diagnosticShareButton = share
        } catch { updateDiagnosticDetail(excerpt, problem: error.localizedDescription) }
    }

    private func presentDiagnosticShare(_ id: UUID, reviewID: UUID) {
        guard canPresent, panelID == id, let excerpt = diagnosticExcerpt, excerpt.id == id,
              shareActivity == nil, let terminal, terminal.source == source,
              let anchor = diagnosticShareButton, anchor.window === terminal.window else { return }
        // Resolve only this terminal's responder chain. A process-global key
        // window/root lookup could present the captured text in another scene.
        var responder: UIResponder? = terminal
        while responder != nil && !(responder is UIViewController) { responder = responder?.next }
        guard let presenter = responder as? UIViewController,
              presenter.viewIfLoaded?.window === terminal.window, presenter.presentedViewController == nil,
              !presenter.isBeingDismissed, !presenter.isBeingPresented else {
            onFailure?(TerminalDiagnosticExcerptError.unavailable); return
        }
        do {
            try excerpt.share(reviewID: reviewID) { value in
                let item = try NativeTerminalDiagnosticShareItem(text: value)
                let activity = item.makeActivityController(anchor: anchor)
                let token = UUID()
                shareItem = item; shareActivity = activity; shareID = token
                activity.completionWithItemsHandler = { [weak self] _, _, _, error in
                    Task { @MainActor [weak self] in
                        guard let self, self.shareID == token else { return }
                        self.dismiss()
                        if let error, self.canPresent { self.onFailure?(error) }
                    }
                }
                presenter.present(activity, animated: true)
            }
        } catch { dismiss(); onFailure?(error) }
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
        // Ancestor terminal recognizers must not steal the editor's caret,
        // text-selection or scrolling gestures (or tap a hidden remote TUI).
        panelGestures = (terminal.gestureRecognizers ?? []).map { ($0, $0.isEnabled) }
        for (recognizer, _) in panelGestures { recognizer.isEnabled = false }
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
        overlay = panel; textView = text; detailLabel = explanation; panelID = id
        UIAccessibility.post(notification: .screenChanged, argument: heading)
    }
    private func dismiss() {
        generation = UUID(); copyTask?.cancel()
        commandTask?.cancel(); commandDraft?.retire(); commandDraft = nil
        diagnosticExcerpt?.retire(); diagnosticExcerpt = nil
        // A handed-off service may already own its copy. Retiring prevents any
        // later provider callback from retrieving our retained text or replaying
        // the same local approval after a scene/source change.
        shareID = nil; shareItem?.retire(); shareItem = nil
        shareActivity?.completionWithItemsHandler = nil
        if let shareActivity, shareActivity.presentingViewController != nil { shareActivity.dismiss(animated: false) }
        shareActivity = nil
        if let candidate = review.candidate { terminal?.cancelPaste(id: candidate.id) }
        review.retire()
        terminal?.input.setReviewingPaste(false)
        removePanel()
    }
    private func removePanel() {
        _ = textView?.resignFirstResponder()
        (textView as? NativeTerminalCopyTextView)?.clearSnapshot()
        (textView as? NativeTerminalCommandTextView)?.clearDraft()
        (textView as? NativeTerminalDiagnosticTextView)?.clearExcerpt()
        textView?.text = ""; textView = nil
        overlay?.removeFromSuperview(); overlay = nil; panelID = nil; detailLabel = nil
        diagnosticContextButton = nil; diagnosticShareButton = nil
        for (recognizer, enabled) in panelGestures { recognizer.isEnabled = enabled }
        panelGestures.removeAll()
    }
    public func retire() {
        guard !retired else { return }
        retired = true; active = false; dismiss(); frame = nil
        terminal?.onPasteReview = nil
        onFailure = nil; onCommandDraftFocus = nil
    }
    public func waitUntilSettled() async {
        if let copyTask { _ = await copyTask.result }
        if let commandTask { await commandTask.value }
    }
}

/// A normal native text document with bounded edits, undo and IME. It has no
/// terminal-input callbacks: Return, selection, cut, paste and language entry
/// affect only the local draft. Review is a separate controller action.
@MainActor
public final class NativeTerminalCommandTextView: UITextView, UITextViewDelegate {
    private var draft: TerminalCommandDraft<TerminalCommandDraftDestination>?
    public var onChange: (@MainActor () -> Void)?
    public var onFailure: (@MainActor (Error) -> Void)?

    public init(draft: TerminalCommandDraft<TerminalCommandDraftDestination>) {
        self.draft = draft
        super.init(frame: .zero, textContainer: nil)
        text = draft.text; delegate = self; isEditable = true; isSelectable = true
        font = .monospacedSystemFont(ofSize: 14, weight: .regular)
        autocapitalizationType = .none; autocorrectionType = .no; spellCheckingType = .no
        smartQuotesType = .no; smartDashesType = .no; smartInsertDeleteType = .no
        dataDetectorTypes = []; writingToolsBehavior = .none; textContentType = nil
        textDragInteraction?.isEnabled = false
        accessibilityLabel = "Local command draft"
        accessibilityIdentifier = "terminal.command.editor"
        returnKeyType = .default
    }
    @available(*, unavailable)
    public required init?(coder: NSCoder) { nil }
    public override func didMoveToWindow() { super.didMoveToWindow(); undoManager?.levelsOfUndo = 32 }

    public func textView(_ textView: UITextView, shouldChangeTextIn range: NSRange, replacementText: String) -> Bool {
        guard let draft else { return false }
        do {
            if (textView.text ?? "") != draft.text { try draft.update(textView.text ?? "") }
            _ = try draft.proposedText(replacing: range, with: replacementText)
            return true
        } catch { onFailure?(error); return false }
    }
    public func textViewDidChange(_ textView: UITextView) {
        guard let draft else { textView.text = ""; return }
        do { try draft.update(textView.text ?? ""); onChange?() }
        catch {
            // Dictation/marked-text providers can publish a whole document.
            // Restore the last accepted draft instead of retaining an oversized
            // or control-bearing value that bypassed a replacement callback.
            textView.text = draft.text
            textView.undoManager?.removeAllActions()
            textView.selectedRange = NSRange(location: draft.text.utf16.count, length: 0)
            onFailure?(error)
        }
    }
    public func clearDraft() {
        draft = nil; delegate = nil; onChange = nil; onFailure = nil
        undoManager?.removeAllActions(); text = ""; isEditable = false
    }
}

/// A bounded, local redaction document. The ordinary native Share/Lookup menu
/// is withheld: only the controller's exact final preview opens a share sheet.
@MainActor
public final class NativeTerminalDiagnosticTextView: UITextView, UITextViewDelegate {
    private var excerpt: TerminalDiagnosticExcerpt?
    public var onChange: (@MainActor () -> Void)?
    public var onFailure: (@MainActor (Error) -> Void)?

    public init(excerpt: TerminalDiagnosticExcerpt) {
        self.excerpt = excerpt
        super.init(frame: .zero, textContainer: nil)
        text = excerpt.text; delegate = self; isEditable = true; isSelectable = true
        font = .monospacedSystemFont(ofSize: 14, weight: .regular)
        autocapitalizationType = .none; autocorrectionType = .no; spellCheckingType = .no
        smartQuotesType = .no; smartDashesType = .no; smartInsertDeleteType = .no
        dataDetectorTypes = []; writingToolsBehavior = .none; textContentType = nil
        textDragInteraction?.isEnabled = false
        accessibilityLabel = "Editable diagnostic excerpt"
        accessibilityIdentifier = "terminal.excerpt.editor"
        returnKeyType = .default
    }
    @available(*, unavailable)
    public required init?(coder: NSCoder) { nil }
    public override func didMoveToWindow() { super.didMoveToWindow(); undoManager?.levelsOfUndo = 16 }

    public override func canPerformAction(_ action: Selector, withSender sender: Any?) -> Bool {
        guard excerpt?.phase == .editing else { return false }
        let editing = [#selector(paste(_:)),
                       #selector(select(_:)), #selector(selectAll(_:)), #selector(delete(_:)),
                       NSSelectorFromString("undo:"), NSSelectorFromString("redo:")]
        return editing.contains(action) && super.canPerformAction(action, withSender: sender)
    }
    // Transcript copying stays on the bounded, local-only Copy Selection path.
    // Block direct selectors too; sharing this editor requires its final review.
    public override func cut(_ sender: Any?) {}
    public override func copy(_ sender: Any?) {}
    public func textView(_ textView: UITextView, shouldChangeTextIn range: NSRange, replacementText: String) -> Bool {
        guard let excerpt else { return false }
        do {
            if (textView.text ?? "") != excerpt.text { try excerpt.update(textView.text ?? "") }
            _ = try excerpt.proposedText(replacing: range, with: replacementText)
            return true
        } catch { onFailure?(error); return false }
    }
    public func textViewDidChange(_ textView: UITextView) {
        guard let excerpt else { textView.text = ""; return }
        do { try excerpt.update(textView.text ?? ""); onChange?() }
        catch {
            textView.text = excerpt.text
            textView.undoManager?.removeAllActions()
            textView.selectedRange = NSRange(location: excerpt.text.utf16.count, length: 0)
            onFailure?(error)
        }
    }
    public func redactSelection() {
        guard let excerpt else { return }
        do {
            unmarkText()
            try excerpt.update(text ?? "")
            let range = try excerpt.redact(selectedRange)
            text = excerpt.text; selectedRange = range
            // Redact deliberately drops the native undo copy of removed text.
            undoManager?.removeAllActions()
            onChange?()
        } catch { onFailure?(error) }
    }
    public func clearExcerpt() {
        excerpt = nil; delegate = nil; onChange = nil; onFailure = nil
        undoManager?.removeAllActions(); text = ""; isEditable = false
    }
}

/// UIKit requests this bounded in-memory string only after explicit local
/// review. Nothing is written to a file or clipboard. Retirement prevents late
/// provider requests; it cannot retract a copy already accepted by a service.
@MainActor
final class NativeTerminalDiagnosticShareItem: NSObject, UIActivityItemSource {
    private var text: String?
    init(text: String) throws {
        guard !text.isEmpty, text.utf8.count <= TerminalDiagnosticExcerpt.maximumSharedUTF8Bytes else {
            throw TerminalDiagnosticExcerptError.capacity
        }
        self.text = text
        super.init()
    }
    func activityViewControllerPlaceholderItem(_ activityViewController: UIActivityViewController) -> Any { "" }
    func activityViewController(_ activityViewController: UIActivityViewController,
                                itemForActivityType activityType: UIActivity.ActivityType?) -> Any? { text }
    func activityViewController(_ activityViewController: UIActivityViewController,
                                dataTypeIdentifierForActivityType activityType: UIActivity.ActivityType?) -> String {
        "public.utf8-plain-text"
    }
    func makeActivityController(anchor: UIView) -> UIActivityViewController {
        let activity = UIActivityViewController(activityItems: [self], applicationActivities: nil)
        // Keep copying on the original device-local, expiring path. The exact
        // view/rect pair also makes this valid as an iPad popover.
        activity.excludedActivityTypes = [.copyToPasteboard]
        activity.popoverPresentationController?.sourceView = anchor
        activity.popoverPresentationController?.sourceRect = anchor.bounds
        return activity
    }
    func retire() { text = nil }
}

/// The preview must not replace/resign the responder whose exact paste request
/// it is reviewing. This is an inline review, not a modal presentation or editor.
@MainActor
private final class PassiveClipboardTextView: UITextView {
    override var canBecomeFirstResponder: Bool { false }
}

/// A transcript copy path shared by the explicit button and UIKit's selection
/// menu / hardware Copy command. Sharing requires the controller's separate
/// selected-excerpt review; no native Share, cut, paste or URL detector is
/// installed on this read-only snapshot. Tests inject a non-system writer.
@MainActor
public final class NativeTerminalCopyTextView: UITextView {
    private var document: TerminalCopyDocument?
    private let writer: @MainActor (String) -> Void
    public var onFailure: (@MainActor (Error) -> Void)?
    public init(document: TerminalCopyDocument, writer: @escaping @MainActor (String) -> Void = { text in
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
    public required init?(coder: NSCoder) { nil }
    public override func canPerformAction(_ action: Selector, withSender sender: Any?) -> Bool {
        guard document != nil else { return false }
        if action == #selector(copy(_:)) { return selectedRange.length > 0 }
        if action == #selector(selectAll(_:)) { return !text.isEmpty }
        return false
    }
    public override func copy(_ sender: Any?) {
        guard let document else { return }
        do {
            let range = try document.normalizedRange(selectedRange)
            let value = try document.selectedText(in: range)
            selectedRange = range
            writer(value)
        } catch { onFailure?(error) }
    }
    public func makeDiagnosticExcerpt(context: TerminalDiagnosticExcerpt.Context) throws -> TerminalDiagnosticExcerpt {
        guard let document else { throw TerminalDiagnosticExcerptError.unavailable }
        let range = try document.normalizedRange(selectedRange)
        let excerpt = try TerminalDiagnosticExcerpt(document: document, selection: range, context: context)
        selectedRange = range
        return excerpt
    }
    public func clearSnapshot() { document = nil; text = ""; onFailure = nil }
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
