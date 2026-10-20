import Foundation
import CZlib
#if canImport(CryptoKit)
import CryptoKit
import Security
#endif

/// Fixed SSH curve profiles. The protocol never accepts an arbitrary curve name
/// or asks a native primitive to infer the algorithm from untrusted key bytes.
public enum SSHECDSACurve: String, Sendable, CaseIterable {
    case nistp256, nistp384, nistp521
    public var algorithm: String { "ecdsa-sha2-" + rawValue }
    public var scalarBytes: Int {
        switch self { case .nistp256: return 32; case .nistp384: return 48; case .nistp521: return 66 }
    }
    var publicByteCount: Int { scalarBytes * 2 + 1 }
    var order: Data {
        get throws {
            // NIST SP 800-186 group orders, at the exact scalar width. These
            // bounds prevent a primitive from silently reducing an input mod n.
            let hex: String
            switch self {
            case .nistp256: hex = "FFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551"
            case .nistp384: hex = "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFC7634D81F4372DDF581A0DB248B0A77AECEC196ACCC52973"
            case .nistp521: hex = "01FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFA51868783BF2F966B7FCC0148F709A5D03BB5C9B8899C47AEBB6FB71E91386409"
            }
            return try Data(ftHex: hex)
        }
    }
    func validateScalar(_ scalar: Data) throws {
        guard scalar.count == scalarBytes, scalar.contains(where: { $0 != 0 }),
              try scalar.lexicographicallyPrecedes(order) else { throw FTError.malformed("key.ecdsa_scalar") }
    }
}

/// Trusted platform primitive boundary; no algorithm selection comes from JSON.
/// Apple products use AppleCryptography. Linux tests use an independent oracle.
public protocol AirGapCryptography: Sendable {
    func sha256(_ bytes: Data) -> Data
    func randomBytes(count: Int) throws -> Data
    func seal(_ plaintext: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data
    func open(_ ciphertextAndTag: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data
    func ed25519PublicKey(seed: Data) throws -> Data
    /// A fixed-width big-endian private scalar to an uncompressed SEC1 point.
    /// The maintained primitive must reject zero and out-of-range scalars.
    func ecdsaPublicKey(scalar: Data, curve: SSHECDSACurve) throws -> Data
    /// Bounded OpenSSH source-key work only; FTAG itself uses generated-code HKDF.
    func bcryptPBKDF(passphrase: Data, salt: Data, rounds: UInt32, outputByteCount: Int) throws -> Data
    /// AES-CTR has no authentication tag. The complete decrypted OpenSSH container
    /// and its derived public identity must validate before any key is accepted.
    func decryptAESCTR(_ ciphertext: Data, key: Data, iv: Data) throws -> Data
}

#if canImport(CryptoKit)
public struct AppleCryptography: AirGapCryptography {
    public init() {}
    public func sha256(_ bytes: Data) -> Data { Data(SHA256.hash(data: bytes)) }
    public func randomBytes(count: Int) throws -> Data {
        guard (1...64).contains(count) else { throw FTError.limit("random.length") }
        var bytes = Data(count: count)
        let status = bytes.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, count, $0.baseAddress!) }
        guard status == errSecSuccess else { throw FTError.storage }
        return bytes
    }
    private func key(secret: Data, salt: Data) -> SymmetricKey {
        HKDF<SHA256>.deriveKey(inputKeyMaterial: SymmetricKey(data: secret), salt: salt,
                              info: Data("FrankenTerm/FTAG/v1/code-key".utf8), outputByteCount: 32)
    }
    public func seal(_ plaintext: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data {
        let box = try ChaChaPoly.seal(plaintext, using: key(secret: secret, salt: salt),
                                     nonce: ChaChaPoly.Nonce(data: nonce), authenticating: aad)
        return box.ciphertext + box.tag
    }
    public func open(_ ciphertextAndTag: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data {
        guard ciphertextAndTag.count >= 16 else { throw FTError.authentication }
        do {
            let box = try ChaChaPoly.SealedBox(nonce: ChaChaPoly.Nonce(data: nonce),
                                               ciphertext: ciphertextAndTag.dropLast(16), tag: ciphertextAndTag.suffix(16))
            return try ChaChaPoly.open(box, using: key(secret: secret, salt: salt), authenticating: aad)
        } catch { throw FTError.authentication }
    }
    public func ed25519PublicKey(seed: Data) throws -> Data {
        do { return try Curve25519.Signing.PrivateKey(rawRepresentation: seed).publicKey.rawRepresentation }
        catch { throw FTError.malformed("key.ed25519") }
    }
    public func ecdsaPublicKey(scalar: Data, curve: SSHECDSACurve) throws -> Data {
        try curve.validateScalar(scalar)
        do {
            switch curve {
            case .nistp256: return try P256.Signing.PrivateKey(rawRepresentation: scalar).publicKey.x963Representation
            case .nistp384: return try P384.Signing.PrivateKey(rawRepresentation: scalar).publicKey.x963Representation
            case .nistp521: return try P521.Signing.PrivateKey(rawRepresentation: scalar).publicKey.x963Representation
            }
        } catch { throw FTError.malformed("key.ecdsa_scalar") }
    }
}
#endif

extension Data {
    public var ftHex: String { map { String(format: "%02X", $0) }.joined() }
    public init(ftHex text: String) throws {
        let bytes = Array(text.utf8)
        guard bytes.count.isMultiple(of: 2), bytes.count <= FTLimit.archive * 2 else { throw FTError.malformed("hex.length") }
        var result = Data()
        result.reserveCapacity(bytes.count / 2)
        func nibble(_ b: UInt8) throws -> UInt8 {
            switch b {
            case 48...57: return b - 48
            case 65...70: return b - 55
            case 97...102: return b - 87
            default: throw FTError.malformed("hex.character")
            }
        }
        for i in stride(from: 0, to: bytes.count, by: 2) { result.append(try (nibble(bytes[i]) << 4) | nibble(bytes[i + 1])) }
        self = result
    }
    mutating func appendBE(_ value: UInt64, bytes count: Int) {
        for shift in stride(from: (count - 1) * 8, through: 0, by: -8) { append(UInt8(truncatingIfNeeded: value >> shift)) }
    }
}

struct ByteCursor {
    let data: Data
    var offset = 0
    init(_ data: Data) { self.data = Data(data) }
    var remaining: Int { data.count - offset }
    mutating func take(_ count: Int) throws -> Data {
        guard count >= 0, count <= remaining else { throw FTError.malformed("binary.truncated") }
        let start = offset
        offset += count
        return data.subdata(in: start..<offset)
    }
    mutating func uint(_ count: Int) throws -> UInt64 {
        guard (1...8).contains(count), remaining >= count else { throw FTError.malformed("binary.integer") }
        var result: UInt64 = 0
        for _ in 0..<count { result = (result << 8) | UInt64(data[offset]); offset += 1 }
        return result
    }
    mutating func sshString(cap: Int = FTLimit.key) throws -> Data {
        let count = try uint(4)
        guard count <= UInt64(cap) else { throw FTError.limit("ssh.string") }
        return try take(Int(count))
    }
    /// RFC 4251 positive mpint, normalized only after checking its canonical
    /// signed representation. Validate the exact curve order before crypto too.
    mutating func sshScalar(curve: SSHECDSACurve) throws -> Data {
        let width = curve.scalarBytes
        let encoded = try sshString(cap: width + 1)
        guard let first = encoded.first, first & 0x80 == 0 else { throw FTError.malformed("key.scalar_sign") }
        let magnitude: Data
        if first == 0 {
            guard encoded.count > 1, encoded[1] & 0x80 != 0 else { throw FTError.malformed("key.scalar_noncanonical") }
            magnitude = Data(encoded.dropFirst())
        } else { magnitude = encoded }
        guard magnitude.count <= width else { throw FTError.limit("key.scalar_width") }
        let scalar = Data(repeating: 0, count: width - magnitude.count) + magnitude
        try curve.validateScalar(scalar)
        return scalar
    }
}

/// Deterministic restricted CBOR: shortest lengths, definite arrays/strings only.
struct CBORReader {
    var cursor: ByteCursor
    init(_ data: Data) { cursor = ByteCursor(data) }
    mutating func head(major: UInt8) throws -> UInt64 {
        let first = UInt8(try cursor.uint(1))
        guard first >> 5 == major else { throw FTError.malformed("cbor.type") }
        let extra = first & 31
        if extra < 24 { return UInt64(extra) }
        let width: Int, minimum: UInt64
        switch extra {
        case 24: width = 1; minimum = 24
        case 25: width = 2; minimum = 256
        case 26: width = 4; minimum = 65536
        case 27: width = 8; minimum = 4294967296
        default: throw FTError.malformed("cbor.indefinite_or_reserved")
        }
        let result = try cursor.uint(width)
        guard result >= minimum else { throw FTError.malformed("cbor.noncanonical") }
        return result
    }
    mutating func array(cap: Int) throws -> Int {
        let count = try head(major: 4)
        guard count <= UInt64(cap), count <= UInt64(cursor.remaining) else { throw FTError.limit("cbor.array") }
        return Int(count)
    }
    mutating func tuple(_ count: Int) throws {
        guard try array(cap: count) == count else { throw FTError.malformed("cbor.tuple") }
    }
    mutating func bytes(cap: Int) throws -> Data {
        let count = try head(major: 2)
        guard count <= UInt64(cap) else { throw FTError.limit("cbor.bytes") }
        return try cursor.take(Int(count))
    }
    mutating func text() throws -> String {
        let count = try head(major: 3)
        guard count <= FTLimit.text else { throw FTError.limit("cbor.text") }
        guard let value = String(data: try cursor.take(Int(count)), encoding: .utf8) else { throw FTError.malformed("cbor.utf8") }
        return value
    }
    func finish() throws { guard cursor.remaining == 0 else { throw FTError.malformed("cbor.trailing") } }
}

enum CBORWriter {
    static func head(_ major: UInt8, _ value: UInt64) -> Data {
        var result = Data()
        if value < 24 { result.append((major << 5) | UInt8(value)); return result }
        let width: Int, extra: UInt8
        if value <= 255 { width = 1; extra = 24 }
        else if value <= 65535 { width = 2; extra = 25 }
        else if value <= 4294967295 { width = 4; extra = 26 }
        else { width = 8; extra = 27 }
        result.append((major << 5) | extra)
        result.appendBE(value, bytes: width)
        return result
    }
    static func uint(_ value: UInt64) -> Data { head(0, value) }
    static func bytes(_ data: Data) -> Data { head(2, UInt64(data.count)) + data }
    static func text(_ text: String) -> Data { head(3, UInt64(text.utf8.count)) + Data(text.utf8) }
    static func array(_ count: Int) -> Data { head(4, UInt64(count)) }
}

public struct TransferCode: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let secret: Data
    private static let alphabet = Array("0123456789ABCDEFGHJKMNPQRSTVWXYZ".utf8)
    public var description: String { "[private transfer code]" }
    public var debugDescription: String { description }
    public init<C: AirGapCryptography>(using crypto: C) throws { secret = try crypto.randomBytes(count: 16) }
    init(secret: Data) throws {
        guard secret.count == 16 else { throw FTError.malformed("code.secret") }
        self.secret = Data(secret)
    }
    public init<C: AirGapCryptography>(entered text: String, using crypto: C) throws {
        guard text.utf8.count <= 128 else { throw FTError.limit("code.text") }
        var values: [UInt8] = []
        for var byte in text.utf8 {
            if byte == 32 || byte == 45 { continue }
            if (97...122).contains(byte) { byte -= 32 }
            if byte == 79 { byte = 48 }
            if byte == 73 || byte == 76 { byte = 49 }
            guard let index = Self.alphabet.firstIndex(of: byte) else { throw FTError.malformed("code.character") }
            values.append(UInt8(index))
        }
        guard values.count == 28, values[0] <= 7 else { throw FTError.malformed("code.length_or_top_bits") }
        var decoded = Data(repeating: 0, count: 16)
        for bit in 0..<128 {
            let sourceBit = bit + 2
            let value = (values[sourceBit / 5] >> (4 - sourceBit % 5)) & 1
            decoded[bit / 8] |= value << (7 - bit % 8)
        }
        let check = Self.checksum(decoded, using: crypto)
        guard values[26] == check.0, values[27] == check.1 else { throw FTError.checksum }
        secret = decoded
    }
    private static func checksum<C: AirGapCryptography>(_ secret: Data, using crypto: C) -> (UInt8, UInt8) {
        let hash = crypto.sha256(Data("FrankenTerm/FTAG/v1/code-check\0".utf8) + secret)
        return (hash[0] >> 3, ((hash[0] & 7) << 2) | (hash[1] >> 6))
    }
    public func reveal<C: AirGapCryptography>(using crypto: C) -> String {
        var encoded: [UInt8] = []
        for group in 0..<26 {
            var value: UInt8 = 0
            for j in 0..<5 {
                let bit = group * 5 + j - 2
                value <<= 1
                if bit >= 0 { value |= (secret[bit / 8] >> (7 - bit % 8)) & 1 }
            }
            encoded.append(Self.alphabet[Int(value)])
        }
        let check = Self.checksum(secret, using: crypto)
        encoded += [Self.alphabet[Int(check.0)], Self.alphabet[Int(check.1)]]
        return stride(from: 0, to: 28, by: 4).map { String(decoding: encoded[$0..<$0 + 4], as: UTF8.self) }.joined(separator: "-")
    }
}

public struct PrivateKeyRecord: Sendable, CustomDebugStringConvertible {
    public let credentialID: String
    public let container: Data
    public let publicBlob: Data
    public let certificates: [Data]
    public var debugDescription: String { "PrivateKeyRecord([redacted])" }
    public init(credentialID: String, container: Data, publicBlob: Data, certificates: [Data] = []) {
        self.credentialID = credentialID; self.container = container; self.publicBlob = publicBlob; self.certificates = certificates
    }
    public static func fromPEM<C: AirGapCryptography>(_ pem: Data, credentialID: String, using crypto: C) throws -> PrivateKeyRecord {
        guard pem.count <= FTLimit.key, let text = String(data: pem, encoding: .utf8) else { throw FTError.limit("key.pem") }
        let lines = text.trimmingCharacters(in: .whitespacesAndNewlines).components(separatedBy: .newlines).filter { !$0.isEmpty }
        guard lines.count >= 3, lines.first == "-----BEGIN OPENSSH PRIVATE KEY-----", lines.last == "-----END OPENSSH PRIVATE KEY-----",
              let container = Data(base64Encoded: lines.dropFirst().dropLast().joined()) else { throw FTError.malformed("key.pem") }
        let blob = try validateContainer(container, using: crypto)
        return PrivateKeyRecord(credentialID: credentialID, container: container, publicBlob: blob)
    }
    static func validateContainer<C: AirGapCryptography>(_ data: Data, using crypto: C) throws -> Data {
        guard data.count <= FTLimit.key else { throw FTError.limit("key.container") }
        var reader = ByteCursor(data)
        guard try reader.take(15) == Data("openssh-key-v1\0".utf8) else { throw FTError.malformed("key.magic") }
        guard try reader.sshString() == Data("none".utf8), try reader.sshString() == Data("none".utf8), try reader.sshString().isEmpty else {
            throw FTError.unsupported("key.encrypted_container")
        }
        guard try reader.uint(4) == 1 else { throw FTError.unsupported("key.count") }
        let publicBlob = try reader.sshString()
        let privatePart = try reader.sshString()
        guard reader.remaining == 0 else { throw FTError.malformed("key.extent") }
        _ = try validatePrivatePart(privatePart, publicBlob: publicBlob, blockSize: 8, using: crypto)
        return publicBlob
    }

    /// Source-file unlocking changes only the outer protection and padding.
    /// Validate the complete decrypted record against its real derived public
    /// identity before constructing the clear inner container for an FTAG archive.
    static func fromDecryptedOpenSSH<C: AirGapCryptography>(publicBlob: Data, privatePart: Data,
        credentialID: String, using crypto: C) throws -> PrivateKeyRecord {
        let bodyLength = try validatePrivatePart(privatePart, publicBlob: publicBlob, blockSize: 16, using: crypto)
        var body = Data(privatePart.prefix(bodyLength))
        defer { body.resetBytes(in: 0..<body.count) }
        var padding: UInt8 = 1
        while !body.count.isMultiple(of: 8) { body.append(padding); padding += 1 }
        var container = Data("openssh-key-v1\0".utf8)
        func appendString(_ bytes: Data) {
            container.appendBE(UInt64(bytes.count), bytes: 4); container += bytes
        }
        appendString(Data("none".utf8)); appendString(Data("none".utf8)); appendString(Data())
        container.appendBE(1, bytes: 4)
        appendString(publicBlob); appendString(body)
        guard container.count <= FTLimit.key else { throw FTError.limit("key.container") }
        return .init(credentialID: credentialID, container: container, publicBlob: publicBlob)
    }

    private static func validatePrivatePart<C: AirGapCryptography>(_ privatePart: Data, publicBlob: Data,
        blockSize: Int, using crypto: C) throws -> Int {
        guard [8, 16].contains(blockSize), !privatePart.isEmpty, privatePart.count <= FTLimit.key,
              privatePart.count.isMultiple(of: blockSize) else { throw FTError.malformed("key.extent") }
        var publicReader = ByteCursor(publicBlob), privateReader = ByteCursor(privatePart)
        let algorithmBytes = try publicReader.sshString(cap: 64)
        guard let algorithm = String(data: algorithmBytes, encoding: .ascii),
              algorithm == "ssh-ed25519" || SSHECDSACurve.allCases.contains(where: { $0.algorithm == algorithm }) else {
            throw FTError.unsupported("key.family")
        }
        guard try privateReader.uint(4) == privateReader.uint(4),
              try privateReader.sshString(cap: 64) == algorithmBytes else { throw FTError.malformed("key.private_header") }
        if algorithm == "ssh-ed25519" {
            let publicKey = try publicReader.sshString(cap: 32)
            guard publicKey.count == 32, publicReader.remaining == 0 else { throw FTError.malformed("key.public") }
            guard try privateReader.sshString(cap: 32) == publicKey else { throw FTError.malformed("key.private_header") }
            let privateKey = try privateReader.sshString(cap: 64)
            guard privateKey.count == 64, privateKey.suffix(32) == publicKey,
                  try crypto.ed25519PublicKey(seed: Data(privateKey.prefix(32))) == publicKey else { throw FTError.malformed("key.identity") }
        } else {
            guard let curve = SSHECDSACurve.allCases.first(where: { $0.algorithm == algorithm }) else { throw FTError.unsupported("key.family") }
            let curveName = Data(curve.rawValue.utf8)
            guard try publicReader.sshString(cap: 16) == curveName,
                  try privateReader.sshString(cap: 16) == curveName else { throw FTError.malformed("key.ecdsa_curve") }
            let publicKey = try publicReader.sshString(cap: curve.publicByteCount)
            guard publicKey.count == curve.publicByteCount, publicKey.first == 4, publicReader.remaining == 0,
                  try privateReader.sshString(cap: curve.publicByteCount) == publicKey else { throw FTError.malformed("key.public") }
            let scalar = try privateReader.sshScalar(curve: curve)
            guard try crypto.ecdsaPublicKey(scalar: scalar, curve: curve) == publicKey else { throw FTError.malformed("key.identity") }
        }
        _ = try privateReader.sshString(cap: FTLimit.text)
        let bodyLength = privateReader.offset
        guard privateReader.remaining < blockSize else { throw FTError.malformed("key.padding") }
        let padding = try privateReader.take(privateReader.remaining)
        guard padding.enumerated().allSatisfy({ $0.element == UInt8($0.offset + 1) }) else { throw FTError.malformed("key.padding") }
        return bodyLength
    }
}

public struct AirGapArchive: Sendable, CustomDebugStringConvertible {
    public struct Portability: Sendable, Equatable {
        public let entityID: String, setting: String, disposition: UInt64, reason: String
        public init(entityID: String, setting: String, disposition: UInt64, reason: String) {
            self.entityID = entityID; self.setting = setting; self.disposition = disposition; self.reason = reason
        }
    }
    public struct Annotation: Sendable, Equatable, CustomDebugStringConvertible {
        public let entityID: String, setting: String, original: Data
        public init(entityID: String, setting: String, original: Data) {
            self.entityID = entityID; self.setting = setting; self.original = original
        }
        public var debugDescription: String { "AirGapArchive.Annotation([redacted])" }
    }
    public let workspace: WorkspaceDocument
    public let keys: [PrivateKeyRecord]
    public let portability: [Portability]
    public let annotations: [Annotation]
    public var debugDescription: String { "AirGapArchive([redacted])" }

    public init<C: AirGapCryptography>(workspace: WorkspaceDocument, keys: [PrivateKeyRecord],
                                      portability: [Portability] = [], annotations: [Annotation] = [], using crypto: C) throws {
        let metadata = try Self.validatedMetadata(workspace: workspace, portability: portability, annotations: annotations)
        self.workspace = workspace; self.keys = keys
        self.portability = metadata.portability; self.annotations = metadata.annotations
        try validateKeys(using: crypto)
    }
    public init<C: AirGapCryptography>(data: Data, using crypto: C) throws {
        guard data.count <= FTLimit.archive else { throw FTError.limit("archive.bytes") }
        var reader = CBORReader(data)
        try reader.tuple(5)
        guard try reader.head(major: 0) == 1 else { throw FTError.unsupported("archive.version") }
        let workspace = try WorkspaceDocument(data: reader.bytes(cap: FTLimit.manifest))
        let keyCount = try reader.array(cap: 256)
        var keys: [PrivateKeyRecord] = []
        var certificateCount = 0, certificateBytes = 0
        for _ in 0..<keyCount {
            try reader.tuple(5)
            let id = try reader.text()
            guard try reader.head(major: 0) == 1 else { throw FTError.unsupported("key.format") }
            let container = try reader.bytes(cap: FTLimit.key), publicBlob = try reader.bytes(cap: FTLimit.key)
            let count = try reader.array(cap: 256)
            certificateCount += count
            guard certificateCount <= 256 else { throw FTError.limit("archive.certificates") }
            var certificates: [Data] = []
            for _ in 0..<count {
                let certificate = try reader.bytes(cap: FTLimit.key)
                certificateBytes += certificate.count
                guard certificateBytes <= 1024 * 1024 else { throw FTError.limit("archive.certificate_bytes") }
                certificates.append(certificate)
            }
            keys.append(PrivateKeyRecord(credentialID: id, container: container, publicBlob: publicBlob, certificates: certificates))
        }
        var portability: [Portability] = [], annotations: [Annotation] = []
        let portCount = try reader.array(cap: 1024)
        for _ in 0..<portCount {
            try reader.tuple(4)
            let id = try reader.text(), setting = try reader.text(), disposition = try reader.head(major: 0), reason = try reader.text()
            portability.append(Portability(entityID: id, setting: setting, disposition: disposition, reason: reason))
        }
        let annotationCount = try reader.array(cap: 1024)
        for _ in 0..<annotationCount {
            try reader.tuple(3)
            let id = try reader.text(), setting = try reader.text(), original = try reader.bytes(cap: FTLimit.text)
            annotations.append(Annotation(entityID: id, setting: setting, original: original))
        }
        try reader.finish()
        try self.init(workspace: workspace, keys: keys, portability: portability, annotations: annotations, using: crypto)
    }
    /// Archive annotations eventually enter the public configuration catalog.
    /// Only registered, typed public values may cross that boundary. Unknown
    /// opaque bytes are never retained, even though the FTAG envelope was valid.
    static func validatedMetadata(workspace: WorkspaceDocument, portability: [Portability], annotations: [Annotation]) throws
        -> (portability: [Portability], annotations: [Annotation]) {
        guard portability.count <= 1024, annotations.count <= 1024 else { throw FTError.limit("archive.metadata") }
        let entityIDs = workspace.entityIDs
        struct Reference: Hashable { let entityID: String, setting: String }
        func reference(_ entityID: String, _ setting: String) throws -> Reference {
            _ = try WorkspaceDocument.identifier(entityID)
            _ = try WorkspaceDocument.identifier(setting)
            guard entityIDs.contains(entityID) else { throw FTError.malformed("archive.metadata_reference") }
            return Reference(entityID: entityID, setting: setting)
        }
        // Reserved, content-free diagnostic remains stable across re-export.
        let omittedSetting = "frankenterm.discarded_annotations"
        let omittedReason = "Unsupported desktop annotations were omitted from public storage."
        var records = portability, conditions: [Reference: UInt64] = [:]
        var hasOmissionFinding = false
        for item in portability {
            let key = try reference(item.entityID, item.setting)
            guard conditions.updateValue(item.disposition, forKey: key) == nil else { throw FTError.malformed("archive.duplicate_portability") }
            guard item.disposition <= 1 else { throw FTError.malformed("archive.disposition") }
            guard !item.reason.isEmpty, item.reason.utf8.count <= FTLimit.text,
                  !item.reason.unicodeScalars.contains(where: { $0.value < 32 || (127...159).contains($0.value) })
            else { throw FTError.malformed("archive.portability_reason") }
            if item.setting == omittedSetting {
                guard item.entityID == workspace.id, item.disposition == 0, item.reason == omittedReason else {
                    throw FTError.malformed("archive.reserved_portability")
                }
                hasOmissionFinding = true
            }
        }
        var retained: [Annotation] = [], seen = Set<Reference>(), omitted = false
        for item in annotations {
            let key = try reference(item.entityID, item.setting)
            guard seen.insert(key).inserted else { throw FTError.malformed("archive.duplicate_annotation") }
            guard item.original.count <= FTLimit.text else { throw FTError.limit("annotation.bytes") }
            guard String(data: item.original, encoding: .utf8) != nil else { throw FTError.malformed("annotation.utf8") }
            let canonical: Data
            switch item.setting {
            // Actual desktop configuration fields. These remain inert: importing
            // a value never activates desktop configuration or executes code.
            case "enable_tab_bar", "hide_tab_bar_if_only_one_tab", "use_fancy_tab_bar":
                guard case .boolean(let value) = try StrictJSON.decode(item.original) else { throw FTError.malformed("annotation.boolean") }
                canonical = Data((value ? "true" : "false").utf8)
            case "window_background_opacity", "text_background_opacity":
                guard case .number(let text) = try StrictJSON.decode(item.original), let value = Double(text),
                      value.isFinite, (0...1).contains(value) else { throw FTError.malformed("annotation.opacity") }
                // Arbitrary whitespace and numeric spellings do not become an
                // opaque payload in the public catalog.
                canonical = try JSONEncoder().encode(value)
            default:
                guard conditions[key] != 1 else { throw FTError.unsupported("annotation.required_setting") }
                omitted = true
                continue
            }
            retained.append(Annotation(entityID: item.entityID, setting: item.setting, original: canonical))
        }
        if omitted && !hasOmissionFinding {
            guard records.count < 1024 else { throw FTError.limit("archive.metadata") }
            records.append(Portability(entityID: workspace.id, setting: omittedSetting, disposition: 0, reason: omittedReason))
        }
        return (records, retained)
    }
    private func validateKeys<C: AirGapCryptography>(using crypto: C) throws {
        guard keys.count <= 256 else { throw FTError.limit("archive.keys") }
        let references = Dictionary(uniqueKeysWithValues: workspace.credentials.map { ($0.id, $0) })
        var ids = Set<String>(), identities = Set<Data>()
        for key in keys {
            guard ids.insert(key.credentialID).inserted, let reference = references[key.credentialID] else { throw FTError.malformed("archive.key_reference") }
            guard key.certificates.isEmpty else { throw FTError.unsupported("key.user_certificate") }
            let blob = try PrivateKeyRecord.validateContainer(key.container, using: crypto)
            var publicReader = ByteCursor(blob)
            let algorithm = try publicReader.sshString(cap: 64)
            guard blob == key.publicBlob, identities.insert(blob).inserted, Data(reference.algorithm.utf8) == algorithm,
                  reference.fingerprint == "SHA256:" + crypto.sha256(blob).base64EncodedString().replacingOccurrences(of: "=", with: "")
            else { throw FTError.malformed("archive.key_binding") }
        }
    }
    public func encoded() throws -> Data {
        var data = CBORWriter.array(5) + CBORWriter.uint(1) + CBORWriter.bytes(workspace.raw) + CBORWriter.array(keys.count)
        for key in keys {
            data += CBORWriter.array(5) + CBORWriter.text(key.credentialID) + CBORWriter.uint(1)
                + CBORWriter.bytes(key.container) + CBORWriter.bytes(key.publicBlob) + CBORWriter.array(key.certificates.count)
            for certificate in key.certificates { data += CBORWriter.bytes(certificate) }
            guard data.count <= FTLimit.archive else { throw FTError.limit("archive.bytes") }
        }
        data += CBORWriter.array(portability.count)
        for item in portability {
            data += CBORWriter.array(4) + CBORWriter.text(item.entityID) + CBORWriter.text(item.setting)
                + CBORWriter.uint(item.disposition) + CBORWriter.text(item.reason)
            guard data.count <= FTLimit.archive else { throw FTError.limit("archive.bytes") }
        }
        data += CBORWriter.array(annotations.count)
        for item in annotations {
            data += CBORWriter.array(3) + CBORWriter.text(item.entityID) + CBORWriter.text(item.setting) + CBORWriter.bytes(item.original)
            guard data.count <= FTLimit.archive else { throw FTError.limit("archive.bytes") }
        }
        guard data.count <= FTLimit.archive else { throw FTError.limit("archive.bytes") }
        return data
    }
}

enum ZlibPacking {
    static func compress(_ input: Data) throws -> Data {
        guard input.count <= FTLimit.archive else { throw FTError.limit("zlib.input") }
        var length = compressBound(uLong(input.count))
        var output = Data(count: Int(length))
        let result = output.withUnsafeMutableBytes { out in
            input.withUnsafeBytes { source in
                compress2(out.bindMemory(to: UInt8.self).baseAddress, &length,
                          source.bindMemory(to: UInt8.self).baseAddress, uLong(input.count), Z_BEST_COMPRESSION)
            }
        }
        guard result == Z_OK else { throw FTError.malformed("zlib.compress") }
        output.count = Int(length)
        return output
    }
    static func expand(_ input: Data, expected: Int) throws -> Data {
        guard expected > 0, expected <= FTLimit.archive, input.count <= FTLimit.envelope else { throw FTError.limit("zlib.extent") }
        var output = Data(count: expected + 1), stream = z_stream()
        guard inflateInit_(&stream, ZLIB_VERSION, Int32(MemoryLayout<z_stream>.size)) == Z_OK else { throw FTError.malformed("zlib.init") }
        defer { inflateEnd(&stream) }
        let result = output.withUnsafeMutableBytes { out in
            input.withUnsafeBytes { source in
                // zlib's historical API is mutable; inflate does not mutate next_in.
                stream.next_in = UnsafeMutablePointer(mutating: source.bindMemory(to: UInt8.self).baseAddress)
                stream.avail_in = uInt(input.count)
                stream.next_out = out.bindMemory(to: UInt8.self).baseAddress
                stream.avail_out = uInt(expected + 1)
                return inflate(&stream, Z_FINISH)
            }
        }
        guard result == Z_STREAM_END, stream.total_out == expected, stream.avail_in == 0 else { throw FTError.malformed("zlib.stream") }
        output.count = expected
        return output
    }
}

public struct FTAGHeader: Sendable, Equatable {
    public let transferID: Data, header: Data, modeHeader: Data
    public let ciphertextLength: Int
    public init(envelope: Data) throws {
        guard envelope.count <= FTLimit.envelope else { throw FTError.limit("envelope.bytes") }
        var reader = ByteCursor(envelope)
        guard try reader.take(4) == Data("FTAG".utf8), try reader.uint(1) == 1 else { throw FTError.unsupported("envelope.version") }
        let mode = try reader.uint(1)
        guard mode == 1 else { throw FTError.unsupported("envelope.mode") }
        guard try reader.uint(2) == 0 else { throw FTError.malformed("envelope.flags") }
        transferID = try reader.take(16)
        let count = try reader.uint(4), modeLength = try reader.uint(2)
        guard modeLength == 28, count >= 4112, count <= FTLimit.envelope,
              (count - 16).isMultiple(of: 4096), UInt64(reader.remaining) == modeLength + count else { throw FTError.malformed("envelope.extent") }
        ciphertextLength = Int(count)
        modeHeader = try reader.take(28)
        header = envelope.prefix(58)
    }
    var aad: Data { Data("FrankenTerm/FTAG/v1/envelope\0".utf8) + header }
}

public enum FTAG {
    public struct Export: Sendable, CustomDebugStringConvertible {
        public let envelope: Data, code: TransferCode
        public var debugDescription: String { "FTAG.Export([encrypted], [code redacted])" }
    }
    public static func seal<C: AirGapCryptography>(_ archive: AirGapArchive, using crypto: C) throws -> Export {
        let code = try TransferCode(using: crypto)
        let envelope = try sealArchiveBytes(archive.encoded(), code: code, transferID: crypto.randomBytes(count: 16),
                                           salt: crypto.randomBytes(count: 16), nonce: crypto.randomBytes(count: 12), using: crypto)
        return Export(envelope: envelope, code: code)
    }
    public static func open<C: AirGapCryptography>(_ envelope: Data, code: TransferCode, using crypto: C) throws -> AirGapArchive {
        try AirGapArchive(data: openArchiveBytes(envelope, code: code, using: crypto), using: crypto)
    }
    public static func comparison<C: AirGapCryptography>(_ envelope: Data, using crypto: C) -> String {
        let bytes = Array(crypto.sha256(envelope).ftHex.prefix(20).utf8)
        return stride(from: 0, to: bytes.count, by: 4).map { String(decoding: bytes[$0..<$0 + 4], as: UTF8.self) }.joined(separator: "-")
    }
    static func pack(_ archive: Data, compress: Bool = true) throws -> Data {
        guard !archive.isEmpty, archive.count <= FTLimit.archive else { throw FTError.limit("archive.bytes") }
        let compressed = compress ? try ZlibPacking.compress(archive) : archive
        let useCompressed = compressed.count < archive.count
        let payload = useCompressed ? compressed : archive
        let packedLength = ((9 + payload.count + 4095) / 4096) * 4096
        guard packedLength + 74 <= FTLimit.envelope else { throw FTError.limit("envelope.bytes") }
        var data = Data([useCompressed ? 1 : 0])
        data.appendBE(UInt64(archive.count), bytes: 4); data.appendBE(UInt64(payload.count), bytes: 4)
        data += payload
        data += Data(repeating: 0, count: packedLength - data.count)
        return data
    }
    static func unpackAuthenticated(_ packed: Data) throws -> Data {
        guard packed.count > 0, packed.count <= FTLimit.envelope, packed.count.isMultiple(of: 4096) else { throw FTError.malformed("packing.extent") }
        var reader = ByteCursor(packed)
        let codec = try reader.uint(1), archiveLength = try reader.uint(4), payloadLength = try reader.uint(4)
        guard archiveLength > 0, archiveLength <= FTLimit.archive, payloadLength > 0,
              payloadLength <= UInt64(reader.remaining) else { throw FTError.limit("packing.length") }
        guard packed.count == ((9 + Int(payloadLength) + 4095) / 4096) * 4096 else { throw FTError.malformed("packing.excess_padding") }
        let payload = try reader.take(Int(payloadLength)), padding = try reader.take(reader.remaining)
        guard padding.allSatisfy({ $0 == 0 }) else { throw FTError.malformed("packing.padding") }
        switch codec {
        case 0:
            guard archiveLength == payloadLength else { throw FTError.malformed("packing.raw_length") }
            return payload
        case 1: return try ZlibPacking.expand(payload, expected: Int(archiveLength))
        default: throw FTError.unsupported("packing.codec")
        }
    }
    static func sealArchiveBytes<C: AirGapCryptography>(_ archive: Data, code: TransferCode, transferID: Data,
                                                       salt: Data, nonce: Data, compress: Bool = true, using crypto: C) throws -> Data {
        guard transferID.count == 16, salt.count == 16, nonce.count == 12 else { throw FTError.malformed("envelope.random_lengths") }
        let packed = try pack(archive, compress: compress)
        var header = Data("FTAG".utf8) + Data([1, 1, 0, 0]) + transferID
        header.appendBE(UInt64(packed.count + 16), bytes: 4); header.appendBE(28, bytes: 2)
        header += salt + nonce
        let ciphertext = try crypto.seal(packed, secret: code.secret, salt: salt, nonce: nonce,
                                         aad: Data("FrankenTerm/FTAG/v1/envelope\0".utf8) + header)
        guard ciphertext.count == packed.count + 16 else { throw FTError.malformed("crypto.sealed_length") }
        return header + ciphertext
    }
    static func openArchiveBytes<C: AirGapCryptography>(_ envelope: Data, code: TransferCode, using crypto: C) throws -> Data {
        let header = try FTAGHeader(envelope: envelope)
        let packed = try crypto.open(Data(envelope.suffix(header.ciphertextLength)), secret: code.secret,
                                     salt: Data(header.modeHeader.prefix(16)), nonce: Data(header.modeHeader.suffix(12)), aad: header.aad)
        return try unpackAuthenticated(packed)
    }
}
