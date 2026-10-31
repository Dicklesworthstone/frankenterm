import Foundation

/// A bounded OpenSSH host certificate whose exact signed contents and issuer
/// signature have been verified. This value does not trust its issuer: the
/// reviewed route must separately authorize the CA and reject revoked identities.
public struct SSHHostCertificate: Equatable, Sendable, CustomDebugStringConvertible {
    public static let maximumBlobBytes = SSHCertificateFields.maximumBlobBytes
    public static let supportedAuthorityAlgorithms: Set<String> = [
        "ssh-ed25519", "ssh-rsa", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521"
    ]
    public static let supportedAlgorithms: Set<String> = Set(
        supportedAuthorityAlgorithms.map { $0 + "-cert-v01@openssh.com" })

    public let blob: Data, algorithm: String, fingerprint: String
    public let subject: SSHPublicIdentity, authority: SSHPublicIdentity
    /// The CA's signature envelope algorithm, independent of the host key's
    /// negotiated key-exchange signature algorithm.
    public let signatureAlgorithm: String
    public let serial: UInt64, validAfter: UInt64, validBefore: UInt64
    public let principals: [String], criticalOptions: [String: Data], extensionOptions: [String: Data]
    public var debugDescription: String { "SSHHostCertificate([redacted])" }

    public init(blob: Data) throws {
        let parsed = try SSHCertificateFields(blob: blob, kind: .host)
        guard !parsed.principals.isEmpty else { throw FTError.unsupported("certificate.principal") }
        guard parsed.criticalOptions.isEmpty else { throw FTError.unsupported("certificate.critical_options") }
        self.blob = parsed.blob; algorithm = parsed.algorithm; fingerprint = parsed.fingerprint
        subject = parsed.subject; authority = parsed.authority; signatureAlgorithm = parsed.signatureAlgorithm
        serial = parsed.serial; validAfter = parsed.validAfter; validBefore = parsed.validBefore
        principals = parsed.principals; criticalOptions = parsed.criticalOptions; extensionOptions = parsed.extensionOptions
    }

    /// Format compatibility only. The native transport must also verify the
    /// exchange signature with the exact certified subject and exchange hash.
    public func permitsHostSignature(_ negotiated: String) -> Bool {
        if subject.algorithm == "ssh-rsa" {
            return negotiated == "rsa-sha2-256-cert-v01@openssh.com"
                || negotiated == "rsa-sha2-512-cert-v01@openssh.com"
        }
        return negotiated == algorithm
    }

    /// `host` is the route's hostname or explicit HostKeyAlias, never a display
    /// label. Match current OpenSSH host-principal semantics: case-sensitive
    /// byte matching with only '*' and '?' wildcards. Empty lists authorize no
    /// host. Nil skips only the clock for an already-established session grant.
    public func validate(host: String, now: Date? = Date()) throws {
        guard !host.isEmpty, host.utf8.count <= 256,
              !host.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }),
              !principals.isEmpty else { throw FTError.unsupported("certificate.principal") }
        guard criticalOptions.isEmpty else { throw FTError.unsupported("certificate.critical_options") }
        let name = Array(host.utf8)
        guard principals.contains(where: { Self.matches(name, pattern: Array($0.utf8)) }) else {
            throw FTError.unsupported("certificate.principal")
        }
        if let now { try validateValidity(now: now) }
    }

    public func validateValidity(now: Date = Date()) throws {
        try SSHCertificateFields.validateValidity(validAfter: validAfter, validBefore: validBefore, now: now)
    }

    /// Dynamic programming bounds work by the admitted 256-byte hostname and
    /// principal, with one row of state. Repeated stars never recurse/backtrack.
    /// Negation, character classes, escaping and comma lists are literal here,
    /// matching OpenSSH match_pattern rather than known_hosts pattern lists.
    private static func matches(_ name: [UInt8], pattern: [UInt8]) -> Bool {
        var states = [Bool](repeating: false, count: name.count + 1)
        states[0] = true
        for byte in pattern {
            if byte == 42 { // '*': zero bytes, or extend the current match.
                for index in 1...name.count { states[index] = states[index] || states[index - 1] }
            } else {
                for index in stride(from: name.count, through: 1, by: -1) {
                    states[index] = states[index - 1] && (byte == 63 || byte == name[index - 1])
                }
                states[0] = false
            }
        }
        return states[name.count]
    }
}
