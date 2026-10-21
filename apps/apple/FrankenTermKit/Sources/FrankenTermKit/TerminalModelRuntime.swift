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
    public let waitingPresentationRequests: Int
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
    private enum OperationKind { case consumer, keyboard, presentation }
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
    private var visibilityGeneration = UUID()
    private var viewportTop: Int64?
    private var queuedReplies: UInt64 = 0, droppedReplies: UInt64 = 0
    private var publishedFrames: UInt64 = 0, unavailableFrames: UInt64 = 0

    public init(model: any TerminalSessionModel, coordinator: TerminalInputCoordinator,
                delivery: any TerminalModelDelivery, initiallyVisible: Bool = true) {
        self.model = model; source = model.source
        self.coordinator = coordinator; self.delivery = delivery
        visible = initiallyVisible
    }

    public func accounting() -> TerminalModelAccounting {
        .init(queuedReplies: queuedReplies, droppedReplies: droppedReplies,
              publishedFrames: publishedFrames, unavailableFrames: unavailableFrames,
              endOfOutput: endOfOutput, closing: closing, closed: closed,
              waitingPresentationRequests: waitingOperations.filter { $0.kind == .presentation }.count)
    }

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
            guard waitingOperations.count < 3, !waitingOperations.contains(where: { $0.kind == kind }) else {
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
        if !update.output.isEmpty { try model.feed(update.output) }
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
            let bytes = try model.captureFrame(viewportTop: viewportTop)
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

    /// Hidden terminals keep consuming output and producing answerbacks. They
    /// allocate no new display snapshot until made visible or explicitly refreshed.
    public func setVisible(_ visible: Bool, source: TerminalSource) async throws {
        try requireSource(source); try Task.checkCancellation()
        guard !closing, !closed else { throw TerminalModelError.closed }
        if !visible {
            // Hiding is a lifecycle state change, not a model mutation. It must
            // take effect even while the already-owned frame is being delivered.
            visibilityGeneration = UUID()
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
        try await publishFrame()
    }
    public func refresh(source: TerminalSource) async throws {
        try requireSource(source); try beginOperation(); defer { finishOperation() }
        if visible { try await publishFrame() }
    }
    public func setViewport(top: Int64?, source: TerminalSource) async throws {
        try requireSource(source)
        guard top != Int64.min else { throw TerminalModelError.invalid }
        try beginOperation(); defer { finishOperation() }
        viewportTop = top
        if visible { try await publishFrame() }
    }
    public func setFontRevision(_ revision: UInt64, source: TerminalSource) async throws {
        try requireSource(source); try beginOperation(); defer { finishOperation() }
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
