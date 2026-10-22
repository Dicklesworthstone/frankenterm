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
    /// Present when this connection attaches to the machine's existing mux
    /// panes (like the desktop app) instead of starting a shell.
    public nonisolated let muxControl: TerminalMuxControl?
    /// The command that reaches the machine's mux when no route names one:
    /// the default socket of a FrankenTerm mux server.
    public static let defaultMuxCommand = "nc -N -U /run/user/$(id -u)/frankenterm/sock"
    private let transport: WorkspaceNativeSSHTransport
    private let mux: (command: String, factory: @Sendable () throws -> any TerminalMuxClient)?
    private var attached: TerminalSessionPump?
    private var coordinator: TerminalInputCoordinator?
    private var opening: Task<TerminalSessionPump, Error>?
    private var openingAttempted = false, sourcePublished = false, closed = false
    private var shutdown: Task<Void, Never>?
    private var settlement: (id: UUID, task: Task<Void, Error>)?
    private var journalEpoch: (journal: TerminalSessionJournal, epoch: TerminalSessionEpoch)?
    private var inputSource: TerminalSource?
    private var inputSeal: TerminalInputSeal?

    private init(transport: WorkspaceNativeSSHTransport, peer: NativeSSHPeer, scope: TerminalSessionScope,
                 mux: (command: String, factory: @Sendable () throws -> any TerminalMuxClient)?) {
        self.transport = transport; self.peer = peer; identity = transport.identity
        self.scope = scope; workspaceRevision = scope.workspaceRevision
        self.mux = mux; muxControl = transport.muxControl
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
                               authenticationTimeout: Duration = .seconds(60),
                               liveness: WorkspaceSSHLiveness = .standard,
                               muxClientFactory: (@Sendable () throws -> any TerminalMuxClient)? = nil,
                               attachMux: Bool = true) async throws -> WorkspaceNativeSSHConnection {
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
              route.jumpRoutes.isEmpty,
              (route.mode == "direct_ssh" && (route.sessionCreation == "on_select" || route.sessionCreation == "manual"))
                || (route.mode == "frankenterm_mux" && (route.attachPolicy ?? "existing_only") == "existing_only"),
              let ready = entry.routes.first(where: { $0.routeID == routeID }),
              ready.isLocallyReady, ready.usableCredentialIDs.contains(credentialID) else {
            throw SSHAuthenticationError.denied
        }
        // A mux route attaches to the machine's existing panes with the same
        // command the desktop runs. A plain SSH route tries the default mux
        // socket first; the caller falls back to a shell when none answers.
        let muxCommand: String?
        if route.mode == "frankenterm_mux" {
            guard muxClientFactory != nil else { throw SSHAuthenticationError.denied }
            muxCommand = route.remoteCommand ?? defaultMuxCommand
        } else {
            muxCommand = attachMux && muxClientFactory != nil ? defaultMuxCommand : nil
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
                foreground: foreground, networkLease: lease, authority: authority, liveness: liveness,
                muxControl: muxCommand == nil ? nil : TerminalMuxControl())
            let mux = muxCommand.flatMap { command in muxClientFactory.map { (command: command, factory: $0) } }
            return WorkspaceNativeSSHConnection(transport: transport, peer: peer, scope: scope, mux: mux)
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
        if let mux, let muxControl {
            try await transport.openMux(command: mux.command, geometry: geometry,
                                        client: try mux.factory(), control: muxControl)
        } else {
            try await transport.openShell(geometry: geometry)
        }
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
    nonisolated let muxControl: TerminalMuxControl?
    nonisolated var acceptsModelReplies: Bool { muxControl == nil }

    nonisolated func captureInputWriter() throws -> any TerminalInputWriter {
        guard let muxControl else { return self }
        return WorkspaceMuxInputWriter(transport: self, routing: muxControl.inputRouting,
                                       destination: try muxControl.inputRouting.capture())
    }
    private let session: NativeSSHSession
    private let broker: SSHAuthenticationBroker
    private let foreground: SSHForegroundLease
    private let authority: WorkspaceNativeSSHAuthentication
    private var driver: NativeSSHDriver?
    private var networkLease: WorkspaceNetworkLease?
    private var closed = false
    private var shutdown: Task<Void, Never>?
    private let liveness: WorkspaceSSHLiveness
    private var probing = true, lastReceived: UInt64 = 0, silentIntervals = 0
    private var mux: MuxAttachment?

    /// Bytes for the phone's terminal, applied after their resize.
    private struct Segment {
        var resize: TerminalSessionGeometry?
        var bytes: Data
        var destination: TerminalMuxInputRouting.Destination? = nil
    }

    /// One attachment to the machine's mux over this SSH command channel.
    /// Only the transport actor touches it.
    private final class MuxAttachment {
        let client: any TerminalMuxClient
        let control: TerminalMuxControl
        /// The phone terminal's size as the screen last asked for it.
        var geometry: TerminalSessionGeometry
        /// The model's size once every queued segment is applied, and as
        /// already applied.
        var queued: TerminalSessionGeometry, applied: TerminalSessionGeometry
        var panes: [TerminalMuxPane] = []
        var listed = false, ready = false, flushing = false
        var shown: Int?
        /// The shown pane is sized to the phone and drawing.
        var showing = false
        var segments: [Segment] = []
        var outgoing = Data()
        /// The client's search and row-read ids, to the screen's requests.
        var searches: [UInt64: (id: UUID, pane: Int)] = [:]
        var contexts: [UInt64: UUID] = [:]
        init(client: any TerminalMuxClient, control: TerminalMuxControl, geometry: TerminalSessionGeometry) {
            self.client = client; self.control = control; self.geometry = geometry
            queued = geometry; applied = geometry
        }
    }

    init(session: NativeSSHSession, identity: TerminalConnectionIdentity, broker: SSHAuthenticationBroker,
         foreground: SSHForegroundLease, networkLease: WorkspaceNetworkLease,
         authority: WorkspaceNativeSSHAuthentication, liveness: WorkspaceSSHLiveness,
         muxControl: TerminalMuxControl?) {
        self.session = session; self.identity = identity; self.broker = broker
        self.foreground = foreground; self.networkLease = networkLease; self.authority = authority
        self.liveness = liveness; self.muxControl = muxControl
    }

    func openShell(geometry: NativeSSHGeometry) async throws {
        do {
            try await validateForeground()
            guard driver == nil, muxControl == nil else { throw NativeSSHError.busy }
            try await session.openShell(geometry: geometry)
            try await validateForeground()
            driver = try await session.claimTerminalDriver(transportID: identity.transportID, channelID: identity.channelID)
            try await validateForeground()
        } catch { await close(); throw error }
    }

    /// Attaches to the machine's mux instead of starting a shell: the same
    /// relay command the desktop runs, then the mux protocol over it. Fails
    /// with `TerminalMuxError.unavailable` when nothing answers there.
    func openMux(command: String, geometry: NativeSSHGeometry, client: any TerminalMuxClient,
                 control: TerminalMuxControl) async throws {
        do {
            try await validateForeground()
            guard driver == nil, control === muxControl else { throw NativeSSHError.busy }
            try await session.openCommand(command)
            try await validateForeground()
            let driver = try await session.claimTerminalDriver(transportID: identity.transportID,
                                                               channelID: identity.channelID)
            self.driver = driver
            let state = MuxAttachment(client: client, control: control, geometry: try TerminalSessionGeometry(
                columns: geometry.columns, rows: geometry.rows,
                pixelWidth: geometry.pixelWidth, pixelHeight: geometry.pixelHeight))
            mux = state
            // The server's protocol check and its pane list.
            let deadline = ContinuousClock.now + .seconds(15)
            while !state.listed {
                guard ContinuousClock.now < deadline else { throw TerminalMuxError.unavailable }
                _ = try await pumpMux(timeout: .seconds(5), driver: driver)
                try await validateForeground()
            }
            guard let first = state.panes.first(where: \.isActive) ?? state.panes.first else {
                throw TerminalMuxError.noPanes
            }
            state.shown = first.paneId
            try control.inputRouting.select(pane: first.paneId)
            control.publish(panes: state.panes, shown: first.paneId)
            control.setWaker { [weak self] in Task { await self?.wakeService() } }
            try reconcileMux()
            try await flushMux(driver: driver)
        } catch { await close(); throw error }
    }

    /// One turn of the mux's SSH channel: send what the client has queued,
    /// read what arrived and apply its events.
    private func pumpMux(timeout: Duration, driver: NativeSSHDriver) async throws -> NativeSSHServiceEvent {
        guard let state = mux else { throw NativeSSHError.closed }
        try await flushMux(driver: driver)
        let event = try await session.service(timeout: timeout, driver: driver)
        if !event.output.isEmpty { try state.client.receive(event.output) }
        if event.endOfOutput {
            try state.client.channelClosed()
            throw state.ready ? TerminalTransportError.connectionLost : TerminalMuxError.unavailable
        }
        try drainMux()
        return event
    }

    /// Admits queued protocol bytes to the channel's single native slot.
    private func flushMux(driver: NativeSSHDriver) async throws {
        guard let state = mux, !state.flushing else { return }
        state.flushing = true
        defer { state.flushing = false }
        while true {
            if state.outgoing.isEmpty { state.outgoing = try state.client.outgoing(limit: 1 << 20) }
            guard !state.outgoing.isEmpty else { return }
            let chunk = Data(state.outgoing.prefix(NativeSSHSession.maximumWriteBytes))
            switch try await session.admit(chunk, driver: driver) {
            case .accepted(let count): state.outgoing.removeFirst(count)
            case .wouldBlock: return
            }
        }
    }

    private func drainMux() throws {
        guard let state = mux else { return }
        while let event = try state.client.nextEvent() {
            switch event {
            case .ready:
                state.ready = true
            case .panes(let panes):
                try state.control.inputRouting.replaceInventory(panes.map(\.paneId))
                state.panes = panes; state.listed = true
                if let shown = state.shown, !panes.contains(where: { $0.paneId == shown }) {
                    state.shown = nil; state.showing = false
                }
                state.control.publish(panes: panes, shown: state.shown)
            case .output(let pane, let rows, let columns, let bytes):
                guard pane == state.shown, state.showing else { continue }
                // The phone's terminal holds what fits its cell budget. A pane
                // someone else made larger is asked back to the phone's size.
                guard (1...512).contains(rows), (1...512).contains(columns),
                      (rows * 2 + 256) * columns <= 65536 else {
                    try state.client.resize(pane: pane, rows: Int(state.geometry.rows),
                                            columns: Int(state.geometry.columns))
                    continue
                }
                let cellWidth = state.geometry.pixelWidth / max(state.geometry.columns, 1)
                let cellHeight = state.geometry.pixelHeight / max(state.geometry.rows, 1)
                let size = try TerminalSessionGeometry(columns: UInt32(columns), rows: UInt32(rows),
                    pixelWidth: min(cellWidth * UInt32(columns), 65535), pixelHeight: min(cellHeight * UInt32(rows), 65535))
                let resize = size == state.queued ? nil : size
                state.queued = size
                state.segments.append(Segment(resize: resize, bytes: bytes,
                    destination: try state.control.inputRouting.capture()))
            case .paneTitle(let pane, let title):
                if let index = state.panes.firstIndex(where: { $0.paneId == pane }) {
                    let old = state.panes[index]
                    state.panes[index] = TerminalMuxPane(paneId: old.paneId, tabId: old.tabId, windowId: old.windowId,
                        tabTitle: old.tabTitle, title: title, workspace: old.workspace, workingDir: old.workingDir,
                        size: old.size, isActive: old.isActive, isZoomed: old.isZoomed)
                }
                state.control.retitle(pane: pane, title: title)
            case .paneGone(let pane):
                state.control.inputRouting.remove(pane: pane)
                state.panes.removeAll { $0.paneId == pane }
                state.control.remove(pane: pane)
                if state.shown == pane { state.shown = nil; state.showing = false }
            case .searchResults(let pane, let results):
                if let request = state.searches.removeValue(forKey: results.search) {
                    state.control.deliver(search: request.id, pane: pane, results: results)
                }
            case .context(_, let context):
                if let id = state.contexts.removeValue(forKey: context.request) {
                    state.control.deliver(context: id, lines: context.lines)
                }
            case .failed(let reason):
                throw state.ready ? TerminalTransportError.connectionLost : TerminalMuxError.failed(reason)
            }
        }
    }

    /// Puts the right pane on screen: the one the user picked, sized to the
    /// phone and drawing while this machine is visible. A pane that leaves the
    /// screen stops drawing and gets its own size back, so the desktop's
    /// layout is untouched by a phone that is not looking at it.
    private func reconcileMux() throws {
        guard let state = mux else { return }
        if let requested = state.control.takeRequest(), requested != state.shown,
           state.panes.contains(where: { $0.paneId == requested }) {
            if let old = state.shown, state.showing {
                try state.client.hide(pane: old); try state.client.restoreSize(pane: old)
            }
            state.shown = requested; state.showing = false
            try state.control.inputRouting.select(pane: requested)
            // Finish already-decoded output events before the replacement.
            // Dropping a chunk's suffix could strand the canonical parser in
            // UTF-8 or an escape sequence. Old visit markers cannot enable the
            // new pane, and its retired view no longer accepts these frames.
            state.control.publish(shown: requested)
        }
        // A vanished selection stays vanished. Neither old queued input nor a
        // newly created responder may silently inherit an unrelated pane.
        // Searches run on the server, each pane's scrollback where it lives.
        if let search = state.control.takeSearch() {
            state.searches.removeAll()
            for pane in search.panes {
                do {
                    let id = try state.client.search(pane: pane, text: search.query.text,
                        caseSensitive: search.query.caseSensitive, regex: search.query.regex)
                    state.searches[id] = (search.id, pane)
                } catch {
                    state.control.deliver(search: search.id, pane: pane, results: TerminalMuxSearchResults(
                        search: 0, matches: [], searchedRows: 0, error: "The search could not be sent."))
                }
            }
        }
        if let context = state.control.takeContext() {
            state.contexts.removeAll()
            if let id = try? state.client.context(pane: context.pane, firstRow: max(context.first, 0), count: context.count) {
                state.contexts[id] = context.id
            } else {
                state.control.deliver(context: context.id, lines: [])
            }
        }
        guard let pane = state.shown else { return }
        let visible = state.control.current.visible
        if visible, !state.showing {
            try state.client.resize(pane: pane, rows: Int(state.geometry.rows), columns: Int(state.geometry.columns))
            try state.client.show(pane: pane)
            state.showing = true
        } else if !visible, state.showing {
            try state.client.hide(pane: pane); try state.client.restoreSize(pane: pane)
            state.showing = false
        }
    }

    /// The next bytes for the phone's terminal, at most one receive's worth.
    private func nextMuxUpdate() -> TerminalSessionUpdate? {
        guard let state = mux, !state.segments.isEmpty else { return nil }
        var segment = state.segments.removeFirst()
        let chunk = Data(segment.bytes.prefix(16_384))
        segment.bytes.removeFirst(chunk.count)
        let resize = segment.resize
        if let resize { state.applied = resize }
        if !segment.bytes.isEmpty {
            segment.resize = nil
            state.segments.insert(segment, at: 0)
        }
        return TerminalSessionUpdate(output: chunk, resized: resize,
            muxDestination: segment.bytes.isEmpty ? segment.destination : nil)
    }

    func didApplyOutput(_ update: TerminalSessionUpdate) async {
        guard !closed, let destination = update.muxDestination else { return }
        muxControl?.inputRouting.didApplyOutput(destination)
    }

    private func serviceMux(driver: NativeSSHDriver) async throws -> TerminalSessionUpdate {
        try reconcileMux()
        if let update = nextMuxUpdate() { return update }
        let event = try await pumpMux(timeout: liveness.interval, driver: driver)
        try await validateForeground()
        try await checkLiveness(event, driver: driver)
        try reconcileMux()
        try await flushMux(driver: driver)
        return nextMuxUpdate() ?? TerminalSessionUpdate(output: Data())
    }

    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        do {
            try await validateForeground()
            guard let driver else { throw NativeSSHError.closed }
            guard !bytes.isEmpty, bytes.count <= 65536 else { throw TerminalInputError.invalidWriteSize }
            // Mux input requires the focus-captured writer below. Looking up
            // state.shown here would retarget queued keys/paste after a swipe.
            guard muxControl == nil else { throw TerminalInputError.invalidIdentity }
            switch try await session.admit(Data(bytes.prefix(NativeSSHSession.maximumWriteBytes)), driver: driver) {
            case .accepted(let count): return .accepted(count)
            case .wouldBlock: return .wouldBlock
            }
        } catch { await close(); throw error }
    }

    func writeMux(_ bytes: Data, destination: TerminalMuxInputRouting.Destination) async throws -> TerminalTransportWrite {
        do {
            try await validateForeground()
            guard let driver, let state = mux else { throw NativeSSHError.closed }
            guard !bytes.isEmpty, bytes.count <= 65536 else { throw TerminalInputError.invalidWriteSize }
            // Recheck after authorization's awaits. No actor suspension occurs
            // between verifying the registration and encoding its numeric target.
            try state.control.inputRouting.validateDelivery(destination)
            guard state.outgoing.count < 256 * 1024 else { return .wouldBlock }
            try state.client.write(pane: destination.paneID, bytes: bytes)
            try await flushMux(driver: driver)
            return .accepted(bytes.count)
        } catch { await close(); throw error }
    }

    func service() async throws -> TerminalSessionUpdate {
        do {
            try await validateForeground()
            guard let driver else { throw NativeSSHError.closed }
            if mux != nil { return try await serviceMux(driver: driver) }
            let event = try await session.service(timeout: liveness.interval, driver: driver)
            try await validateForeground()
            try await checkLiveness(event, driver: driver)
            let geometry = try event.resized.map {
                try TerminalSessionGeometry(columns: $0.columns, rows: $0.rows,
                                            pixelWidth: $0.pixelWidth, pixelHeight: $0.pixelHeight)
            }
            return .init(output: event.output, endOfOutput: event.endOfOutput, resized: geometry,
                         completion: event.completion?.terminalCompletion)
        } catch {
            // Only a failure that precedes any owner's close is a lost
            // connection; after one, a socket error is that close's effect.
            let lost = !closed && (error as? NativeSSHError)?.isConnectionFailure == true
            await close()
            throw lost ? TerminalTransportError.connectionLost : error
        }
    }

    /// Any byte from the server proves it is reachable. Each idle interval
    /// without one sends a probe that a live server must answer; silence
    /// through `missedReplies` further intervals means it is not answering.
    /// A deferred probe counts too: outbound has stayed blocked a whole interval.
    private func checkLiveness(_ event: NativeSSHServiceEvent, driver: NativeSSHDriver) async throws {
        guard probing else { return }
        if event.accounting.receivedBytes != lastReceived {
            lastReceived = event.accounting.receivedBytes; silentIntervals = 0
        }
        guard event.waitExpired else { return }
        silentIntervals += 1
        guard silentIntervals <= liveness.missedReplies else { throw TerminalTransportError.peerUnresponsive }
        if try await session.keepalive(driver: driver) == .unavailable { probing = false }
    }

    func requestResize(_ geometry: NativeSSHGeometry) async throws {
        do {
            try await validateForeground()
            guard let driver else { throw NativeSSHError.closed }
            if let state = mux {
                // A mux pane has no PTY here: the server resizes it, and the
                // pane's next output arrives at the new size, which completes
                // this request through the ordered update path.
                let size = try TerminalSessionGeometry(columns: geometry.columns, rows: geometry.rows,
                    pixelWidth: geometry.pixelWidth, pixelHeight: geometry.pixelHeight)
                state.geometry = size
                if (size.rows, size.columns) == (state.queued.rows, state.queued.columns) {
                    // Same cells, new pixels: nothing for the server to redraw.
                    state.queued = size
                    state.segments.append(Segment(resize: size, bytes: Data()))
                } else if let pane = state.shown, state.showing {
                    try state.client.resize(pane: pane, rows: Int(size.rows), columns: Int(size.columns))
                }
                try await flushMux(driver: driver)
                try await session.wakeService(driver: driver)
                return
            }
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
        // Fence input before the first await. Concurrent closes share the same
        // actual restore/teardown job, not a prematurely completed closed flag.
        muxControl?.inputRouting.retire()
        muxControl?.setWaker(nil)
        closed = true; inputSignal.finish()
        let session = self.session, lease = networkLease
        let task = Task<Void, Never> {
            if let state = self.mux, let driver = self.driver, let pane = state.shown, state.showing {
                try? state.client.restoreSize(pane: pane)
                try? await self.flushMux(driver: driver)
                _ = try? await session.service(timeout: .milliseconds(300), driver: driver)
            }
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
        muxControl?.inputRouting.retire()
        inputSignal.finish()
        let session = self.session, lease = networkLease
        Task {
            await session.close()
            withExtendedLifetime(lease) {}
        }
    }
}

/// The coordinator owns this small immutable value with its queued bytes. A
/// selected-pane change invalidates new admission, not the original destination
/// of already admitted writes or their bounded key/paste cleanup.
private struct WorkspaceMuxInputWriter: TerminalInputWriter {
    let transport: WorkspaceNativeSSHTransport
    let routing: TerminalMuxInputRouting
    let destination: TerminalMuxInputRouting.Destination
    func validateInputAdmission() throws { try routing.validateAdmission(destination) }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        try await transport.writeMux(bytes, destination: destination)
    }
}
