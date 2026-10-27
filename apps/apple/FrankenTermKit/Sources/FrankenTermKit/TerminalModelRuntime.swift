import Foundation

public enum TerminalModelError: Error, Equatable, Sendable {
    case invalid, stale, closed, capacity, busy, unsupported, poisoned, exhausted, viewportUnavailable
}

/// A complete canonical answerback, still owned and charged by the model.
/// Copying is not acceptance. Only settling this exact sequence can release it.
public struct TerminalModelReply: Sendable {
    public let sequence: UInt64
    public let bytes: Data
    public init(sequence: UInt64, bytes: Data) throws {
        guard sequence > 0, !bytes.isEmpty, bytes.count <= 16_384 else { throw TerminalModelError.invalid }
        self.sequence = sequence; self.bytes = bytes
    }
}

public enum TerminalModelReplyDisposition: Sendable { case accepted, droppedWhole }

/// The synchronous canonical model boundary. The native implementation invokes
/// frankenterm-term through its versioned C ABI; this is not an ANSI parser.
/// Calls must enforce their budgets before allocation and perform no network,
/// callbacks, deferred parsing, UI work, or internal user-input admission.
///
/// Transfer exclusive model ownership to TerminalModelRuntime. Encoding is
/// read-only and retryable. copyReply retains the original complete unit until
/// settleReply; captureFrame returns an owned complete FTSF snapshot.
public protocol TerminalSessionModel: AnyObject, Sendable {
    var source: TerminalSource { get }
    func feed(_ bytes: Data) throws
    func resize(_ geometry: TerminalSessionGeometry) throws
    func encode(_ intent: TerminalKeyboardIntent, maximumBytes: Int) throws -> Data
    func preparePaste(_ text: String, maximumBytes: Int) throws -> TerminalPreparedPaste
    func copyReply() throws -> TerminalModelReply?
    func settleReply(sequence: UInt64, disposition: TerminalModelReplyDisposition) throws
    func captureFrame(viewportTop: Int64?) throws -> Data
    func setFontRevision(_ revision: UInt64) throws
    /// See WorkspaceDocument.Theme.terminalBasePalette for the layout.
    func setBasePalette(_ rgba: [UInt8]) throws
    /// Idempotent; finishes model ownership without replaying outstanding bytes.
    /// Failure must retain ownership for a later close attempt.
    func close() throws
}

public enum TerminalModelPresentation: Sendable {
    case frame(Data)
    /// The canonical model is still live. An unsupported or over-budget viewport
    /// cannot be presented as a faithful current terminal or enable stale input.
    case unavailable(TerminalRenderError)
}

/// The session owner provides both destinations. Outcomes are never discarded
/// just because a viewport is hidden. A thrown delivery leaves its complete
/// batch in the coordinator for reconciliation; side effects must be idempotent.
public protocol TerminalModelDelivery: Sendable {
    func present(_ presentation: TerminalModelPresentation, source: TerminalSource) async throws
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) async throws
}

public struct TerminalModelAccounting: Equatable, Sendable {
    public let queuedReplies: UInt64, droppedReplies: UInt64
    public let publishedFrames: UInt64, unavailableFrames: UInt64
    public let endOfOutput: Bool, closing: Bool, closed: Bool
    public let waitingPresentationRequests: Int, waitingViewportRequests: Int, waitingPaletteRequests: Int
    public let waitingHistoryRequests: Int
}

/// One source-bound owner joins the real SSH session pump, canonical terminal,
/// keyboard encoder, answerbacks and native display. No task is created per byte,
/// key, reply or frame. The pump awaits complete consumption before reading again.
///
/// Only one model transaction/publication runs at a time, with one bounded
/// waiting consumer, keyboard transaction and presentation request. During an awaited
/// reply/delivery handoff, read-only encoding may inspect already-applied modes.
/// All egress still enters the same coordinator.
public actor TerminalModelRuntime: TerminalKeyboardTransactions, TerminalSessionConsumer {
    public nonisolated let source: TerminalSource
    public nonisolated let supportsPaste = true
    private let model: any TerminalSessionModel
    private let coordinator: TerminalInputCoordinator
    private let delivery: any TerminalModelDelivery
    private var operationActive = false
    private enum OperationKind: CaseIterable { case consumer, keyboard, presentation, viewport, palette, font, history }
    private struct WaitingOperation {
        let kind: OperationKind
        let continuation: CheckedContinuation<Void, Error>
    }
    // One pump may retain its current bounded chunk while one keyboard owner
    // waits for an input transaction. One presentation request may also wait;
    // it retains no terminal bytes. No kind may install a second waiter.
    private var waitingOperations: [WaitingOperation] = []
    private var closeWaiter: CheckedContinuation<Void, Never>?
    private var closeTask: Task<Void, Error>?
    private var closing = false, closed = false, endOfOutput = false
    private var visible = true
    /// Called the first time output arrives while the terminal is hidden after
    /// it was on screen, and again only after it is shown again. Output before
    /// the terminal was ever shown (a new shell's greeting) is not news, nor is
    /// output within `hiddenOutputSettle` of hiding: the keyboard leaving with
    /// the machine resizes its terminal, and the shell's redraw follows.
    private let hiddenOutput: (@Sendable () -> Void)?
    private let hiddenOutputSettle: Duration
    private var shownOnce = false, hiddenOutputReported = false
    private var hiddenAt: ContinuousClock.Instant?
    /// The person's input left this phone after the machine's last output.
    private var inputUnanswered = false
    /// Hears each change of `inputUnanswered`, to keep it past a killed launch.
    private let awaitingAnswer: (@Sendable (Bool) -> Void)?
    private var visibilityGeneration = UUID()
    private var viewportTop: Int64?
    private let historyOwner = UUID()
    private var queuedReplies: UInt64 = 0, droppedReplies: UInt64 = 0
    private var publishedFrames: UInt64 = 0, unavailableFrames: UInt64 = 0

    public init(model: any TerminalSessionModel, coordinator: TerminalInputCoordinator,
                delivery: any TerminalModelDelivery, initiallyVisible: Bool = true,
                hiddenOutput: (@Sendable () -> Void)? = nil, hiddenOutputSettle: Duration = .seconds(3),
                awaitingAnswer: (@Sendable (Bool) -> Void)? = nil) {
        self.model = model; source = model.source
        self.coordinator = coordinator; self.delivery = delivery
        visible = initiallyVisible; shownOnce = initiallyVisible
        self.hiddenOutput = hiddenOutput; self.hiddenOutputSettle = hiddenOutputSettle
        self.awaitingAnswer = awaitingAnswer
    }

    public func accounting() -> TerminalModelAccounting {
        .init(queuedReplies: queuedReplies, droppedReplies: droppedReplies,
              publishedFrames: publishedFrames, unavailableFrames: unavailableFrames,
              endOfOutput: endOfOutput, closing: closing, closed: closed,
              waitingPresentationRequests: waitingOperations.filter { $0.kind == .presentation }.count,
              waitingViewportRequests: waitingOperations.filter { $0.kind == .viewport }.count,
              waitingPaletteRequests: waitingOperations.filter { $0.kind == .palette }.count,
              waitingHistoryRequests: waitingOperations.filter { $0.kind == .history }.count)
    }

    /// Whether typed or pasted input left this phone after the machine's last
    /// output. If the connection ends now, that input may or may not have
    /// arrived. The pump delivers a write's outcome before any later read.
    public func inputAwaitsAnswer() -> Bool { inputUnanswered }

    private func requireSource(_ source: TerminalSource) throws {
        guard source == self.source else { throw TerminalInputError.stale }
    }
    private func beginOperation() throws {
        try Task.checkCancellation()
        guard !closing, !closed else { throw TerminalModelError.closed }
        guard !operationActive else { throw TerminalModelError.busy }
        operationActive = true
    }
    private func finishOperation() {
        operationActive = false
        if !closing, !waitingOperations.isEmpty {
            let next = waitingOperations.removeFirst()
            operationActive = true // Reserve ownership before resuming a waiter.
            next.continuation.resume()
            return
        }
        let waiter = closeWaiter; closeWaiter = nil
        waiter?.resume()
    }
    private func acquireOperation(_ kind: OperationKind) async throws {
        try Task.checkCancellation()
        guard !closing, !closed else { throw TerminalModelError.closed }
        if operationActive {
            guard waitingOperations.count < OperationKind.allCases.count,
                  !waitingOperations.contains(where: { $0.kind == kind }) else {
                throw TerminalModelError.busy
            }
            try await withCheckedThrowingContinuation { continuation in
                waitingOperations.append(.init(kind: kind, continuation: continuation))
            }
            // Ownership has already transferred. Release it even if this
            // bounded waiter was cancelled while another operation completed.
            do {
                try Task.checkCancellation()
                guard !closing, !closed else { throw TerminalModelError.closed }
            } catch { finishOperation(); throw error }
        } else { operationActive = true }
    }
    private func requireInput(_ source: TerminalSource, maximumBytes: Int) throws {
        try requireSource(source); try Task.checkCancellation()
        guard !closing, !closed, !endOfOutput else { throw TerminalModelError.closed }
        guard (1...65_536).contains(maximumBytes) else { throw TerminalModelError.capacity }
    }

    public func encode(_ intent: TerminalKeyboardIntent, source: TerminalSource,
                       maximumBytes: Int) async throws -> Data {
        try requireInput(source, maximumBytes: maximumBytes)
        let bytes = try model.encode(intent, maximumBytes: maximumBytes)
        guard bytes.count <= maximumBytes else { throw TerminalModelError.capacity }
        try Task.checkCancellation()
        return bytes
    }

    public func preparePaste(_ text: String, source: TerminalSource,
                             maximumBytes: Int) async throws -> TerminalPreparedPaste {
        try requireInput(source, maximumBytes: maximumBytes)
        guard text.utf8.count <= maximumBytes else { throw TerminalModelError.capacity }
        let prepared = try model.preparePaste(text, maximumBytes: maximumBytes)
        guard prepared.text.utf8.count <= maximumBytes,
              !prepared.text.unicodeScalars.contains(where: {
                  ($0.value < 32 && $0.value != 9 && $0.value != 10 && $0.value != 13)
                      || (127...159).contains($0.value)
              }) else { throw TerminalModelError.invalid }
        try Task.checkCancellation()
        return prepared
    }

    /// The keyboard's existing single pump brackets encoding and coordinator
    /// admission together. Remote bytes cannot change canonical keyboard/paste
    /// modes in between them. No task or second egress queue is created here.
    public func withInputTransaction<Value: Sendable>(source: TerminalSource,
        operation: @Sendable () async throws -> Value) async throws -> Value {
        try requireSource(source)
        try await acquireOperation(.keyboard); defer { finishOperation() }
        guard !endOfOutput else { throw TerminalModelError.closed }
        // The callback may encode and admit to the coordinator, but must not
        // call receive, another transaction, or await this runtime's close.
        // Successful admission must return even if cancellation races it.
        return try await operation()
    }

    public func receive(_ update: TerminalSessionUpdate, source: TerminalSource) async throws {
        try requireSource(source)
        guard update.output.count <= 16_384 else { throw TerminalModelError.capacity }
        try await acquireOperation(.consumer); defer { finishOperation() }
        guard !endOfOutput else { throw TerminalModelError.closed }
        // SSH service reports its completed window change with the output read
        // after it. Apply that geometry before advancing the same ordered bytes.
        if let geometry = update.resized { try model.resize(geometry) }
        if !update.output.isEmpty {
            try model.feed(update.output)
            if inputUnanswered { inputUnanswered = false; awaitingAnswer?(false) }
            if !visible, shownOnce, !hiddenOutputReported,
               hiddenAt.map({ ContinuousClock.now - $0 >= hiddenOutputSettle }) ?? true {
                hiddenOutputReported = true; hiddenOutput?()
            }
        }
        if update.endOfOutput { endOfOutput = true }
        try await transferReplies()
        try Task.checkCancellation()
        if visible, !closing { try await publishFrame() }
    }

    public func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) async throws {
        try requireSource(source)
        guard outcomes.count <= 64, outcomes.allSatisfy({ $0.receipt.source == source }) else {
            throw TerminalModelError.invalid
        }
        try await acquireOperation(.consumer); defer { finishOperation() }
        if !inputUnanswered,
           outcomes.contains(where: { ($0.kind == .encodedInput || $0.kind == .paste) && $0.disposition == .transportAccepted }) {
            inputUnanswered = true; awaitingAnswer?(true)
        }
        if !outcomes.isEmpty { try await delivery.receive(outcomes, source: source) }
        // No cancellation check after successful delivery: the pump must be
        // allowed to acknowledge exactly the batch its owner has accepted.
    }

    private func transferReplies() async throws {
        // The canonical staging ring owns at most 128 complete replies. Refuse
        // a corrupt adapter that keeps manufacturing units instead of yielding.
        for _ in 0..<128 {
            try Task.checkCancellation()
            if closing { return }
            guard let reply = try model.copyReply() else { return }
            let result: TerminalReplyResult
            do { result = try await coordinator.reply(source, encodedBytes: reply.bytes) }
            catch {
                // The concrete coordinator throws only before enqueueing.
                // Release this whole rejected copy before propagating failure;
                // native close must never inherit an unsettled reply lease.
                try model.settleReply(sequence: reply.sequence, disposition: .droppedWhole)
                if droppedReplies < UInt64.max { droppedReplies += 1 }
                throw error
            }
            // Once accepted/dropped, settle BEFORE honoring cancellation or
            // releasing model ownership. Retrying this reply would duplicate it.
            switch result {
            case .queued:
                try model.settleReply(sequence: reply.sequence, disposition: .accepted)
                if queuedReplies < UInt64.max { queuedReplies += 1 }
            case .droppedWhole:
                try model.settleReply(sequence: reply.sequence, disposition: .droppedWhole)
                if droppedReplies < UInt64.max { droppedReplies += 1 }
            }
        }
        if let extra = try model.copyReply() {
            try model.settleReply(sequence: extra.sequence, disposition: .droppedWhole)
            if droppedReplies < UInt64.max { droppedReplies += 1 }
            throw TerminalModelError.capacity
        }
    }

    private func publishFrame() async throws {
        let presentation: TerminalModelPresentation
        do {
            let bytes = try captureSelectedViewport()
            guard !bytes.isEmpty, bytes.count <= 8 * 1024 * 1024 else { throw TerminalModelError.capacity }
            presentation = .frame(bytes)
        } catch TerminalModelError.unsupported {
            presentation = .unavailable(.unsupported)
        } catch TerminalModelError.capacity {
            presentation = .unavailable(.capacity)
        } catch TerminalModelError.viewportUnavailable {
            // Scrollback eviction invalidates a historical view, not the live
            // SSH model. Preserve output/replies until the owner returns live.
            presentation = .unavailable(.unavailable)
        }
        try await delivery.present(presentation, source: source)
        switch presentation {
        case .frame: if publishedFrames < UInt64.max { publishedFrames += 1 }
        case .unavailable: if unavailableFrames < UInt64.max { unavailableFrames += 1 }
        }
    }

    /// Output evicts the oldest history first, and a resize or screen switch
    /// can move it, so a selected history row may no longer be resident. Keep
    /// the reader as close as the retained history allows: its oldest row, or
    /// the live screen when no history remains.
    private func captureSelectedViewport() throws -> Data {
        guard let top = viewportTop else { return try model.captureFrame(viewportTop: nil) }
        do { return try model.captureFrame(viewportTop: top) }
        catch TerminalModelError.viewportUnavailable {
            let live = try model.captureFrame(viewportTop: nil)
            guard let history = TerminalRenderFrame.history(ofEncoded: live) else {
                viewportTop = nil; return live
            }
            let clamped = min(max(top, history.first), history.live)
            viewportTop = nil
            guard clamped != history.live, clamped != top,
                  let retained = try? model.captureFrame(viewportTop: clamped) else { return live }
            viewportTop = clamped
            return retained
        }
    }

    /// Hidden terminals keep consuming output and producing answerbacks. They
    /// allocate no new display snapshot until made visible or explicitly refreshed.
    public func setVisible(_ visible: Bool, source: TerminalSource) async throws {
        try requireSource(source); try Task.checkCancellation()
        guard !closing, !closed else { throw TerminalModelError.closed }
        if !visible {
            // Hiding is a lifecycle state change, not a model mutation. It must
            // take effect even while the already-owned frame is being delivered.
            visibilityGeneration = UUID()
            if self.visible { hiddenAt = ContinuousClock.now }
            self.visible = false
            return
        }
        let generation = visibilityGeneration
        // A swipe back to a busy terminal is ordinary scheduling, not a fatal
        // display error. Wait once behind the current canonical transaction.
        // A newer hide invalidates this request without waiting for that work.
        try await acquireOperation(.presentation); defer { finishOperation() }
        guard visibilityGeneration == generation else { return }
        self.visible = true
        shownOnce = true; hiddenOutputReported = false
        try await publishFrame()
    }
    public func refresh(source: TerminalSource) async throws {
        try requireSource(source); try beginOperation(); defer { finishOperation() }
        if visible { try await publishFrame() }
    }
    /// Selects a history viewport (nil is the live screen). Like presentation,
    /// a request waits once behind output instead of failing a busy terminal.
    public func setViewport(top: Int64?, source: TerminalSource) async throws {
        try requireSource(source)
        guard top != Int64.min else { throw TerminalModelError.invalid }
        try await acquireOperation(.viewport); defer { finishOperation() }
        viewportTop = top
        if visible { try await publishFrame() }
    }

    /// Read-only canonical history for a bounded search. Each page owns one
    /// model transaction and releases it before the search scans or requests
    /// another page, so output and keyboard work can continue between pages.
    /// Capturing never changes the selected viewport or publishes a frame.
    public func captureSearchPage(top: Int64?, source: TerminalSource) async throws
        -> TerminalSearchPage<TerminalSource> {
        try requireSource(source)
        guard top != Int64.min else { throw TerminalHistorySearchError.invalidPage }
        try await acquireOperation(.history); defer { finishOperation() }
        return try searchPage(top: top)
    }

    /// Capture after the UI has joined any prior scroll request. The result is
    /// intentionally not Codable: a restored app has no authority to reuse it.
    public func captureHistoryBookmark(source: TerminalSource) async throws -> TerminalHistoryBookmark {
        try requireSource(source)
        try await acquireOperation(.history); defer { finishOperation() }
        let page = try searchPage(top: viewportTop)
        return TerminalHistoryBookmark(source: source, viewportTop: viewportTop, owner: historyOwner,
            geometryRevision: page.geometryRevision, alternateScreen: page.alternateScreen,
            rowSequence: page.rows[0].sequence)
    }

    /// Validate the exact match and select its viewport in the SAME canonical
    /// transaction. A stale result cannot move the viewport to unrelated text.
    /// This operation admits no input and never requests a remote resize.
    public func revealSearchMatch(_ match: TerminalHistoryMatch<TerminalSource>, source: TerminalSource) async throws {
        try requireSource(source)
        guard match.source == source else { throw TerminalHistorySearchError.staleResult }
        try await acquireOperation(.viewport); defer { finishOperation() }
        let live = try searchPage(top: nil)
        guard match.row >= live.first,
              match.row <= live.live + Int64(live.rows.count - 1) else {
            throw TerminalHistorySearchError.staleResult
        }
        let top = min(match.row, live.live)
        let page: TerminalSearchPage<TerminalSource>
        do { page = top == live.top ? live : try searchPage(top: top) }
        catch TerminalHistorySearchError.unavailable { throw TerminalHistorySearchError.staleResult }
        try TerminalHistorySearch.validate(match, in: page)
        try Task.checkCancellation()
        viewportTop = top
        if visible { try await publishFrame() }
    }

    /// Return to the pre-search position only while its canonical anchors still
    /// identify it. Expiry is explicit; callers may offer Return to Live as a
    /// separate deliberate action instead of silently selecting another row.
    public func restoreHistoryBookmark(_ bookmark: TerminalHistoryBookmark, source: TerminalSource) async throws {
        try requireSource(source)
        guard bookmark.source == source, bookmark.owner == historyOwner else {
            throw TerminalHistorySearchError.staleResult
        }
        try await acquireOperation(.viewport); defer { finishOperation() }
        let page: TerminalSearchPage<TerminalSource>
        do { page = try searchPage(top: bookmark.viewportTop) }
        catch TerminalHistorySearchError.unavailable { throw TerminalHistorySearchError.staleResult }
        guard page.geometryRevision == bookmark.geometryRevision,
              page.alternateScreen == bookmark.alternateScreen,
              bookmark.viewportTop == nil || page.rows[0].sequence == bookmark.rowSequence else {
            throw TerminalHistorySearchError.staleResult
        }
        try Task.checkCancellation()
        viewportTop = bookmark.viewportTop
        if visible { try await publishFrame() }
    }

    /// A bounded copy of the latest received live viewport, useful after EOF
    /// and before close destroys the canonical model. Set liveViewport to false
    /// to copy the selected history viewport for a search preview. Neither mode
    /// navigates or publishes a frame. It is not the entire
    /// transcript, a claim that Metal presented it, or restored input authority.
    /// Concealed cells are suppressed by TerminalCopyDocument. Incomplete
    /// synchronized output is refused, just like the live Copy action.
    public func captureOutputDocument(source: TerminalSource, liveViewport: Bool = true) async throws -> TerminalCopyDocument {
        try requireSource(source)
        try await acquireOperation(.history); defer { finishOperation() }
        let top = liveViewport ? nil : viewportTop
        let frame = try decodedHistoryFrame(top: top)
        guard frame.flags & 8 == 0 else { throw TerminalClipboardError.unavailable }
        if let expected = top ?? frame.history?.live, frame.viewportFirst != expected {
            throw TerminalHistorySearchError.invalidPage
        }
        return try TerminalCopyDocument(frame: frame)
    }

    private func searchPage(top: Int64?) throws -> TerminalSearchPage<TerminalSource> {
        let page = try TerminalSearchPage(frame: decodedHistoryFrame(top: top))
        guard page.top == (top ?? page.live) else { throw TerminalHistorySearchError.invalidPage }
        return page
    }

    private func decodedHistoryFrame(top: Int64?) throws -> TerminalRenderFrame {
        do {
            let bytes = try model.captureFrame(viewportTop: top)
            guard !bytes.isEmpty else { throw TerminalHistorySearchError.invalidPage }
            guard bytes.count <= 8 * 1024 * 1024 else { throw TerminalHistorySearchError.capacity }
            return try TerminalRenderFrame.decode(bytes, expectedSource: source)
        } catch TerminalModelError.viewportUnavailable {
            throw TerminalHistorySearchError.unavailable
        } catch TerminalModelError.unsupported {
            throw TerminalHistorySearchError.unavailable
        } catch TerminalModelError.capacity {
            throw TerminalHistorySearchError.capacity
        } catch TerminalRenderError.capacity {
            throw TerminalHistorySearchError.capacity
        } catch is TerminalRenderError {
            throw TerminalHistorySearchError.invalidPage
        }
    }
    /// Re-themes the terminal. Like a scroll, a request waits once behind
    /// output instead of failing; the next frame carries the new palette.
    public func setBasePalette(_ rgba: [UInt8], source: TerminalSource) async throws {
        try requireSource(source)
        try await acquireOperation(.palette); defer { finishOperation() }
        try model.setBasePalette(rgba)
        if visible { try await publishFrame() }
    }
    /// Marks the frames that follow as laid out for the renderer's new font
    /// (a text-size change). Like a palette change, it waits once behind
    /// output instead of failing.
    public func setFontRevision(_ revision: UInt64, source: TerminalSource) async throws {
        try requireSource(source)
        try await acquireOperation(.font); defer { finishOperation() }
        try model.setFontRevision(revision)
        if visible { try await publishFrame() }
    }

    /// Safe from a delivery callback: close admission now, but do not await the
    /// operation containing that callback. The owner later joins with close().
    public func requestClose() {
        guard !closed, closeTask == nil else { return }
        closing = true
        let pending = waitingOperations; waitingOperations.removeAll()
        for waiter in pending { waiter.continuation.resume(throwing: TerminalModelError.closed) }
        closeTask = Task { try await self.finishClose() }
    }
    /// Join model cleanup after the current receive/delivery settles. Calling
    /// this from inside this runtime's delivery callback would await that same
    /// callback; use requestClose there. Transport shutdown is owned by the pump.
    public func close() async throws {
        if closed { return }
        requestClose()
        try await closeTask?.value
    }
    private func finishClose() async throws {
        if operationActive { await withCheckedContinuation { closeWaiter = $0 } }
        do { try model.close(); closed = true }
        catch { closeTask = nil; throw error }
    }
}
