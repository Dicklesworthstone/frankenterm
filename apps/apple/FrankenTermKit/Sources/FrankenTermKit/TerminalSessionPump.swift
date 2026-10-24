import Foundation

/// One coalescing wake bit and one waiter. A key, answerback or cancellation
/// never allocates a new task or a growing notification queue. All state is
/// protected by lock; continuations are always resumed after releasing it.
public final class TerminalSessionSignal: @unchecked Sendable {
    private let lock = NSLock()
    private var pending = false, ended = false
    private var waiter: CheckedContinuation<Bool, Never>?
    public init() {}
    public func notify() {
        lock.lock()
        guard !ended else { lock.unlock(); return }
        let current = waiter; waiter = nil
        if current == nil { pending = true }
        lock.unlock()
        current?.resume(returning: true)
    }
    public func finish() {
        lock.lock()
        ended = true; pending = false
        let current = waiter; waiter = nil
        lock.unlock()
        current?.resume(returning: false)
    }
    public var isFinished: Bool {
        lock.lock(); defer { lock.unlock() }; return ended
    }
    /// Single-consumer contract. A competing consumer is refused, not installed
    /// over a continuation that would then never resume. Cancellation retires the
    /// entire signal; the owner joins its single driver before discarding it.
    public func wait() async -> Bool {
        await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                lock.lock()
                if ended || Task.isCancelled || waiter != nil {
                    lock.unlock(); continuation.resume(returning: false)
                } else if pending {
                    pending = false; lock.unlock(); continuation.resume(returning: true)
                } else { waiter = continuation; lock.unlock() }
            }
        } onCancel: { self.finish() }
    }
}

public struct TerminalSessionGeometry: Equatable, Sendable {
    public let columns: UInt32, rows: UInt32, pixelWidth: UInt32, pixelHeight: UInt32
    /// The most cells a grid the phone's terminal shows may have: its model
    /// keeps both screens (primary and alternate) within 65,536 resident
    /// cells, beside whatever history still fits, and refuses a larger grid.
    public static let maximumShownCells = 32768
    public init(columns: UInt32, rows: UInt32, pixelWidth: UInt32 = 0, pixelHeight: UInt32 = 0) throws {
        guard (1...512).contains(columns), (1...512).contains(rows), columns * rows <= 65536,
              pixelWidth <= 65535, pixelHeight <= 65535 else { throw TerminalInputError.invalidLimits }
        self.columns = columns; self.rows = rows; self.pixelWidth = pixelWidth; self.pixelHeight = pixelHeight
    }
}

/// Evidence reported by the peer, never a command replay or input-effect ACK.
public enum TerminalRemoteExit: Equatable, Sendable {
    case exitStatus(UInt32)
    case signal(String)
}

public struct TerminalSessionCompletion: Equatable, Sendable {
    /// nil means unavailable. Some backends cannot distinguish a reported zero
    /// from an absent exit status; callers must not render that as success.
    public let remoteExit: TerminalRemoteExit?
    /// False means output EOF was drained but the bounded wait for channel
    /// closure expired. EOF alone never proves that the remote process stopped.
    public let channelClosed: Bool
    public init(remoteExit: TerminalRemoteExit?, channelClosed: Bool) {
        self.remoteExit = remoteExit; self.channelClosed = channelClosed
    }
}

public struct TerminalSessionUpdate: Sendable {
    public let output: Data
    public let endOfOutput: Bool
    public let completion: TerminalSessionCompletion?
    /// Ordinary SSH window change has no server geometry acknowledgement. This
    /// is only the transport's local completion, still bound to this source.
    public let resized: TerminalSessionGeometry?
    /// Present only on the final chunk of one complete mux output event. The
    /// pump acknowledges it after the canonical consumer succeeds, never merely
    /// because the pane list or transport's selected pane changed.
    public let muxDestination: TerminalMuxInputRouting.Destination?
    public init(output: Data, endOfOutput: Bool = false, resized: TerminalSessionGeometry? = nil,
                completion: TerminalSessionCompletion? = nil,
                muxDestination: TerminalMuxInputRouting.Destination? = nil) {
        self.output = output; self.endOfOutput = endOfOutput; self.resized = resized; self.completion = completion
        self.muxDestination = muxDestination
    }
}

/// One already-authenticated phone-owned PTY, never a mux mirror. service makes
/// bounded network progress and may return an empty update after an idle expiry
/// or local wake. wakeService must latch a wake even before service starts.
/// close must abort service and settle all owned network work.
public protocol TerminalSessionTransport: TerminalByteTransport {
    var inputSignal: TerminalSessionSignal { get }
    func service() async throws -> TerminalSessionUpdate
    func wakeService() async
    /// Local model-consumption evidence, not remote delivery or rendering proof.
    /// Must be bounded and must not wait for this pump or perform another read.
    func didApplyOutput(_ update: TerminalSessionUpdate) async
}

public extension TerminalSessionTransport {
    func inputAvailable() { inputSignal.notify() }
    func didApplyOutput(_ update: TerminalSessionUpdate) async {}
}

/// The canonical terminal adapter consumes a chunk completely before returning;
/// it must not enqueue bytes into an unbounded parser or fire-and-forget task.
/// Replies use the SAME coordinator and source. A mux mirror is not an adapter
/// for this interface. No alternate ANSI parser or local echo is supplied here.
public protocol TerminalSessionConsumer: Sendable {
    func receive(_ update: TerminalSessionUpdate, source: TerminalSource) async throws
    /// Success acknowledges delivery of content-free disposition records only.
    /// A thrown/cancelled delivery leaves its complete batch in the coordinator.
    /// Partial downstream side effects require the consumer's own idempotency.
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) async throws
}

/// Thrown by a transport that closed itself because its connection failed,
/// not because an owner closed it. The attachment ends as disconnected even
/// though that self-close also finished the input signal.
public enum TerminalTransportError: Error, Equatable, Sendable {
    /// The peer stopped answering, including the replies liveness probes ask for.
    case peerUnresponsive
    /// The network connection itself failed: reset, closed or unreadable.
    case connectionLost
}

public enum TerminalSessionEnd: Equatable, Sendable {
    case cancelled, disconnected, transportFailed, consumerFailed, ownershipConflict
    /// Client policy ends this attachment after peer output EOF. This is NOT an
    /// exit code, process-completion proof, or a promise the remote PTY survives.
    case peerOutputEnded
    /// Delivered only after the consumer has applied all final received output.
    /// Local cancellation and transport loss keep their separate origins.
    case completed(TerminalSessionCompletion)
}

/// Joins the real transport and input coordinator. There is one bounded pump and
/// one coalescing-wake child, not one task per key, pane update or camera frame.
/// The owner retains run's task through completion. requestStop is safe from a
/// consumer callback; awaiting the same run task from inside that callback is not.
///
/// Construct only after authentication, explicit PTY creation, and the session
/// owner's durable uncertainty marker. This driver neither authorizes network
/// use nor replaces that marker, the canonical encoder, or the Rust model.
public actor TerminalSessionPump {
    public nonisolated let source: TerminalSource
    private let transport: any TerminalSessionTransport
    private let coordinator: TerminalInputCoordinator
    private let signal: TerminalSessionSignal
    private var started = false, finished = false
    private var finalizing = false, reconciling = false
    private let writesPerTurn = 8

    private init(source: TerminalSource, transport: any TerminalSessionTransport,
                 coordinator: TerminalInputCoordinator) {
        self.source = source; self.transport = transport; self.coordinator = coordinator
        signal = transport.inputSignal
    }
    /// Successful attachment transfers driver ownership. Failure leaves the
    /// caller responsible for its transport. The source never changes afterward.
    public static func attach(_ transport: any TerminalSessionTransport,
                              coordinator: TerminalInputCoordinator) async throws -> TerminalSessionPump {
        try Task.checkCancellation()
        guard !transport.inputSignal.isFinished else { throw TerminalInputError.unavailable }
        let source = try await coordinator.attach(transport)
        return TerminalSessionPump(source: source, transport: transport, coordinator: coordinator)
    }
    public nonisolated func requestStop() { signal.finish() }

    /// Also usable before run. Finishing run is the join point for consumer work;
    /// disconnect alone intentionally does not wait on a suspended consumer.
    public func disconnect() async throws {
        signal.finish()
        try await coordinator.disconnect(source)
    }

    /// Explicit final metadata delivery after the sole run has settled (or an
    /// attachment was stopped before run). Refuse overlapping reconciliation and
    /// live consumers rather than consuming records that they still own.
    ///
    /// A throwing delivery retains its exact unacknowledged prefix. Successful
    /// delivery is acknowledged even if cancellation races its return. Retrying
    /// this method can only redeliver metadata, NEVER input bytes; partial delivery
    /// effects must be idempotent. The session's durable uncertainty marker is
    /// independent and is not cleared here. Retire responder focus separately,
    /// then coordinator.retire(source) only after this operation succeeds.
    @discardableResult
    public func reconcileStoppedOutcomes(
        deliver: @Sendable ([TerminalInputOutcome], TerminalSource) async throws -> Void
    ) async throws -> Int {
        try Task.checkCancellation()
        guard signal.isFinished, !started || finished, !reconciling else {
            throw TerminalInputError.unavailable
        }
        finalizing = true; reconciling = true
        defer { reconciling = false }
        try await coordinator.disconnect(source)
        var delivered = 0
        // Admission is closed and the sole writer has joined. The ledger can
        // only shrink; its configured per-source record cap bounds this loop.
        while true {
            try Task.checkCancellation()
            let batch = try await coordinator.pendingOutcomes(source)
            guard !batch.outcomes.isEmpty else { return delivered }
            try await deliver(batch.outcomes, source)
            try await coordinator.acknowledgeOutcomes(batch)
            delivered += batch.outcomes.count
        }
    }

    /// Exactly one run; focus and responder generations remain owned by the
    /// coordinator/UI. This does not retire their handles or discard outcomes.
    /// onResizeApplied runs only after the canonical consumer has applied the
    /// ordered update, even when its viewport is hidden. It is not a peer ACK.
    /// Keep the callback bounded; never await this pump's run/close from it.
    public func run(consumer: any TerminalSessionConsumer,
                    onResizeApplied: (@Sendable (TerminalSessionGeometry, TerminalSource) async throws -> Void)? = nil)
        async throws -> TerminalSessionEnd {
        guard !started, !finalizing else { throw TerminalInputError.unavailable }
        started = true
        defer { finished = true }
        let result = await withTaskCancellationHandler {
            await withTaskGroup(of: Void.self, returning: TerminalSessionEnd.self) { group in
                let signal = self.signal, transport = self.transport
                let coordinator = self.coordinator, source = self.source
                group.addTask {
                    while await signal.wait() { await transport.wakeService() }
                    // Retire input admission even while the consumer is suspended.
                    // Closing only the socket would leave the coordinator accepting
                    // new input until that unrelated callback returned.
                    do { try await coordinator.disconnect(source) }
                    catch { await transport.close() }
                }
                let reason = await self.drive(consumer: consumer, onResizeApplied: onResizeApplied)
                signal.finish()
                // Invalidate admission before any further await. Existing queued
                // work receives its real disposition on its original source.
                try? await self.coordinator.disconnect(self.source)
                group.cancelAll()
                await group.waitForAll()
                // Do not consume final outcomes on cancellation or failure. They
                // remain bounded and charged, available for explicit reconciliation.
                return reason
            }
        } onCancel: { self.signal.finish() }
        return result
    }

    private func drive(consumer: any TerminalSessionConsumer,
                       onResizeApplied: (@Sendable (TerminalSessionGeometry, TerminalSource) async throws -> Void)?)
        async -> TerminalSessionEnd {
        while !signal.isFinished && !Task.isCancelled {
            do {
                // Fairness: an input flood cannot indefinitely starve terminal
                // output or resize. Each turn leases at most eight normal chunks.
                var exhaustedBudget = true
                for _ in 0..<writesPerTurn {
                    if signal.isFinished || Task.isCancelled { return .cancelled }
                    switch try await coordinator.drainOne(source) {
                    case .progressed: continue
                    case .idle, .wouldBlock: exhaustedBudget = false; break
                    case .busy: return .ownershipConflict
                    case .connectionClosed: return .disconnected
                    }
                    break
                }
                // Work predating the last coalesced notification may still be
                // queued when the fairness budget expires. Latch another turn.
                if exhaustedBudget { signal.notify() }
            } catch {
                return signal.isFinished || Task.isCancelled ? .cancelled : .transportFailed
            }
            do {
                let batch = try await coordinator.pendingOutcomes(source)
                if !batch.outcomes.isEmpty {
                    try await consumer.receive(batch.outcomes, source: source)
                    // Once the consumer reported success, acknowledgement must
                    // complete even if cancellation arrives at this await boundary.
                    try await coordinator.acknowledgeOutcomes(batch)
                    // Cancellation may have produced more than one batch without
                    // further socket activity. Keep metadata delivery progressing.
                    if batch.outcomes.count == 64 { signal.notify() }
                }
            } catch { return signal.isFinished || Task.isCancelled ? .cancelled : .consumerFailed }
            if signal.isFinished || Task.isCancelled { return .cancelled }
            let update: TerminalSessionUpdate
            do { update = try await transport.service() }
            catch is TerminalTransportError { return Task.isCancelled ? .cancelled : .disconnected }
            catch { return signal.isFinished || Task.isCancelled ? .cancelled : .transportFailed }
            // A corrupt adapter cannot push an oversized receive into the parser.
            guard update.output.count <= 16_384, update.completion == nil || update.endOfOutput else { return .transportFailed }
            if signal.isFinished || Task.isCancelled { return .cancelled }
            if !update.output.isEmpty || update.resized != nil || update.endOfOutput || update.muxDestination != nil {
                do {
                    try await consumer.receive(update, source: source)
                    if !signal.isFinished, !Task.isCancelled, let geometry = update.resized {
                        try await onResizeApplied?(geometry, source)
                    }
                    if !signal.isFinished, !Task.isCancelled, !update.endOfOutput {
                        await transport.didApplyOutput(update)
                    }
                } catch { return signal.isFinished || Task.isCancelled ? .cancelled : .consumerFailed }
            }
            if signal.isFinished || Task.isCancelled { return .cancelled }
            // Deliver all final bytes first. Stop this attachment deliberately;
            // never turn repeated native EOF into a busy loop or an exit(0) claim.
            if update.endOfOutput {
                return update.completion.map(TerminalSessionEnd.completed) ?? .peerOutputEnded
            }
        }
        return .cancelled
    }
}
