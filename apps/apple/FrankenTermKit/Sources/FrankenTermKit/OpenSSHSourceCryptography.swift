import Foundation
import COpenSSHBcrypt
import CryptoExtras
#if canImport(CryptoKit)
import CryptoKit
#else
import Crypto
#endif

/// OpenSSL's traditional PEM compatibility derivation: EVP_BytesToKey with
/// MD5, count 1 and the first eight bytes of the file's IV as salt. The IV
/// itself comes from DEK-Info, not the KDF output (OpenSSL pem_lib.c).
/// Only reading an explicitly selected existing file can reach this legacy
/// function. FTAG continues to use HKDF-SHA256 and ChaCha20-Poly1305.
enum LegacyPEMSourcePrimitives {
    static func key(passphrase: Data, salt: Data, outputByteCount: Int) throws -> Data {
        guard (1...OpenSSHSourceKey.maximumPassphraseBytes).contains(passphrase.count),
              salt.count == 8, [16, 24, 32].contains(outputByteCount) else {
            throw FTError.limit("source_key.legacy_pem_kdf")
        }
        try Task.checkCancellation()
        var previous = Data(), result = Data()
        defer { previous.resetBytes(in: 0..<previous.count); result.resetBytes(in: 0..<result.count) }
        result.reserveCapacity(32)
        // AES admits at most two digest blocks; no file-supplied work factor.
        while result.count < outputByteCount {
            var digest = Insecure.MD5()
            digest.update(data: previous); digest.update(data: passphrase); digest.update(data: salt)
            previous.resetBytes(in: 0..<previous.count)
            previous = Data(digest.finalize())
            result.append(previous)
        }
        try Task.checkCancellation()
        return Data(result.prefix(outputByteCount))
    }
}

/// Existing encrypted source compatibility through pinned Swift CryptoExtras:
/// CommonCrypto on Apple, BoringSSL on Linux. Bounds apply before native work.
/// The unchecked-rounds API is needed only to read already-encrypted files
/// below a new-key recommendation; this code never encrypts a source file.
enum PKCS8SourcePrimitives {
    static func pbkdf2(passphrase: Data, salt: Data, iterations: UInt32,
                       prf: PKCS8PBKDF2PRF, outputByteCount: Int) throws -> Data {
        guard (1...OpenSSHSourceKey.maximumPassphraseBytes).contains(passphrase.count),
              (8...64).contains(salt.count),
              (1...EncryptedPKCS8SourceKey.maximumIterations).contains(iterations),
              [16, 24, 32].contains(outputByteCount) else {
            throw FTError.limit("source_key.pbkdf2_parameters")
        }
        try Task.checkCancellation()
        let function: KDF.Insecure.PBKDF2.HashFunction
        switch prf {
        case .hmacSHA1: function = .insecureSHA1
        case .hmacSHA256: function = .sha256
        case .hmacSHA384: function = .sha384
        case .hmacSHA512: function = .sha512
        }
        let key = try KDF.Insecure.PBKDF2.deriveKey(from: passphrase, salt: salt, using: function,
            outputByteCount: outputByteCount, unsafeUncheckedRounds: Int(iterations))
        try Task.checkCancellation()
        var result = key.withUnsafeBytes { Data($0) }
        do {
            guard result.count == outputByteCount else { throw FTError.malformed("source_key.kdf_output") }
            try Task.checkCancellation()
            return result
        } catch {
            result.resetBytes(in: 0..<result.count)
            throw error
        }
    }
}

/// Shared admission and ownership for the pinned OpenSSH PBKDF. Its C target
/// has no platform crypto dependency: each operation receives one maintained
/// SHA-512 implementation synchronously and retains no callback or private data.
enum OpenSSHSourcePrimitives {
    static func bcryptPBKDF(passphrase: Data, salt: Data, rounds: UInt32, outputByteCount: Int,
                           sha512: ft_bcrypt_sha512_fn) throws -> Data {
        guard (1...1024).contains(passphrase.count), (16...64).contains(salt.count),
              (1...64).contains(rounds), [32, 40, 48].contains(outputByteCount) else {
            throw FTError.limit("key.source_kdf")
        }
        // The native call is bounded but cannot be interrupted inside a round.
        // Cancellation is checked on both sides before any result is released.
        try Task.checkCancellation()
        var result = Data(count: outputByteCount)
        let status = result.withUnsafeMutableBytes { output in
            passphrase.withUnsafeBytes { password in
                salt.withUnsafeBytes { saltBytes in
                    ft_openssh_bcrypt_pbkdf(password.bindMemory(to: UInt8.self).baseAddress, passphrase.count,
                        saltBytes.bindMemory(to: UInt8.self).baseAddress, salt.count,
                        output.bindMemory(to: UInt8.self).baseAddress, outputByteCount, rounds, sha512)
                }
            }
        }
        do {
            guard status == 0 else { throw FTError.authentication }
            try Task.checkCancellation()
            return result
        } catch {
            result.resetBytes(in: 0..<result.count)
            throw error
        }
    }

    static func validateAES(ciphertext: Data, key: Data, iv: Data) throws {
        guard !ciphertext.isEmpty, ciphertext.count <= FTLimit.key, ciphertext.count.isMultiple(of: 16),
              [16, 24, 32].contains(key.count), iv.count == 16 else {
            throw FTError.limit("key.source_cipher")
        }
        try Task.checkCancellation()
    }
}

#if canImport(CryptoKit)
extension AppleCryptography {
    public func bcryptPBKDF(passphrase: Data, salt: Data, rounds: UInt32, outputByteCount: Int) throws -> Data {
        try OpenSSHSourcePrimitives.bcryptPBKDF(passphrase: passphrase, salt: salt, rounds: rounds,
                                              outputByteCount: outputByteCount, sha512: { output, input, count in
            guard let input, let output else { return }
            let digest = SHA512.hash(data: Data(bytes: input, count: count))
            for (index, byte) in digest.enumerated() { output[index] = byte }
        })
    }

    public func decryptAESCTR(_ ciphertext: Data, key: Data, iv: Data) throws -> Data {
        try decryptSourceAES(ciphertext, key: key, iv: iv, cbc: false)
    }

    public func decryptAESCBC(_ ciphertext: Data, key: Data, iv: Data) throws -> Data {
        try decryptSourceAES(ciphertext, key: key, iv: iv, cbc: true)
    }

    private func decryptSourceAES(_ ciphertext: Data, key: Data, iv: Data, cbc: Bool) throws -> Data {
        try OpenSSHSourcePrimitives.validateAES(ciphertext: ciphertext, key: key, iv: iv)
        var result = Data(count: ciphertext.count)
        let status = result.withUnsafeMutableBytes { output in
            ciphertext.withUnsafeBytes { input in
                key.withUnsafeBytes { keyBytes in
                    iv.withUnsafeBytes { ivBytes in
                        let decrypt = cbc ? ft_openssh_aes_cbc : ft_openssh_aes_ctr
                        return decrypt(input.bindMemory(to: UInt8.self).baseAddress, ciphertext.count,
                            keyBytes.bindMemory(to: UInt8.self).baseAddress, key.count,
                            ivBytes.bindMemory(to: UInt8.self).baseAddress, iv.count,
                            output.bindMemory(to: UInt8.self).baseAddress, ciphertext.count)
                    }
                }
            }
        }
        do {
            guard status == 1 else { throw FTError.authentication }
            try Task.checkCancellation()
            return result
        } catch {
            result.resetBytes(in: 0..<result.count)
            throw error
        }
    }
}
#endif
