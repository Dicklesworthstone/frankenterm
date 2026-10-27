// Synthetic workspace -> production store/broker/signer -> real SSH exchange.
// Only the vault and durable-storage devices are replaced. The Python parent
// owns an independent libssh server and generates disposable test identities.
import Foundation
import FrankenTermKit
import FrankenTermSSH
import FrankenTermSSHIntegration
#if canImport(CryptoKit)
import CryptoKit
#else
import Crypto
#endif

private enum WorkspaceProbeError: Error { case invalid, unexpected, unsupported }

/// Archive identity validation uses maintained primitives. Encryption and source
/// key unlocking are outside this probe and must fail if accidentally invoked.
private struct ProbeCryptography: AirGapCryptography {
    func sha256(_ bytes: Data) -> Data { Data(SHA256.hash(data: bytes)) }
    func ed25519PublicKey(seed: Data) throws -> Data {
        try Curve25519.Signing.PrivateKey(rawRepresentation: seed).publicKey.rawRepresentation
    }
    func ecdsaPublicKey(scalar: Data, curve: SSHECDSACurve) throws -> Data {
        switch curve {
        case .nistp256: return try P256.Signing.PrivateKey(rawRepresentation: scalar).publicKey.x963Representation
        case .nistp384: return try P384.Signing.PrivateKey(rawRepresentation: scalar).publicKey.x963Representation
        case .nistp521: return try P521.Signing.PrivateKey(rawRepresentation: scalar).publicKey.x963Representation
        }
    }
    func hmacSHA1(key: Data, message: Data) throws -> Data { throw WorkspaceProbeError.unsupported }
    func randomBytes(count: Int) throws -> Data { throw WorkspaceProbeError.unsupported }
    func seal(_ plaintext: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data { throw WorkspaceProbeError.unsupported }
    func open(_ ciphertextAndTag: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data { throw WorkspaceProbeError.unsupported }
    func bcryptPBKDF(passphrase: Data, salt: Data, rounds: UInt32, outputByteCount: Int) throws -> Data { throw WorkspaceProbeError.unsupported }
    func decryptAESCTR(_ ciphertext: Data, key: Data, iv: Data) throws -> Data { throw WorkspaceProbeError.unsupported }
    func decryptAESCBC(_ ciphertext: Data, key: Data, iv: Data) throws -> Data { throw WorkspaceProbeError.unsupported }
}

private final class ProbePersistence: WorkspacePersistence, @unchecked Sendable {
    private let lock = NSRecursiveLock()
    private var slots: [WorkspaceStorageSlot: Data] = [:]
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T {
        lock.lock(); defer { lock.unlock() }; return try body()
    }
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? {
        lock.lock(); defer { lock.unlock() }
        guard slots[slot].map({ $0.count <= limit }) ?? true else { throw WorkspaceProbeError.invalid }
        return slots[slot]
    }
    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws {
        lock.lock(); defer { lock.unlock() }; slots[slot] = data
    }
    func removeJournal() throws { lock.lock(); defer { lock.unlock() }; slots[.journal] = nil }
}

/// Park a real creation write or the first catalog read after its publication,
/// which is the source-binding boundary. Release performs the actual file I/O,
/// including its atomic write and directory fsync after cancellation.
private final class ParkedJournalPersistence: WorkspacePersistence, @unchecked Sendable {
    enum Boundary: Sendable { case creationWrite, inputBindingRead }
    private let file: FileWorkspacePersistence
    private let boundary: Boundary
    private let condition = NSCondition()
    private var armed = true, parked = false, released = false, wroteCatalog = false
    private var observer: CheckedContinuation<Void, Never>?
    init(directory: URL, boundary: Boundary = .creationWrite) throws {
        file = try FileWorkspacePersistence(directory: directory); self.boundary = boundary
    }
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T { try file.withExclusiveAccess(body) }
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? {
        condition.lock()
        if boundary == .inputBindingRead && wroteCatalog && slot == .catalog { parkWhileLocked() }
        condition.unlock()
        return try file.read(slot, limit: limit)
    }
    func removeJournal() throws { try file.removeJournal() }
    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws {
        condition.lock()
        if boundary == .creationWrite && slot == .catalog { parkWhileLocked() }
        condition.unlock()
        try file.write(data, to: slot)
        if slot == .catalog { condition.lock(); wroteCatalog = true; condition.unlock() }
    }
    private func parkWhileLocked() {
        guard armed else { return }
        armed = false; parked = true
        let observer = self.observer; self.observer = nil
        condition.unlock(); observer?.resume(); condition.lock()
        while !released { condition.wait() }
    }
    func waitUntilParked() async {
        await withCheckedContinuation { continuation in
            condition.lock()
            if parked { condition.unlock(); continuation.resume() }
            else { precondition(observer == nil); observer = continuation; condition.unlock() }
        }
    }
    func release() {
        condition.lock(); released = true; condition.broadcast(); condition.unlock()
    }
}

/// A real synchronized file store with one injected publication failure. This
/// exercises owner retry after source retirement without simulating SSH I/O.
private final class FailedJournalPublication: WorkspacePersistence, @unchecked Sendable {
    private let file: FileWorkspacePersistence
    private let lock = NSLock()
    private var armed = false
    init(directory: URL) throws { file = try FileWorkspacePersistence(directory: directory) }
    func failNextWrite() { lock.lock(); armed = true; lock.unlock() }
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T { try file.withExclusiveAccess(body) }
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? { try file.read(slot, limit: limit) }
    func removeJournal() throws { try file.removeJournal() }
    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws {
        lock.lock(); let fail = armed && slot == .catalog; if fail { armed = false }; lock.unlock()
        if fail { throw WorkspaceProbeError.unexpected }
        try file.write(data, to: slot)
    }
}

private actor CompletionFlag {
    private(set) var finished = false
    func finish() { finished = true }
}

private final class ProbeVault: WorkspaceCredentialVault, @unchecked Sendable {
    private struct Item {
        let container: Data, identity: SSHPublicIdentity
    }
    private let lock = NSLock()
    private var items: [String: Item] = [:]
    func create(account: String, privateContainer: Data, fingerprint: String) throws {
        let pem = Data(("-----BEGIN OPENSSH PRIVATE KEY-----\n" + privateContainer.base64EncodedString()
            + "\n-----END OPENSSH PRIVATE KEY-----\n").utf8)
        let record = try PrivateKeyRecord.fromPEM(pem, credentialID: "key", using: ProbeCryptography())
        let identity = try SSHPublicIdentity(blob: record.publicBlob)
        guard identity.fingerprint == fingerprint else { throw WorkspaceProbeError.invalid }
        lock.lock(); defer { lock.unlock() }
        guard items[account] == nil else { throw WorkspaceProbeError.invalid }
        items[account] = Item(container: privateContainer, identity: identity)
    }
    func fingerprint(account: String) throws -> String? {
        lock.lock(); defer { lock.unlock() }; return items[account]?.identity.fingerprint
    }
    func publicIdentity(account: String) throws -> SSHPublicIdentity? {
        lock.lock(); defer { lock.unlock() }; return items[account]?.identity
    }
    func remove(account: String) throws { lock.lock(); defer { lock.unlock() }; items[account] = nil }
    func sign(_ challenge: SSHUserAuthenticationChallenge, authorization: WorkspaceSSHAuthorization) throws -> Data {
        let item = try selected(authorization)
        guard item.identity.blob == challenge.subjectPublicBlob,
              challenge.publicBlob == (authorization.userCertificate?.blob ?? item.identity.blob),
              challenge.user.utf8.elementsEqual(authorization.user.utf8) else {
            throw WorkspaceProbeError.invalid
        }
        return try SSHUserAuthenticationSigner.sign(challenge, privateContainer: item.container)
    }
    func publicIdentity(_ authorization: WorkspaceSSHAuthorization) throws -> Data {
        try selected(authorization).identity.blob
    }
    private func selected(_ authorization: WorkspaceSSHAuthorization) throws -> Item {
        lock.lock(); defer { lock.unlock() }
        guard let item = items.values.first(where: { $0.identity.fingerprint == authorization.fingerprint &&
            $0.identity.algorithm == authorization.algorithm }) else { throw WorkspaceProbeError.invalid }
        return item
    }
}

/// A single deliberately delayed protected operation. Cancellation acknowledges
/// the request but retains ownership until finish(), like a native prompt that
/// has been invalidated while its platform callback is still outstanding.
private final class ProbeSigner: WorkspaceSSHSigning, @unchecked Sendable {
    private let lock = NSLock(), vault: ProbeVault, held: Bool
    private var active: UUID?
    private var gate: CheckedContinuation<Void, Never>?
    private var started: CheckedContinuation<Void, Never>?
    private var cancelled: CheckedContinuation<Void, Never>?
    private var callsValue = 0, cancelledValue = 0
    init(vault: ProbeVault, held: Bool) { self.vault = vault; self.held = held }
    var calls: Int { lock.lock(); defer { lock.unlock() }; return callsValue }
    var cancellations: Int { lock.lock(); defer { lock.unlock() }; return cancelledValue }
    func publicIdentity(requestID: UUID, authorization: WorkspaceSSHAuthorization) async throws -> Data {
        try Task.checkCancellation()
        return try vault.publicIdentity(authorization)
    }
    func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
              challenge: SSHUserAuthenticationChallenge) async throws -> Data {
        if held {
            await withCheckedContinuation { continuation in
                lock.lock()
                precondition(active == nil && gate == nil)
                active = requestID; callsValue += 1; gate = continuation
                let observer = started; started = nil
                lock.unlock(); observer?.resume()
            }
        } else { recordImmediate(requestID) }
        defer { settle(requestID) }
        return try vault.sign(challenge, authorization: authorization)
    }
    private func recordImmediate(_ id: UUID) {
        lock.lock(); defer { lock.unlock() }; precondition(active == nil)
        active = id; callsValue += 1
    }
    private func settle(_ id: UUID) {
        lock.lock(); defer { lock.unlock() }; if active == id { active = nil }
    }
    func cancel(requestID: UUID) {
        lock.lock()
        guard active == requestID else { lock.unlock(); return }
        cancelledValue += 1
        let observer = cancelled; cancelled = nil
        lock.unlock(); observer?.resume()
    }
    func waitUntilStarted() async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if active != nil { lock.unlock(); continuation.resume() }
            else { precondition(started == nil); started = continuation; lock.unlock() }
        }
    }
    func waitUntilCancelled() async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if cancelledValue > 0 { lock.unlock(); continuation.resume() }
            else { precondition(cancelled == nil); cancelled = continuation; lock.unlock() }
        }
    }
    func finish() {
        lock.lock(); let continuation = gate; gate = nil; lock.unlock()
        continuation?.resume()
    }
}

private func manifest(identity: SSHPublicIdentity, host: SSHPublicIdentity, port: UInt16,
                      secondIdentity: SSHPublicIdentity? = nil) throws -> WorkspaceDocument {
    let identities = [("key", identity)] + (secondIdentity.map { [("key-2", $0)] } ?? [])
    let theme: [String: Any] = ["id": "theme", "name": "Loopback", "foreground": "#FFFFFF", "background": "#10223C",
        "accent": "#3B82F6", "ansi": Array(repeating: "#123456", count: 16), "cursor_foreground": "#000000",
        "cursor_background": "#FFFFFF", "cursor_border": "#FFFFFF", "selection_foreground": "#FFFFFF", "selection_background": "#345D91"]
    let workspace: [String: Any] = [
        "id": "workspace", "name": "Synthetic loopback", "groups": [["id": "group", "name": "Local", "machine_order": ["machine"]]],
        "machines": [["id": "machine", "name": "Loopback", "theme_ref": "theme", "route_refs": ["route"],
                       "preferred_route_ref": "route", "auto_connect": true]],
        "routes": [["id": "route", "mode": "direct_ssh", "session_creation": "on_select", "host": "LOCALHOST.",
                    "port": Int(port), "user": "operator", "credential_refs": identities.map { $0.0 }, "authentication_methods": ["publickey"],
                    "host_trust_ref": "trust", "jump_routes": []]],
        "credentials": identities.map { id, identity in
            ["id": id, "algorithm": identity.algorithm, "public_fingerprint": identity.fingerprint,
             "requirement": "matching_private_key", "private_material": "not_included"]
        },
        "host_trust": [["id": "trust", "host": "LOCALHOST.", "port": Int(port), "revoked_keys": [],
                        "pins": [["algorithm": host.algorithm, "fingerprint": host.fingerprint]]]],
        "themes": [theme], "preferences": ["initial_machine_ref": "machine", "initial_group_ref": "group", "connect_on_open": true]
    ]
    return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: ["format": "ftworkspace", "version": 1,
        "bundle_id": "test-bundle", "source": ["exporter_id": "independent-server-probe"], "required_capabilities": [],
        "workspace": workspace], options: [.sortedKeys]))
}

private func mustFail(_ body: @Sendable () async throws -> Void) async throws {
    do { try await body() } catch { return }
    throw WorkspaceProbeError.unexpected
}

private func exerciseShell(_ session: NativeSSHSession) async throws -> Int {
    try await session.openShell(geometry: .init(columns: 80, rows: 24))
    let greeting = Data("ready\r\nstderr\r\n".utf8)
    let payload = Data("protected workspace → actual SSH 🦀\r\nEND-SESSION\r".utf8)
    var output = Data(), sent = false
    let deadline = ContinuousClock.now + .seconds(5)
    while ContinuousClock.now < deadline {
        let event = try await session.service(timeout: .seconds(1))
        guard output.count + event.output.count <= greeting.count + payload.count else { throw WorkspaceProbeError.invalid }
        output += event.output
        if !sent && output.count == greeting.count {
            guard output == greeting, try await session.admit(payload) == .accepted(payload.count) else { throw WorkspaceProbeError.invalid }
            sent = true
        }
        if event.endOfOutput {
            guard sent, output == greeting + payload else { throw WorkspaceProbeError.invalid }
            return payload.count
        }
    }
    throw WorkspaceProbeError.unexpected
}

private actor ManagedConsumer: TerminalSessionConsumer {
    let source: TerminalSource
    private let connection: WorkspaceSSHConnection
    private var output = Data()
    private var outcomes: [TerminalInputOutcome] = []
    private var ended = false
    init(source: TerminalSource, connection: WorkspaceSSHConnection) {
        self.source = source; self.connection = connection
    }
    func receive(_ update: TerminalSessionUpdate, source: TerminalSource) async throws {
        guard source == self.source, !ended, output.count + update.output.count <= 256 else { throw WorkspaceProbeError.invalid }
        output += update.output; ended = update.endOfOutput
    }
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) async throws {
        guard source == self.source, self.outcomes.count + outcomes.count <= 2 else { throw WorkspaceProbeError.invalid }
        try await connection.recordInput(outcomes, source: source)
        self.outcomes += outcomes
    }
    func waitForGreeting() async throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while output.count < 15 {
            guard ContinuousClock.now < deadline else { throw WorkspaceProbeError.unexpected }
            try await Task.sleep(for: .milliseconds(5))
        }
        guard output == Data("ready\r\nstderr\r\n".utf8) else { throw WorkspaceProbeError.invalid }
    }
    func verify(receipt: TerminalInputReceipt, payload: Data) throws {
        guard ended, output == Data("ready\r\nstderr\r\n".utf8) + payload, outcomes.count == 1,
              outcomes[0].receipt == receipt, outcomes[0].disposition == .transportAccepted,
              outcomes[0].originalBytes == payload.count, outcomes[0].acceptedOriginalBytes == payload.count,
              outcomes[0].acceptedCleanupBytes == 0 else { throw WorkspaceProbeError.invalid }
    }
}

/// The durable journal a relaunch would reopen. Apple anchors its history in
/// the data-protection Keychain, which this unsigned command-line probe cannot
/// use (errSecMissingEntitlement); the app-hosted FrankenTermTests cover that
/// anchor. Here the same catalog files persist without it.
private func durableJournal(directory: URL, identifier: String) throws -> TerminalSessionJournal {
    #if canImport(Security)
    try TerminalSessionJournal(persistence: FileWorkspacePersistence(directory: directory))
    #else
    try TerminalSessionJournal(directory: directory, identifier: identifier)
    #endif
}

/// Suspend only between fully settled connection attempts. The parent exercises
/// actual caller cancellation or changes durable authority before releasing it.
private actor OrderedCredentialBoundary {
    private var reached = false, released = false
    private var observer: CheckedContinuation<Void, Never>?
    private var continuation: CheckedContinuation<Void, Never>?
    func pause() async {
        reached = true; observer?.resume(); observer = nil
        guard !released else { return }
        await withCheckedContinuation { continuation = $0 }
    }
    func wait() async {
        guard !reached else { return }
        await withCheckedContinuation { observer = $0 }
    }
    func release() { released = true; continuation?.resume(); continuation = nil }
}

/// Use the same production attempt plan as the native controller. Every choice
/// still passes through the actual store, manager, signer and native transport.
private func exerciseOrderedCredentials(store: WorkspaceStore, installed: InstalledWorkspace,
    document: WorkspaceDocument, broker: SSHAuthenticationBroker, signer: ProbeSigner,
    directory: URL, host: Data, mode: String) async throws -> Int {
    let journal = try durableJournal(directory: directory, identifier: "ordered-probe-" + UUID().uuidString)
    let coordinator = try TerminalInputCoordinator()
    let manager = WorkspaceSSHConnectionManager(store: store, broker: broker, journal: journal, coordinator: coordinator)
    await manager.resume()
    let ready = try await store.loadWithReadiness(support: WorkspaceNativeSSHAuthentication.routeSupport)
    guard let entry = ready.first(where: { $0.workspace.id == installed.id }),
          let route = entry.routes.first(where: { $0.routeID == "route" }), route.isLocallyReady,
          route.usableCredentialIDs == ["key", "key-2"] else { throw WorkspaceProbeError.invalid }
    let boundary = OrderedCredentialBoundary()
    let operation = Task<WorkspaceSSHConnection, Error> {
        var plan = try TerminalRouteAttemptPlan(routes: [.init(routeID: "route", credentialIDs: route.usableCredentialIDs)])
        defer { plan.cancel() }
        var offered = 0
        while let attempt = plan.current {
            try Task.checkCancellation()
            offered += 1
            let connection: WorkspaceSSHConnection
            do {
                connection = try await manager.connect(workspaceID: installed.id, expectedRevision: installed.revision,
                    machineID: "machine", routeID: attempt.selection.routeID, authentication: attempt.selection.authentication,
                    confirmedByUser: true, connectionTimeout: .seconds(5), authenticationTimeout: .seconds(6), attachMux: false)
            } catch {
                try Task.checkCancellation()
                guard let failure = error as? NativeSSHError, failure.isCredentialRefused else {
                    _ = try plan.failed(.terminal, attempt: attempt)
                    throw error
                }
                guard offered == 1, attempt.selection.credentialID == "key", signer.calls == 0,
                      await !broker.isBusy, await manager.pendingCount == 0,
                      await manager.connections().isEmpty,
                      WorkspaceNetworkActivity.shared.accounting().connections == 0,
                      NativeSSHConnector.accounting().resolvingHosts == 0,
                      NativeSSHConnector.accounting().connectingSockets == 0,
                      try await journal.records().isEmpty else { throw WorkspaceProbeError.invalid }
                guard try plan.failed(.credentialRefused, attempt: attempt)?.selection.credentialID == "key-2" else {
                    throw WorkspaceProbeError.invalid
                }
                if mode != "orderedSuccess" { await boundary.pause() }
                continue
            }
            do {
                try Task.checkCancellation()
                guard offered == 2, try plan.connected(attempt).credentialID == "key-2",
                      connection.peer.hostKey == host, signer.calls == 1,
                      try await journal.records().isEmpty else { throw WorkspaceProbeError.invalid }
                return connection
            } catch { try? await manager.close(connectionID: connection.id); throw error }
        }
        throw WorkspaceProbeError.unexpected
    }
    if mode != "orderedSuccess" {
        await boundary.wait()
        switch mode {
        case "orderedCancel":
            // Like a synchronously sealed native scene, cancel before awaiting
            // manager invalidation; a bare invalidate permits a NEW user action.
            operation.cancel()
            await manager.invalidate(workspaceID: installed.id)
        case "orderedRevokeNext":
            guard let next = document.credentials.first(where: { $0.id == "key-2" }) else { throw WorkspaceProbeError.invalid }
            let result = try await manager.revokeCredentialLocally(fingerprint: next.fingerprint, confirmedByUser: true)
            guard result.durableDenialConfirmed, result.affectedWorkspaceIDs == [installed.id],
                  result.retainedConnectionIDs.isEmpty else { throw WorkspaceProbeError.invalid }
        case "orderedStaleRevision":
            let crypto = ProbeCryptography()
            let replacement = try AirGapArchive(workspace: document.reissuingBundle(id: "ordered-reimport"), keys: [], using: crypto)
            let imported = try await store.install(replacement, envelopeDigest: crypto.sha256(try replacement.encoded()),
                expectedRevision: installed.revision, sourceAndContentsConfirmed: true,
                securityChangesConfirmed: true, associateWithExistingWorkspace: true).workspace
            guard imported.revision != installed.revision else { throw WorkspaceProbeError.invalid }
            await manager.invalidate(workspaceID: installed.id)
        case "orderedSuspend": await manager.suspend()
        default: throw WorkspaceProbeError.invalid
        }
        await boundary.release()
        do { _ = try await operation.value; throw WorkspaceProbeError.unexpected }
        catch {
            switch mode {
            case "orderedCancel": guard error is CancellationError else { throw error }
            case "orderedRevokeNext": guard error as? SSHAuthenticationError == .denied else { throw error }
            case "orderedStaleRevision": guard error as? SSHAuthenticationError == .stale else { throw error }
            case "orderedSuspend": guard error as? WorkspaceSSHConnectionError == .offline else { throw error }
            default: throw error
            }
        }
        guard signer.calls == 0, await !broker.isBusy, await manager.pendingCount == 0,
              await manager.connections().isEmpty, try await journal.records().isEmpty,
              WorkspaceNetworkActivity.shared.accounting().connections == 0,
              NativeSSHConnector.accounting().resolvingHosts == 0,
              NativeSSHConnector.accounting().connectingSockets == 0 else { throw WorkspaceProbeError.invalid }
        await manager.suspend()
        return 0
    }
    let connection = try await operation.value
    let pump = try await connection.openTerminal(geometry: .init(columns: 80, rows: 24))
    let consumer = ManagedConsumer(source: pump.source, connection: connection)
    let run = Task { try await pump.run(consumer: consumer) }
    let watchdog = Task {
        do { try await Task.sleep(for: .seconds(7)); pump.requestStop() } catch {}
    }
    defer { watchdog.cancel() }
    do {
        try await consumer.waitForGreeting()
        let stream = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(stream, to: pump.source)
        let payload = Data("protected workspace → actual SSH 🦀\r\nEND-SESSION\r".utf8)
        let receipt = try await coordinator.submit(focus, encodedBytes: payload)
        guard try await run.value == .completed(.init(remoteExit: nil, channelClosed: true)) else { throw WorkspaceProbeError.invalid }
        try await consumer.verify(receipt: receipt, payload: payload)
        try await mustFail { try await manager.close(connectionID: connection.id) }
        try await coordinator.suspend(focus)
        try await pump.reconcileStoppedOutcomes { outcomes, source in try await connection.recordInput(outcomes, source: source) }
        try await connection.prepareInputSettlement(source: pump.source)
        try await coordinator.retire(pump.source)
        try await manager.close(connectionID: connection.id)
        let records = try await journal.records(), closed = await coordinator.accounting()
        guard records.count == 1, records[0].scope == connection.scope, records[0].state == .interrupted,
              records[0].canResolve, records[0].inputEvidence.contains(.transportAccepted),
              closed.connections == 0, closed.streams == 0, closed.retainedBytes == 0,
              closed.inFlightBytes == 0, await manager.pendingCount == 0,
              await manager.connections().isEmpty,
              WorkspaceNetworkActivity.shared.accounting().connections == 0 else { throw WorkspaceProbeError.invalid }
        try await journal.resolve(records[0])
        await manager.suspend()
        return payload.count
    } catch {
        pump.requestStop(); try? await manager.close(connectionID: connection.id)
        _ = try? await run.value
        throw error
    }
}

private func exerciseManagedConnection(store: WorkspaceStore, installed: InstalledWorkspace,
    broker: SSHAuthenticationBroker, directory: URL,
    host: Data, failFinalPublication: Bool = false, revokeFingerprint: String? = nil) async throws -> Int {
    let identifier = "workspace-probe-" + UUID().uuidString
    let fault = try failFinalPublication ? FailedJournalPublication(directory: directory) : nil
    let journal: TerminalSessionJournal
    if let fault { journal = try TerminalSessionJournal(persistence: fault) }
    else { journal = try durableJournal(directory: directory, identifier: identifier) }
    let coordinator = try TerminalInputCoordinator()
    let manager = WorkspaceSSHConnectionManager(store: store, broker: broker,
        journal: journal, coordinator: coordinator)
    do {
        _ = try await manager.connect(workspaceID: installed.id, expectedRevision: installed.revision,
            machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true)
        throw WorkspaceProbeError.unexpected
    } catch WorkspaceSSHConnectionError.offline {}
    guard NativeSSHConnector.accounting().resolvingHosts == 0,
          NativeSSHConnector.accounting().connectingSockets == 0,
          WorkspaceNetworkActivity.shared.accounting().connections == 0 else { throw WorkspaceProbeError.invalid }
    await manager.resume()
    do {
        _ = try await manager.connect(workspaceID: installed.id, expectedRevision: installed.revision,
            machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: false)
        throw WorkspaceProbeError.unexpected
    } catch WorkspaceNetworkError.confirmationRequired {}
    let connection = try await manager.connect(workspaceID: installed.id, expectedRevision: installed.revision,
        machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true,
        connectionTimeout: .seconds(5), authenticationTimeout: .seconds(6))
    do {
        guard connection.peer.hostKey == host, connection.scope.workspaceID == installed.id,
              connection.scope.workspaceRevision == installed.revision,
              connection.identity.workspaceID == installed.id.uuidString,
              connection.identity.machineID == "machine", connection.identity.routeID == "route",
              connection.identity.transportID == connection.peer.transportID,
              WorkspaceNetworkActivity.shared.accounting().connections == 1,
              try await journal.records().isEmpty,
              await coordinator.accounting().connections == 0 else { throw WorkspaceProbeError.invalid }
        do {
            let restore = try WorkspaceNetworkActivity.shared.beginOfflineRestore()
            withExtendedLifetime(restore) {}
            throw WorkspaceProbeError.unexpected
        } catch WorkspaceNetworkError.connectionsActive {}
        do {
            _ = try await manager.connect(workspaceID: installed.id, expectedRevision: installed.revision,
                machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true)
            throw WorkspaceProbeError.unexpected
        } catch WorkspaceSSHConnectionError.busy {}
        let pump = try await connection.openTerminal(geometry: .init(columns: 80, rows: 24))
        let active = try await journal.records()
        guard active.count == 1, active[0].scope == connection.scope, active[0].state == .creationStarted,
              active[0].requiresReview, !active[0].canResolve else { throw WorkspaceProbeError.invalid }
        try await mustFail { try await journal.resolve(active[0]) }
        // No service has started: the first resize owns the native slot. A
        // second request is ordinary backpressure and must not tear down this
        // authenticated connection or discard the accepted geometry.
        try await connection.requestResize(.init(columns: 132, rows: 42), source: pump.source)
        do {
            try await connection.requestResize(.init(columns: 160, rows: 50), source: pump.source)
            throw WorkspaceProbeError.unexpected
        } catch NativeSSHError.busy {}
        guard await !connection.isClosed else { throw WorkspaceProbeError.invalid }
        try await mustFail { try await connection.prepareInputSettlement(source: pump.source) }
        let consumer = ManagedConsumer(source: pump.source, connection: connection)
        let run = Task { try await pump.run(consumer: consumer) }
        let watchdog = Task {
            do { try await Task.sleep(for: .seconds(7)); pump.requestStop() } catch {}
        }
        defer { watchdog.cancel() }
        do {
            try await consumer.waitForGreeting()
            let stream = try await coordinator.makeStream(sceneID: UUID())
            let focus = try await coordinator.changeFocus(stream, to: pump.source)
            let payload = Data("protected workspace → actual SSH 🦀\r\nEND-SESSION\r".utf8)
            let receipt = try await coordinator.submit(focus, encodedBytes: payload)
            guard try await run.value == .completed(.init(remoteExit: nil, channelClosed: true)) else { throw WorkspaceProbeError.invalid }
            try await consumer.verify(receipt: receipt, payload: payload)
            // EOF alone does not erase uncertain remote effects or release the
            // connection owner's epoch. Explicit local teardown retires it.
            let delivered = try await journal.records()
            guard delivered.first?.state == .creationStarted,
                  delivered.first?.inputEvidence.contains(.transportAccepted) == true,
                  delivered.first?.inputEvidence.contains(.acceptedPrefix) == true else { throw WorkspaceProbeError.invalid }
            // Native close cannot make the epoch reviewable while the scene
            // still owns a responder. The manager retains its admission slot.
            if let revokeFingerprint {
                let foreground = try await broker.foregroundLease()
                let revoked = try await manager.revokeCredentialLocally(fingerprint: revokeFingerprint, confirmedByUser: true)
                guard revoked.durableDenialConfirmed, revoked.affectedWorkspaceIDs == [installed.id],
                      revoked.retainedConnectionIDs == [connection.id] else { throw WorkspaceProbeError.invalid }
                try await broker.validateForeground(foreground)
            } else { try await mustFail { try await manager.close(connectionID: connection.id) } }
            guard WorkspaceNetworkActivity.shared.accounting().connections == 0,
                  await manager.connections().count == 1,
                  try await journal.records().first?.canResolve == false else { throw WorkspaceProbeError.invalid }
            try await mustFail { _ = try await coordinator.submit(focus, encodedBytes: Data("late".utf8)) }
            try await coordinator.suspend(focus)
            try await pump.reconcileStoppedOutcomes { outcomes, source in
                try await connection.recordInput(outcomes, source: source)
            }
            try await connection.prepareInputSettlement(source: pump.source)
            try await coordinator.retire(pump.source)
            if let fault {
                fault.failNextWrite()
                if let revokeFingerprint {
                    let retry = try await manager.revokeCredentialLocally(fingerprint: revokeFingerprint, confirmedByUser: true)
                    guard retry.durableDenialConfirmed, retry.retainedConnectionIDs == [connection.id] else {
                        throw WorkspaceProbeError.invalid
                    }
                } else { try await mustFail { try await manager.close(connectionID: connection.id) } }
                guard await coordinator.accounting().connections == 0,
                      WorkspaceNetworkActivity.shared.accounting().connections == 0,
                      await manager.connections().count == 1,
                      try await journal.records().first?.canResolve == true else { throw WorkspaceProbeError.invalid }
            }
            try await manager.close(connectionID: connection.id)
            let closed = await coordinator.accounting(), records = try await journal.records()
            guard closed.connections == 0, closed.streams == 0, closed.retainedBytes == 0,
                  closed.retainedRecords == 0, closed.inFlightBytes == 0,
                  WorkspaceNetworkActivity.shared.accounting().connections == 0,
                  records.count == 1, records[0].state == .interrupted, records[0].requiresReview,
                  records[0].canResolve, records[0].inputEvidence.contains(.transportAccepted),
                  records[0].inputEvidence.contains(.acceptedPrefix),
                  await manager.connections().isEmpty else { throw WorkspaceProbeError.invalid }
            do {
                _ = try await manager.connect(workspaceID: installed.id, expectedRevision: installed.revision,
                    machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true)
                throw WorkspaceProbeError.unexpected
            } catch {
                if revokeFingerprint != nil {
                    guard error as? SSHAuthenticationError == .denied else { throw error }
                } else {
                    guard error as? TerminalSessionJournalError == .reviewRequired else { throw error }
                }
            }
            guard NativeSSHConnector.accounting().resolvingHosts == 0,
                  NativeSSHConnector.accounting().connectingSockets == 0 else { throw WorkspaceProbeError.invalid }
            try await journal.resolve(records[0])
            let reopened: TerminalSessionJournal
            if let fault { reopened = try TerminalSessionJournal(persistence: fault) }
            else { reopened = try durableJournal(directory: directory, identifier: identifier) }
            let recovered = try await reopened.records()
            guard recovered.count == 1, recovered[0].state == .acknowledged, !recovered[0].requiresReview,
                  await manager.connections().isEmpty, await manager.pendingCount == 0 else { throw WorkspaceProbeError.invalid }
            await manager.suspend()
            return payload.count
        } catch {
            pump.requestStop()
            try? await manager.close(connectionID: connection.id)
            _ = try? await run.value
            throw error
        }
    } catch {
        try? await manager.close(connectionID: connection.id)
        throw error
    }
}

/// Replacement removes the old key's last current-manifest reference. The
/// manager must still find it in a pending or live owner's original revision.
private func replaceRevocationIdentity(store: WorkspaceStore, installed: InstalledWorkspace,
                                      document: WorkspaceDocument) async throws {
    guard var object = try JSONSerialization.jsonObject(with: document.raw) as? [String: Any],
          var workspace = object["workspace"] as? [String: Any],
          var credentials = workspace["credentials"] as? [[String: Any]], credentials.count == 1 else {
        throw WorkspaceProbeError.invalid
    }
    credentials[0]["public_fingerprint"] = "SHA256:" + Data(repeating: 213, count: 32).base64EncodedString().dropLast()
    workspace["credentials"] = credentials; object["workspace"] = workspace; object["bundle_id"] = "replacement-key"
    let document = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]))
    let crypto = ProbeCryptography(), replacement = try AirGapArchive(workspace: document, keys: [], using: crypto)
    _ = try await store.install(replacement, envelopeDigest: crypto.sha256(try replacement.encoded()),
        expectedRevision: installed.revision, sourceAndContentsConfirmed: true,
        securityChangesConfirmed: true, associateWithExistingWorkspace: true)
}

private func exerciseManagedRevocation(store: WorkspaceStore, installed: InstalledWorkspace,
    broker: SSHAuthenticationBroker, signer: ProbeSigner, document: WorkspaceDocument,
    fingerprint: String, duringAuthentication: Bool, reimport: Bool) async throws {
    let journal = try TerminalSessionJournal(persistence: ProbePersistence())
    let manager = try WorkspaceSSHConnectionManager(store: store, broker: broker,
        journal: journal, coordinator: TerminalInputCoordinator())
    await manager.resume()
    let foreground = try await broker.foregroundLease()
    let connecting = Task {
        try await manager.connect(workspaceID: installed.id, expectedRevision: installed.revision,
            machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true,
            connectionTimeout: .seconds(5), authenticationTimeout: .seconds(6))
    }
    do {
        let connection: WorkspaceSSHConnection?
        if duringAuthentication { await signer.waitUntilStarted(); connection = nil }
        else { connection = try await connecting.value }
        if reimport { try await replaceRevocationIdentity(store: store, installed: installed, document: document) }
        let revoked = Task { try await manager.revokeCredentialLocally(fingerprint: fingerprint, confirmedByUser: true) }
        if duringAuthentication {
            await signer.waitUntilCancelled()
            // The persistence boundary precedes signer cancellation. The
            // protected callback remains held while the receipt's caller exits.
            guard try await store.locallyRevokedCredentialFingerprints().contains(fingerprint),
                  await broker.isBusy, await manager.pendingCount == 1 else { throw WorkspaceProbeError.invalid }
            revoked.cancel()
            try await broker.validateForeground(foreground)
            signer.finish()
            try await mustFail { _ = try await connecting.value }
        }
        let receipt = try await revoked.value
        guard receipt.durableDenialConfirmed, receipt.affectedWorkspaceIDs == [installed.id],
              receipt.retainedConnectionIDs.isEmpty, await manager.connections().isEmpty,
              await manager.pendingCount == 0, await manager.permitsConnections,
              await !broker.isBusy, WorkspaceNetworkActivity.shared.accounting().connections == 0 else {
            throw WorkspaceProbeError.invalid
        }
        try await broker.validateForeground(foreground)
        if let connection {
            guard await connection.isClosed else { throw WorkspaceProbeError.invalid }
            try await mustFail { _ = try await connection.openTerminal(geometry: .init(columns: 80, rows: 24)) }
        }
        guard try await journal.records().isEmpty else { throw WorkspaceProbeError.invalid }
        await manager.suspend()
    } catch {
        signer.finish(); connecting.cancel()
        _ = try? await connecting.value
        await manager.suspend()
        throw error
    }
}

private func exerciseCloseDuringCreation(store: WorkspaceStore, installed: InstalledWorkspace,
    broker: SSHAuthenticationBroker, directory: URL) async throws {
    let persistence = try ParkedJournalPersistence(directory: directory)
    let journal = try TerminalSessionJournal(persistence: persistence), coordinator = try TerminalInputCoordinator()
    let manager = WorkspaceSSHConnectionManager(store: store, broker: broker,
        journal: journal, coordinator: coordinator)
    await manager.resume()
    let connection = try await manager.connect(workspaceID: installed.id, expectedRevision: installed.revision,
        machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true,
        connectionTimeout: .seconds(5), authenticationTimeout: .seconds(6))
    let previousForeground = try await broker.foregroundLease()
    let opening = Task { try await connection.openTerminal(geometry: .init(columns: 80, rows: 24)) }
    await persistence.waitUntilParked()
    let completion = CompletionFlag()
    let closing = Task { await manager.suspend(); await completion.finish() }
    do {
        let deadline = ContinuousClock.now + .seconds(2)
        while await !connection.isClosed {
            guard ContinuousClock.now < deadline else { throw WorkspaceProbeError.unexpected }
            await Task.yield()
        }
        // Give a wrongly detached close time to complete while the actual
        // persistence call remains deterministically parked.
        try await Task.sleep(for: .milliseconds(30))
        guard await !completion.finished else { throw WorkspaceProbeError.invalid }
        // A new foreground intent may arrive while the previous shutdown joins
        // storage. It must not revive old grants or be overwritten when that
        // older shutdown eventually returns.
        await manager.resume()
        guard await manager.permitsConnections, await !completion.finished else { throw WorkspaceProbeError.invalid }
        try await mustFail { try await broker.validateForeground(previousForeground) }
        let latestForeground = try await broker.foregroundLease()
        try await broker.validateForeground(latestForeground)
        persistence.release()
        try await mustFail { _ = try await opening.value }
        await closing.value
        guard await completion.finished, await coordinator.accounting().connections == 0,
              await manager.connections().isEmpty,
              await manager.permitsConnections,
              WorkspaceNetworkActivity.shared.accounting().connections == 0 else { throw WorkspaceProbeError.invalid }
        try await broker.validateForeground(latestForeground)
        let records = try await journal.records()
        guard records.count == 1, records[0].scope == connection.scope,
              records[0].state == .interrupted, records[0].requiresReview, records[0].canResolve else {
            throw WorkspaceProbeError.invalid
        }
        try await mustFail { _ = try await connection.openTerminal(geometry: .init(columns: 80, rows: 24)) }
        await manager.suspend()
    } catch {
        persistence.release()
        _ = try? await opening.value
        await closing.value
        await manager.suspend()
        throw error
    }
}

private func exerciseConcurrentCloseDuringBinding(store: WorkspaceStore, installed: InstalledWorkspace,
    broker: SSHAuthenticationBroker, directory: URL) async throws {
    let persistence = try ParkedJournalPersistence(directory: directory, boundary: .inputBindingRead)
    let journal = try TerminalSessionJournal(persistence: persistence), coordinator = try TerminalInputCoordinator()
    let manager = WorkspaceSSHConnectionManager(store: store, broker: broker,
        journal: journal, coordinator: coordinator)
    await manager.resume()
    let connection = try await manager.connect(workspaceID: installed.id, expectedRevision: installed.revision,
        machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true,
        connectionTimeout: .seconds(5), authenticationTimeout: .seconds(6))
    let opening = Task { try await connection.openTerminal(geometry: .init(columns: 80, rows: 24)) }
    await persistence.waitUntilParked()
    let managedClose = Task { try await manager.close(connectionID: connection.id) }
    // Direct closes deliberately bypass the manager's existing job coalescing.
    // The failed opening wrapper is another independent native close caller.
    let closes = (0..<8).map { _ in Task { try await connection.close() } }
    do {
        let deadline = ContinuousClock.now + .seconds(2)
        while await !connection.isClosed {
            guard ContinuousClock.now < deadline else { throw WorkspaceProbeError.unexpected }
            await Task.yield()
        }
        let parked = await coordinator.accounting()
        guard parked.connections == 1, parked.streams == 0, parked.retainedBytes == 0,
              parked.retainedRecords == 0, parked.inFlightBytes == 0,
              await manager.connections().count == 1 else { throw WorkspaceProbeError.invalid }
        persistence.release()
        try await mustFail { _ = try await opening.value }
        try await managedClose.value
        for close in closes { try await close.value }
        // Successful retirement remains idempotent after its source disappears.
        try await connection.close()
        try await manager.close(connectionID: connection.id)
        let accounting = await coordinator.accounting(), records = try await journal.records()
        guard accounting.connections == 0, accounting.streams == 0,
              accounting.retainedBytes == 0, accounting.retainedRecords == 0,
              accounting.inFlightBytes == 0, await manager.connections().isEmpty,
              await manager.pendingCount == 0,
              WorkspaceNetworkActivity.shared.accounting().connections == 0,
              records.count == 1, records[0].scope == connection.scope,
              records[0].state == .interrupted, records[0].canResolve,
              records[0].requiresReview, records[0].inputEvidence.isEmpty else {
            throw WorkspaceProbeError.invalid
        }
        try await mustFail { _ = try await connection.openTerminal(geometry: .init(columns: 80, rows: 24)) }
        await manager.suspend()
    } catch {
        persistence.release()
        _ = try? await opening.value
        _ = try? await managedClose.value
        for close in closes { _ = try? await close.value }
        await manager.suspend()
        throw error
    }
}

@main
private enum WorkspaceProbe {
    static func main() async throws {
        guard CommandLine.arguments.count == 4, let port = UInt16(CommandLine.arguments[1]),
              let hostBlob = Data(base64Encoded: CommandLine.arguments[2]),
              let input = try FileHandle.standardInput.read(upToCount: 16_385), input.count <= 16_384,
              let request = try JSONSerialization.jsonObject(with: input) as? [String: String],
              let pemText = request["pem"] else { throw WorkspaceProbeError.invalid }
        let mode = CommandLine.arguments[3]
        let delayed: Set<String> = ["revokeKeyDuring", "revokeHostDuring", "reimportDuring", "cancelDuring", "suspendDuring",
            "managedRevokeDuring", "managedRevokeDuringReimport"]
        let positive: Set<String> = ["io", "foreignCancel"]
        let crypto = ProbeCryptography(), key = try PrivateKeyRecord.fromPEM(Data(pemText.utf8), credentialID: "key", using: crypto)
        let identity = try SSHPublicIdentity(blob: key.publicBlob), host = try SSHPublicIdentity(blob: hostBlob)
        let second = try request["secondPEM"].map { try PrivateKeyRecord.fromPEM(Data($0.utf8), credentialID: "key-2", using: crypto) }
        let document = try manifest(identity: identity, host: host, port: port,
            secondIdentity: second.map { try SSHPublicIdentity(blob: $0.publicBlob) })
        let archive = try AirGapArchive(workspace: document, keys: [key] + (second.map { [$0] } ?? []), using: crypto)
        let vault = ProbeVault(), persistence = ProbePersistence()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let installed = try await store.install(archive, envelopeDigest: crypto.sha256(try archive.encoded()),
            transferID: Data(repeating: 1, count: 16), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        guard installed.connectLater, document.routes.first?.host == "localhost" else { throw WorkspaceProbeError.invalid }
        let support = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
            credentialAlgorithms: Set(document.credentials.map(\.algorithm)), hostKeyAlgorithms: [host.algorithm])
        let signer = ProbeSigner(vault: vault, held: delayed.contains(mode))
        let broker = SSHAuthenticationBroker(store: store, signer: signer, support: support)
        await broker.resume()
        if mode.hasPrefix("ordered") {
            guard second != nil, let path = request["journalDirectory"] else { throw WorkspaceProbeError.invalid }
            let bytes = try await exerciseOrderedCredentials(store: store, installed: installed, document: document,
                broker: broker, signer: signer, directory: URL(fileURLWithPath: path, isDirectory: true), host: hostBlob, mode: mode)
            FileHandle.standardOutput.write(try JSONSerialization.data(withJSONObject: ["mode": mode, "bytes": bytes,
                "signs": signer.calls, "closed": true], options: [.sortedKeys]) + Data([10]))
            return
        }
        if ["managedRevokeDuring", "managedRevokeDuringReimport", "managedRevokeAfterReimport"].contains(mode) {
            try await exerciseManagedRevocation(store: store, installed: installed, broker: broker, signer: signer,
                document: document, fingerprint: identity.fingerprint, duringAuthentication: delayed.contains(mode),
                reimport: mode != "managedRevokeDuring")
            FileHandle.standardOutput.write(try JSONSerialization.data(withJSONObject: ["mode": mode, "bytes": 0,
                "signs": signer.calls, "cancellations": signer.cancellations, "closed": true,
                "algorithm": identity.algorithm, "fingerprint": identity.fingerprint], options: [.sortedKeys]) + Data([10]))
            return
        }
        if mode == "managed" || mode == "managedClose" || mode == "managedFinalWriteFailure" || mode == "managedBindingClose"
            || mode == "managedRevokeFinalWriteFailure" {
            guard let path = request["journalDirectory"] else { throw WorkspaceProbeError.invalid }
            let directory = URL(fileURLWithPath: path, isDirectory: true), bytes: Int
            if mode == "managed" || mode == "managedFinalWriteFailure" || mode == "managedRevokeFinalWriteFailure" {
                bytes = try await exerciseManagedConnection(store: store, installed: installed, broker: broker,
                    directory: directory, host: hostBlob, failFinalPublication: mode != "managed",
                    revokeFingerprint: mode == "managedRevokeFinalWriteFailure" ? identity.fingerprint : nil)
            } else if mode == "managedClose" {
                try await exerciseCloseDuringCreation(store: store, installed: installed, broker: broker,
                    directory: directory)
                bytes = 0
            } else {
                try await exerciseConcurrentCloseDuringBinding(store: store, installed: installed, broker: broker,
                    directory: directory)
                bytes = 0
            }
            guard signer.calls == 1, await !broker.isBusy else { throw WorkspaceProbeError.invalid }
            FileHandle.standardOutput.write(try JSONSerialization.data(withJSONObject: ["mode": mode, "bytes": bytes,
                "signs": signer.calls, "closed": true, "algorithm": identity.algorithm, "fingerprint": identity.fingerprint],
                options: [.sortedKeys]) + Data([10]))
            return
        }
        if mode == "revokeKeyBefore" { try await store.revokeLocally(fingerprint: identity.fingerprint) }
        if mode == "revokeHostBefore" {
            try await store.revokeHostKeyLocally(host: "LOCALHOST.", port: port, algorithm: host.algorithm, fingerprint: host.fingerprint)
        }
        let session = try await NativeSSHConnector.connect(to: .init(host: "localhost", port: port), timeout: .seconds(5))
        do {
            let authority = WorkspaceNativeSSHAuthentication(broker: broker, workspaceID: installed.id,
                revision: mode == "staleRevision" ? UUID() : installed.revision, routeID: "route", credentialID: "key",
                transportID: session.transportID, endpoint: try .init(host: "localhost", port: port))
            if mode == "foreignCancel" { authority.cancel(transportID: UUID()) }
            let authentication = Task { try await session.authenticate(using: authority, timeout: .seconds(6)) }
            if delayed.contains(mode) {
                await signer.waitUntilStarted()
                switch mode {
                case "revokeKeyDuring": try await store.revokeLocally(fingerprint: identity.fingerprint)
                case "revokeHostDuring":
                    try await store.revokeHostKeyLocally(host: "LOCALHOST.", port: port, algorithm: host.algorithm, fingerprint: host.fingerprint)
                case "reimportDuring":
                    let replacement = try AirGapArchive(workspace: document.reissuingBundle(id: "reimport"), keys: [], using: crypto)
                    let replaced = try await store.install(replacement, envelopeDigest: crypto.sha256(try replacement.encoded()),
                        transferID: Data(repeating: 2, count: 16), expectedRevision: installed.revision, sourceAndContentsConfirmed: true,
                        securityChangesConfirmed: true, associateWithExistingWorkspace: true).workspace
                    guard replaced.revision != installed.revision else { throw WorkspaceProbeError.invalid }
                case "cancelDuring":
                    authentication.cancel()
                    await signer.waitUntilCancelled()
                    guard await broker.isBusy else { throw WorkspaceProbeError.invalid }
                case "suspendDuring":
                    await broker.suspend()
                    guard await broker.isBusy, signer.cancellations > 0 else { throw WorkspaceProbeError.invalid }
                default: throw WorkspaceProbeError.invalid
                }
                signer.finish()
            }
            var bytes = 0
            if positive.contains(mode) || mode == "beforeShellRevocation" {
                let peer = try await authentication.value
                guard peer.transportID == session.transportID, peer.endpoint == session.endpoint,
                      peer.hostKey == hostBlob, signer.calls == 1 else { throw WorkspaceProbeError.invalid }
                try await authority.validateCurrent()
                let authorization = try await broker.authorization(workspaceID: installed.id, expectedRevision: installed.revision,
                    routeID: "route", credentialID: "key", presentedHostKey: .init(algorithm: host.algorithm, fingerprint: host.fingerprint))
                guard authorization.workspaceID == installed.id, authorization.workspaceRevision == installed.revision,
                      authorization.host == "localhost", authorization.port == port, authorization.user == "operator",
                      authorization.fingerprint == identity.fingerprint else { throw WorkspaceProbeError.invalid }
                if mode == "beforeShellRevocation" {
                    try await store.revokeLocally(fingerprint: identity.fingerprint)
                    try await mustFail { try await session.openShell(geometry: .init(columns: 80, rows: 24)) }
                } else { bytes = try await exerciseShell(session) }
            } else {
                try await mustFail { _ = try await authentication.value }
                guard signer.calls == (delayed.contains(mode) ? 1 : 0) else { throw WorkspaceProbeError.invalid }
                try await mustFail { try await session.openShell(geometry: .init(columns: 80, rows: 24)) }
            }
            guard await !broker.isBusy else { throw WorkspaceProbeError.invalid }
            await session.close()
            let accounting = try await session.accounting()
            guard accounting.allocatedBytes == 0, accounting.pendingWriteBytes == 0,
                  accounting.locallyAcceptedBytes == bytes, accounting.channelAcceptedBytes == bytes,
                  NativeSSHConnector.accounting().resolvingHosts == 0,
                  NativeSSHConnector.accounting().connectingSockets == 0 else { throw WorkspaceProbeError.invalid }
            let result: [String: Any] = ["mode": mode, "bytes": bytes, "signs": signer.calls,
                "cancellations": signer.cancellations, "closed": true, "algorithm": identity.algorithm,
                "fingerprint": identity.fingerprint]
            FileHandle.standardOutput.write(try JSONSerialization.data(withJSONObject: result, options: [.sortedKeys]) + Data([10]))
        } catch {
            signer.finish()
            await session.close()
            throw error
        }
    }
}
