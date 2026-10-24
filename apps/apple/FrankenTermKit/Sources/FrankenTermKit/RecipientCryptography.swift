import Foundation

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

/// The same bounded, one-shot maintained implementation is used by Apple and
/// Linux callers. Independent deterministic test oracles remain test-only.
extension RecipientCryptography {
    public func x25519PublicKey(privateKey: Data) throws -> Data {
        try RecipientHPKE.publicKey(forPrivateKey: privateKey)
    }

    public func hpkeSeal(_ plaintext: Data, recipientPublicKey: Data, info: Data,
                         aadPrefix: Data) throws -> HPKESealedMessage {
        guard aadPrefix.count <= 4096 - 32 else { throw FTError.limit("recipient.hpke_aad") }
        let sealed = try RecipientHPKE.seal(plaintext, recipientPublicKey: recipientPublicKey, info: info) { aadPrefix + $0 }
        return HPKESealedMessage(encapsulatedKey: sealed.encapsulatedKey, ciphertext: sealed.ciphertext)
    }

    public func hpkeOpen(_ ciphertextAndTag: Data, recipientPrivateKey: Data,
                         encapsulatedKey: Data, info: Data, aad: Data) throws -> Data {
        try RecipientHPKE.open(ciphertextAndTag, privateKey: recipientPrivateKey,
                               encapsulatedKey: encapsulatedKey, info: info, authenticating: aad)
    }
}

#if canImport(CryptoKit)
extension AppleCryptography: RecipientCryptography {}
#endif
