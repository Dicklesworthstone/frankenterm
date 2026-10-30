import Foundation
import FrankenTermKit
import CFrankenTermTerminal

/// The actual canonical Rust terminal, exposed through generation-fenced handles.
/// Transfer this model to one TerminalModelRuntime; do not retain a second driver.
/// There is no Swift ANSI parser, local echo, transport writer or pending task.
///
/// Sendability audit: lock serializes all access to handle/closing and every C
/// call. C retains no borrowed buffer, never calls Swift, and performs bounded
/// synchronous model work. Returned Data owns its bytes. Native release cannot
/// fail from unrelated-session contention and also releases any owned frame.
public final class NativeTerminalModel: TerminalSessionModel, @unchecked Sendable {
    public let source: TerminalSource
    private let lock = NSLock()
    private var handle: UInt64
    private var closing = false

    /// Keeps at most `scrollbackRows` of history, fewer beside a grid too
    /// large for them. A mux pane's mirror sends no more history than the
    /// default keeps (`frankenterm_mobile_mux::mirror::history_rows`).
    public init(source: TerminalSource, geometry: TerminalSessionGeometry,
                scrollbackRows: UInt32 = 256, fontRevision: UInt64 = 1) throws {
        guard MemoryLayout<ft_terminal_v1_config>.size == 80 else { throw TerminalModelError.unsupported }
        guard scrollbackRows <= 4096, fontRevision > 0,
              (UInt64(geometry.rows) * 2 + UInt64(scrollbackRows)) * UInt64(geometry.columns) <= 65_536
        else { throw TerminalModelError.capacity }
        var configuration = ft_terminal_v1_config()
        configuration.abi_version = UInt32(FT_TERMINAL_ABI_VERSION)
        configuration.struct_size = 80
        configuration.columns = geometry.columns; configuration.rows = geometry.rows
        configuration.pixel_width = geometry.pixelWidth; configuration.pixel_height = geometry.pixelHeight
        configuration.scrollback_rows = scrollbackRows
        configuration.route_revision = source.identity.routeRevision
        configuration.font_revision = fontRevision
        withUnsafeBytes(of: source.identity.channelID.uuid) { bytes in
            withUnsafeMutableBytes(of: &configuration.channel_id) { $0.copyMemory(from: bytes) }
        }
        withUnsafeBytes(of: source.identity.transportID.uuid) { bytes in
            withUnsafeMutableBytes(of: &configuration.transport_id) { $0.copyMemory(from: bytes) }
        }
        var created: UInt64 = 0
        try Self.check(ft_terminal_v1_create(&configuration, &created))
        guard created != 0 else { throw TerminalModelError.invalid }
        self.source = source; handle = created
    }

    deinit {
        // No aliases remain. release closes the model and invalidates its frame;
        // it neither needs an executor nor leaves asynchronous cleanup behind.
        if handle != 0 { _ = ft_terminal_v1_release(handle) }
    }

    private static func check(_ status: UInt32) throws {
        switch status {
        case UInt32(FT_TERMINAL_OK): return
        case UInt32(FT_TERMINAL_INVALID): throw TerminalModelError.invalid
        case UInt32(FT_TERMINAL_STALE): throw TerminalModelError.stale
        case UInt32(FT_TERMINAL_CLOSED): throw TerminalModelError.closed
        case UInt32(FT_TERMINAL_CAPACITY): throw TerminalModelError.capacity
        case UInt32(FT_TERMINAL_BUSY): throw TerminalModelError.busy
        case UInt32(FT_TERMINAL_UNSUPPORTED): throw TerminalModelError.unsupported
        case UInt32(FT_TERMINAL_PANIC): throw TerminalModelError.poisoned
        case UInt32(FT_TERMINAL_SEQUENCE_EXHAUSTED): throw TerminalModelError.exhausted
        case UInt32(FT_TERMINAL_VIEWPORT_UNAVAILABLE): throw TerminalModelError.viewportUnavailable
        default: throw TerminalModelError.invalid
        }
    }

    private func withHandle<T>(_ operation: (UInt64) throws -> T) throws -> T {
        try lock.withLock {
            guard handle != 0, !closing else { throw TerminalModelError.closed }
            return try operation(handle)
        }
    }
    private static func requireCapacity(_ capacity: Int) throws {
        guard (1...Int(FT_TERMINAL_MAX_INPUT_BYTES)).contains(capacity) else { throw TerminalModelError.capacity }
    }
    private static func output(capacity: Int,
                               _ operation: (UnsafeMutablePointer<UInt8>?, UInt32, UnsafeMutablePointer<UInt32>) -> UInt32) throws -> Data {
        // Every caller has checked a fixed ABI limit before reaching allocation.
        var bytes = Data(count: capacity), written: UInt32 = 0
        let status = bytes.withUnsafeMutableBytes {
            operation($0.bindMemory(to: UInt8.self).baseAddress, UInt32(capacity), &written)
        }
        try check(status)
        guard written <= capacity else { throw TerminalModelError.invalid }
        bytes.removeSubrange(Int(written)..<bytes.count)
        return bytes
    }

    public func feed(_ bytes: Data) throws {
        guard bytes.count <= Int(FT_TERMINAL_MAX_FEED_BYTES) else { throw TerminalModelError.capacity }
        try withHandle { session in
            try Self.check(bytes.withUnsafeBytes {
                ft_terminal_v1_feed(session, $0.bindMemory(to: UInt8.self).baseAddress, UInt32(bytes.count))
            })
        }
    }
    public func resize(_ geometry: TerminalSessionGeometry) throws {
        try withHandle {
            try Self.check(ft_terminal_v1_resize($0, geometry.columns, geometry.rows, geometry.pixelWidth, geometry.pixelHeight))
        }
    }
    public func setFontRevision(_ revision: UInt64) throws {
        try withHandle { try Self.check(ft_terminal_v1_set_font_revision($0, revision)) }
    }
    /// The machine's theme becomes the configured palette; terminal OSC color
    /// changes remain transient overrides. See Theme.terminalBasePalette.
    public func setBasePalette(_ rgba: [UInt8]) throws {
        guard rgba.count == Int(FT_TERMINAL_BASE_PALETTE_BYTES) else { throw TerminalModelError.invalid }
        try withHandle { session in
            try Self.check(rgba.withUnsafeBufferPointer {
                ft_terminal_v1_set_base_palette(session, $0.baseAddress, UInt32(rgba.count))
            })
        }
    }

    /// A coherent mux snapshot owns these colors only until its next reset.
    /// Keep the machine theme intact for a later pane or direct shell.
    public func setMuxPalette(_ rgba: [UInt8]) throws {
        guard rgba.count == Int(FT_TERMINAL_BASE_PALETTE_BYTES) else { throw TerminalModelError.invalid }
        try withHandle { session in
            try Self.check(rgba.withUnsafeBufferPointer {
                ft_terminal_v1_set_mux_palette(session, $0.baseAddress, UInt32(rgba.count))
            })
        }
    }

    private static func encodeKey(_ key: TerminalHardwareKey, phase: TerminalKeyPhase,
                                  session: UInt64, capacity: Int) throws -> Data {
        let nativePhase: UInt32
        switch phase {
        case .down: nativePhase = 0
        case .repeated: nativePhase = 1
        case .up: nativePhase = 2
        case .press: nativePhase = 3
        }
        let characters = Data(key.characters.utf8) // HardwareKey limits this to 256 bytes.
        return try characters.withUnsafeBytes { text in
            try output(capacity: capacity) { bytes, count, written in
                ft_terminal_v1_encode_key(session, UInt32(key.usage), UInt32(key.modifiers.rawValue), nativePhase,
                    text.bindMemory(to: UInt8.self).baseAddress, UInt32(characters.count), bytes, count, written)
            }
        }
    }
    public func encode(_ intent: TerminalKeyboardIntent, maximumBytes: Int) throws -> Data {
        try Self.requireCapacity(maximumBytes)
        return try withHandle { session in
            switch intent {
            case .text(let text):
                guard text.utf8.count <= Int(FT_TERMINAL_MAX_TEXT_BYTES) else { throw TerminalModelError.capacity }
                let input = Data(text.utf8)
                return try input.withUnsafeBytes { text in
                    try Self.output(capacity: maximumBytes) { bytes, count, written in
                        ft_terminal_v1_encode_text(session, text.bindMemory(to: UInt8.self).baseAddress,
                                                  UInt32(input.count), bytes, count, written)
                    }
                }
            case .key(let key, let phase):
                return try Self.encodeKey(key, phase: phase, session: session, capacity: maximumBytes)
            case .releaseAll(let keys):
                guard keys.count <= 32 else { throw TerminalModelError.capacity }
                var result = Data()
                for key in keys {
                    let bytes = try Self.encodeKey(key, phase: .up, session: session, capacity: maximumBytes - result.count)
                    result.append(bytes)
                }
                // All encoders are pure. A failed later key admits no prefix.
                return result
            }
        }
    }

    public func preparePaste(_ text: String, maximumBytes: Int) throws -> TerminalPreparedPaste {
        try Self.requireCapacity(maximumBytes)
        guard text.utf8.count <= maximumBytes else { throw TerminalModelError.capacity }
        return try withHandle { session in
            let input = Data(text.utf8)
            var flags: UInt32 = 0
            let prepared = try input.withUnsafeBytes { text in
                try Self.output(capacity: maximumBytes) { bytes, count, written in
                    ft_terminal_v1_prepare_paste(session, text.bindMemory(to: UInt8.self).baseAddress,
                        UInt32(input.count), bytes, count, written, &flags)
                }
            }
            guard flags & ~UInt32(1) == 0, let normalized = String(data: prepared, encoding: .utf8) else {
                throw TerminalModelError.invalid
            }
            return TerminalPreparedPaste(text: normalized, bracketed: flags & 1 != 0)
        }
    }

    public func encodePointer(_ event: TerminalPointerEvent, maximumBytes: Int) throws -> Data {
        try Self.requireCapacity(maximumBytes)
        guard event.source == source else { throw TerminalInputError.stale }
        let action: UInt32, steps: Int32
        switch event.action {
        case .click: action = 0; steps = 0
        case .wheel(let count):
            guard count != 0, (-32...32).contains(count) else { throw TerminalModelError.invalid }
            action = 1; steps = Int32(count)
        }
        return try withHandle { session in
            try Self.output(capacity: maximumBytes) { bytes, count, written in
                ft_terminal_v1_encode_mouse(session, action, steps, event.column, event.row,
                    UInt32(event.modifiers.rawValue), event.geometryRevision, bytes, count, written)
            }
        }
    }

    public func copyReply() throws -> TerminalModelReply? {
        try withHandle { session in
            var sequence: UInt64 = 0
            let bytes = try Self.output(capacity: Int(FT_TERMINAL_MAX_REPLY_BYTES)) { output, count, written in
                ft_terminal_v1_reply_copy(session, output, count, written, &sequence)
            }
            if bytes.isEmpty, sequence == 0 { return nil }
            return try TerminalModelReply(sequence: sequence, bytes: bytes)
        }
    }
    public func settleReply(sequence: UInt64, disposition: TerminalModelReplyDisposition) throws {
        let nativeDisposition: UInt32
        switch disposition { case .accepted: nativeDisposition = 0; case .droppedWhole: nativeDisposition = 1 }
        try withHandle { try Self.check(ft_terminal_v1_reply_settle($0, sequence, nativeDisposition)) }
    }

    public func captureFrame(viewportTop: Int64?) throws -> Data {
        guard viewportTop != Int64.min else { throw TerminalModelError.invalid }
        return try withHandle { session in
            var frame: UInt64 = 0, length: UInt32 = 0
            try Self.check(ft_terminal_v1_frame_acquire(session, viewportTop ?? Int64.min, &frame, &length))
            var releaseNeeded = true
            defer { if releaseNeeded { _ = ft_terminal_v1_frame_release(frame) } }
            guard frame != 0, length > 0, length <= FT_TERMINAL_MAX_FRAME_BYTES else { throw TerminalModelError.invalid }
            let bytes = try Self.output(capacity: Int(length)) { output, count, written in
                ft_terminal_v1_frame_copy(frame, 0, output, count, written)
            }
            guard bytes.count == length else { throw TerminalModelError.invalid }
            try Self.check(ft_terminal_v1_frame_release(frame)); releaseNeeded = false
            return bytes
        }
    }

    public func close() throws {
        try lock.withLock {
            guard handle != 0 else { return }
            closing = true
            try Self.check(ft_terminal_v1_release(handle))
            handle = 0
        }
    }
}
