// UR consensus algorithm and Bytewords alphabet adapted from Blockchain Commons
// URKit / BCR-2024-001. Copyright © 2020 Blockchain Commons, LLC.
// SPDX-License-Identifier: BSD-2-Clause-Patent. See THIRD_PARTY_NOTICES.md.
// The bounded Gaussian decoder and FTQR1 framing are FrankenTerm integration.
import Foundation
import CZlib

enum URCRC {
    static func checksum(_ data: Data) -> UInt32 {
        UInt32(data.withUnsafeBytes {
            crc32(0, $0.bindMemory(to: UInt8.self).baseAddress, uInt(data.count))
        })
    }
}

enum Bytewords {
    private static let words = """
    able acid also apex aqua arch atom aunt away axis back bald barn belt beta bias
    blue body brag brew bulb buzz calm cash cats chef city claw code cola cook cost
    crux curl cusp cyan dark data days deli dice diet door down draw drop drum dull
    duty each easy echo edge epic even exam exit eyes fact fair fern figs film fish
    fizz flap flew flux foxy free frog fuel fund gala game gear gems gift girl glow
    good gray grim guru gush gyro half hang hard hawk heat help high hill holy hope
    horn huts iced idea idle inch inky into iris iron item jade jazz join jolt jowl
    judo jugs jump junk jury keep keno kept keys kick kiln king kite kiwi knob lamb
    lava lazy leaf legs liar limp lion list logo loud love luau luck lung main many
    math maze memo menu meow mild mint miss monk nail navy need news next noon note
    numb obey oboe omit onyx open oval owls paid part peck play plus poem pool pose
    puff puma purr quad quiz race ramp real redo rich road rock roof ruby ruin runs
    rust safe saga scar sets silk skew slot soap solo song stub surf swan taco task
    taxi tent tied time tiny toil tomb toys trip tuna twin ugly undo unit urge user
    vast very veto vial vibe view visa void vows wall wand warm wasp wave waxy webs
    what when whiz wolf work yank yawn yell yoga yurt zaps zero zest zinc zone zoom
    """.split(whereSeparator: { $0.isWhitespace }).map { Array($0.uppercased().utf8) }
    private static let decodeTable: [UInt16: UInt8] = Dictionary(uniqueKeysWithValues:
        words.enumerated().map { (UInt16($0.element[0]) << 8 | UInt16($0.element[3]), UInt8($0.offset)) })
    static func encode(_ data: Data) -> String {
        var checked = data
        checked.appendBE(UInt64(URCRC.checksum(data)), bytes: 4)
        var result: [UInt8] = []
        result.reserveCapacity(checked.count * 2)
        for byte in checked { let word = words[Int(byte)]; result += [word[0], word[3]] }
        return String(decoding: result, as: UTF8.self)
    }
    static func decode(_ text: Substring) throws -> Data {
        guard text.utf8.count <= FTLimit.qrText, text.utf8.count >= 10, text.utf8.count.isMultiple(of: 2) else {
            throw FTError.malformed("bytewords.extent")
        }
        let bytes = Array(text.utf8)
        var result = Data()
        result.reserveCapacity(bytes.count / 2)
        for i in stride(from: 0, to: bytes.count, by: 2) {
            guard let byte = decodeTable[UInt16(bytes[i]) << 8 | UInt16(bytes[i + 1])] else { throw FTError.malformed("bytewords.character") }
            result.append(byte)
        }
        let payload = Data(result.dropLast(4))
        var reader = ByteCursor(Data(result.suffix(4)))
        guard try reader.uint(4) == URCRC.checksum(payload) else { throw FTError.checksum }
        return payload
    }
}

/// This deterministic PRNG is for public fountain indexes, NEVER for secrets.
struct URRandom {
    var state: [UInt64]
    init<C: AirGapCryptography>(seed: Data, using crypto: C) {
        let digest = crypto.sha256(seed)
        state = stride(from: 0, to: 32, by: 8).map { start in
            digest[start..<start + 8].reduce(UInt64(0)) { ($0 << 8) | UInt64($1) }
        }
    }
    mutating func next() -> UInt64 {
        func rotate(_ value: UInt64, _ count: Int) -> UInt64 { (value << count) | (value >> (64 - count)) }
        let result = rotate(state[1] &* 5, 7) &* 9, t = state[1] << 17
        state[2] ^= state[0]; state[3] ^= state[1]; state[1] ^= state[2]; state[0] ^= state[3]
        state[2] ^= t; state[3] = rotate(state[3], 45)
        return result
    }
    mutating func fraction() -> Double { Double(next()) / (Double(UInt64.max) + 1) }
    mutating func index(count: Int) throws -> Int {
        let result = Int(fraction() * Double(count))
        // Preserve consensus arithmetic, but reject its rare rounded-to-one edge.
        guard result >= 0, result < count else { throw FTError.malformed("ur.random_endpoint") }
        return result
    }
}

struct URDegree {
    let probability: [Double], alias: [Int]
    init(count: Int) {
        let weights = (1...count).map { 1.0 / Double($0) }, total = weights.reduce(0, +)
        var scaled = weights.map { $0 * Double(count) / total }, small: [Int] = [], large: [Int] = []
        var probability = Array(repeating: 0.0, count: count), alias = Array(repeating: 0, count: count)
        for i in (0..<count).reversed() { if scaled[i] < 1 { small.append(i) } else { large.append(i) } }
        while !small.isEmpty && !large.isEmpty {
            let low = small.removeLast(), high = large.removeLast()
            probability[low] = scaled[low]; alias[low] = high
            scaled[high] += scaled[low] - 1
            if scaled[high] < 1 { small.append(high) } else { large.append(high) }
        }
        for i in small + large { probability[i] = 1 }
        self.probability = probability; self.alias = alias
    }
    func next(using random: inout URRandom) throws -> Int {
        let index = try random.index(count: probability.count), coin = random.fraction()
        return (coin < probability[index] ? index : alias[index]) + 1
    }
    func indexes<C: AirGapCryptography>(sequence: UInt32, checksum: UInt32, using crypto: C) throws -> [Int] {
        guard sequence > 0 else { throw FTError.malformed("ur.sequence") }
        if sequence <= probability.count { return [Int(sequence) - 1] }
        var seed = Data(); seed.appendBE(UInt64(sequence), bytes: 4); seed.appendBE(UInt64(checksum), bytes: 4)
        var random = URRandom(seed: seed, using: crypto), remaining = Array(probability.indices)
        let degree = try next(using: &random)
        var result: [Int] = []
        for _ in 0..<degree { result.append(remaining.remove(at: try random.index(count: remaining.count))) }
        return result
    }
}

struct URPart: Sendable {
    let sequence: UInt32, count: Int, messageLength: Int, checksum: UInt32, payload: Data
    func encoded() -> Data {
        CBORWriter.array(5) + CBORWriter.uint(UInt64(sequence)) + CBORWriter.uint(UInt64(count))
            + CBORWriter.uint(UInt64(messageLength)) + CBORWriter.uint(UInt64(checksum)) + CBORWriter.bytes(payload)
    }
    init(sequence: UInt32, count: Int, messageLength: Int, checksum: UInt32, payload: Data) {
        self.sequence = sequence; self.count = count; self.messageLength = messageLength; self.checksum = checksum; self.payload = payload
    }
    init(cbor: Data) throws {
        var reader = CBORReader(cbor)
        try reader.tuple(5)
        let sequence = try reader.head(major: 0), count = try reader.head(major: 0)
        let messageLength = try reader.head(major: 0), checksum = try reader.head(major: 0)
        guard sequence > 0, sequence <= UInt32.max, count >= 2, count <= 129,
              messageLength > 256, messageLength <= FTLimit.page + 3, checksum <= UInt32.max else {
            throw FTError.malformed("ur.part_bounds")
        }
        payload = try reader.bytes(cap: 256)
        try reader.finish()
        let nominal = (Int(messageLength) + Int(count) - 1) / Int(count)
        guard count == (messageLength + 255) / 256, payload.count == nominal else { throw FTError.malformed("ur.fragment_geometry") }
        self.sequence = UInt32(sequence); self.count = Int(count); self.messageLength = Int(messageLength); self.checksum = UInt32(checksum)
    }
}

struct URPageEncoder: Sendable {
    let message: Data, fragments: [Data], checksum: UInt32
    init(page: Data) {
        let message = CBORWriter.bytes(page)
        self.message = message
        checksum = URCRC.checksum(message)
        let count = (message.count + 255) / 256, length = (message.count + count - 1) / count
        fragments = stride(from: 0, to: message.count, by: length).map {
            let fragment = message.subdata(in: $0..<min($0 + length, message.count))
            return fragment + Data(repeating: 0, count: length - fragment.count)
        }
    }
    func text<C: AirGapCryptography>(sequence: UInt32, using crypto: C) throws -> String {
        guard sequence > 0 else { throw FTError.malformed("ur.sequence") }
        if fragments.count == 1 { return "UR:BYTES/" + Bytewords.encode(message) }
        var mixed = Data(repeating: 0, count: fragments[0].count)
        for index in try URDegree(count: fragments.count).indexes(sequence: sequence, checksum: checksum, using: crypto) {
            for i in mixed.indices { mixed[i] ^= fragments[index][i] }
        }
        let part = URPart(sequence: sequence, count: fragments.count, messageLength: message.count, checksum: checksum, payload: mixed)
        return "UR:BYTES/\(sequence)-\(fragments.count)/" + Bytewords.encode(part.encoded())
    }
}

/// At most N pivot rows of N bits + 256 bytes. No per-sequence history grows.
/// Echelon reduction is equivalent to the wire's XOR equations, independent of
/// which valid encoder produced them. A call performs at most 129 row reductions.
struct URPageDecoder: Sendable {
    struct Row: Sendable { var bits: [UInt64]; var payload: Data }
    let count: Int, messageLength: Int, checksum: UInt32, fragmentLength: Int
    private var rows: [Row?]
    private(set) var rank = 0
    init(part: URPart) {
        count = part.count; messageLength = part.messageLength; checksum = part.checksum; fragmentLength = part.payload.count
        rows = Array(repeating: nil, count: part.count)
    }
    mutating func receive<C: AirGapCryptography>(_ part: URPart, using crypto: C) throws -> Data? {
        guard part.count == count, part.messageLength == messageLength, part.checksum == checksum,
              part.payload.count == fragmentLength else { throw FTError.malformed("ur.changed_geometry") }
        var row = Row(bits: Array(repeating: 0, count: (count + 63) / 64), payload: part.payload)
        for index in try URDegree(count: count).indexes(sequence: part.sequence, checksum: checksum, using: crypto) {
            row.bits[index / 64] |= UInt64(1) << (index % 64)
        }
        func xor(_ source: Row, into target: inout Row) {
            for i in target.bits.indices { target.bits[i] ^= source.bits[i] }
            for i in target.payload.indices { target.payload[i] ^= source.payload[i] }
        }
        var inserted = false
        for pivot in 0..<count where row.bits[pivot / 64] & (UInt64(1) << (pivot % 64)) != 0 {
            if let existing = rows[pivot] { xor(existing, into: &row) }
            else { rows[pivot] = row; rank += 1; inserted = true; break }
        }
        if !inserted && row.payload.contains(where: { $0 != 0 }) { throw FTError.checksum }
        guard rank == count else { return nil }
        var solved = Array(repeating: Data(), count: count)
        for pivot in (0..<count).reversed() {
            guard var row = rows[pivot] else { throw FTError.malformed("ur.missing_pivot") }
            for next in (pivot + 1)..<count where row.bits[next / 64] & (UInt64(1) << (next % 64)) != 0 {
                for i in row.payload.indices { row.payload[i] ^= solved[next][i] }
            }
            solved[pivot] = row.payload
        }
        let padded = solved.reduce(into: Data()) { $0 += $1 }
        guard padded.dropFirst(messageLength).allSatisfy({ $0 == 0 }) else { throw FTError.malformed("ur.padding") }
        let message = Data(padded.prefix(messageLength))
        guard URCRC.checksum(message) == checksum else { throw FTError.checksum }
        return message
    }
}

public struct OpticalFrame: Sendable {
    public struct Identity: Equatable, Sendable {
        public let transferID: Data, envelopeDigest: Data
        public let envelopeLength: Int
        public var pageCount: Int { (envelopeLength + FTLimit.page - 1) / FTLimit.page }
    }
    public let identity: Identity, pageIndex: Int, pageDigest: Data
    let ur: String
    public var expectedPageLength: Int { min(FTLimit.page, identity.envelopeLength - pageIndex * FTLimit.page) }
    public init(text: String) throws {
        guard text.utf8.count <= FTLimit.qrText, text.utf8.allSatisfy({ $0 < 128 }) else { throw FTError.limit("qr.text") }
        let fields = text.split(separator: "/", maxSplits: 6, omittingEmptySubsequences: false)
        guard fields.count == 7, fields[0] == "FTQR1" else { throw FTError.malformed("qr.wrapper") }
        func hex(_ field: Substring, length: Int) throws -> Data {
            guard field.count == length, field.utf8.allSatisfy({ (48...57).contains($0) || (65...70).contains($0) }) else { throw FTError.malformed("qr.hex") }
            return try Data(ftHex: String(field))
        }
        let transferID = try hex(fields[1], length: 32), length = try Self.decimal(fields[2], maximum: FTLimit.envelope)
        guard length >= 4170 else { throw FTError.malformed("qr.envelope_length") }
        let pages = fields[3].split(separator: "-", omittingEmptySubsequences: false)
        guard pages.count == 2 else { throw FTError.malformed("qr.page") }
        let index = try Self.decimal(pages[0], maximum: 127), count = try Self.decimal(pages[1], maximum: 128)
        guard count == (length + FTLimit.page - 1) / FTLimit.page, index < count else { throw FTError.malformed("qr.page_extent") }
        identity = Identity(transferID: transferID, envelopeDigest: try hex(fields[4], length: 64), envelopeLength: length)
        pageIndex = index; pageDigest = try hex(fields[5], length: 64)
        guard fields[6].hasPrefix("UR:BYTES/"), fields[6].utf8.allSatisfy({ !(97...122).contains($0) }) else { throw FTError.malformed("qr.ur") }
        ur = String(fields[6])
    }
    static func decimal(_ value: Substring, maximum: Int) throws -> Int {
        guard !value.isEmpty, value.utf8.count <= 10, (value == "0" || value.first != "0"),
              value.utf8.allSatisfy({ (48...57).contains($0) }), let result = Int(value), result <= maximum else { throw FTError.malformed("qr.integer") }
        return result
    }
}

public struct OpticalSender: Sendable {
    public let identity: OpticalFrame.Identity
    private let pages: [URPageEncoder], pageDigests: [Data]
    private var sequences: [UInt32]
    private var emitted = 0
    public init<C: AirGapCryptography>(envelope: Data, using crypto: C) throws {
        let header = try FTAGHeader(envelope: envelope)
        identity = OpticalFrame.Identity(transferID: header.transferID, envelopeDigest: crypto.sha256(envelope), envelopeLength: envelope.count)
        let chunks = stride(from: 0, to: envelope.count, by: FTLimit.page).map { envelope.subdata(in: $0..<min($0 + FTLimit.page, envelope.count)) }
        pages = chunks.map(URPageEncoder.init); pageDigests = chunks.map(crypto.sha256)
        sequences = Array(repeating: 0, count: chunks.count)
    }
    public func sourcePartCount(page: Int) throws -> Int {
        guard pages.indices.contains(page) else { throw FTError.malformed("qr.page") }
        return pages[page].fragments.count
    }
    public func frame<C: AirGapCryptography>(page: Int, sequence: UInt32, using crypto: C) throws -> String {
        guard pages.indices.contains(page) else { throw FTError.malformed("qr.page") }
        let prefix = "FTQR1/\(identity.transferID.ftHex)/\(identity.envelopeLength)/\(page)-\(pages.count)/\(identity.envelopeDigest.ftHex)/\(pageDigests[page].ftHex)/"
        let text = try prefix + pages[page].text(sequence: sequence, using: crypto)
        guard text.utf8.count <= FTLimit.qrText else { throw FTError.limit("qr.text") }
        return text
    }
    public mutating func next<C: AirGapCryptography>(repairPage: Int? = nil, using crypto: C) throws -> String {
        let page = repairPage ?? ((emitted / 8) % pages.count)
        guard pages.indices.contains(page), sequences[page] < UInt32.max, emitted < Int.max else { throw FTError.limit("qr.sequence") }
        sequences[page] += 1; emitted += 1
        return try frame(page: page, sequence: sequences[page], using: crypto)
    }
}

/// Caller explicitly selects one identity before construction. No receive call
/// can switch sources. Value ownership permits an actor to serialize mutations.
public struct OpticalReceiver: Sendable {
    public let identity: OpticalFrame.Identity
    private var decoders: [Int: URPageDecoder] = [:]
    private var expectedDigests: [Int: Data] = [:]
    private var verified: [Int: Data] = [:]
    public var verifiedBytes: Int { verified.values.reduce(0) { $0 + $1.count } }
    public var verifiedPages: Int { verified.count }
    public var isComplete: Bool { verified.count == identity.pageCount }
    public var independentParts: Int { decoders.values.reduce(0) { $0 + $1.rank } }
    public init(selecting frame: OpticalFrame) { identity = frame.identity }
    public mutating func resetIncompletePage(_ page: Int) {
        guard verified[page] == nil else { return }
        decoders[page] = nil; expectedDigests[page] = nil
    }
    public mutating func receive<C: AirGapCryptography>(_ frame: OpticalFrame, using crypto: C) throws {
        guard frame.identity == identity else { throw FTError.stale }
        if let previous = expectedDigests[frame.pageIndex], previous != frame.pageDigest { throw FTError.malformed("qr.changed_page_digest") }
        if verified[frame.pageIndex] != nil { return }
        let parts = frame.ur.split(separator: "/", omittingEmptySubsequences: false)
        let message: Data?
        if parts.count == 2 {
            let single = try Bytewords.decode(parts[1])
            guard single.count <= 256 else { throw FTError.malformed("ur.single_extent") }
            message = single
        } else if parts.count == 3 {
            let part = try URPart(cbor: Bytewords.decode(parts[2]))
            let sequence = parts[1].split(separator: "-", omittingEmptySubsequences: false)
            guard sequence.count == 2, try OpticalFrame.decimal(sequence[0], maximum: Int(UInt32.max)) == part.sequence,
                  try OpticalFrame.decimal(sequence[1], maximum: 129) == part.count,
                  part.messageLength == frame.expectedPageLength + CBORWriter.head(2, UInt64(frame.expectedPageLength)).count else { throw FTError.malformed("ur.sequence_or_length") }
            var decoder = decoders[frame.pageIndex] ?? URPageDecoder(part: part)
            do { message = try decoder.receive(part, using: crypto) }
            catch { resetIncompletePage(frame.pageIndex); throw error }
            decoders[frame.pageIndex] = decoder
        } else { throw FTError.malformed("ur.path") }
        expectedDigests[frame.pageIndex] = frame.pageDigest
        guard let message else { return }
        do {
            var reader = CBORReader(message)
            let page = try reader.bytes(cap: FTLimit.page)
            try reader.finish()
            guard page.count == frame.expectedPageLength, crypto.sha256(page) == frame.pageDigest else { throw FTError.checksum }
            verified[frame.pageIndex] = page; decoders[frame.pageIndex] = nil
        } catch { resetIncompletePage(frame.pageIndex); throw error }
    }
    public func envelope<C: AirGapCryptography>(using crypto: C) throws -> Data {
        guard isComplete else { throw FTError.malformed("qr.incomplete") }
        var result = Data(); result.reserveCapacity(identity.envelopeLength)
        for index in 0..<identity.pageCount {
            guard let page = verified[index] else { throw FTError.malformed("qr.missing_page") }
            result += page
        }
        guard result.count == identity.envelopeLength, crypto.sha256(result) == identity.envelopeDigest,
              try FTAGHeader(envelope: result).transferID == identity.transferID else { throw FTError.checksum }
        return result
    }
}
