import Foundation
#if canImport(CryptoKit)
import CryptoKit
#elseif canImport(Crypto)
import Crypto
#endif

public enum SSHAuthenticationError: Error, Equatable, Sendable {
    case unavailable, busy, stale, denied, invalidChallenge, identityMismatch, unsupportedAlgorithm
}

/// A bounded canonical public identity, never private material or authority to
/// use it. The same parser binds imported credentials, host pins and SSH offers.
public struct SSHPublicIdentity: Sendable, Equatable {
    public let blob: Data, algorithm: String, fingerprint: String

    public init(blob: Data) throws {
        guard (1...512).contains(blob.count) else { throw SSHAuthenticationError.invalidChallenge }
        var reader = ByteCursor(blob)
        guard let algorithm = String(data: try reader.sshString(cap: 64), encoding: .ascii) else {
            throw SSHAuthenticationError.invalidChallenge
        }
        if algorithm == "ssh-ed25519" {
            let point = try reader.sshString(cap: 32)
            guard point.count == 32 else { throw SSHAuthenticationError.invalidChallenge }
            _ = try Curve25519.Signing.PublicKey(rawRepresentation: point)
        } else if let curve = SSHECDSACurve.allCases.first(where: { $0.algorithm == algorithm }) {
            guard try reader.sshString(cap: 16) == Data(curve.rawValue.utf8) else { throw SSHAuthenticationError.invalidChallenge }
            let point = try reader.sshString(cap: curve.publicByteCount)
            guard point.count == curve.publicByteCount, point.first == 4 else { throw SSHAuthenticationError.invalidChallenge }
            switch curve {
            case .nistp256: _ = try P256.Signing.PublicKey(x963Representation: point)
            case .nistp384: _ = try P384.Signing.PublicKey(x963Representation: point)
            case .nistp521: _ = try P521.Signing.PublicKey(x963Representation: point)
            }
        } else { throw SSHAuthenticationError.unsupportedAlgorithm }
        guard reader.remaining == 0 else { throw SSHAuthenticationError.invalidChallenge }
        self.blob = blob; self.algorithm = algorithm
        fingerprint = "SHA256:" + Data(SHA256.hash(data: blob)).base64EncodedString().replacingOccurrences(of: "=", with: "")
    }
}

/// A public-key user-authentication preimage, constructed from typed fields.
/// There is deliberately no arbitrary-message signing entry point. The transport
/// must supply the original session identifier after verifying the actual peer's
/// exchange signature, and must not substitute a later rekey's exchange hash.
public struct SSHUserAuthenticationChallenge: Sendable, CustomDebugStringConvertible {
    public let sessionIdentifier: Data, user: String, publicBlob: Data, algorithm: String
    public let fingerprint: String
    public var debugDescription: String { "SSHUserAuthenticationChallenge([redacted])" }

    public init(sessionIdentifier: Data, user: String, publicBlob: Data) throws {
        // Initial mobile profile uses SHA-2 session identifiers. Legacy SHA-1
        // exchange identifiers and arbitrary-size signing inputs are not enabled.
        guard [32, 48, 64].contains(sessionIdentifier.count), !user.isEmpty, user.utf8.count <= 256,
              !user.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }),
              !publicBlob.isEmpty, publicBlob.count <= 512 else { throw SSHAuthenticationError.invalidChallenge }
        let identity = try SSHPublicIdentity(blob: publicBlob)
        self.sessionIdentifier = sessionIdentifier; self.user = user
        self.publicBlob = publicBlob; self.algorithm = identity.algorithm
        fingerprint = identity.fingerprint
    }

    /// RFC 4252 section 7. The signature field itself is not part of this data.
    public var signedData: Data {
        var result = Self.string(sessionIdentifier)
        result.append(50) // SSH_MSG_USERAUTH_REQUEST
        result += Self.string(Data(user.utf8))
        result += Self.string(Data("ssh-connection".utf8))
        result += Self.string(Data("publickey".utf8))
        result.append(1)
        result += Self.string(Data(algorithm.utf8))
        result += Self.string(publicBlob)
        return result
    }

    static func string(_ data: Data) -> Data {
        var result = Data(); result.appendBE(UInt64(data.count), bytes: 4)
        result += data; return result
    }
}

/// Maintained native primitives on Apple and Swift Crypto on Linux. This returns
/// an SSH signature blob (algorithm string + signature string), not a packet or
/// an outer SSH string. Private input is validated using the same canonical
/// OpenSSH parser as import before any signature is produced.
public enum SSHUserAuthenticationSigner {
    private struct IdentityCryptography: SSHIdentityCryptography {
        func sha256(_ bytes: Data) -> Data { Data(SHA256.hash(data: bytes)) }
        func ed25519PublicKey(seed: Data) throws -> Data {
            try Curve25519.Signing.PrivateKey(rawRepresentation: seed).publicKey.rawRepresentation
        }
        func ecdsaPublicKey(scalar: Data, curve: SSHECDSACurve) throws -> Data {
            try curve.validateScalar(scalar)
            switch curve {
            case .nistp256: return try P256.Signing.PrivateKey(rawRepresentation: scalar).publicKey.x963Representation
            case .nistp384: return try P384.Signing.PrivateKey(rawRepresentation: scalar).publicKey.x963Representation
            case .nistp521: return try P521.Signing.PrivateKey(rawRepresentation: scalar).publicKey.x963Representation
            }
        }
    }

    /// Derive and validate the public identity inside the protected worker.
    /// Existing imported keys can be offered without ever returning a container.
    public static func publicIdentity(privateContainer: Data) throws -> SSHPublicIdentity {
        try Task.checkCancellation()
        return try SSHPublicIdentity(blob: PrivateKeyRecord.validateContainer(privateContainer, using: IdentityCryptography()))
    }

    public static func sign(_ challenge: SSHUserAuthenticationChallenge, privateContainer: Data) throws -> Data {
        try Task.checkCancellation()
        guard try PrivateKeyRecord.validateContainer(privateContainer, using: IdentityCryptography()) == challenge.publicBlob else {
            throw SSHAuthenticationError.identityMismatch
        }
        var outer = ByteCursor(privateContainer)
        _ = try outer.take(15)
        for _ in 0..<3 { _ = try outer.sshString() }
        _ = try outer.uint(4); _ = try outer.sshString()
        var innerBytes = try outer.sshString()
        defer { innerBytes.resetBytes(in: 0..<innerBytes.count) }
        var inner = ByteCursor(innerBytes)
        _ = try inner.take(8); _ = try inner.sshString(cap: 64)
        let payload: Data
        if challenge.algorithm == "ssh-ed25519" {
            _ = try inner.sshString(cap: 32)
            var privateBytes = try inner.sshString(cap: 64)
            defer { privateBytes.resetBytes(in: 0..<privateBytes.count) }
            payload = try Curve25519.Signing.PrivateKey(rawRepresentation: privateBytes.prefix(32)).signature(for: challenge.signedData)
            guard payload.count == 64 else { throw SSHAuthenticationError.unavailable }
        } else {
            guard let curve = SSHECDSACurve.allCases.first(where: { $0.algorithm == challenge.algorithm }) else {
                throw SSHAuthenticationError.unsupportedAlgorithm
            }
            _ = try inner.sshString(cap: 16); _ = try inner.sshString(cap: curve.publicByteCount)
            var scalar = try inner.sshScalar(curve: curve)
            defer { scalar.resetBytes(in: 0..<scalar.count) }
            let raw: Data
            // signature(for:) hashes with SHA-256, SHA-384, or SHA-512 for the
            // corresponding NIST curve, as required by RFC 5656 section 6.2.1.
            switch curve {
            case .nistp256: raw = try P256.Signing.PrivateKey(rawRepresentation: scalar).signature(for: challenge.signedData).rawRepresentation
            case .nistp384: raw = try P384.Signing.PrivateKey(rawRepresentation: scalar).signature(for: challenge.signedData).rawRepresentation
            case .nistp521: raw = try P521.Signing.PrivateKey(rawRepresentation: scalar).signature(for: challenge.signedData).rawRepresentation
            }
            guard raw.count == 2 * curve.scalarBytes else { throw SSHAuthenticationError.unavailable }
            payload = mpint(Data(raw.prefix(curve.scalarBytes))) + mpint(Data(raw.suffix(curve.scalarBytes)))
        }
        try Task.checkCancellation()
        return SSHUserAuthenticationChallenge.string(Data(challenge.algorithm.utf8)) + SSHUserAuthenticationChallenge.string(payload)
    }

    /// RFC 4251 positive, minimal mpint. Never encode a high bit as a negative.
    private static func mpint(_ fixedWidth: Data) -> Data {
        var magnitude = Data(fixedWidth.drop(while: { $0 == 0 }))
        if let first = magnitude.first, first & 0x80 != 0 { magnitude.insert(0, at: magnitude.startIndex) }
        return SSHUserAuthenticationChallenge.string(magnitude)
    }
}

/// A store-issued snapshot, not an SSH proof or a persistent signing grant.
/// The account remains internal; a successful signature never returns key bytes.
public struct WorkspaceSSHAuthorization: Equatable, Sendable, CustomDebugStringConvertible {
    let storeOwner: UUID, account: String
    public let workspaceID: UUID, workspaceRevision: UUID
    public let routeID: String, credentialID: String, host: String, port: UInt16, user: String
    public let routeMode: String, jumpRouteIDs: [String]
    public let algorithm: String, fingerprint: String, presentedHostKey: WorkspaceDocument.HostKey
    public var debugDescription: String { "WorkspaceSSHAuthorization([redacted])" }
}

/// Implementations own one bounded authentication worker. A cancellation request
/// must invalidate its prompt; sign must settle its worker before returning.
/// There is no plaintext-key retrieval method on this boundary.
public protocol WorkspaceSSHSigning: Sendable {
    /// Return only the selected identity's public blob. Metadata may avoid an
    /// unlock prompt; older items may need one bounded protected read. Neither
    /// path returns private bytes or authorizes a future signature.
    func publicIdentity(requestID: UUID, authorization: WorkspaceSSHAuthorization) async throws -> Data
    func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
              challenge: SSHUserAuthenticationChallenge) async throws -> Data
    func cancel(requestID: UUID)
}

public struct WorkspaceSSHIdentity: Sendable {
    public let authorization: WorkspaceSSHAuthorization
    public let identity: SSHPublicIdentity
    let brokerOwner: UUID, foregroundGeneration: UUID
}

/// A foreground lifetime witness only. It grants neither network permission nor
/// signing authority and cannot be constructed by callers or reused on resume.
public struct SSHForegroundLease: Sendable {
    fileprivate let brokerOwner: UUID, generation: UUID
}

public struct SSHAuthenticationRequest: Identifiable, Sendable, CustomDebugStringConvertible {
    public let id: UUID, transportID: UUID
    public let authorization: WorkspaceSSHAuthorization
    public var debugDescription: String { "SSHAuthenticationRequest([redacted])" }
}

public struct SSHAuthenticationSignature: Sendable, CustomDebugStringConvertible {
    public let requestID: UUID, transportID: UUID
    public let authorization: WorkspaceSSHAuthorization
    public let sessionIdentifier: Data, publicBlob: Data, signatureBlob: Data
    public var debugDescription: String { "SSHAuthenticationSignature([redacted])" }
}

/// One app-owned broker serializes prompts across scenes. Every request captures
/// an immutable route revision, actual peer key, original SSH session identifier,
/// user, and selected public identity. Reimport/revocation is checked under the
/// store's persistence lease before and after the protected-key operation.
///
/// This implements credential use, not a transport. A driver must verify the
/// actual key exchange, associate transportID with that connection, and consume
/// the returned signature only for that exact live session. Before publishing a
/// connected route it must revalidate the returned authorization again and retire
/// attempts when local policy changes. Each ProxyJump peer
/// needs its own independently verified request. No path auto-connects a host.
public actor SSHAuthenticationBroker {
    private struct Pending {
        let id: UUID, transportID: UUID
        let expires: ContinuousClock.Instant
        var authorization: WorkspaceSSHAuthorization?
        var challenge: SSHUserAuthenticationChallenge?
        var preparing = true, signing = false, cancelled = false
    }
    private let store: WorkspaceStore
    private let signer: any WorkspaceSSHSigning
    private let support: WorkspaceDocument.RouteSupport
    private let owner = UUID()
    private var foregroundGeneration = UUID()
    private var foreground = false
    private var pending: Pending?

    public init(store: WorkspaceStore, signer: any WorkspaceSSHSigning, support: WorkspaceDocument.RouteSupport) {
        self.store = store; self.signer = signer; self.support = support
    }
    public func resume() {
        if !foreground { foregroundGeneration = UUID() }
        foreground = true
    }
    public func suspend() {
        foreground = false
        foregroundGeneration = UUID()
        if let id = pending?.id { cancel(requestID: id) }
    }
    public var isBusy: Bool { pending != nil }

    /// Admission check before DNS/TCP. It grants no host or credential authority.
    public func requireForeground() throws {
        guard foreground else { throw SSHAuthenticationError.unavailable }
        try Task.checkCancellation()
    }

    public func foregroundLease() throws -> SSHForegroundLease {
        try requireForeground()
        return .init(brokerOwner: owner, generation: foregroundGeneration)
    }

    public func validateForeground(_ lease: SSHForegroundLease) throws {
        try requireForeground()
        guard lease.brokerOwner == owner, lease.generation == foregroundGeneration else { throw SSHAuthenticationError.stale }
    }

    /// Read current authority without touching a private key, so a concrete
    /// transport can refuse unimplemented route modes before requesting unlock.
    public func authorization(workspaceID: UUID, expectedRevision: UUID, routeID: String, credentialID: String,
                              presentedHostKey: WorkspaceDocument.HostKey) async throws -> WorkspaceSSHAuthorization {
        guard foreground else { throw SSHAuthenticationError.unavailable }
        try Task.checkCancellation()
        let generation = foregroundGeneration
        let result = try await store.authorizeSSHAuthentication(workspaceID: workspaceID,
            expectedRevision: expectedRevision, routeID: routeID, credentialID: credentialID,
            presentedHostKey: presentedHostKey, support: support)
        guard foreground, foregroundGeneration == generation else { throw SSHAuthenticationError.unavailable }
        try Task.checkCancellation()
        return result
    }

    /// Verify the real host and current route before reading or offering the
    /// selected public identity. This uses the same bounded worker as signing.
    public func publicIdentity(workspaceID: UUID, expectedRevision: UUID, routeID: String, credentialID: String,
                               presentedHostKey: WorkspaceDocument.HostKey, transportID: UUID) async throws -> WorkspaceSSHIdentity {
        guard foreground else { throw SSHAuthenticationError.unavailable }
        if let old = pending, !old.preparing, !old.signing, ContinuousClock.now >= old.expires { pending = nil }
        guard pending == nil else { throw SSHAuthenticationError.busy }
        try Task.checkCancellation()
        let id = UUID()
        pending = Pending(id: id, transportID: transportID, expires: ContinuousClock.now + .seconds(60))
        defer { if pending?.id == id { pending = nil } }
        let authorization = try await store.authorizeSSHAuthentication(workspaceID: workspaceID,
            expectedRevision: expectedRevision, routeID: routeID, credentialID: credentialID,
            presentedHostKey: presentedHostKey, support: support)
        try checkCurrent(id)
        pending?.authorization = authorization
        let signer = self.signer
        let bytes = try await withTaskCancellationHandler {
            try await signer.publicIdentity(requestID: id, authorization: authorization)
        } onCancel: { signer.cancel(requestID: id) }
        try checkCurrent(id)
        let identity = try SSHPublicIdentity(blob: bytes)
        guard identity.algorithm == authorization.algorithm, identity.fingerprint == authorization.fingerprint else {
            throw SSHAuthenticationError.identityMismatch
        }
        try await store.validateSSHAuthentication(authorization, support: support)
        try checkCurrent(id)
        return .init(authorization: authorization, identity: identity, brokerOwner: owner, foregroundGeneration: foregroundGeneration)
    }

    /// Recheck foreground and durable policy immediately before publishing a
    /// transport or requesting its shell, even after signing has finished.
    public func validate(_ selected: WorkspaceSSHIdentity) async throws {
        guard foreground, selected.brokerOwner == owner, selected.foregroundGeneration == foregroundGeneration else {
            throw SSHAuthenticationError.stale
        }
        try Task.checkCancellation()
        try await store.validateSSHAuthentication(selected.authorization, support: support)
        guard foreground, selected.foregroundGeneration == foregroundGeneration else { throw SSHAuthenticationError.stale }
        try Task.checkCancellation()
    }

    public func cancel(transportID: UUID) {
        if let selected = pending, selected.transportID == transportID { cancel(requestID: selected.id) }
    }

    public func prepare(workspaceID: UUID, expectedRevision: UUID, routeID: String, credentialID: String,
                        presentedHostKey: WorkspaceDocument.HostKey, transportID: UUID,
                        sessionIdentifier: Data, publicBlob: Data) async throws -> SSHAuthenticationRequest {
        guard foreground else { throw SSHAuthenticationError.unavailable }
        if let old = pending, !old.preparing, !old.signing, ContinuousClock.now >= old.expires { pending = nil }
        guard pending == nil else { throw SSHAuthenticationError.busy }
        try Task.checkCancellation()
        // Bound input before retaining it across the first suspension point.
        guard [32, 48, 64].contains(sessionIdentifier.count), (1...512).contains(publicBlob.count) else {
            throw SSHAuthenticationError.invalidChallenge
        }
        let id = UUID()
        pending = Pending(id: id, transportID: transportID, expires: ContinuousClock.now + .seconds(60))
        do {
            let authorization = try await store.authorizeSSHAuthentication(workspaceID: workspaceID,
                expectedRevision: expectedRevision, routeID: routeID, credentialID: credentialID,
                presentedHostKey: presentedHostKey, support: support)
            try checkCurrent(id)
            let challenge = try SSHUserAuthenticationChallenge(sessionIdentifier: sessionIdentifier,
                                                               user: authorization.user, publicBlob: publicBlob)
            guard challenge.algorithm == authorization.algorithm, challenge.fingerprint == authorization.fingerprint else {
                throw SSHAuthenticationError.identityMismatch
            }
            pending?.authorization = authorization; pending?.challenge = challenge; pending?.preparing = false
            return .init(id: id, transportID: transportID, authorization: authorization)
        } catch {
            if pending?.id == id { pending = nil }
            throw error
        }
    }

    /// A deliberate foreground request consumes this ID once, on success or
    /// failure. There is no retry loop, passphrase cache, or signature replay.
    public func confirm(requestID: UUID) async throws -> SSHAuthenticationSignature {
        try checkCurrent(requestID)
        guard let selected = pending, !selected.signing, let authorization = selected.authorization,
              let challenge = selected.challenge else { throw SSHAuthenticationError.stale }
        pending?.signing = true
        defer { if pending?.id == requestID { pending = nil } }
        try await store.validateSSHAuthentication(authorization, support: support)
        try checkCurrent(requestID)
        let signer = self.signer
        let signature = try await withTaskCancellationHandler {
            try await signer.sign(requestID: requestID, authorization: authorization, challenge: challenge)
        } onCancel: { signer.cancel(requestID: requestID) }
        try checkCurrent(requestID)
        guard !signature.isEmpty, signature.count <= 512 else { throw SSHAuthenticationError.unavailable }
        try await store.validateSSHAuthentication(authorization, support: support)
        try checkCurrent(requestID)
        return .init(requestID: requestID, transportID: selected.transportID, authorization: authorization,
                     sessionIdentifier: challenge.sessionIdentifier, publicBlob: challenge.publicBlob, signatureBlob: signature)
    }

    public func cancel(requestID: UUID) {
        guard pending?.id == requestID else { return }
        if pending?.signing == true || pending?.preparing == true {
            pending?.cancelled = true
            signer.cancel(requestID: requestID)
            // Keep the sole worker reservation until confirm settles. Resume
            // cannot start another prompt while cancelled native I/O still owns it.
        } else { pending = nil }
    }
    private func checkCurrent(_ id: UUID) throws {
        try Task.checkCancellation()
        if let selected = pending, selected.id == id, ContinuousClock.now >= selected.expires { cancel(requestID: id) }
        guard foreground, pending?.id == id, pending?.cancelled == false else { throw SSHAuthenticationError.stale }
    }
}
