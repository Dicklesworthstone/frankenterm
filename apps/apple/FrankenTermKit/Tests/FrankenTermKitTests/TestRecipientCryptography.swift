import Foundation
@testable import FrankenTermKit
#if !canImport(CryptoKit)
import CTestOpenSSL

/// Independent RFC 9180 test oracle. Curve and AEAD operations are OpenSSL;
/// this labeled HKDF/key schedule never enters the production target.
struct HPKETestKeySchedule {
    let sharedSecret: Data, context: Data, secret: Data
    let key: Data, baseNonce: Data, exporterSecret: Data
}

extension TestCryptography: RecipientCryptography {
    func x25519PublicKey(privateKey: Data) throws -> Data {
        guard privateKey.count == 32, let key = privateKey.withUnsafeBytes({
            EVP_PKEY_new_raw_private_key(EVP_PKEY_X25519, nil, $0.bindMemory(to: UInt8.self).baseAddress, privateKey.count)
        }) else { throw FTError.malformed("recipient.private_key") }
        defer { EVP_PKEY_free(key) }
        var result = Data(count: 32), count = 32
        guard result.withUnsafeMutableBytes({
            EVP_PKEY_get_raw_public_key(key, $0.bindMemory(to: UInt8.self).baseAddress, &count)
        }) == 1, count == 32 else { throw FTError.authentication }
        return result
    }

    private func x25519(privateKey: Data, publicKey: Data) throws -> Data {
        guard privateKey.count == 32, publicKey.count == 32,
              let key = privateKey.withUnsafeBytes({
                  EVP_PKEY_new_raw_private_key(EVP_PKEY_X25519, nil, $0.bindMemory(to: UInt8.self).baseAddress, privateKey.count)
              }) else { throw FTError.authentication }
        defer { EVP_PKEY_free(key) }
        guard let peer = publicKey.withUnsafeBytes({
            EVP_PKEY_new_raw_public_key(EVP_PKEY_X25519, nil, $0.bindMemory(to: UInt8.self).baseAddress, publicKey.count)
        }) else { throw FTError.authentication }
        defer { EVP_PKEY_free(peer) }
        guard let context = EVP_PKEY_CTX_new(key, nil) else { throw FTError.storage }
        defer { EVP_PKEY_CTX_free(context) }
        var result = Data(count: 32), count = 32
        guard EVP_PKEY_derive_init(context) == 1, EVP_PKEY_derive_set_peer(context, peer) == 1,
              result.withUnsafeMutableBytes({
                  EVP_PKEY_derive(context, $0.bindMemory(to: UInt8.self).baseAddress, &count)
              }) == 1, count == 32, result.contains(where: { $0 != 0 }) else { throw FTError.authentication }
        return result
    }

    private func hpkeExtract(salt: Data, suite: Data, label: String, input: Data) -> Data {
        hmac(salt.isEmpty ? Data(repeating: 0, count: 32) : salt,
             Data("HPKE-v1".utf8) + suite + Data(label.utf8) + input)
    }

    private func hpkeExpand(prk: Data, suite: Data, label: String, info: Data, length: Int) throws -> Data {
        guard (1...8160).contains(length) else { throw FTError.limit("test.hkdf.length") }
        var labeledInfo = Data()
        labeledInfo.appendBE(UInt64(length), bytes: 2)
        labeledInfo += Data("HPKE-v1".utf8) + suite + Data(label.utf8) + info
        var output = Data(), block = Data(), counter: UInt8 = 1
        while output.count < length {
            block = hmac(prk, block + labeledInfo + Data([counter]))
            output += block
            if output.count < length { counter += 1 }
        }
        return Data(output.prefix(length))
    }

    func hpkeKeySchedule(privateKey: Data, peerPublicKey: Data, encapsulatedKey: Data,
                         recipientPublicKey: Data, info: Data) throws -> HPKETestKeySchedule {
        guard encapsulatedKey.count == 32, recipientPublicKey.count == 32 else { throw FTError.authentication }
        let kem = Data("KEM".utf8) + Data([0, 32])
        let suite = Data("HPKE".utf8) + Data([0, 32, 0, 1, 0, 3])
        let dh = try x25519(privateKey: privateKey, publicKey: peerPublicKey)
        let eae = hpkeExtract(salt: Data(), suite: kem, label: "eae_prk", input: dh)
        let shared = try hpkeExpand(prk: eae, suite: kem, label: "shared_secret",
                                    info: encapsulatedKey + recipientPublicKey, length: 32)
        let pskIDHash = hpkeExtract(salt: Data(), suite: suite, label: "psk_id_hash", input: Data())
        let infoHash = hpkeExtract(salt: Data(), suite: suite, label: "info_hash", input: info)
        let context = Data([0]) + pskIDHash + infoHash
        let secret = hpkeExtract(salt: shared, suite: suite, label: "secret", input: Data())
        return try HPKETestKeySchedule(
            sharedSecret: shared, context: context, secret: secret,
            key: hpkeExpand(prk: secret, suite: suite, label: "key", info: context, length: 32),
            baseNonce: hpkeExpand(prk: secret, suite: suite, label: "base_nonce", info: context, length: 12),
            exporterSecret: hpkeExpand(prk: secret, suite: suite, label: "exp", info: context, length: 32))
    }

    /// Raw AAD is intentional here: RFC vectors predate FTAG's header binding.
    func hpkeSealDeterministically(_ plaintext: Data, recipientPublicKey: Data,
                                  ephemeralPrivateKey: Data, info: Data, aad: Data) throws -> HPKESealedMessage {
        let enc = try x25519PublicKey(privateKey: ephemeralPrivateKey)
        let schedule = try hpkeKeySchedule(privateKey: ephemeralPrivateKey, peerPublicKey: recipientPublicKey,
                                           encapsulatedKey: enc, recipientPublicKey: recipientPublicKey, info: info)
        return try HPKESealedMessage(encapsulatedKey: enc,
                                     ciphertext: aeadSeal(plaintext, key: schedule.key, nonce: schedule.baseNonce, aad: aad))
    }

    func hpkeSeal(_ plaintext: Data, recipientPublicKey: Data, info: Data,
                  aadPrefix: Data) throws -> HPKESealedMessage {
        guard recipientPublicKey.count == 32 else { throw FTError.malformed("recipient.public_key") }
        let ephemeral = try randomBytes(count: 32)
        let enc = try x25519PublicKey(privateKey: ephemeral)
        return try hpkeSealDeterministically(plaintext, recipientPublicKey: recipientPublicKey,
                                            ephemeralPrivateKey: ephemeral, info: info, aad: aadPrefix + enc)
    }

    func hpkeOpen(_ ciphertextAndTag: Data, recipientPrivateKey: Data,
                  encapsulatedKey: Data, info: Data, aad: Data) throws -> Data {
        guard recipientPrivateKey.count == 32, encapsulatedKey.count == 32,
              ciphertextAndTag.count >= 16 else { throw FTError.authentication }
        let publicKey = try x25519PublicKey(privateKey: recipientPrivateKey)
        let schedule = try hpkeKeySchedule(privateKey: recipientPrivateKey, peerPublicKey: encapsulatedKey,
                                           encapsulatedKey: encapsulatedKey, recipientPublicKey: publicKey, info: info)
        return try aeadOpen(ciphertextAndTag, key: schedule.key, nonce: schedule.baseNonce, aad: aad)
    }
}
#endif
