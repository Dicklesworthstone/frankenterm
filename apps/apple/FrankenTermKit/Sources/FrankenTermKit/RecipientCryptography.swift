import Foundation
#if canImport(CryptoKit)
import CryptoKit
#endif

/// One RFC 9180 Base-mode message. Its encapsulated key is public; the native
/// sender context and ephemeral private key never leave the primitive boundary.
public struct HPKESealedMessage: Sendable {
    public let encapsulatedKey: Data
    public let ciphertext: Data

    public init(encapsulatedKey: Data, ciphertext: Data) {
        self.encapsulatedKey = encapsulatedKey
        self.ciphertext = ciphertext
    }
}

/// Fixed FTAG recipient profile: DHKEM(X25519, HKDF-SHA256), HKDF-SHA256,
/// ChaCha20-Poly1305, Base mode. No wire input can select another suite or mode.
public protocol RecipientCryptography: AirGapCryptography {
    func x25519PublicKey(privateKey: Data) throws -> Data

    /// Sets up a fresh sender, obtains `enc`, and authenticates exactly
    /// `aadPrefix + enc`. The single seal uses HPKE sequence number zero.
    func hpkeSeal(_ plaintext: Data, recipientPublicKey: Data, info: Data,
                  aadPrefix: Data) throws -> HPKESealedMessage

    /// Authenticates the complete message before returning any plaintext.
    func hpkeOpen(_ ciphertextAndTag: Data, recipientPrivateKey: Data,
                  encapsulatedKey: Data, info: Data, aad: Data) throws -> Data
}

#if canImport(CryptoKit)
extension AppleCryptography: RecipientCryptography {
    public func x25519PublicKey(privateKey: Data) throws -> Data {
        guard privateKey.count == 32 else { throw FTError.malformed("recipient.private_key") }
        do {
            return try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: privateKey).publicKey.rawRepresentation
        } catch { throw FTError.malformed("recipient.private_key") }
    }

    public func hpkeSeal(_ plaintext: Data, recipientPublicKey: Data, info: Data,
                         aadPrefix: Data) throws -> HPKESealedMessage {
        guard recipientPublicKey.count == 32 else { throw FTError.malformed("recipient.public_key") }
        do {
            let key = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: recipientPublicKey)
            var sender = try HPKE.Sender(recipientKey: key, ciphersuite: .Curve25519_SHA256_ChachaPoly, info: info)
            let enc = sender.encapsulatedKey
            guard enc.count == 32 else { throw FTError.authentication }
            let ciphertext = try sender.seal(plaintext, authenticating: aadPrefix + enc)
            guard ciphertext.count == plaintext.count + 16 else { throw FTError.authentication }
            return HPKESealedMessage(encapsulatedKey: enc, ciphertext: ciphertext)
        } catch { throw FTError.authentication }
    }

    public func hpkeOpen(_ ciphertextAndTag: Data, recipientPrivateKey: Data,
                         encapsulatedKey: Data, info: Data, aad: Data) throws -> Data {
        guard recipientPrivateKey.count == 32, encapsulatedKey.count == 32,
              ciphertextAndTag.count >= 16 else { throw FTError.authentication }
        do {
            let key = try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: recipientPrivateKey)
            var recipient = try HPKE.Recipient(privateKey: key, ciphersuite: .Curve25519_SHA256_ChachaPoly,
                                               info: info, encapsulatedKey: encapsulatedKey)
            return try recipient.open(ciphertextAndTag, authenticating: aad)
        } catch { throw FTError.authentication }
    }
}
#endif
