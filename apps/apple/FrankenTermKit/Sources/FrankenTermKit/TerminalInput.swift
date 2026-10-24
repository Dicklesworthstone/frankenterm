import Foundation

/// This owner is for a phone-owned, authenticated direct-SSH channel, not a mux
/// mirror. The SSH adapter must retain its own immutable channel and host trust.
public struct TerminalConnectionIdentity: Hashable, Sendable {
    public let workspaceID: String
    public let machineID: String
    public let routeID: String
    public let routeRevision: UInt64
    public let transportID: UUID
    public let channelID: UUID

    public init(workspaceID: String, machineID: String, routeID: String,
                routeRevision: UInt64, transportID: UUID, channelID: UUID) throws {
        for value in [workspaceID, machineID, routeID] {
            guard !value.isEmpty, value.utf8.count <= 128, value.utf8.allSatisfy({
                (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0)
                    || [45, 46, 58, 95].contains($0)
            }) else { throw TerminalInputError.invalidIdentity }
        }
        guard routeRevision > 0 else { throw TerminalInputError.invalidIdentity }
        self.workspaceID = workspaceID; self.machineID = machineID; self.routeID = routeID
        self.routeRevision = routeRevision; self.transportID = transportID; self.channelID = channelID
    }
}

public enum TerminalInputError: Error, Equatable, Sendable {
    case invalidIdentity, invalidLimits, invalidWriteSize, stale, unavailable
    case capacity, emptyInput, unsafePaste, compositionPending, sequenceExhausted
}

/// Positive counts prove only acceptance by the bounded SSH transport, NEVER
/// remote PTY delivery or execution. wouldBlock guarantees that this call accepted
/// no bytes. A throw/failed result supplies no such proof; its leased tail is unknown.
public enum TerminalTransportWrite: Equatable, Sendable {
    case accepted(Int), wouldBlock, failed
}

/// A destination captured by a keyboard focus, not a mutable "selected pane"
/// lookup. Every operation retains this writer through partial writes and paste
/// cancellation. Admission may expire while delivery to its original live pane
/// remains valid. A writer must never fall back to another pane or connection.
public protocol TerminalInputWriter: Sendable {
    /// Synchronous, bounded, and side-effect-free. Reject stale UI admission;
    /// cleanup for previously admitted keys deliberately bypasses this check.
    func validateInputAdmission() throws
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite
}

public extension TerminalInputWriter {
    func validateInputAdmission() throws {}
}

public protocol TerminalByteTransport: AnyObject, TerminalInputWriter {
    var identity: TerminalConnectionIdentity { get }
    /// Called before publishing a focus. Direct SSH returns its immutable
    /// channel; a multiplexed transport must capture an exact pane incarnation.
    func captureInputWriter() throws -> any TerminalInputWriter
    /// A phone-owned terminal answers queries. A mux mirror must not answer
    /// again: its authoritative server already owns that side effect.
    var acceptsModelReplies: Bool { get }
    /// A bounded, synchronous notification, not a write or a Task per event.
    /// A session driver uses it to interrupt its idle socket wait. Implementations
    /// must coalesce notifications and must not reenter the coordinator.
    func inputAvailable()
    /// Must neither block a cooperative executor nor enqueue into an unbounded
    /// downstream writer. Consume/copy only the returned prefix before returning.
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite
    /// Must interrupt outstanding I/O and settle ownership before returning.
    /// Closing one channel must not terminate unrelated channels or remote mux PTYs.
    func close() async
}

public extension TerminalByteTransport {
    func inputAvailable() {}
    func captureInputWriter() throws -> any TerminalInputWriter { self }
    var acceptsModelReplies: Bool { true }
}

public struct TerminalInputLimits: Sendable {
    public var connections = 32
    public var streams = 32
    public var bytesPerConnection = 128 * 1024
    public var totalBytes = 1024 * 1024
    public var recordsPerConnection = 512
    public var totalRecords = 4096
    public var cleanupBytesPerConnection = 8 * 1024
    public var totalCleanupBytes = 64 * 1024
    public var cleanupRecordsPerConnection = 32
    public var totalCleanupRecords = 256
    public var maximumWriteBytes = 4096
    public var maximumCleanupBytes = 1024
    public init() {}

    fileprivate func validate() throws {
        guard (1...128).contains(connections), (1...256).contains(streams),
              (1...1_048_576).contains(bytesPerConnection),
              (bytesPerConnection...8_388_608).contains(totalBytes),
              (1...4096).contains(recordsPerConnection),
              (recordsPerConnection...32768).contains(totalRecords),
              (16...65536).contains(cleanupBytesPerConnection),
              (cleanupBytesPerConnection...2_097_152).contains(totalCleanupBytes),
              (1...128).contains(cleanupRecordsPerConnection),
              (cleanupRecordsPerConnection...2048).contains(totalCleanupRecords),
              (1...65536).contains(maximumWriteBytes),
              (1...cleanupBytesPerConnection).contains(maximumCleanupBytes)
        else { throw TerminalInputError.invalidLimits }
    }
}

/// Handles are local ownership witnesses, not SSH authentication or mux authority.
public struct TerminalSource: Hashable, Sendable {
    fileprivate let owner: UUID, serial: UInt64
    public let identity: TerminalConnectionIdentity
}
/// A source-wide settlement witness, minted only after admission, every stream,
/// all writes/cleanup/outcomes, and native close have finished. It grants no
/// replay authority and cannot be constructed by an external session owner.
public struct TerminalInputSeal: Sendable {
    public let source: TerminalSource
    public let hasAcceptedBytes: Bool
    public let hasUnknownTransportOutcome: Bool
    fileprivate init(source: TerminalSource, hasAcceptedBytes: Bool, hasUnknownTransportOutcome: Bool) {
        self.source = source; self.hasAcceptedBytes = hasAcceptedBytes
        self.hasUnknownTransportOutcome = hasUnknownTransportOutcome
    }
}
public struct TerminalFocus: Equatable, Sendable {
    fileprivate let owner: UUID, stream: UUID
    public let sceneID: UUID, generation: UInt64
    public let source: TerminalSource?
}
public struct TerminalComposition: Equatable, Sendable {
    fileprivate let id: UUID
    public let focus: TerminalFocus
}
public struct TerminalInputReceipt: Hashable, Sendable {
    fileprivate let owner: UUID, serial: UInt64
    public let source: TerminalSource
}
public enum TerminalInputKind: Sendable { case encodedInput, paste, reply, cleanup }
public enum TerminalInputDisposition: Equatable, Sendable {
    /// All original bytes crossed the adapter's bounded acceptance boundary.
    /// This does not mean that a remote program processed them successfully.
    case transportAccepted
    case cancelledBeforeWrite
    /// Cancellation can finish a partially accepted UTF-8 scalar/delimiter and
    /// the closing bracket. No later operation interleaves with that cleanup.
    case cancelledAfterPrefix
    case definitelyNotWritten
    /// Includes any bytes already accepted before the connection was lost.
    case connectionLost(leasedTailUnknown: Bool)
}
public struct TerminalInputOutcome: Equatable, Sendable {
    public let receipt: TerminalInputReceipt
    public let kind: TerminalInputKind
    public let originalBytes: Int
    public let acceptedOriginalBytes: Int
    public let acceptedCleanupBytes: Int
    public let disposition: TerminalInputDisposition
}
/// Content-free, non-consuming prefix. A delivery failure leaves the original
/// records charged to their source until an explicit acknowledgement succeeds.
public struct TerminalOutcomeBatch: Sendable {
    fileprivate let source: TerminalSource
    public let outcomes: [TerminalInputOutcome]
}
public enum TerminalDrainResult: Equatable, Sendable {
    case idle, busy, wouldBlock, progressed(Int), connectionClosed
}
public enum TerminalCancelResult: Equatable, Sendable { case cancelled, pendingWrite, tooLate }
public enum TerminalReplyResult: Equatable, Sendable { case queued(TerminalInputReceipt), droppedWhole }
public struct TerminalInputAccounting: Equatable, Sendable {
    public let connections: Int, streams: Int, retainedBytes: Int, cleanupReservationBytes: Int
    public let retainedRecords: Int, cleanupRecords: Int, inFlightBytes: Int
    public let droppedReplyUnits: UInt64
}

/// One actor orders all direct-SSH egress: user input, replies, and cleanup. It
/// deliberately owns no ANSI parser, SSH implementation, display loop, or timer.
/// A bounded runtime driver calls drainOne when its channel becomes writable.
///
/// The current Rust ThreadedWriter is NOT a suitable upstream adapter: it must
/// be bypassed/replaced before the future Rust bridge can claim end-to-end bounds.
/// Crash-durable uncertainty markers remain the session owner's separate duty;
/// in-memory outcomes are never replay authority and are never persisted here.
public actor TerminalInputCoordinator {
    private let owner = UUID()
    private let limits: TerminalInputLimits
    private var serial: UInt64 = 0
    private var sources: [UInt64: Connection] = [:]
    private var streams: [UUID: Stream] = [:]
    private var droppedReplies: UInt64 = 0
    private static let pasteStart = Data("\u{1B}[200~".utf8)
    private static let pasteEnd = Data("\u{1B}[201~".utf8)
    private static let pasteCleanupReservation = 16

    // Mutable reference objects never escape the actor. Keeping a connection
    // alive during close/write preserves reservations while that work settles.
    private final class Connection {
        let source: TerminalSource, transport: any TerminalByteTransport
        var active = true, closed = false
        var closeTask: Task<Void, Never>?
        var queue: [Operation] = []
        var outcomes: [TerminalInputOutcome] = []
        var inFlight: UInt64?
        var inFlightBytes = 0
        var hasAcceptedBytes = false, hasUnknownTransportOutcome = false
        init(_ source: TerminalSource, _ transport: any TerminalByteTransport) {
            self.source = source; self.transport = transport
        }
        var inputBytes: Int { queue.reduce(0) { $0 + $1.inputCharge } }
        var cleanupBytes: Int { queue.reduce(0) { $0 + $1.cleanupCharge } }
        var records: Int { queue.filter { $0.kind != .cleanup }.count + outcomes.filter { $0.kind != .cleanup }.count }
        var cleanupRecords: Int { queue.filter { $0.kind == .cleanup }.count + outcomes.filter { $0.kind == .cleanup }.count }
    }
    private final class Operation {
        let receipt: TerminalInputReceipt, kind: TerminalInputKind, originalBytes: Int
        let writer: any TerminalInputWriter
        let pastePayload: Range<Int>?
        let bracketed: Bool
        var bytes: Data, cursor = 0, acceptedOriginal = 0, acceptedCleanup = 0
        var inputCharge: Int, cleanupCharge: Int
        var cancelRequested = false, cleaning = false
        init(_ receipt: TerminalInputReceipt, _ kind: TerminalInputKind, _ bytes: Data, writer: any TerminalInputWriter,
             pastePayload: Range<Int>?, bracketed: Bool, inputCharge: Int, cleanupCharge: Int) {
            self.receipt = receipt; self.kind = kind; self.bytes = bytes; originalBytes = bytes.count
            self.writer = writer
            self.pastePayload = pastePayload; self.bracketed = bracketed
            self.inputCharge = inputCharge; self.cleanupCharge = cleanupCharge
        }
    }
    private struct Stream {
        var focus: TerminalFocus
        var composition: TerminalComposition?
        var writer: (any TerminalInputWriter)?
    }

    public init(limits: TerminalInputLimits = TerminalInputLimits()) throws {
        try limits.validate(); self.limits = limits
    }
    private func nextSerial() throws -> UInt64 {
        guard serial < UInt64.max else { throw TerminalInputError.sequenceExhausted }
        serial += 1; return serial
    }
    private func connection(_ source: TerminalSource, active: Bool = true) throws -> Connection {
        guard source.owner == owner, let connection = sources[source.serial], connection.source == source
        else { throw TerminalInputError.stale }
        guard !active || connection.active else { throw TerminalInputError.unavailable }
        return connection
    }
    private func stream(_ focus: TerminalFocus) throws -> Stream {
        guard focus.owner == owner, let value = streams[focus.stream], value.focus == focus
        else { throw TerminalInputError.stale }
        return value
    }
    private func focusedConnection(_ focus: TerminalFocus, admitting: Bool = true) throws -> Connection {
        let state = try stream(focus)
        guard let source = focus.source else { throw TerminalInputError.unavailable }
        guard let writer = state.writer else { throw TerminalInputError.unavailable }
        if admitting { try writer.validateInputAdmission() }
        return try connection(source)
    }
    private func writer(_ focus: TerminalFocus) throws -> any TerminalInputWriter {
        guard let writer = try stream(focus).writer else { throw TerminalInputError.unavailable }
        return writer
    }

    /// Call only after the selected route's real SSH authentication and PTY
    /// creation. Neither importing configuration nor a saved view calls attach.
    public func attach(_ transport: any TerminalByteTransport) throws -> TerminalSource {
        guard sources.count < limits.connections else { throw TerminalInputError.capacity }
        guard !sources.values.contains(where: {
            $0.source.identity == transport.identity || ObjectIdentifier($0.transport) == ObjectIdentifier(transport)
        }) else { throw TerminalInputError.invalidIdentity }
        let source = TerminalSource(owner: owner, serial: try nextSerial(), identity: transport.identity)
        sources[source.serial] = Connection(source, transport)
        return source
    }
    public func makeStream(sceneID: UUID) throws -> TerminalFocus {
        guard streams.count < limits.streams else { throw TerminalInputError.capacity }
        let id = UUID(), focus = TerminalFocus(owner: owner, stream: id, sceneID: sceneID, generation: 1, source: nil)
        streams[id] = Stream(focus: focus); return focus
    }

    /// The local barrier is this atomic actor turn, not a socket flush. Cleanup
    /// enters the old FIFO before the destination token is published. If it
    /// cannot be reserved, the user-initiated switch leaves focus unchanged.
    public func changeFocus(_ from: TerminalFocus, to source: TerminalSource?, cleanup: Data = Data()) throws -> TerminalFocus {
        let state = try stream(from)
        guard state.composition == nil else { throw TerminalInputError.compositionPending }
        guard from.generation < UInt64.max else { throw TerminalInputError.sequenceExhausted }
        let nextWriter = try source.map { try connection($0).transport.captureInputWriter() }
        guard cleanup.count <= limits.maximumCleanupBytes else { throw TerminalInputError.capacity }
        if !cleanup.isEmpty {
            guard let old = from.source else { throw TerminalInputError.unavailable }
            _ = try enqueue(connection(old), kind: .cleanup, bytes: cleanup, writer: writer(from))
        }
        let focus = TerminalFocus(owner: owner, stream: from.stream, sceneID: from.sceneID,
                                  generation: from.generation + 1, source: source)
        streams[from.stream] = Stream(focus: focus, writer: nextWriter); return focus
    }
    /// OS suspension cannot wait for composition or a stalled writer. Retires
    /// this responder permanently. Any draft is owned by the native input view.
    /// Returns false only when requested source-bound cleanup could not be queued.
    @discardableResult public func suspend(_ focus: TerminalFocus, cleanup: Data = Data()) throws -> Bool {
        _ = try stream(focus)
        var queued = cleanup.isEmpty
        if !cleanup.isEmpty, cleanup.count <= limits.maximumCleanupBytes, let source = focus.source {
            do { _ = try enqueue(connection(source), kind: .cleanup, bytes: cleanup, writer: writer(focus)); queued = true }
            catch { queued = false }
        }
        streams[focus.stream] = nil
        return queued
    }
    public func beginComposition(_ focus: TerminalFocus) throws -> TerminalComposition {
        var value = try stream(focus)
        _ = try focusedConnection(focus)
        if let composition = value.composition { return composition }
        let composition = TerminalComposition(id: UUID(), focus: focus)
        value.composition = composition; streams[focus.stream] = value
        return composition
    }
    public func cancelComposition(_ composition: TerminalComposition) throws {
        var value = try stream(composition.focus)
        guard value.composition == composition else { throw TerminalInputError.stale }
        value.composition = nil; streams[composition.focus.stream] = value
    }
    /// The shared terminal encoder supplies final bytes. No Enter is synthesized.
    /// A rejected reservation leaves composition pending, so it can be cancelled
    /// or retried on its original source, never silently committed to the next one.
    public func commitComposition(_ composition: TerminalComposition, encodedBytes: Data) throws -> TerminalInputReceipt? {
        var value = try stream(composition.focus)
        guard value.composition == composition else { throw TerminalInputError.stale }
        let connection = try focusedConnection(composition.focus)
        let receipt = encodedBytes.isEmpty ? nil : try enqueue(connection, kind: .encodedInput, bytes: encodedBytes,
                                                               writer: writer(composition.focus))
        value.composition = nil; streams[composition.focus.stream] = value
        return receipt
    }
    public func submit(_ focus: TerminalFocus, encodedBytes: Data) throws -> TerminalInputReceipt {
        guard try stream(focus).composition == nil else { throw TerminalInputError.compositionPending }
        return try enqueue(focusedConnection(focus), kind: .encodedInput, bytes: encodedBytes, writer: writer(focus))
    }
    /// Source-owned key/button release uses the reserved cleanup lane. It does
    /// not finish/cancel a composition or advance focus, and never targets a new
    /// responder merely because the original source is still connected.
    public func cleanup(_ focus: TerminalFocus, encodedBytes: Data) throws -> TerminalInputReceipt? {
        let connection = try focusedConnection(focus, admitting: false)
        guard encodedBytes.count <= limits.maximumCleanupBytes else { throw TerminalInputError.capacity }
        guard !encodedBytes.isEmpty else { return nil }
        return try enqueue(connection, kind: .cleanup, bytes: encodedBytes, writer: writer(focus))
    }
    /// Text has already passed the shared core's newline/mode policy. This last
    /// admission boundary refuses ESC/C1 and non-text controls instead of silently
    /// rewriting them. Raw controls use the explicit shared input encoder, not paste.
    public func paste(_ focus: TerminalFocus, text: String, bracketed: Bool) throws -> TerminalInputReceipt {
        guard try stream(focus).composition == nil else { throw TerminalInputError.compositionPending }
        guard !text.isEmpty else { throw TerminalInputError.emptyInput }
        guard text.utf8.count <= limits.bytesPerConnection else { throw TerminalInputError.capacity }
        guard !text.unicodeScalars.contains(where: {
            ($0.value < 32 && ![9, 10, 13].contains($0.value)) || (127...159).contains($0.value)
        }) else { throw TerminalInputError.unsafePaste }
        let connection = try focusedConnection(focus)
        // Check complete encoded size and the prepaid cancellation tail before
        // making the UTF-8/framing copies. No actor suspension intervenes.
        let encodedCount = text.utf8.count + (bracketed ? Self.pasteStart.count + Self.pasteEnd.count : 0)
        try checkCapacity(connection, kind: .paste, byteCount: encodedCount)
        let payload = Data(text.utf8), start = bracketed ? Self.pasteStart.count : 0
        let bytes = (bracketed ? Self.pasteStart : Data()) + payload + (bracketed ? Self.pasteEnd : Data())
        return try enqueue(connection, kind: .paste, bytes: bytes, writer: writer(focus),
                           pastePayload: start..<(start + payload.count), bracketed: bracketed)
    }
    /// Direct-SSH answerbacks remain active while their viewport is hidden.
    /// This must never be connected to a mux mirror or replayed snapshot.
    public func reply(_ source: TerminalSource, encodedBytes: Data) throws -> TerminalReplyResult {
        let connection = try connection(source)
        guard connection.transport.acceptsModelReplies else {
            if droppedReplies < UInt64.max { droppedReplies += 1 }
            return .droppedWhole
        }
        do { return .queued(try enqueue(connection, kind: .reply, bytes: encodedBytes, writer: connection.transport)) }
        catch TerminalInputError.capacity {
            if droppedReplies < UInt64.max { droppedReplies += 1 }
            return .droppedWhole
        }
    }
    private func checkCapacity(_ connection: Connection, kind: TerminalInputKind, byteCount: Int) throws {
        guard byteCount > 0 else { throw TerminalInputError.emptyInput }
        let isCleanup = kind == .cleanup
        let inputCharge = isCleanup ? 0 : byteCount
        let cleanupCharge = isCleanup ? byteCount : (kind == .paste ? Self.pasteCleanupReservation : 0)
        let totals = accounting()
        guard inputCharge <= limits.bytesPerConnection - connection.inputBytes,
              inputCharge <= limits.totalBytes - totals.retainedBytes,
              cleanupCharge <= limits.cleanupBytesPerConnection - connection.cleanupBytes,
              cleanupCharge <= limits.totalCleanupBytes - totals.cleanupReservationBytes
        else { throw TerminalInputError.capacity }
        if isCleanup {
            guard connection.cleanupRecords < limits.cleanupRecordsPerConnection,
                  totals.cleanupRecords < limits.totalCleanupRecords else { throw TerminalInputError.capacity }
        } else {
            guard connection.records < limits.recordsPerConnection,
                  totals.retainedRecords < limits.totalRecords else { throw TerminalInputError.capacity }
        }
    }
    private func enqueue(_ connection: Connection, kind: TerminalInputKind, bytes: Data, writer: any TerminalInputWriter,
                         pastePayload: Range<Int>? = nil, bracketed: Bool = false) throws -> TerminalInputReceipt {
        try checkCapacity(connection, kind: kind, byteCount: bytes.count)
        let inputCharge = kind == .cleanup ? 0 : bytes.count
        let cleanupCharge = kind == .cleanup ? bytes.count : (kind == .paste ? Self.pasteCleanupReservation : 0)
        let receipt = TerminalInputReceipt(owner: owner, serial: try nextSerial(), source: connection.source)
        connection.queue.append(Operation(receipt, kind, Data(bytes), writer: writer, pastePayload: pastePayload,
                                          bracketed: bracketed, inputCharge: inputCharge, cleanupCharge: cleanupCharge))
        connection.transport.inputAvailable()
        return receipt
    }

    public func cancel(_ receipt: TerminalInputReceipt) throws -> TerminalCancelResult {
        guard receipt.owner == owner else { throw TerminalInputError.stale }
        let connection = try connection(receipt.source, active: false)
        if connection.outcomes.contains(where: { $0.receipt == receipt }) { return .tooLate }
        guard let operation = connection.queue.first(where: { $0.receipt == receipt }) else { throw TerminalInputError.stale }
        if operation.kind != .paste && (operation.cursor > 0 || connection.inFlight == receipt.serial) { return .tooLate }
        operation.cancelRequested = true
        if connection.inFlight == receipt.serial { return .pendingWrite }
        applyCancellation(operation, in: connection)
        connection.transport.inputAvailable()
        return .cancelled
    }
    private func applyCancellation(_ operation: Operation, in connection: Connection) {
        guard operation.cancelRequested, !operation.cleaning else { return }
        if operation.cursor == 0 {
            finish(operation, in: connection, disposition: .cancelledBeforeWrite); return
        }
        guard let payload = operation.pastePayload else { return }
        // The outstanding adapter call has settled. Finish an incomplete UTF-8
        // scalar (at most 3 bytes) or opening delimiter, then close the bracket.
        let at = operation.cursor
        var tail = Data()
        if at < payload.lowerBound {
            tail = operation.bytes.subdata(in: at..<payload.lowerBound)
            if operation.bracketed { tail += Self.pasteEnd }
        } else if at < payload.upperBound {
            var boundary = at
            while boundary < payload.upperBound && operation.bytes[boundary] & 0xC0 == 0x80 { boundary += 1 }
            tail = operation.bytes.subdata(in: at..<boundary)
            if operation.bracketed { tail += Self.pasteEnd }
        } else { tail = operation.bytes.subdata(in: at..<operation.bytes.count) }
        // The complete maximum tail was reserved before admitting this paste.
        operation.bytes = tail; operation.cursor = 0; operation.cleaning = true; operation.inputCharge = 0
        if tail.isEmpty { finish(operation, in: connection, disposition: .cancelledAfterPrefix) }
    }
    private func finish(_ operation: Operation, in connection: Connection, disposition: TerminalInputDisposition) {
        connection.hasAcceptedBytes = connection.hasAcceptedBytes || operation.acceptedOriginal > 0 || operation.acceptedCleanup > 0
        if case .connectionLost(leasedTailUnknown: true) = disposition { connection.hasUnknownTransportOutcome = true }
        connection.outcomes.append(TerminalInputOutcome(receipt: operation.receipt, kind: operation.kind,
            originalBytes: operation.originalBytes, acceptedOriginalBytes: operation.acceptedOriginal,
            acceptedCleanupBytes: operation.acceptedCleanup, disposition: disposition))
        // drainOne may still retain this object while awaiting transport close.
        // Release our payload explicitly before freeing its accounted reservation.
        // This is lifetime control, not a promise of forensic memory erasure.
        operation.bytes = Data(); operation.inputCharge = 0; operation.cleanupCharge = 0
        connection.queue.removeAll { $0.receipt == operation.receipt }
    }

    /// At most one leased chunk per source. Another source or scene remains
    /// usable while this call awaits I/O. No Task-per-key or unbounded retry loop.
    public func drainOne(_ source: TerminalSource, maximumBytes: Int? = nil) async throws -> TerminalDrainResult {
        // A driver cancelled before leasing bytes must not touch the transport.
        // Already admitted operations remain owned by this queue for settlement.
        try Task.checkCancellation()
        let maximum = maximumBytes ?? limits.maximumWriteBytes
        guard maximum > 0, maximum <= limits.maximumWriteBytes else { throw TerminalInputError.invalidWriteSize }
        let connection = try connection(source, active: false)
        guard connection.active else { return .connectionClosed }
        guard connection.inFlight == nil else { return .busy }
        guard let operation = connection.queue.first else { return .idle }
        let count = min(maximum, operation.bytes.count - operation.cursor)
        var bytes = operation.bytes.subdata(in: operation.cursor..<(operation.cursor + count))
        connection.inFlight = operation.receipt.serial; connection.inFlightBytes = count
        let result: TerminalTransportWrite
        do { result = try await operation.writer.writeSome(bytes) }
        catch { result = .failed }
        bytes = Data()
        connection.inFlight = nil; connection.inFlightBytes = 0
        if !connection.active {
            // A concurrent close does not erase a subsequently returned exact
            // prefix/zero-effect result from the already-owned transport call.
            let unknown: Bool
            switch result {
            case .accepted(let written) where written > 0 && written <= count:
                if operation.cleaning { operation.acceptedCleanup += written }
                else { operation.acceptedOriginal += written }
                unknown = false
            case .wouldBlock: unknown = false
            default: unknown = true
            }
            let disposition: TerminalInputDisposition = !unknown && operation.acceptedOriginal == 0 && operation.acceptedCleanup == 0
                ? .definitelyNotWritten : .connectionLost(leasedTailUnknown: unknown)
            finish(operation, in: connection, disposition: disposition)
            return .connectionClosed
        }
        switch result {
        case .wouldBlock:
            applyCancellation(operation, in: connection)
            return .wouldBlock
        case .accepted(let written) where written > 0 && written <= count:
            operation.cursor += written
            if operation.cleaning { operation.acceptedCleanup += written }
            else { operation.acceptedOriginal += written }
            if operation.cursor == operation.bytes.count {
                finish(operation, in: connection, disposition: operation.cleaning ? .cancelledAfterPrefix : .transportAccepted)
            } else { applyCancellation(operation, in: connection) }
            return .progressed(written)
        default:
            finish(operation, in: connection, disposition: .connectionLost(leasedTailUnknown: true))
            invalidate(connection)
            await closeTransport(connection)
            return .connectionClosed
        }
    }
    private func invalidate(_ connection: Connection) {
        connection.active = false
        for operation in connection.queue where operation.receipt.serial != connection.inFlight {
            let disposition: TerminalInputDisposition = operation.acceptedOriginal == 0 && operation.acceptedCleanup == 0
                ? .definitelyNotWritten : .connectionLost(leasedTailUnknown: false)
            finish(operation, in: connection, disposition: disposition)
        }
        // Focus tokens remain source-bound but unusable; no automatic failover.
        // Source-bound composition can be cancelled or explicitly suspended.
    }
    private func closeTransport(_ connection: Connection) async {
        guard !connection.closed else { return }
        // One retained close job per bounded connection, never one per caller.
        // Every concurrent disconnect awaits the same actual settlement.
        let task: Task<Void, Never>
        if let existing = connection.closeTask { task = existing }
        else {
            let transport = connection.transport
            task = Task { await transport.close() }
            connection.closeTask = task
        }
        await task.value
        connection.closed = true
    }
    /// Seal admission in one actor turn before awaiting prompts, writes, or
    /// connection teardown. Queued input settles into the retained outcome
    /// ledger; an already leased write keeps its owner until its result arrives.
    /// This does not close the transport or make the source eligible to retire.
    /// The caller must still disconnect to join actual native teardown.
    public func stopAdmission(_ source: TerminalSource) throws {
        let connection = try connection(source, active: false)
        invalidate(connection)
    }
    public func disconnect(_ source: TerminalSource) async throws {
        let connection = try connection(source, active: false)
        invalidate(connection)
        await closeTransport(connection)
    }
    public func pendingOutcomes(_ source: TerminalSource, maximum: Int = 64) throws -> TerminalOutcomeBatch {
        guard (1...256).contains(maximum) else { throw TerminalInputError.invalidLimits }
        let connection = try connection(source, active: false)
        return TerminalOutcomeBatch(source: source, outcomes: Array(connection.outcomes.prefix(maximum)))
    }
    /// Acknowledgement is local metadata delivery, never replay or remote-effect
    /// authority. A stale/foreign batch cannot consume a newer source's records.
    public func acknowledgeOutcomes(_ batch: TerminalOutcomeBatch) throws {
        let connection = try connection(batch.source, active: false)
        guard !batch.outcomes.isEmpty,
              connection.outcomes.starts(with: batch.outcomes) else { throw TerminalInputError.stale }
        connection.outcomes.removeFirst(batch.outcomes.count)
    }
    /// Consuming content-free outcomes releases record slots, never authorizes a
    /// retry. Callers must preserve any required uncertainty UI/journal separately.
    public func takeOutcomes(_ source: TerminalSource, maximum: Int = 64) throws -> [TerminalInputOutcome] {
        guard (1...256).contains(maximum) else { throw TerminalInputError.invalidLimits }
        let connection = try connection(source, active: false)
        let count = min(maximum, connection.outcomes.count)
        let result = Array(connection.outcomes.prefix(count))
        connection.outcomes.removeFirst(count)
        return result
    }
    public func retire(_ source: TerminalSource) throws {
        let connection = try connection(source, active: false)
        guard connection.closed, connection.inFlight == nil, connection.queue.isEmpty, connection.outcomes.isEmpty,
              !streams.values.contains(where: { $0.focus.source == source }) else { throw TerminalInputError.unavailable }
        sources[source.serial] = nil
    }
    public func mintSeal(for source: TerminalSource) throws -> TerminalInputSeal {
        let connection = try connection(source, active: false)
        guard !connection.active, connection.closed, connection.inFlight == nil,
              connection.queue.isEmpty, connection.outcomes.isEmpty,
              !streams.values.contains(where: { $0.focus.source == source }) else {
            throw TerminalInputError.unavailable
        }
        return TerminalInputSeal(source: source, hasAcceptedBytes: connection.hasAcceptedBytes,
                                 hasUnknownTransportOutcome: connection.hasUnknownTransportOutcome)
    }
    public func accounting() -> TerminalInputAccounting {
        TerminalInputAccounting(connections: sources.count, streams: streams.count,
            retainedBytes: sources.values.reduce(0) { $0 + $1.inputBytes },
            cleanupReservationBytes: sources.values.reduce(0) { $0 + $1.cleanupBytes },
            retainedRecords: sources.values.reduce(0) { $0 + $1.records },
            cleanupRecords: sources.values.reduce(0) { $0 + $1.cleanupRecords },
            inFlightBytes: sources.values.reduce(0) { $0 + $1.inFlightBytes }, droppedReplyUnits: droppedReplies)
    }
}
