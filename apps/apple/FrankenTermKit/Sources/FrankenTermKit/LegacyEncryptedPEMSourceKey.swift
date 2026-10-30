import Foundation

/// An explicitly selected existing RSA/EC PEM protected with OpenSSL's
/// traditional AES-CBC profile. Metadata and ciphertext bounds are admitted
/// before any passphrase prompt. The file carries no clear public identity:
/// decryption must produce a complete validated key matching the separately
/// approved workspace reference before the selection owner releases it.
struct LegacyEncryptedPEMSourceKey: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    enum Family: Sendable {
        case rsa, ecdsa
        var maximumDERBytes: Int {
            switch self {
            case .rsa: LegacyRSASourceKey.maximumDERBytes
            case .ecdsa: LegacyECDSASourceKey.maximumDERBytes
            }
        }
        func accepts(algorithm: String) -> Bool {
            switch self {
            case .rsa: algorithm == "ssh-rsa"
            case .ecdsa: SSHECDSACurve.allCases.contains { $0.algorithm == algorithm }
            }
        }
    }
    let family: Family
    let keyBytes: Int
    private let iv: Data, ciphertext: Data
    var protection: SourceKeyProtection { .legacyPEM(aesKeyBits: keyBytes * 8) }
    var description: String { "LegacyEncryptedPEMSourceKey([redacted])" }
    var debugDescription: String { description }

    static func recognizes(_ pem: Data) -> Bool {
        guard pem.count <= FTLimit.key, let text = String(data: pem, encoding: .utf8) else { return false }
        let lines = text.trimmingCharacters(in: .whitespacesAndNewlines).components(separatedBy: .newlines)
        guard ["-----BEGIN RSA PRIVATE KEY-----", "-----BEGIN EC PRIVATE KEY-----"].contains(lines.first ?? "") else { return false }
        return lines.contains { $0.hasPrefix("Proc-Type:") || $0.hasPrefix("DEK-Info:") }
    }

    init(pem: Data) throws {
        guard !pem.isEmpty, pem.count <= FTLimit.key,
              pem.allSatisfy({ $0 == 9 || $0 == 10 || $0 == 13 || (32...126).contains($0) }),
              let text = String(data: pem, encoding: .utf8) else { throw FTError.limit("source_key.pem") }
        let normalized = text.replacingOccurrences(of: "\r\n", with: "\n")
        guard !normalized.contains("\r") else { throw FTError.malformed("source_key.legacy_pem_headers") }
        let lines = normalized.trimmingCharacters(in: .whitespacesAndNewlines).components(separatedBy: "\n")
        guard lines.count >= 6 else { throw FTError.malformed("source_key.pem") }
        switch (lines.first, lines.last) {
        case ("-----BEGIN RSA PRIVATE KEY-----"?, "-----END RSA PRIVATE KEY-----"?): family = .rsa
        case ("-----BEGIN EC PRIVATE KEY-----"?, "-----END EC PRIVATE KEY-----"?): family = .ecdsa
        default: throw FTError.malformed("source_key.pem")
        }
        guard lines[1] == "Proc-Type: 4,ENCRYPTED", lines[2].hasPrefix("DEK-Info: "), lines[3].isEmpty else {
            throw FTError.malformed("source_key.legacy_pem_headers")
        }
        let parameters = lines[2].dropFirst("DEK-Info: ".count).split(separator: ",", omittingEmptySubsequences: false)
        guard parameters.count == 2 else { throw FTError.malformed("source_key.legacy_pem_headers") }
        switch parameters[0] {
        case "AES-128-CBC": keyBytes = 16
        case "AES-192-CBC": keyBytes = 24
        case "AES-256-CBC": keyBytes = 32
        default: throw FTError.unsupported("source_key.legacy_pem_cipher")
        }
        let hex = Array(parameters[1].utf8)
        guard hex.count == 32 else { throw FTError.malformed("source_key.legacy_pem_iv") }
        func nibble(_ byte: UInt8) throws -> UInt8 {
            switch byte {
            case 48...57: return byte - 48
            case 65...70: return byte - 55
            case 97...102: return byte - 87
            default: throw FTError.malformed("source_key.legacy_pem_iv")
            }
        }
        var decodedIV = Data(); decodedIV.reserveCapacity(16)
        for index in stride(from: 0, to: hex.count, by: 2) {
            decodedIV.append(try nibble(hex[index]) << 4 | nibble(hex[index + 1]))
        }
        iv = decodedIV
        let body = lines.dropFirst(4).dropLast()
        guard body.allSatisfy({ !$0.isEmpty }) else { throw FTError.malformed("source_key.legacy_pem_body") }
        let encoded = body.joined(), maximumCiphertextBytes = family.maximumDERBytes + 16
        guard encoded.utf8.count <= ((maximumCiphertextBytes + 2) / 3) * 4 else {
            throw FTError.limit("source_key.legacy_pem_ciphertext")
        }
        guard let encrypted = Data(base64Encoded: encoded), !encrypted.isEmpty,
              encrypted.count <= maximumCiphertextBytes, encrypted.count.isMultiple(of: 16),
              encrypted.base64EncodedString() == encoded else { throw FTError.malformed("source_key.legacy_pem_body") }
        ciphertext = encrypted
    }

    func unlock<C: AirGapCryptography>(passphrase: Data, credentialID: String, using crypto: C) throws -> PrivateKeyRecord {
        _ = try WorkspaceDocument.identifier(credentialID)
        guard (1...OpenSSHSourceKey.maximumPassphraseBytes).contains(passphrase.count) else {
            throw FTError.limit("source_key.passphrase")
        }
        try Task.checkCancellation()
        var key = try crypto.legacyPEMKey(passphrase: passphrase, salt: Data(iv.prefix(8)), outputByteCount: keyBytes)
        defer { key.resetBytes(in: 0..<key.count) }
        try Task.checkCancellation()
        guard key.count == keyBytes else { throw FTError.malformed("source_key.kdf_output") }
        var clear = try crypto.decryptAESCBC(ciphertext, key: key, iv: iv)
        defer { clear.resetBytes(in: 0..<clear.count) }
        try Task.checkCancellation()
        do {
            guard clear.count == ciphertext.count, let finalByte = clear.last,
                  (1...16).contains(Int(finalByte)) else { throw FTError.malformed("source_key.legacy_pem_padding") }
            let padding = Int(finalByte)
            guard clear.count > padding, clear.suffix(padding).allSatisfy({ $0 == finalByte }) else {
                throw FTError.malformed("source_key.legacy_pem_padding")
            }
            var der = Data(clear.dropLast(padding))
            defer { der.resetBytes(in: 0..<der.count) }
            let record: PrivateKeyRecord
            switch family {
            case .rsa: record = try LegacyRSASourceKey(pkcs1DER: der).privateRecord(credentialID: credentialID, using: crypto)
            case .ecdsa: record = try LegacyECDSASourceKey(sec1DER: der).privateRecord(credentialID: credentialID, using: crypto)
            }
            try Task.checkCancellation()
            return record
        } catch is CancellationError { throw CancellationError() }
        catch { throw OpenSSHSourceKeyError.incorrectPassphraseOrDamagedKey }
    }
}
