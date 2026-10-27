import Foundation
import CryptoExtras
#if canImport(CryptoKit)
import CryptoKit
#else
import Crypto
#endif

/// A fully parsed OpenSSH v01 user certificate with a verified CA signature.
/// This establishes its contents, not trust in a host or permission to use a
/// private key. The reviewed workspace binds the exact certificate to its raw
/// subject; the remote SSH server independently decides which issuers it trusts.
public struct SSHUserCertificate: Equatable, Sendable, CustomDebugStringConvertible {
    /// Leaves room for the username, authentication fields and largest admitted
    /// signature inside the pinned libssh2 transport's 34,744-byte payload bound.
    public static let maximumBlobBytes = 32 * 1024
    public static let capability = "ssh_user_certificate_v1"
    public let blob: Data, algorithm: String, fingerprint: String
    public let subject: SSHPublicIdentity, authority: SSHPublicIdentity
    public let serial: UInt64, validAfter: UInt64, validBefore: UInt64
    public let principals: [String], criticalOptions: [String: Data], extensions: Set<String>
    public var debugDescription: String { "SSHUserCertificate([redacted])" }

    public init(blob: Data) throws {
        guard !blob.isEmpty, blob.count <= Self.maximumBlobBytes else { throw FTError.limit("certificate.bytes") }
        var reader = ByteCursor(blob)
        let algorithm = try Self.name(reader.sshString(cap: 64))
        let rawAlgorithm: String
        if algorithm == "ssh-ed25519-cert-v01@openssh.com" { rawAlgorithm = "ssh-ed25519" }
        else if algorithm == "ssh-rsa-cert-v01@openssh.com" { rawAlgorithm = "ssh-rsa" }
        else if let curve = SSHECDSACurve.allCases.first(where: { algorithm == $0.algorithm + "-cert-v01@openssh.com" }) {
            rawAlgorithm = curve.algorithm
        } else { throw FTError.unsupported("certificate.algorithm") }
        _ = try reader.sshString(cap: Self.maximumBlobBytes) // nonce, covered by the CA signature
        var subjectBlob = SSHUserAuthenticationChallenge.string(Data(rawAlgorithm.utf8))
        if rawAlgorithm == "ssh-ed25519" {
            subjectBlob += SSHUserAuthenticationChallenge.string(try reader.sshString(cap: 32))
        } else if rawAlgorithm == "ssh-rsa" {
            subjectBlob += SSHUserAuthenticationChallenge.string(try reader.sshString(cap: 9))
            subjectBlob += SSHUserAuthenticationChallenge.string(try reader.sshString(cap: 1025))
        } else {
            subjectBlob += SSHUserAuthenticationChallenge.string(try reader.sshString(cap: 16))
            subjectBlob += SSHUserAuthenticationChallenge.string(try reader.sshString(cap: 133))
        }
        let subject = try SSHPublicIdentity(blob: subjectBlob)
        let serial = try reader.uint(8)
        guard try reader.uint(4) == 1 else { throw FTError.unsupported("certificate.type") }
        _ = try reader.sshString(cap: 1024) // key ID is opaque signed metadata, never a trusted UI label
        var principalBytes = ByteCursor(try reader.sshString(cap: Self.maximumBlobBytes))
        var principals: [String] = [], seen = Set<Data>()
        while principalBytes.remaining > 0 {
            guard principals.count < 128 else { throw FTError.limit("certificate.principals") }
            let bytes = try principalBytes.sshString(cap: 256)
            guard let value = String(data: bytes, encoding: .utf8), !value.isEmpty,
                  !value.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }),
                  seen.insert(bytes).inserted else { throw FTError.malformed("certificate.principal") }
            principals.append(value)
        }
        let validAfter = try reader.uint(8), validBefore = try reader.uint(8)
        guard validAfter < validBefore else { throw FTError.malformed("certificate.validity") }
        let criticalOptions = try Self.options(reader.sshString(cap: Self.maximumBlobBytes))
        let extensionValues = try Self.options(reader.sshString(cap: Self.maximumBlobBytes))
        let flags: Set<String> = ["permit-X11-forwarding", "permit-agent-forwarding", "permit-port-forwarding",
                                  "permit-pty", "permit-user-rc", "no-touch-required"]
        for (name, value) in extensionValues where flags.contains(name) {
            guard value.isEmpty else { throw FTError.malformed("certificate.extension") }
        }
        // Future reserved data cannot silently acquire policy meaning here.
        guard try reader.sshString(cap: Self.maximumBlobBytes).isEmpty else { throw FTError.unsupported("certificate.reserved") }
        let authority = try SSHPublicIdentity(blob: reader.sshString(cap: SSHPublicIdentity.maximumBlobBytes))
        let signedPrefix = Data(blob.prefix(reader.offset))
        let signature = try reader.sshString(cap: 1044)
        guard reader.remaining == 0 else { throw FTError.malformed("certificate.trailing_bytes") }
        try Self.verify(signature: signature, message: signedPrefix, authority: authority)
        self.blob = blob; self.algorithm = algorithm; self.subject = subject; self.authority = authority
        self.serial = serial; self.validAfter = validAfter; self.validBefore = validBefore
        self.principals = principals; self.criticalOptions = criticalOptions; self.extensions = Set(extensionValues.keys)
        fingerprint = "SHA256:" + Data(SHA256.hash(data: blob)).base64EncodedString().replacingOccurrences(of: "=", with: "")
    }

    /// Time is checked for new offers, signatures and authentication completion.
    /// Established sessions retain their revision/revocation checks separately.
    /// Empty principals mean the protocol's wildcard; alternate server-side
    /// AuthorizedPrincipals mappings are never guessed from a display label.
    public func validateAuthentication(user: String, now: Date = Date(), requiresPTY: Bool = false,
                                       requiresPortForwarding: Bool = false) throws {
        try validateRestrictions(user: user, requiresPTY: requiresPTY, requiresPortForwarding: requiresPortForwarding)
        let seconds = now.timeIntervalSince1970
        guard seconds.isFinite, seconds >= 0, seconds < Double(UInt64.max) else { throw FTError.malformed("certificate.clock") }
        let instant = UInt64(seconds.rounded(.down))
        guard instant >= validAfter, instant < validBefore else { throw FTError.unsupported("certificate.validity") }
    }

    /// Static restrictions remain true after authentication; only wall-clock
    /// expiry ceases to be a reason to end an already established connection.
    func validateRestrictions(user: String, requiresPTY: Bool, requiresPortForwarding: Bool) throws {
        guard criticalOptions.isEmpty else { throw FTError.unsupported("certificate.critical_options") }
        guard principals.isEmpty || principals.contains(where: { $0.utf8.elementsEqual(user.utf8) }) else {
            throw FTError.unsupported("certificate.principal")
        }
        guard !requiresPTY || extensions.contains("permit-pty") else { throw FTError.unsupported("certificate.pty") }
        guard !requiresPortForwarding || extensions.contains("permit-port-forwarding") else {
            throw FTError.unsupported("certificate.port_forwarding")
        }
    }

    /// Decode only a bounded algorithm name. Used to select the raw/certificate
    /// parser before any certificate is allowed to become a signing offer.
    static func algorithmName(in blob: Data) throws -> String {
        guard !blob.isEmpty, blob.count <= maximumBlobBytes else { throw FTError.limit("certificate.bytes") }
        var reader = ByteCursor(blob)
        return try name(reader.sshString(cap: 64))
    }

    private static func name(_ bytes: Data) throws -> String {
        guard !bytes.isEmpty, bytes.count <= 128,
              bytes.allSatisfy({ (33...126).contains($0) }), let text = String(data: bytes, encoding: .ascii) else {
            throw FTError.malformed("certificate.name")
        }
        return text
    }

    private static func options(_ bytes: Data) throws -> [String: Data] {
        var reader = ByteCursor(bytes), result: [String: Data] = [:], previous: String?
        while reader.remaining > 0 {
            guard result.count < 128 else { throw FTError.limit("certificate.options") }
            let key = try name(reader.sshString(cap: 128))
            guard previous.map({ $0.utf8.lexicographicallyPrecedes(key.utf8) }) ?? true else {
                throw FTError.malformed("certificate.option_order")
            }
            result[key] = try reader.sshString(cap: maximumBlobBytes)
            previous = key
        }
        return result
    }

    private static func magnitude(_ encoded: Data, maximumBytes: Int) throws -> Data {
        guard let first = encoded.first, first & 0x80 == 0 else { throw FTError.malformed("certificate.signature_integer") }
        let value: Data
        if first == 0 {
            guard encoded.count > 1, encoded[encoded.startIndex + 1] & 0x80 != 0 else {
                throw FTError.malformed("certificate.signature_integer")
            }
            value = Data(encoded.dropFirst())
        } else { value = encoded }
        guard value.count <= maximumBytes else { throw FTError.limit("certificate.signature_integer") }
        return value
    }

    private static func verify(signature: Data, message: Data, authority: SSHPublicIdentity) throws {
        var signatureReader = ByteCursor(signature), keyReader = ByteCursor(authority.blob)
        let algorithm = try name(signatureReader.sshString(cap: 64))
        let payload = try signatureReader.sshString(cap: 1024)
        guard signatureReader.remaining == 0 else { throw FTError.malformed("certificate.signature_extent") }
        _ = try keyReader.sshString(cap: 64)
        let valid: Bool
        if authority.algorithm == "ssh-ed25519" {
            guard algorithm == authority.algorithm, payload.count == 64 else { throw FTError.malformed("certificate.signature_algorithm") }
            let key = try Curve25519.Signing.PublicKey(rawRepresentation: keyReader.sshString(cap: 32))
            valid = key.isValidSignature(payload, for: message)
        } else if authority.algorithm == "ssh-rsa" {
            guard algorithm == "rsa-sha2-256" || algorithm == "rsa-sha2-512" else {
                throw FTError.unsupported("certificate.signature_algorithm")
            }
            let exponent = try magnitude(keyReader.sshString(cap: 9), maximumBytes: 8)
            let modulus = try magnitude(keyReader.sshString(cap: 1025), maximumBytes: 1024)
            guard !payload.isEmpty, payload.count <= modulus.count else { throw FTError.malformed("certificate.signature_size") }
            let key = try _RSA.Signing.PublicKey(n: modulus, e: exponent)
            let signature = _RSA.Signing.RSASignature(rawRepresentation: Data(repeating: 0, count: modulus.count - payload.count) + payload)
            if algorithm == "rsa-sha2-256" {
                valid = key.isValidSignature(signature, for: SHA256.hash(data: message), padding: .insecurePKCS1v1_5)
            } else {
                valid = key.isValidSignature(signature, for: SHA512.hash(data: message), padding: .insecurePKCS1v1_5)
            }
        } else {
            guard algorithm == authority.algorithm,
                  let curve = SSHECDSACurve.allCases.first(where: { $0.algorithm == algorithm }) else {
                throw FTError.malformed("certificate.signature_algorithm")
            }
            _ = try keyReader.sshString(cap: 16)
            let point = try keyReader.sshString(cap: curve.publicByteCount)
            var components = ByteCursor(payload)
            let r = try magnitude(components.sshString(cap: curve.scalarBytes + 1), maximumBytes: curve.scalarBytes)
            let s = try magnitude(components.sshString(cap: curve.scalarBytes + 1), maximumBytes: curve.scalarBytes)
            guard components.remaining == 0 else { throw FTError.malformed("certificate.signature_extent") }
            let raw = Data(repeating: 0, count: curve.scalarBytes - r.count) + r
                + Data(repeating: 0, count: curve.scalarBytes - s.count) + s
            switch curve {
            case .nistp256:
                valid = try P256.Signing.PublicKey(x963Representation: point).isValidSignature(
                    P256.Signing.ECDSASignature(rawRepresentation: raw), for: message)
            case .nistp384:
                valid = try P384.Signing.PublicKey(x963Representation: point).isValidSignature(
                    P384.Signing.ECDSASignature(rawRepresentation: raw), for: message)
            case .nistp521:
                valid = try P521.Signing.PublicKey(x963Representation: point).isValidSignature(
                    P521.Signing.ECDSASignature(rawRepresentation: raw), for: message)
            }
        }
        guard valid else { throw FTError.malformed("certificate.signature") }
    }
}
