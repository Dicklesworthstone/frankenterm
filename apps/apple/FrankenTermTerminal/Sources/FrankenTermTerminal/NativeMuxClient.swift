import Foundation
import FrankenTermKit
import CFrankenTermTerminal

/// The Rust mux client (frankenterm-mobile-mux) behind ft_mux_v1_*: the
/// protocol of the machine's FrankenTerm mux server, the same one the
/// desktop app attaches to.
///
/// Sendability audit: lock serializes every C call and the handle. C keeps no
/// borrowed buffer and never calls Swift. Returned Data owns its bytes.
public final class NativeMuxClient: TerminalMuxClient, @unchecked Sendable {
    private let lock = NSLock()
    private var handle: UInt64

    public init() throws {
        var created: UInt64 = 0
        try Self.check(ft_mux_v1_create(&created))
        guard created != 0 else { throw TerminalModelError.invalid }
        handle = created
    }

    deinit { if handle != 0 { _ = ft_mux_v1_release(handle) } }

    private static func check(_ status: UInt32) throws {
        switch status {
        case UInt32(FT_TERMINAL_OK): return
        case UInt32(FT_TERMINAL_INVALID): throw TerminalModelError.invalid
        case UInt32(FT_TERMINAL_STALE): throw TerminalModelError.stale
        case UInt32(FT_TERMINAL_CLOSED): throw TerminalModelError.closed
        case UInt32(FT_TERMINAL_CAPACITY): throw TerminalModelError.capacity
        case UInt32(FT_TERMINAL_PANIC): throw TerminalModelError.poisoned
        default: throw TerminalModelError.invalid
        }
    }

    private func call(_ operation: (UInt64) -> UInt32) throws {
        try lock.withLock { try Self.check(operation(handle)) }
    }

    public func receive(_ bytes: Data) throws {
        var offset = 0
        while offset < bytes.count {
            let count = min(bytes.count - offset, Int(FT_MUX_MAX_RECEIVE_BYTES))
            try bytes[bytes.startIndex + offset ..< bytes.startIndex + offset + count].withUnsafeBytes { buffer in
                try call { ft_mux_v1_receive($0, buffer.bindMemory(to: UInt8.self).baseAddress, UInt32(count)) }
            }
            offset += count
        }
    }

    public func channelClosed() throws { try call { ft_mux_v1_closed($0) } }

    public func outgoing(limit: Int) throws -> Data {
        guard limit > 0 else { return Data() }
        let capacity = min(limit, Int(FT_MUX_MAX_COPY_BYTES))
        var bytes = Data(count: capacity), written: UInt32 = 0
        try bytes.withUnsafeMutableBytes { buffer in
            try call { ft_mux_v1_outgoing($0, buffer.bindMemory(to: UInt8.self).baseAddress, UInt32(capacity), &written) }
        }
        guard written <= capacity else { throw TerminalModelError.invalid }
        bytes.count = Int(written)
        return bytes
    }

    public func nextEvent() throws -> TerminalMuxEvent? {
        try lock.withLock {
            var kind: UInt32 = 0, pane: UInt64 = 0, rows: UInt32 = 0, cols: UInt32 = 0, length: UInt32 = 0
            try Self.check(ft_mux_v1_next_event(handle, &kind, &pane, &rows, &cols, &length))
            // Refuse before allocating or copying an event into Swift. Rust
            // decoder allocations are a separate bound; this is the native
            // delivery profile shared with the transport's single-event slot.
            guard length <= TerminalMuxOutputBuffer.maximumEventBytes else { throw TerminalModelError.capacity }
            guard let paneID = Int(exactly: pane) else { throw TerminalModelError.invalid }
            if kind == UInt32(FT_MUX_EVENT_NONE) {
                guard length == 0 else { throw TerminalModelError.invalid }
                return nil
            }
            var payload = Data(count: Int(length))
            if length > 0 {
                var written: UInt32 = 0
                try payload.withUnsafeMutableBytes { buffer in
                    try Self.check(ft_mux_v1_event_copy(handle, 0, buffer.bindMemory(to: UInt8.self).baseAddress,
                                                        length, &written))
                }
                guard written == length else { throw TerminalModelError.invalid }
            }
            switch kind {
            case UInt32(FT_MUX_EVENT_NONE): return nil
            case UInt32(FT_MUX_EVENT_READY):
                return .ready(serverVersion: String(decoding: payload, as: UTF8.self))
            case UInt32(FT_MUX_EVENT_PANES):
                let decoder = JSONDecoder()
                decoder.keyDecodingStrategy = .convertFromSnakeCase
                return .panes(try decoder.decode([TerminalMuxPane].self, from: payload))
            case UInt32(FT_MUX_EVENT_OUTPUT):
                return .output(paneID: paneID, rows: Int(rows), columns: Int(cols), bytes: payload)
            case UInt32(FT_MUX_EVENT_PANE_TITLE):
                return .paneTitle(paneID: paneID, title: String(decoding: payload, as: UTF8.self))
            case UInt32(FT_MUX_EVENT_PANE_GONE): return .paneGone(paneID: paneID)
            case UInt32(FT_MUX_EVENT_FAILED): return .failed(String(decoding: payload, as: UTF8.self))
            case UInt32(FT_MUX_EVENT_SEARCH_RESULTS):
                let decoder = JSONDecoder()
                decoder.keyDecodingStrategy = .convertFromSnakeCase
                return .searchResults(paneID: paneID, try decoder.decode(TerminalMuxSearchResults.self, from: payload))
            case UInt32(FT_MUX_EVENT_CONTEXT):
                return .context(paneID: paneID, try JSONDecoder().decode(TerminalMuxContext.self, from: payload))
            case UInt32(FT_MUX_EVENT_MOUSE_GRAB):
                guard payload.count == 1, payload[payload.startIndex] <= 1 else { throw TerminalModelError.invalid }
                return .mouseGrab(paneID: paneID, grabbed: payload[payload.startIndex] == 1)
            case UInt32(FT_MUX_EVENT_ACTIVITY):
                guard payload.isEmpty else { throw TerminalModelError.invalid }
                return .activity(paneID: paneID)
            case UInt32(FT_MUX_EVENT_PHONE_SIZE):
                let decoder = JSONDecoder()
                decoder.keyDecodingStrategy = .convertFromSnakeCase
                let desktop = payload.isEmpty ? nil : try decoder.decode(TerminalMuxSize.self, from: payload)
                return .phoneSize(paneID: paneID, desktop: desktop, rows: Int(rows), columns: Int(cols))
            default: throw TerminalModelError.invalid
            }
        }
    }

    public func refreshPanes() throws { try call { ft_mux_v1_refresh_panes($0) } }
    private static func paneID(_ pane: Int) throws -> UInt64 {
        guard let value = UInt64(exactly: pane) else { throw TerminalModelError.invalid }
        return value
    }
    public func show(pane: Int) throws {
        let pane = try Self.paneID(pane)
        try call { ft_mux_v1_show($0, pane) }
    }
    public func hide(pane: Int) throws {
        let pane = try Self.paneID(pane)
        try call { ft_mux_v1_hide($0, pane) }
    }

    public func write(pane: Int, bytes: Data) throws {
        let pane = try Self.paneID(pane)
        guard !bytes.isEmpty else { return }
        guard bytes.count <= Int(FT_MUX_MAX_WRITE_BYTES) else { throw TerminalModelError.capacity }
        try bytes.withUnsafeBytes { buffer in
            try call { ft_mux_v1_write($0, pane, buffer.bindMemory(to: UInt8.self).baseAddress, UInt32(bytes.count)) }
        }
    }

    public func paste(pane: Int, text: String) throws {
        let pane = try Self.paneID(pane)
        guard text.utf8.count <= Int(FT_MUX_MAX_WRITE_BYTES) else { throw TerminalModelError.capacity }
        let bytes = Data(text.utf8)
        guard !bytes.isEmpty else { return }
        guard bytes.count <= Int(FT_MUX_MAX_WRITE_BYTES) else { throw TerminalModelError.capacity }
        try bytes.withUnsafeBytes { buffer in
            try call { ft_mux_v1_paste($0, pane, buffer.bindMemory(to: UInt8.self).baseAddress, UInt32(bytes.count)) }
        }
    }

    public func resize(pane: Int, rows: Int, columns: Int) throws {
        let pane = try Self.paneID(pane)
        guard (1...512).contains(rows), (1...512).contains(columns) else { throw TerminalModelError.invalid }
        try call { ft_mux_v1_resize($0, pane, UInt32(rows), UInt32(columns)) }
    }

    public func restoreSize(pane: Int) throws {
        let pane = try Self.paneID(pane)
        try call { ft_mux_v1_restore_size($0, pane) }
    }

    public func restoring() throws -> Bool {
        var restoring: UInt32 = 0
        try call { ft_mux_v1_restoring($0, &restoring) }
        return restoring == 1
    }

    public func unansweredInput(pane: Int) throws -> Bool {
        let pane = try Self.paneID(pane)
        var unanswered: UInt32 = 0
        try call { ft_mux_v1_unanswered_input($0, pane, &unanswered) }
        return unanswered == 1
    }

    public func click(pane: Int, column: Int, row: Int) throws {
        let pane = try Self.paneID(pane)
        guard (0...4096).contains(column), (0...4096).contains(row) else { throw TerminalModelError.invalid }
        try call { ft_mux_v1_click($0, pane, UInt32(column), UInt32(row)) }
    }

    public func adoptPhoneSize(pane: Int, desktop: TerminalMuxSize, rows: Int, columns: Int) throws -> Bool {
        let pane = try Self.paneID(pane)
        guard (1...4096).contains(desktop.rows), (1...4096).contains(desktop.cols),
              (1...512).contains(rows), (1...512).contains(columns),
              let pixelWidth = UInt32(exactly: desktop.pixelWidth), let pixelHeight = UInt32(exactly: desktop.pixelHeight),
              let dpi = UInt32(exactly: desktop.dpi) else { throw TerminalModelError.invalid }
        var adopted: UInt32 = 0
        try call {
            ft_mux_v1_adopt_phone_size($0, pane, UInt32(desktop.rows), UInt32(desktop.cols), pixelWidth, pixelHeight,
                                       dpi, UInt32(rows), UInt32(columns), &adopted)
        }
        return adopted == 1
    }

    public func wheel(pane: Int, steps: Int, column: Int, row: Int) throws {
        let pane = try Self.paneID(pane)
        guard steps != 0, let steps = Int32(exactly: max(-64, min(64, steps))),
              (0...4096).contains(column), (0...4096).contains(row) else { throw TerminalModelError.invalid }
        try call { ft_mux_v1_wheel($0, pane, steps, UInt32(column), UInt32(row)) }
    }

    public func search(pane: Int, text: String, caseSensitive: Bool, regex: Bool) throws -> UInt64 {
        let pane = try Self.paneID(pane)
        guard (1...4096).contains(text.utf8.count) else { throw TerminalModelError.invalid }
        let bytes = Data(text.utf8)
        guard (1...4096).contains(bytes.count) else { throw TerminalModelError.invalid }
        let flags = (caseSensitive ? UInt32(FT_MUX_SEARCH_CASE_SENSITIVE) : 0) | (regex ? UInt32(FT_MUX_SEARCH_REGEX) : 0)
        var search: UInt64 = 0
        try bytes.withUnsafeBytes { buffer in
            try call {
                ft_mux_v1_search($0, pane, buffer.bindMemory(to: UInt8.self).baseAddress,
                                 UInt32(bytes.count), flags, &search)
            }
        }
        return search
    }

    public func context(pane: Int, firstRow: Int64, count: Int) throws -> UInt64 {
        let pane = try Self.paneID(pane)
        guard (1...1000).contains(count) else { throw TerminalModelError.invalid }
        guard !firstRow.addingReportingOverflow(Int64(count)).overflow else { throw TerminalModelError.invalid }
        var request: UInt64 = 0
        try call { ft_mux_v1_context($0, pane, firstRow, UInt32(count), &request) }
        return request
    }
}
