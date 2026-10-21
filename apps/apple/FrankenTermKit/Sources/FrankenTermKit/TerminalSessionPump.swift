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
    public init(columns: UInt32, rows: UInt32, pixelWidth: UInt32 = 0, pixelHeight: UInt32 = 0) throws {
        guard (1...512).contains(columns), (1...512).contains(rows), columns * rows <= 65536,
              pixelWidth <= 65535, pixelHeight <= 65535 else { throw TerminalInputError.invalidLimits }
        self.columns = columns; self.rows = rows; self.pixelWidth = pixelWidth; self.pixelHeight = pixelHeight
    }
}

public struct TerminalSessionUpdate: Sendable {
    public let output: Data
    public let endOfOutput: Bool
    /// Ordinary SSH window change has no server geometry acknowledgement. This
    /// is only the transport's local completion, still bound to this source.
    public let resized: TerminalSessionGeometry?
    public init(output: Data, endOfOutput: Bool = false, resized: TerminalSessionGeometry? = nil) {
        self.output = output; self.endOfOutput = endOfOutput; self.resized = resized
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
}

public extension TerminalSessionTransport {
    func inputAvailable() { inputSignal.notify() }
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

public enum TerminalSessionEnd: Equatable, Sendable {
    case cancelled, disconnected, transportFailed, consumerFailed, ownershipConflict
    /// Client policy ends this attachment after peer output EOF. This is NOT an
    /// exit code, process-completion proof, or a promise the remote PTY survives.
    case peerOutputEnded
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
    private var started = false
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

    /// Exactly one run; focus and responder generations remain owned by the
    /// coordinator/UI. This does not retire their handles or discard outcomes.
    public func run(consumer: any TerminalSessionConsumer) async throws -> TerminalSessionEnd {
        guard !started else { throw TerminalInputError.unavailable }
        started = true
        let result = await withTaskCancellationHandler {
            await withTaskGroup(of: Void.self, returning: TerminalSessionEnd.self) { group in
                let signal = self.signal, transport = self.transport
                group.addTask {
                    while await signal.wait() { await transport.wakeService() }
                    // Cancellation or requestStop also interrupts an idle native
                    // read, before the pump next regains executor time.
                    await transport.close()
                }
                let reason = await self.drive(consumer: consumer)
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

    private func drive(consumer: any TerminalSessionConsumer) async -> TerminalSessionEnd {
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
            catch { return signal.isFinished || Task.isCancelled ? .cancelled : .transportFailed }
            // A corrupt adapter cannot push an oversized receive into the parser.
            guard update.output.count <= 16_384 else { return .transportFailed }
            if signal.isFinished || Task.isCancelled { return .cancelled }
            if !update.output.isEmpty || update.resized != nil || update.endOfOutput {
                do { try await consumer.receive(update, source: source) }
                catch { return signal.isFinished || Task.isCancelled ? .cancelled : .consumerFailed }
            }
            if signal.isFinished || Task.isCancelled { return .cancelled }
            // Deliver all final bytes first. Stop this attachment deliberately;
            // never turn repeated native EOF into a busy loop or an exit(0) claim.
            if update.endOfOutput { return .peerOutputEnded }
        }
        return .cancelled
    }
}
