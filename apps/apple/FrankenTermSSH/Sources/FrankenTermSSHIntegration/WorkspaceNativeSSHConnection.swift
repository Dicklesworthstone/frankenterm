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
                               attachMux: Bool = true, muxAccessMode: TerminalMuxAccessMode = .interactive,
                               requiresExistingMux: Bool = false) async throws -> WorkspaceNativeSSHConnection {
        try await connect(workspaceID: workspaceID, expectedRevision: expectedRevision, machineID: machineID,
            routeID: routeID, authentication: .publicKey(credentialID), confirmedByUser: confirmedByUser,
            store: store, broker: broker, journal: journal, networkActivity: networkActivity,
            connectionTimeout: connectionTimeout, authenticationTimeout: authenticationTimeout, liveness: liveness,
            muxClientFactory: muxClientFactory, attachMux: attachMux, muxAccessMode: muxAccessMode,
            requiresExistingMux: requiresExistingMux)
    }

    public static func connect(workspaceID: UUID, expectedRevision: UUID, machineID: String,
                               routeID: String, authentication: WorkspaceSSHAuthenticationChoice, confirmedByUser: Bool,
                               store: WorkspaceStore, broker: SSHAuthenticationBroker,
                               passwordPrompter: (any WorkspaceSSHPasswordPrompting)? = nil,
                               keyboardInteractivePrompter: (any WorkspaceSSHKeyboardInteractivePrompting)? = nil,
                               journal: TerminalSessionJournal? = nil,
                               networkActivity: WorkspaceNetworkActivity = .shared,
                               connectionTimeout: Duration = .seconds(15),
                               authenticationTimeout: Duration = .seconds(60),
                               liveness: WorkspaceSSHLiveness = .standard,
                               muxClientFactory: (@Sendable () throws -> any TerminalMuxClient)? = nil,
                               attachMux: Bool = true, muxAccessMode: TerminalMuxAccessMode = .interactive,
                               requiresExistingMux: Bool = false) async throws -> WorkspaceNativeSSHConnection {
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
              (route.mode == "direct_ssh" && (route.sessionCreation == "on_select" || route.sessionCreation == "manual"))
                || (route.mode == "frankenterm_mux" && (route.attachPolicy ?? "existing_only") == "existing_only"),
              let ready = entry.routes.first(where: { $0.routeID == routeID }),
              ready.isLocallyReady,
              WorkspaceSSHAuthenticationChoice.choices(for: route, usableCredentialIDs: ready.usableCredentialIDs)
                .contains(authentication) else {
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
        // An explicit Observe/Use Existing Pane request cannot manufacture a
        // direct shell when its mux transport is absent or disabled.
        guard !requiresExistingMux || muxCommand != nil else { throw TerminalMuxError.unavailable }
        let endpoint = try NativeSSHEndpoint(host: route.host, port: route.port)
        let hops = try document.orderedJumpRoutes(forRouteID: routeID)
        let hopChoices = try hops.map { hop -> [WorkspaceSSHAuthenticationChoice] in
            guard let readiness = entry.routes.first(where: { $0.routeID == hop.id }) else { throw SSHAuthenticationError.denied }
            let choices = WorkspaceSSHAuthenticationChoice.choices(for: hop, usableCredentialIDs: readiness.usableCredentialIDs)
            guard !choices.isEmpty else { throw SSHAuthenticationError.denied }
            return choices
        }
        guard hops.count <= WorkspaceNativeSSHAuthentication.routeSupport.maximumJumpHops else {
            throw SSHAuthenticationError.denied
        }
        guard passwordPrompter != nil || (authentication != .password && !hopChoices.contains(where: { $0.contains(.password) })) else {
            throw SSHAuthenticationError.unavailable
        }
        guard keyboardInteractivePrompter != nil || (authentication != .keyboardInteractive
            && !hopChoices.contains(where: { $0.contains(.keyboardInteractive) })) else {
            throw SSHAuthenticationError.unavailable
        }
        try await broker.validateForeground(foreground)
        try Task.checkCancellation()
        let session: NativeSSHSession
        let promptedHop: Bool
        if !hops.isEmpty {
            let path = try await connectThroughHops(hops, authenticationChoices: hopChoices,
                destination: endpoint, destinationRouteID: routeID, workspaceID: workspaceID,
                revision: expectedRevision, store: store, broker: broker, foreground: foreground,
                passwordPrompter: passwordPrompter, keyboardInteractivePrompter: keyboardInteractivePrompter, lease: lease,
                connectionTimeout: connectionTimeout, authenticationTimeout: authenticationTimeout)
            session = path.session; promptedHop = path.usedPrompt
        } else {
            session = try await NativeSSHConnector.connect(to: endpoint, timeout: connectionTimeout, networkLifetime: lease)
            promptedHop = false
        }
        do {
            try await broker.validateForeground(foreground)
            try Task.checkCancellation()
            let authenticated = try await authenticate(session: session, choice: authentication,
                route: route, workspaceID: workspaceID, revision: expectedRevision, broker: broker,
                passwordPrompter: passwordPrompter, keyboardInteractivePrompter: keyboardInteractivePrompter,
                timeout: authenticationTimeout)
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
                foreground: foreground, networkLease: lease, authority: authenticated.authority, liveness: liveness,
                muxControl: muxCommand == nil ? nil : TerminalMuxControl(accessMode: muxAccessMode))
            let mux = muxCommand.flatMap { command in muxClientFactory.map { (command: command, factory: $0) } }
            return WorkspaceNativeSSHConnection(transport: transport, peer: authenticated.peer, scope: scope, mux: mux)
        } catch {
            await session.close()
            // Keep exclusion until cancellation/readiness teardown has joined.
            withExtendedLifetime(lease) {}
            if (error as? NativeSSHError)?.isCredentialRefused == true, promptedHop {
                throw WorkspaceSSHConnectionError.promptedHopRetryRequired
            }
            throw error
        }
    }

    /// Every hop has independent peer verification and signer authority. A
    /// refused unsigned key closes the entire chain before trying that hop's
    /// next candidate, with fresh ancestor logins. Previously refused candidates
    /// stay consumed across reconstruction; no transit shell or command opens.
    /// A hop failure never masquerades as a refusal of the destination's key.
    private static func connectThroughHops(_ hops: [WorkspaceDocument.Route], authenticationChoices: [[WorkspaceSSHAuthenticationChoice]],
        destination: NativeSSHEndpoint, destinationRouteID: String, workspaceID: UUID, revision: UUID,
        store: WorkspaceStore, broker: SSHAuthenticationBroker, foreground: SSHForegroundLease,
        passwordPrompter: (any WorkspaceSSHPasswordPrompting)?,
        keyboardInteractivePrompter: (any WorkspaceSSHKeyboardInteractivePrompting)?,
        lease: WorkspaceNetworkLease, connectionTimeout: Duration, authenticationTimeout: Duration)
        async throws -> (session: NativeSSHSession, usedPrompt: Bool) {
        var hadCredentialRefusal = false
        var usedPrompt = false
        do {
            guard (1...3).contains(hops.count), authenticationChoices.count == hops.count else {
                throw SSHAuthenticationError.denied
            }
            // Validate every candidate list before any network operation.
            for index in hops.indices {
                _ = try TerminalRouteAttemptPlan(routes: [.init(routeID: hops[index].id,
                                                                authenticationChoices: authenticationChoices[index])])
            }
            var candidateIndices = Array(repeating: 0, count: hops.count)
            // Every retry consumes a previously untried candidate from one hop:
            // at most 24 candidates, independent of the prefix's length.
            while true {
                var session: NativeSSHSession?
                var pending: (index: Int, plan: TerminalRouteAttemptPlan, attempt: TerminalRouteAttemptPlan.Attempt)?
                do {
                    for index in hops.indices {
                        let hop = hops[index]
                        var plan = try TerminalRouteAttemptPlan(routes: [.init(routeID: hop.id,
                            authenticationChoices: Array(authenticationChoices[index].dropFirst(candidateIndices[index])))])
                        guard let attempt = plan.current else { throw SSHAuthenticationError.denied }
                        try Task.checkCancellation()
                        try await broker.validateForeground(foreground)
                        // Revalidate the complete destination graph before each
                        // login and after any chain reconstruction.
                        let current = try await store.loadWithReadiness(support: WorkspaceNativeSSHAuthentication.routeSupport)
                        guard let entry = current.first(where: { $0.workspace.id == workspaceID }),
                              entry.workspace.revision == revision,
                              entry.routes.first(where: { $0.routeID == destinationRouteID })?.isLocallyReady == true,
                              let readiness = entry.routes.first(where: { $0.routeID == hop.id }),
                              WorkspaceSSHAuthenticationChoice.choices(for: hop, usableCredentialIDs: readiness.usableCredentialIDs)
                                .contains(attempt.selection.authentication) else { throw SSHAuthenticationError.stale }
                        try await broker.validateForeground(foreground)
                        try Task.checkCancellation()
                        let endpoint = try NativeSSHEndpoint(host: hop.host, port: hop.port)
                        if session == nil {
                            session = try await NativeSSHConnector.connect(to: endpoint, timeout: connectionTimeout,
                                                                           networkLifetime: lease)
                        }
                        guard let owner = session, owner.endpoint == endpoint else { throw SSHAuthenticationError.denied }
                        pending = (index, plan, attempt)
                        if attempt.selection.authentication.requiresForegroundPrompt { usedPrompt = true }
                        let authenticated = try await authenticate(session: owner, choice: attempt.selection.authentication,
                            route: hop, workspaceID: workspaceID, revision: revision, broker: broker,
                            passwordPrompter: passwordPrompter, keyboardInteractivePrompter: keyboardInteractivePrompter,
                            transitForRouteID: destinationRouteID,
                            timeout: authenticationTimeout)
                        usedPrompt = usedPrompt || authenticated.usedForegroundPrompt
                        _ = try plan.connected(attempt)
                        pending = nil
                        try await broker.validateForeground(foreground)
                        try await owner.validateForwardingPath()
                        try Task.checkCancellation()
                        let next = index + 1 < hops.count
                            ? try NativeSSHEndpoint(host: hops[index + 1].host, port: hops[index + 1].port)
                            : destination
                        session = try await owner.forward(to: next, timeout: connectionTimeout)
                    }
                    guard let child = session else { throw SSHAuthenticationError.denied }
                    try await broker.validateForeground(foreground)
                    try await child.validateForwardingPath()
                    try Task.checkCancellation()
                    return (child, usedPrompt)
                } catch {
                    await session?.close()
                    try Task.checkCancellation()
                    try await broker.validateForeground(foreground)
                    if !usedPrompt, (error as? NativeSSHError)?.isCredentialRefused == true, var refused = pending {
                        hadCredentialRefusal = true
                        if try refused.plan.failed(.credentialRefused, attempt: refused.attempt) != nil {
                            candidateIndices[refused.index] += 1
                            continue
                        }
                    }
                    throw error
                }
            }
        } catch {
            if Task.isCancelled { throw CancellationError() }
            // No answer from the hop stays that: the caller tries the next
            // route and says the jump host could not be reached.
            if !hadCredentialRefusal, !usedPrompt, let failure = error as? NativeSSHError, failure.isUnreachable { throw failure }
            throw NativeSSHError.jumpConnectionFailed
        }
    }

    private static func authenticate(session: NativeSSHSession, choice: WorkspaceSSHAuthenticationChoice,
        route: WorkspaceDocument.Route, workspaceID: UUID, revision: UUID, broker: SSHAuthenticationBroker,
        passwordPrompter: (any WorkspaceSSHPasswordPrompting)?,
        keyboardInteractivePrompter: (any WorkspaceSSHKeyboardInteractivePrompting)?, transitForRouteID: String? = nil,
        timeout: Duration) async throws -> (peer: NativeSSHPeer, authority: any WorkspaceNativeSSHAuthority, usedForegroundPrompt: Bool) {
        let endpoint = try NativeSSHEndpoint(host: route.host, port: route.port)
        switch choice {
        case let .publicKey(credentialID):
            let authority = WorkspaceNativeSSHAuthentication(broker: broker, workspaceID: workspaceID,
                revision: revision, routeID: route.id, credentialID: credentialID, transportID: session.transportID,
                endpoint: endpoint, transitForRouteID: transitForRouteID, allowedJumpRouteIDs: route.jumpRoutes)
            if NativeSSHSession.supportsPublicKeyInteractive, route.authenticationMethods.contains(.keyboardInteractive),
               let keyboardInteractivePrompter {
                let interactive = WorkspaceNativeSSHInteractiveAuthentication(broker: broker, prompter: keyboardInteractivePrompter,
                    workspaceID: workspaceID, revision: revision, routeID: route.id, transportID: session.transportID,
                    endpoint: endpoint, transitForRouteID: transitForRouteID, allowedJumpRouteIDs: route.jumpRoutes,
                    continuingPublicKey: authority)
                // The native owner asks for this second factor only after the
                // server accepts the signed key with partial success. A refused
                // key never becomes permission to display an MFA prompt.
                let result = try await session.authenticate(using: authority, continuingWithInteractive: interactive, timeout: timeout)
                if result.usedKeyboardInteractive {
                    try await interactive.authenticationCompleted()
                    // This authority retains the signed key and revalidates
                    // both factors throughout the terminal/forwarding lifetime.
                    return (result.peer, interactive, true)
                }
                return (result.peer, authority, false)
            }
            return (try await session.authenticate(using: authority, timeout: timeout), authority, false)
        case .password:
            guard let passwordPrompter else { throw SSHAuthenticationError.unavailable }
            let authority = WorkspaceNativeSSHPasswordAuthentication(broker: broker, prompter: passwordPrompter,
                workspaceID: workspaceID, revision: revision, routeID: route.id, transportID: session.transportID,
                endpoint: endpoint, transitForRouteID: transitForRouteID, allowedJumpRouteIDs: route.jumpRoutes)
            let peer = try await session.authenticate(usingPassword: authority, timeout: timeout)
            try await authority.authenticationCompleted()
            return (peer, authority, true)
        case .keyboardInteractive:
            guard let keyboardInteractivePrompter else { throw SSHAuthenticationError.unavailable }
            let authority = WorkspaceNativeSSHInteractiveAuthentication(broker: broker, prompter: keyboardInteractivePrompter,
                workspaceID: workspaceID, revision: revision, routeID: route.id, transportID: session.transportID,
                endpoint: endpoint, transitForRouteID: transitForRouteID, allowedJumpRouteIDs: route.jumpRoutes)
            let peer = try await session.authenticate(usingInteractive: authority, timeout: timeout)
            try await authority.authenticationCompleted()
            return (peer, authority, true)
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

    /// A hint that the device's network took a new route: an open terminal
    /// probes its server now and is reported lost if one interval passes
    /// unanswered. A connection that survived the change answers and stays.
    public func networkRouteChanged() async {
        guard !closed, attached != nil else { return }
        await transport.networkRouteChanged()
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
    private let authority: any WorkspaceNativeSSHAuthority
    private var driver: NativeSSHDriver?
    private var networkLease: WorkspaceNetworkLease?
    private var closed = false
    private var shutdown: Task<Void, Never>?
    private let liveness: WorkspaceSSHLiveness
    private var probing = true, lastReceived: UInt64 = 0, silentIntervals = 0
    private var nextLivenessProbe: ContinuousClock.Instant?
    /// The device took a new network route since the last liveness check.
    private var routeChanged = false
    /// When a keepalive last left. libssh2 sends at most one per 2 s, by its
    /// own whole-second clock; one it holds back comes back deferred.
    private var lastProbe: ContinuousClock.Instant?
    private static let probeSpacing: Duration = .seconds(2)
    private var mux: MuxAttachment?

    /// One attachment to the machine's mux over this SSH command channel.
    /// Only the transport actor touches it.
    private final class MuxAttachment {
        let client: any TerminalMuxClient
        let control: TerminalMuxControl
        let queries: TerminalMuxQueryDriver
        /// The phone terminal's size as the screen last asked for it.
        var geometry: TerminalSessionGeometry
        /// One event plus one coalesced pixel-only resize. Do not read more
        /// network data or pull another event until the consumer drains it.
        var output: TerminalMuxOutputBuffer
        var panes: [TerminalMuxPane] = []
        var listed = false, ready = false, flushing = false
        var shown: Int?
        /// The shown pane is sized to the phone and drawing.
        var showing = false
        var outgoing = Data()
        /// Panes left at the phone's size on this machine, across connections.
        let sizes: TerminalMuxPhoneSizes
        /// The pane shown on this machine, across connections.
        let shownPane: TerminalMuxShownPane
        /// The server's run, when it says (current servers do, first).
        var incarnation: Data?
        /// Which panes' programs have asked for the mouse.
        var grabs: [Int: Bool] = [:]
        /// When to ask for the pane list again. The server pushes changes only
        /// for panes this connection has shown, so the titles and directories
        /// of the others come from the list alone.
        var nextListing = ContinuousClock.now + MuxAttachment.listingInterval(visible: false)
        /// Often while the machine is on screen (its pane strip), rarely while
        /// it is not (the fleet overview).
        static func listingInterval(visible: Bool) -> Duration { visible ? .seconds(5) : .seconds(30) }
        init(client: any TerminalMuxClient, control: TerminalMuxControl, geometry: TerminalSessionGeometry,
             sizes: TerminalMuxPhoneSizes, shownPane: TerminalMuxShownPane) {
            self.client = client; self.control = control; self.geometry = geometry; self.sizes = sizes
            self.shownPane = shownPane
            queries = TerminalMuxQueryDriver(client: client, control: control)
            output = TerminalMuxOutputBuffer(geometry: geometry)
        }

        /// A size event names only a numeric pane. Persist its current server
        /// and topology witness here, where the authoritative list is owned.
        func rememberPhoneSize(pane: Int, desktop: TerminalMuxSize?, rows: Int, columns: Int) {
            // A nil desktop was emitted before acknowledgement by older
            // native artifacts. It grants no authority to forget recovery.
            guard let desktop, let listed = panes.first(where: { $0.paneId == pane }) else { return }
            sizes.set(pane: listed, incarnation: incarnation, desktop: desktop, rows: rows, columns: columns)
        }
    }

    init(session: NativeSSHSession, identity: TerminalConnectionIdentity, broker: SSHAuthenticationBroker,
         foreground: SSHForegroundLease, networkLease: WorkspaceNetworkLease,
         authority: any WorkspaceNativeSSHAuthority, liveness: WorkspaceSSHLiveness,
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
                pixelWidth: geometry.pixelWidth, pixelHeight: geometry.pixelHeight),
                sizes: TerminalMuxPhoneSizes(workspace: identity.workspaceID, machine: identity.machineID),
                shownPane: TerminalMuxShownPane(workspace: identity.workspaceID, machine: identity.machineID))
            mux = state
            // The server's protocol check and its pane list.
            let deadline = ContinuousClock.now + .seconds(15)
            while !state.listed {
                guard ContinuousClock.now < deadline else { throw TerminalMuxError.unavailable }
                let drained = try drainMux()
                if state.listed { break }
                if !drained {
                    await Task.yield()
                    try await validateForeground()
                    continue
                }
                _ = try await pumpMux(timeout: .seconds(5), driver: driver)
                try await validateForeground()
            }
            // Back to the pane this phone was showing when it is still there.
            let (chosen, lost, restarted, unconfirmed) = state.shownPane.choose(
                from: state.panes, incarnation: state.incarnation)
            guard let first = chosen else { throw TerminalMuxError.noPanes }
            if restarted {
                // Even a pane under the same numbers may be a new shell now.
                control.recordNotice("FrankenTerm restarted on this machine while you were away, so the pane "
                    + "you were viewing may be gone. This is the desktop's active pane.")
            } else if lost {
                control.recordNotice("The pane you were viewing closed while you were away. This is the desktop's active pane.")
            } else if unconfirmed {
                control.recordInputUnconfirmed()
            }
            // Restore only the same pane on the same identified server run,
            // still at the phone's size. Reused IDs after a restart, topology
            // changes and legacy records cannot resize a replacement shell.
            // The pane about to be shown keeps its desktop size for later.
            if control.accessMode.controlsSize {
                for (pane, record) in state.sizes.records(matching: state.panes, incarnation: state.incarnation)
                    .sorted(by: { $0.key < $1.key }) {
                    guard try client.adoptPhoneSize(pane: pane, desktop: record.desktop,
                                                    rows: record.rows, columns: record.columns) else { continue }
                    if pane != first.paneId { try client.restoreSize(pane: pane) }
                }
            }
            // Adoption and queued restores do not prove the server applied
            // them. Keep each record until its explicit confirmed event.
            state.shown = first.paneId; state.shownPane.set(first, incarnation: state.incarnation)
            try control.inputRouting.select(pane: first.paneId)
            control.publish(panes: state.panes, shown: first.paneId)
            let signal = inputSignal
            control.setWaker { signal.notify() }
            try reconcileMux()
            try await flushMux(driver: driver)
        } catch { await close(); throw error }
    }

    /// One turn of the mux's SSH channel: send what the client has queued,
    /// read what arrived and apply its events.
    private func pumpMux(timeout: Duration, driver: NativeSSHDriver) async throws -> NativeSSHServiceEvent {
        guard let state = mux else { throw NativeSSHError.closed }
        try await flushMux(driver: driver)
        let event = try await session.service(timeout: state.queries.waitTimeout(maximum: timeout), driver: driver)
        // Even after close: it may be the answer close waits for.
        if !event.output.isEmpty { try state.client.receive(event.output) }
        guard !closed else { throw NativeSSHError.closed }
        if event.endOfOutput {
            try state.client.channelClosed()
            throw state.ready ? TerminalTransportError.connectionLost : TerminalMuxError.unavailable
        }
        _ = try drainMux()
        return event
    }

    /// Admits queued protocol bytes to the channel's single native slot.
    /// One turn admits at most four native slots. True proves that both local
    /// and client egress are empty; a busy/blocked/yielded turn returns false.
    @discardableResult
    private func flushMux(driver: NativeSSHDriver) async throws -> Bool {
        guard let state = mux, !state.flushing else { return false }
        state.flushing = true
        defer { state.flushing = false }
        for _ in 0..<4 {
            if state.outgoing.isEmpty {
                state.outgoing = try state.client.outgoing(limit: NativeSSHSession.maximumWriteBytes)
                guard state.outgoing.count <= NativeSSHSession.maximumWriteBytes else { throw TerminalInputError.capacity }
            }
            guard !state.outgoing.isEmpty else { return true }
            let chunk = Data(state.outgoing.prefix(NativeSSHSession.maximumWriteBytes))
            switch try await session.admit(chunk, driver: driver) {
            case .accepted(let count):
                guard count > 0, count <= chunk.count else { throw TerminalInputError.invalidWriteSize }
                state.outgoing.removeFirst(count)
            case .wouldBlock: return false
            }
        }
        inputSignal.notify()
        return false
    }

    /// True only after observing an empty native event queue. A model backlog
    /// or 64-event control burst yields instead, without another socket read.
    /// Output remains in the native queue until its bounded Swift slot is free.
    private func drainMux() throws -> Bool {
        guard let state = mux else { throw NativeSSHError.closed }
        guard state.output.isEmpty else { return false }
        for _ in 0..<64 {
            guard let event = try state.client.nextEvent() else { return true }
            switch event {
            case .ready:
                state.ready = true
            case .incarnation(let incarnation):
                guard incarnation.count == 16,
                      incarnation.contains(where: { $0 != 0 }),
                      state.incarnation == nil || state.incarnation == incarnation else {
                    throw TerminalMuxError.failed("The mux server identity changed. Reconnect before using its panes.")
                }
                state.incarnation = incarnation
            case .panes(let panes):
                try state.control.inputRouting.replaceInventory(panes.map(\.paneId))
                let present = Set(panes.map(\.paneId))
                state.grabs = state.grabs.filter { present.contains($0.key) }
                state.panes = panes; state.listed = true
                if let shown = state.shown {
                    // The record follows a pane the desktop moved; one that left clears it.
                    let listed = panes.first { $0.paneId == shown }
                    if listed == nil { state.shown = nil; state.showing = false }
                    state.shownPane.set(listed, incarnation: state.incarnation)
                }
                state.control.publish(panes: panes, shown: state.shown)
            case .output(let pane, let rows, let columns, let bytes):
                guard pane == state.shown, state.showing else { continue }
                // The phone draws its own size (at most maximumShownCells), and
                // a size someone else gave the pane while 256 history rows
                // still fit beside both screens. A pane someone else made
                // larger is asked back to the phone's size.
                let own = rows == Int(state.geometry.rows) && columns == Int(state.geometry.columns)
                guard (1...512).contains(rows), (1...512).contains(columns),
                      rows * columns <= TerminalSessionGeometry.maximumShownCells,
                      !state.control.accessMode.controlsSize || own || (rows * 2 + 256) * columns <= 65536 else {
                    guard state.control.accessMode.controlsSize else {
                        let reason = "This shared pane is too large to view on this phone. Its size was not changed."
                        state.control.recordFailure(reason)
                        throw TerminalMuxError.failed(reason)
                    }
                    // The mirror counts this output as drawn; draw the pane
                    // again from scratch once it is back at the phone's size.
                    try state.client.resize(pane: pane, rows: Int(state.geometry.rows),
                                            columns: Int(state.geometry.columns))
                    try state.client.show(pane: pane)
                    continue
                }
                // The cells the screen asked for come back with exactly the
                // pixels it asked for: the resize driver completes only on an
                // exact match, which whole-pixel cell arithmetic rarely gives.
                let size: TerminalSessionGeometry
                if UInt32(rows) == state.geometry.rows, UInt32(columns) == state.geometry.columns {
                    size = state.geometry
                } else {
                    let cellWidth = state.geometry.pixelWidth / max(state.geometry.columns, 1)
                    let cellHeight = state.geometry.pixelHeight / max(state.geometry.rows, 1)
                    size = try TerminalSessionGeometry(columns: UInt32(columns), rows: UInt32(rows),
                        pixelWidth: min(cellWidth * UInt32(columns), 65535), pixelHeight: min(cellHeight * UInt32(rows), 65535))
                }
                if size.rows != state.output.queuedGeometry.rows || size.columns != state.output.queuedGeometry.columns {
                    state.control.inputRouting.discardPointers()
                }
                try state.output.append(bytes, geometry: size,
                    destination: state.control.inputRouting.capture())
                return false
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
                state.sizes.remove(pane: pane)
                state.grabs[pane] = nil
                state.panes.removeAll { $0.paneId == pane }
                state.control.remove(pane: pane)
                if state.shown == pane {
                    state.shown = nil; state.showing = false; state.shownPane.set(nil, incarnation: state.incarnation)
                }
            case .searchResults(let pane, let results):
                state.queries.receive(pane: pane, results: results)
            case .context(let pane, let context):
                state.queries.receive(pane: pane, context: context)
            case .phoneSize(let pane, let desktop, let rows, let columns):
                state.rememberPhoneSize(pane: pane, desktop: desktop, rows: rows, columns: columns)
            case .phoneSizeRestored(let pane):
                state.sizes.remove(pane: pane)
            case .mouseGrab(let pane, let grabbed):
                guard state.panes.contains(where: { $0.paneId == pane }) else { continue }
                state.grabs[pane] = grabbed ? true : nil
                state.control.setMouseGrabbed(state.shown.flatMap { state.grabs[$0] } ?? false)
            case .activity(let pane):
                state.control.noteActivity(pane: pane)
            case .failed(let reason):
                // Once attached, a failure still ends this connection (and may
                // be reconnected), but its reason reaches the screen.
                if state.ready { state.control.recordFailure(reason) }
                throw state.ready ? TerminalTransportError.connectionLost : TerminalMuxError.failed(reason)
            }
        }
        return false
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
                try state.client.hide(pane: old)
                if state.control.accessMode.controlsSize { try state.client.restoreSize(pane: old) }
            }
            state.shown = requested; state.showing = false
            state.shownPane.set(state.panes.first { $0.paneId == requested }, incarnation: state.incarnation)
            try state.control.inputRouting.select(pane: requested)
            // Finish already-decoded output events before the replacement.
            // Dropping a chunk's suffix could strand the canonical parser in
            // UTF-8 or an escape sequence. Old visit markers cannot enable the
            // new pane, and its retired view no longer accepts these frames.
            state.control.publish(shown: requested)
        }
        // A vanished selection stays vanished. Neither old queued input nor a
        // newly created responder may silently inherit an unrelated pane.
        // One bounded scheduler owns all accepted remote lookup IDs, including
        // superseded and timed-out work. Terminal input keeps its separate lane.
        state.queries.step()
        guard let pane = state.shown else {
            // No pane on screen: nothing has the mouse, and taps meant for a
            // pane that left are not replayed on the next one.
            state.control.inputRouting.discardPointers()
            state.control.setMouseGrabbed(false)
            return
        }
        let visible = state.control.current.visible
        if visible, !state.showing {
            if state.control.accessMode.controlsSize {
                try state.client.resize(pane: pane, rows: Int(state.geometry.rows), columns: Int(state.geometry.columns))
            }
            try state.client.show(pane: pane)
            state.showing = true
            // Back on screen: its pane strip may be a hidden interval old.
            state.nextListing = .now
        } else if !visible, state.showing {
            try state.client.hide(pane: pane)
            if state.control.accessMode.controlsSize { try state.client.restoreSize(pane: pane) }
            state.showing = false
        }
        state.control.setMouseGrabbed(state.grabs[pane] ?? false)
    }

    /// One gesture per service turn, only after earlier protocol egress drains.
    /// No await separates taking a gesture, checking its exact pane visit and
    /// encoding it. Superseded/hidden gestures never move to the shown pane.
    private func dispatchMuxPointer(driver: NativeSSHDriver) async throws {
        guard let state = mux, state.control.inputRouting.hasPendingPointers else { return }
        guard state.control.accessMode.permitsInput, state.showing, state.control.current.visible else {
            state.control.inputRouting.discardPointers(); return
        }
        guard try await flushMux(driver: driver) else { return }
        try await validateForeground()
        guard mux === state, state.showing, state.control.current.visible else {
            state.control.inputRouting.discardPointers(); return
        }
        // Authorization awaited another actor. A resize/write may have queued
        // bytes meanwhile; recheck without suspending before appending an RPC.
        guard !state.flushing, state.outgoing.isEmpty else { return }
        state.outgoing = try state.client.outgoing(limit: NativeSSHSession.maximumWriteBytes)
        guard state.outgoing.count <= NativeSSHSession.maximumWriteBytes else { throw TerminalInputError.capacity }
        guard state.outgoing.isEmpty else { inputSignal.notify(); return }
        guard let event = state.control.inputRouting.takePointer() else { return }
        defer { if state.control.inputRouting.hasPendingPointers { inputSignal.notify() } }
        guard event.destination.paneID == state.shown else { return }
        try state.control.inputRouting.validateAdmission(event.destination)
        let size = state.output.queuedGeometry
        switch event.action {
        case .click(let column, let row):
            guard state.grabs[event.destination.paneID] == true,
                  column < Int(size.columns), row < Int(size.rows) else { return }
            try state.client.click(pane: event.destination.paneID, column: column, row: row)
        case .wheel(let steps, let column, let row):
            guard column < Int(size.columns), row < Int(size.rows) else { return }
            try state.client.wheel(pane: event.destination.paneID, steps: steps, column: column, row: row)
        }
        // A failure may follow encoding; close owns the uncertain operation.
        // It is never returned to the queue or replayed after reconnect.
        try await flushMux(driver: driver)
    }

    /// The next bytes for the phone's terminal, at most one receive's worth.
    private func nextMuxUpdate() -> TerminalSessionUpdate? {
        mux?.output.next()
    }

    func didApplyOutput(_ update: TerminalSessionUpdate) async {
        guard !closed, let destination = update.muxDestination else { return }
        muxControl?.inputRouting.didApplyOutput(destination)
    }

    private func serviceMux(driver: NativeSSHDriver) async throws -> TerminalSessionUpdate {
        try reconcileMux()
        try await dispatchMuxPointer(driver: driver)
        try await flushMux(driver: driver)
        guard !closed else { throw NativeSSHError.closed }
        if let update = nextMuxUpdate() { return update }
        let drained = try drainMux()
        try reconcileMux()
        try await flushMux(driver: driver)
        guard !closed else { throw NativeSSHError.closed }
        if let update = nextMuxUpdate() { return update }
        // A bounded control-only turn yields to input/cancellation. Only an
        // observed-empty event queue permits the next receive/decode operation.
        guard drained else { return TerminalSessionUpdate(output: Data()) }
        let now = ContinuousClock.now
        if let state = mux, now >= state.nextListing {
            try state.client.refreshPanes()
            state.nextListing = now + MuxAttachment.listingInterval(visible: state.showing)
        }
        let untilListing = max(.milliseconds(1), now.duration(to: mux?.nextListing ?? now + serviceTimeout()))
        let event = try await pumpMux(timeout: min(serviceTimeout(), untilListing), driver: driver)
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
        guard muxControl?.accessMode.permitsInput == true else { throw TerminalInputError.unavailable }
        do {
            try await validateForeground()
            guard let driver, let state = mux else { throw NativeSSHError.closed }
            guard !bytes.isEmpty, bytes.count <= 65536 else { throw TerminalInputError.invalidWriteSize }
            // Recheck after authorization's awaits. No actor suspension occurs
            // between verifying the registration and encoding its numeric target.
            try state.control.inputRouting.validateDelivery(destination)
            // Do not append user data behind blocked protocol bytes in Rust's
            // outgoing queue. An old accepted operation stays in its coordinator
            // slot until both native/client egress have demonstrably drained.
            guard try await flushMux(driver: driver) else { return .wouldBlock }
            try await validateForeground()
            try state.control.inputRouting.validateDelivery(destination)
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
            let event = try await session.service(timeout: serviceTimeout(), driver: driver)
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
    private func serviceTimeout(now: ContinuousClock.Instant = .now) -> Duration {
        guard probing, let nextLivenessProbe else { return liveness.interval }
        return max(.nanoseconds(1), min(liveness.interval, now.duration(to: nextLivenessProbe)))
    }

    /// The device took a new network route (another interface, or the network
    /// back): a hint that this connection may be stranded on the old one, never
    /// proof either way. The next service turn probes at once.
    func networkRouteChanged() {
        guard !closed, probing else { return }
        routeChanged = true
        inputSignal.notify()
    }

    private func checkLiveness(_ event: NativeSSHServiceEvent, driver: NativeSSHDriver,
                               now: ContinuousClock.Instant = .now) async throws {
        guard probing else { return }
        // After a new route the server must answer a probe sent now within one
        // interval. Bytes counted so far may predate the change, so they only
        // make this the last probe. A last probe already out keeps its earlier
        // deadline: the hint never delays reporting a loss.
        if routeChanged {
            if event.accounting.receivedBytes != lastReceived || silentIntervals < liveness.missedReplies {
                // One sent sooner would never leave: wait out the gap instead
                // of waiting for its answer. No pending deadline is earlier,
                // since an interval is at least 2 s.
                if let lastProbe, now < lastProbe + Self.probeSpacing {
                    nextLivenessProbe = lastProbe + Self.probeSpacing
                    return
                }
                let (received, silent, deadline) = (lastReceived, silentIntervals, nextLivenessProbe)
                routeChanged = false
                lastReceived = event.accounting.receivedBytes; silentIntervals = liveness.missedReplies
                nextLivenessProbe = now + liveness.interval
                if try await probe(driver: driver, now: now) || !probing { return }
                // Nothing left (a write or packet was pending, or libssh2 held
                // it back), so no reply is due: the deadlines stand as they
                // were, and the next turn tries again after the spacing.
                (lastReceived, silentIntervals) = (received, silent)
                routeChanged = true
                nextLivenessProbe = min(deadline ?? now + Self.probeSpacing, now + Self.probeSpacing)
                return
            }
            routeChanged = false
        }
        if event.accounting.receivedBytes != lastReceived {
            lastReceived = event.accounting.receivedBytes; silentIntervals = 0
            nextLivenessProbe = now + liveness.interval
            return
        }
        // Query deadlines and local input wakes can shorten a service wait.
        // Count elapsed peer-silence intervals, NOT waitExpired notifications;
        // neither many short waits nor constant local wakes change the policy.
        guard let deadline = nextLivenessProbe else {
            nextLivenessProbe = now + liveness.interval
            return
        }
        guard now >= deadline else { return }
        nextLivenessProbe = now + liveness.interval
        silentIntervals += 1
        guard silentIntervals <= liveness.missedReplies else { throw TerminalTransportError.peerUnresponsive }
        try await probe(driver: driver, now: now)
    }

    /// True when the keepalive left.
    @discardableResult
    private func probe(driver: NativeSSHDriver, now: ContinuousClock.Instant) async throws -> Bool {
        switch try await session.keepalive(driver: driver) {
        case .sent: lastProbe = now; return true
        case .deferred: return false
        case .unavailable: probing = false; return false
        }
    }

    func requestResize(_ geometry: NativeSSHGeometry) async throws {
        do {
            try await validateForeground()
            guard let driver else { throw NativeSSHError.closed }
            if let state = mux {
                // The immutable observing connection never owns remote size.
                // Its renderer fits the received grid locally instead.
                guard state.control.accessMode.controlsSize else { return }
                // A mux pane has no PTY here: the server resizes it, and the
                // pane's next output arrives at the new size, which completes
                // this request through the ordered update path.
                let size = try TerminalSessionGeometry(columns: geometry.columns, rows: geometry.rows,
                    pixelWidth: geometry.pixelWidth, pixelHeight: geometry.pixelHeight)
                if size != state.geometry { state.control.inputRouting.discardPointers() }
                state.geometry = size
                if (size.rows, size.columns) == (state.output.queuedGeometry.rows, state.output.queuedGeometry.columns) {
                    // Same cells, new pixels: nothing for the server to redraw.
                    try state.output.requestPixelGeometry(size)
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
        mux?.queries.retire()
        mux?.output.retire()
        closed = true; inputSignal.finish()
        let session = self.session, lease = networkLease
        let task = Task<Void, Never> {
            if let state = self.mux, let driver = self.driver { await self.settleBeforeClose(state, driver: driver) }
            await session.close()
            withExtendedLifetime(lease) {}
        }
        shutdown = task
        await task.value
        networkLease = nil
        // Retained diagnostic owners must not retain decoded output, native
        // event payloads or pending protocol bytes after actual shutdown joins.
        mux = nil; driver = nil
    }

    /// Before the channel closes, what was typed reaches the pane and the
    /// desktop gets its size back: this pane's, and any an earlier hide gave
    /// back. Reads until the server has answered both, since a relay like
    /// `nc -U` drops what it has not forwarded when the channel closes. The
    /// pump may be in its own service turn (ours is then busy); it writes what
    /// we admit. Bounded: a dead server must not hold teardown. An unanswered
    /// restore keeps its record for the next connection to give back, and
    /// unanswered input is told now and marked for the next connection to tell.
    private func settleBeforeClose(_ state: MuxAttachment, driver: NativeSSHDriver) async {
        if let pane = state.shown, state.showing {
            try? state.client.hide(pane: pane)
            if state.control.accessMode.controlsSize { try? state.client.restoreSize(pane: pane) }
        }
        let typedTo = state.shownPane.record?.paneId
        func inputAnswered() -> Bool {
            guard let typedTo else { return true }
            return (try? state.client.unansweredInput(pane: typedTo)) == false
        }
        var failed = false
        // Nothing is on screen now; only sizes and the session's end matter.
        func settle() {
            while let event = try? state.client.nextEvent() {
                switch event {
                case .phoneSizeRestored(let pane), .paneGone(let pane):
                    state.sizes.remove(pane: pane)
                case .phoneSize(let pane, let desktop, let rows, let columns):
                    state.rememberPhoneSize(pane: pane, desktop: desktop, rows: rows, columns: columns)
                case .failed: failed = true
                default: break
                }
            }
        }
        let deadline = ContinuousClock.now + .seconds(1)
        while ContinuousClock.now < deadline {
            let drained = (try? await flushMux(driver: driver)) ?? false
            settle()
            if failed { break }
            if drained, (try? state.client.restoring()) == false, inputAnswered(),
               (try? await session.accounting().pendingWriteBytes) == 0 { break }
            do {
                let event = try await session.service(timeout: .milliseconds(100), driver: driver)
                if !event.output.isEmpty { try state.client.receive(event.output) }
                if event.endOfOutput { break }
            } catch NativeSSHError.busy { try? await Task.sleep(for: .milliseconds(10)) }
            catch { break }
        }
        settle()
        if !inputAnswered() { state.shownPane.markInputUnconfirmed(); state.control.recordInputUnconfirmed() }
        // `restoring() == false` can also follow rejection or disconnect.
        // Only the confirmed events above retire durable recovery records.
    }

    func validateForeground() async throws {
        try Task.checkCancellation()
        guard !closed else { throw NativeSSHError.closed }
        try await broker.validateForeground(foreground)
        try await authority.validateCurrent()
        try await session.validateForwardingPath()
        guard !closed else { throw NativeSSHError.closed }
        try Task.checkCancellation()
    }

    deinit {
        muxControl?.inputRouting.retire()
        muxControl?.endQueries()
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
    func validateCleanupAdmission() throws {
        guard routing.permitsInput else { throw TerminalInputError.unavailable }
    }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        try await transport.writeMux(bytes, destination: destination)
    }
}
