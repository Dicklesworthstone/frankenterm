import Foundation

/// Exact SSH user-authentication request algorithms permitted by one route.
/// Key-blob names and certificate signing algorithms are separate identities:
/// an ssh-rsa subject can authenticate with RSA-SHA2, and certificate requests
/// retain their certificate suffix. Nil preserves the backend's default profile;
/// an empty list forbids public-key authentication without disabling other
/// explicitly permitted authentication methods.
public struct WorkspacePublicKeyAlgorithmPolicy: Sendable, Equatable {
    public static let maximumAlgorithms = 64
    public let acceptedAlgorithms: [String]?
    public var isConstrained: Bool { acceptedAlgorithms != nil }
    public var requiredCapabilities: Set<String> {
        isConstrained ? [WorkspaceDocument.publicKeyAlgorithmPolicyCapability] : []
    }

    public init(acceptedAlgorithms: [String]? = nil) throws {
        if let acceptedAlgorithms {
            guard acceptedAlgorithms.count <= Self.maximumAlgorithms else {
                throw FTError.limit("route.pubkey_accepted_algorithms")
            }
            guard Set(acceptedAlgorithms).count == acceptedAlgorithms.count else {
                throw FTError.malformed("route.duplicate_public_key_algorithm")
            }
            self.acceptedAlgorithms = try acceptedAlgorithms.map(WorkspaceDocument.algorithm)
        } else {
            self.acceptedAlgorithms = nil
        }
    }

    private init(unconstrained: Void) { acceptedAlgorithms = nil }
    public static let unconstrained = Self(unconstrained: ())

    /// This mapping describes wire identities, not linked backend capability.
    /// Callers must intersect it with their qualified signature algorithms.
    public static func signatureAlgorithms(forKeyAlgorithm algorithm: String) -> [String] {
        switch algorithm {
        case "ssh-rsa": return ["rsa-sha2-512", "rsa-sha2-256"]
        case "ssh-rsa-cert-v01@openssh.com":
            return ["rsa-sha2-512-cert-v01@openssh.com", "rsa-sha2-256-cert-v01@openssh.com"]
        case "ssh-ed25519", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521",
             "ssh-ed25519-cert-v01@openssh.com", "ecdsa-sha2-nistp256-cert-v01@openssh.com",
             "ecdsa-sha2-nistp384-cert-v01@openssh.com", "ecdsa-sha2-nistp521-cert-v01@openssh.com":
            return [algorithm]
        default: return []
        }
    }

    /// Before offering the chosen credential, narrow the actual backend
    /// profile in the imported preference order. Unknown-only policies and
    /// raw/certificate mismatches produce an empty result, never a default.
    public func filterAlgorithms(forKeyAlgorithm algorithm: String, qualified: [String]) -> [String] {
        let family = Set(Self.signatureAlgorithms(forKeyAlgorithm: algorithm))
        let supported = qualified.filter(family.contains)
        guard let acceptedAlgorithms else { return supported }
        let available = Set(supported)
        return acceptedAlgorithms.filter(available.contains)
    }

    public func permits(keyAlgorithm: String, qualifiedAlgorithms: Set<String>) -> Bool {
        guard isConstrained else { return true }
        return !filterAlgorithms(forKeyAlgorithm: keyAlgorithm, qualified: Array(qualifiedAlgorithms)).isEmpty
    }

    /// Repeat at the signing boundary against the exact request name. Checking
    /// only the underlying signing key would allow a different hash or a raw
    /// offer to bypass a certificate-only route policy.
    public func permits(authenticationAlgorithm: String) -> Bool {
        acceptedAlgorithms?.contains(authenticationAlgorithm) ?? true
    }
}
