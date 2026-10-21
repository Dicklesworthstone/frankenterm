import Foundation
import FrankenTermKit
import FrankenTermSSH

/// One explicitly requested route, from local admission through authenticated
/// SSH and the existing terminal pump. No imported auto-connect preference calls
/// this factory. No raw native session escapes its network-scope owner.
///
/// The app cancels connect and closes its owned connections on deactivation. The
/// broker's foreground witness also rejects a result crossing suspend/resume.
/// A direct-SSH close has no remote-shell survival guarantee and never reconnects
/// or repeats session creation automatically.
public actor WorkspaceNativeSSHConnection {
    public nonisolated let identity: TerminalConnectionIdentity
    public nonisolated let peer: NativeSSHPeer
    public nonisolated let workspaceRevision: UUID
    public nonisolated let scope: TerminalSessionScope
    private let transport: WorkspaceNativeSSHTransport
    private var attached: TerminalSessionPump?
    private var coordinator: TerminalInputCoordinator?
    private var opening: Task<TerminalSessionPump, Error>?
    private var openingAttempted = false, sourcePublished = false, closed = false
    private var shutdown: Task<Void, Never>?
    private var settlement: (id: UUID, task: Task<Void, Error>)?
    private var journalEpoch: (journal: TerminalSessionJournal, epoch: TerminalSessionEpoch)?
    private var inputSource: TerminalSource?
    private var inputSeal: TerminalInputSeal?

    private init(transport: WorkspaceNativeSSHTransport, peer: NativeSSHPeer, scope: TerminalSessionScope) {
        self.transport = transport; self.peer = peer; identity = transport.identity
        self.scope = scope; workspaceRevision = scope.workspaceRevision
    }
    public var isClosed: Bool { closed }

    /// The explicit confirmation is independent of imported preferences and is
    /// required on every new connection. The durable Connect Later bit remains
    /// intact, including across relaunch. A stale/disabled/unsupported route or
    /// missing selected identity is refused before hostname resolution begins.
    public static func connect(workspaceID: UUID, expectedRevision: UUID, machineID: String,
                               routeID: String, credentialID: String, confirmedByUser: Bool,
                               store: WorkspaceStore, broker: SSHAuthenticationBroker,
                               journal: TerminalSessionJournal? = nil,
                               networkActivity: WorkspaceNetworkActivity = .shared,
                               connectionTimeout: Duration = .seconds(15),
                               authenticationTimeout: Duration = .seconds(60)) async throws -> WorkspaceNativeSSHConnection {
        guard confirmedByUser else { throw WorkspaceNetworkError.confirmationRequired }
        try Task.checkCancellation()
        let foreground = try await broker.foregroundLease()
        if let journal {
            let records = try await journal.records()
            guard !records.contains(where: {
                $0.requiresReview && $0.scope.workspaceID == workspaceID && $0.scope.machineID == machineID
            }) else { throw TerminalSessionJournalError.reviewRequired }
        }
        let lease = try networkActivity.beginConnection()
        let entries = try await store.loadWithReadiness(support: WorkspaceNativeSSHAuthentication.routeSupport)
        guard let entry = entries.first(where: { $0.workspace.id == workspaceID }),
              entry.workspace.revision == expectedRevision else { throw SSHAuthenticationError.stale }
        let document = try entry.workspace.document()
        guard let machine = document.machines.first(where: { $0.id == machineID }),
              machine.routeIDs.contains(routeID),
              let route = document.routes.first(where: { $0.id == routeID }),
              route.mode == "direct_ssh", route.jumpRoutes.isEmpty,
              route.sessionCreation == "on_select" || route.sessionCreation == "manual",
              let ready = entry.routes.first(where: { $0.routeID == routeID }),
              ready.isLocallyReady, ready.usableCredentialIDs.contains(credentialID) else {
            throw SSHAuthenticationError.denied
        }
        let endpoint = try NativeSSHEndpoint(host: route.host, port: route.port)
        try await broker.validateForeground(foreground)
        try Task.checkCancellation()
        let session = try await NativeSSHConnector.connect(to: endpoint, timeout: connectionTimeout, networkLifetime: lease)
        do {
            try await broker.validateForeground(foreground)
            try Task.checkCancellation()
            let authority = WorkspaceNativeSSHAuthentication(broker: broker, workspaceID: workspaceID,
                revision: expectedRevision, routeID: routeID, credentialID: credentialID,
                transportID: session.transportID, endpoint: endpoint)
            let peer = try await session.authenticate(using: authority, timeout: authenticationTimeout)
            try await broker.validateForeground(foreground)
            try Task.checkCancellation()
            // This source never changes route revision in place. The complete
            // durable workspace revision is retained separately, and every new
            // source has an independent transport UUID and channel UUID.
            let identity = try TerminalConnectionIdentity(workspaceID: workspaceID.uuidString,
                machineID: machineID, routeID: routeID, routeRevision: 1,
                transportID: session.transportID, channelID: session.channelID)
            let scope = try TerminalSessionScope(workspaceID: workspaceID, workspaceRevision: expectedRevision,
                machineID: machineID, routeID: routeID, transportID: session.transportID,
                channelID: session.channelID)
            let transport = WorkspaceNativeSSHTransport(session: session, identity: identity, broker: broker,
                foreground: foreground, networkLease: lease, authority: authority)
            return WorkspaceNativeSSHConnection(transport: transport, peer: peer, scope: scope)
        } catch {
            await session.close()
            // Keep exclusion until cancellation/readiness teardown has joined.
            withExtendedLifetime(lease) {}
            throw error
        }
    }

    /// Separate deliberate remote side effect. prepareForInput must durably
    /// record this exact source's uncertainty marker before returning. Failure
    /// creates no shell. The caller supplies the canonical terminal consumer to
    /// the returned pump; this owner adds no ANSI parser or local echo. Custom
    /// durability providers retain their own marker and cleanup responsibility;
    /// production workspace owners should use the journal overload below.
    public func openTerminal(geometry: NativeSSHGeometry, coordinator: TerminalInputCoordinator,
                             prepareForInput: @escaping @Sendable (TerminalConnectionIdentity) async throws -> Void) async throws -> TerminalSessionPump {
        try await startOpening(geometry: geometry, coordinator: coordinator,
                               journal: nil, prepareForInput: prepareForInput)
    }

    /// Persist the exact source's creation uncertainty before requesting a remote
    /// shell. This owner retains the epoch through cancellation, native teardown,
    /// and retryable storage errors. Every attempt is explicit and occurs once.
    /// The scene must close this owner when pump.run ends, including EOF/error,
    /// and separately join its pump consumer and outcome delivery.
    public func openTerminal(geometry: NativeSSHGeometry, coordinator: TerminalInputCoordinator,
                             journal: TerminalSessionJournal) async throws -> TerminalSessionPump {
        try await startOpening(geometry: geometry, coordinator: coordinator,
                               journal: journal, prepareForInput: nil)
    }

    private func startOpening(geometry: NativeSSHGeometry, coordinator: TerminalInputCoordinator,
                              journal: TerminalSessionJournal?,
                              prepareForInput: (@Sendable (TerminalConnectionIdentity) async throws -> Void)?)
        async throws -> TerminalSessionPump {
        try Task.checkCancellation()
        guard !closed, !openingAttempted, attached == nil else { throw NativeSSHError.busy }
        openingAttempted = true
        self.coordinator = coordinator
        let task = Task<TerminalSessionPump, Error> {
            try await self.performOpen(geometry: geometry, coordinator: coordinator,
                                       journal: journal, prepareForInput: prepareForInput)
        }
        opening = task
        return try await withTaskCancellationHandler {
            do {
                let pump = try await task.value
                try await validateCurrent()
                sourcePublished = true
                opening = nil
                return pump
            } catch {
                // This wrapper is outside the retained opening task. Its close
                // can join that task without creating an open/close self-join.
                task.cancel()
                await close()
                opening = nil
                throw error
            }
        } onCancel: { task.cancel() }
    }

    private func performOpen(geometry: NativeSSHGeometry, coordinator: TerminalInputCoordinator,
                             journal: TerminalSessionJournal?,
                             prepareForInput: (@Sendable (TerminalConnectionIdentity) async throws -> Void)?)
        async throws -> TerminalSessionPump {
        try await validateCurrent()
        if let journal {
            let epoch = try await journal.beginCreation(scope: scope)
            // Close may have entered while the durable write was in progress.
            // Store the returned owner before checking closed/cancelled state;
            // shutdown joins this task before attempting epoch retirement.
            journalEpoch = (journal, epoch)
        } else if let prepareForInput {
            try await prepareForInput(identity)
        } else { throw NativeSSHError.closed }
        try await validateCurrent()
        try await transport.openShell(geometry: geometry)
        try await validateCurrent()
        let pump = try await TerminalSessionPump.attach(transport, coordinator: coordinator)
        attached = pump
        do {
            if let retained = journalEpoch {
                try await retained.journal.bindInput(epoch: retained.epoch, to: pump.source)
                inputSource = pump.source
            }
            try await validateCurrent()
        }
        catch {
            try? await pump.disconnect()
            // The outer opening owner joins this task and caches any bound
            // input seal before retiring this unpublished source.
            throw error
        }
        return pump
    }

    public func requestResize(_ geometry: NativeSSHGeometry) async throws {
        do {
            try await validateCurrent()
            guard attached != nil else { throw NativeSSHError.closed }
            try await transport.requestResize(geometry)
        } catch NativeSSHError.busy {
            // One immutable resize is already pending. Its service completion
            // is the retry boundary; normal backpressure does not lose a shell.
            throw NativeSSHError.busy
        } catch { await close(); throw error }
    }

    /// The manager facade preserves the exact coordinator-issued source witness
    /// without retaining a second attachment or exposing the native owner.
    func requestResize(_ geometry: NativeSSHGeometry, source: TerminalSource) async throws {
        guard attached?.source == source else { throw TerminalInputError.stale }
        try await requestResize(geometry)
    }

    /// Persist only metadata from this exact source before the pump acknowledges
    /// its outcome batch. The retained epoch remains usable after native close
    /// while the scene joins and reconciles its final consumer work.
    func recordInput(_ outcomes: [TerminalInputOutcome], source: TerminalSource) async throws {
        guard source == inputSource, outcomes.allSatisfy({ $0.receipt.source == source }),
              let retained = journalEpoch else { throw TerminalInputError.stale }
        try await retained.journal.recordInput(outcomes, epoch: retained.epoch)
    }

    /// Cache the coordinator's immutable proof before the scene retires its
    /// source. A later protected-storage failure can retry publication without
    /// retaining a closed coordinator slot or minting from a retired source.
    func prepareInputSettlement(source: TerminalSource) async throws {
        guard source == inputSource, closed, let coordinator else { throw TerminalInputError.stale }
        guard inputSeal == nil else { return }
        inputSeal = try await coordinator.mintSeal(for: source)
    }

    /// Stop input before joining any prompts, opening operation, or native I/O.
    /// Managers can seal every owned source first, then join their closes. This
    /// barrier does not itself wait for native teardown or forget its journal.
    public func sealAdmission() async {
        closed = true
        opening?.cancel()
        if let pump = attached {
            pump.requestStop()
            if let coordinator { try? await coordinator.stopAdmission(pump.source) }
        }
    }

    /// Compatibility close. Storage failure retains the epoch and its durable
    /// uncertainty; application owners use closeAndRecordInterruption to surface
    /// the failure and retain this object for an explicit retry.
    public func close() async {
        try? await closeAndRecordInterruption()
    }

    /// Join native teardown and any in-progress creation before retiring its
    /// epoch. A marker that arrives during close is included in this same join.
    /// On storage failure the epoch remains owned for retry; another shell is
    /// never authorized. Concurrent closes share native teardown and the same
    /// settlement attempt; only a failed settlement may start a fresh retry.
    public func closeAndRecordInterruption() async throws {
        await sealAdmission()
        let task: Task<Void, Never>
        if let existing = shutdown { task = existing }
        else {
            let pump = attached, transport = self.transport, opening = self.opening
            task = Task<Void, Never> {
                if let pump { try? await pump.disconnect() }
                await transport.close()
                // performOpen never calls this public close path. Its own
                // transport cleanup can finish independently before this join.
                if let opening { _ = await opening.result }
            }
            shutdown = task
        }
        await task.value
        let attempt: (id: UUID, task: Task<Void, Error>)
        if let existing = settlement { attempt = existing }
        else {
            attempt = (UUID(), Task { try await self.settleAfterNativeShutdown() })
            settlement = attempt
        }
        do { try await attempt.task.value }
        catch {
            // Another waiter on this failed attempt must not erase a newer
            // retry. A successful task remains retained as the close result.
            if settlement?.id == attempt.id { settlement = nil }
            throw error
        }
    }

    private func settleAfterNativeShutdown() async throws {
        if !sourcePublished, let pump = attached, let coordinator {
            // A task result may have been completed but refused by the outer
            // cancellation/closed check before its source reached the caller.
            // No responder or consumer can own this unpublished source. This
            // retained settlement job alone may retire it across actor awaits.
            try? await pump.disconnect()
            if inputSource == pump.source { try await prepareInputSettlement(source: pump.source) }
            try await coordinator.retire(pump.source)
        }
        attached = nil
        // A failed settlement can retry protected-storage publication after
        // source retirement; its immutable seal remains owned by this object.
        if let retained = journalEpoch {
            if let source = inputSource {
                // Published responders and outcomes must settle first. Refusing
                // here retains both the epoch and the manager's admission slot;
                // native teardown has already joined and cannot reopen.
                try await prepareInputSettlement(source: source)
                guard let inputSeal else { throw TerminalInputError.unavailable }
                _ = try await retained.journal.sealInput(epoch: retained.epoch, boundary: inputSeal)
            } else { try await retained.journal.markInterrupted(epoch: retained.epoch) }
            if journalEpoch?.epoch.id == retained.epoch.id { journalEpoch = nil }
        }
    }

    private func validateCurrent() async throws {
        try Task.checkCancellation()
        guard !closed else { throw NativeSSHError.closed }
        try await transport.validateForeground()
        guard !closed else { throw NativeSSHError.closed }
        try Task.checkCancellation()
    }

    /// A manager's final fence after its awaited factory call. Use the same
    /// foreground and durable authority checks as the owned native operations.
    func validateForPublication() async throws {
        try await validateCurrent()
    }

    deinit {
        let pump = attached, transport = self.transport, retained = journalEpoch
        let source = inputSource, seal = inputSeal
        Task {
            if let pump { try? await pump.disconnect() }
            await transport.close()
            if let retained {
                if let seal { _ = try? await retained.journal.sealInput(epoch: retained.epoch, boundary: seal) }
                else if source == nil { try? await retained.journal.markInterrupted(epoch: retained.epoch) }
                // A bound source without proof retains its durable uncertainty;
                // deinitialization cannot invent completed input settlement.
            }
        }
    }
}

/// The sole byte-service capability stays inside the connection and its pump.
/// No cycle points back to the public owner, and every native operation repeats
/// the current foreground and durable workspace authorization checks.
private actor WorkspaceNativeSSHTransport: TerminalSessionTransport {
    nonisolated let identity: TerminalConnectionIdentity
    nonisolated let inputSignal = TerminalSessionSignal()
    private let session: NativeSSHSession
    private let broker: SSHAuthenticationBroker
    private let foreground: SSHForegroundLease
    private let authority: WorkspaceNativeSSHAuthentication
    private var driver: NativeSSHDriver?
    private var networkLease: WorkspaceNetworkLease?
    private var closed = false
    private var shutdown: Task<Void, Never>?

    init(session: NativeSSHSession, identity: TerminalConnectionIdentity, broker: SSHAuthenticationBroker,
         foreground: SSHForegroundLease, networkLease: WorkspaceNetworkLease,
         authority: WorkspaceNativeSSHAuthentication) {
        self.session = session; self.identity = identity; self.broker = broker
        self.foreground = foreground; self.networkLease = networkLease; self.authority = authority
    }

    func openShell(geometry: NativeSSHGeometry) async throws {
        do {
            try await validateForeground()
            guard driver == nil else { throw NativeSSHError.busy }
            try await session.openShell(geometry: geometry)
            try await validateForeground()
            driver = try await session.claimTerminalDriver(transportID: identity.transportID, channelID: identity.channelID)
            try await validateForeground()
        } catch { await close(); throw error }
    }

    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        do {
            try await validateForeground()
            guard let driver else { throw NativeSSHError.closed }
            guard !bytes.isEmpty, bytes.count <= 65536 else { throw TerminalInputError.invalidWriteSize }
            switch try await session.admit(Data(bytes.prefix(NativeSSHSession.maximumWriteBytes)), driver: driver) {
            case .accepted(let count): return .accepted(count)
            case .wouldBlock: return .wouldBlock
            }
        } catch { await close(); throw error }
    }

    func service() async throws -> TerminalSessionUpdate {
        do {
            try await validateForeground()
            guard let driver else { throw NativeSSHError.closed }
            let event = try await session.service(driver: driver)
            try await validateForeground()
            let geometry = try event.resized.map {
                try TerminalSessionGeometry(columns: $0.columns, rows: $0.rows,
                                            pixelWidth: $0.pixelWidth, pixelHeight: $0.pixelHeight)
            }
            return .init(output: event.output, endOfOutput: event.endOfOutput, resized: geometry)
        } catch { await close(); throw error }
    }

    func requestResize(_ geometry: NativeSSHGeometry) async throws {
        do {
            try await validateForeground()
            guard let driver else { throw NativeSSHError.closed }
            try await session.requestResize(geometry, driver: driver)
        } catch NativeSSHError.busy {
            throw NativeSSHError.busy
        } catch { await close(); throw error }
    }

    func wakeService() async {
        guard !closed, let driver else { return }
        do { try await session.wakeService(driver: driver) }
        catch { await close() }
    }

    /// Join the native close before releasing offline exclusion. Concurrent
    /// callers await the same teardown, and no subsequent call can reopen it.
    func close() async {
        if let shutdown { await shutdown.value; return }
        closed = true; inputSignal.finish()
        let session = self.session, lease = networkLease
        let task = Task<Void, Never> {
            await session.close()
            withExtendedLifetime(lease) {}
        }
        shutdown = task
        await task.value
        networkLease = nil
    }

    func validateForeground() async throws {
        try Task.checkCancellation()
        guard !closed else { throw NativeSSHError.closed }
        try await broker.validateForeground(foreground)
        try await authority.validateCurrent()
        guard !closed else { throw NativeSSHError.closed }
        try Task.checkCancellation()
    }

    deinit {
        inputSignal.finish()
        let session = self.session, lease = networkLease
        Task {
            await session.close()
            withExtendedLifetime(lease) {}
        }
    }
}
