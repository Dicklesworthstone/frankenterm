import Foundation
import FrankenTermKit
import FrankenTermSSH

/// Connects the actual libssh2 exchange to the imported workspace and protected
/// credential broker. One instance belongs to one explicitly selected attempt;
/// cancellation permanently retires it. It never grants permission to network.
/// The app shares one foreground broker configured with routeSupport and checks
/// its offline/network scope before invoking NativeSSHConnector.
public actor WorkspaceNativeSSHAuthentication: NativeSSHAuthentication {
    /// This is the implemented adapter profile, not Apple-device qualification.
    /// Unknown mandatory policies remain blocked by WorkspaceStore. RSA host
    /// keys, certificates, password/MFA and jumps are not enabled. A mux route
    /// signs in exactly like a direct one; only the channel it opens differs.
    public static let routeSupport = WorkspaceDocument.RouteSupport(
        routeModes: ["direct_ssh", "frankenterm_mux"], authenticationMethods: [.publicKey],
        credentialAlgorithms: ["ssh-ed25519", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521"],
        hostKeyAlgorithms: ["ssh-ed25519", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521"])

    private final class Retirement: @unchecked Sendable {
        private let lock = NSLock()
        private var retired = false
        private var operation: (id: UUID, cancel: @Sendable () -> Void)?
        func retire() -> Bool {
            lock.lock()
            if retired { lock.unlock(); return false }
            retired = true
            let cancel = operation?.cancel
            lock.unlock()
            cancel?()
            return true
        }
        func install(_ id: UUID, cancel: @escaping @Sendable () -> Void) {
            lock.lock()
            if retired { lock.unlock(); cancel(); return }
            operation = (id, cancel)
            lock.unlock()
        }
        func finish(_ id: UUID) {
            lock.lock(); defer { lock.unlock() }
            if operation?.id == id { operation = nil }
        }
        func check() throws {
            lock.lock(); defer { lock.unlock() }
            if retired { throw SSHAuthenticationError.stale }
        }
    }
    private enum Phase { case fresh, selecting, selected, signing, signed }
    private nonisolated let retirement = Retirement()
    private nonisolated let broker: SSHAuthenticationBroker
    private nonisolated let transportID: UUID
    private let workspaceID: UUID, revision: UUID
    private let routeID: String, credentialID: String, endpoint: NativeSSHEndpoint
    private var phase = Phase.fresh
    private var selected: WorkspaceSSHIdentity?
    private var selectedPeer: NativeSSHPeer?
    private var selectedIdentity: NativeSSHIdentity?

    public init(broker: SSHAuthenticationBroker, workspaceID: UUID, revision: UUID,
                routeID: String, credentialID: String, transportID: UUID, endpoint: NativeSSHEndpoint) {
        self.broker = broker; self.workspaceID = workspaceID; self.revision = revision
        self.routeID = routeID; self.credentialID = credentialID
        self.transportID = transportID; self.endpoint = endpoint
    }

    public func identity(for peer: NativeSSHPeer) async throws -> NativeSSHIdentity {
        try checkCurrent()
        guard phase == .fresh else { throw SSHAuthenticationError.stale }
        phase = .selecting
        let id = UUID()
        let work = Task { try await selectIdentity(for: peer) }
        retirement.install(id) { work.cancel() }
        defer { retirement.finish(id) }
        return try await withTaskCancellationHandler {
            let result = try await work.value
            try checkCurrent()
            return result
        } onCancel: { self.cancel(transportID: self.transportID) }
    }

    private func selectIdentity(for peer: NativeSSHPeer) async throws -> NativeSSHIdentity {
        do {
            try checkCurrent()
            let host = try validatePeer(peer)
            let foreground = try await broker.foregroundLease()
            try checkCurrent()
            let authorization = try await broker.authorization(workspaceID: workspaceID, expectedRevision: revision,
                routeID: routeID, credentialID: credentialID, presentedHostKey: host)
            try await broker.validateForeground(foreground)
            try checkCurrent(); try validateRoute(authorization)
            let value = try await broker.publicIdentity(workspaceID: workspaceID, expectedRevision: revision,
                routeID: routeID, credentialID: credentialID, presentedHostKey: host, transportID: transportID)
            try await broker.validateForeground(foreground)
            try checkCurrent(); try validateRoute(value.authorization)
            guard value.authorization == authorization else { throw SSHAuthenticationError.stale }
            let identity = try NativeSSHIdentity(user: authorization.user, publicKey: value.identity.blob)
            selected = value; selectedPeer = peer; selectedIdentity = identity; phase = .selected
            return identity
        } catch { cancel(transportID: transportID); throw error }
    }

    public func signature(for challenge: NativeSSHChallenge, peer: NativeSSHPeer) async throws -> Data {
        try checkCurrent()
        guard phase == .selected, let selected, peer == selectedPeer,
              challenge.transportID == transportID, challenge.identity == selectedIdentity else {
            throw SSHAuthenticationError.stale
        }
        phase = .signing
        let id = UUID()
        let work = Task { try await sign(challenge, selected: selected, peer: peer) }
        retirement.install(id) { work.cancel() }
        defer { retirement.finish(id) }
        return try await withTaskCancellationHandler {
            let result = try await work.value
            try checkCurrent()
            return result
        } onCancel: { self.cancel(transportID: self.transportID) }
    }

    private func sign(_ challenge: NativeSSHChallenge, selected: WorkspaceSSHIdentity, peer: NativeSSHPeer) async throws -> Data {
        do {
            try checkCurrent()
            try await broker.validate(selected)
            try checkCurrent()
            let typed = try SSHUserAuthenticationChallenge(sessionIdentifier: challenge.sessionIdentifier,
                user: selected.authorization.user, publicBlob: selected.identity.blob)
            guard typed.algorithm == challenge.algorithm, typed.signedData == challenge.signedData else {
                throw SSHAuthenticationError.invalidChallenge
            }
            let request = try await broker.prepare(workspaceID: workspaceID, expectedRevision: revision,
                routeID: routeID, credentialID: credentialID, presentedHostKey: selected.authorization.presentedHostKey,
                transportID: transportID, sessionIdentifier: challenge.sessionIdentifier, publicBlob: selected.identity.blob)
            try checkCurrent()
            guard request.authorization == selected.authorization else { throw SSHAuthenticationError.stale }
            let result = try await broker.confirm(requestID: request.id)
            try checkCurrent()
            try await broker.validate(selected)
            try checkCurrent()
            guard result.requestID == request.id, result.transportID == transportID,
                  result.authorization == selected.authorization, result.sessionIdentifier == challenge.sessionIdentifier,
                  result.publicBlob == selected.identity.blob else { throw SSHAuthenticationError.stale }
            phase = .signed
            return result.signatureBlob
        } catch { cancel(transportID: transportID); throw error }
    }

    public func validate(peer: NativeSSHPeer, identity: NativeSSHIdentity) async throws {
        try checkCurrent()
        guard phase == .signed, peer == selectedPeer, identity == selectedIdentity else {
            throw SSHAuthenticationError.stale
        }
        try await validateCurrent()
    }

    /// A live session owner repeats this before byte admission, service, and
    /// resize. Durable revocation/reimport must invalidate established access as
    /// well as future signatures; a successful login is not a permanent grant.
    public func validateCurrent() async throws {
        try checkCurrent()
        guard phase == .signed, let selected, let peer = selectedPeer else { throw SSHAuthenticationError.stale }
        _ = try validatePeer(peer); try validateRoute(selected.authorization)
        try await broker.validate(selected)
        try checkCurrent()
    }

    public nonisolated func cancel(transportID: UUID) {
        guard transportID == self.transportID, retirement.retire() else { return }
        let broker = self.broker
        // Synchronous child cancellation also covers work not yet enqueued on
        // the broker. This one cleanup task releases an already prepared request.
        Task { await broker.cancel(transportID: transportID) }
    }

    private func checkCurrent() throws {
        if Task.isCancelled { cancel(transportID: transportID) }
        try Task.checkCancellation(); try retirement.check()
    }

    private func validateRoute(_ authorization: WorkspaceSSHAuthorization) throws {
        guard authorization.workspaceID == workspaceID, authorization.workspaceRevision == revision,
              authorization.routeID == routeID, authorization.credentialID == credentialID,
              authorization.host == endpoint.host, authorization.port == endpoint.port,
              Self.routeSupport.routeModes.contains(authorization.routeMode), authorization.jumpRouteIDs.isEmpty,
              Self.routeSupport.credentialAlgorithms.contains(authorization.algorithm) else {
            throw SSHAuthenticationError.denied
        }
    }

    private func validatePeer(_ peer: NativeSSHPeer) throws -> WorkspaceDocument.HostKey {
        let exchanges: Set<String> = ["curve25519-sha256", "curve25519-sha256@libssh.org",
            "ecdh-sha2-nistp256", "ecdh-sha2-nistp384", "ecdh-sha2-nistp521"]
        let ciphers: Set<String> = ["aes256-ctr", "aes128-ctr"]
        let macs: Set<String> = ["hmac-sha2-256-etm@openssh.com", "hmac-sha2-512-etm@openssh.com"]
        guard peer.transportID == transportID, peer.endpoint == endpoint,
              exchanges.contains(peer.keyExchange), ciphers.contains(peer.outboundCipher), ciphers.contains(peer.inboundCipher),
              macs.contains(peer.outboundMAC), macs.contains(peer.inboundMAC),
              peer.outboundCompression == "none", peer.inboundCompression == "none" else {
            throw SSHAuthenticationError.denied
        }
        let host = try SSHPublicIdentity(blob: peer.hostKey)
        guard peer.hostSignature == host.algorithm, Self.routeSupport.hostKeyAlgorithms.contains(host.algorithm) else {
            throw SSHAuthenticationError.denied
        }
        return try WorkspaceDocument.HostKey(algorithm: host.algorithm, fingerprint: host.fingerprint)
    }
}
