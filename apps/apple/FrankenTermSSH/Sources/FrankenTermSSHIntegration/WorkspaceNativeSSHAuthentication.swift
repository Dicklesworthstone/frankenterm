import Foundation
import FrankenTermKit
import FrankenTermSSH

protocol WorkspaceNativeSSHAuthority: Sendable {
    func validateCurrent() async throws
}

/// One password attempt for one actual peer. Account selection verifies local
/// policy and the exchanged host key before the native transport can request a
/// prompt. The password exists only during the single bounded authentication.
public actor WorkspaceNativeSSHPasswordAuthentication: NativeSSHPasswordAuthentication, WorkspaceNativeSSHAuthority {
    private enum Phase { case fresh, selecting, selected, requesting, supplied, authenticated }
    private nonisolated let retirement = WorkspaceNativeSSHAuthentication.Retirement()
    private nonisolated let broker: SSHAuthenticationBroker
    private nonisolated let transportID: UUID
    private let prompter: any WorkspaceSSHPasswordPrompting
    private let workspaceID: UUID, revision: UUID
    private let routeID: String, endpoint: NativeSSHEndpoint
    private let transitForRouteID: String?, allowedJumpRouteIDs: [String]
    private let hostKeyProbe: WorkspaceHostKeyProbe?
    private var phase = Phase.fresh
    private var selected: WorkspaceSSHPasswordIdentity?
    private var selectedPeer: NativeSSHPeer?
    private var selectedAccount: NativeSSHPasswordAccount?
    private var validatedPeer: (peer: NativeSSHPeer, host: WorkspaceNativeSSHAuthentication.HostIdentity)?

    public init(broker: SSHAuthenticationBroker, prompter: any WorkspaceSSHPasswordPrompting,
                workspaceID: UUID, revision: UUID, routeID: String, transportID: UUID,
                endpoint: NativeSSHEndpoint, transitForRouteID: String? = nil, allowedJumpRouteIDs: [String] = [],
                hostKeyProbe: WorkspaceHostKeyProbe? = nil) {
        self.broker = broker; self.prompter = prompter
        self.workspaceID = workspaceID; self.revision = revision; self.routeID = routeID
        self.transportID = transportID; self.endpoint = endpoint
        self.transitForRouteID = transitForRouteID; self.allowedJumpRouteIDs = allowedJumpRouteIDs
        self.hostKeyProbe = hostKeyProbe
    }

    public func account(for peer: NativeSSHPeer) async throws -> NativeSSHPasswordAccount {
        try checkCurrent()
        guard phase == .fresh else { throw SSHAuthenticationError.stale }
        phase = .selecting
        let id = UUID()
        let work = Task { try await selectAccount(for: peer) }
        retirement.install(id) { work.cancel() }
        defer { retirement.finish(id) }
        return try await withTaskCancellationHandler {
            let account = try await work.value
            try checkCurrent()
            return account
        } onCancel: { self.cancel(transportID: self.transportID) }
    }

    private func selectAccount(for peer: NativeSSHPeer) async throws -> NativeSSHPasswordAccount {
        do {
            let host = try validatePeer(peer)
            let selected = try await broker.passwordIdentity(workspaceID: workspaceID, expectedRevision: revision,
                routeID: routeID, presentedHostKey: host.key, hostCertificate: host.certificate,
                transitForRouteID: transitForRouteID, hostKeyProbe: hostKeyProbe)
            try checkCurrent()
            try validateRoute(selected.authorization)
            let account = try NativeSSHPasswordAccount(user: selected.authorization.user)
            self.selected = selected; selectedPeer = peer; selectedAccount = account; phase = .selected
            return account
        } catch { cancel(transportID: transportID); throw error }
    }

    public func password(for account: NativeSSHPasswordAccount, peer: NativeSSHPeer) async throws -> Data {
        try checkCurrent()
        guard phase == .selected, let selected, account == selectedAccount, peer == selectedPeer else {
            throw SSHAuthenticationError.stale
        }
        phase = .requesting
        let id = UUID()
        let work = Task { try await broker.password(for: selected, transportID: transportID, prompter: prompter) }
        retirement.install(id) { work.cancel() }
        defer { retirement.finish(id) }
        do {
            var bytes = try await withTaskCancellationHandler {
                try await work.value
            } onCancel: { self.cancel(transportID: self.transportID) }
            defer { bytes.resetBytes(in: 0..<bytes.count) }
            try checkCurrent()
            try await broker.validate(selected)
            try checkCurrent()
            phase = .supplied
            return bytes
        } catch { cancel(transportID: transportID); throw error }
    }

    public func validate(peer: NativeSSHPeer, account: NativeSSHPasswordAccount) async throws {
        try checkCurrent()
        guard phase == .selected || phase == .supplied || phase == .authenticated,
              peer == selectedPeer, account == selectedAccount, let selected else { throw SSHAuthenticationError.stale }
        _ = try validatePeer(peer)
        try validateRoute(selected.authorization)
        if phase == .authenticated { try await broker.validate(selected) }
        else { try await broker.validatePasswordAttempt(selected) }
        try checkCurrent()
    }

    /// Only the connection factory calls this after native SSH reports success
    /// and repeats validation. No prompt remains, and no password is retained.
    /// Subsequent live use still checks the broker and durable route policy.
    func authenticationCompleted() async throws {
        try checkCurrent()
        guard phase == .supplied, let selected else { throw SSHAuthenticationError.stale }
        try await broker.validatePasswordAttempt(selected)
        try checkCurrent()
        phase = .authenticated
    }

    public func validateCurrent() async throws {
        guard phase == .authenticated, let selectedPeer, let selectedAccount else { throw SSHAuthenticationError.stale }
        try await validate(peer: selectedPeer, account: selectedAccount)
    }

    public nonisolated func cancel(transportID: UUID) {
        guard transportID == self.transportID, retirement.retire() else { return }
        Task { await broker.cancel(transportID: transportID) }
    }

    private func checkCurrent() throws { try Task.checkCancellation(); try retirement.check() }

    private func validatePeer(_ peer: NativeSSHPeer) throws -> WorkspaceNativeSSHAuthentication.HostIdentity {
        guard peer.transportID == transportID, peer.endpoint == endpoint else { throw SSHAuthenticationError.stale }
        if let validatedPeer {
            guard validatedPeer.peer == peer else { throw SSHAuthenticationError.stale }
            return validatedPeer.host
        }
        let host = try WorkspaceNativeSSHAuthentication.hostIdentity(for: peer)
        validatedPeer = (peer, host)
        return host
    }

    private func validateRoute(_ authorization: WorkspaceSSHPasswordAuthorization) throws {
        guard authorization.workspaceID == workspaceID, authorization.workspaceRevision == revision,
              authorization.hostKeyProbe == hostKeyProbe,
              authorization.routeID == routeID, authorization.host == endpoint.host, authorization.port == endpoint.port,
              authorization.transitForRouteID == transitForRouteID, authorization.jumpRouteIDs == allowedJumpRouteIDs,
              allowedJumpRouteIDs.count <= WorkspaceNativeSSHAuthentication.routeSupport.maximumJumpHops,
              transitForRouteID != nil || WorkspaceNativeSSHAuthentication.routeSupport.routeModes.contains(authorization.routeMode) else {
            throw SSHAuthenticationError.denied
        }
    }
}

/// One keyboard-interactive exchange with one verified host. Every round keeps
/// the same account, transport, workspace revision and broker-issued attempt.
/// Server text is challenge content; it never supplies local identity labels.
public actor WorkspaceNativeSSHInteractiveAuthentication: NativeSSHInteractiveAuthentication, WorkspaceNativeSSHAuthority {
    private enum Phase { case fresh, selecting, selected, requesting, supplied, authenticated }
    private nonisolated let retirement = WorkspaceNativeSSHAuthentication.Retirement()
    private nonisolated let broker: SSHAuthenticationBroker
    private nonisolated let transportID: UUID
    private nonisolated let publicKeyPrerequisite: WorkspaceNativeSSHAuthentication?
    private let attemptID = UUID()
    private let prompter: any WorkspaceSSHKeyboardInteractivePrompting
    private let workspaceID: UUID, revision: UUID
    private let routeID: String, endpoint: NativeSSHEndpoint
    private let transitForRouteID: String?, allowedJumpRouteIDs: [String]
    private let hostKeyProbe: WorkspaceHostKeyProbe?
    private var phase = Phase.fresh
    private var selected: WorkspaceSSHKeyboardInteractiveIdentity?
    private var selectedPeer: NativeSSHPeer?
    private var selectedAccount: NativeSSHInteractiveAccount?
    private var validatedPeer: (peer: NativeSSHPeer, host: WorkspaceNativeSSHAuthentication.HostIdentity)?
    private var lastRound: UInt32 = 0

    public init(broker: SSHAuthenticationBroker, prompter: any WorkspaceSSHKeyboardInteractivePrompting,
                workspaceID: UUID, revision: UUID, routeID: String, transportID: UUID,
                endpoint: NativeSSHEndpoint, transitForRouteID: String? = nil, allowedJumpRouteIDs: [String] = [],
                continuingPublicKey: WorkspaceNativeSSHAuthentication? = nil,
                hostKeyProbe: WorkspaceHostKeyProbe? = nil) {
        self.broker = broker; self.prompter = prompter
        self.workspaceID = workspaceID; self.revision = revision; self.routeID = routeID
        self.transportID = transportID; self.endpoint = endpoint
        self.transitForRouteID = transitForRouteID; self.allowedJumpRouteIDs = allowedJumpRouteIDs
        self.hostKeyProbe = hostKeyProbe
        publicKeyPrerequisite = continuingPublicKey
    }

    public func account(for peer: NativeSSHPeer) async throws -> NativeSSHInteractiveAccount {
        try checkCurrent()
        guard phase == .fresh else { throw SSHAuthenticationError.stale }
        phase = .selecting
        let id = UUID()
        let work = Task { try await selectAccount(for: peer) }
        retirement.install(id) { work.cancel() }
        defer { retirement.finish(id) }
        return try await withTaskCancellationHandler {
            let account = try await work.value
            try checkCurrent()
            return account
        } onCancel: { self.cancel(transportID: self.transportID) }
    }

    private func selectAccount(for peer: NativeSSHPeer) async throws -> NativeSSHInteractiveAccount {
        do {
            let host = try validatePeer(peer)
            try await validatePrerequisite(peer: peer)
            let selected = try await broker.keyboardInteractiveIdentity(workspaceID: workspaceID,
                expectedRevision: revision, routeID: routeID, presentedHostKey: host.key, hostCertificate: host.certificate,
                transitForRouteID: transitForRouteID, hostKeyProbe: hostKeyProbe)
            do {
                try checkCurrent()
                try validateRoute(selected.authorization)
                let account = try NativeSSHInteractiveAccount(user: selected.authorization.user)
                try await validatePrerequisite(peer: peer, account: account)
                self.selected = selected; selectedPeer = peer; selectedAccount = account; phase = .selected
                return account
            } catch {
                await broker.finishKeyboardInteractiveAttempt(selected)
                throw error
            }
        } catch { cancel(transportID: transportID); throw error }
    }

    public func responses(for challenge: NativeSSHInteractiveChallenge, peer: NativeSSHPeer) async throws -> [Data] {
        try checkCurrent()
        guard phase == .selected || phase == .supplied, let selected,
              peer == selectedPeer, challenge.transportID == transportID, challenge.account == selectedAccount,
              challenge.round == lastRound + 1 else { throw SSHAuthenticationError.stale }
        let typed = try SSHKeyboardInteractiveChallenge(round: challenge.round, name: challenge.name,
            instruction: challenge.instruction,
            prompts: try challenge.prompts.map { try .init(text: $0.text, echo: $0.echo) })
        phase = .requesting
        let id = UUID()
        // The broker may queue this round behind another foreground prompt.
        // Revalidate the first factor at actual UI admission, not only before
        // entering that queue, and again before accepting its answers.
        let roundPrompter = PrerequisitePrompter(base: prompter) {
            try await self.validatePrerequisite(peer: peer, account: challenge.account)
        }
        let work = Task {
            try await validatePrerequisite(peer: peer, account: challenge.account)
            return try await broker.keyboardInteractiveResponses(for: selected, transportID: transportID,
                attemptID: attemptID, challenge: typed, prompter: roundPrompter)
        }
        retirement.install(id) { work.cancel() }
        defer { retirement.finish(id) }
        do {
            var answers = try await withTaskCancellationHandler {
                try await work.value
            } onCancel: { self.cancel(transportID: self.transportID) }
            defer {
                for index in answers.indices { answers[index].resetBytes(in: 0..<answers[index].count) }
            }
            try checkCurrent()
            try await broker.validateKeyboardInteractiveAttempt(selected)
            try await validatePrerequisite(peer: peer, account: challenge.account)
            try checkCurrent()
            lastRound = challenge.round; phase = .supplied
            return answers
        } catch { cancel(transportID: transportID); throw error }
    }

    public func validate(peer: NativeSSHPeer, account: NativeSSHInteractiveAccount) async throws {
        try checkCurrent()
        guard phase == .selected || phase == .supplied || phase == .authenticated,
              peer == selectedPeer, account == selectedAccount, let selected else { throw SSHAuthenticationError.stale }
        _ = try validatePeer(peer)
        try validateRoute(selected.authorization)
        try await validatePrerequisite(peer: peer, account: account)
        if phase == .authenticated { try await broker.validate(selected) }
        else { try await broker.validateKeyboardInteractiveAttempt(selected) }
        try checkCurrent()
    }

    func authenticationCompleted() async throws {
        try checkCurrent()
        guard phase == .supplied, let selected, let selectedPeer, let selectedAccount else {
            throw SSHAuthenticationError.stale
        }
        try await broker.validateKeyboardInteractiveAttempt(selected)
        try await validatePrerequisite(peer: selectedPeer, account: selectedAccount)
        try checkCurrent()
        phase = .authenticated
        await broker.finishKeyboardInteractiveAttempt(selected)
        try checkCurrent()
    }

    public func validateCurrent() async throws {
        guard phase == .authenticated, let selectedPeer, let selectedAccount else { throw SSHAuthenticationError.stale }
        try await validate(peer: selectedPeer, account: selectedAccount)
    }

    public nonisolated func cancel(transportID: UUID) {
        guard transportID == self.transportID, retirement.retire() else { return }
        publicKeyPrerequisite?.cancel(transportID: transportID)
        Task {
            await broker.cancel(transportID: transportID)
            await retireAttempt()
        }
    }

    private func retireAttempt() async {
        if let selected { await broker.finishKeyboardInteractiveAttempt(selected) }
    }
    private func checkCurrent() throws { try Task.checkCancellation(); try retirement.check() }

    /// A continued exchange retains the exact signed credential as part of its
    /// live authority. Revoking it must invalidate this login even while the
    /// route still independently permits keyboard-interactive authentication.
    private func validatePrerequisite(peer: NativeSSHPeer, account: NativeSSHInteractiveAccount? = nil) async throws {
        try checkCurrent()
        try await publicKeyPrerequisite?.validateInteractiveContinuation(peer: peer, account: account,
            workspaceID: workspaceID, revision: revision, routeID: routeID,
            transitForRouteID: transitForRouteID, allowedJumpRouteIDs: allowedJumpRouteIDs,
            hostKeyProbe: hostKeyProbe, forAuthentication: phase != .authenticated)
        try checkCurrent()
    }

    private struct PrerequisitePrompter: WorkspaceSSHKeyboardInteractivePrompting {
        let base: any WorkspaceSSHKeyboardInteractivePrompting
        let validate: @Sendable () async throws -> Void
        func responses(for request: WorkspaceSSHKeyboardInteractiveRequest) async throws -> [Data] {
            try await validate()
            var answers = try await base.responses(for: request)
            defer {
                for index in answers.indices { answers[index].resetBytes(in: 0..<answers[index].count) }
            }
            try await validate()
            return answers
        }
        func cancel(requestID: UUID) { base.cancel(requestID: requestID) }
    }

    private func validatePeer(_ peer: NativeSSHPeer) throws -> WorkspaceNativeSSHAuthentication.HostIdentity {
        guard peer.transportID == transportID, peer.endpoint == endpoint else { throw SSHAuthenticationError.stale }
        if let validatedPeer {
            guard validatedPeer.peer == peer else { throw SSHAuthenticationError.stale }
            return validatedPeer.host
        }
        let host = try WorkspaceNativeSSHAuthentication.hostIdentity(for: peer)
        validatedPeer = (peer, host)
        return host
    }

    private func validateRoute(_ authorization: WorkspaceSSHKeyboardInteractiveAuthorization) throws {
        guard authorization.workspaceID == workspaceID, authorization.workspaceRevision == revision,
              authorization.hostKeyProbe == hostKeyProbe,
              authorization.routeID == routeID, authorization.host == endpoint.host, authorization.port == endpoint.port,
              authorization.transitForRouteID == transitForRouteID, authorization.jumpRouteIDs == allowedJumpRouteIDs,
              allowedJumpRouteIDs.count <= WorkspaceNativeSSHAuthentication.routeSupport.maximumJumpHops,
              transitForRouteID != nil || WorkspaceNativeSSHAuthentication.routeSupport.routeModes.contains(authorization.routeMode) else {
            throw SSHAuthenticationError.denied
        }
    }
}

/// Connects the actual libssh2 exchange to the imported workspace and protected
/// credential broker. One instance belongs to one explicitly selected attempt;
/// cancellation permanently retires it. It never grants permission to network.
/// The app shares one foreground broker configured with routeSupport and checks
/// its offline/network scope before invoking NativeSSHConnector.
public actor WorkspaceNativeSSHAuthentication: NativeSSHAuthentication, WorkspaceNativeSSHAuthority {
    /// This is the implemented adapter profile, not Apple-device qualification.
    /// Unknown mandatory policies remain blocked by WorkspaceStore. RSA hosts
    /// and credentials require SHA-2 signatures. User certificates additionally
    /// require a verified issuer signature, subject and account binding, current
    /// validity and route permissions. Key-to-MFA continuation additionally
    /// requires a patched native backend, actual server partial success, and
    /// explicit keyboard-interactive policy. Each supported method permits three jumps. A mux route
    /// authenticates like a direct one; only the destination channel differs.
    public static let routeSupport = WorkspaceDocument.RouteSupport(
        routeModes: ["direct_ssh", "frankenterm_mux"], authenticationMethods: [.publicKey, .password, .keyboardInteractive],
        credentialAlgorithms: ["ssh-ed25519", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521", "ssh-rsa"],
        hostKeyAlgorithms: SSHHostCertificate.supportedAuthorityAlgorithms.union(
            NativeSSHSession.supportsHostCertificates ? SSHHostCertificate.supportedAlgorithms : []),
        capabilities: NativeSSHSession.supportsHostCertificates
            ? [WorkspaceDocument.userCertificateCapability, WorkspaceDocument.hostCertificateCapability]
            : [WorkspaceDocument.userCertificateCapability],
        maximumJumpHops: 3)

    fileprivate final class Retirement: @unchecked Sendable {
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
    private let transitForRouteID: String?, allowedJumpRouteIDs: [String]
    private let hostKeyProbe: WorkspaceHostKeyProbe?
    private var phase = Phase.fresh
    private var selected: WorkspaceSSHIdentity?
    private var selectedPeer: NativeSSHPeer?
    private var selectedIdentity: NativeSSHIdentity?
    private var validatedPeer: (peer: NativeSSHPeer, host: HostIdentity)?

    public init(broker: SSHAuthenticationBroker, workspaceID: UUID, revision: UUID,
                routeID: String, credentialID: String, transportID: UUID, endpoint: NativeSSHEndpoint,
                transitForRouteID: String? = nil, allowedJumpRouteIDs: [String] = [],
                hostKeyProbe: WorkspaceHostKeyProbe? = nil) {
        self.broker = broker; self.workspaceID = workspaceID; self.revision = revision
        self.routeID = routeID; self.credentialID = credentialID
        self.transportID = transportID; self.endpoint = endpoint
        self.transitForRouteID = transitForRouteID; self.allowedJumpRouteIDs = allowedJumpRouteIDs
        self.hostKeyProbe = hostKeyProbe
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
                routeID: routeID, credentialID: credentialID, presentedHostKey: host.key, hostCertificate: host.certificate,
                transitForRouteID: transitForRouteID,
                hostKeyProbe: hostKeyProbe)
            try await broker.validateForeground(foreground)
            try checkCurrent(); try validateRoute(authorization)
            let value = try await broker.publicIdentity(workspaceID: workspaceID, expectedRevision: revision,
                routeID: routeID, credentialID: credentialID, presentedHostKey: host.key, hostCertificate: host.certificate,
                transportID: transportID,
                transitForRouteID: transitForRouteID, hostKeyProbe: hostKeyProbe)
            try await broker.validateForeground(foreground)
            try checkCurrent(); try validateRoute(value.authorization)
            guard value.authorization == authorization else { throw SSHAuthenticationError.stale }
            try await broker.validateForAuthentication(value)
            try checkCurrent()
            let identity = try NativeSSHIdentity(user: authorization.user, publicKey: value.authenticationBlob)
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
            try await broker.validateForAuthentication(selected)
            try checkCurrent()
            let typed = try SSHUserAuthenticationChallenge(sessionIdentifier: challenge.sessionIdentifier,
                user: selected.authorization.user, publicBlob: selected.authenticationBlob,
                signatureAlgorithm: challenge.algorithm)
            guard typed.authenticationAlgorithm == challenge.algorithm, typed.signedData == challenge.signedData else {
                throw SSHAuthenticationError.invalidChallenge
            }
            let request = try await broker.prepare(workspaceID: workspaceID, expectedRevision: revision,
                routeID: routeID, credentialID: credentialID, presentedHostKey: selected.authorization.presentedHostKey,
                hostCertificate: selected.authorization.hostCertificate,
                transportID: transportID, sessionIdentifier: challenge.sessionIdentifier, publicBlob: selected.authenticationBlob,
                signatureAlgorithm: challenge.algorithm,
                transitForRouteID: transitForRouteID, hostKeyProbe: hostKeyProbe)
            try checkCurrent()
            guard request.authorization == selected.authorization else { throw SSHAuthenticationError.stale }
            let result = try await broker.confirm(requestID: request.id)
            try checkCurrent()
            try await broker.validateForAuthentication(selected)
            try checkCurrent()
            guard result.requestID == request.id, result.transportID == transportID,
                  result.authorization == selected.authorization, result.sessionIdentifier == challenge.sessionIdentifier,
                  result.publicBlob == selected.authenticationBlob, result.algorithm == typed.algorithm else {
                throw SSHAuthenticationError.stale
            }
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

    public func validateForAuthentication(peer: NativeSSHPeer, identity: NativeSSHIdentity) async throws {
        try checkCurrent()
        guard phase == .signed, let selected, peer == selectedPeer, identity == selectedIdentity else {
            throw SSHAuthenticationError.stale
        }
        _ = try validatePeer(peer); try validateRoute(selected.authorization)
        try await broker.validateForAuthentication(selected)
        try checkCurrent()
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

    fileprivate func validateInteractiveContinuation(peer: NativeSSHPeer, account: NativeSSHInteractiveAccount?,
        workspaceID: UUID, revision: UUID, routeID: String, transitForRouteID: String?, allowedJumpRouteIDs: [String],
        hostKeyProbe: WorkspaceHostKeyProbe?, forAuthentication: Bool) async throws {
        try checkCurrent()
        guard phase == .signed, peer == selectedPeer, let selectedIdentity,
              self.workspaceID == workspaceID, self.revision == revision, self.routeID == routeID,
              self.transitForRouteID == transitForRouteID, self.allowedJumpRouteIDs == allowedJumpRouteIDs,
              self.hostKeyProbe == hostKeyProbe,
              account.map({ $0.user.utf8.elementsEqual(selectedIdentity.user.utf8) }) ?? true else {
            throw SSHAuthenticationError.stale
        }
        if forAuthentication { try await validateForAuthentication(peer: peer, identity: selectedIdentity) }
        else { try await validate(peer: peer, identity: selectedIdentity) }
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
              authorization.hostKeyProbe == hostKeyProbe,
              authorization.routeID == routeID, authorization.credentialID == credentialID,
              authorization.host == endpoint.host, authorization.port == endpoint.port,
              authorization.transitForRouteID == transitForRouteID,
              transitForRouteID != nil || Self.routeSupport.routeModes.contains(authorization.routeMode),
              authorization.jumpRouteIDs == allowedJumpRouteIDs, allowedJumpRouteIDs.count <= 3,
              Self.routeSupport.credentialAlgorithms.contains(authorization.algorithm) else {
            throw SSHAuthenticationError.denied
        }
    }

    private func validatePeer(_ peer: NativeSSHPeer) throws -> HostIdentity {
        guard peer.transportID == transportID, peer.endpoint == endpoint else { throw SSHAuthenticationError.denied }
        if let validatedPeer {
            guard validatedPeer.peer == peer else { throw SSHAuthenticationError.stale }
            return validatedPeer.host
        }
        let host = try Self.hostIdentity(for: peer)
        validatedPeer = (peer, host)
        return host
    }

    /// Preserve the whole certificate as the presented identity. A subject key
    /// is neither an imported raw pin nor a substitute for a verified issuer.
    struct HostIdentity: Sendable {
        let key: WorkspaceDocument.HostKey
        let certificate: SSHHostCertificate?
        let rawIdentity: SSHPublicIdentity?
    }

    /// Each fresh transport selects its profile from this exact peer's current
    /// imported trust. A CA-only route cannot negotiate a raw-key downgrade.
    static func hostKeyPolicy(for route: WorkspaceDocument.Route,
                              document: WorkspaceDocument) throws -> NativeSSHHostKeyPolicy {
        guard let trust = document.hostTrust.first(where: { $0.id == route.trustID }) else {
            throw SSHAuthenticationError.denied
        }
        guard !trust.certificateAuthorities.isEmpty else { return .rawKeys }
        guard NativeSSHSession.supportsHostCertificates else { throw SSHAuthenticationError.unavailable }
        return trust.pins.isEmpty ? .certificates : .certificatesAndRawKeys
    }

    /// Authentication and read-only route diagnosis apply the same negotiated
    /// exchange profile. The native driver has proved subject-key possession;
    /// parsing a certificate separately verifies its complete issuer signature.
    /// The broker then checks current CA trust, principals, time and revocation.
    /// Actors cache this immutable parse only for the exact same exchanged peer;
    /// the broker continues to revalidate mutable policy at every trust boundary.
    static func hostIdentity(for peer: NativeSSHPeer) throws -> HostIdentity {
        let exchanges: Set<String> = ["curve25519-sha256", "curve25519-sha256@libssh.org",
            "ecdh-sha2-nistp256", "ecdh-sha2-nistp384", "ecdh-sha2-nistp521"]
        let ciphers: Set<String> = ["aes256-ctr", "aes128-ctr"]
        let macs: Set<String> = ["hmac-sha2-256-etm@openssh.com", "hmac-sha2-512-etm@openssh.com"]
        guard exchanges.contains(peer.keyExchange), ciphers.contains(peer.outboundCipher), ciphers.contains(peer.inboundCipher),
              macs.contains(peer.outboundMAC), macs.contains(peer.inboundMAC),
              peer.outboundCompression == "none", peer.inboundCompression == "none" else {
            throw SSHAuthenticationError.denied
        }
        if peer.hostSignature.hasSuffix("-cert-v01@openssh.com") {
            guard NativeSSHSession.supportsHostCertificates else { throw SSHAuthenticationError.unavailable }
            let certificate = try SSHHostCertificate(blob: peer.hostKey)
            guard certificate.permitsHostSignature(peer.hostSignature),
                  Self.routeSupport.hostKeyAlgorithms.contains(certificate.subject.algorithm) else {
                throw SSHAuthenticationError.denied
            }
            return HostIdentity(key: try .init(algorithm: certificate.algorithm, fingerprint: certificate.fingerprint),
                certificate: certificate, rawIdentity: nil)
        }
        let host = try SSHPublicIdentity(blob: peer.hostKey)
        guard host.permitsHostSignature(peer.hostSignature), Self.routeSupport.hostKeyAlgorithms.contains(host.algorithm) else {
            throw SSHAuthenticationError.denied
        }
        return HostIdentity(key: try .init(algorithm: host.algorithm, fingerprint: host.fingerprint),
            certificate: nil, rawIdentity: host)
    }
}
