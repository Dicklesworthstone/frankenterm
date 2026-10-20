import Foundation
@testable import FrankenTermKit
#if canImport(CryptoKit)
typealias TestCryptography = AppleCryptography
#else
import CTestOpenSSL

/// Independent, test-only Linux oracle. Never linked into an Apple product.
struct TestCryptography: AirGapCryptography {
    func sha256(_ bytes: Data) -> Data {
        var result = Data(count: 32)
        _ = result.withUnsafeMutableBytes { output in
            bytes.withUnsafeBytes { input in SHA256(input.bindMemory(to: UInt8.self).baseAddress, bytes.count, output.bindMemory(to: UInt8.self).baseAddress) }
        }
        return result
    }
    func randomBytes(count: Int) throws -> Data { Data((0..<count).map { _ in UInt8.random(in: 0...255) }) }
    private func hmac(_ key: Data, _ input: Data) -> Data {
        var output = Data(count: 32), length: UInt32 = 32
        _ = output.withUnsafeMutableBytes { out in
            key.withUnsafeBytes { key in
                input.withUnsafeBytes { bytes in
                    HMAC(EVP_sha256(), key.baseAddress, Int32(key.count), bytes.bindMemory(to: UInt8.self).baseAddress,
                         input.count, out.bindMemory(to: UInt8.self).baseAddress, &length)
                }
            }
        }
        return output
    }
    private func key(_ secret: Data, _ salt: Data) -> Data {
        hmac(hmac(salt, secret), Data("FrankenTerm/FTAG/v1/code-key".utf8) + Data([1]))
    }
    func seal(_ plaintext: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data {
        guard let context = EVP_CIPHER_CTX_new() else { throw FTError.storage }
        defer { EVP_CIPHER_CTX_free(context) }
        let key = key(secret, salt)
        guard key.withUnsafeBytes({ k in nonce.withUnsafeBytes({ n in
            EVP_EncryptInit_ex(context, EVP_chacha20_poly1305(), nil, k.bindMemory(to: UInt8.self).baseAddress, n.bindMemory(to: UInt8.self).baseAddress)
        }) }) == 1 else { throw FTError.authentication }
        var length: Int32 = 0
        guard aad.withUnsafeBytes({ EVP_EncryptUpdate(context, nil, &length, $0.bindMemory(to: UInt8.self).baseAddress, Int32(aad.count)) }) == 1 else { throw FTError.authentication }
        var output = Data(count: plaintext.count + 16)
        guard output.withUnsafeMutableBytes({ out in plaintext.withUnsafeBytes({ input in
            EVP_EncryptUpdate(context, out.bindMemory(to: UInt8.self).baseAddress, &length, input.bindMemory(to: UInt8.self).baseAddress, Int32(plaintext.count))
        }) }) == 1 else { throw FTError.authentication }
        let written = Int(length)
        guard output.withUnsafeMutableBytes({ EVP_EncryptFinal_ex(context, $0.bindMemory(to: UInt8.self).baseAddress!.advanced(by: written), &length) }) == 1 else { throw FTError.authentication }
        guard written + Int(length) == plaintext.count else { throw FTError.authentication }
        var tag = Data(count: 16)
        guard tag.withUnsafeMutableBytes({ EVP_CIPHER_CTX_ctrl(context, EVP_CTRL_AEAD_GET_TAG, 16, $0.baseAddress) }) == 1 else { throw FTError.authentication }
        output.count = plaintext.count
        return output + tag
    }
    func open(_ ciphertextAndTag: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data {
        guard ciphertextAndTag.count >= 16, let context = EVP_CIPHER_CTX_new() else { throw FTError.authentication }
        defer { EVP_CIPHER_CTX_free(context) }
        let key = key(secret, salt), ciphertext = Data(ciphertextAndTag.dropLast(16))
        var tag = Data(ciphertextAndTag.suffix(16)), length: Int32 = 0
        guard key.withUnsafeBytes({ k in nonce.withUnsafeBytes({ n in
            EVP_DecryptInit_ex(context, EVP_chacha20_poly1305(), nil, k.bindMemory(to: UInt8.self).baseAddress, n.bindMemory(to: UInt8.self).baseAddress)
        }) }) == 1 else { throw FTError.authentication }
        guard aad.withUnsafeBytes({ EVP_DecryptUpdate(context, nil, &length, $0.bindMemory(to: UInt8.self).baseAddress, Int32(aad.count)) }) == 1 else { throw FTError.authentication }
        var output = Data(count: ciphertext.count + 16)
        guard output.withUnsafeMutableBytes({ out in ciphertext.withUnsafeBytes({ input in
            EVP_DecryptUpdate(context, out.bindMemory(to: UInt8.self).baseAddress, &length, input.bindMemory(to: UInt8.self).baseAddress, Int32(ciphertext.count))
        }) }) == 1 else { throw FTError.authentication }
        let written = Int(length)
        guard tag.withUnsafeMutableBytes({ EVP_CIPHER_CTX_ctrl(context, EVP_CTRL_AEAD_SET_TAG, 16, $0.baseAddress) }) == 1 else { throw FTError.authentication }
        guard output.withUnsafeMutableBytes({ EVP_DecryptFinal_ex(context, $0.bindMemory(to: UInt8.self).baseAddress!.advanced(by: written), &length) }) == 1 else { throw FTError.authentication }
        output.count = written + Int(length)
        return output
    }
    func ed25519PublicKey(seed: Data) throws -> Data {
        guard seed.count == 32, let key = seed.withUnsafeBytes({
            EVP_PKEY_new_raw_private_key(EVP_PKEY_ED25519, nil, $0.bindMemory(to: UInt8.self).baseAddress, seed.count)
        }) else { throw FTError.malformed("test.ed25519") }
        defer { EVP_PKEY_free(key) }
        var result = Data(count: 32), count = 32
        guard result.withUnsafeMutableBytes({ EVP_PKEY_get_raw_public_key(key, $0.bindMemory(to: UInt8.self).baseAddress, &count) }) == 1,
              count == 32 else { throw FTError.malformed("test.ed25519") }
        return result
    }
    func ecdsaPublicKey(scalar: Data, curve: SSHECDSACurve) throws -> Data {
        let nid: Int32
        switch curve {
        case .nistp256: nid = NID_X9_62_prime256v1
        case .nistp384: nid = NID_secp384r1
        case .nistp521: nid = NID_secp521r1
        }
        guard scalar.count == curve.scalarBytes, let group = EC_GROUP_new_by_curve_name(nid) else { throw FTError.malformed("test.ecdsa") }
        defer { EC_GROUP_free(group) }
        guard let value = scalar.withUnsafeBytes({ BN_bin2bn($0.bindMemory(to: UInt8.self).baseAddress, Int32(scalar.count), nil) }) else {
            throw FTError.malformed("test.ecdsa")
        }
        defer { BN_clear_free(value) }
        guard let order = BN_new() else { throw FTError.storage }
        defer { BN_free(order) }
        // OpenSSL obtains the order independently, rather than reusing the
        // application's table or accepting a silently reduced private scalar.
        guard EC_GROUP_get_order(group, order, nil) == 1, BN_num_bits(value) > 0, BN_cmp(value, order) < 0,
              let point = EC_POINT_new(group) else { throw FTError.malformed("test.ecdsa") }
        defer { EC_POINT_free(point) }
        guard EC_POINT_mul(group, point, value, nil, nil, nil) == 1 else { throw FTError.malformed("test.ecdsa") }
        var output = Data(count: 1 + 2 * curve.scalarBytes)
        let length = output.count
        guard output.withUnsafeMutableBytes({
            EC_POINT_point2oct(group, point, POINT_CONVERSION_UNCOMPRESSED, $0.bindMemory(to: UInt8.self).baseAddress, length, nil)
        }) == length else { throw FTError.malformed("test.ecdsa") }
        return output
    }
}
#endif
