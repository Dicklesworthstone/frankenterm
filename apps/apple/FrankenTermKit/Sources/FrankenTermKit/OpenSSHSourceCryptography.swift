import Foundation
import COpenSSHBcrypt

/// Shared admission and ownership for the pinned OpenSSH PBKDF. Its C target
/// has no platform crypto dependency: each operation receives one maintained
/// SHA-512 implementation synchronously and retains no callback or private data.
enum OpenSSHSourcePrimitives {
    static func bcryptPBKDF(passphrase: Data, salt: Data, rounds: UInt32, outputByteCount: Int,
                           sha512: ft_bcrypt_sha512_fn) throws -> Data {
        guard (1...1024).contains(passphrase.count), (16...64).contains(salt.count),
              (1...64).contains(rounds), outputByteCount == 32 || outputByteCount == 48 else {
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

    static func validateAESCTR(ciphertext: Data, key: Data, iv: Data) throws {
        guard !ciphertext.isEmpty, ciphertext.count <= FTLimit.key, ciphertext.count.isMultiple(of: 16),
              key.count == 16 || key.count == 32, iv.count == 16 else {
            throw FTError.limit("key.source_cipher")
        }
        try Task.checkCancellation()
    }
}

#if canImport(CryptoKit)
import CryptoKit

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
        try OpenSSHSourcePrimitives.validateAESCTR(ciphertext: ciphertext, key: key, iv: iv)
        var result = Data(count: ciphertext.count)
        let status = result.withUnsafeMutableBytes { output in
            ciphertext.withUnsafeBytes { input in
                key.withUnsafeBytes { keyBytes in
                    iv.withUnsafeBytes { ivBytes in
                        ft_openssh_aes_ctr(input.bindMemory(to: UInt8.self).baseAddress, ciphertext.count,
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
