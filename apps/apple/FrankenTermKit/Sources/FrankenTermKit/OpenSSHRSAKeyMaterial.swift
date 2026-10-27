import Foundation
import CryptoExtras
#if canImport(CryptoKit)
import CryptoKit
#else
import Crypto
#endif

/// The admitted RSA profile is shared by explicit source import, archive
/// validation and the protected SSH signer. Big-integer arithmetic, key
/// consistency checks and signatures belong to maintained CryptoExtras; this
/// boundary only checks bounded, canonical encodings and exact identities.
struct OpenSSHRSAKeyMaterial: Sendable, CustomDebugStringConvertible {
    private let key: _RSA.Signing.PrivateKey
    let publicBlob: Data
    var debugDescription: String { "OpenSSHRSAKeyMaterial([redacted])" }

    /// OpenSSH private fields follow the family name in n,e,d,iqmp,p,q order.
    /// The caller continues with its comment, padding and container checks.
    init(reading reader: inout ByteCursor) throws {
        let n = try Self.readMPInt(&reader, maximumBytes: 1024)
        let e = try Self.readMPInt(&reader, maximumBytes: 8)
        var d = try Self.readMPInt(&reader, maximumBytes: 1024)
        defer { d.resetBytes(in: 0..<d.count) }
        var iqmp = try Self.readMPInt(&reader, maximumBytes: 1024)
        defer { iqmp.resetBytes(in: 0..<iqmp.count) }
        var p = try Self.readMPInt(&reader, maximumBytes: 1024)
        defer { p.resetBytes(in: 0..<p.count) }
        var q = try Self.readMPInt(&reader, maximumBytes: 1024)
        defer { q.resetBytes(in: 0..<q.count) }
        try self.init(n: n, e: e, d: d, iqmp: iqmp, p: p, q: q)
    }

    fileprivate init(n: Data, e: Data, d: Data, iqmp: Data, p: Data, q: Data,
                     dmp1: Data? = nil, dmq1: Data? = nil) throws {
        let blob = Self.publicBlob(n: n, e: e)
        // This also enforces odd modulus/exponent, e >= 3, the 64-bit exponent
        // cap and the complete 2048...8192-bit public profile before native work.
        _ = try SSHPublicIdentity(blob: blob)
        for value in [d, iqmp, p, q] {
            guard !value.isEmpty, value.count <= n.count, value.first != 0 else {
                throw FTError.malformed("key.rsa_component")
            }
        }
        guard p.last.map({ $0 & 1 == 1 }) == true,
              q.last.map({ $0 & 1 == 1 }) == true,
              p.count > 1 || p.first.map({ $0 > 1 }) == true,
              q.count > 1 || q.first.map({ $0 > 1 }) == true else {
            throw FTError.malformed("key.rsa_component")
        }
        try Task.checkCancellation()
        let imported: _RSA.Signing.PrivateKey
        do {
            // CryptoExtras computes CRT parameters, then BoringSSL's
            // RSA_new_private_key runs RSA_check_key for n,e,d,p,q and CRT.
            imported = try _RSA.Signing.PrivateKey(n: n, e: e, d: d, p: p, q: q)
        } catch { throw FTError.malformed("key.rsa_identity") }
        guard (2048...8192).contains(imported.keySizeInBits) else { throw FTError.unsupported("key.rsa_bits") }
        var canonicalDER = imported.derRepresentation
        defer { canonicalDER.resetBytes(in: 0..<canonicalDER.count) }
        var canonical = try RSAPrivateComponents(pkcs1DER: canonicalDER)
        defer { canonical.erasePrivateBytes() }
        // Never discard the source's coefficient or supplied PKCS#1 CRT
        // exponents. They must equal the independently computed native values.
        guard canonical.n == n, canonical.e == e, canonical.d == d,
              canonical.p == p, canonical.q == q, canonical.iqmp == iqmp,
              dmp1 == nil || dmp1 == canonical.dmp1,
              dmq1 == nil || dmq1 == canonical.dmq1 else { throw FTError.malformed("key.rsa_identity") }
        try Task.checkCancellation()
        key = imported; publicBlob = blob
    }

    /// Raw RFC 8332 RSA signature bytes; the caller wraps the exact negotiated
    /// algorithm in the SSH signature blob. SHA-1 and RSA-PSS are never selected.
    func signature(for payload: Data, algorithm: String) throws -> Data {
        guard !payload.isEmpty, payload.count <= SSHUserAuthenticationChallenge.maximumSignedDataBytes else {
            throw SSHAuthenticationError.invalidChallenge
        }
        try Task.checkCancellation()
        let signature: Data
        switch algorithm {
        case "rsa-sha2-256":
            signature = try key.signature(for: SHA256.hash(data: payload), padding: .insecurePKCS1v1_5).rawRepresentation
        case "rsa-sha2-512":
            signature = try key.signature(for: SHA512.hash(data: payload), padding: .insecurePKCS1v1_5).rawRepresentation
        default: throw SSHAuthenticationError.unsupportedAlgorithm
        }
        guard signature.count == (key.keySizeInBits + 7) / 8, signature.count <= 1024 else {
            throw SSHAuthenticationError.unavailable
        }
        try Task.checkCancellation()
        return signature
    }

    fileprivate func appendPrivateFields(to body: inout Data) throws {
        var der = key.derRepresentation
        defer { der.resetBytes(in: 0..<der.count) }
        var parts = try RSAPrivateComponents(pkcs1DER: der)
        defer { parts.erasePrivateBytes() }
        for value in [parts.n, parts.e, parts.d, parts.iqmp, parts.p, parts.q] {
            Self.appendMPInt(value, to: &body)
        }
    }

    fileprivate static func publicBlob(n: Data, e: Data) -> Data {
        var blob = SSHUserAuthenticationChallenge.string(Data("ssh-rsa".utf8))
        appendMPInt(e, to: &blob); appendMPInt(n, to: &blob)
        return blob
    }

    private static func appendMPInt(_ magnitude: Data, to result: inout Data) {
        let signByte = magnitude.first.map({ $0 & 0x80 != 0 }) == true
        result.appendBE(UInt64(magnitude.count + (signByte ? 1 : 0)), bytes: 4)
        if signByte { result.append(0) }
        result += magnitude
    }

    private static func readMPInt(_ reader: inout ByteCursor, maximumBytes: Int) throws -> Data {
        try RSADERReader.positiveMagnitude(reader.sshString(cap: maximumBytes + 1), maximumBytes: maximumBytes)
    }
}

/// Clear PKCS#1 and rsaEncryption PKCS#8 are source formats only. The normalized
/// FTAG record remains OpenSSH, protected by its authenticated outer envelope.
/// Encrypted PEM, RSA-PSS restrictions, multiprime keys and extra attributes do
/// not belong to this bounded profile and must not be silently normalized away.
struct LegacyRSASourceKey: Sendable, CustomDebugStringConvertible {
    static let maximumDERBytes = 8192
    private static let rsaEncryptionOID = Data([0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01])
    private let key: OpenSSHRSAKeyMaterial
    var debugDescription: String { "LegacyRSASourceKey([redacted])" }

    static func recognizes(_ pem: Data) -> Bool {
        guard pem.count <= FTLimit.key, let text = String(data: pem, encoding: .utf8) else { return false }
        let lines = text.trimmingCharacters(in: .whitespacesAndNewlines)
            .components(separatedBy: .newlines).filter { !$0.isEmpty }
        if lines.first == "-----BEGIN RSA PRIVATE KEY-----" { return true }
        guard lines.first == "-----BEGIN PRIVATE KEY-----", lines.count >= 3,
              let der = Data(base64Encoded: lines.dropFirst().dropLast().joined()), der.count <= maximumDERBytes else { return false }
        // Only select the RSA parser after reading the exact outer algorithm
        // identifier. An EC PKCS#8 file continues through its existing parser.
        do {
            var document = RSADERReader(der)
            var root = RSADERReader(try document.read(tag: 0x30))
            _ = try root.read(tag: 0x02)
            var algorithm = RSADERReader(try root.read(tag: 0x30))
            return try algorithm.read(tag: 0x06) == rsaEncryptionOID
        } catch { return false }
    }

    init(pem: Data) throws {
        var parts = try Self.components(pem: pem)
        defer { parts.erasePrivateBytes() }
        key = try parts.validatedKey()
    }

    /// Normalize decrypted DER in memory without creating a plaintext PEM
    /// string or changing the selected source file.
    init(pkcs8DER: Data) throws {
        var parts = try Self.components(der: pkcs8DER, pkcs8: true)
        defer { parts.erasePrivateBytes() }
        key = try parts.validatedKey()
    }

    /// Passive discovery may copy the public n/e embedded in a clear legacy
    /// container, exactly as it copies an OpenSSH public header. This path does
    /// not construct a private key, perform RSA arithmetic, derive an identity,
    /// sign or unlock anything. Explicit selection must still validate all CRT
    /// and private components before creating a transferable private record.
    static func embeddedPublicBlob(pem: Data) throws -> Data {
        var parts = try components(pem: pem)
        defer { parts.erasePrivateBytes() }
        let blob = OpenSSHRSAKeyMaterial.publicBlob(n: parts.n, e: parts.e)
        return try SSHPublicIdentity(blob: blob).blob
    }

    private static func components(pem: Data) throws -> RSAPrivateComponents {
        guard !pem.isEmpty, pem.count <= FTLimit.key, let text = String(data: pem, encoding: .utf8) else {
            throw FTError.limit("source_key.pem")
        }
        let lines = text.trimmingCharacters(in: .whitespacesAndNewlines)
            .components(separatedBy: .newlines).filter { !$0.isEmpty }
        guard !lines.contains(where: { $0.hasPrefix("Proc-Type:") || $0.hasPrefix("DEK-Info:") }) else {
            throw FTError.unsupported("source_key.encrypted_pem")
        }
        let pkcs8: Bool
        switch (lines.first, lines.last) {
        case ("-----BEGIN RSA PRIVATE KEY-----"?, "-----END RSA PRIVATE KEY-----"?): pkcs8 = false
        case ("-----BEGIN PRIVATE KEY-----"?, "-----END PRIVATE KEY-----"?): pkcs8 = true
        default: throw FTError.malformed("source_key.pem")
        }
        let encoded = lines.dropFirst().dropLast().joined()
        guard lines.count >= 3, encoded.utf8.count <= ((Self.maximumDERBytes + 2) / 3) * 4,
              var der = Data(base64Encoded: encoded), !der.isEmpty, der.count <= Self.maximumDERBytes,
              der.base64EncodedString() == encoded else { throw FTError.malformed("source_key.der") }
        defer { der.resetBytes(in: 0..<der.count) }
        return try components(der: der, pkcs8: pkcs8)
    }

    private static func components(der: Data, pkcs8: Bool) throws -> RSAPrivateComponents {
        guard !der.isEmpty, der.count <= Self.maximumDERBytes else { throw FTError.limit("source_key.der") }
        if pkcs8 {
            var document = RSADERReader(der)
            var root = RSADERReader(try document.read(tag: 0x30))
            try document.finish()
            guard try root.read(tag: 0x02) == Data([0]) else { throw FTError.unsupported("source_key.pkcs8_version") }
            var algorithm = RSADERReader(try root.read(tag: 0x30))
            guard try algorithm.read(tag: 0x06) == Self.rsaEncryptionOID,
                  try algorithm.read(tag: 0x05).isEmpty else { throw FTError.unsupported("source_key.family") }
            try algorithm.finish()
            var inner = try root.read(tag: 0x04)
            defer { inner.resetBytes(in: 0..<inner.count) }
            try root.finish()
            return try RSAPrivateComponents(pkcs1DER: inner)
        } else {
            return try RSAPrivateComponents(pkcs1DER: der)
        }
    }

    func privateRecord<C: AirGapCryptography>(credentialID: String, using crypto: C) throws -> PrivateKeyRecord {
        _ = try WorkspaceDocument.identifier(credentialID)
        try Task.checkCancellation()
        let check = try crypto.randomBytes(count: 4)
        guard check.count == 4 else { throw FTError.storage }
        var body = check + check
        defer { body.resetBytes(in: 0..<body.count) }
        body += SSHUserAuthenticationChallenge.string(Data("ssh-rsa".utf8))
        try key.appendPrivateFields(to: &body)
        body += SSHUserAuthenticationChallenge.string(Data())
        var padding: UInt8 = 1
        while !body.count.isMultiple(of: 16) { body.append(padding); padding += 1 }
        return try PrivateKeyRecord.fromDecryptedOpenSSH(publicBlob: key.publicBlob, privatePart: body,
                                                        credentialID: credentialID, using: crypto)
    }
}

private struct RSAPrivateComponents {
    let n: Data, e: Data
    var d: Data, p: Data, q: Data, dmp1: Data, dmq1: Data, iqmp: Data

    init(pkcs1DER: Data) throws {
        guard !pkcs1DER.isEmpty, pkcs1DER.count <= LegacyRSASourceKey.maximumDERBytes else {
            throw FTError.limit("key.rsa_der")
        }
        var document = RSADERReader(pkcs1DER)
        var root = RSADERReader(try document.read(tag: 0x30))
        try document.finish()
        guard try root.read(tag: 0x02) == Data([0]) else { throw FTError.unsupported("key.rsa_version") }
        n = try root.positiveInteger(maximumBytes: 1024)
        e = try root.positiveInteger(maximumBytes: 8)
        d = try root.positiveInteger(maximumBytes: 1024)
        p = try root.positiveInteger(maximumBytes: 1024)
        q = try root.positiveInteger(maximumBytes: 1024)
        dmp1 = try root.positiveInteger(maximumBytes: 1024)
        dmq1 = try root.positiveInteger(maximumBytes: 1024)
        iqmp = try root.positiveInteger(maximumBytes: 1024)
        try root.finish()
    }

    func validatedKey() throws -> OpenSSHRSAKeyMaterial {
        try OpenSSHRSAKeyMaterial(n: n, e: e, d: d, iqmp: iqmp, p: p, q: q, dmp1: dmp1, dmq1: dmq1)
    }

    mutating func erasePrivateBytes() {
        d.resetBytes(in: 0..<d.count); p.resetBytes(in: 0..<p.count); q.resetBytes(in: 0..<q.count)
        dmp1.resetBytes(in: 0..<dmp1.count); dmq1.resetBytes(in: 0..<dmq1.count); iqmp.resetBytes(in: 0..<iqmp.count)
    }
}

/// Shallow DER reader for this fixed source profile and the native PKCS#1
/// serializer. No arbitrary nesting, indefinite lengths or permissive integers.
private struct RSADERReader {
    private var cursor: ByteCursor
    init(_ data: Data) { cursor = ByteCursor(data) }

    mutating func read(tag: UInt8) throws -> Data {
        guard try cursor.uint(1) == UInt64(tag) else { throw FTError.malformed("key.rsa_der_tag") }
        let initial = try cursor.uint(1)
        let length: UInt64
        if initial < 128 { length = initial }
        else {
            let width = Int(initial & 0x7f)
            guard (1...2).contains(width) else { throw FTError.malformed("key.rsa_der_length") }
            length = try cursor.uint(width)
            guard length >= (width == 1 ? 128 : 256) else { throw FTError.malformed("key.rsa_der_length") }
        }
        guard length <= UInt64(LegacyRSASourceKey.maximumDERBytes), length <= UInt64(cursor.remaining) else {
            throw FTError.limit("key.rsa_der_length")
        }
        return try cursor.take(Int(length))
    }

    mutating func positiveInteger(maximumBytes: Int) throws -> Data {
        try Self.positiveMagnitude(read(tag: 0x02), maximumBytes: maximumBytes)
    }

    static func positiveMagnitude(_ encoded: Data, maximumBytes: Int) throws -> Data {
        guard let first = encoded.first, first & 0x80 == 0 else { throw FTError.malformed("key.rsa_integer") }
        let magnitude: Data
        if first == 0 {
            guard encoded.count > 1, encoded[encoded.startIndex + 1] & 0x80 != 0 else {
                throw FTError.malformed("key.rsa_integer")
            }
            magnitude = Data(encoded.dropFirst())
        } else { magnitude = encoded }
        guard magnitude.count <= maximumBytes else { throw FTError.limit("key.rsa_integer") }
        return magnitude
    }

    func finish() throws {
        guard cursor.remaining == 0 else { throw FTError.malformed("key.rsa_der_extent") }
    }
}
