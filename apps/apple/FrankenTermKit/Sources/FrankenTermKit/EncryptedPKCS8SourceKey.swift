import Foundation

/// Exact PBKDF2 profiles accepted only when reading an existing selected key.
/// SHA-1 remains the RFC 8018 default for older source files; none of these
/// choices changes FTAG encryption or creates new password-protected keys.
public enum PKCS8PBKDF2PRF: String, CaseIterable, Sendable {
    case hmacSHA1, hmacSHA256, hmacSHA384, hmacSHA512
}

/// Public protection metadata for one selected source file. Encrypted PKCS#8
/// and traditional PEM hide their public identity too, so the prompt names the expected
/// workspace identity, which is checked only after successful decryption.
public enum SourceKeyProtection: Equatable, Sendable {
    case openSSH(bcryptRounds: UInt32)
    case encryptedPKCS8(prf: PKCS8PBKDF2PRF, iterations: UInt32, aesKeyBits: Int)
    case legacyPEM(aesKeyBits: Int)

    public var hidesPublicIdentity: Bool {
        switch self {
        case .openSSH: false
        case .encryptedPKCS8, .legacyPEM: true
        }
    }
}

/// RFC 5208 EncryptedPrivateKeyInfo with one deliberately bounded RFC 8018
/// PBES2 profile: PBKDF2, a specified salt, and AES-CBC with PKCS#7 padding.
/// Strict parsing admits every algorithm and resource bound before a prompt or
/// KDF. Ciphertext remains opaque here; only validated RSA/ECDSA source records
/// can leave unlock, and the owner must match their identity to its reference.
struct EncryptedPKCS8SourceKey: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    static let maximumDERBytes = 16 * 1024
    static let maximumIterations: UInt32 = 1_000_000
    let iterations: UInt32
    let prf: PKCS8PBKDF2PRF
    let keyBytes: Int
    private let salt: Data, iv: Data, ciphertext: Data
    var description: String { "EncryptedPKCS8SourceKey([redacted])" }
    var debugDescription: String { description }
    var protection: SourceKeyProtection {
        .encryptedPKCS8(prf: prf, iterations: iterations, aesKeyBits: keyBytes * 8)
    }

    static func recognizes(_ pem: Data) -> Bool {
        guard pem.count <= FTLimit.key, let text = String(data: pem, encoding: .utf8) else { return false }
        return text.trimmingCharacters(in: .whitespacesAndNewlines)
            .components(separatedBy: .newlines).first == "-----BEGIN ENCRYPTED PRIVATE KEY-----"
    }

    init(pem: Data) throws {
        guard !pem.isEmpty, pem.count <= FTLimit.key, let text = String(data: pem, encoding: .utf8) else {
            throw FTError.limit("source_key.pem")
        }
        let lines = text.trimmingCharacters(in: .whitespacesAndNewlines)
            .components(separatedBy: .newlines).filter { !$0.isEmpty }
        guard lines.count >= 3, lines.first == "-----BEGIN ENCRYPTED PRIVATE KEY-----",
              lines.last == "-----END ENCRYPTED PRIVATE KEY-----" else { throw FTError.malformed("source_key.pem") }
        let encoded = lines.dropFirst().dropLast().joined()
        guard encoded.utf8.count <= ((Self.maximumDERBytes + 2) / 3) * 4 else {
            throw FTError.limit("source_key.pkcs8_der")
        }
        guard let der = Data(base64Encoded: encoded), !der.isEmpty,
              der.count <= Self.maximumDERBytes, der.base64EncodedString() == encoded else {
            throw FTError.malformed("source_key.der")
        }
        var document = PKCS8DERReader(der)
        var root = PKCS8DERReader(try document.read(tag: 0x30))
        try document.finish()
        var encryption = PKCS8DERReader(try root.read(tag: 0x30))
        guard try encryption.read(tag: 0x06) == Data([0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x05, 0x0d]) else {
            throw FTError.unsupported("source_key.pkcs8_encryption")
        }
        var parameters = PKCS8DERReader(try encryption.read(tag: 0x30))
        try encryption.finish()
        var kdf = PKCS8DERReader(try parameters.read(tag: 0x30))
        guard try kdf.read(tag: 0x06) == Data([0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x05, 0x0c]) else {
            throw FTError.unsupported("source_key.pkcs8_kdf")
        }
        var derivation = PKCS8DERReader(try kdf.read(tag: 0x30))
        try kdf.finish()
        // An algorithm-supplied salt is not a local fixed-size salt and must
        // never trigger a provider lookup or inferred default.
        guard derivation.nextTag == 0x04 else { throw FTError.unsupported("source_key.pkcs8_kdf") }
        salt = try derivation.read(tag: 0x04)
        let iterations = try derivation.positiveInteger()
        guard (8...64).contains(salt.count), (1...UInt64(Self.maximumIterations)).contains(iterations) else {
            throw FTError.limit("source_key.pbkdf2_parameters")
        }
        self.iterations = UInt32(iterations)
        let declaredKeyBytes: UInt64?
        if derivation.nextTag == 0x02 { declaredKeyBytes = try derivation.positiveInteger() }
        else { declaredKeyBytes = nil }
        if derivation.nextTag == nil { prf = .hmacSHA1 }
        else {
            var function = PKCS8DERReader(try derivation.read(tag: 0x30))
            switch try function.read(tag: 0x06) {
            // DER omits a field equal to its DEFAULT. Older HMAC-SHA1
            // sources therefore take the omitted-field branch above.
            case Data([0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x07]):
                throw FTError.malformed("source_key.pkcs8_parameters")
            case Data([0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x09]): prf = .hmacSHA256
            case Data([0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x0a]): prf = .hmacSHA384
            case Data([0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x0b]): prf = .hmacSHA512
            default: throw FTError.unsupported("source_key.pkcs8_prf")
            }
            guard try function.read(tag: 0x05).isEmpty else { throw FTError.malformed("source_key.pkcs8_parameters") }
            try function.finish()
        }
        try derivation.finish()
        var cipher = PKCS8DERReader(try parameters.read(tag: 0x30))
        switch try cipher.read(tag: 0x06) {
        case Data([0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x02]): keyBytes = 16
        case Data([0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x16]): keyBytes = 24
        case Data([0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x2a]): keyBytes = 32
        default: throw FTError.unsupported("source_key.pkcs8_cipher")
        }
        guard declaredKeyBytes == nil || declaredKeyBytes == UInt64(keyBytes) else {
            throw FTError.malformed("source_key.pkcs8_parameters")
        }
        iv = try cipher.read(tag: 0x04)
        guard iv.count == 16 else { throw FTError.malformed("source_key.pkcs8_parameters") }
        try cipher.finish(); try parameters.finish()
        ciphertext = try root.read(tag: 0x04)
        guard !ciphertext.isEmpty, ciphertext.count.isMultiple(of: 16) else {
            throw FTError.malformed("source_key.pkcs8_parameters")
        }
        try root.finish()
    }

    /// CBC is not authenticated. Valid padding alone never admits a private
    /// record: the complete canonical key and native-derived identity must also
    /// validate, then the selected-file owner checks its approved fingerprint.
    func unlock<C: AirGapCryptography>(passphrase: Data, credentialID: String, using crypto: C) throws -> PrivateKeyRecord {
        _ = try WorkspaceDocument.identifier(credentialID)
        guard (1...OpenSSHSourceKey.maximumPassphraseBytes).contains(passphrase.count) else {
            throw FTError.limit("source_key.passphrase")
        }
        try Task.checkCancellation()
        var key = try crypto.pbkdf2(passphrase: passphrase, salt: salt, iterations: iterations,
                                    prf: prf, outputByteCount: keyBytes)
        defer { key.resetBytes(in: 0..<key.count) }
        try Task.checkCancellation()
        guard key.count == keyBytes else { throw FTError.malformed("source_key.kdf_output") }
        var clear = try crypto.decryptAESCBC(ciphertext, key: key, iv: iv)
        defer { clear.resetBytes(in: 0..<clear.count) }
        try Task.checkCancellation()
        do {
            guard clear.count == ciphertext.count, let finalByte = clear.last,
                  (1...16).contains(Int(finalByte)) else { throw FTError.malformed("source_key.pkcs8_padding") }
            let padding = Int(finalByte)
            guard clear.suffix(padding).allSatisfy({ $0 == finalByte }), clear.count > padding else {
                throw FTError.malformed("source_key.pkcs8_padding")
            }
            var der = Data(clear.dropLast(padding))
            defer { der.resetBytes(in: 0..<der.count) }
            var document = PKCS8DERReader(der)
            var root = PKCS8DERReader(try document.read(tag: 0x30))
            try document.finish()
            guard try root.read(tag: 0x02) == Data([0]) else { throw FTError.unsupported("source_key.pkcs8_version") }
            var algorithm = PKCS8DERReader(try root.read(tag: 0x30))
            let record: PrivateKeyRecord
            switch try algorithm.read(tag: 0x06) {
            case Data([0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01]):
                record = try LegacyRSASourceKey(pkcs8DER: der).privateRecord(credentialID: credentialID, using: crypto)
            case Data([0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]):
                record = try LegacyECDSASourceKey(pkcs8DER: der).privateRecord(credentialID: credentialID, using: crypto)
            default: throw FTError.unsupported("source_key.family")
            }
            try Task.checkCancellation()
            return record
        } catch is CancellationError { throw CancellationError() }
        catch { throw OpenSSHSourceKeyError.incorrectPassphraseOrDamagedKey }
    }
}

/// A few fixed, shallow DER sequences. Reject nonminimal/indefinite lengths,
/// negative/overlong integers and trailing fields before entering a primitive.
private struct PKCS8DERReader {
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
        guard length <= UInt64(EncryptedPKCS8SourceKey.maximumDERBytes), length <= UInt64(cursor.remaining) else {
            throw FTError.malformed("source_key.der_length")
        }
        return try cursor.take(Int(length))
    }
    mutating func positiveInteger() throws -> UInt64 {
        let bytes = try read(tag: 0x02)
        guard (1...5).contains(bytes.count), let first = bytes.first, first & 0x80 == 0,
              bytes.count == 1 || first != 0 || bytes[bytes.startIndex + 1] & 0x80 != 0 else {
            throw FTError.malformed("source_key.der_integer")
        }
        let value = bytes.reduce(UInt64(0)) { ($0 << 8) | UInt64($1) }
        guard value > 0 else { throw FTError.malformed("source_key.der_integer") }
        return value
    }
    func finish() throws {
        guard cursor.remaining == 0 else { throw FTError.malformed("source_key.der_extent") }
    }
}
