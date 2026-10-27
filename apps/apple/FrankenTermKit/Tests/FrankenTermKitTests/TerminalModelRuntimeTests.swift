import Foundation
import XCTest
@testable import FrankenTermKit

// These injected-model tests exercise ownership and scheduling in the Swift
// runtime. The opaque bytes below are not ANSI results or valid FTSF frames;
// they make no claim to execute the canonical Rust parser/encoder/renderer.
private actor ModelRuntimeGate {
    private var opened = false
    private var waiters: [CheckedContinuation<Void, Never>] = []
    func wait() async {
        if opened { return }
        await withCheckedContinuation { waiters.append($0) }
    }
    func release() {
        opened = true
        let old = waiters; waiters = []
        for waiter in old { waiter.resume() }
    }
}

private actor ModelRuntimeTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    private var written = Data()
    init(identity: TerminalConnectionIdentity? = nil) throws {
        self.identity = try identity ?? .init(workspaceID: "model-test", machineID: "machine",
            routeID: "direct", routeRevision: 1, transportID: UUID(), channelID: UUID())
    }
    func writeSome(_ bytes: Data) -> TerminalTransportWrite {
        written += bytes
        return .accepted(bytes.count)
    }
    func close() {}
    func bytes() -> Data { written }
}

private struct ModelRuntimeSnapshot: Sendable {
    let events: [String], fed: Data, pendingReplies: Int, settled: [UInt64], dropped: [UInt64]
    let copiedSequence: UInt64?
    let captures: Int, viewports: [Int64?], fontRevision: UInt64, closeCalls: Int
}

/// All mutable state is synchronized so the test can inspect the model while
/// its exclusive runtime is suspended in a delivery callback.
private final class OwnedModelFixture: TerminalSessionModel, @unchecked Sendable {
    let source: TerminalSource
    private let lock = NSLock()
    private var events: [String] = [], fed = Data(), replies: [TerminalModelReply]
    private var settled: [UInt64] = [], dropped: [UInt64] = [], captures = 0
    private var copiedSequence: UInt64?
    private var viewports: [Int64?] = [], fontRevision: UInt64 = 0, closeCalls = 0
    private var closeFailures: Int
    private var cancelFirstReplyCopy: Bool
    private let frameError: TerminalModelError?
    private let unavailableExplicitViewport: Bool
    private let encoded: Data
    private let paste: TerminalPreparedPaste?
    // When set, frames are 256-byte FTSF headers reporting this retained
    // history range, and explicit viewports outside it are unavailable.
    private var history: TerminalRenderHistory?

    init(source: TerminalSource, replies: [TerminalModelReply] = [],
         frameError: TerminalModelError? = nil, encoded: Data = Data([0xA5]),
         paste: TerminalPreparedPaste? = nil, closeFailures: Int = 0,
         cancelFirstReplyCopy: Bool = false, unavailableExplicitViewport: Bool = false,
         history: TerminalRenderHistory? = nil) {
        self.source = source; self.replies = replies; self.frameError = frameError
        self.encoded = encoded; self.paste = paste; self.closeFailures = closeFailures
        self.cancelFirstReplyCopy = cancelFirstReplyCopy
        self.unavailableExplicitViewport = unavailableExplicitViewport
        self.history = history
    }
    func retain(_ history: TerminalRenderHistory) {
        lock.lock(); defer { lock.unlock() }
        self.history = history
    }
    /// The viewport's first stable row in a header frame from this fixture.
    static func viewportFirst(_ frame: Data) -> Int64 {
        frame.withUnsafeBytes { $0.loadUnaligned(fromByteOffset: 104, as: Int64.self) }
    }
    func feed(_ bytes: Data) {
        lock.lock(); defer { lock.unlock() }
        events.append("feed"); fed += bytes
    }
    func resize(_ geometry: TerminalSessionGeometry) {
        lock.lock(); defer { lock.unlock() }
        events.append("resize:\(geometry.columns)x\(geometry.rows)")
    }
    func encode(_ intent: TerminalKeyboardIntent, maximumBytes: Int) -> Data {
        lock.lock(); defer { lock.unlock() }
        events.append("encode")
        return encoded
    }
    func preparePaste(_ text: String, maximumBytes: Int) -> TerminalPreparedPaste {
        lock.lock(); defer { lock.unlock() }
        events.append("paste")
        return paste ?? .init(text: text, bracketed: true)
    }
    func copyReply() throws -> TerminalModelReply? {
        lock.lock()
        guard copiedSequence == nil else { lock.unlock(); throw TerminalModelError.busy }
        let reply = replies.first, cancel = cancelFirstReplyCopy && reply != nil
        copiedSequence = reply?.sequence
        cancelFirstReplyCopy = false
        lock.unlock()
        // Cancellation at the model-to-coordinator handoff must not cause an
        // accepted reply to be left in the model and replayed on the next call.
        if cancel { withUnsafeCurrentTask { $0?.cancel() } }
        return reply
    }
    func settleReply(sequence: UInt64, disposition: TerminalModelReplyDisposition) throws {
        lock.lock(); defer { lock.unlock() }
        guard replies.first?.sequence == sequence, copiedSequence == sequence else { throw TerminalModelError.stale }
        replies.removeFirst()
        copiedSequence = nil
        switch disposition {
        case .accepted: settled.append(sequence)
        case .droppedWhole: dropped.append(sequence)
        }
    }
    func captureFrame(viewportTop: Int64?) throws -> Data {
        lock.lock(); defer { lock.unlock() }
        events.append("capture"); captures += 1; viewports.append(viewportTop)
        if unavailableExplicitViewport, viewportTop != nil { throw TerminalModelError.viewportUnavailable }
        if let frameError { throw frameError }
        guard let history else { return Data([0xF0, UInt8(truncatingIfNeeded: captures)]) }
        if let top = viewportTop, top < history.first || top > history.live {
            throw TerminalModelError.viewportUnavailable
        }
        var header = Data(repeating: 0, count: 256)
        header.replaceSubrange(0..<4, with: Array("FTSF".utf8))
        for (at, value) in [(12, Int64(17)), (104, viewportTop ?? history.live),
                            (176, history.first), (184, history.live)] {
            withUnsafeBytes(of: value.littleEndian) { header.replaceSubrange(at..<(at + 8), with: $0) }
        }
        return header
    }
    func setFontRevision(_ revision: UInt64) {
        lock.lock(); defer { lock.unlock() }
        events.append("font"); fontRevision = revision
    }
    func setBasePalette(_ rgba: [UInt8]) {
        lock.lock(); defer { lock.unlock() }
        events.append("palette:\(rgba.count)")
    }
    func close() throws {
        lock.lock(); defer { lock.unlock() }
        events.append("close"); closeCalls += 1
        guard copiedSequence == nil else { throw TerminalModelError.busy }
        if closeFailures > 0 { closeFailures -= 1; throw TerminalModelError.busy }
    }
    func snapshot() -> ModelRuntimeSnapshot {
        lock.lock(); defer { lock.unlock() }
        return .init(events: events, fed: fed, pendingReplies: replies.count, settled: settled,
            dropped: dropped, copiedSequence: copiedSequence, captures: captures, viewports: viewports,
            fontRevision: fontRevision, closeCalls: closeCalls)
    }
    func recordAdmission() {
        lock.lock(); defer { lock.unlock() }
        events.append("admitted")
    }
}

private actor ModelRuntimeDelivery: TerminalModelDelivery {
    let entered = ModelRuntimeGate()
    let hold: ModelRuntimeGate?
    private var presentations: [TerminalModelPresentation] = []
    private var sources: [TerminalSource] = [], outcomes: [TerminalInputOutcome] = []
    private let cancelOutcomes: Bool
    private var failOutcomes: Bool

    init(hold: ModelRuntimeGate? = nil, cancelOutcomes: Bool = false, failOutcomes: Bool = false) {
        self.hold = hold; self.cancelOutcomes = cancelOutcomes; self.failOutcomes = failOutcomes
    }
    func present(_ presentation: TerminalModelPresentation, source: TerminalSource) async {
        await entered.release()
        await hold?.wait()
        presentations.append(presentation); sources.append(source)
    }
    func receive(_ values: [TerminalInputOutcome], source: TerminalSource) throws {
        if failOutcomes { throw TerminalModelError.busy }
        guard values.allSatisfy({ $0.receipt.source == source }) else { throw TerminalModelError.stale }
        outcomes += values
        if cancelOutcomes { withUnsafeCurrentTask { $0?.cancel() } }
    }
    func allowOutcomes() { failOutcomes = false }
    func snapshot() -> ([TerminalModelPresentation], [TerminalSource], [TerminalInputOutcome]) {
        (presentations, sources, outcomes)
    }
}

final class TerminalModelRuntimeTests: XCTestCase, @unchecked Sendable {
    private func source(limits: TerminalInputLimits = .init()) async throws
        -> (TerminalInputCoordinator, ModelRuntimeTransport, TerminalSource) {
        let coordinator = try TerminalInputCoordinator(limits: limits), transport = try ModelRuntimeTransport()
        return (coordinator, transport, try await coordinator.attach(transport))
    }

    func testHiddenModelKeepsAnswerbacksInTheUserInputFIFOWithoutCapturing() async throws {
        let (coordinator, transport, source) = try await source()
        let model = OwnedModelFixture(source: source, replies: [
            try .init(sequence: 1, bytes: Data([0x41, 0x42])),
            try .init(sequence: 2, bytes: Data([0x43]))])
        let delivery = ModelRuntimeDelivery(), runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: delivery)
        let stream = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(stream, to: source)
        let before = try await coordinator.submit(focus, encodedBytes: Data([0x10]))
        try await runtime.setVisible(false, source: source)
        let size = try TerminalSessionGeometry(columns: 100, rows: 30)
        try await runtime.receive(.init(output: Data([0x90, 0x91]), resized: size), source: source)
        let after = try await coordinator.submit(focus, encodedBytes: Data([0x20]))
        for _ in 0..<4 { _ = try await coordinator.drainOne(source) }
        let bytes = await transport.bytes()
        XCTAssertEqual(bytes, Data([0x10, 0x41, 0x42, 0x43, 0x20]))
        let batch = try await coordinator.pendingOutcomes(source)
        XCTAssertEqual(batch.outcomes.map(\.kind), [.encodedInput, .reply, .reply, .encodedInput])
        XCTAssertEqual(batch.outcomes.first?.receipt, before)
        XCTAssertEqual(batch.outcomes.last?.receipt, after)
        let snapshot = model.snapshot()
        XCTAssertEqual(snapshot.events, ["resize:100x30", "feed"])
        XCTAssertEqual(snapshot.fed, Data([0x90, 0x91]))
        XCTAssertEqual(snapshot.settled, [1, 2])
        XCTAssertEqual(snapshot.captures, 0)
        let accounting = await runtime.accounting()
        XCTAssertEqual(accounting.queuedReplies, 2)
        XCTAssertEqual(accounting.droppedReplies, 0)
        try await runtime.setVisible(true, source: source)
        XCTAssertEqual(model.snapshot().captures, 1)
        let presentations = await delivery.snapshot()
        XCTAssertEqual(presentations.0.count, 1)
        XCTAssertEqual(presentations.1, [source])
        try await runtime.close()
    }

    func testFullCoordinatorDropsOnlyACompleteReplyAndSettlesItsOriginalSequence() async throws {
        var limits = TerminalInputLimits(); limits.bytesPerConnection = 3
        let (coordinator, transport, source) = try await source(limits: limits)
        let model = OwnedModelFixture(source: source, replies: [
            try .init(sequence: 8, bytes: Data([1, 2, 3])),
            try .init(sequence: 9, bytes: Data([4, 5]))])
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: ModelRuntimeDelivery())
        try await runtime.setVisible(false, source: source)
        try await runtime.receive(.init(output: Data([0x90])), source: source)
        XCTAssertEqual(model.snapshot().settled, [8])
        XCTAssertEqual(model.snapshot().dropped, [9])
        XCTAssertEqual(model.snapshot().pendingReplies, 0)
        let accounting = await runtime.accounting(), inputs = await coordinator.accounting()
        XCTAssertEqual(accounting.queuedReplies, 1)
        XCTAssertEqual(accounting.droppedReplies, 1)
        XCTAssertEqual(inputs.droppedReplyUnits, 1)
        XCTAssertEqual(inputs.retainedBytes, 3)
        _ = try await coordinator.drainOne(source)
        let bytes = await transport.bytes()
        XCTAssertEqual(bytes, Data([1, 2, 3]))
        try await runtime.close()
    }

    func testCancellationAtReplyHandoffSettlesAcceptedBytesBeforeReturning() async throws {
        let (coordinator, transport, source) = try await source()
        let model = OwnedModelFixture(source: source,
            replies: [try .init(sequence: 17, bytes: Data([7, 8]))], cancelFirstReplyCopy: true)
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: ModelRuntimeDelivery())
        let receive = Task { try await runtime.receive(.init(output: Data([0x90])), source: source) }
        do { try await receive.value; XCTFail("Cancellation was hidden") }
        catch { XCTAssertTrue(error is CancellationError) }
        XCTAssertEqual(model.snapshot().settled, [17])
        XCTAssertEqual(model.snapshot().pendingReplies, 0)
        XCTAssertEqual(model.snapshot().captures, 0)
        let accounting = await runtime.accounting()
        XCTAssertEqual(accounting.queuedReplies, 1)
        // A later callback cannot replay the already accepted complete reply.
        try await runtime.receive(.init(output: Data()), source: source)
        _ = try await coordinator.drainOne(source)
        let bytes = await transport.bytes()
        XCTAssertEqual(bytes, Data([7, 8]))
        let batch = try await coordinator.pendingOutcomes(source)
        XCTAssertEqual(batch.outcomes.count, 1)
        try await runtime.close()
    }

    func testSourceWitnessAndReceiveBoundsAreCheckedBeforeModelEffects() async throws {
        let (coordinator, transport, source) = try await source()
        let other = try TerminalInputCoordinator()
        let sameIdentity = try ModelRuntimeTransport(identity: transport.identity)
        let foreign = try await other.attach(sameIdentity)
        XCTAssertEqual(foreign.identity, source.identity)
        XCTAssertNotEqual(foreign, source)
        let model = OwnedModelFixture(source: source)
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: ModelRuntimeDelivery())
        do { try await runtime.receive(.init(output: Data([1])), source: foreign); XCTFail("Foreign output admitted") }
        catch { XCTAssertEqual(error as? TerminalInputError, .stale) }
        do { _ = try await runtime.encode(.text("x"), source: foreign, maximumBytes: 10); XCTFail("Foreign input encoded") }
        catch { XCTAssertEqual(error as? TerminalInputError, .stale) }
        do { try await runtime.receive(.init(output: Data(repeating: 1, count: 16_385)), source: source); XCTFail("Oversize output applied") }
        catch { XCTAssertEqual(error as? TerminalModelError, .capacity) }
        do { try await runtime.setViewport(top: Int64.min, source: source); XCTFail("Sentinel accepted as viewport") }
        catch { XCTAssertEqual(error as? TerminalModelError, .invalid) }
        XCTAssertTrue(model.snapshot().events.isEmpty)
        try await runtime.close()
    }

    func testDisconnectedCoordinatorReleasesCopiedReplyBeforeReportingAdmissionFailure() async throws {
        let (coordinator, _, source) = try await source()
        let model = OwnedModelFixture(source: source, replies: [try .init(sequence: 23, bytes: Data([1, 2]))])
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: ModelRuntimeDelivery())
        try await coordinator.disconnect(source)
        do { try await runtime.receive(.init(output: Data([0x90])), source: source); XCTFail("Disconnected source admitted a reply") }
        catch { XCTAssertEqual(error as? TerminalInputError, .unavailable) }
        XCTAssertEqual(model.snapshot().settled, [])
        XCTAssertEqual(model.snapshot().dropped, [23])
        XCTAssertNil(model.snapshot().copiedSequence)
        XCTAssertEqual(model.snapshot().captures, 0)
        let inputs = await coordinator.accounting()
        XCTAssertEqual(inputs.retainedBytes, 0)
        XCTAssertEqual(inputs.retainedRecords, 0)
        try await runtime.close()
        XCTAssertEqual(model.snapshot().closeCalls, 1)
    }

    func testSuspendedPublicationWaitsOneConsumerRejectsExcessAndAllowsReadOnlyEncoding() async throws {
        let (coordinator, _, source) = try await source()
        let hold = ModelRuntimeGate(), delivery = ModelRuntimeDelivery(hold: hold)
        let model = OwnedModelFixture(source: source, encoded: Data([0xA1, 0xA2]))
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: delivery)
        let receive = Task { try await runtime.receive(.init(output: Data([0x90])), source: source) }
        await delivery.entered.wait()
        let refused = ModelRuntimeGate()
        let next = Task {
            do { try await runtime.receive(.init(output: Data([0x91])), source: source); return true }
            catch TerminalModelError.busy { await refused.release(); return false }
        }
        let excess = Task {
            do { try await runtime.receive(.init(output: Data([0x92])), source: source); return true }
            catch TerminalModelError.busy { await refused.release(); return false }
        }
        // One bounded consumer is waiting; the competing one must complete
        // refusal before publication is released, without dropping the channel.
        await refused.wait()
        // A history scroll waits once behind publication like a presentation
        // request; a second concurrent viewport request is refused, not queued.
        let viewport = Task { try await runtime.setViewport(top: -20, source: source) }
        while await runtime.accounting().waitingViewportRequests == 0 { await Task.yield() }
        do { try await runtime.setViewport(top: -21, source: source); XCTFail("Second viewport request queued") }
        catch { XCTAssertEqual(error as? TerminalModelError, .busy) }
        let encoded = try await runtime.encode(.text("x"), source: source, maximumBytes: 2)
        XCTAssertEqual(encoded, Data([0xA1, 0xA2]))
        XCTAssertEqual(model.snapshot().fed, Data([0x90]))
        XCTAssertEqual(model.snapshot().events, ["feed", "capture", "encode"])
        await hold.release()
        try await receive.value
        let results = try await (next.value, excess.value)
        try await viewport.value
        XCTAssertNotEqual(results.0, results.1)
        XCTAssertEqual(model.snapshot().fed, Data([0x90, results.0 ? 0x91 : 0x92]))
        XCTAssertEqual(model.snapshot().captures, 3)
        XCTAssertEqual(model.snapshot().viewports, [nil, nil, -20], "the waiting request ran after the waiting consumer")
        try await runtime.close()
    }

    func testPaletteChangeWaitsOnceBehindOutputAndRepublishesOnlyAVisibleTerminal() async throws {
        let (coordinator, _, source) = try await source()
        let hold = ModelRuntimeGate(), delivery = ModelRuntimeDelivery(hold: hold)
        let model = OwnedModelFixture(source: source)
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: delivery)
        let palette = [UInt8](repeating: 9, count: 1052)
        let receive = Task { try await runtime.receive(.init(output: Data([0x90])), source: source) }
        await delivery.entered.wait()
        // A theme picked while output is being published waits for it; a
        // second pick in that window is refused, not queued behind the first.
        let change = Task { try await runtime.setBasePalette(palette, source: source) }
        while await runtime.accounting().waitingPaletteRequests == 0 { await Task.yield() }
        do { try await runtime.setBasePalette(palette, source: source); XCTFail("Second palette change queued") }
        catch { XCTAssertEqual(error as? TerminalModelError, .busy) }
        XCTAssertEqual(model.snapshot().events, ["feed", "capture"])
        await hold.release()
        try await receive.value
        try await change.value
        XCTAssertEqual(model.snapshot().events, ["feed", "capture", "palette:1052", "capture"],
                       "the visible terminal shows the new palette at once")
        let published = await delivery.snapshot().0
        XCTAssertEqual(published.count, 2)

        try await runtime.setVisible(false, source: source)
        try await runtime.setBasePalette(palette, source: source)
        XCTAssertEqual(model.snapshot().events.suffix(2), ["capture", "palette:1052"], "a hidden terminal is not captured")
        let (_, _, other) = try await self.source()
        do { try await runtime.setBasePalette(palette, source: other); XCTFail("Foreign source accepted") }
        catch { XCTAssertEqual(error as? TerminalInputError, .stale) }
        XCTAssertEqual(model.snapshot().events.filter { $0.hasPrefix("palette") }.count, 2)
        try await runtime.close()
    }

    func testInputTransactionPreventsModeMutationUntilActualCoordinatorAdmission() async throws {
        let (coordinator, transport, source) = try await source()
        let model = OwnedModelFixture(source: source, encoded: Data([0x31, 0x32]))
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: ModelRuntimeDelivery())
        try await runtime.setVisible(false, source: source)
        let stream = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(stream, to: source)
        let encoded = ModelRuntimeGate(), admit = ModelRuntimeGate(), refused = ModelRuntimeGate()
        let input = Task {
            try await runtime.withInputTransaction(source: source) {
                let bytes = try await runtime.encode(.text("x"), source: source, maximumBytes: 2)
                await encoded.release()
                await admit.wait()
                let receipt = try await coordinator.submit(focus, encodedBytes: bytes)
                model.recordAdmission()
                return receipt
            }
        }
        await encoded.wait()
        // Competing consumer calls force one real queued operation and one
        // bounded refusal while the input callback still owns its mode sample.
        let output = Task {
            do { try await runtime.receive(.init(output: Data([0x90])), source: source); return true }
            catch TerminalModelError.busy { await refused.release(); return false }
        }
        let extra = Task {
            do { try await runtime.receive(.init(output: Data([0x91])), source: source); return true }
            catch TerminalModelError.busy { await refused.release(); return false }
        }
        await refused.wait()
        XCTAssertEqual(model.snapshot().events, ["encode"])
        let before = await coordinator.accounting()
        XCTAssertEqual(before.retainedBytes, 0)
        await admit.release()
        let receipt = try await input.value
        let results = try await (output.value, extra.value)
        XCTAssertNotEqual(results.0, results.1)
        XCTAssertEqual(model.snapshot().events, ["encode", "admitted", "feed"])
        _ = try await coordinator.drainOne(source)
        let written = await transport.bytes()
        XCTAssertEqual(written, Data([0x31, 0x32]))
        let outcomes = try await coordinator.pendingOutcomes(source)
        XCTAssertEqual(outcomes.outcomes.map(\.receipt), [receipt])
        try await runtime.close()
    }

    func testClosingRefusesQueuedKeyboardTransactionWithoutRevivingInput() async throws {
        let (coordinator, _, source) = try await source()
        let hold = ModelRuntimeGate(), delivery = ModelRuntimeDelivery(hold: hold)
        let model = OwnedModelFixture(source: source)
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: delivery)
        let receive = Task { try await runtime.receive(.init(output: Data([0x90])), source: source) }
        await delivery.entered.wait()
        let refused = ModelRuntimeGate()
        let first = Task {
            do {
                return try await runtime.withInputTransaction(source: source) {
                    _ = try await runtime.encode(.text("never"), source: source, maximumBytes: 10)
                    return TerminalModelError.invalid
                }
            } catch let error as TerminalModelError {
                if error == .busy { await refused.release() }
                return error
            }
        }
        let second = Task {
            do {
                return try await runtime.withInputTransaction(source: source) {
                    _ = try await runtime.encode(.text("never"), source: source, maximumBytes: 10)
                    return TerminalModelError.invalid
                }
            } catch let error as TerminalModelError {
                if error == .busy { await refused.release() }
                return error
            }
        }
        await refused.wait()
        await runtime.requestClose()
        let results = try await (first.value, second.value)
        XCTAssertTrue((results.0 == .closed && results.1 == .busy) || (results.0 == .busy && results.1 == .closed))
        XCTAssertEqual(model.snapshot().events, ["feed", "capture"])
        XCTAssertEqual(model.snapshot().closeCalls, 0)
        await hold.release()
        try await receive.value
        try await runtime.close()
        XCTAssertEqual(model.snapshot().events, ["feed", "capture", "close"])
    }

    func testRequestedCloseInvalidatesInputBeforeJoiningSuspendedDelivery() async throws {
        let (coordinator, _, source) = try await source()
        let hold = ModelRuntimeGate(), delivery = ModelRuntimeDelivery(hold: hold)
        let model = OwnedModelFixture(source: source)
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: delivery)
        let receive = Task { try await runtime.receive(.init(output: Data([0x90])), source: source) }
        await delivery.entered.wait()
        await runtime.requestClose()
        let pending = await runtime.accounting()
        XCTAssertTrue(pending.closing)
        XCTAssertFalse(pending.closed)
        XCTAssertEqual(model.snapshot().closeCalls, 0, "The borrowed model must survive its in-flight delivery")
        do { _ = try await runtime.encode(.text("x"), source: source, maximumBytes: 10); XCTFail("Input revived during close") }
        catch { XCTAssertEqual(error as? TerminalModelError, .closed) }
        async let first: Void = runtime.close()
        async let second: Void = runtime.close()
        await hold.release()
        try await receive.value
        _ = try await (first, second)
        XCTAssertEqual(model.snapshot().events, ["feed", "capture", "close"])
        XCTAssertEqual(model.snapshot().closeCalls, 1)
        let closed = await runtime.accounting()
        XCTAssertTrue(closed.closed)
        try await runtime.close()
        XCTAssertEqual(model.snapshot().closeCalls, 1)
    }

    func testFailedModelCloseRetainsOwnershipAndCanBeJoinedAgain() async throws {
        let (coordinator, _, source) = try await source()
        let model = OwnedModelFixture(source: source, closeFailures: 1)
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: ModelRuntimeDelivery())
        do { try await runtime.close(); XCTFail("Failed native close was hidden") }
        catch { XCTAssertEqual(error as? TerminalModelError, .busy) }
        let failed = await runtime.accounting()
        XCTAssertTrue(failed.closing)
        XCTAssertFalse(failed.closed)
        XCTAssertEqual(model.snapshot().closeCalls, 1)
        do { try await runtime.refresh(source: source); XCTFail("Failed close reopened model") }
        catch { XCTAssertEqual(error as? TerminalModelError, .closed) }
        try await runtime.close()
        XCTAssertEqual(model.snapshot().closeCalls, 2)
        let final = await runtime.accounting()
        XCTAssertTrue(final.closed)
    }

    func testHidingDuringSuspendedPresentationStopsFutureSnapshotsWithoutStoppingOutput() async throws {
        let (coordinator, _, source) = try await source()
        let hold = ModelRuntimeGate(), delivery = ModelRuntimeDelivery(hold: hold)
        let model = OwnedModelFixture(source: source)
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: delivery)
        let first = Task { try await runtime.receive(.init(output: Data([1])), source: source) }
        await delivery.entered.wait()
        // This lifecycle change must not fail as busy or wait on a borrowed
        // presentation. Its current callback may settle; future captures stop.
        try await runtime.setVisible(false, source: source)
        let next = Task { try await runtime.receive(.init(output: Data([2])), source: source) }
        await hold.release()
        try await first.value
        try await next.value
        try await runtime.receive(.init(output: Data([3])), source: source)
        XCTAssertEqual(model.snapshot().fed, Data([1, 2, 3]))
        XCTAssertEqual(model.snapshot().captures, 1)
        let hidden = await runtime.accounting()
        XCTAssertEqual(hidden.publishedFrames, 1)
        try await runtime.setVisible(true, source: source)
        XCTAssertEqual(model.snapshot().captures, 2)
        let shown = await delivery.snapshot()
        XCTAssertEqual(shown.0.count, 2)
        XCTAssertEqual(shown.1, [source, source])
        try await runtime.close()
    }

    /// Output evicts the oldest history first. A reader whose rows were evicted
    /// moves to the oldest retained row, or to the live screen when none is
    /// left; the terminal is never reported unavailable for it, and output and
    /// answerbacks continue throughout.
    func testEvictedViewportFollowsRetainedHistoryWhileLiveOutputAndAnswerbacksContinue() async throws {
        let (coordinator, transport, source) = try await source()
        let model = OwnedModelFixture(source: source,
            replies: [try .init(sequence: 5, bytes: Data([0x41, 0x42]))], history: .init(first: 100, live: 200))
        let delivery = ModelRuntimeDelivery(), runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: delivery)
        try await runtime.setViewport(top: 120, source: source)
        model.retain(.init(first: 150, live: 260))
        try await runtime.receive(.init(output: Data([0x90])), source: source)
        XCTAssertEqual(model.snapshot().fed, Data([0x90]))
        XCTAssertEqual(model.snapshot().settled, [5])
        _ = try await coordinator.drainOne(source)
        let reply = await transport.bytes()
        XCTAssertEqual(reply, Data([0x41, 0x42]))
        try await runtime.receive(.init(output: Data([0x91])), source: source)
        model.retain(.init(first: 300, live: 300))
        try await runtime.receive(.init(output: Data([0x92])), source: source)
        try await runtime.receive(.init(output: Data([0x93])), source: source)
        let shown = await delivery.snapshot().0.compactMap { presentation -> Int64? in
            guard case let .frame(bytes) = presentation else { return nil }
            return OwnedModelFixture.viewportFirst(bytes)
        }
        XCTAssertEqual(shown, [120, 150, 150, 300, 300], "every publication is a frame at a retained row")
        XCTAssertEqual(model.snapshot().viewports, [120, 120, nil, 150, 150, 150, nil, nil])
        let stats = await runtime.accounting()
        XCTAssertEqual(stats.unavailableFrames, 0)
        XCTAssertEqual(stats.publishedFrames, 5)
        XCTAssertEqual(stats.queuedReplies, 1)
        XCTAssertFalse(stats.closed)
        try await runtime.close()
    }

    func testEvictedViewportWithoutReportedHistoryReturnsLive() async throws {
        let (coordinator, _, source) = try await source()
        let model = OwnedModelFixture(source: source, unavailableExplicitViewport: true)
        let delivery = ModelRuntimeDelivery(), runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: delivery)
        try await runtime.setViewport(top: 9_000, source: source)
        try await runtime.receive(.init(output: Data([0x90])), source: source)
        XCTAssertEqual(model.snapshot().viewports, [9_000, nil, nil], "one failed attempt, then live")
        for presentation in await delivery.snapshot().0 {
            guard case .frame = presentation else { return XCTFail("The terminal was reported unavailable") }
        }
        try await runtime.close()
    }

    func testOutcomeDeliveryFailureRetainsTheBatchAndSuccessAllowsCancellationSafeAcknowledgement() async throws {
        let (coordinator, _, source) = try await source()
        let stream = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(stream, to: source)
        let receipt = try await coordinator.submit(focus, encodedBytes: Data([1]))
        _ = try await coordinator.cancel(receipt)
        let batch = try await coordinator.pendingOutcomes(source)
        XCTAssertEqual(batch.outcomes.count, 1)
        let model = OwnedModelFixture(source: source)
        let delivery = ModelRuntimeDelivery(cancelOutcomes: true, failOutcomes: true)
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: delivery)
        do { try await runtime.receive(batch.outcomes, source: source); XCTFail("Delivery failure swallowed") }
        catch { XCTAssertEqual(error as? TerminalModelError, .busy) }
        let retained = try await coordinator.pendingOutcomes(source)
        XCTAssertEqual(retained.outcomes, batch.outcomes)
        await delivery.allowOutcomes()
        let accept = Task {
            try await runtime.receive(batch.outcomes, source: source)
            // The actual pump owns this acknowledgement. Successful delivery
            // cannot become a retry merely because cancellation arrived in it.
            try await coordinator.acknowledgeOutcomes(batch)
            return Task.isCancelled
        }
        let wasCancelled = try await accept.value
        XCTAssertTrue(wasCancelled)
        let remaining = try await coordinator.pendingOutcomes(source)
        XCTAssertTrue(remaining.outcomes.isEmpty)
        let accepted = await delivery.snapshot()
        XCTAssertEqual(accepted.2, batch.outcomes)
        XCTAssertTrue(model.snapshot().events.isEmpty)
        try await runtime.close()
    }

    /// Input that left this phone awaits the machine's answer until its next
    /// output. The terminal's own replies and a local resize are neither.
    func testInputAwaitsAnAnswerUntilTheMachinesNextOutput() async throws {
        let (coordinator, _, source) = try await source()
        let model = OwnedModelFixture(source: source)
        final class Changes: @unchecked Sendable { let lock = NSLock(); var values: [Bool] = [] }
        let changes = Changes()
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: ModelRuntimeDelivery(),
                                           awaitingAnswer: { value in changes.lock.withLock { changes.values.append(value) } })
        let stream = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(stream, to: source)
        func deliver() async throws {
            for _ in 0..<8 { guard case .progressed = try await coordinator.drainOne(source) else { break } }
            let batch = try await coordinator.pendingOutcomes(source)
            XCTAssertFalse(batch.outcomes.isEmpty)
            try await runtime.receive(batch.outcomes, source: source)
            try await coordinator.acknowledgeOutcomes(batch)
        }
        var awaits = await runtime.inputAwaitsAnswer()
        XCTAssertFalse(awaits)
        _ = try await coordinator.reply(source, encodedBytes: Data("\u{1b}[0n".utf8))
        try await deliver()
        awaits = await runtime.inputAwaitsAnswer()
        XCTAssertFalse(awaits, "the terminal's own reply is not the person's input")
        _ = try await coordinator.submit(focus, encodedBytes: Data("lost\r".utf8))
        try await deliver()
        awaits = await runtime.inputAwaitsAnswer()
        XCTAssertTrue(awaits, "accepted input waits for the machine's answer")
        try await runtime.receive(.init(output: Data(), resized: try TerminalSessionGeometry(columns: 90, rows: 30)),
                                  source: source)
        awaits = await runtime.inputAwaitsAnswer()
        XCTAssertTrue(awaits, "a local resize is not an answer")
        try await runtime.receive(.init(output: Data("lost\r\n".utf8)), source: source)
        awaits = await runtime.inputAwaitsAnswer()
        XCTAssertFalse(awaits, "the machine's output answers it")
        try await runtime.receive(.init(output: Data("more\r\n".utf8)), source: source)
        XCTAssertEqual(changes.lock.withLock { changes.values }, [true, false], "each change is heard once")
        try await runtime.close()
    }

    func testEOFDeliversFinalOutputAndRepliesThenRefusesFurtherInput() async throws {
        let (coordinator, _, source) = try await source()
        let model = OwnedModelFixture(source: source, replies: [try .init(sequence: 1, bytes: Data([2]))])
        let delivery = ModelRuntimeDelivery(), runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: delivery)
        try await runtime.receive(.init(output: Data([1]), endOfOutput: true), source: source)
        XCTAssertEqual(model.snapshot().events, ["feed", "capture"])
        XCTAssertEqual(model.snapshot().settled, [1])
        let stats = await runtime.accounting()
        XCTAssertTrue(stats.endOfOutput)
        XCTAssertEqual(stats.publishedFrames, 1)
        do { _ = try await runtime.encode(.text("x"), source: source, maximumBytes: 10); XCTFail("Input after EOF") }
        catch { XCTAssertEqual(error as? TerminalModelError, .closed) }
        do { try await runtime.receive(.init(output: Data([3])), source: source); XCTFail("Output after EOF") }
        catch { XCTAssertEqual(error as? TerminalModelError, .closed) }
        XCTAssertEqual(model.snapshot().fed, Data([1]))
        try await runtime.close()
    }

    func testUnsupportedAndOversizePresentationRemainExplicitWithoutDiscardingModelOutput() async throws {
        for failure in [TerminalModelError.unsupported, .capacity] {
            let (coordinator, _, source) = try await source()
            let model = OwnedModelFixture(source: source, frameError: failure)
            let delivery = ModelRuntimeDelivery(), runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: delivery)
            try await runtime.receive(.init(output: Data([1, 2])), source: source)
            let delivered = await delivery.snapshot()
            XCTAssertEqual(delivered.0.count, 1)
            if case .unavailable(let reason) = delivered.0.first {
                XCTAssertEqual(reason, failure == .unsupported ? .unsupported : .capacity)
            } else { XCTFail("Unavailable viewport was reported as a frame") }
            let stats = await runtime.accounting()
            XCTAssertEqual(stats.unavailableFrames, 1)
            XCTAssertEqual(stats.publishedFrames, 0)
            XCTAssertEqual(model.snapshot().fed, Data([1, 2]))
            try await runtime.close()
        }
    }

    func testCorruptAdapterCannotExceedEncodingOrReplyTurnBudgets() async throws {
        let (coordinator, _, source) = try await source()
        let replies = try (1...129).map { try TerminalModelReply(sequence: UInt64($0), bytes: Data([1])) }
        let model = OwnedModelFixture(source: source, replies: replies, encoded: Data([1, 2]),
            paste: .init(text: "unsafe\u{1B}", bracketed: true))
        let runtime = TerminalModelRuntime(model: model, coordinator: coordinator, delivery: ModelRuntimeDelivery())
        do { _ = try await runtime.encode(.text("x"), source: source, maximumBytes: 1); XCTFail("Oversize encoding escaped") }
        catch { XCTAssertEqual(error as? TerminalModelError, .capacity) }
        do { _ = try await runtime.preparePaste("x", source: source, maximumBytes: 20); XCTFail("Unsafe normalized paste escaped") }
        catch { XCTAssertEqual(error as? TerminalModelError, .invalid) }
        do { try await runtime.receive(.init(output: Data([1])), source: source); XCTFail("Unbounded reply generator accepted") }
        catch { XCTAssertEqual(error as? TerminalModelError, .capacity) }
        XCTAssertEqual(model.snapshot().settled, Array(1...128).map(UInt64.init))
        XCTAssertEqual(model.snapshot().dropped, [129])
        XCTAssertEqual(model.snapshot().pendingReplies, 0)
        XCTAssertNil(model.snapshot().copiedSequence)
        XCTAssertEqual(model.snapshot().captures, 0)
        let stats = await runtime.accounting()
        XCTAssertEqual(stats.queuedReplies, 128)
        try await runtime.close()
    }
}
