import Foundation
#if canImport(CryptoKit)
import CryptoKit
#else
import Crypto
#endif

/// The fixed RFC 9180 Base-mode suite required by FTAG recipient transfers.
/// System CryptoKit implements it on Apple; pinned Swift Crypto implements the
/// same API on Linux. This adapter never implements a KEM or key schedule.
public enum RecipientHPKE {
    public struct Sealed: Sendable, CustomDebugStringConvertible {
        public let encapsulatedKey: Data
        public let ciphertext: Data
        public var debugDescription: String { "RecipientHPKE.Sealed([encrypted])" }
    }

    private static let maximumContextBytes = 4096

    public static func generatePrivateKey() throws -> Data {
        try Task.checkCancellation()
        let key = Curve25519.KeyAgreement.PrivateKey()
        try Task.checkCancellation()
        return key.rawRepresentation
    }

    public static func publicKey(forPrivateKey privateKey: Data) throws -> Data {
        guard privateKey.count == 32 else { throw FTError.malformed("recipient.private_key") }
        try Task.checkCancellation()
        do {
            return try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: privateKey).publicKey.rawRepresentation
        } catch { throw FTError.malformed("recipient.private_key") }
    }

    /// The closure receives the freshly encapsulated key so the caller can
    /// construct and authenticate the complete immutable envelope header. The
    /// sender context stays local, seals exactly once, and is never reusable.
    public static func seal(_ plaintext: Data, recipientPublicKey: Data, info: Data,
                            authenticating makeAAD: (Data) throws -> Data) throws -> Sealed {
        guard recipientPublicKey.count == 32 else { throw FTError.malformed("recipient.public_key") }
        guard plaintext.count <= FTLimit.envelope - 16, info.count <= maximumContextBytes else {
            throw FTError.limit("recipient.hpke_input")
        }
        try Task.checkCancellation()
        var sender: HPKE.Sender
        do {
            let recipient = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: recipientPublicKey)
            try requireContributoryAgreement(privateKey: .init(), publicKey: recipient)
            sender = try HPKE.Sender(recipientKey: recipient, ciphersuite: .Curve25519_SHA256_ChachaPoly, info: info)
        } catch { throw FTError.authentication }
        let encapsulated = sender.encapsulatedKey
        guard encapsulated.count == 32 else { throw FTError.authentication }
        try Task.checkCancellation()
        let aad = try makeAAD(encapsulated)
        guard aad.count <= maximumContextBytes else { throw FTError.limit("recipient.hpke_aad") }
        try Task.checkCancellation()
        let ciphertext: Data
        do { ciphertext = try sender.seal(plaintext, authenticating: aad) }
        catch { throw FTError.authentication }
        try Task.checkCancellation()
        guard ciphertext.count == plaintext.count + 16 else { throw FTError.authentication }
        return Sealed(encapsulatedKey: encapsulated, ciphertext: ciphertext)
    }

    /// A fresh recipient context always opens sequence zero once. Caller-owned
    /// session policy determines whether the ephemeral private key still exists;
    /// an unavailable key cannot cause a fallback to generated-code mode.
    public static func open(_ ciphertext: Data, privateKey: Data, encapsulatedKey: Data,
                            info: Data, authenticating aad: Data) throws -> Data {
        guard privateKey.count == 32, encapsulatedKey.count == 32 else {
            throw FTError.malformed("recipient.key_extent")
        }
        guard (16...FTLimit.envelope).contains(ciphertext.count), info.count <= maximumContextBytes,
              aad.count <= maximumContextBytes else { throw FTError.limit("recipient.hpke_input") }
        try Task.checkCancellation()
        var clear: Data
        do {
            let key = try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: privateKey)
            let peer = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: encapsulatedKey)
            try requireContributoryAgreement(privateKey: key, publicKey: peer)
            var recipient = try HPKE.Recipient(privateKey: key, ciphersuite: .Curve25519_SHA256_ChachaPoly,
                                               info: info, encapsulatedKey: encapsulatedKey)
            clear = try recipient.open(ciphertext, authenticating: aad)
        } catch { throw FTError.authentication }
        do {
            try Task.checkCancellation()
            return clear
        } catch {
            clear.resetBytes(in: 0..<clear.count)
            throw error
        }
    }

    /// RFC 9180 section 7.1.4 requires rejecting an all-zero X25519 agreement.
    /// Some CryptoKit-compatible implementations deliberately return that value
    /// rather than throwing. Perform the agreement with the maintained primitive
    /// and check all bytes before HPKE setup; no field arithmetic, key schedule,
    /// or encoding blocklist belongs in this adapter. The sender's temporary
    /// validation key is separate from HPKE's one-shot ephemeral key.
    private static func requireContributoryAgreement(privateKey: Curve25519.KeyAgreement.PrivateKey,
                                                     publicKey: Curve25519.KeyAgreement.PublicKey) throws {
        let shared = try privateKey.sharedSecretFromKeyAgreement(with: publicKey)
        let nonzero = shared.withUnsafeBytes { bytes in bytes.reduce(UInt8(0)) { $0 | $1 } }
        guard nonzero != 0 else { throw FTError.authentication }
    }
}
