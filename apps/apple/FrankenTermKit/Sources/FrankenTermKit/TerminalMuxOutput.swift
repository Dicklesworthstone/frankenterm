import Foundation

/// One owned mux output event, consumed completely before another is admitted.
/// The transport stops pulling decoded events and reading SSH while this slot
/// is occupied. No output is truncated or replaced by a newer frame: the
/// canonical parser may be inside an escape sequence or a Unicode scalar.
///
/// These are bounds on this Swift delivery stage, not the Rust decoder's
/// allocations or a measured whole-process footprint. A retained event stays
/// fully charged until its last chunk is returned; advancing a Data slice does
/// not release the original allocation. The consumer owns at most one further
/// 16 KiB chunk through the session pump's existing synchronous delivery.
public struct TerminalMuxOutputBuffer: Sendable {
    public static let maximumEventBytes = 4 * 1024 * 1024
    public static let maximumChunkBytes = 16_384
    public static let paletteBytes = 263 * 4

    private struct Event: Sendable {
        let bytes: Data
        let geometry: TerminalSessionGeometry
        let destination: TerminalMuxInputRouting.Destination?
        let palette: Data?
        let preparing: Bool
        var offset = 0
    }
    private var event: Event?
    private var pixelGeometry: TerminalSessionGeometry?
    private var emittedGeometry: TerminalSessionGeometry
    public private(set) var queuedGeometry: TerminalSessionGeometry
    public private(set) var isRetired = false

    public init(geometry: TerminalSessionGeometry) {
        emittedGeometry = geometry; queuedGeometry = geometry
    }
    public var isEmpty: Bool { event == nil && pixelGeometry == nil }
    public var retainedBytes: Int { event.map { $0.bytes.count + ($0.palette?.count ?? 0) } ?? 0 }
    public var remainingBytes: Int { event.map { $0.bytes.count - $0.offset + ($0.palette?.count ?? 0) } ?? 0 }

    /// A false capacity assumption is an owner error, never permission to drop
    /// the preceding event. Oversized payloads fail before making an owned copy.
    public mutating func append(_ bytes: Data, geometry: TerminalSessionGeometry,
                                destination: TerminalMuxInputRouting.Destination?, preparing: Bool = false) throws {
        guard !isRetired else { throw TerminalInputError.unavailable }
        guard isEmpty else { throw TerminalInputError.capacity }
        guard bytes.count <= Self.maximumEventBytes else { throw TerminalInputError.capacity }
        guard !preparing || destination == nil else { throw TerminalInputError.invalidIdentity }
        // Copy only the selected extent: a small slice must not retain an
        // arbitrarily larger backing buffer. Nonzero Data indices are valid.
        let owned = bytes.withUnsafeBytes { Data($0) }
        event = Event(bytes: owned, geometry: geometry, destination: destination, palette: nil, preparing: preparing)
        queuedGeometry = geometry
    }

    /// Colors share the same single-event slot as output. They cannot overtake
    /// a reset or the retained suffix of a preceding frame, or enable input.
    public mutating func appendPalette(_ rgba: Data) throws {
        guard !isRetired else { throw TerminalInputError.unavailable }
        guard isEmpty else { throw TerminalInputError.capacity }
        guard rgba.count == Self.paletteBytes else { throw TerminalInputError.invalidLimits }
        let owned = rgba.withUnsafeBytes { Data($0) }
        event = Event(bytes: Data(), geometry: queuedGeometry, destination: nil, palette: owned, preparing: true)
    }

    /// Same-cell pixel changes need no remote redraw. Keep one latest value
    /// AFTER the pending event; thousands of layout callbacks cannot append
    /// thousands of empty segments. A cell-count change needs server output.
    public mutating func requestPixelGeometry(_ geometry: TerminalSessionGeometry) throws {
        guard !isRetired else { throw TerminalInputError.unavailable }
        guard geometry.rows == queuedGeometry.rows, geometry.columns == queuedGeometry.columns else {
            throw TerminalInputError.invalidLimits
        }
        pixelGeometry = geometry
        queuedGeometry = geometry
    }

    /// Only the first chunk carries the event's geometry change and only the
    /// last carries its exact pane-visit witness. Empty events still deliver
    /// their witness once. Returning a witness is NOT model-consumption proof;
    /// TerminalSessionPump acknowledges it after its consumer succeeds.
    public mutating func next() -> TerminalSessionUpdate? {
        guard !isRetired else { return nil }
        if var current = event {
            let end = min(current.bytes.count, current.offset + Self.maximumChunkBytes)
            let bytes = current.bytes.subdata(in: current.offset..<end)
            let resize = current.geometry == emittedGeometry ? nil : current.geometry
            emittedGeometry = current.geometry
            current.offset = end
            let complete = end == current.bytes.count
            event = complete ? nil : current
            return TerminalSessionUpdate(output: bytes, resized: resize,
                muxDestination: complete ? current.destination : nil,
                muxPalette: current.palette, muxPreparation: current.preparing || !complete)
        }
        if let geometry = pixelGeometry {
            pixelGeometry = nil
            let resize = geometry == emittedGeometry ? nil : geometry
            emittedGeometry = geometry
            return TerminalSessionUpdate(output: Data(), resized: resize)
        }
        return nil
    }

    /// Teardown only. No later callback can repopulate this slot. A consumer's
    /// already returned chunk remains independently owned until it settles.
    public mutating func retire() {
        isRetired = true; event = nil; pixelGeometry = nil
    }
}
