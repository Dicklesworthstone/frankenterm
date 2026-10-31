import Foundation
import FrankenTermKit
import FrankenTermSSH

/// File transfers have their own authenticated transport and never borrow a
/// terminal's channel, input authority, or creation journal.
public enum WorkspaceSSHConnectionPurpose: String, Sendable, Hashable {
    case terminal, fileTransfer
}

/// Optional prompt owners for one connection attempt, including its jump
/// hosts. Supplying this bundle replaces both manager defaults: a nil member
/// refuses that prompting method instead of borrowing another screen's editor.
/// Host, route, method and foreground authorization remain the broker's work.
public struct WorkspaceSSHAuthenticationPrompters: Sendable {
    public let password: (any WorkspaceSSHPasswordPrompting)?
    public let keyboardInteractive: (any WorkspaceSSHKeyboardInteractivePrompting)?

    public init(password: (any WorkspaceSSHPasswordPrompting)? = nil,
                keyboardInteractive: (any WorkspaceSSHKeyboardInteractivePrompting)? = nil) {
        self.password = password
        self.keyboardInteractive = keyboardInteractive
    }
}

public enum WorkspaceSSHConnectionError: Error, Equatable, Sendable, LocalizedError {
    case offline, busy, capacity, stale, promptedHopRetryRequired, filesUnavailable
    public var errorDescription: String? {
        switch self {
        case .offline: return "Connections are paused. Resume while the app is active to connect."
        case .busy: return "This machine already has a connection or an attempt for this activity."
        case .capacity: return "The connection limit has been reached. Close a session before connecting another machine."
        case .stale: return "This connection changed or closed. Select the machine again."
        case .filesUnavailable: return "Remote Files requires an updated native SSH component in this build."
        case .promptedHopRetryRequired:
            return "The destination refused this sign-in after an interactive sign-in to a jump host. Choose Retry to sign in again; the app will not automatically repeat authentication prompts."
        }
    }
}

/// Local denial applies to every installed or subsequently imported alias of
/// this fingerprint. It does not remove a key from a remote authorized_keys.
/// A storage error may occur after publication: false means unconfirmed
/// durability, never that the denial was rolled back. Retry to reaffirm it.
public struct WorkspaceCredentialRevocationReceipt: Sendable {
    public let fingerprint: String
    public let affectedWorkspaceIDs: Set<UUID>
    public let durableDenialConfirmed: Bool
    /// Native I/O is closed, but these exact owners still need input/journal
    /// settlement. Their admission slots remain retained for an explicit retry.
    public let retainedConnectionIDs: Set<UUID>
}

/// Local refusal of one host key at one host and port. Every installed or later
/// imported workspace naming that endpoint refuses the key, whatever its pins
/// say. It changes nothing on the server or on the exporting Mac.
public struct WorkspaceHostRevocationReceipt: Sendable {
    public let host: String, port: UInt16
    public let key: WorkspaceDocument.HostKey
    public let affectedWorkspaceIDs: Set<UUID>
    /// Native I/O is closed, but these exact owners still need input/journal
    /// settlement. Their admission slots remain retained for an explicit retry.
    public let retainedConnectionIDs: Set<UUID>
}

/// How a live connection notices that its server stopped answering: a dropped
/// or changed network, a sleeping or hung host. After each `interval` with no
/// bytes from the server the connection sends one keepalive that asks for a
/// reply; once `missedReplies` further intervals pass in silence it closes and
/// its session ends as disconnected (about interval x (missedReplies + 1)).
/// After the device takes a new network route, a probe sent then has one
/// interval to be answered. A live server answers every probe, so an idle
/// terminal is never ended.
public struct WorkspaceSSHLiveness: Sendable, Equatable {
    public let interval: Duration, missedReplies: Int
    /// About 30 seconds from the last byte to a reported loss.
    public static let standard = WorkspaceSSHLiveness(checkedInterval: .seconds(10), missedReplies: 2)

    /// libssh2 sends at most one keepalive per 2 s, the shortest interval.
    public init(interval: Duration, missedReplies: Int) throws {
        guard interval >= .seconds(2), interval <= .seconds(120), (1...10).contains(missedReplies) else {
            throw NativeSSHError.invalidInput
        }
        self.init(checkedInterval: interval, missedReplies: missedReplies)
    }
    private init(checkedInterval: Duration, missedReplies: Int) {
        interval = checkedInterval; self.missedReplies = missedReplies
    }
}

/// One app-owned manager shared by scenes. It admits at most three attempts and
/// sixteen retained owners. A machine may own one terminal and one independent
/// file connection; at most two file connections are admitted across the app.
/// Networking starts disabled. Imported preferences and row selection do not
/// call connect; each attempt requires explicit user confirmation. All native
/// work uses WorkspaceNativeSSHConnection and its process-wide offline exclusion.
public actor WorkspaceSSHConnectionManager {
    private struct Machine: Hashable, Sendable {
        let workspaceID: UUID
        let machineID: String
        let purpose: WorkspaceSSHConnectionPurpose
    }
    private struct Pending {
        let id: UUID
        let machine: Machine
        let task: Task<WorkspaceSSHConnection, Error>
        var credentialFingerprints: Set<String> = []
    }
    private let store: WorkspaceStore, broker: SSHAuthenticationBroker
    private let journal: TerminalSessionJournal, coordinator: TerminalInputCoordinator
    private let networkActivity: WorkspaceNetworkActivity
    private let liveness: WorkspaceSSHLiveness
    /// Makes the protocol client for a connection that attaches to a
    /// machine's mux panes; without one every connection starts a shell.
    private let muxClientFactory: (@Sendable () throws -> any TerminalMuxClient)?
    private let passwordPrompter: (any WorkspaceSSHPasswordPrompting)?
    private let keyboardInteractivePrompter: (any WorkspaceSSHKeyboardInteractivePrompting)?
    private var foreground = false, requestedForeground = false, mustSuspend = false
    private var lifecycle = UUID()
    private var transition: Task<Void, Never>?
    private var pending: [Machine: Pending] = [:]
    private var live: [Machine: WorkspaceSSHConnection] = [:]
    private var settlements: [UUID: Task<Void, Error>] = [:]
    private var retiringWorkspaces: [UUID: Int] = [:]
    private var deniedCredentials: Set<String> = []
    private var revocations: [String: Task<WorkspaceCredentialRevocationReceipt, Error>] = [:]

    public init(store: WorkspaceStore, broker: SSHAuthenticationBroker,
                journal: TerminalSessionJournal, coordinator: TerminalInputCoordinator,
                networkActivity: WorkspaceNetworkActivity = .shared, liveness: WorkspaceSSHLiveness = .standard,
                muxClientFactory: (@Sendable () throws -> any TerminalMuxClient)? = nil,
                passwordPrompter: (any WorkspaceSSHPasswordPrompting)? = nil,
                keyboardInteractivePrompter: (any WorkspaceSSHKeyboardInteractivePrompting)? = nil) {
        self.store = store; self.broker = broker; self.journal = journal
        self.coordinator = coordinator; self.networkActivity = networkActivity; self.liveness = liveness
        self.muxClientFactory = muxClientFactory
        self.passwordPrompter = passwordPrompter
        self.keyboardInteractivePrompter = keyboardInteractivePrompter
    }

    public func resume() async {
        requestedForeground = true
        await synchronizeForeground().value
    }

    /// One retained driver coalesces lifecycle requests. A suspend must cross
    /// the broker even if resume arrives before its actor hop completes, so old
    /// foreground witnesses never revive. The latest requested state wins.
    private func synchronizeForeground() -> Task<Void, Never> {
        if let transition { return transition }
        let task = Task { await driveForeground() }
        transition = task
        return task
    }

    private func driveForeground() async {
        defer { transition = nil }
        while true {
            if mustSuspend {
                mustSuspend = false
                await broker.suspend()
                foreground = false
                continue
            }
            guard requestedForeground else { return }
            let generation = lifecycle
            await broker.resume()
            guard !mustSuspend, requestedForeground, lifecycle == generation else { continue }
            foreground = true
            return
        }
    }

    /// Seal every source before joining any prompt, creation write or native
    /// teardown. A failed journal settlement retains its closed owner for retry.
    public func suspend() async {
        foreground = false; requestedForeground = false; mustSuspend = true; lifecycle = UUID()
        let stateChange = synchronizeForeground()
        let attempts = Array(pending.values), connections = Array(live)
        for attempt in attempts { attempt.task.cancel() }
        for (_, connection) in connections { await connection.sealAdmission() }
        await stateChange.value
        await settle(attempts: attempts, connections: connections)
    }

    /// Call after reimport, revocation or a local policy edit. The native owner
    /// independently revalidates current durable authority before every I/O.
    public func invalidate(workspaceID: UUID) async {
        retiringWorkspaces[workspaceID, default: 0] += 1
        defer {
            if retiringWorkspaces[workspaceID] == 1 { retiringWorkspaces.removeValue(forKey: workspaceID) }
            else { retiringWorkspaces[workspaceID, default: 1] -= 1 }
        }
        let attempts = pending.filter { $0.key.workspaceID == workspaceID }.map(\.value)
        let connections = live.filter { $0.key.workspaceID == workspaceID }.map { ($0.key, $0.value) }
        for attempt in attempts { attempt.task.cancel() }
        for (_, connection) in connections { await connection.sealAdmission() }
        await settle(attempts: attempts, connections: connections)
    }

    /// Confirm the exact fingerprint and disclose workspace-wide disconnection
    /// before calling. Persist global denial before cancelling managed prompts
    /// and closing all connections in workspaces that reference the identity.
    /// Retained owner metadata includes earlier revisions, so reimport cannot
    /// hide a connection using a key removed from the current manifest.
    ///
    /// Once admitted, the bounded operation outlives its caller's cancellation.
    /// Even an ambiguous write keeps the fingerprint denied in this manager;
    /// unrelated workspaces and the broker's foreground lease remain active.
    public func revokeCredentialLocally(fingerprint: String, confirmedByUser: Bool) async throws
        -> WorkspaceCredentialRevocationReceipt {
        guard confirmedByUser else { throw WorkspaceNetworkError.confirmationRequired }
        try Task.checkCancellation()
        guard fingerprint.utf8.count == 50 else { throw FTError.malformed("identity.fingerprint") }
        if let operation = revocations[fingerprint] { return try await operation.value }
        guard revocations.count < 3 else { throw WorkspaceSSHConnectionError.capacity }
        let operation = Task { try await performCredentialRevocation(fingerprint) }
        revocations[fingerprint] = operation
        defer { revocations.removeValue(forKey: fingerprint) }
        return try await operation.value
    }

    private func performCredentialRevocation(_ fingerprint: String) async throws
        -> WorkspaceCredentialRevocationReceipt {
        var affected: Set<UUID> = []
        for workspace in try await store.load() {
            if try workspace.document().credentials.contains(where: { $0.fingerprint == fingerprint }) {
                affected.insert(workspace.id)
            }
        }
        affected.formUnion(pending.values.filter { $0.credentialFingerprints.contains(fingerprint) }
            .map { $0.machine.workspaceID })
        affected.formUnion(live.values.filter { $0.credentialFingerprints.contains(fingerprint) }
            .map { $0.scope.workspaceID })
        // Every admitted value came from a validated workspace, a retained
        // owner, or a previously admitted denial. Arbitrary strings cannot
        // consume the bounded process-local denial inventory.
        guard !affected.isEmpty || deniedCredentials.contains(fingerprint) else {
            throw SSHAuthenticationError.denied
        }
        guard deniedCredentials.count < 4096 || deniedCredentials.contains(fingerprint) else {
            throw FTError.limit("store.revocations")
        }
        deniedCredentials.insert(fingerprint)
        for id in affected { retiringWorkspaces[id, default: 0] += 1 }
        defer {
            for id in affected {
                if retiringWorkspaces[id] == 1 { retiringWorkspaces.removeValue(forKey: id) }
                else { retiringWorkspaces[id, default: 1] -= 1 }
            }
        }
        let persisted: Bool
        do { try await store.revokeLocally(fingerprint: fingerprint); persisted = true }
        catch { persisted = false }

        let attempts = pending.filter { affected.contains($0.key.workspaceID) }.map(\.value)
        let connections = live.filter { affected.contains($0.key.workspaceID) }.map { ($0.key, $0.value) }
        for attempt in attempts { attempt.task.cancel() }
        for (_, connection) in connections { await connection.sealAdmission() }
        await settle(attempts: attempts, connections: connections)
        return .init(fingerprint: fingerprint, affectedWorkspaceIDs: affected,
            durableDenialConfirmed: persisted,
            retainedConnectionIDs: Set(live.values.filter { affected.contains($0.scope.workspaceID) }.map(\.id)))
    }

    /// Confirm the exact host, port and fingerprint and disclose workspace-wide
    /// disconnection before calling. The refusal is saved for the endpoint, so
    /// every workspace naming the same host and port is affected. Attempts and
    /// connections in those workspaces end even when saving fails: the error
    /// then means the refusal may not be saved, and a retry reaffirms it.
    public func revokeHostLocally(workspaceID: UUID, trustID: String, key: WorkspaceDocument.HostKey,
                                  confirmedByUser: Bool) async throws -> WorkspaceHostRevocationReceipt {
        guard confirmedByUser else { throw WorkspaceNetworkError.confirmationRequired }
        try Task.checkCancellation()
        let workspaces = try await store.load()
        // Only an imported pin or CA can be refused, so arbitrary input
        // cannot consume the bounded saved revocation inventory.
        guard let trust = try workspaces.first(where: { $0.id == workspaceID })?.document().hostTrust
            .first(where: { $0.id == trustID }),
              trust.pins.contains(key) || trust.certificateAuthorities.contains(key) else { throw FTError.stale }
        var affected: Set<UUID> = [workspaceID]
        for workspace in workspaces {
            if try workspace.document().hostTrust.contains(where: { $0.host == trust.host && $0.port == trust.port }) {
                affected.insert(workspace.id)
            }
        }
        for id in affected { retiringWorkspaces[id, default: 0] += 1 }
        defer {
            for id in affected {
                if retiringWorkspaces[id] == 1 { retiringWorkspaces.removeValue(forKey: id) }
                else { retiringWorkspaces[id, default: 1] -= 1 }
            }
        }
        let saving: Error?
        do { try await store.revokeHostLocally(workspaceID: workspaceID, trustID: trustID, key: key); saving = nil }
        catch { saving = error }

        let attempts = pending.filter { affected.contains($0.key.workspaceID) }.map(\.value)
        let connections = live.filter { affected.contains($0.key.workspaceID) }.map { ($0.key, $0.value) }
        for attempt in attempts { attempt.task.cancel() }
        for (_, connection) in connections { await connection.sealAdmission() }
        await settle(attempts: attempts, connections: connections)
        if let saving { throw saving }
        return .init(host: trust.host, port: trust.port, key: key, affectedWorkspaceIDs: affected,
            retainedConnectionIDs: Set(live.values.filter { affected.contains($0.scope.workspaceID) }.map(\.id)))
    }

    public func connections() -> [WorkspaceSSHConnection] {
        live.values.sorted {
            if $0.scope.workspaceID != $1.scope.workspaceID {
                return $0.scope.workspaceID.uuidString < $1.scope.workspaceID.uuidString
            }
            if $0.identity.machineID != $1.identity.machineID {
                return $0.identity.machineID < $1.identity.machineID
            }
            return $0.purpose.rawValue < $1.purpose.rawValue
        }
    }
    public var pendingCount: Int { pending.count }
    public var permitsConnections: Bool { foreground }

    public func connect(workspaceID: UUID, expectedRevision: UUID, machineID: String,
                        routeID: String, credentialID: String, confirmedByUser: Bool,
                        connectionTimeout: Duration = .seconds(15),
                        authenticationTimeout: Duration = .seconds(60),
                        attachMux: Bool = true, muxAccessMode: TerminalMuxAccessMode = .interactive,
                        requiresExistingMux: Bool = false,
                        purpose: WorkspaceSSHConnectionPurpose = .terminal,
                        authenticationPrompters: WorkspaceSSHAuthenticationPrompters? = nil) async throws -> WorkspaceSSHConnection {
        try await connect(workspaceID: workspaceID, expectedRevision: expectedRevision, machineID: machineID,
            routeID: routeID, authentication: .publicKey(credentialID), confirmedByUser: confirmedByUser,
            connectionTimeout: connectionTimeout, authenticationTimeout: authenticationTimeout, attachMux: attachMux,
            muxAccessMode: muxAccessMode, requiresExistingMux: requiresExistingMux, purpose: purpose,
            authenticationPrompters: authenticationPrompters)
    }

    public func connect(workspaceID: UUID, expectedRevision: UUID, machineID: String,
                        routeID: String, authentication: WorkspaceSSHAuthenticationChoice, confirmedByUser: Bool,
                        connectionTimeout: Duration = .seconds(15),
                        authenticationTimeout: Duration = .seconds(60),
                        attachMux: Bool = true, muxAccessMode: TerminalMuxAccessMode = .interactive,
                        requiresExistingMux: Bool = false,
                        selection: WorkspaceSSHConnectionSelection? = nil,
                        purpose: WorkspaceSSHConnectionPurpose = .terminal,
                        authenticationPrompters: WorkspaceSSHAuthenticationPrompters? = nil) async throws -> WorkspaceSSHConnection {
        guard confirmedByUser else { throw WorkspaceNetworkError.confirmationRequired }
        guard purpose == .terminal || (!attachMux && !requiresExistingMux) else {
            throw NativeSSHError.invalidInput
        }
        guard purpose == .terminal || NativeSSHSession.supportsSFTP else {
            throw WorkspaceSSHConnectionError.filesUnavailable
        }
        guard foreground else { throw WorkspaceSSHConnectionError.offline }
        guard retiringWorkspaces[workspaceID] == nil else { throw WorkspaceSSHConnectionError.stale }
        if let selection {
            guard selection.workspaceID == workspaceID, selection.expectedRevision == expectedRevision,
                  selection.machineID == machineID, selection.routeID == routeID,
                  selection.authentication == authentication else {
                throw WorkspaceSSHConnectionSelectionError.stale
            }
            guard selection.hops.count <= WorkspaceNativeSSHAuthentication.routeSupport.maximumJumpHops else {
                throw WorkspaceSSHConnectionSelectionError.hopMismatch
            }
        }
        guard connectionTimeout > .zero, connectionTimeout <= .seconds(120),
              authenticationTimeout > .zero, authenticationTimeout <= .seconds(120) else {
            throw NativeSSHError.invalidInput
        }
        try Task.checkCancellation()
        let machine = Machine(workspaceID: workspaceID, machineID: machineID, purpose: purpose)
        guard pending[machine] == nil, live[machine] == nil else { throw WorkspaceSSHConnectionError.busy }
        guard pending.count < 3, pending.count + live.count + settlements.count < 16 else {
            throw WorkspaceSSHConnectionError.capacity
        }
        if purpose == .fileTransfer {
            let owners = pending.keys.filter { $0.purpose == .fileTransfer }.count
                + live.keys.filter { $0.purpose == .fileTransfer }.count
            guard owners < 2 else { throw WorkspaceSSHConnectionError.capacity }
        }
        let generation = lifecycle, attemptID = UUID()
        let store = self.store, broker = self.broker, journal = self.journal
        let coordinator = self.coordinator, networkActivity = self.networkActivity, liveness = self.liveness
        let muxClientFactory = self.muxClientFactory
        let prompts = authenticationPrompters ?? .init(password: self.passwordPrompter,
            keyboardInteractive: self.keyboardInteractivePrompter)
        let passwordPrompter = prompts.password, keyboardInteractivePrompter = prompts.keyboardInteractive
        let task = Task<WorkspaceSSHConnection, Error> {
            let fingerprints = try await credentialSnapshot(machine: machine, attemptID: attemptID,
                generation: generation, expectedRevision: expectedRevision, routeID: routeID,
                authentication: authentication, selection: selection)
            let native = try await WorkspaceNativeSSHConnection.connect(workspaceID: workspaceID,
                expectedRevision: expectedRevision, machineID: machineID, routeID: routeID,
                authentication: authentication, confirmedByUser: confirmedByUser,
                store: store, broker: broker, passwordPrompter: passwordPrompter,
                keyboardInteractivePrompter: keyboardInteractivePrompter, journal: journal, networkActivity: networkActivity,
                connectionTimeout: connectionTimeout, authenticationTimeout: authenticationTimeout,
                liveness: liveness, muxClientFactory: muxClientFactory, attachMux: attachMux,
                muxAccessMode: muxAccessMode, requiresExistingMux: requiresExistingMux,
                selection: selection, purpose: purpose)
            do {
                try Task.checkCancellation()
                return WorkspaceSSHConnection(native: native, journal: journal, coordinator: coordinator,
                    credentialFingerprints: fingerprints)
            } catch {
                await native.close()
                throw error
            }
        }
        pending[machine] = .init(id: attemptID, machine: machine, task: task)
        defer { if pending[machine]?.id == attemptID { pending.removeValue(forKey: machine) } }
        let connection: WorkspaceSSHConnection
        do {
            connection = try await withTaskCancellationHandler {
                try await task.value
            } onCancel: { task.cancel() }
        } catch {
            // A completed task retains its original error even when revocation
            // or invalidation cancels it before this continuation runs. Never
            // let that obsolete key refusal authorize the caller's next key.
            try Task.checkCancellation()
            guard !task.isCancelled else { throw CancellationError() }
            guard foreground, lifecycle == generation, retiringWorkspaces[workspaceID] == nil,
                  pending[machine]?.id == attemptID else { throw WorkspaceSSHConnectionError.stale }
            throw error
        }
        do {
            try Task.checkCancellation()
            guard !task.isCancelled, foreground, lifecycle == generation, retiringWorkspaces[workspaceID] == nil,
                  pending[machine]?.id == attemptID else { throw WorkspaceSSHConnectionError.stale }
            try await connection.validateForPublication()
            try Task.checkCancellation()
            guard !task.isCancelled, foreground, lifecycle == generation, retiringWorkspaces[workspaceID] == nil,
                  pending[machine]?.id == attemptID else { throw WorkspaceSSHConnectionError.stale }
            live[machine] = connection
            return connection
        } catch {
            try? await settleConnection(machine, connection).value
            throw error
        }
    }

    /// Register immutable credential metadata before the native factory can
    /// resolve a hostname or request a signer. Recheck after the store actor
    /// hop: a revocation admitted while this read was pending wins the race.
    private func credentialSnapshot(machine: Machine, attemptID: UUID, generation: UUID,
                                    expectedRevision: UUID, routeID: String,
                                    authentication: WorkspaceSSHAuthenticationChoice,
                                    selection: WorkspaceSSHConnectionSelection?) async throws -> Set<String> {
        try Task.checkCancellation()
        let workspaces = try await store.loadWithLocalPolicy(support: WorkspaceNativeSSHAuthentication.routeSupport)
        try Task.checkCancellation()
        guard let local = workspaces.first(where: { $0.workspace.id == machine.workspaceID }),
              local.workspace.revision == expectedRevision else { throw SSHAuthenticationError.stale }
        let document = try local.workspace.document()
        var selectedChoices = [authentication]
        if let selection {
            let options = try WorkspaceSSHRouteOptions(localState: local, machineID: machine.machineID,
                routeID: routeID, support: WorkspaceNativeSSHAuthentication.routeSupport)
            try options.validate(selection)
            selectedChoices += selection.hops.map(\.authentication)
        } else {
            for hop in try document.orderedJumpRoutes(forRouteID: routeID) {
                guard let readiness = local.routes.first(where: { $0.routeID == hop.id }) else {
                    throw SSHAuthenticationError.denied
                }
                selectedChoices += WorkspaceSSHAuthenticationChoice.choices(for: hop,
                    usableCredentialIDs: readiness.usableCredentialIDs)
            }
        }
        // A failed durable revocation remains denied by this manager. Apply
        // that denial to every key the chosen path could offer, including
        // transit candidates; selecting password explicitly does not offer it.
        for choice in selectedChoices {
            guard case let .publicKey(credentialID) = choice else { continue }
            guard let credential = document.credentials.first(where: { $0.id == credentialID }),
                  !deniedCredentials.contains(credential.fingerprint) else { throw SSHAuthenticationError.denied }
        }
        let fingerprints = Set(document.credentials.map(\.fingerprint))
        guard foreground, lifecycle == generation, pending[machine]?.id == attemptID,
              retiringWorkspaces[machine.workspaceID] == nil,
              fingerprints.isDisjoint(with: revocations.keys) else { throw WorkspaceSSHConnectionError.stale }
        pending[machine]?.credentialFingerprints = fingerprints
        return fingerprints
    }

    /// Remove only the exact owner after its durable settlement succeeds. A
    /// delayed close cannot remove a replacement connection for the same machine.
    public func close(connectionID: UUID) async throws {
        guard let (machine, connection) = live.first(where: { $0.value.id == connectionID }) else { return }
        try await settleConnection(machine, connection).value
    }

    private func settle(attempts: [Pending], connections: [(Machine, WorkspaceSSHConnection)]) async {
        // Repeated lifecycle signals share one job per exact owned attempt or
        // connection. Start all bounded jobs before waiting on any of them.
        let jobs = attempts.map(settleAttempt) + connections.map { settleConnection($0.0, $0.1) }
        for job in jobs { try? await job.value }
    }

    private func settleAttempt(_ attempt: Pending) -> Task<Void, Error> {
        if let existing = settlements[attempt.id] { return existing }
        let id = attempt.id, operation = attempt.task
        let job = Task<Void, Error> {
            defer { settlements.removeValue(forKey: id) }
            if let connection = try? await operation.value {
                try await settleConnection(attempt.machine, connection).value
            }
        }
        settlements[id] = job
        return job
    }

    private func settleConnection(_ machine: Machine, _ connection: WorkspaceSSHConnection) -> Task<Void, Error> {
        let id = connection.id
        if let existing = settlements[id] { return existing }
        // A completed attempt can lose publication to revocation or suspension.
        // It still needs a retained cleanup owner if journal settlement fails.
        if live[machine] == nil { live[machine] = connection }
        let job = Task<Void, Error> {
            defer { settlements.removeValue(forKey: id) }
            try await connection.close()
            if live[machine]?.id == id { live.removeValue(forKey: machine) }
        }
        settlements[id] = job
        return job
    }
}

/// The managed connection exposes the existing native owner through a durable
/// creation boundary. There is no alternate signer, transport or native session
/// here. The journal and coordinator cannot be replaced after admission.
public final class WorkspaceSSHConnection: Sendable {
    public let id = UUID()
    public let identity: TerminalConnectionIdentity
    public let peer: NativeSSHPeer
    public let scope: TerminalSessionScope
    public var purpose: WorkspaceSSHConnectionPurpose { native.purpose }
    /// The account and endpoint selected by the validated route, including its
    /// final destination when the transport traverses jump hosts.
    public var host: String { native.host }
    public var port: UInt16 { native.port }
    public var user: String { native.user }
    /// The machine's mux panes when this connection attaches to them.
    public var muxControl: TerminalMuxControl? { native.muxControl }
    fileprivate let credentialFingerprints: Set<String>
    private let native: WorkspaceNativeSSHConnection
    private let journal: TerminalSessionJournal, coordinator: TerminalInputCoordinator

    fileprivate init(native: WorkspaceNativeSSHConnection, journal: TerminalSessionJournal,
                     coordinator: TerminalInputCoordinator, credentialFingerprints: Set<String>) {
        self.native = native; self.journal = journal; self.coordinator = coordinator
        self.credentialFingerprints = credentialFingerprints
        identity = native.identity; peer = native.peer; scope = native.scope
    }
    public var isClosed: Bool { get async { await native.isClosed } }

    public func openTerminal(geometry: TerminalSessionGeometry) async throws -> TerminalSessionPump {
        try await native.openTerminal(geometry: NativeSSHGeometry(columns: geometry.columns, rows: geometry.rows,
            pixelWidth: geometry.pixelWidth, pixelHeight: geometry.pixelHeight),
            coordinator: coordinator, journal: journal)
    }

    public func openFiles() async throws -> WorkspaceSFTPClient {
        try await native.openFiles()
    }

    public func requestResize(_ geometry: TerminalSessionGeometry, source: TerminalSource) async throws {
        try await native.requestResize(NativeSSHGeometry(columns: geometry.columns, rows: geometry.rows,
            pixelWidth: geometry.pixelWidth, pixelHeight: geometry.pixelHeight), source: source)
    }

    /// See WorkspaceNativeSSHConnection.networkRouteChanged.
    public func networkRouteChanged() async { await native.networkRouteChanged() }

    /// Durably record the exact content-free outcome batch before acknowledging
    /// it to the pump. This never accepts, retries, or replays terminal bytes.
    public func recordInput(_ outcomes: [TerminalInputOutcome], source: TerminalSource) async throws {
        try await native.recordInput(outcomes, source: source)
    }

    /// After joining the pump, responder cleanup and final outcome delivery,
    /// retain proof of complete input settlement before coordinator.retire.
    /// A later close publishes that proof and can retry a failed journal write.
    public func prepareInputSettlement(source: TerminalSource) async throws {
        try await native.prepareInputSettlement(source: source)
    }

    /// Call when the pump ends, including EOF/error. Closing preserves unresolved
    /// remote state. A published source must settle its responders and outcomes
    /// before journal publication can succeed. Native I/O closes immediately;
    /// manager.close releases its admission slot only after durable settlement.
    public func close() async throws { try await native.closeAndRecordInterruption() }
    fileprivate func sealAdmission() async { await native.sealAdmission() }
    fileprivate func validateForPublication() async throws { try await native.validateForPublication() }
}
