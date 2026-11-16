import Foundation

/// Public, ephemeral destination request. The exact canonical CBOR bytes are
/// the HPKE transcript; a request grants no trust in the source of an archive.
public struct RecipientRequest: Sendable, Equatable {
    public static let maximumTextBytes = 4096
    public let requestID: Data
    public let publicKey: Data
    public let maximumEnvelopeBytes: Int
    public let encoded: Data

    public var text: String { "FTREQ1:UR:BYTES/" + Bytewords.encode(CBORWriter.bytes(encoded)) }

    public init(text: String) throws {
        // Admit only the explicit, single-part optical request syntax. Never
        // normalize case or reinterpret a terminal command or an ordinary URL.
        let prefix = "FTREQ1:UR:BYTES/"
        guard text.utf8.count <= Self.maximumTextBytes, text.hasPrefix(prefix) else { throw FTError.malformed("recipient.request_text") }
        let encodedUR = try Bytewords.decode(text.dropFirst(prefix.count))
        var outer = CBORReader(encodedUR)
        let request = try outer.bytes(cap: 4096)
        try outer.finish()
        try self.init(encoded: request)
    }

    public init(encoded: Data) throws {
        guard encoded.count <= 4096 else { throw FTError.limit("recipient.request_bytes") }
        var reader = CBORReader(encoded)
        try reader.tuple(4)
        guard try reader.head(major: 0) == 1 else { throw FTError.unsupported("recipient.request_version") }
        let requestID = try reader.bytes(cap: 16), publicKey = try reader.bytes(cap: 32)
        let maximum = try reader.head(major: 0)
        try reader.finish()
        guard requestID.count == 16, publicKey.count == 32,
              maximum >= UInt64(FTAGMode.recipient.headerLength + 4096 + 16),
              maximum <= UInt64(FTLimit.envelope) else { throw FTError.malformed("recipient.request_fields") }
        self.requestID = requestID; self.publicKey = publicKey
        maximumEnvelopeBytes = Int(maximum); self.encoded = encoded
    }

    init(requestID: Data, publicKey: Data, maximumEnvelopeBytes: Int) throws {
        guard maximumEnvelopeBytes >= 0 else { throw FTError.limit("recipient.envelope_limit") }
        try self.init(encoded: CBORWriter.array(4) + CBORWriter.uint(1) + CBORWriter.bytes(requestID)
            + CBORWriter.bytes(publicKey) + CBORWriter.uint(UInt64(maximumEnvelopeBytes)))
    }

    public func comparison<C: AirGapCryptography>(using crypto: C) -> String {
        FTAG.comparison(encoded, using: crypto)
    }

    public func digest<C: AirGapCryptography>(using crypto: C) -> Data { crypto.sha256(encoded) }
    func info<C: AirGapCryptography>(using crypto: C) -> Data {
        Data("FrankenTerm/FTAG/v1/recipient\0".utf8) + digest(using: crypto)
    }
}

/// Owned only by the current receive session. This key is never a workspace
/// credential, is never persisted, and is never included in a status or review.
/// Releasing the session drops its owned key state; it is not a promise that
/// every copy made by the OS or cryptographic implementation has been zeroed.
public struct RecipientSession: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    public let request: RecipientRequest
    fileprivate var privateKey: Data
    public var description: String { "RecipientSession([private key redacted])" }
    public var debugDescription: String { description }

    public init<C: RecipientCryptography>(maximumEnvelopeBytes: Int = FTLimit.envelope, using crypto: C) throws {
        try self.init(privateKey: crypto.randomBytes(count: 32), requestID: crypto.randomBytes(count: 16),
                      maximumEnvelopeBytes: maximumEnvelopeBytes, using: crypto)
    }

    init<C: RecipientCryptography>(privateKey: Data, requestID: Data, maximumEnvelopeBytes: Int = FTLimit.envelope,
                                   using crypto: C) throws {
        guard privateKey.count == 32 else { throw FTError.malformed("recipient.private_key") }
        request = try RecipientRequest(requestID: requestID, publicKey: crypto.x25519PublicKey(privateKey: privateKey),
                                       maximumEnvelopeBytes: maximumEnvelopeBytes)
        self.privateKey = privateKey
    }

    /// Clears this owned value where Foundation permits. Other copies, including
    /// those made by a cryptographic implementation, have their own lifetimes.
    mutating func retire() {
        privateKey.resetBytes(in: 0..<privateKey.count)
        privateKey = Data()
    }
}

extension FTAG {
    /// The caller confirms the request comparison on the intended phone before
    /// exporting private material. Base-mode HPKE does not authenticate a Mac.
    /// Native export flows use the confirmedRequestDigest overload to bind their
    /// explicit approval to the exact request displayed by the confirmation UI.
    public static func seal<C: RecipientCryptography>(_ archive: AirGapArchive, recipient: RecipientRequest,
                                                       using crypto: C) throws -> Data {
        try sealArchiveBytes(archive.encoded(), recipient: recipient,
                             transferID: crypto.randomBytes(count: 16), using: crypto)
    }

    /// Check the complete 32-byte request digest before serializing private
    /// material or setting up a sender. The displayed abbreviation alone cannot
    /// authorize a different request or a later asynchronous scanner result.
    public static func seal<C: RecipientCryptography>(_ archive: AirGapArchive, recipient: RecipientRequest,
        confirmedRequestDigest: Data, using crypto: C) throws -> Data {
        try Task.checkCancellation()
        let digest = recipient.digest(using: crypto)
        guard digest.count == 32, confirmedRequestDigest.count == 32, confirmedRequestDigest == digest else {
            throw FTError.unsupported("recipient.confirmation_required")
        }
        return try seal(archive, recipient: recipient, using: crypto)
    }

    public static func open<C: RecipientCryptography>(_ envelope: Data, recipient: RecipientSession,
                                                       using crypto: C) throws -> AirGapArchive {
        try AirGapArchive(data: openArchiveBytes(envelope, recipient: recipient, using: crypto), using: crypto)
    }

    static func sealArchiveBytes<C: RecipientCryptography>(_ archive: Data, recipient: RecipientRequest,
                                                          transferID: Data, compress: Bool = true, using crypto: C) throws -> Data {
        try Task.checkCancellation()
        guard transferID.count == 16 else { throw FTError.malformed("envelope.random_lengths") }
        var packed = try pack(archive, compress: compress, mode: .recipient)
        defer { packed.resetBytes(in: 0..<packed.count) }
        guard packed.count + FTAGMode.recipient.headerLength + 16 <= recipient.maximumEnvelopeBytes else {
            throw FTError.limit("recipient.envelope_limit")
        }
        var prefix = Data("FTAG".utf8) + Data([1, FTAGMode.recipient.rawValue, 0, 0]) + transferID
        prefix.appendBE(UInt64(packed.count + 16), bytes: 4)
        prefix.appendBE(UInt64(FTAGMode.recipient.modeHeaderLength), bytes: 2)
        prefix += recipient.requestID + recipient.digest(using: crypto)
        // The primitive owns setup, encapsulation, nonce derivation, and exactly
        // one seal. enc is the final header field and is authenticated verbatim.
        let sealed = try crypto.hpkeSeal(packed, recipientPublicKey: recipient.publicKey, info: recipient.info(using: crypto),
                                         aadPrefix: Data("FrankenTerm/FTAG/v1/envelope\0".utf8) + prefix)
        guard sealed.encapsulatedKey.count == 32, sealed.ciphertext.count == packed.count + 16 else {
            throw FTError.malformed("crypto.sealed_length")
        }
        try Task.checkCancellation()
        return prefix + sealed.encapsulatedKey + sealed.ciphertext
    }

    static func openArchiveBytes<C: RecipientCryptography>(_ envelope: Data, recipient: RecipientSession,
                                                          using crypto: C) throws -> Data {
        try Task.checkCancellation()
        let header = try FTAGHeader(envelope: envelope)
        guard header.mode == .recipient else { throw FTError.unsupported("envelope.requires_transfer_code") }
        guard envelope.count <= recipient.request.maximumEnvelopeBytes else { throw FTError.limit("recipient.envelope_limit") }
        var reader = ByteCursor(header.modeHeader)
        let requestID = try reader.take(16), requestDigest = try reader.take(32), encapsulatedKey = try reader.take(32)
        guard requestID == recipient.request.requestID, requestDigest == recipient.request.digest(using: crypto) else {
            throw FTError.authentication
        }
        var packed = try crypto.hpkeOpen(Data(envelope.suffix(header.ciphertextLength)), recipientPrivateKey: recipient.privateKey,
                                         encapsulatedKey: encapsulatedKey, info: recipient.request.info(using: crypto), aad: header.aad)
        defer { packed.resetBytes(in: 0..<packed.count) }
        try Task.checkCancellation()
        return try unpackAuthenticated(packed)
    }
}
