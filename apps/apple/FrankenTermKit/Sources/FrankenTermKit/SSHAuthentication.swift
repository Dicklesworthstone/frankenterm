import Foundation
#if canImport(CryptoKit)
import CryptoKit
#elseif canImport(Crypto)
import Crypto
#endif

public enum SSHAuthenticationError: Error, Equatable, Sendable, LocalizedError {
    case unavailable, busy, stale, denied, invalidChallenge, identityMismatch, unsupportedAlgorithm
    /// The machine presented a host key other than the one saved for it.
    case hostKeyMismatch

    public var errorDescription: String? {
        switch self {
        case .unavailable: "This machine's key isn't available on this device."
        case .busy: "Another sign-in is already in progress."
        case .stale: "This machine's saved settings changed. Open it again."
        case .denied: "This connection isn't allowed on this device: the route isn't ready, or the machine's host key or encryption isn't one it accepts."
        case .invalidChallenge: "The machine sent a sign-in request this app can't answer."
        case .identityMismatch: "The saved key doesn't match the key this machine expects."
        case .unsupportedAlgorithm: "This key type isn't supported yet."
        case .hostKeyMismatch:
            "The machine's identity doesn't match the one saved from your Mac. If it was reinstalled, send it to this phone again from the Mac; otherwise someone may be intercepting the connection."
        }
    }
}

/// A bounded canonical public identity, never private material or authority to
/// use it. The same parser binds imported credentials, host pins and SSH offers.
public struct SSHPublicIdentity: Sendable, Equatable {
    public let blob: Data, algorithm: String, fingerprint: String

    public init(blob: Data) throws {
        // An 8192-bit RSA modulus needs 1024 magnitude bytes and one mpint
        // sign byte. The exponent is at most 64 bits; all three SSH strings
        // together fit in 1053 bytes. User-authentication keeps its own cap.
        guard (1...1053).contains(blob.count) else { throw SSHAuthenticationError.invalidChallenge }
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
        } else if algorithm == "ssh-rsa" {
            let exponent = try Self.positiveMPInt(&reader, maximumBytes: 8)
            let modulus = try Self.positiveMPInt(&reader, maximumBytes: 1024)
            guard let first = modulus.first, let last = modulus.last,
                  last & 1 == 1, exponent.last.map({ $0 & 1 == 1 }) == true,
                  exponent.count > 1 || exponent.first.map({ $0 >= 3 }) == true else {
                throw SSHAuthenticationError.invalidChallenge
            }
            let bits = (modulus.count - 1) * 8 + (8 - first.leadingZeroBitCount)
            guard (2048...8192).contains(bits) else { throw SSHAuthenticationError.unsupportedAlgorithm }
        } else { throw SSHAuthenticationError.unsupportedAlgorithm }
        guard reader.remaining == 0 else { throw SSHAuthenticationError.invalidChallenge }
        self.blob = blob; self.algorithm = algorithm
        fingerprint = "SHA256:" + Data(SHA256.hash(data: blob)).base64EncodedString().replacingOccurrences(of: "=", with: "")
    }

    /// RFC 8332 keeps the public key blob and fingerprint in `ssh-rsa` format
    /// while negotiating a distinct SHA-2 host signature. This is a format
    /// compatibility check, not signature verification or host trust. The
    /// transport must verify the actual exchange before supplying this name.
    public func permitsHostSignature(_ signatureAlgorithm: String) -> Bool {
        if algorithm == "ssh-rsa" {
            return signatureAlgorithm == "rsa-sha2-256" || signatureAlgorithm == "rsa-sha2-512"
        }
        return signatureAlgorithm == algorithm
    }

    private static func positiveMPInt(_ reader: inout ByteCursor, maximumBytes: Int) throws -> Data {
        let encoded = try reader.sshString(cap: maximumBytes + 1)
        guard let first = encoded.first, first & 0x80 == 0 else { throw SSHAuthenticationError.invalidChallenge }
        if first == 0 {
            guard encoded.count > 1, encoded[encoded.startIndex + 1] & 0x80 != 0 else {
                throw SSHAuthenticationError.invalidChallenge
            }
            return Data(encoded.dropFirst())
        }
        guard encoded.count <= maximumBytes else { throw SSHAuthenticationError.invalidChallenge }
        return encoded
    }
}

/// A public-key user-authentication preimage, constructed from typed fields.
/// There is deliberately no arbitrary-message signing entry point. The transport
/// must supply the original session identifier after verifying the actual peer's
/// exchange signature, and must not substitute a later rekey's exchange hash.
public struct SSHUserAuthenticationChallenge: Sendable, CustomDebugStringConvertible {
    public let sessionIdentifier: Data, user: String, publicBlob: Data, algorithm: String
    /// The stable public-identity format is distinct from an RSA-SHA2 signature
    /// algorithm. Changing the hash never changes a saved credential fingerprint.
    public let publicKeyAlgorithm: String, fingerprint: String
    public var debugDescription: String { "SSHUserAuthenticationChallenge([redacted])" }

    public init(sessionIdentifier: Data, user: String, publicBlob: Data,
                signatureAlgorithm: String? = nil) throws {
        // Initial mobile profile uses SHA-2 session identifiers. Legacy SHA-1
        // exchange identifiers and arbitrary-size signing inputs are not enabled.
        guard [32, 48, 64].contains(sessionIdentifier.count), !user.isEmpty, user.utf8.count <= 256,
              !user.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }),
              !publicBlob.isEmpty, publicBlob.count <= 1053 else { throw SSHAuthenticationError.invalidChallenge }
        let identity = try SSHPublicIdentity(blob: publicBlob)
        let algorithm: String
        if identity.algorithm == "ssh-rsa" {
            // RFC 8332 retains the ssh-rsa public blob but signs the exact
            // negotiated SHA-2 name. There is no implicit SHA-1 or guessed hash.
            guard let signatureAlgorithm,
                  signatureAlgorithm == "rsa-sha2-256" || signatureAlgorithm == "rsa-sha2-512" else {
                throw SSHAuthenticationError.unsupportedAlgorithm
            }
            algorithm = signatureAlgorithm
        } else {
            guard signatureAlgorithm == nil || signatureAlgorithm == identity.algorithm else {
                throw SSHAuthenticationError.unsupportedAlgorithm
            }
            algorithm = identity.algorithm
        }
        self.sessionIdentifier = sessionIdentifier; self.user = user
        self.publicBlob = publicBlob; self.algorithm = algorithm
        publicKeyAlgorithm = identity.algorithm
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
        } else if challenge.publicKeyAlgorithm == "ssh-rsa" {
            let key = try OpenSSHRSAKeyMaterial(reading: &inner)
            guard key.publicBlob == challenge.publicBlob else { throw SSHAuthenticationError.identityMismatch }
            payload = try key.signature(for: challenge.signedData, algorithm: challenge.algorithm)
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
    /// A store-validated member of the exact bounded hop chain, never a generic transit
    /// bypass. Nil means this route is being authenticated as a destination.
    public let transitForRouteID: String?
    public let algorithm: String, fingerprint: String, presentedHostKey: WorkspaceDocument.HostKey
    public var debugDescription: String { "WorkspaceSSHAuthorization([redacted])" }
}

/// A password route's current policy and actual verified host. It contains no
/// password, key binding, or reusable credential. Only WorkspaceStore issues it.
public struct WorkspaceSSHPasswordAuthorization: Equatable, Sendable, CustomDebugStringConvertible {
    let storeOwner: UUID
    public let workspaceID: UUID, workspaceRevision: UUID
    public let routeID: String, host: String, port: UInt16, user: String
    public let routeMode: String, jumpRouteIDs: [String], transitForRouteID: String?
    public let presentedHostKey: WorkspaceDocument.HostKey
    public var debugDescription: String { "WorkspaceSSHPasswordAuthorization([redacted])" }
}

public struct WorkspaceSSHPasswordIdentity: Sendable {
    public let authorization: WorkspaceSSHPasswordAuthorization
    fileprivate let brokerOwner: UUID, foregroundGeneration: UUID, passwordGeneration: UUID
}

/// Trusted local labels for exactly one verified SSH peer. Server-provided
/// banners and keyboard-interactive challenges are never password prompts.
public struct WorkspaceSSHPasswordRequest: Identifiable, Sendable, CustomDebugStringConvertible {
    public let id: UUID, transportID: UUID
    public let authorization: WorkspaceSSHPasswordAuthorization
    public var debugDescription: String { "WorkspaceSSHPasswordRequest([redacted])" }
}

/// An app-owned prompt must settle on cancellation and clear its editable
/// secret. The shared broker permits only one protected prompt at a time and
/// imposes its existing 60-second deadline, including queueing.
public protocol WorkspaceSSHPasswordPrompting: Sendable {
    func password(for request: WorkspaceSSHPasswordRequest) async throws -> Data
    func cancel(requestID: UUID)
}

/// Keyboard-interactive is a separately authorized SSH method. A password
/// permission never authorizes server-provided questions or one-time codes.
public struct WorkspaceSSHKeyboardInteractiveAuthorization: Equatable, Sendable, CustomDebugStringConvertible {
    let storeOwner: UUID
    public let workspaceID: UUID, workspaceRevision: UUID
    public let routeID: String, host: String, port: UInt16, user: String
    public let routeMode: String, jumpRouteIDs: [String], transitForRouteID: String?
    public let presentedHostKey: WorkspaceDocument.HostKey
    public var debugDescription: String { "WorkspaceSSHKeyboardInteractiveAuthorization([redacted])" }
}

/// The server's bounded questions, not trusted labels for the destination.
/// Trusted machine, route, account and verified-host labels come from the
/// request's authorization and must remain outside these server-owned strings.
public struct SSHKeyboardInteractiveChallenge: Equatable, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    public static let maximumRounds: UInt32 = 4
    public static let maximumPrompts = 8
    public static let maximumResponseBytes = 1024
    public static let maximumResponseTotalBytes = 8192

    public struct Prompt: Equatable, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        public let text: String, echo: Bool
        public var description: String { "SSHKeyboardInteractiveChallenge.Prompt([redacted])" }
        public var debugDescription: String { description }
        public init(text: String, echo: Bool) throws {
            guard text.utf8.count <= 1024, !text.utf8.contains(0) else { throw SSHAuthenticationError.invalidChallenge }
            self.text = text; self.echo = echo
        }
    }

    public let round: UInt32
    public let name: String, instruction: String
    public let prompts: [Prompt]
    public var description: String { "SSHKeyboardInteractiveChallenge([redacted])" }
    public var debugDescription: String { description }

    public init(round: UInt32, name: String, instruction: String, prompts: [Prompt]) throws {
        guard (1...Self.maximumRounds).contains(round), prompts.count <= Self.maximumPrompts,
              name.utf8.count <= 256, instruction.utf8.count <= 1024,
              !name.utf8.contains(0), !instruction.utf8.contains(0) else {
            throw SSHAuthenticationError.invalidChallenge
        }
        // Match the native encoding: two SSH strings, one count, then an
        // echo byte and SSH string for each prompt. Bound before suspension.
        let encodedBytes = 12 + name.utf8.count + instruction.utf8.count
            + prompts.reduce(0) { $0 + 5 + $1.text.utf8.count }
        guard encodedBytes <= 8192 else { throw SSHAuthenticationError.invalidChallenge }
        self.round = round; self.name = name; self.instruction = instruction; self.prompts = prompts
    }
}

public struct WorkspaceSSHKeyboardInteractiveIdentity: Sendable {
    public let authorization: WorkspaceSSHKeyboardInteractiveAuthorization
    fileprivate let brokerOwner: UUID, foregroundGeneration: UUID, passwordGeneration: UUID, attemptGeneration: UUID
}

public struct WorkspaceSSHKeyboardInteractiveRequest: Identifiable, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    public let id: UUID, transportID: UUID, attemptID: UUID
    public let authorization: WorkspaceSSHKeyboardInteractiveAuthorization
    public let challenge: SSHKeyboardInteractiveChallenge
    public var description: String { "WorkspaceSSHKeyboardInteractiveRequest([redacted])" }
    public var debugDescription: String { description }
}

/// Every answer is transient, including an echo-enabled answer. Implementations
/// present all prompts in one round, preserve echo flags, and explicitly confirm
/// an informational zero-prompt round. Cancellation clears every editable answer
/// and settles the request; no response may be cached for a subsequent round.
public protocol WorkspaceSSHKeyboardInteractivePrompting: Sendable {
    func responses(for request: WorkspaceSSHKeyboardInteractiveRequest) async throws -> [Data]
    func cancel(requestID: UUID)
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
    public let algorithm: String
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
        let foregroundGeneration: UUID
        let expires: ContinuousClock.Instant
        var authorization: WorkspaceSSHAuthorization?
        var passwordPrompter: (any WorkspaceSSHPasswordPrompting)?
        var keyboardInteractivePrompter: (any WorkspaceSSHKeyboardInteractivePrompting)?
        var keyboardInteractiveGeneration: UUID?
        var challenge: SSHUserAuthenticationChallenge?
        var preparing = true, signing = false, cancelled = false
    }
    private struct KeyboardInteractiveAttempt {
        let authorization: WorkspaceSSHKeyboardInteractiveAuthorization
        let expires: ContinuousClock.Instant
        var transportID: UUID?, attemptID: UUID?
        var nextRound: UInt32 = 1
        var responding = false
    }
    private struct Waiting {
        let request: Pending
        let continuation: CheckedContinuation<Void, Error>
    }
    private let store: WorkspaceStore
    private let signer: any WorkspaceSSHSigning
    private let support: WorkspaceDocument.RouteSupport
    private let owner = UUID()
    private var foregroundGeneration = UUID()
    private var foreground = false
    private var passwordGeneration = UUID()
    private var passwordForeground = false
    private var keyboardInteractiveAttempts: [UUID: KeyboardInteractiveAttempt] = [:]
    private var pending: Pending?
    private var waiting: [Waiting] = []
    private var deadlineTask: Task<Void, Never>?
    private let maximumQueuedRequests = 32

    public init(store: WorkspaceStore, signer: any WorkspaceSSHSigning, support: WorkspaceDocument.RouteSupport) {
        self.store = store; self.signer = signer; self.support = support
    }
    deinit { deadlineTask?.cancel() }
    public func resume() {
        if !foreground { foregroundGeneration = UUID(); setPasswordForeground(true) }
        foreground = true
    }
    public func suspend() {
        foreground = false
        foregroundGeneration = UUID()
        setPasswordForeground(false)
        let retired = waiting; waiting.removeAll()
        for waiter in retired { waiter.continuation.resume(throwing: SSHAuthenticationError.stale) }
        if let id = pending?.id { cancel(requestID: id) }
    }
    public var isBusy: Bool { pending != nil }
    public var queuedRequests: Int { waiting.count }

    /// The app may keep authenticated terminals alive during its background
    /// grace period. An editable password or keyboard-interactive prompt never survives that boundary;
    /// retiring its own epoch need not revoke those already-live transports.
    public func setPasswordForeground(_ active: Bool) {
        guard passwordForeground != active else { return }
        passwordForeground = active; passwordGeneration = UUID()
        if !active {
            keyboardInteractiveAttempts.removeAll()
            let retired = waiting.filter { $0.request.keyboardInteractiveGeneration != nil }
            waiting.removeAll { $0.request.keyboardInteractiveGeneration != nil }
            for waiter in retired { waiter.continuation.resume(throwing: SSHAuthenticationError.stale) }
            if let pending, pending.passwordPrompter != nil || pending.keyboardInteractiveGeneration != nil {
                cancel(requestID: pending.id)
            }
        }
    }

    /// One protected operation owns the signer, with a bounded FIFO shared by
    /// all scenes and connections. Waiting never opens a second prompt. The
    /// original foreground epoch and a 60-second lifetime cover both waiting
    /// and confirmation; queued work is never carried across suspend/resume.
    private func acquireWorker(transportID: UUID, expires: ContinuousClock.Instant? = nil,
                               keyboardInteractiveGeneration: UUID? = nil) async throws -> UUID {
        try requireForeground()
        expireRequests()
        guard pending?.cancelled != true,
              pending?.transportID != transportID,
              !waiting.contains(where: { $0.request.transportID == transportID }) else {
            throw SSHAuthenticationError.busy
        }
        let request = Pending(id: UUID(), transportID: transportID,
            foregroundGeneration: foregroundGeneration, expires: expires ?? ContinuousClock.now + .seconds(60),
            keyboardInteractiveGeneration: keyboardInteractiveGeneration)
        if pending == nil {
            pending = request; scheduleDeadline()
            return request.id
        }
        guard waiting.count < maximumQueuedRequests else { throw SSHAuthenticationError.busy }
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
                guard !Task.isCancelled else { continuation.resume(throwing: CancellationError()); return }
                waiting.append(.init(request: request, continuation: continuation))
                scheduleDeadline()
            }
        } onCancel: {
            Task { await self.cancelWaiting(requestID: request.id) }
        }
        do { try checkCurrent(request.id); return request.id }
        catch { releaseWorker(request.id); throw error }
    }

    private func cancelWaiting(requestID: UUID) {
        if let index = waiting.firstIndex(where: { $0.request.id == requestID }) {
            waiting.remove(at: index).continuation.resume(throwing: CancellationError())
        } else { cancel(requestID: requestID) }
    }

    private func releaseWorker(_ id: UUID) {
        guard pending?.id == id else { return }
        pending = nil
        promoteWaiting()
    }

    private func promoteWaiting() {
        guard pending == nil else { return }
        while !waiting.isEmpty {
            let waiter = waiting.removeFirst()
            guard foreground, waiter.request.foregroundGeneration == foregroundGeneration,
                  ContinuousClock.now < waiter.request.expires else {
                waiter.continuation.resume(throwing: SSHAuthenticationError.stale)
                continue
            }
            // Reserve before resuming: a newly arriving request cannot overtake
            // this waiter or acquire a second signer while its actor task wakes.
            pending = waiter.request
            waiter.continuation.resume()
            break
        }
        scheduleDeadline()
    }

    /// The single watchdog also retires an abandoned prepared request, so it
    /// cannot indefinitely block every other machine. Native signing resources
    /// remain reserved after cancellation until their actual worker settles.
    func expireRequests(at now: ContinuousClock.Instant = .now) {
        keyboardInteractiveAttempts = keyboardInteractiveAttempts.filter { now < $0.value.expires }
        var retained: [Waiting] = []
        for waiter in waiting {
            if !foreground || waiter.request.foregroundGeneration != foregroundGeneration || now >= waiter.request.expires {
                waiter.continuation.resume(throwing: SSHAuthenticationError.stale)
            } else { retained.append(waiter) }
        }
        waiting = retained
        if let selected = pending, !selected.cancelled, now >= selected.expires { cancel(requestID: selected.id) }
        promoteWaiting()
    }

    private func scheduleDeadline() {
        guard deadlineTask == nil else { return }
        let deadlines = waiting.map(\.request.expires) + (pending.flatMap { $0.cancelled ? nil : $0.expires }.map { [$0] } ?? [])
            + keyboardInteractiveAttempts.values.map(\.expires)
        guard let deadline = deadlines.min() else { return }
        deadlineTask = Task { [weak self] in
            do { try await ContinuousClock().sleep(until: deadline) }
            catch { return }
            await self?.deadlineReached()
        }
    }
    private func deadlineReached() {
        deadlineTask = nil
        expireRequests()
        scheduleDeadline()
    }

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
                              presentedHostKey: WorkspaceDocument.HostKey,
                              transitForRouteID: String? = nil) async throws -> WorkspaceSSHAuthorization {
        guard foreground else { throw SSHAuthenticationError.unavailable }
        try Task.checkCancellation()
        let generation = foregroundGeneration
        let result = try await store.authorizeSSHAuthentication(workspaceID: workspaceID,
            expectedRevision: expectedRevision, routeID: routeID, credentialID: credentialID,
            presentedHostKey: presentedHostKey, support: support, transitForRouteID: transitForRouteID)
        guard foreground, foregroundGeneration == generation else { throw SSHAuthenticationError.unavailable }
        try Task.checkCancellation()
        return result
    }

    /// This runs after the transport verified its real key exchange and before
    /// the server is asked which methods it offers. It never opens a prompt.
    public func passwordIdentity(workspaceID: UUID, expectedRevision: UUID, routeID: String,
                                 presentedHostKey: WorkspaceDocument.HostKey,
                                 transitForRouteID: String? = nil) async throws -> WorkspaceSSHPasswordIdentity {
        let lease = try foregroundLease()
        guard passwordForeground else { throw SSHAuthenticationError.stale }
        let passwordEpoch = passwordGeneration
        let authorization = try await store.authorizeSSHPasswordAuthentication(workspaceID: workspaceID,
            expectedRevision: expectedRevision, routeID: routeID, presentedHostKey: presentedHostKey,
            support: support, transitForRouteID: transitForRouteID)
        try validateForeground(lease)
        guard passwordForeground, passwordGeneration == passwordEpoch else { throw SSHAuthenticationError.stale }
        return .init(authorization: authorization, brokerOwner: owner, foregroundGeneration: foregroundGeneration,
                     passwordGeneration: passwordEpoch)
    }

    public func validate(_ selected: WorkspaceSSHPasswordIdentity) async throws {
        guard foreground, selected.brokerOwner == owner, selected.foregroundGeneration == foregroundGeneration else {
            throw SSHAuthenticationError.stale
        }
        try Task.checkCancellation()
        try await store.validateSSHPasswordAuthentication(selected.authorization, support: support)
        guard foreground, selected.foregroundGeneration == foregroundGeneration else { throw SSHAuthenticationError.stale }
        try Task.checkCancellation()
    }

    /// Pending authentication additionally belongs to the current visible
    /// password-prompt epoch. A completed native login may retain only the
    /// normal foreground and durable-policy witness via validate(_:).
    public func validatePasswordAttempt(_ selected: WorkspaceSSHPasswordIdentity) async throws {
        try validatePasswordForeground(selected)
        try await validate(selected)
        try validatePasswordForeground(selected)
    }

    public func password(for selected: WorkspaceSSHPasswordIdentity, transportID: UUID,
                         prompter: any WorkspaceSSHPasswordPrompting) async throws -> Data {
        try validatePasswordForeground(selected)
        try await validate(selected)
        let id = try await acquireWorker(transportID: transportID)
        defer { releaseWorker(id) }
        try checkCurrent(id)
        try await validate(selected)
        try checkCurrent(id)
        try validatePasswordForeground(selected)
        pending?.passwordPrompter = prompter
        let request = WorkspaceSSHPasswordRequest(id: id, transportID: transportID, authorization: selected.authorization)
        var bytes = try await withTaskCancellationHandler {
            try await prompter.password(for: request)
        } onCancel: { prompter.cancel(requestID: id) }
        defer { bytes.resetBytes(in: 0..<bytes.count) }
        try checkCurrent(id)
        try validatePasswordForeground(selected)
        guard (1...1024).contains(bytes.count), !bytes.contains(0) else { throw SSHAuthenticationError.invalidChallenge }
        try await validate(selected)
        try checkCurrent(id)
        try validatePasswordForeground(selected)
        return bytes
    }

    private func validatePasswordForeground(_ selected: WorkspaceSSHPasswordIdentity) throws {
        guard passwordForeground, selected.passwordGeneration == passwordGeneration else { throw SSHAuthenticationError.stale }
    }

    /// Admission uses the separately permitted keyboard-interactive method and
    /// the actual verified peer. One bounded lifetime covers every round, even
    /// while another machine owns the shared protected-prompt worker.
    public func keyboardInteractiveIdentity(workspaceID: UUID, expectedRevision: UUID, routeID: String,
                                            presentedHostKey: WorkspaceDocument.HostKey,
                                            transitForRouteID: String? = nil) async throws -> WorkspaceSSHKeyboardInteractiveIdentity {
        let lease = try foregroundLease()
        guard passwordForeground else { throw SSHAuthenticationError.stale }
        let promptEpoch = passwordGeneration
        let authorization = try await store.authorizeSSHKeyboardInteractiveAuthentication(workspaceID: workspaceID,
            expectedRevision: expectedRevision, routeID: routeID, presentedHostKey: presentedHostKey,
            support: support, transitForRouteID: transitForRouteID)
        try validateForeground(lease)
        guard passwordForeground, passwordGeneration == promptEpoch else { throw SSHAuthenticationError.stale }
        expireRequests()
        guard keyboardInteractiveAttempts.count < maximumQueuedRequests else { throw SSHAuthenticationError.busy }
        let generation = UUID()
        keyboardInteractiveAttempts[generation] = .init(authorization: authorization,
            expires: ContinuousClock.now + .seconds(60))
        scheduleDeadline()
        return .init(authorization: authorization, brokerOwner: owner, foregroundGeneration: foregroundGeneration,
                     passwordGeneration: promptEpoch, attemptGeneration: generation)
    }

    /// A completed native login retains the normal foreground and policy
    /// witness. This method never proves that the server accepted an answer.
    public func validate(_ selected: WorkspaceSSHKeyboardInteractiveIdentity) async throws {
        guard foreground, selected.brokerOwner == owner, selected.foregroundGeneration == foregroundGeneration else {
            throw SSHAuthenticationError.stale
        }
        try Task.checkCancellation()
        try await store.validateSSHKeyboardInteractiveAuthentication(selected.authorization, support: support)
        guard foreground, selected.foregroundGeneration == foregroundGeneration else { throw SSHAuthenticationError.stale }
        try Task.checkCancellation()
    }

    public func validateKeyboardInteractiveAttempt(_ selected: WorkspaceSSHKeyboardInteractiveIdentity) async throws {
        try validateKeyboardInteractiveForeground(selected)
        try await validate(selected)
        try validateKeyboardInteractiveForeground(selected)
    }

    /// A native authentication attempt supplies one immutable ID. The first
    /// round binds it and its transport to this broker-issued identity; later
    /// rounds cannot change either, skip ahead, repeat, or reuse old responses.
    public func keyboardInteractiveResponses(for selected: WorkspaceSSHKeyboardInteractiveIdentity,
                                              transportID: UUID, attemptID: UUID,
                                              challenge: SSHKeyboardInteractiveChallenge,
                                              prompter: any WorkspaceSSHKeyboardInteractivePrompting) async throws -> [Data] {
        try await validateKeyboardInteractiveAttempt(selected)
        guard var attempt = keyboardInteractiveAttempts[selected.attemptGeneration], !attempt.responding else {
            throw SSHAuthenticationError.busy
        }
        guard challenge.round == attempt.nextRound,
              attempt.transportID == nil || attempt.transportID == transportID,
              attempt.attemptID == nil || attempt.attemptID == attemptID else { throw SSHAuthenticationError.stale }
        attempt.transportID = transportID; attempt.attemptID = attemptID; attempt.responding = true
        keyboardInteractiveAttempts[selected.attemptGeneration] = attempt
        var workerID: UUID?, completed = false
        defer {
            if !completed { keyboardInteractiveAttempts[selected.attemptGeneration] = nil }
            if let workerID { releaseWorker(workerID) }
        }
        let id = try await acquireWorker(transportID: transportID, expires: attempt.expires,
                                         keyboardInteractiveGeneration: selected.attemptGeneration)
        workerID = id
        try checkCurrent(id)
        try await validateKeyboardInteractiveAttempt(selected)
        try checkCurrent(id)
        pending?.keyboardInteractivePrompter = prompter
        let request = WorkspaceSSHKeyboardInteractiveRequest(id: id, transportID: transportID, attemptID: attemptID,
            authorization: selected.authorization, challenge: challenge)
        var answers = try await withTaskCancellationHandler {
            try await prompter.responses(for: request)
        } onCancel: { prompter.cancel(requestID: id) }
        defer {
            for index in answers.indices { answers[index].resetBytes(in: 0..<answers[index].count) }
        }
        try checkCurrent(id)
        try validateKeyboardInteractiveForeground(selected)
        guard answers.count == challenge.prompts.count,
              answers.allSatisfy({ $0.count <= SSHKeyboardInteractiveChallenge.maximumResponseBytes && !$0.contains(0) }),
              answers.reduce(0, { $0 + $1.count }) <= SSHKeyboardInteractiveChallenge.maximumResponseTotalBytes else {
            throw SSHAuthenticationError.invalidChallenge
        }
        try await validateKeyboardInteractiveAttempt(selected)
        try checkCurrent(id)
        guard var current = keyboardInteractiveAttempts[selected.attemptGeneration], current.responding,
              current.nextRound == challenge.round, current.transportID == transportID, current.attemptID == attemptID else {
            throw SSHAuthenticationError.stale
        }
        current.nextRound += 1; current.responding = false
        keyboardInteractiveAttempts[selected.attemptGeneration] = current
        completed = true
        return answers
    }

    /// Call after the native method completes, fails or is cancelled. Retiring
    /// an attempt is idempotent and cannot dismiss another transport's prompt.
    public func finishKeyboardInteractiveAttempt(_ selected: WorkspaceSSHKeyboardInteractiveIdentity) {
        guard selected.brokerOwner == owner else { return }
        keyboardInteractiveAttempts[selected.attemptGeneration] = nil
        if let index = waiting.firstIndex(where: { $0.request.keyboardInteractiveGeneration == selected.attemptGeneration }) {
            waiting.remove(at: index).continuation.resume(throwing: SSHAuthenticationError.stale)
        }
        if let pending, pending.keyboardInteractiveGeneration == selected.attemptGeneration { cancel(requestID: pending.id) }
    }

    private func validateKeyboardInteractiveForeground(_ selected: WorkspaceSSHKeyboardInteractiveIdentity) throws {
        guard passwordForeground, selected.passwordGeneration == passwordGeneration,
              let attempt = keyboardInteractiveAttempts[selected.attemptGeneration],
              attempt.authorization == selected.authorization, ContinuousClock.now < attempt.expires else {
            throw SSHAuthenticationError.stale
        }
    }

    /// Verify the real host and current route before reading or offering the
    /// selected public identity. This uses the same bounded worker as signing.
    public func publicIdentity(workspaceID: UUID, expectedRevision: UUID, routeID: String, credentialID: String,
                               presentedHostKey: WorkspaceDocument.HostKey, transportID: UUID,
                               transitForRouteID: String? = nil) async throws -> WorkspaceSSHIdentity {
        _ = try WorkspaceDocument.identifier(routeID); _ = try WorkspaceDocument.identifier(credentialID)
        let id = try await acquireWorker(transportID: transportID)
        defer { releaseWorker(id) }
        try checkCurrent(id)
        let authorization = try await store.authorizeSSHAuthentication(workspaceID: workspaceID,
            expectedRevision: expectedRevision, routeID: routeID, credentialID: credentialID,
            presentedHostKey: presentedHostKey, support: support, transitForRouteID: transitForRouteID)
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
        keyboardInteractiveAttempts = keyboardInteractiveAttempts.filter { $0.value.transportID != transportID }
        if let index = waiting.firstIndex(where: { $0.request.transportID == transportID }) {
            waiting.remove(at: index).continuation.resume(throwing: SSHAuthenticationError.stale)
        }
        if let selected = pending, selected.transportID == transportID { cancel(requestID: selected.id) }
    }

    public func prepare(workspaceID: UUID, expectedRevision: UUID, routeID: String, credentialID: String,
                        presentedHostKey: WorkspaceDocument.HostKey, transportID: UUID,
                        sessionIdentifier: Data, publicBlob: Data,
                        signatureAlgorithm: String? = nil,
                        transitForRouteID: String? = nil) async throws -> SSHAuthenticationRequest {
        // Bound input before retaining it across the first suspension point.
        _ = try WorkspaceDocument.identifier(routeID); _ = try WorkspaceDocument.identifier(credentialID)
        guard [32, 48, 64].contains(sessionIdentifier.count), (1...1053).contains(publicBlob.count),
              signatureAlgorithm.map({ $0.utf8.count <= 64 }) ?? true else {
            throw SSHAuthenticationError.invalidChallenge
        }
        let id = try await acquireWorker(transportID: transportID)
        do {
            try checkCurrent(id)
            let authorization = try await store.authorizeSSHAuthentication(workspaceID: workspaceID,
                expectedRevision: expectedRevision, routeID: routeID, credentialID: credentialID,
                presentedHostKey: presentedHostKey, support: support, transitForRouteID: transitForRouteID)
            try checkCurrent(id)
            let challenge = try SSHUserAuthenticationChallenge(sessionIdentifier: sessionIdentifier,
                user: authorization.user, publicBlob: publicBlob, signatureAlgorithm: signatureAlgorithm)
            guard challenge.publicKeyAlgorithm == authorization.algorithm, challenge.fingerprint == authorization.fingerprint else {
                throw SSHAuthenticationError.identityMismatch
            }
            pending?.authorization = authorization; pending?.challenge = challenge; pending?.preparing = false
            return .init(id: id, transportID: transportID, authorization: authorization)
        } catch {
            releaseWorker(id)
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
        defer { releaseWorker(requestID) }
        try await store.validateSSHAuthentication(authorization, support: support)
        try checkCurrent(requestID)
        let signer = self.signer
        let signature = try await withTaskCancellationHandler {
            try await signer.sign(requestID: requestID, authorization: authorization, challenge: challenge)
        } onCancel: { signer.cancel(requestID: requestID) }
        try checkCurrent(requestID)
        // 8192-bit RSA contributes 1024 raw bytes plus two SSH strings and the
        // rsa-sha2-512 name. No private material crosses this bounded result.
        guard !signature.isEmpty, signature.count <= 1044 else { throw SSHAuthenticationError.unavailable }
        try await store.validateSSHAuthentication(authorization, support: support)
        try checkCurrent(requestID)
        return .init(requestID: requestID, transportID: selected.transportID, authorization: authorization,
                     algorithm: challenge.algorithm,
                     sessionIdentifier: challenge.sessionIdentifier, publicBlob: challenge.publicBlob, signatureBlob: signature)
    }

    public func cancel(requestID: UUID) {
        guard pending?.id == requestID else { return }
        if let generation = pending?.keyboardInteractiveGeneration { keyboardInteractiveAttempts[generation] = nil }
        if pending?.signing == true || pending?.preparing == true {
            pending?.cancelled = true
            signer.cancel(requestID: requestID)
            pending?.passwordPrompter?.cancel(requestID: requestID)
            pending?.keyboardInteractivePrompter?.cancel(requestID: requestID)
            // Keep the sole worker reservation until confirm settles. Resume
            // cannot start another prompt while cancelled native I/O still owns it.
        } else { releaseWorker(requestID) }
    }
    private func checkCurrent(_ id: UUID) throws {
        try Task.checkCancellation()
        if let selected = pending, selected.id == id, ContinuousClock.now >= selected.expires { cancel(requestID: id) }
        guard foreground, pending?.foregroundGeneration == foregroundGeneration,
              pending?.id == id, pending?.cancelled == false else { throw SSHAuthenticationError.stale }
    }
}
