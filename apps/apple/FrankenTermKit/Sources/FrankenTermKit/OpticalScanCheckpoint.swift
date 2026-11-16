import Foundation

/// A restart cache, not an authenticated export or an import approval. The only
/// payload it can retain is reconstructed FTAG ciphertext. Partial fountain
/// equations, camera images, transfer codes and recipient private keys are absent.
/// All hashes are corruption checks; the normal envelope authentication and
/// workspace review remain mandatory after resuming.
public struct OpticalScanCheckpoint: Sendable {
    public static let maximumBytes = FTLimit.envelope + 128 * 40 + 96
    public let identity: OpticalFrame.Identity
    public var verifiedPages: Int { pages.count }
    public var verifiedBytes: Int { pages.values.reduce(0) { $0 + $1.count } }
    public var incompletePageIndices: [Int] { (0..<identity.pageCount).filter { pages[$0] == nil } }
    let pages: [Int: Data]
    let pageDigests: [Int: Data]

    init<C: AirGapCryptography>(identity: OpticalFrame.Identity, pages: [Int: Data],
                                pageDigests: [Int: Data], using crypto: C) throws {
        try Self.validateIdentity(identity)
        guard !pages.isEmpty, pages.count <= identity.pageCount,
              Set(pages.keys) == Set(pageDigests.keys) else { throw FTError.malformed("scan.pages") }
        for (index, bytes) in pages {
            guard (0..<identity.pageCount).contains(index),
                  bytes.count == min(FTLimit.page, identity.envelopeLength - index * FTLimit.page),
                  pageDigests[index]?.count == 32 else { throw FTError.malformed("scan.page_extent") }
            guard crypto.sha256(bytes) == pageDigests[index] else { throw FTError.checksum }
        }
        self.identity = identity; self.pages = pages; self.pageDigests = pageDigests
    }

    private static func validateIdentity(_ identity: OpticalFrame.Identity) throws {
        guard identity.transferID.count == 16, identity.envelopeDigest.count == 32,
              (4170...FTLimit.envelope).contains(identity.envelopeLength) else {
            throw FTError.malformed("scan.identity")
        }
    }

    /// Fixed big-endian fields, sorted page records, no extensible object graph:
    /// FTSC, version/u16, reserved/u16, length/u32, transfer/16, digest/32,
    /// page-count/u16, reserved/u16; then index/u16, reserved/u16, length/u32,
    /// digest/32 and exact ciphertext bytes per page. A final SHA-256 covers all
    /// preceding bytes with a domain separator. This checksum is NOT a MAC.
    public func encoded<C: AirGapCryptography>(using crypto: C) throws -> Data {
        var result = Data("FTSC".utf8)
        result.appendBE(1, bytes: 2); result.appendBE(0, bytes: 2)
        result.appendBE(UInt64(identity.envelopeLength), bytes: 4)
        result += identity.transferID; result += identity.envelopeDigest
        result.appendBE(UInt64(pages.count), bytes: 2); result.appendBE(0, bytes: 2)
        for index in pages.keys.sorted() {
            guard let bytes = pages[index], let digest = pageDigests[index] else {
                throw FTError.malformed("scan.missing_page")
            }
            result.appendBE(UInt64(index), bytes: 2); result.appendBE(0, bytes: 2)
            result.appendBE(UInt64(bytes.count), bytes: 4); result += digest; result += bytes
        }
        guard result.count <= Self.maximumBytes - 32 else { throw FTError.limit("scan.bytes") }
        let digest = crypto.sha256(Self.domain + result)
        guard digest.count == 32 else { throw FTError.malformed("scan.digest") }
        result += digest
        return result
    }

    public init<C: AirGapCryptography>(data: Data, using crypto: C) throws {
        // Check BEFORE a cursor or a body copy. A file reader must enforce the
        // same limit before reading; this also protects non-file callers.
        guard data.count <= Self.maximumBytes else { throw FTError.limit("scan.bytes") }
        guard data.count >= 137 else { throw FTError.malformed("scan.truncated") }
        let body = Data(data.dropLast(32))
        guard crypto.sha256(Self.domain + body) == Data(data.suffix(32)) else { throw FTError.checksum }
        var reader = ByteCursor(body)
        guard try reader.take(4) == Data("FTSC".utf8),
              try reader.uint(2) == 1, try reader.uint(2) == 0 else { throw FTError.malformed("scan.version") }
        let length = try reader.uint(4)
        guard (4170...UInt64(FTLimit.envelope)).contains(length) else { throw FTError.malformed("scan.extent") }
        let identity = try OpticalFrame.Identity(transferID: reader.take(16), envelopeDigest: reader.take(32),
                                                 envelopeLength: Int(length))
        let count = try reader.uint(2)
        guard count > 0, count <= identity.pageCount, try reader.uint(2) == 0 else {
            throw FTError.malformed("scan.page_count")
        }
        var pages: [Int: Data] = [:], digests: [Int: Data] = [:], previous = -1
        for _ in 0..<Int(count) {
            let index = Int(try reader.uint(2))
            guard index > previous, index < identity.pageCount, try reader.uint(2) == 0 else {
                throw FTError.malformed("scan.page_order")
            }
            let pageLength = try reader.uint(4)
            guard pageLength == min(FTLimit.page, identity.envelopeLength - index * FTLimit.page) else {
                throw FTError.malformed("scan.page_extent")
            }
            let digest = try reader.take(32), page = try reader.take(Int(pageLength))
            guard crypto.sha256(page) == digest else { throw FTError.checksum }
            pages[index] = page; digests[index] = digest; previous = index
        }
        guard reader.remaining == 0 else { throw FTError.malformed("scan.trailing_bytes") }
        self.identity = identity; self.pages = pages; pageDigests = digests
    }

    private static var domain: Data { Data("FrankenTerm/FTSC/v1/checkpoint\0".utf8) }
}
