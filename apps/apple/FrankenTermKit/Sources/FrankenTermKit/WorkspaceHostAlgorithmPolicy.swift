import Foundation

/// Mandatory restrictions on SSH signature algorithms, independently of the
/// serialized public-key family. In particular, an `ssh-rsa` key can sign with
/// either RSA-SHA2 algorithm, while a certificate has its own CA signature.
/// Nil leaves that dimension unconstrained; an empty array denies every value.
public struct WorkspaceHostAlgorithmPolicy: Sendable, Equatable, Codable {
    public static let maximumAlgorithms = 64
    public let hostKeyAlgorithms: [String]?
    public let caSignatureAlgorithms: [String]?
    public let transport: WorkspaceSSHTransportAlgorithmPolicy
    public var isSignatureConstrained: Bool { hostKeyAlgorithms != nil || caSignatureAlgorithms != nil }
    public var isConstrained: Bool { isSignatureConstrained || transport.isConstrained }
    public var requiredCapabilities: Set<String> {
        (isSignatureConstrained ? [WorkspaceDocument.hostAlgorithmPolicyCapability] : Set<String>())
            .union(transport.isConstrained ? [WorkspaceDocument.transportAlgorithmPolicyCapability] : [])
    }

    public init(hostKeyAlgorithms: [String]? = nil, caSignatureAlgorithms: [String]? = nil,
                transport: WorkspaceSSHTransportAlgorithmPolicy = .unconstrained) throws {
        func checked(_ values: [String]?) throws -> [String]? {
            guard let values else { return nil }
            guard values.count <= Self.maximumAlgorithms else { throw FTError.limit("trust.algorithms") }
            guard Set(values).count == values.count else { throw FTError.malformed("trust.duplicate_algorithm") }
            return try values.map(WorkspaceDocument.algorithm)
        }
        self.hostKeyAlgorithms = try checked(hostKeyAlgorithms)
        self.caSignatureAlgorithms = try checked(caSignatureAlgorithms)
        self.transport = transport
    }

    private init(validatedHost: [String]?, validatedCA: [String]?, transport: WorkspaceSSHTransportAlgorithmPolicy = .unconstrained) {
        hostKeyAlgorithms = validatedHost; caSignatureAlgorithms = validatedCA
        self.transport = transport
    }

    public static let unconstrained = Self(validatedHost: nil, validatedCA: nil)

    public init(from decoder: any Decoder) throws {
        let fields = try decoder.container(keyedBy: CodingKeys.self)
        try self.init(hostKeyAlgorithms: fields.decodeIfPresent([String].self, forKey: .hostKeyAlgorithms),
                      caSignatureAlgorithms: fields.decodeIfPresent([String].self, forKey: .caSignatureAlgorithms),
                      transport: fields.decodeIfPresent(WorkspaceSSHTransportAlgorithmPolicy.self, forKey: .transport) ?? .unconstrained)
    }

    /// Constraints only become stronger. The receiver's preference order stays
    /// intact; an older export or a second alias cannot re-enable removed names.
    public func intersecting(_ other: Self) -> Self {
        func intersection(_ lhs: [String]?, _ rhs: [String]?) -> [String]? {
            guard let lhs else { return rhs }
            guard let rhs else { return lhs }
            let accepted = Set(rhs)
            return lhs.filter(accepted.contains)
        }
        return .init(validatedHost: intersection(hostKeyAlgorithms, other.hostKeyAlgorithms),
                     validatedCA: intersection(caSignatureAlgorithms, other.caSignatureAlgorithms),
                     transport: transport.intersecting(other.transport))
    }

    /// Use before key exchange. A qualified native profile is narrowed and
    /// reordered; unsupported imported names never expand native capability.
    public func filterHostKeyAlgorithms(_ qualified: [String]) -> [String] {
        guard let hostKeyAlgorithms else { return qualified }
        let supported = Set(qualified)
        return hostKeyAlgorithms.filter(supported.contains)
    }

    public func permits(hostKeyAlgorithm: String, negotiatedHostSignature: String?,
                        certificate: SSHHostCertificate?, negotiatedTransport: SSHTransportAlgorithms? = nil) -> Bool {
        guard transport.permits(negotiatedTransport) else { return false }
        if let certificate {
            guard certificate.algorithm == hostKeyAlgorithm else { return false }
        } else if hostKeyAlgorithm.hasSuffix("-cert-v01@openssh.com") { return false }
        if let negotiatedHostSignature {
            guard Self.signatures(forHostKeyAlgorithm: hostKeyAlgorithm).contains(negotiatedHostSignature) else { return false }
        }
        if let hostKeyAlgorithms {
            guard let negotiatedHostSignature, hostKeyAlgorithms.contains(negotiatedHostSignature) else { return false }
        }
        if let certificate, let caSignatureAlgorithms,
           !caSignatureAlgorithms.contains(certificate.signatureAlgorithm) { return false }
        return true
    }

    /// Current verified raw/certificate signature families. These mappings are
    /// not backend qualification; callers must intersect actual linked support.
    public static func signatures(forHostKeyAlgorithm algorithm: String) -> Set<String> {
        Set(WorkspacePublicKeyAlgorithmPolicy.signatureAlgorithms(forKeyAlgorithm: algorithm))
    }

    public func permitsHostKeyFamily(_ algorithm: String, qualifiedSignatures: Set<String>) -> Bool {
        guard let hostKeyAlgorithms else { return true }
        let possible = Self.signatures(forHostKeyAlgorithm: algorithm).intersection(qualifiedSignatures)
        return !possible.isDisjoint(with: hostKeyAlgorithms)
    }

    public func permitsCertificateAuthorityFamily(_ algorithm: String) -> Bool {
        guard let caSignatureAlgorithms else { return true }
        let possible = Self.signatures(forHostKeyAlgorithm: algorithm)
        return !possible.isDisjoint(with: caSignatureAlgorithms)
    }

    /// Used to validate the persisted intersection without trusting catalog
    /// ordering. An absent dimension can never replace an existing restriction.
    func isAtLeastAsRestrictive(as other: Self) -> Bool {
        func contains(_ lhs: [String]?, _ rhs: [String]?) -> Bool {
            guard let rhs else { return true }
            guard let lhs else { return false }
            return Set(lhs).isSubset(of: Set(rhs))
        }
        return contains(hostKeyAlgorithms, other.hostKeyAlgorithms)
            && contains(caSignatureAlgorithms, other.caSignatureAlgorithms)
            && transport.isAtLeastAsRestrictive(as: other.transport)
    }
}
