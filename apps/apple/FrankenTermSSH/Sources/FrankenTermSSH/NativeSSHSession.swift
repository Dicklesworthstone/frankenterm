import Foundation
import Dispatch
import CFrankenTermSSH
#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

public enum NativeSSHError: Error, Equatable, Sendable, LocalizedError {
    case invalidInput, busy, closed, timedOut, authenticationTimedOut, invalidChallenge
    case jumpConnectionFailed, jumpAuthenticationRequired
    case native(Int32)
    case resolution(Int32), connection(Int32)

    /// The socket itself failed: reset, closed by the peer, or unreadable.
    public var isConnectionFailure: Bool {
        if case let .native(code) = self { return code == FTSSH_IO }
        return false
    }

    /// The approved server explicitly refused an unsigned public-key offer.
    /// No signing callback ran. Only this evidence permits another configured
    /// credential to be considered; signed/partial/unknown failures never do.
    public var isCredentialRefused: Bool {
        if case let .native(code) = self { return code == FTSSH_AUTH_REJECTED }
        return false
    }

    /// The address never answered or could not be reached: nothing about
    /// the machine's SSH server, host key or the user's key is known.
    public var isUnreachable: Bool {
        switch self {
        case .timedOut, .resolution, .connection: true
        default: false
        }
    }

    /// The machine's SSH server answered, and another attempt would get the
    /// same answer: its encryption, host key or key policy ended this one. A
    /// reset, a close or a timeout says nothing about the next attempt.
    public var isMachineAnswer: Bool {
        switch self {
        case .invalidChallenge, .jumpAuthenticationRequired: true
        case let .native(code):
            [FTSSH_NEGOTIATION, FTSSH_HOST_REJECTED, FTSSH_AUTH_REJECTED, FTSSH_CHALLENGE, FTSSH_UNSUPPORTED,
             FTSSH_AUTH_FAILED, FTSSH_PASSWORD_FAILED].contains(Int(code))
        default: false
        }
    }

    public var errorDescription: String? {
        switch self {
        case .invalidInput: "The connection settings are not valid."
        case .busy: "Another operation is already using this connection."
        case .closed: "The connection is closed."
        case .timedOut: "The machine didn't answer in time."
        case .authenticationTimedOut: "The SSH connection did not finish authentication in time."
        case .invalidChallenge: "The machine asked for authentication this app can't answer."
        case .jumpConnectionFailed: "The jump host could not establish the approved connection. Check its route, key and host trust."
        case .jumpAuthenticationRequired: "Checking this route requires signing in to its jump host. Connect to check the complete route."
        case let .resolution(code): "The machine's name couldn't be looked up (\(String(cString: gai_strerror(code))))."
        case let .connection(code): "The machine couldn't be reached (\(String(cString: strerror(code))))."
        case let .native(code):
            switch Int(code) {
            case FTSSH_IO: "The connection to the machine was lost."
            case FTSSH_NEGOTIATION: "The machine's SSH server offers no encryption this app accepts."
            case FTSSH_HOST_REJECTED: "The machine's host key changed during the connection."
            case FTSSH_AUTH_REJECTED: "The machine didn't accept this key. Add the key's public half to ~/.ssh/authorized_keys on the machine."
            case FTSSH_AUTH_FAILED: "SSH key authentication could not be completed. The server may require an additional authentication method."
            case FTSSH_PASSWORD_FAILED: "The server did not complete password authentication. Check the password; the account may require a password change or another authentication method."
            case FTSSH_CLOSED: "The machine closed the connection."
            case FTSSH_UNSUPPORTED: "The machine requires an SSH feature this app doesn't support."
            default: "The SSH connection failed (code \(code))."
            }
        }
    }
}

/// Public evidence from this exact completed exchange, not a trust decision.
public struct NativeSSHPeer: Sendable, Equatable {
    public let transportID: UUID
    public let endpoint: NativeSSHEndpoint?
    public let hostKey: Data
    public let keyExchange: String, hostSignature: String
    public let outboundCipher: String, inboundCipher: String
    public let outboundMAC: String, inboundMAC: String
    public let outboundCompression: String, inboundCompression: String
}

public struct NativeSSHIdentity: Sendable, Equatable {
    public let user: String, publicKey: Data
    public init(user: String, publicKey: Data) throws {
        guard !user.isEmpty, user.utf8.count <= 256,
              !user.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains),
              (1...Int(FTSSH_PUBLIC_KEY_CAPACITY)).contains(publicKey.count) else { throw NativeSSHError.invalidInput }
        self.user = user; self.publicKey = publicKey
    }
}

/// Constructed only from the driver's verified user-authentication preimage.
/// The existing workspace signer should reconstruct its own typed challenge and
/// require equality with signedData before returning its SSH signature blob.
public struct NativeSSHChallenge: Sendable {
    public let transportID: UUID, sessionIdentifier: Data
    public let identity: NativeSSHIdentity, publicKeyAlgorithm: String, algorithm: String, signedData: Data

    init(bytes: Data, transportID: UUID, identity: NativeSSHIdentity) throws {
        guard bytes.count <= Int(FTSSH_CHALLENGE_CAPACITY) else { throw NativeSSHError.invalidChallenge }
        var offset = 0
        func field() throws -> Data {
            guard bytes.count - offset >= 4 else { throw NativeSSHError.invalidChallenge }
            let count = bytes[offset..<offset + 4].reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
            offset += 4
            guard count <= bytes.count - offset else { throw NativeSSHError.invalidChallenge }
            let value = bytes.subdata(in: offset..<offset + Int(count)); offset += Int(count)
            return value
        }
        let session = try field()
        guard [32, 48, 64].contains(session.count), offset < bytes.count, bytes[offset] == 50 else {
            throw NativeSSHError.invalidChallenge
        }
        offset += 1
        guard try field() == Data(identity.user.utf8), try field() == Data("ssh-connection".utf8),
              try field() == Data("publickey".utf8), offset < bytes.count, bytes[offset] == 1 else {
            throw NativeSSHError.invalidChallenge
        }
        offset += 1
        let algorithmBytes = try field()
        guard identity.publicKey.count >= 4 else { throw NativeSSHError.invalidChallenge }
        let keyAlgorithmLength = identity.publicKey.prefix(4).reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
        guard (1...64).contains(keyAlgorithmLength), keyAlgorithmLength <= identity.publicKey.count - 4,
              let keyAlgorithm = String(data: identity.publicKey.subdata(in: 4..<4 + Int(keyAlgorithmLength)), encoding: .ascii) else {
            throw NativeSSHError.invalidChallenge
        }
        guard let algorithm = String(data: algorithmBytes, encoding: .ascii),
              keyAlgorithm == "ssh-rsa"
                ? ["rsa-sha2-256", "rsa-sha2-512"].contains(algorithm)
                : (["ssh-ed25519", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521"].contains(keyAlgorithm)
                   && algorithm == keyAlgorithm),
              try field() == identity.publicKey, offset == bytes.count else { throw NativeSSHError.invalidChallenge }
        self.transportID = transportID; sessionIdentifier = session
        self.identity = identity; publicKeyAlgorithm = keyAlgorithm; self.algorithm = algorithm; signedData = bytes
    }
}

/// The application supplies endpoint/revision/host-pin and algorithm policy and
/// the protected signing broker. There is no default trust policy or key loader.
/// Calls must cooperate with cancellation; cancel must interrupt a native prompt.
/// A cancelled callback retains its owner until it settles, but cannot revive SSH.
public protocol NativeSSHAuthentication: Sendable {
    func identity(for peer: NativeSSHPeer) async throws -> NativeSSHIdentity
    func signature(for challenge: NativeSSHChallenge, peer: NativeSSHPeer) async throws -> Data
    func validate(peer: NativeSSHPeer, identity: NativeSSHIdentity) async throws
    func cancel(transportID: UUID)
}

/// A selected account is public policy, never a password or a synthetic key.
public struct NativeSSHPasswordAccount: Sendable, Equatable {
    public let user: String
    public init(user: String) throws {
        guard !user.isEmpty, user.utf8.count <= 256,
              !user.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains) else {
            throw NativeSSHError.invalidInput
        }
        self.user = user
    }
}

/// A foreground password prompt belongs to one verified exchange and attempt.
/// account(for:) must verify the exact peer and imported password policy before
/// returning. The driver asks for a secret only after the server advertises the
/// SSH password method. Keyboard-interactive and password changes are separate.
/// Implementations must retire pending prompts on cancel and must not persist
/// the response. validate is called before and after prompting and while live.
public protocol NativeSSHPasswordAuthentication: Sendable {
    func account(for peer: NativeSSHPeer) async throws -> NativeSSHPasswordAccount
    func password(for account: NativeSSHPasswordAccount, peer: NativeSSHPeer) async throws -> Data
    func validate(peer: NativeSSHPeer, account: NativeSSHPasswordAccount) async throws
    func cancel(transportID: UUID)
}

public struct NativeSSHGeometry: Sendable, Equatable {
    public let columns: UInt32, rows: UInt32, pixelWidth: UInt32, pixelHeight: UInt32
    public init(columns: UInt32, rows: UInt32, pixelWidth: UInt32 = 0, pixelHeight: UInt32 = 0) throws {
        guard (1...512).contains(columns), (1...512).contains(rows), columns * rows <= 65536,
              pixelWidth <= 65535, pixelHeight <= 65535 else { throw NativeSSHError.invalidInput }
        self.columns = columns; self.rows = rows; self.pixelWidth = pixelWidth; self.pixelHeight = pixelHeight
    }
}

public struct NativeSSHAccounting: Sendable, Equatable {
    public let allocatedBytes: Int, peakAllocatedBytes: Int, pendingWriteBytes: Int
    public let locallyAcceptedBytes: UInt64, channelAcceptedBytes: UInt64
    public let terminalError: Int32
    /// Bytes read from the server, including replies to keepalive probes.
    public let receivedBytes: UInt64
}

public enum NativeSSHWriteAdmission: Sendable, Equatable { case accepted(Int), wouldBlock }
/// The result of one liveness probe. A deferred probe sent nothing because
/// output or a library packet is still pending; unavailable means this
/// platform cannot prove a probe would not be left half sent.
public enum NativeSSHKeepalive: Sendable, Equatable { case sent, deferred, unavailable }
public enum NativeSSHRemoteExit: Sendable, Equatable {
    case exitStatus(UInt32)
    case signal(String)
}
public struct NativeSSHCompletion: Sendable, Equatable {
    /// A zero getter result in libssh2 cannot establish that a status was sent.
    /// Both absent status and reported zero therefore remain nil here.
    public let remoteExit: NativeSSHRemoteExit?
    /// False means drained output EOF followed by expiry of the bounded wait.
    public let channelClosed: Bool
}
public struct NativeSSHServiceEvent: Sendable {
    /// At most 16 KiB; consume through the canonical terminal before asking again.
    public let output: Data
    public let endOfOutput: Bool
    public let completion: NativeSSHCompletion?
    /// A readiness interval expired without discarding channel state or queued
    /// bytes. An idle terminal is not a dead connection. Keepalive/failure policy
    /// belongs to the session owner, which can explicitly close the connection.
    public let waitExpired: Bool
    /// Local window-change completion only; ordinary SSH has no geometry ACK.
    public let resized: NativeSSHGeometry?
    public let accounting: NativeSSHAccounting
}

/// Issued only by the live channel owner. Other targets in this package may
/// drive that channel without exposing a reusable raw-I/O capability to apps.
package struct NativeSSHDriver: Equatable, Sendable {
    fileprivate let id = UUID()
}

/// A single actor owns one libssh2 session/channel. Network progress uses dispatch
/// readiness, not sleeps/poll loops or a thread per connection. No call returns a
/// libssh2 pointer. Authentication, shell creation and each service request have
/// finite monotonic deadlines. The app retains this owner while the pane is hidden.
public actor NativeSSHSession {
    public nonisolated static let maximumWriteBytes = Int(FTSSH_WRITE_CAPACITY)
    public nonisolated let transportID: UUID
    /// This session creates at most one shell channel. A replacement connection
    /// always gets a new channel identity, even when its endpoint is unchanged.
    public nonisolated let channelID = UUID()
    public nonisolated let endpoint: NativeSSHEndpoint?
    private enum Phase { case fresh, authenticating, authenticated, opening, live, closed }
    private let native: NativeHandle
    private var phase = Phase.fresh
    private var operation: UUID?
    private var waiter: SocketReadiness?
    private var shutdown: Task<Void, Never>?
    private var authentication: (any NativeSSHAuthentication)?
    private var passwordAuthentication: (any NativeSSHPasswordAuthentication)?
    private enum ForwardingWitness: Sendable {
        case publicKey(any NativeSSHAuthentication, NativeSSHPeer, NativeSSHIdentity)
        case password(any NativeSSHPasswordAuthentication, NativeSSHPeer, NativeSSHPasswordAccount)

        func validate() async throws {
            switch self {
            case let .publicKey(authority, peer, identity): try await authority.validate(peer: peer, identity: identity)
            case let .password(authority, peer, account): try await authority.validate(peer: peer, account: account)
            }
        }
        func cancel() {
            switch self {
            case let .publicKey(authority, peer, _): authority.cancel(transportID: peer.transportID)
            case let .password(authority, peer, _): authority.cancel(transportID: peer.transportID)
            }
        }
    }
    private var forwardingWitnesses: [ForwardingWitness] = []
    private var authorization: ForwardingWitness?
    private var requestedResize: NativeSSHGeometry?
    private var outputEnded = false
    private var completionDeadline: ContinuousClock.Instant?
    private var observedRemoteExit: NativeSSHRemoteExit?
    private var completion: NativeSSHCompletion?
    private var driver: NativeSSHDriver?
    private var legacyChannelID: UUID?
    private enum ChannelAuthorization {
        case legacy(UUID?)
        case driver(NativeSSHDriver)
    }
    private var channelDriven = false, serviceWakePending = false

    /// On success consumes the already-connected descriptor. On failure the caller
    /// must close it. DNS/connect are deliberately owned by the endpoint connector.
    public init(connectedDescriptor: Int32, transportID: UUID = UUID(), endpoint: NativeSSHEndpoint? = nil,
                maximumNativeBytes: Int = 2 * 1024 * 1024) throws {
        var handle: OpaquePointer?
        let result = ftssh_create(connectedDescriptor, maximumNativeBytes, &handle)
        guard result == FTSSH_OK, let handle else { throw NativeSSHError.native(result) }
        native = NativeHandle(handle); self.transportID = transportID; self.endpoint = endpoint
    }

    private init(native: NativeHandle, endpoint: NativeSSHEndpoint, witnesses: [ForwardingWitness]) {
        self.native = native; self.endpoint = endpoint; transportID = UUID()
        forwardingWitnesses = witnesses
    }

    /// Creates an independently authenticated SSH transport inside this exact
    /// approved hop. Success consumes this outer owner; only the returned actor
    /// can drive either library session. There is no transit PTY, exec, socket
    /// listener or shared forwarding transport. At most three transit sessions
    /// belong to the returned chain; every ancestor retains its own authority.
    public func forward(to endpoint: NativeSSHEndpoint, timeout: Duration = .seconds(15),
                        maximumNativeBytes: Int = 2 * 1024 * 1024) async throws -> NativeSSHSession {
        let deadline = try deadline(timeout)
        guard phase == .authenticated, operation == nil,
              let authority = authorization else {
            throw phase == .closed ? NativeSSHError.closed : .busy
        }
        guard forwardingWitnesses.count < Int(FTSSH_MAX_JUMP_HOPS) else { throw NativeSSHError.native(Int32(FTSSH_UNSUPPORTED)) }
        guard (65536...4 * 1024 * 1024).contains(maximumNativeBytes) else { throw NativeSSHError.invalidInput }
        try Task.checkCancellation()
        let token = UUID(); operation = token; phase = .opening
        let timer = expiry(deadline, operation: token)
        defer { timer.cancel(); if operation == token { operation = nil } }
        do {
            return try await withTaskCancellationHandler {
                try await validateForwardingPath()
                try await authority.validate()
                try current(token, deadline)
                try requireOK(endpoint.host.withCString {
                    ftssh_begin_forward(native.pointer, $0, endpoint.host.utf8.count, endpoint.port)
                })
                while true {
                    try current(token, deadline)
                    let result = ftssh_forward_step(native.pointer)
                    if result == FTSSH_OK { break }
                    try requireAgain(result)
                    try await ready(directions: ftssh_directions(native.pointer), deadline: deadline)
                }
                try await validateForwardingPath()
                try await authority.validate()
                try current(token, deadline)
                let transferred = try native.forwarded(maximumNativeBytes: maximumNativeBytes)
                // No suspension point after native ownership moves. Closing
                // this consumed actor must never cancel the child's hop grant.
                let child = NativeSSHSession(native: transferred, endpoint: endpoint,
                    witnesses: forwardingWitnesses + [authority])
                forwardingWitnesses.removeAll()
                authentication = nil; passwordAuthentication = nil; authorization = nil
                phase = .closed; operation = nil
                return child
            } onCancel: { [weak self] in Task { await self?.close() } }
        } catch {
            await close()
            if Task.isCancelled { throw CancellationError() }
            throw error
        }
    }

    /// The live workspace owner repeats this along with destination policy.
    /// Reimport, host/credential revocation and foreground loss at the hop
    /// invalidate the dependent connection even after both logins succeeded.
    public func validateForwardingPath() async throws {
        try Task.checkCancellation()
        guard phase != .closed else { throw NativeSSHError.closed }
        for witness in forwardingWitnesses {
            try await witness.validate()
            try Task.checkCancellation()
            guard phase != .closed else { throw NativeSSHError.closed }
        }
    }

    public func authenticate(using authority: any NativeSSHAuthentication, timeout: Duration = .seconds(60)) async throws -> NativeSSHPeer {
        let deadline = try deadline(timeout)
        guard phase == .fresh, operation == nil else { throw phase == .closed ? NativeSSHError.closed : .busy }
        try Task.checkCancellation()
        let token = UUID(); operation = token; phase = .authenticating; authentication = authority
        let transportID = self.transportID
        let timer = expiry(deadline, operation: token)
        defer { timer.cancel(); if operation == token { operation = nil } }
        do {
            return try await withTaskCancellationHandler {
                try await validateForwardingPath()
                while true {
                    try current(token, deadline)
                    let result = ftssh_handshake(native.pointer)
                    if result == FTSSH_OK { break }
                    try requireAgain(result)
                    try await ready(directions: ftssh_directions(native.pointer), deadline: deadline)
                }
                let found = try copyPeer()
                try await validateForwardingPath()
                try current(token, deadline)
                let selected = try await authority.identity(for: found)
                try await validateForwardingPath()
                try current(token, deadline)
                try requireOK(found.hostKey.withUnsafeBytes {
                    ftssh_approve_host_key(native.pointer, $0.bindMemory(to: UInt8.self).baseAddress, found.hostKey.count)
                })
                try requireOK(selected.user.withCString { user in selected.publicKey.withUnsafeBytes { key in
                    ftssh_begin_authentication(native.pointer, user, selected.user.utf8.count,
                                               key.bindMemory(to: UInt8.self).baseAddress, selected.publicKey.count)
                } })
                var signed = false
                while true {
                    try current(token, deadline)
                    let result = ftssh_authentication_step(native.pointer)
                    if result == FTSSH_OK { break }
                    if result == FTSSH_SIGNATURE_NEEDED {
                        guard !signed else { throw NativeSSHError.invalidChallenge }
                        let bytes = try copyBytes(capacity: Int(FTSSH_CHALLENGE_CAPACITY), ftssh_copy_challenge)
                        let challenge = try NativeSSHChallenge(bytes: bytes, transportID: transportID, identity: selected)
                        try await validateForwardingPath()
                        try current(token, deadline)
                        let signature = try await authority.signature(for: challenge, peer: found)
                        try current(token, deadline)
                        try await authority.validate(peer: found, identity: selected)
                        try await validateForwardingPath()
                        try current(token, deadline)
                        guard (1...Int(FTSSH_SIGNATURE_CAPACITY)).contains(signature.count) else { throw NativeSSHError.invalidChallenge }
                        try requireOK(signature.withUnsafeBytes {
                            ftssh_supply_signature(native.pointer, $0.bindMemory(to: UInt8.self).baseAddress, signature.count)
                        })
                        signed = true
                    } else {
                        try requireAgain(result)
                        try await ready(directions: ftssh_directions(native.pointer), deadline: deadline)
                    }
                }
                try await authority.validate(peer: found, identity: selected)
                try await validateForwardingPath()
                try current(token, deadline)
                authorization = .publicKey(authority, found, selected); phase = .authenticated
                return found
            } onCancel: { [weak self] in
                authority.cancel(transportID: transportID)
                Task { await self?.close() }
            }
        } catch {
            await close()
            if Task.isCancelled { throw CancellationError() }
            if ContinuousClock.now >= deadline || error as? NativeSSHError == .timedOut {
                throw NativeSSHError.authenticationTimedOut
            }
            throw error
        }
    }

    /// One explicit password attempt. The native method probe precedes the UI
    /// request; the secret is supplied once and retained only for this attempt.
    public func authenticate(usingPassword authority: any NativeSSHPasswordAuthentication,
                             timeout: Duration = .seconds(60)) async throws -> NativeSSHPeer {
        let deadline = try deadline(timeout)
        guard phase == .fresh, operation == nil else { throw phase == .closed ? NativeSSHError.closed : .busy }
        try Task.checkCancellation()
        let token = UUID(); operation = token; phase = .authenticating; passwordAuthentication = authority
        let transportID = self.transportID
        let timer = expiry(deadline, operation: token)
        defer { timer.cancel(); if operation == token { operation = nil } }
        do {
            return try await withTaskCancellationHandler {
                try await validateForwardingPath()
                while true {
                    try current(token, deadline)
                    let result = ftssh_handshake(native.pointer)
                    if result == FTSSH_OK { break }
                    try requireAgain(result)
                    try await ready(directions: ftssh_directions(native.pointer), deadline: deadline)
                }
                let found = try copyPeer()
                try await validateForwardingPath()
                try current(token, deadline)
                let account = try await authority.account(for: found)
                try await authority.validate(peer: found, account: account)
                try await validateForwardingPath()
                try current(token, deadline)
                try requireOK(found.hostKey.withUnsafeBytes {
                    ftssh_approve_host_key(native.pointer, $0.bindMemory(to: UInt8.self).baseAddress, found.hostKey.count)
                })
                try requireOK(account.user.withCString {
                    ftssh_begin_password_authentication(native.pointer, $0, account.user.utf8.count)
                })
                var supplied = false
                while true {
                    try current(token, deadline)
                    let result = ftssh_authentication_step(native.pointer)
                    if result == FTSSH_OK { break }
                    if result == FTSSH_PASSWORD_NEEDED {
                        guard !supplied else { throw NativeSSHError.invalidChallenge }
                        try await authority.validate(peer: found, account: account)
                        try await validateForwardingPath()
                        try current(token, deadline)
                        var secret = try await authority.password(for: account, peer: found)
                        defer { secret.resetBytes(in: 0..<secret.count) }
                        try current(token, deadline)
                        try await authority.validate(peer: found, account: account)
                        try await validateForwardingPath()
                        try current(token, deadline)
                        guard (1...Int(FTSSH_PASSWORD_CAPACITY)).contains(secret.count) else { throw NativeSSHError.invalidInput }
                        try requireOK(secret.withUnsafeBytes {
                            ftssh_supply_password(native.pointer, $0.bindMemory(to: UInt8.self).baseAddress, secret.count)
                        })
                        supplied = true
                    } else {
                        try requireAgain(result)
                        try await ready(directions: ftssh_directions(native.pointer), deadline: deadline)
                    }
                }
                guard supplied else { throw NativeSSHError.invalidChallenge }
                try await authority.validate(peer: found, account: account)
                try await validateForwardingPath()
                try current(token, deadline)
                authorization = .password(authority, found, account); phase = .authenticated
                return found
            } onCancel: { [weak self] in
                authority.cancel(transportID: transportID)
                Task { await self?.close() }
            }
        } catch {
            await close()
            if Task.isCancelled { throw CancellationError() }
            if ContinuousClock.now >= deadline || error as? NativeSSHError == .timedOut {
                throw NativeSSHError.authenticationTimedOut
            }
            throw error
        }
    }

    /// Explicit remote side effect. Does not occur as part of authenticate/import.
    public func openShell(geometry: NativeSSHGeometry, timeout: Duration = .seconds(15)) async throws {
        try await openChannel(timeout: timeout) {
            ftssh_begin_shell(native.pointer, geometry.columns, geometry.rows, geometry.pixelWidth, geometry.pixelHeight)
        }
    }

    /// Runs one remote command instead of a shell: no PTY, so the channel
    /// carries its bytes unchanged (a mux socket relay such as
    /// `nc -N -U <socket>`), and its stderr is discarded. Printable ASCII only.
    public func openCommand(_ command: String, timeout: Duration = .seconds(15)) async throws {
        let bytes = Array(command.utf8)
        guard !bytes.isEmpty, bytes.count <= Int(FTSSH_COMMAND_CAPACITY),
              bytes.allSatisfy({ (32...126).contains($0) }) else { throw NativeSSHError.invalidInput }
        try await openChannel(timeout: timeout) {
            bytes.withUnsafeBufferPointer { buffer in
                buffer.withMemoryRebound(to: CChar.self) { ftssh_begin_command(native.pointer, $0.baseAddress, bytes.count) }
            }
        }
    }

    private func openChannel(timeout: Duration, begin: () -> Int32) async throws {
        let deadline = try deadline(timeout)
        guard phase == .authenticated, operation == nil, let authority = authorization else {
            throw phase == .closed ? NativeSSHError.closed : .busy
        }
        try Task.checkCancellation()
        let token = UUID(); operation = token; phase = .opening
        let timer = expiry(deadline, operation: token)
        defer { timer.cancel(); if operation == token { operation = nil } }
        do {
            try await withTaskCancellationHandler {
                try await authority.validate()
                try await validateForwardingPath()
                try current(token, deadline)
                try requireOK(begin())
                while true {
                    try current(token, deadline)
                    let result = ftssh_shell_step(native.pointer)
                    if result == FTSSH_OK { break }
                    try requireAgain(result)
                    try await ready(directions: ftssh_directions(native.pointer), deadline: deadline)
                }
                try await authority.validate()
                try await validateForwardingPath()
                try current(token, deadline)
                phase = .live
            } onCancel: { [weak self] in Task { await self?.close() } }
        } catch {
            await close()
            if Task.isCancelled { throw CancellationError() }
            if ContinuousClock.now >= deadline { throw NativeSSHError.timedOut }
            throw error
        }
    }

    /// Copies one whole bounded unit or none. This is local adapter acceptance,
    /// never remote execution. The session owner must drive service afterward.
    public func admit(_ bytes: Data, channelID: UUID? = nil) throws -> NativeSSHWriteAdmission {
        try admit(bytes, authorized: .legacy(channelID))
    }
    package func admit(_ bytes: Data, driver: NativeSSHDriver) throws -> NativeSSHWriteAdmission {
        try admit(bytes, authorized: .driver(driver))
    }
    private func admit(_ bytes: Data, authorized: ChannelAuthorization) throws -> NativeSSHWriteAdmission {
        try Task.checkCancellation()
        guard phase == .live, completion == nil else { throw NativeSSHError.closed }
        try requireDriver(authorized)
        guard !bytes.isEmpty, bytes.count <= Self.maximumWriteBytes else { throw NativeSSHError.invalidInput }
        channelDriven = true
        // This owner ends an output-EOF attachment after bounded metadata
        // settlement. Do not accept new bytes into an undriven native slot;
        // the coordinator retains them as unsent on their original source.
        if requestedResize != nil || outputEnded { return .wouldBlock }
        let result = bytes.withUnsafeBytes {
            ftssh_admit_write(native.pointer, $0.bindMemory(to: UInt8.self).baseAddress, bytes.count)
        }
        if result == FTSSH_AGAIN { return .wouldBlock }
        try requireOK(result)
        waiter?.wake()
        return .accepted(bytes.count)
    }

    /// One liveness probe between service turns: a keepalive request that asks
    /// the server to reply, which advances `receivedBytes`. The owner decides
    /// how many silent intervals mean the server is gone. Call at least 2 s apart.
    package func keepalive(driver: NativeSSHDriver) throws -> NativeSSHKeepalive {
        guard phase == .live, completion == nil else { throw NativeSSHError.closed }
        try requireDriver(.driver(driver))
        guard operation == nil else { throw NativeSSHError.busy }
        let result = ftssh_keepalive(native.pointer)
        if result == FTSSH_OK { return .sent }
        if result == FTSSH_AGAIN { return .deferred }
        if result == FTSSH_UNSUPPORTED { return .unavailable }
        throw NativeSSHError.native(result)
    }

    /// One pending immutable resize. Retry after its service completion, rather
    /// than overwriting a request that libssh2 has partially transmitted.
    public func requestResize(_ geometry: NativeSSHGeometry, channelID: UUID? = nil) throws {
        try requestResize(geometry, authorized: .legacy(channelID))
    }
    package func requestResize(_ geometry: NativeSSHGeometry, driver: NativeSSHDriver) throws {
        try requestResize(geometry, authorized: .driver(driver))
    }
    private func requestResize(_ geometry: NativeSSHGeometry, authorized: ChannelAuthorization) throws {
        try Task.checkCancellation()
        guard phase == .live, completion == nil else { throw NativeSSHError.closed }
        try requireDriver(authorized)
        guard requestedResize == nil, !outputEnded else { throw NativeSSHError.busy }
        channelDriven = true; requestedResize = geometry; waiter?.wake()
    }

    /// Coalesces an owner notification even before service starts. The next
    /// service turn still performs bounded I/O, then returns without waiting for
    /// peer traffic. It does not cancel the connection or discard pending writes.
    /// This legacy notification is a no-op for a capability-owned attachment,
    /// even when its public channel UUID is supplied. It never grants ownership.
    public func wakeService(channelID: UUID? = nil) {
        guard phase == .live else { return }
        do { try requireDriver(.legacy(channelID)) } catch { return }
        serviceWakePending = true; waiter?.wake()
    }
    package func wakeService(driver: NativeSSHDriver) throws {
        guard phase == .live else { throw NativeSSHError.closed }
        try requireDriver(.driver(driver))
        serviceWakePending = true; waiter?.wake()
    }

    /// Compatibility for existing explicit UUID-bound channel drivers. The UUID
    /// authorizes that legacy surface only; it cannot access a package-capability
    /// attachment. Either claim permanently excludes every competing driver.
    public func claimTerminalChannel(_ channelID: UUID) throws {
        try Task.checkCancellation()
        guard phase == .live, !outputEnded else { throw NativeSSHError.closed }
        guard driver == nil, legacyChannelID == nil, operation == nil, !channelDriven else {
            throw NativeSSHError.busy
        }
        legacyChannelID = channelID
    }

    /// A shell can transfer into the terminal driver exactly once and only
    /// before raw channel I/O has consumed output or admitted input. No prior
    /// effects or partially transmitted resize can silently cross this boundary.
    package func claimTerminalDriver(transportID: UUID, channelID: UUID) throws -> NativeSSHDriver {
        try Task.checkCancellation()
        guard transportID == self.transportID, channelID == self.channelID else { throw NativeSSHError.invalidInput }
        guard phase == .live else { throw phase == .closed ? NativeSSHError.closed : .busy }
        guard driver == nil, legacyChannelID == nil, operation == nil, !channelDriven else { throw NativeSSHError.busy }
        let issued = NativeSSHDriver()
        driver = issued
        return issued
    }
    private func requireDriver(_ authorized: ChannelAuthorization) throws {
        switch authorized {
        case .legacy(let channel):
            guard driver == nil else { throw NativeSSHError.busy }
            guard legacyChannelID == channel else { throw NativeSSHError.invalidInput }
        case .driver(let candidate):
            guard driver == candidate, legacyChannelID == nil else { throw NativeSSHError.busy }
        }
    }

    /// One bounded receive/pump call at a time; admit and close remain available
    /// during readiness waits. At EOF consume optional completion evidence for
    /// at most two seconds, using readiness and bounded native work. Final bytes
    /// are delivered first; absent/ambiguous exit status is never invented.
    /// A readiness timeout preserves the connection and exact pending
    /// writes; cancellation explicitly aborts this owner, not an unrelated pane.
    public func service(timeout: Duration = .seconds(120), channelID: UUID? = nil) async throws -> NativeSSHServiceEvent {
        try await service(timeout: timeout, authorized: .legacy(channelID))
    }
    package func service(timeout: Duration = .seconds(120), driver: NativeSSHDriver) async throws -> NativeSSHServiceEvent {
        try await service(timeout: timeout, authorized: .driver(driver))
    }
    private func service(timeout: Duration, authorized: ChannelAuthorization) async throws -> NativeSSHServiceEvent {
        let deadline = try deadline(timeout)
        guard phase == .live else { throw NativeSSHError.closed }
        try requireDriver(authorized)
        guard operation == nil else { throw NativeSSHError.busy }
        try Task.checkCancellation()
        let token = UUID(); operation = token; channelDriven = true
        defer { if operation == token { operation = nil } }
        do {
            return try await withTaskCancellationHandler {
                while true {
                    try current(token, deadline)
                    if let completion { return try completedEvent(completion) }
                    if outputEnded {
                        var evidence = ftssh_completion()
                        let result = ftssh_completion_step(native.pointer, &evidence)
                        guard result == FTSSH_OK || result == FTSSH_AGAIN || result == FTSSH_YIELD else {
                            throw NativeSSHError.native(result)
                        }
                        if evidence.kind == FTSSH_EXIT_STATUS {
                            observedRemoteExit = .exitStatus(evidence.exit_status)
                        } else if evidence.kind == FTSSH_EXIT_SIGNAL {
                            let name = withUnsafeBytes(of: evidence.signal) {
                                String(decoding: $0.prefix(while: { $0 != 0 }), as: UTF8.self)
                            }
                            observedRemoteExit = .signal(name)
                        }
                        guard let completionDeadline else { throw NativeSSHError.invalidInput }
                        if result == FTSSH_OK || ContinuousClock.now >= completionDeadline {
                            let final = NativeSSHCompletion(remoteExit: observedRemoteExit, channelClosed: result == FTSSH_OK)
                            completion = final
                            return try completedEvent(final)
                        }
                        if result == FTSSH_YIELD { await Task.yield(); continue }
                        do { try await ready(directions: ftssh_directions(native.pointer), deadline: min(deadline, completionDeadline)) }
                        catch NativeSSHError.timedOut {
                            if phase == .live, operation == token, !Task.isCancelled,
                               ContinuousClock.now >= completionDeadline {
                                let final = NativeSSHCompletion(remoteExit: observedRemoteExit, channelClosed: false)
                                completion = final
                                return try completedEvent(final)
                            }
                            throw NativeSSHError.timedOut
                        }
                        continue
                    }
                    let before = try accounting()
                    let flush = ftssh_flush_write(native.pointer)
                    guard flush == FTSSH_OK || flush == FTSSH_AGAIN else { throw NativeSSHError.native(flush) }
                    var directions = flush == FTSSH_AGAIN ? ftssh_directions(native.pointer) : 0
                    var resized: NativeSSHGeometry?
                    if let requested = requestedResize, try accounting().pendingWriteBytes == 0 {
                        let result = ftssh_resize(native.pointer, requested.columns, requested.rows, requested.pixelWidth, requested.pixelHeight)
                        if result == FTSSH_OK { requestedResize = nil; resized = requested }
                        else { try requireAgain(result); directions |= ftssh_directions(native.pointer) }
                    }
                    var output = Data(count: 16384), count = 0
                    let result = output.withUnsafeMutableBytes {
                        ftssh_read(native.pointer, $0.bindMemory(to: UInt8.self).baseAddress, 16384, &count)
                    }
                    guard result == FTSSH_OK || result == FTSSH_AGAIN || result == FTSSH_EOF else { throw NativeSSHError.native(result) }
                    output.count = count
                    if result == FTSSH_EOF { outputEnded = true; completionDeadline = .now + .seconds(2) }
                    if result == FTSSH_AGAIN { directions |= ftssh_directions(native.pointer) }
                    let after = try accounting()
                    if count > 0 || resized != nil || serviceWakePending
                        || before.pendingWriteBytes != after.pendingWriteBytes {
                        serviceWakePending = false
                        return .init(output: output, endOfOutput: false, completion: nil, waitExpired: false, resized: resized, accounting: after)
                    }
                    if outputEnded { continue }
                    try await ready(directions: directions, deadline: deadline)
                }
            } onCancel: { [weak self] in Task { await self?.close() } }
        } catch NativeSSHError.timedOut {
            guard phase == .live, operation == token, !Task.isCancelled else {
                await close(); throw NativeSSHError.closed
            }
            return .init(output: Data(), endOfOutput: false, completion: nil, waitExpired: true, resized: nil, accounting: try accounting())
        } catch { await close(); throw error }
    }

    private func completedEvent(_ completion: NativeSSHCompletion) throws -> NativeSSHServiceEvent {
        serviceWakePending = false
        return .init(output: Data(), endOfOutput: true, completion: completion, waitExpired: false,
                     resized: nil, accounting: try accounting())
    }

    public func accounting() throws -> NativeSSHAccounting {
        let stats = try native.accounting()
        return .init(allocatedBytes: stats.allocated_bytes, peakAllocatedBytes: stats.peak_allocated_bytes,
                     pendingWriteBytes: stats.pending_write_bytes, locallyAcceptedBytes: stats.accepted_bytes,
                     channelAcceptedBytes: stats.channel_accepted_bytes, terminalError: stats.terminal_error,
                     receivedBytes: stats.received_bytes)
    }

    /// Immediate native invalidation precedes every suspension point. Concurrent
    /// close calls await the same readiness teardown; delayed policy/signature
    /// results cannot use the closed handle. Final stats remain readable.
    public func close() async {
        if let shutdown { await shutdown.value; return }
        phase = .closed; operation = nil
        authentication?.cancel(transportID: transportID)
        passwordAuthentication?.cancel(transportID: transportID)
        for witness in forwardingWitnesses { witness.cancel() }
        forwardingWitnesses.removeAll()
        authentication = nil; passwordAuthentication = nil; authorization = nil
        requestedResize = nil; serviceWakePending = false
        native.close()
        let old = waiter; waiter = nil
        let task = Task<Void, Never> { await old?.stop() }
        shutdown = task
        await task.value
    }

    private func deadline(_ timeout: Duration) throws -> ContinuousClock.Instant {
        guard timeout > .zero, timeout <= .seconds(120) else { throw NativeSSHError.invalidInput }
        return .now + timeout
    }
    private func expiry(_ deadline: ContinuousClock.Instant, operation token: UUID) -> Task<Void, Never> {
        Task { [weak self] in
            do {
                try await ContinuousClock().sleep(until: deadline)
                try Task.checkCancellation()
                await self?.expire(token, deadline: deadline)
            }
            catch { }
        }
    }
    private func expire(_ token: UUID, deadline: ContinuousClock.Instant) async {
        // A cancelled old timer may already be queued on the actor. It cannot
        // close a subsequent operation or an idle authenticated connection.
        guard operation == token, ContinuousClock.now >= deadline else { return }
        await close()
    }
    private func current(_ token: UUID, _ deadline: ContinuousClock.Instant) throws {
        try Task.checkCancellation()
        guard ContinuousClock.now < deadline else { throw NativeSSHError.timedOut }
        guard phase != .closed, operation == token else { throw NativeSSHError.closed }
    }
    private func ready(directions: Int32, deadline: ContinuousClock.Instant) async throws {
        if directions & Int32(FTSSH_RETRY) != 0 {
            try Task.checkCancellation()
            guard ContinuousClock.now < deadline else { throw NativeSSHError.timedOut }
            await Task.yield()
            return
        }
        let next = try SocketReadiness(descriptor: ftssh_descriptor(native.pointer), directions: directions, deadline: deadline)
        waiter = next
        defer { if waiter === next { waiter = nil } }
        try await next.wait()
    }
    private func copyBytes(capacity: Int, _ copy: (OpaquePointer?, UnsafeMutablePointer<UInt8>?, Int, UnsafeMutablePointer<Int>?) -> Int32) throws -> Data {
        var bytes = Data(count: capacity), count = 0
        let result = bytes.withUnsafeMutableBytes { copy(native.pointer, $0.bindMemory(to: UInt8.self).baseAddress, capacity, &count) }
        try requireOK(result); bytes.count = count; return bytes
    }
    private func copyPeer() throws -> NativeSSHPeer {
        let key = try copyBytes(capacity: 65536, ftssh_copy_host_key)
        func method(_ id: Int32) throws -> String {
            var bytes = [CChar](repeating: 0, count: 129)
            try requireOK(ftssh_copy_method(native.pointer, id, &bytes, bytes.count))
            return String(decoding: bytes.prefix(while: { $0 != 0 }).map(UInt8.init(bitPattern:)), as: UTF8.self)
        }
        return try .init(transportID: transportID, endpoint: endpoint, hostKey: key,
            keyExchange: method(0), hostSignature: method(1), outboundCipher: method(2), inboundCipher: method(3),
            outboundMAC: method(4), inboundMAC: method(5), outboundCompression: method(6), inboundCompression: method(7))
    }
    private func requireOK(_ code: Int32) throws { guard code == FTSSH_OK else { throw NativeSSHError.native(code) } }
    private func requireAgain(_ code: Int32) throws { guard code == FTSSH_AGAIN else { throw NativeSSHError.native(code) } }
}

/// Access is actor-confined until deinit. The C destroy API retains ownership on
/// an unexpected library teardown error instead of freeing still-owned memory.
/// Native handle admission stays charged in that exceptional quarantine case.
private final class NativeHandle: @unchecked Sendable {
    private(set) var pointer: OpaquePointer?
    private var finalAccounting: ftssh_stats?
    init(_ pointer: OpaquePointer) { self.pointer = pointer }
    func forwarded(maximumNativeBytes: Int) throws -> NativeHandle {
        var child: OpaquePointer?
        let result = ftssh_forward_adopt(&pointer, maximumNativeBytes, &child)
        guard result == FTSSH_OK, let child else { throw NativeSSHError.native(result) }
        // This wrapper owns no native resources after transfer. The child's
        // aggregate accounting includes the retained outer library allocations.
        finalAccounting = ftssh_stats()
        return NativeHandle(child)
    }
    func accounting() throws -> ftssh_stats {
        if let finalAccounting { return finalAccounting }
        var stats = ftssh_stats()
        let result = ftssh_get_stats(pointer, &stats)
        guard result == FTSSH_OK else { throw NativeSSHError.native(result) }
        return stats
    }
    func close() {
        guard pointer != nil else { return }
        let result = ftssh_close(pointer)
        var stats = ftssh_stats()
        let copied = ftssh_get_stats(pointer, &stats)
        if result == FTSSH_OK {
            let destroyed = ftssh_destroy(&pointer)
            if destroyed != FTSSH_OK { stats.terminal_error = destroyed }
        } else { stats.terminal_error = result }
        if copied != FTSSH_OK { stats.terminal_error = copied }
        finalAccounting = stats
        // Keeping a closed pane's metadata must not occupy a live native handle
        // or its global admission slot. Unexpected teardown errors retain the
        // pointer and explicit terminalError for quarantine instead of freeing it.
    }
    deinit { _ = ftssh_destroy(&pointer) }
}

/// One finite readiness registration owns a duplicated descriptor until *all*
/// dispatch cancellation handlers have run. Closing the session cannot cause an
/// event on a newly reused descriptor. State and callbacks are queue-confined.
final class SocketReadiness: @unchecked Sendable {
    private static let queue = DispatchQueue(label: "org.frankenterm.ssh.readiness")
    private let descriptor: Int32, directions: Int32, deadline: ContinuousClock.Instant
    private var sources: [any DispatchSourceProtocol] = []
    private var continuation: CheckedContinuation<Void, Error>?
    private var stopContinuation: CheckedContinuation<Void, Never>?
    private var outcome: Result<Void, Error>?
    private var cancelling = 0, installed = false, finished = false

    init(descriptor: Int32, directions: Int32, deadline: ContinuousClock.Instant) throws {
        guard descriptor >= 0 else { throw NativeSSHError.closed }
        let copy = fcntl(descriptor, F_DUPFD_CLOEXEC, 0)
        guard copy >= 0 else { throw NativeSSHError.native(Int32(FTSSH_IO)) }
        self.descriptor = copy; self.directions = directions == 0 ? Int32(FTSSH_READ) : directions
        self.deadline = deadline
    }
    func wake() { Self.queue.async { self.finish(.success(())) } }
    func wait() async throws {
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                Self.queue.async { self.install(continuation) }
            }
        } onCancel: { Self.queue.async { self.finish(.failure(CancellationError())) } }
    }
    func stop() async {
        await withCheckedContinuation { continuation in
            Self.queue.async {
                if self.finished { continuation.resume(); return }
                self.stopContinuation = continuation
                self.finish(.failure(NativeSSHError.closed))
            }
        }
    }
    private func install(_ continuation: CheckedContinuation<Void, Error>) {
        self.continuation = continuation; installed = true
        if outcome != nil { complete(); return }
        let remaining = ContinuousClock.now.duration(to: deadline)
        guard remaining > .zero else { finish(.failure(NativeSSHError.timedOut)); return }
        if directions & Int32(FTSSH_READ) != 0 {
            let read = DispatchSource.makeReadSource(fileDescriptor: descriptor, queue: Self.queue)
            read.setEventHandler { self.finish(.success(())) }; sources.append(read)
        }
        if directions & Int32(FTSSH_WRITE) != 0 {
            let write = DispatchSource.makeWriteSource(fileDescriptor: descriptor, queue: Self.queue)
            write.setEventHandler { self.finish(.success(())) }; sources.append(write)
        }
        let timer = DispatchSource.makeTimerSource(queue: Self.queue)
        let parts = remaining.components
        let nanos = parts.seconds * 1_000_000_000 + parts.attoseconds / 1_000_000_000
        timer.schedule(deadline: .now() + .nanoseconds(Int(max(1, nanos))))
        timer.setEventHandler { self.finish(.failure(NativeSSHError.timedOut)) }; sources.append(timer)
        for source in sources { source.setCancelHandler { self.cancelled() }; source.resume() }
    }
    private func finish(_ result: Result<Void, Error>) {
        guard outcome == nil else { return }
        outcome = result
        guard installed else { return }
        cancelling = sources.count
        if cancelling == 0 { complete() }
        else { for source in sources { source.cancel() } }
    }
    private func cancelled() {
        cancelling -= 1
        if cancelling == 0 { sources.removeAll(); complete() }
    }
    private func complete() {
        guard !finished, let outcome else { return }
        finished = true
        _ = systemClose(descriptor)
        continuation?.resume(with: outcome); continuation = nil
        stopContinuation?.resume(); stopContinuation = nil
    }
}

private func systemClose(_ descriptor: Int32) -> Int32 {
#if canImport(Darwin)
    Darwin.close(descriptor)
#else
    Glibc.close(descriptor)
#endif
}
