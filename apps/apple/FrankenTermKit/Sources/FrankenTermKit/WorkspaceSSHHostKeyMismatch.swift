import Foundation

/// Public identity details captured under the same catalog lease that refused
/// SSH authentication. This is a transient explanation, never permission to
/// replace a pin, retry a connection, or use a credential.
///
/// Only a supported, nonrevoked key that mismatches otherwise usable trust
/// produces this outcome. Revocation and mandatory policy failures stay denied.
/// Descriptions intentionally omit peer details; an explicit review reads the
/// typed fields instead of placing endpoint or fingerprint data in logs.
public struct WorkspaceSSHHostKeyMismatch: Error, Equatable, Sendable,
    LocalizedError, CustomStringConvertible, CustomDebugStringConvertible {
    public let workspaceID: UUID
    public let expectedRevision: UUID
    public let routeID: String
    public let transitForRouteID: String?
    public let host: String
    public let port: UInt16
    public let user: String
    public let hostKeyAlias: String?
    public let certificatePrincipal: String
    /// The selected trust record's currently permitted pins, in source order.
    /// Revoked or unsupported configured pins are excluded.
    public let expectedKeys: [WorkspaceDocument.HostKey]
    public let expectedCertificateAuthorities: [WorkspaceDocument.HostKey]
    public let presentedKey: WorkspaceDocument.HostKey
    /// Verified contents of the exact refused exchange, never trust in its CA.
    public let hostCertificate: SSHHostCertificate?

    init(workspaceID: UUID, expectedRevision: UUID, route: WorkspaceDocument.Route,
         transitForRouteID: String?, expectedKeys: [WorkspaceDocument.HostKey],
         presentedKey: WorkspaceDocument.HostKey,
         expectedCertificateAuthorities: [WorkspaceDocument.HostKey] = [],
         hostCertificate: SSHHostCertificate? = nil) {
        self.workspaceID = workspaceID
        self.expectedRevision = expectedRevision
        self.routeID = route.id
        self.transitForRouteID = transitForRouteID
        self.host = route.host
        self.port = route.port
        self.user = route.user
        self.hostKeyAlias = route.hostKeyAlias
        self.certificatePrincipal = route.certificatePrincipal
        self.expectedKeys = expectedKeys
        self.expectedCertificateAuthorities = expectedCertificateAuthorities
        self.presentedKey = presentedKey
        self.hostCertificate = hostCertificate
    }

    public var description: String {
        "The SSH host identity did not match this workspace's permitted keys or certificate authorities. Review the host identity before connecting again."
    }
    public var debugDescription: String { "WorkspaceSSHHostKeyMismatch" }
    public var errorDescription: String? { description }
}

/// A refused peer bound to the exact machine and ordered hop occurrence of the
/// connection that encountered it. The native factory adds this context after
/// closing the failed transport and rechecking its foreground lease.
///
/// A destination has no hop index, including when it follows jump hosts.
/// A transit peer has a zero-based index; repeated route IDs remain distinct
/// occurrences. This value grants no connection or trust authority.
public struct WorkspaceSSHConnectionHostKeyMismatch: Error, Equatable, Sendable,
    LocalizedError, CustomStringConvertible, CustomDebugStringConvertible {
    public let mismatch: WorkspaceSSHHostKeyMismatch
    public let machineID: String
    public let destinationRouteID: String
    public let hopIndex: Int?

    public init(_ mismatch: WorkspaceSSHHostKeyMismatch, machineID: String,
                destinationRouteID: String, hopIndex: Int? = nil) throws {
        guard (try? WorkspaceDocument.identifier(machineID)) != nil,
              (try? WorkspaceDocument.identifier(destinationRouteID)) != nil else {
            throw SSHAuthenticationError.denied
        }
        if let transit = mismatch.transitForRouteID {
            guard transit == destinationRouteID, let hopIndex, (0..<3).contains(hopIndex) else {
                throw SSHAuthenticationError.denied
            }
        } else {
            guard mismatch.routeID == destinationRouteID, hopIndex == nil else {
                throw SSHAuthenticationError.denied
            }
        }
        self.mismatch = mismatch
        self.machineID = machineID
        self.destinationRouteID = destinationRouteID
        self.hopIndex = hopIndex
    }

    public var workspaceID: UUID { mismatch.workspaceID }
    public var expectedRevision: UUID { mismatch.expectedRevision }
    public var routeID: String { mismatch.routeID }
    public var host: String { mismatch.host }
    public var port: UInt16 { mismatch.port }
    public var user: String { mismatch.user }
    public var hostKeyAlias: String? { mismatch.hostKeyAlias }
    public var certificatePrincipal: String { mismatch.certificatePrincipal }
    public var expectedKeys: [WorkspaceDocument.HostKey] { mismatch.expectedKeys }
    public var expectedCertificateAuthorities: [WorkspaceDocument.HostKey] { mismatch.expectedCertificateAuthorities }
    public var presentedKey: WorkspaceDocument.HostKey { mismatch.presentedKey }
    public var hostCertificate: SSHHostCertificate? { mismatch.hostCertificate }
    public var description: String { mismatch.description }
    public var debugDescription: String { "WorkspaceSSHConnectionHostKeyMismatch" }
    public var errorDescription: String? { description }
}
