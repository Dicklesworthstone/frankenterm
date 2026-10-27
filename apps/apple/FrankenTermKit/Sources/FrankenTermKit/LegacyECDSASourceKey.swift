import Foundation

/// A clear, explicitly selected SEC1 or PKCS#8 ECDSA source file. DER is only
/// an input format: the existing FTAG private-record format remains a complete
/// clear OpenSSH container protected by the authenticated outer envelope.
/// Parsing establishes fixed bounds and named-curve/public-component bindings;
/// a maintained native DER importer and scalar multiplication validate the key.
struct LegacyECDSASourceKey: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    static let maximumDERBytes = 1024
    let curve: SSHECDSACurve
    private let der: Data
    private let scalar: Data
    private let embeddedPoint: Data?
    var description: String { "LegacyECDSASourceKey([redacted])" }
    var debugDescription: String { description }

    static func recognizes(_ pem: Data) -> Bool {
        guard pem.count <= FTLimit.key, let text = String(data: pem, encoding: .utf8) else { return false }
        let header = text.trimmingCharacters(in: .whitespacesAndNewlines).components(separatedBy: .newlines).first
        return ["-----BEGIN EC PRIVATE KEY-----", "-----BEGIN PRIVATE KEY-----",
                "-----BEGIN ENCRYPTED PRIVATE KEY-----"].contains(header ?? "")
    }

    init(pem: Data) throws {
        guard !pem.isEmpty, pem.count <= FTLimit.key, let text = String(data: pem, encoding: .utf8) else {
            throw FTError.limit("source_key.pem")
        }
        let lines = text.trimmingCharacters(in: .whitespacesAndNewlines)
            .components(separatedBy: .newlines).filter { !$0.isEmpty }
        guard lines.count >= 3 else { throw FTError.malformed("source_key.pem") }
        guard lines.first != "-----BEGIN ENCRYPTED PRIVATE KEY-----",
              !lines.contains(where: { $0.hasPrefix("Proc-Type:") || $0.hasPrefix("DEK-Info:") }) else {
            throw FTError.unsupported("source_key.encrypted_pem")
        }
        let pkcs8: Bool
        switch (lines.first, lines.last) {
        case ("-----BEGIN EC PRIVATE KEY-----"?, "-----END EC PRIVATE KEY-----"?): pkcs8 = false
        case ("-----BEGIN PRIVATE KEY-----"?, "-----END PRIVATE KEY-----"?): pkcs8 = true
        default: throw FTError.malformed("source_key.pem")
        }
        let encoded = lines.dropFirst().dropLast().joined()
        guard encoded.utf8.count <= ((Self.maximumDERBytes + 2) / 3) * 4,
              let der = Data(base64Encoded: encoded), !der.isEmpty,
              der.count <= Self.maximumDERBytes, der.base64EncodedString() == encoded else {
            throw FTError.malformed("source_key.der")
        }
        try self.init(der: der, pkcs8: pkcs8)
    }

    /// Decryption supplies bounded DER directly, avoiding an intermediate
    /// plaintext PEM/base64 string that the selected-file owner cannot erase.
    init(pkcs8DER: Data) throws { try self.init(der: pkcs8DER, pkcs8: true) }

    private init(der: Data, pkcs8: Bool) throws {
        guard !der.isEmpty, der.count <= Self.maximumDERBytes else { throw FTError.limit("source_key.der") }
        var document = LegacyECDSADERReader(der)
        var root = LegacyECDSADERReader(try document.read(tag: 0x30))
        try document.finish()
        let parsed: (curve: SSHECDSACurve, scalar: Data, point: Data?)
        if pkcs8 {
            guard try root.read(tag: 0x02) == Data([0]) else { throw FTError.unsupported("source_key.pkcs8_version") }
            var algorithm = LegacyECDSADERReader(try root.read(tag: 0x30))
            guard try algorithm.read(tag: 0x06) == Data([0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]) else {
                throw FTError.unsupported("source_key.family")
            }
            let namedCurve = try Self.namedCurve(algorithm.read(tag: 0x06))
            try algorithm.finish()
            var privateDocument = LegacyECDSADERReader(try root.read(tag: 0x04))
            var privateKey = LegacyECDSADERReader(try privateDocument.read(tag: 0x30))
            try privateDocument.finish()
            parsed = try Self.parseSEC1(&privateKey, inheritedCurve: namedCurve)
            // Attributes, extra keys and RFC 5958 version-one extensions are
            // outside this exact source profile, never silently discarded.
            try root.finish()
        } else {
            parsed = try Self.parseSEC1(&root, inheritedCurve: nil)
        }
        self.der = der; curve = parsed.curve; scalar = parsed.scalar; embeddedPoint = parsed.point
    }

    /// Passive discovery may reuse an embedded public point, as it does an
    /// OpenSSH public header. It never derives a private identity, unlocks a
    /// file, or substitutes a point when a sidecar is present but malformed.
    /// A file without public metadata needs its .pub file for passive discovery.
    var embeddedPublicBlob: Data? { embeddedPoint.map { Self.publicBlob($0, curve: curve) } }

    func privateRecord<C: AirGapCryptography>(credentialID: String, using crypto: C) throws -> PrivateKeyRecord {
        _ = try WorkspaceDocument.identifier(credentialID)
        try Task.checkCancellation()
        var imported = try crypto.importECDSAPrivateScalar(der, curve: curve)
        defer { imported.resetBytes(in: 0..<imported.count) }
        guard imported == scalar else { throw FTError.malformed("key.ecdsa_der_identity") }
        let point = try crypto.ecdsaPublicKey(scalar: imported, curve: curve)
        guard point.count == curve.publicByteCount, point.first == 4,
              embeddedPoint == nil || embeddedPoint == point else { throw FTError.malformed("key.identity") }
        try Task.checkCancellation()
        let publicBlob = Self.publicBlob(point, curve: curve)
        let check = try crypto.randomBytes(count: 4)
        guard check.count == 4 else { throw FTError.storage }
        var body = check + check
        defer { body.resetBytes(in: 0..<body.count) }
        for value in [Data(curve.algorithm.utf8), Data(curve.rawValue.utf8), point] {
            Self.appendString(value, to: &body)
        }
        var magnitude = Data(imported.drop(while: { $0 == 0 }))
        defer { magnitude.resetBytes(in: 0..<magnitude.count) }
        if magnitude.first.map({ $0 & 0x80 != 0 }) == true { magnitude.insert(0, at: magnitude.startIndex) }
        Self.appendString(magnitude, to: &body)
        Self.appendString(Data(), to: &body)
        var padding: UInt8 = 1
        while !body.count.isMultiple(of: 16) { body.append(padding); padding += 1 }
        let record = try PrivateKeyRecord.fromDecryptedOpenSSH(publicBlob: publicBlob, privatePart: body,
                                                              credentialID: credentialID, using: crypto)
        try Task.checkCancellation()
        return record
    }

    private static func parseSEC1(_ key: inout LegacyECDSADERReader, inheritedCurve: SSHECDSACurve?) throws
        -> (curve: SSHECDSACurve, scalar: Data, point: Data?) {
        guard try key.read(tag: 0x02) == Data([1]) else { throw FTError.unsupported("source_key.sec1_version") }
        let scalar = try key.read(tag: 0x04)
        var curve = inheritedCurve
        if key.nextTag == 0xa0 {
            var parameters = LegacyECDSADERReader(try key.read(tag: 0xa0))
            let innerCurve = try namedCurve(parameters.read(tag: 0x06))
            try parameters.finish()
            guard curve == nil || curve == innerCurve else { throw FTError.malformed("source_key.curve_mismatch") }
            curve = innerCurve
        }
        guard let curve else { throw FTError.unsupported("source_key.named_curve") }
        try curve.validateScalar(scalar)
        var point: Data?
        if key.nextTag == 0xa1 {
            var publicKey = LegacyECDSADERReader(try key.read(tag: 0xa1))
            let bits = try publicKey.read(tag: 0x03)
            try publicKey.finish()
            guard bits.count == curve.publicByteCount + 1, bits.first == 0,
                  bits[bits.startIndex + 1] == 4 else { throw FTError.malformed("source_key.ecdsa_public") }
            point = Data(bits.dropFirst())
        }
        try key.finish()
        return (curve, scalar, point)
    }

    private static func namedCurve(_ oid: Data) throws -> SSHECDSACurve {
        switch oid {
        case Data([0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]): return .nistp256
        case Data([0x2b, 0x81, 0x04, 0x00, 0x22]): return .nistp384
        case Data([0x2b, 0x81, 0x04, 0x00, 0x23]): return .nistp521
        default: throw FTError.unsupported("source_key.named_curve")
        }
    }

    private static func publicBlob(_ point: Data, curve: SSHECDSACurve) -> Data {
        var result = Data()
        for value in [Data(curve.algorithm.utf8), Data(curve.rawValue.utf8), point] { appendString(value, to: &result) }
        return result
    }
    private static func appendString(_ value: Data, to result: inout Data) {
        result.appendBE(UInt64(value.count), bytes: 4); result += value
    }
}

/// The source profile admits only a few fixed, shallow DER structures. Check
/// definite/minimal lengths before creating a subvalue or entering native crypto.
private struct LegacyECDSADERReader {
    private var cursor: ByteCursor
    init(_ data: Data) { cursor = ByteCursor(data) }
    var nextTag: UInt8? { cursor.remaining == 0 ? nil : cursor.data[cursor.offset] }
    mutating func read(tag: UInt8) throws -> Data {
        guard try cursor.uint(1) == UInt64(tag) else { throw FTError.malformed("source_key.der_tag") }
        let initial = try cursor.uint(1)
        let length: UInt64
        if initial < 128 { length = initial }
        else {
            let width = Int(initial & 0x7f)
            guard (1...2).contains(width) else { throw FTError.malformed("source_key.der_length") }
            length = try cursor.uint(width)
            guard length >= (width == 1 ? 128 : 256) else { throw FTError.malformed("source_key.der_length") }
        }
        guard length <= UInt64(LegacyECDSASourceKey.maximumDERBytes), length <= UInt64(cursor.remaining) else {
            throw FTError.malformed("source_key.der_length")
        }
        return try cursor.take(Int(length))
    }
    func finish() throws {
        guard cursor.remaining == 0 else { throw FTError.malformed("source_key.der_extent") }
    }
}
