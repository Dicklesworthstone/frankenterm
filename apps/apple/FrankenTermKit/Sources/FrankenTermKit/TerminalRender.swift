import Foundation

public enum TerminalRenderError: Error, Equatable, Sendable {
    case malformed, capacity, staleSource, staleFrame, unsupported, unavailable, exhausted
}

/// Limits apply before copying the input or allocating its decoded tables.
public struct TerminalRenderLimits: Sendable {
    public var maximumFrameBytes = 8 * 1024 * 1024
    public var maximumRows = 512
    public var maximumColumns = 512
    public var maximumCells = 65536
    public var maximumTextBytes = 1024 * 1024
    public var maximumCellTextBytes = 4096
    public var maximumLinks = 4096
    public var maximumLinkBytes = 4096
    public init() {}

    func validate() throws {
        guard (1316...8_388_608).contains(maximumFrameBytes),
              (1...512).contains(maximumRows), (1...512).contains(maximumColumns),
              (1...65536).contains(maximumCells), (1...1_048_576).contains(maximumTextBytes),
              (1...16384).contains(maximumCellTextBytes),
              (0...4096).contains(maximumLinks), (1...4096).contains(maximumLinkBytes)
        else { throw TerminalRenderError.capacity }
    }
}

public struct TerminalRenderRGBA: Equatable, Hashable, Sendable {
    public let red: UInt8, green: UInt8, blue: UInt8, alpha: UInt8
    public init(_ red: UInt8, _ green: UInt8, _ blue: UInt8, _ alpha: UInt8 = 255) {
        self.red = red; self.green = green; self.blue = blue; self.alpha = alpha
    }
    var bytes: [UInt8] { [red, green, blue, alpha] }
    var linearPremultiplied: [Float] {
        let a = Float(alpha) / 255
        return [Self.linear(red) * a, Self.linear(green) * a, Self.linear(blue) * a, a]
    }
    static func linear(_ byte: UInt8) -> Float {
        let c = Float(byte) / 255
        return c <= 0.04045 ? c / 12.92 : powf((c + 0.055) / 1.055, 2.4)
    }
}

public enum TerminalRenderColor: Equatable, Sendable {
    case `default`, palette(UInt8), rgba(TerminalRenderRGBA)
    func resolve(_ palette: [TerminalRenderRGBA], fallback: Int, boldBright: Bool = false) -> TerminalRenderRGBA {
        switch self {
        case .default: return palette[fallback]
        case let .palette(index): return palette[Int(index) + (boldBright && index < 8 ? 8 : 0)]
        case let .rgba(color): return color
        }
    }
}

public struct TerminalRenderCell: Equatable, Sendable {
    public let column: Int, width: Int
    public let text: String
    public let flags: UInt32
    public let foreground: TerminalRenderColor, background: TerminalRenderColor, underline: TerminalRenderColor
    public let linkID: Int
    public var intensity: UInt32 { flags & 3 }
    public var underlineStyle: UInt32 { (flags >> 2) & 7 }
    public var blink: UInt32 { (flags >> 5) & 3 }
    public var italic: Bool { flags & (1 << 7) != 0 }
    public var reverse: Bool { flags & (1 << 8) != 0 }
    public var strikethrough: Bool { flags & (1 << 9) != 0 }
    public var invisible: Bool { flags & (1 << 10) != 0 }
    public var overline: Bool { flags & (1 << 11) != 0 }
    public var verticalAlignment: UInt32 { (flags >> 12) & 3 }
}

public struct TerminalRenderRow: Equatable, Sendable {
    public let stableIndex: Int64, sequence: UInt64
    public let cells: [TerminalRenderCell]

    /// The row as a reader hears it: gaps and blank cells are spaces, concealed
    /// (SGR 8) cells are withheld as spaces, and trailing blanks are dropped.
    public var readableText: String {
        var text = "", column = 0
        for cell in cells where cell.column >= column {
            text.append(String(repeating: " ", count: cell.column - column))
            text.append(cell.invisible || cell.text.isEmpty ? String(repeating: " ", count: cell.width) : cell.text)
            column = cell.column + cell.width
        }
        while text.last == " " { text.removeLast() }
        return text
    }
}

public struct TerminalRenderCursor: Equatable, Sendable {
    public let stableRow: Int64
    public let column: Int, shape: UInt32, width: Int
    public let visible: Bool
    public var blinks: Bool { [0, 1, 3, 5].contains(shape) }
}

/// UTF-16 indices used by Core Text are mapped back to canonical terminal cells.
/// A wide cell contributes its grapheme once; an actual gap contributes spaces.
/// Paint changes do not split the shaping string. No Unicode width is recomputed.
struct TerminalRenderTextLayout {
    let text: String
    let ranges: [Range<Int>]
    let cellIndices: [Int?]
    init(row: TerminalRenderRow, maximumUTF16: Int = 32768) throws {
        guard (1...32768).contains(maximumUTF16) else { throw TerminalRenderError.capacity }
        var text = "", ranges = [Range<Int>](), mapping = [Int?](), column = 0
        for (index, cell) in row.cells.enumerated() {
            let gap = cell.column - column, length = cell.text.utf16.count
            guard gap >= 0, gap <= maximumUTF16 - mapping.count,
                  length <= maximumUTF16 - mapping.count - gap else { throw TerminalRenderError.capacity }
            if gap > 0 {
                text.append(String(repeating: " ", count: gap)); mapping.append(contentsOf: repeatElement(nil, count: gap))
            }
            let start = mapping.count
            text.append(cell.text); mapping.append(contentsOf: repeatElement(Optional(index), count: length))
            ranges.append(start..<mapping.count); column = cell.column + cell.width
        }
        self.text = text; self.ranges = ranges; cellIndices = mapping
    }
}

/// An owned, complete semantic viewport emitted by frankenterm-term's FTSF v1
/// serializer. It never contains escape sequences or native pointers. Decode is
/// deliberately independent of UIKit so malformed frames can be tested on Linux.
public struct TerminalRenderFrame: Equatable, Sendable {
    public let source: TerminalSource
    public let sequence: UInt64, geometryRevision: UInt64, paletteRevision: UInt64
    public let fontRevision: UInt64, revision: UInt64
    public let rows: Int, columns: Int, viewportFirst: Int64
    public let flags: UInt32
    public let cursor: TerminalRenderCursor
    public let lines: [TerminalRenderRow]
    public let palette: [TerminalRenderRGBA]
    public let links: [String]
    public var reverseVideo: Bool { flags & 2 != 0 }
    public var alternateScreen: Bool { flags & 4 != 0 }
    /// The viewport one line per row for VoiceOver and UI automation, without
    /// the blank rows below the last written one.
    public var readableText: String {
        var rows = lines.map(\.readableText)
        while rows.last?.isEmpty == true { rows.removeLast() }
        return rows.joined(separator: "\n")
    }

    public static func decode(_ data: Data, expectedSource: TerminalSource,
                              limits: TerminalRenderLimits = .init()) throws -> Self {
        try limits.validate()
        guard data.count <= limits.maximumFrameBytes else { throw TerminalRenderError.capacity }
        guard data.count >= 256 else { throw TerminalRenderError.malformed }
        let bytes = [UInt8](data)
        let read = RenderBytes(bytes)
        guard Array(bytes[0..<4]) == [70, 84, 83, 70], read.u16(4) == 1,
              read.u16(6) == 256, Int(read.u32(8)) == bytes.count,
              read.u32(12) & 1 != 0, read.u32(12) & ~UInt32(15) == 0,
              bytes[176..<256].allSatisfy({ $0 == 0 })
        else { throw TerminalRenderError.malformed }
        let identity = expectedSource.identity
        guard Array(bytes[16..<32]) == uuidBytes(identity.channelID),
              Array(bytes[32..<48]) == uuidBytes(identity.transportID),
              read.u64(48) == identity.routeRevision else { throw TerminalRenderError.staleSource }
        let rows = Int(read.u32(96)), columns = Int(read.u32(100))
        let count = Int(read.u32(132)), textCount = Int(read.u32(136)), linkCount = Int(read.u32(160))
        guard rows > 0, columns > 0 else { throw TerminalRenderError.malformed }
        guard rows <= limits.maximumRows, columns <= limits.maximumColumns,
              count <= limits.maximumCells, textCount <= limits.maximumTextBytes,
              linkCount <= limits.maximumLinks else { throw TerminalRenderError.capacity }
        guard rows * columns <= limits.maximumCells else { throw TerminalRenderError.capacity }
        // Limits above make all following products and additions safe on both
        // supported 64-bit platforms; no integer read is used as an unchecked slice.
        let cellsOffset = 256 + rows * 24
        let linksOffset = cellsOffset + count * 48
        let textOffset = linksOffset + linkCount * 8
        let paletteOffset = textOffset + textCount
        guard read.u32(128) == UInt32(rows), read.u32(140) == 256,
              Int(read.u32(144)) == cellsOffset, Int(read.u32(148)) == textOffset,
              Int(read.u32(152)) == paletteOffset, read.u32(156) == 265,
              Int(read.u32(164)) == linksOffset, paletteOffset + 265 * 4 == bytes.count,
              read.u64(64) > 0, read.u64(72) > 0, read.u64(80) > 0, read.u64(88) > 0,
              read.u32(124) <= 6, read.u32(168) <= 1, (1...2).contains(read.u32(172)),
              Int(read.u32(120)) < columns,
              Int(read.u32(172)) <= columns - Int(read.u32(120)) else { throw TerminalRenderError.malformed }
        let first = read.i64(104)
        guard first <= Int64.max - Int64(rows - 1) else { throw TerminalRenderError.malformed }
        let pool = Array(bytes[textOffset..<paletteOffset])
        guard String(bytes: pool, encoding: .utf8) != nil else { throw TerminalRenderError.malformed }
        var lines = [TerminalRenderRow](); lines.reserveCapacity(rows)
        var expectedCell = 0, expectedText = 0
        for row in 0..<rows {
            let at = 256 + row * 24
            let firstCell = Int(read.u32(at + 16)), rowCells = Int(read.u32(at + 20))
            guard read.i64(at) == first + Int64(row), read.u64(at + 8) <= read.u64(56), firstCell == expectedCell,
                  rowCells <= count - expectedCell else { throw TerminalRenderError.malformed }
            var cells = [TerminalRenderCell](); cells.reserveCapacity(rowCells)
            var nextColumn = 0
            for index in firstCell..<(firstCell + rowCells) {
                let pos = cellsOffset + index * 48
                let column = Int(read.u32(pos)), width = Int(read.u32(pos + 4))
                let start = Int(read.u32(pos + 8)), length = Int(read.u32(pos + 12))
                let flags = read.u32(pos + 16), link = Int(read.u32(pos + 44))
                guard length <= limits.maximumCellTextBytes else { throw TerminalRenderError.capacity }
                guard column >= nextColumn, (1...2).contains(width), column < columns,
                      width <= columns - column, start == expectedText, length > 0,
                      start <= pool.count, length <= pool.count - start, link <= linkCount,
                      flags & ~UInt32(0x1ffff) == 0, flags & 3 <= 2,
                      (flags >> 2) & 7 <= 5, (flags >> 5) & 3 <= 2,
                      (flags >> 12) & 3 <= 2, (flags >> 15) & 3 <= 2
                else { throw TerminalRenderError.malformed }
                guard flags & (1 << 14) == 0 else { throw TerminalRenderError.unsupported }
                guard let text = String(bytes: pool[start..<(start + length)], encoding: .utf8),
                      !text.unicodeScalars.contains(where: { $0.value < 32 || (127...159).contains($0.value) })
                else { throw TerminalRenderError.malformed }
                cells.append(TerminalRenderCell(column: column, width: width, text: text, flags: flags,
                    foreground: try read.color(pos + 20), background: try read.color(pos + 28),
                    underline: try read.color(pos + 36), linkID: link))
                expectedText += length; nextColumn = column + width
            }
            expectedCell += rowCells
            lines.append(TerminalRenderRow(stableIndex: first + Int64(row), sequence: read.u64(at + 8), cells: cells))
        }
        guard expectedCell == count else { throw TerminalRenderError.malformed }
        var links = [String](); links.reserveCapacity(linkCount)
        for index in 0..<linkCount {
            let pos = linksOffset + index * 8
            let start = Int(read.u32(pos)), length = Int(read.u32(pos + 4))
            guard length <= limits.maximumLinkBytes else { throw TerminalRenderError.capacity }
            guard start == expectedText, length > 0, start <= pool.count, length <= pool.count - start,
                  let link = String(bytes: pool[start..<(start + length)], encoding: .utf8),
                  !link.unicodeScalars.contains(where: { $0.value < 32 || (127...159).contains($0.value) })
            else { throw TerminalRenderError.malformed }
            links.append(link); expectedText += length
        }
        guard expectedText == pool.count else { throw TerminalRenderError.malformed }
        let palette = (0..<265).map { index in
            let p = paletteOffset + index * 4
            return TerminalRenderRGBA(bytes[p], bytes[p + 1], bytes[p + 2], bytes[p + 3])
        }
        return Self(source: expectedSource, sequence: read.u64(56), geometryRevision: read.u64(64),
            paletteRevision: read.u64(72), fontRevision: read.u64(80), revision: read.u64(88),
            rows: rows, columns: columns, viewportFirst: first, flags: read.u32(12),
            cursor: TerminalRenderCursor(stableRow: read.i64(112), column: Int(read.u32(120)),
                shape: read.u32(124), width: Int(read.u32(172)), visible: read.u32(168) == 1),
            lines: lines, palette: palette, links: links)
    }

    private static func uuidBytes(_ value: UUID) -> [UInt8] {
        let b = value.uuid
        return [b.0,b.1,b.2,b.3,b.4,b.5,b.6,b.7,b.8,b.9,b.10,b.11,b.12,b.13,b.14,b.15]
    }

    /// A full replacement still carries causal witnesses. Reusing a geometry
    /// or palette revision for changed data is not a coherent replacement.
    func validateSuccessor(of previous: TerminalRenderFrame) throws {
        guard source == previous.source else { throw TerminalRenderError.staleSource }
        guard revision > previous.revision, sequence >= previous.sequence,
              geometryRevision >= previous.geometryRevision, paletteRevision >= previous.paletteRevision,
              fontRevision >= previous.fontRevision else { throw TerminalRenderError.staleFrame }
        if geometryRevision == previous.geometryRevision {
            guard rows == previous.rows, columns == previous.columns,
                  alternateScreen == previous.alternateScreen else { throw TerminalRenderError.staleFrame }
        }
        if paletteRevision == previous.paletteRevision, palette != previous.palette {
            throw TerminalRenderError.staleFrame
        }
    }
}

private struct RenderBytes {
    let bytes: [UInt8]
    init(_ bytes: [UInt8]) { self.bytes = bytes }
    func u16(_ at: Int) -> UInt16 { UInt16(bytes[at]) | UInt16(bytes[at + 1]) << 8 }
    func u32(_ at: Int) -> UInt32 {
        UInt32(bytes[at]) | UInt32(bytes[at + 1]) << 8 | UInt32(bytes[at + 2]) << 16 | UInt32(bytes[at + 3]) << 24
    }
    func u64(_ at: Int) -> UInt64 { UInt64(u32(at)) | UInt64(u32(at + 4)) << 32 }
    func i64(_ at: Int) -> Int64 { Int64(bitPattern: u64(at)) }
    func color(_ at: Int) throws -> TerminalRenderColor {
        let value = u32(at + 4)
        switch u32(at) {
        case 0 where value == 0: return .default
        case 1 where value <= 255: return .palette(UInt8(value))
        case 2: return .rgba(TerminalRenderRGBA(bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]))
        default: throw TerminalRenderError.malformed
        }
    }
}

/// Monotonic ownership for the three native GPU slots. Exact leases, rather than
/// the maximum completed frame number, release each in-flight resource set.
public struct TerminalRenderSlots: ~Copyable, Sendable {
    public struct Lease: Hashable, Sendable {
        fileprivate let owner: UUID
        public let slot: Int
        public let serial: UInt64
    }
    private enum State: Sendable { case free, preparing(Lease), submitted(Lease) }
    private var slots: [State] = [.free, .free, .free]
    private var serial: UInt64 = 0
    private let owner = UUID()
    public init() {}
    public var inFlight: Int { slots.filter { if case .submitted = $0 { true } else { false } }.count }
    public mutating func acquire() throws -> Lease? {
        guard let index = slots.firstIndex(where: { if case .free = $0 { true } else { false } }) else { return nil }
        guard serial < UInt64.max else { throw TerminalRenderError.exhausted }
        serial += 1
        let lease = Lease(owner: owner, slot: index, serial: serial); slots[index] = .preparing(lease)
        return lease
    }
    public mutating func submit(_ lease: Lease) throws {
        guard slots.indices.contains(lease.slot), case let .preparing(current) = slots[lease.slot], current == lease
        else { throw TerminalRenderError.staleFrame }
        slots[lease.slot] = .submitted(lease)
    }
    public mutating func abandon(_ lease: Lease) throws {
        guard slots.indices.contains(lease.slot), case let .preparing(current) = slots[lease.slot], current == lease
        else { throw TerminalRenderError.staleFrame }
        slots[lease.slot] = .free
    }
    public mutating func complete(_ lease: Lease) throws {
        guard slots.indices.contains(lease.slot), case let .submitted(current) = slots[lease.slot], current == lease
        else { throw TerminalRenderError.staleFrame }
        slots[lease.slot] = .free
    }
}

/// Byte packing for the existing Rust/Metal layout. Swift struct ABI is never
/// exposed to the GPU. Reserved bytes are always zero, including the 64-byte tail.
enum TerminalRenderPacking {
    static let uniformBytes = 256, backgroundBytes = 4, textBytes = 24
    static func u16(_ value: UInt16, into bytes: inout [UInt8], at: Int) {
        bytes[at] = UInt8(truncatingIfNeeded: value); bytes[at + 1] = UInt8(truncatingIfNeeded: value >> 8)
    }
    static func u32(_ value: UInt32, into bytes: inout [UInt8], at: Int) {
        for offset in 0..<4 { bytes[at + offset] = UInt8(truncatingIfNeeded: value >> (offset * 8)) }
    }
    static func u64(_ value: UInt64, into bytes: inout [UInt8], at: Int) {
        for offset in 0..<8 { bytes[at + offset] = UInt8(truncatingIfNeeded: value >> (offset * 8)) }
    }
    static func floats(_ values: [Float], into bytes: inout [UInt8], at: Int) {
        for (offset, value) in values.enumerated() { u32(value.bitPattern, into: &bytes, at: at + offset * 4) }
    }
    static func uniforms(_ value: TerminalRenderUniforms) throws -> [UInt8] {
        guard value.viewport.0 > 0, value.viewport.1 > 0,
              value.viewport.0 <= 16384, value.viewport.1 <= 16384,
              (1...512).contains(value.rows), (1...512).contains(value.columns),
              value.rows * value.columns <= 65536, value.rowOffset < value.rows,
              value.cursorShape <= 4, value.cursorColumn < value.columns,
              value.cursorRow < value.rows, (1...2).contains(value.cursorWidth),
              value.cellSize.0 > 0, value.cellSize.1 > 0
        else { throw TerminalRenderError.capacity }
        let scalars = [value.cellSize.0,value.cellSize.1,value.origin.0,value.origin.1,
                       value.cursorThickness,value.underlinePosition,value.lineThickness,value.strikethroughPosition]
        guard scalars.allSatisfy({ $0.isFinite && abs($0) <= 32768 }) else { throw TerminalRenderError.malformed }
        var bytes = [UInt8](repeating: 0, count: uniformBytes)
        u64(value.frame, into: &bytes, at: 0)
        u32(value.viewport.0, into: &bytes, at: 8); u32(value.viewport.1, into: &bytes, at: 12)
        u32(value.rows, into: &bytes, at: 16); u32(value.columns, into: &bytes, at: 20)
        floats(value.clear.linearPremultiplied, into: &bytes, at: 32)
        floats([value.cellSize.0,value.cellSize.1], into: &bytes, at: 48)
        floats([value.origin.0,value.origin.1], into: &bytes, at: 56)
        u32(value.rowOffset, into: &bytes, at: 64); u32(value.cursorShape, into: &bytes, at: 68)
        u32(value.cursorColumn, into: &bytes, at: 72); u32(value.cursorRow, into: &bytes, at: 76)
        floats(value.cursorColor.linearPremultiplied, into: &bytes, at: 80)
        floats([value.cursorThickness], into: &bytes, at: 96)
        u32(value.cursorWidth, into: &bytes, at: 100)
        floats(value.selection.linearPremultiplied, into: &bytes, at: 112)
        floats(value.search.linearPremultiplied, into: &bytes, at: 128)
        floats(value.currentSearch.linearPremultiplied, into: &bytes, at: 144)
        floats([value.underlinePosition,value.lineThickness,value.strikethroughPosition], into: &bytes, at: 160)
        if let hsb = value.hsb {
            guard [hsb.0,hsb.1,hsb.2].allSatisfy({ $0.isFinite && (0...8).contains($0) })
            else { throw TerminalRenderError.malformed }
            floats([hsb.0,hsb.1,hsb.2,1], into: &bytes, at: 176)
        }
        return bytes
    }
    static func text(column: Int, row: Int, width: Int, foreground: TerminalRenderRGBA,
                     underline: TerminalRenderRGBA, flags: UInt8, atlas: (Int, Int, Int, Int) = (0,0,0,0),
                     offset: (Int, Int) = (0,0)) throws -> [UInt8] {
        guard (0...65535).contains(column), (0...65535).contains(row), (1...2).contains(width),
              [atlas.0,atlas.1,atlas.2,atlas.3].allSatisfy({ (0...16384).contains($0) }),
              (-32768...32767).contains(offset.0), (-32768...32767).contains(offset.1)
        else { throw TerminalRenderError.capacity }
        var bytes = [UInt8](repeating: 0, count: textBytes)
        for (at, value) in [(0,column),(2,row),(4,atlas.0),(6,atlas.1),(8,atlas.2),(10,atlas.3)] {
            u16(UInt16(value), into: &bytes, at: at)
        }
        u16(UInt16(bitPattern: Int16(offset.0)), into: &bytes, at: 12)
        u16(UInt16(bitPattern: Int16(offset.1)), into: &bytes, at: 14)
        bytes.replaceSubrange(16..<20, with: foreground.bytes)
        bytes.replaceSubrange(20..<23, with: underline.bytes.prefix(3))
        bytes[23] = flags | (width == 2 ? 4 : 0)
        return bytes
    }
}

struct TerminalRenderUniforms {
    var frame: UInt64 = 0
    var viewport: (UInt32, UInt32) = (1,1)
    var rows: UInt32 = 1, columns: UInt32 = 1
    var clear = TerminalRenderRGBA(0,0,0)
    var cellSize: (Float,Float) = (1,1), origin: (Float,Float) = (0,0)
    var rowOffset: UInt32 = 0, cursorShape: UInt32 = 0
    var cursorColumn: UInt32 = 0, cursorRow: UInt32 = 0, cursorWidth: UInt32 = 1
    var cursorColor = TerminalRenderRGBA(0,0,0,0)
    var cursorThickness: Float = 1
    var selection = TerminalRenderRGBA(0,0,0,0), search = TerminalRenderRGBA(0,0,0,0)
    var currentSearch = TerminalRenderRGBA(0,0,0,0)
    var underlinePosition: Float = 0, lineThickness: Float = 1, strikethroughPosition: Float = 0
    var hsb: (Float,Float,Float)?
}
