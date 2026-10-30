import Foundation
import XCTest
import FrankenTermKit
@testable import FrankenTermSSH
import FrankenTermSSHIntegration

private enum ObservationFixtureError: Error { case timeout, unexpectedSource }

/// An effect oracle at the protocol boundary, not a replacement SSH transport.
/// The production workspace broker, libssh2 channel, pump, input coordinator and
/// teardown still run against an independent libssh server. This fixture does
/// not claim native Rust codec, Metal or physical-device qualification.
private final class ObservationMuxClient: TerminalMuxClient, @unchecked Sendable {
    static let incarnation = Data(repeating: 0x4a, count: 16)
    static let preparatoryBytes = Data("\u{1b}c\u{1b}[?25l\u{1b}[?2004h".utf8)
    static let completeBytes = Data(repeating: 0x78, count: 40_001)
    static let palette = Data((0..<263).flatMap { index in
        [UInt8(truncatingIfNeeded: index), UInt8(37), UInt8(89), UInt8(255)]
    })
    struct Call: Equatable {
        let name: String, pane: Int
        let column: Int?, row: Int?
        init(name: String, pane: Int, column: Int? = nil, row: Int? = nil) {
            self.name = name; self.pane = pane; self.column = column; self.row = row
        }
    }
    private let lock = NSLock()
    private var events: [TerminalMuxEvent]
    private var calls: [Call] = []
    private var sizes: [Int: (rows: Int, columns: Int)]
    private var nextQuery: UInt64 = 0
    private var inputTimer: ContinuousClock.Instant?
    private var timerStarted = false
    let panes: [TerminalMuxPane]
    let mode: String

    static func identity(pane: Int, registration: UInt8? = nil) throws -> TerminalMuxPaneIdentity {
        try .init(incarnation: incarnation, durablePaneID: Data(repeating: UInt8(pane), count: 16),
                  registration: Data(repeating: registration ?? UInt8(pane + 100), count: 16))
    }

    init(mode: String) {
        self.mode = mode
        let dimensions = mode == "oversize" || mode == "input-only-oversize"
            ? (100, 512) : mode == "invalid-grid" ? (-1, 80) : (100, 256)
        panes = (1...2).map { id in
            TerminalMuxPane(paneId: id, tabId: mode == "identity-moved" ? id + 10 : id,
                windowId: mode == "identity-moved" ? 4 : 0, tabTitle: "Desktop", title: "pane-\(id)",
                workspace: "default", workingDir: nil, size: .init(rows: 100, cols: 256),
                isActive: id == 1, isZoomed: false)
        }
        sizes = [1: dimensions, 2: dimensions]
        events = [.ready(serverVersion: "fixture"), .incarnation(Self.incarnation),
                  .panes(mode == "no-panes" ? [] : panes)]
    }
    var recorded: [Call] { lock.withLock { calls } }
    var effects: [Call] {
        recorded.filter { ["resize", "restore", "adopt", "write", "paste", "wheel", "click"].contains($0.name) }
    }
    var geometryEffects: [Call] { recorded.filter { ["resize", "restore", "adopt"].contains($0.name) } }
    func receive(_ bytes: Data) throws {}
    func channelClosed() throws {}
    func outgoing(limit: Int) throws -> Data { Data() }
    func pollInputTimer() throws -> Duration? {
        lock.withLock {
            guard let deadline = inputTimer else { return nil }
            let now = ContinuousClock.now
            guard now >= deadline else { return now.duration(to: deadline) }
            inputTimer = nil
            calls.append(.init(name: "input-timer", pane: 1))
            events.append(.activity(paneID: 1))
            return nil
        }
    }
    func nextEvent() throws -> TerminalMuxEvent? { lock.withLock { events.isEmpty ? nil : events.removeFirst() } }
    func refreshPanes() throws { lock.withLock { calls.append(.init(name: "list", pane: 0)) } }
    func inspectPaneIdentity(pane: Int) throws {
        let identity = mode == "identity-unavailable" ? nil
            : try Self.identity(pane: pane, registration: mode == "identity-replaced" ? 0xf0 : nil)
        lock.withLock {
            calls.append(.init(name: "identity", pane: pane))
            events.append(.paneIdentity(paneID: pane, identity: identity))
        }
    }
    func show(pane: Int) throws {
        lock.withLock {
            calls.append(.init(name: "show", pane: pane))
            if !timerStarted {
                timerStarted = true
                inputTimer = .now + .milliseconds(100)
            }
            let size = sizes[pane]!
            events.append(.mouseGrab(paneID: pane, grabbed: true))
            if mode == "preparing-output" {
                events.append(.preparingOutput(paneID: pane, rows: size.rows, columns: size.columns,
                    bytes: Self.preparatoryBytes))
                events.append(.palette(paneID: pane, rgba: Self.palette))
                return
            }
            events.append(.output(paneID: pane, rows: size.rows, columns: size.columns,
                bytes: Data("pane-\(pane)\r\n".utf8)))
        }
    }
    func releasePreparedOutput(pane: Int) {
        lock.withLock {
            let size = sizes[pane]!
            events.append(.output(paneID: pane, rows: size.rows, columns: size.columns,
                bytes: Self.completeBytes))
        }
    }
    func hide(pane: Int) throws { lock.withLock { calls.append(.init(name: "hide", pane: pane)) } }
    func write(pane: Int, bytes: Data) throws { lock.withLock { calls.append(.init(name: "write", pane: pane)) } }
    func paste(pane: Int, text: String) throws { lock.withLock { calls.append(.init(name: "paste", pane: pane)) } }
    func resize(pane: Int, rows: Int, columns: Int) throws {
        lock.withLock { calls.append(.init(name: "resize", pane: pane)); sizes[pane] = (rows, columns) }
    }
    func restoreSize(pane: Int) throws {
        lock.withLock {
            calls.append(.init(name: "restore", pane: pane))
            sizes[pane] = (100, 256)
            events.append(.phoneSizeRestored(paneID: pane))
            events.append(.output(paneID: pane, rows: 100, columns: 256,
                bytes: Data("restored-\(pane)\r\n".utf8)))
        }
    }
    func adoptPhoneSize(pane: Int, desktop: TerminalMuxSize, rows: Int, columns: Int) throws -> Bool {
        lock.withLock { calls.append(.init(name: "adopt", pane: pane)); return true }
    }
    func wheel(pane: Int, steps: Int, column: Int, row: Int) throws {
        lock.withLock { calls.append(.init(name: "wheel", pane: pane, column: column, row: row)) }
    }
    func click(pane: Int, column: Int, row: Int) throws {
        lock.withLock { calls.append(.init(name: "click", pane: pane, column: column, row: row)) }
    }
    func search(pane: Int, text: String, caseSensitive: Bool, regex: Bool) throws -> UInt64 {
        lock.withLock {
            nextQuery += 1; calls.append(.init(name: "search", pane: pane))
            events.append(.searchResults(paneID: pane,
                .init(search: nextQuery, matches: [], searchedRows: 50, error: nil)))
            return nextQuery
        }
    }
    func context(pane: Int, firstRow: Int64, count: Int) throws -> UInt64 {
        lock.withLock { nextQuery += 1; calls.append(.init(name: "context", pane: pane)); return nextQuery }
    }
}

private actor ObservationConsumer: TerminalSessionConsumer {
    let identity: TerminalConnectionIdentity
    private var prepared = false
    private var geometry: TerminalSessionGeometry
    private var received = Data()
    init(identity: TerminalConnectionIdentity) throws {
        self.identity = identity
        geometry = try .init(columns: 80, rows: 24)
    }
    func prepare(_ identity: TerminalConnectionIdentity) throws {
        guard identity == self.identity, !prepared else { throw ObservationFixtureError.unexpectedSource }
        prepared = true
    }
    func receive(_ update: TerminalSessionUpdate, source: TerminalSource) throws {
        guard prepared, source.identity == identity, received.count + update.output.count <= 65536 else {
            throw ObservationFixtureError.unexpectedSource
        }
        if let geometry = update.resized { self.geometry = geometry }
        received += update.output
    }
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) throws {
        guard prepared, source.identity == identity, outcomes.allSatisfy({ $0.receipt.source == source }) else {
            throw ObservationFixtureError.unexpectedSource
        }
    }
    func summary() -> (geometry: TerminalSessionGeometry, bytes: Int) { (geometry, received.count) }
}

/// Suspends only the final model-consumption callback. The transport must keep
/// input closed even after the native event and its earlier chunks were drained.
private actor PreparedOutputConsumer: TerminalSessionConsumer {
    let identity: TerminalConnectionIdentity
    nonisolated let completion = TerminalSessionSignal()
    private var prepared = false
    private var received = Data()
    private var palettes: [Data] = []
    private var order: [String] = []
    private var preparations: [Bool] = []
    private var awaitingCompletion = false

    init(identity: TerminalConnectionIdentity) { self.identity = identity }
    func prepare(_ identity: TerminalConnectionIdentity) throws {
        guard identity == self.identity, !prepared else { throw ObservationFixtureError.unexpectedSource }
        prepared = true
    }
    func receive(_ update: TerminalSessionUpdate, source: TerminalSource) async throws {
        guard prepared, source.identity == identity, received.count + update.output.count <= 65536 else {
            throw ObservationFixtureError.unexpectedSource
        }
        if update.muxDestination != nil {
            awaitingCompletion = true
            guard await completion.wait() else { throw CancellationError() }
            awaitingCompletion = false
        }
        preparations.append(update.muxPreparation)
        if let palette = update.muxPalette {
            palettes.append(palette); order.append("palette")
        }
        if !update.output.isEmpty {
            received += update.output; order.append("output")
        }
    }
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) throws {
        guard prepared, source.identity == identity, outcomes.allSatisfy({ $0.receipt.source == source }) else {
            throw ObservationFixtureError.unexpectedSource
        }
    }
    func summary() -> (bytes: Data, palettes: [Data], order: [String], preparations: [Bool], awaitingCompletion: Bool) {
        (received, palettes, order, preparations, awaitingCompletion)
    }
}

@MainActor
final class MuxObservationTests: XCTestCase {
    private func waitUntil(timeout: Duration = .seconds(5), file: StaticString = #filePath, line: UInt = #line,
                           _ predicate: @MainActor () async -> Bool) async throws {
        let deadline = ContinuousClock.now + timeout
        while !(await predicate()) {
            guard ContinuousClock.now < deadline else {
                XCTFail("The live mux fixture did not reach its expected state", file: file, line: line)
                throw ObservationFixtureError.timeout
            }
            try await Task.sleep(for: .milliseconds(5))
        }
    }

    func testExistingMuxRequirementRefusesMissingFactoryBeforeDialOrCredentialAccess() async throws {
        let fixture = try await SSHWorkspaceTestFixture.make(), activity = WorkspaceNetworkActivity()
        let factory: @Sendable () throws -> any TerminalMuxClient = { ObservationMuxClient(mode: "observe") }
        for disabled in [false, true] {
            let before = NativeSSHConnector.accounting(), vault = fixture.vault.counts
            do {
                let owner = try await WorkspaceNativeSSHConnection.connect(workspaceID: fixture.installed.id,
                    expectedRevision: fixture.installed.revision, machineID: "machine", routeID: "route",
                    credentialID: "key", confirmedByUser: true, store: fixture.store, broker: fixture.broker,
                    networkActivity: activity, connectionTimeout: .zero,
                    muxClientFactory: disabled ? factory : nil,
                    attachMux: !disabled, muxAccessMode: .viewOnly, requiresExistingMux: true)
                await owner.close(); XCTFail("An observation request bypassed its required mux")
            } catch { XCTAssertEqual(error as? TerminalMuxError, .unavailable) }
            XCTAssertEqual(fixture.vault.counts.reads, vault.reads)
            XCTAssertEqual(fixture.vault.counts.signatures, vault.signatures)
            XCTAssertEqual(NativeSSHConnector.accounting().resolvingHosts, before.resolvingHosts)
            XCTAssertEqual(NativeSSHConnector.accounting().connectingSockets, before.connectingSockets)
            XCTAssertEqual(activity.accounting().connections, 0)
        }
    }

    func testPreparingOutputAndPaletteCannotAdmitInputBeforeTheCompleteModelUpdate() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let port = env["FT_MUX_OBSERVATION_PORT"].flatMap(UInt16.init),
              let host = env["FT_MUX_OBSERVATION_HOSTKEY"].flatMap({ Data(base64Encoded: $0) }),
              env["FT_MUX_OBSERVATION_MODE"] == "preparing-output" else {
            throw XCTSkip("Requires the independent preparatory mux-output SSH fixture")
        }
        let client = ObservationMuxClient(mode: "preparing-output")
        let fixture = try await SSHWorkspaceTestFixture.make(hostname: "127.0.0.1", port: port,
            hostBlob: host, routeMode: "frankenterm_mux") { workspace in
                var routes = workspace["routes"] as! [[String: Any]]
                routes[0]["remote_command"] = "fixture-mux-observe"; workspace["routes"] = routes
            }
        let activity = WorkspaceNetworkActivity()
        let owner = try await WorkspaceNativeSSHConnection.connect(workspaceID: fixture.installed.id,
            expectedRevision: fixture.installed.revision, machineID: "machine", routeID: "route",
            credentialID: "key", confirmedByUser: true, store: fixture.store, broker: fixture.broker,
            networkActivity: activity, connectionTimeout: .seconds(5), authenticationTimeout: .seconds(5),
            muxClientFactory: { client }, muxAccessMode: .inputOnly, requiresExistingMux: true)
        let control = try XCTUnwrap(owner.muxControl)
        control.setVisible(true)
        let coordinator = try TerminalInputCoordinator()
        let consumer = PreparedOutputConsumer(identity: owner.identity)
        defer { consumer.completion.finish() }
        do {
            let pump = try await owner.openTerminal(geometry: .init(columns: 80, rows: 24), coordinator: coordinator) {
                try await consumer.prepare($0)
            }
            let run = Task { try await pump.run(consumer: consumer) }
            let watchdog = Task {
                try await Task.sleep(for: .seconds(8))
                consumer.completion.finish(); pump.requestStop()
            }
            defer { watchdog.cancel(); consumer.completion.finish(); pump.requestStop() }
            try await waitUntil {
                let snapshot = await consumer.summary()
                return snapshot.bytes == ObservationMuxClient.preparatoryBytes && snapshot.palettes.count == 1
            }
            let prepared = await consumer.summary()
            XCTAssertEqual(prepared.palettes, [ObservationMuxClient.palette])
            XCTAssertEqual(prepared.order, ["output", "palette"], "reset precedes the transient remote palette")
            XCTAssertEqual(prepared.preparations, [true, true])
            XCTAssertFalse(control.inputRouting.current.ready)
            let stream = try await coordinator.makeStream(sceneID: UUID())
            let focus = try await coordinator.changeFocus(stream, to: pump.source)
            do {
                _ = try await coordinator.submit(focus, encodedBytes: Data("too early".utf8))
                XCTFail("A reset or palette update admitted terminal input")
            } catch { XCTAssertEqual(error as? TerminalInputError, .stale) }
            XCTAssertTrue(client.effects.isEmpty)
            client.releasePreparedOutput(pane: 1)
            control.request(pane: 1) // Wake the existing service owner without changing the visit.
            try await waitUntil { await consumer.summary().awaitingCompletion }
            let waiting = await consumer.summary()
            XCTAssertEqual(waiting.bytes, ObservationMuxClient.preparatoryBytes + ObservationMuxClient.completeBytes.prefix(32_768))
            XCTAssertEqual(waiting.order, ["output", "palette", "output", "output"])
            XCTAssertEqual(waiting.preparations, [true, true, true, true])
            XCTAssertFalse(control.inputRouting.current.ready, "dequeueing the last chunk is not model consumption")
            XCTAssertTrue(client.effects.isEmpty)
            consumer.completion.notify()
            try await waitUntil { control.inputRouting.current.ready }
            let complete = await consumer.summary()
            XCTAssertEqual(complete.bytes, ObservationMuxClient.preparatoryBytes + ObservationMuxClient.completeBytes)
            XCTAssertEqual(complete.palettes, [ObservationMuxClient.palette])
            XCTAssertEqual(complete.order, ["output", "palette", "output", "output", "output"])
            XCTAssertEqual(complete.preparations, [true, true, true, true, false])
            _ = try await coordinator.submit(focus, encodedBytes: Data("after complete output".utf8))
            try await waitUntil { client.recorded.filter { $0.name == "write" }.count == 1 }
            XCTAssertEqual(client.effects, [.init(name: "write", pane: 1)])
            _ = try await coordinator.suspend(focus)
            pump.requestStop(); _ = try await run.value
            let pending = try await coordinator.pendingOutcomes(pump.source)
            if !pending.outcomes.isEmpty { try await coordinator.acknowledgeOutcomes(pending) }
            try await coordinator.retire(pump.source)
            await owner.close()
            XCTAssertEqual(activity.accounting().connections, 0)
            let accounting = await coordinator.accounting()
            XCTAssertEqual(accounting.connections, 0); XCTAssertEqual(accounting.streams, 0)
        } catch { consumer.completion.finish(); await owner.close(); throw error }
    }

    func testReconnectPreflightRequiresExactIdentityBeforeSelectionAndGeometry() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let port = env["FT_MUX_OBSERVATION_PORT"].flatMap(UInt16.init),
              let host = env["FT_MUX_OBSERVATION_HOSTKEY"].flatMap({ Data(base64Encoded: $0) }),
              let mode = env["FT_MUX_OBSERVATION_MODE"], mode.hasPrefix("identity-") else {
            throw XCTSkip("Requires the independent mux identity SSH fixture")
        }
        let client = ObservationMuxClient(mode: mode)
        let fixture = try await SSHWorkspaceTestFixture.make(hostname: "127.0.0.1", port: port,
            hostBlob: host, routeMode: "frankenterm_mux") { workspace in
                var routes = workspace["routes"] as! [[String: Any]]
                routes[0]["remote_command"] = "fixture-mux-observe"; workspace["routes"] = routes
            }
        let activity = WorkspaceNetworkActivity()
        let owner = try await WorkspaceNativeSSHConnection.connect(workspaceID: fixture.installed.id,
            expectedRevision: fixture.installed.revision, machineID: "machine", routeID: "route",
            credentialID: "key", confirmedByUser: true, store: fixture.store, broker: fixture.broker,
            networkActivity: activity, connectionTimeout: .seconds(5), authenticationTimeout: .seconds(5),
            muxClientFactory: { client }, muxAccessMode: .interactive, requiresExistingMux: true)
        let recovery = TerminalMuxPhoneSizes(workspace: owner.identity.workspaceID, machine: "machine")
        let shown = TerminalMuxShownPane(workspace: owner.identity.workspaceID, machine: "machine")
        defer { recovery.removeAll(); shown.set(nil, incarnation: nil) }
        let saved = TerminalMuxPane(paneId: 2, tabId: 2, windowId: 0, tabTitle: "Before moving", title: "pane-2",
            workspace: "default", workingDir: nil, size: .init(rows: 100, cols: 256), isActive: false, isZoomed: false)
        let witness = try ObservationMuxClient.identity(pane: 2)
        shown.set(saved, incarnation: ObservationMuxClient.incarnation, identity: witness)
        shown.markInputUnconfirmed()
        recovery.set(pane: saved, incarnation: ObservationMuxClient.incarnation,
            desktop: .init(rows: 60, cols: 160, pixelWidth: 1600, pixelHeight: 1200, dpi: 96),
            rows: 100, columns: 256, identity: witness)
        let control = try XCTUnwrap(owner.muxControl)
        control.setVisible(true)
        let coordinator = try TerminalInputCoordinator(), consumer = try ObservationConsumer(identity: owner.identity)
        do {
            let pump = try await owner.openTerminal(geometry: .init(columns: 80, rows: 24), coordinator: coordinator) {
                try await consumer.prepare($0)
            }
            let resumed = mode == "identity-same" || mode == "identity-moved"
            XCTAssertTrue(control.current.inputUnconfirmed,
                          "fresh identity cannot prove the previous connection's input outcome")
            if resumed {
                XCTAssertEqual(control.current.shown, 2, "resume the verified pane, not active pane 1")
                XCTAssertEqual(shown.record?.identity, witness)
                XCTAssertEqual(shown.record?.tabId, mode == "identity-moved" ? 12 : 2)
                XCTAssertTrue(client.recorded.contains(.init(name: "adopt", pane: 2)))
            } else {
                XCTAssertNil(control.current.shown)
                XCTAssertThrowsError(try control.inputRouting.capture())
                XCTAssertTrue(client.effects.isEmpty)
                XCTAssertFalse(client.recorded.contains { $0.name == "show" })
                XCTAssertTrue(control.current.notice?.contains("Select a pane") == true)
                XCTAssertEqual(shown.record?.identity, witness, "a failed resume preserves the original record")
            }
            let run = Task { try await pump.run(consumer: consumer) }
            let watchdog = Task { try await Task.sleep(for: .seconds(8)); pump.requestStop() }
            defer { watchdog.cancel(); pump.requestStop() }
            if !resumed { control.request(pane: 2) }
            try await waitUntil { control.current.shown == 2 && control.inputRouting.current.ready }
            XCTAssertFalse(client.recorded.contains(.init(name: "show", pane: 1)))
            if !resumed {
                XCTAssertFalse(client.recorded.contains { $0.name == "adopt" },
                               "explicit fresh selection cannot inherit a replacement's saved size")
            }
            let identityIndex = try XCTUnwrap(client.recorded.firstIndex { $0 == .init(name: "identity", pane: 2) })
            let firstEffect = try XCTUnwrap(client.recorded.firstIndex { ["adopt", "resize", "show"].contains($0.name) })
            XCTAssertLessThan(identityIndex, firstEffect)
            let stream = try await coordinator.makeStream(sceneID: UUID())
            let focus = try await coordinator.changeFocus(stream, to: pump.source)
            _ = try await coordinator.submit(focus, encodedBytes: Data("a deliberate new action".utf8))
            try await waitUntil { client.recorded.filter { $0.name == "write" }.count == 1 }
            XCTAssertEqual(client.recorded.filter { $0.name == "write" }.map(\.pane), [2])
            _ = try await coordinator.suspend(focus)
            pump.requestStop(); _ = try await run.value
            let pending = try await coordinator.pendingOutcomes(pump.source)
            if !pending.outcomes.isEmpty { try await coordinator.acknowledgeOutcomes(pending) }
            try await coordinator.retire(pump.source)
            await owner.close()
            XCTAssertEqual(activity.accounting().connections, 0)
        } catch { await owner.close(); throw error }
    }

    func testLiveMuxAccessModeEffectBoundary() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let port = env["FT_MUX_OBSERVATION_PORT"].flatMap(UInt16.init),
              let host = env["FT_MUX_OBSERVATION_HOSTKEY"].flatMap({ Data(base64Encoded: $0) }),
              let mode = env["FT_MUX_OBSERVATION_MODE"] else {
            throw XCTSkip("Requires the independent mux observation SSH fixture")
        }
        let controlsSize = mode == "interactive"
        let permitsInput = controlsSize || mode == "input-only" || mode == "input-only-oversize"
        let accessMode: TerminalMuxAccessMode = controlsSize ? .interactive : (permitsInput ? .inputOnly : .viewOnly)
        let client = ObservationMuxClient(mode: mode)
        let fixture = try await SSHWorkspaceTestFixture.make(hostname: "127.0.0.1", port: port,
            hostBlob: host, routeMode: "frankenterm_mux") { workspace in
                var routes = workspace["routes"] as! [[String: Any]]
                routes[0]["remote_command"] = "fixture-mux-observe"
                workspace["routes"] = routes
            }
        let activity = WorkspaceNetworkActivity()
        let owner = try await WorkspaceNativeSSHConnection.connect(workspaceID: fixture.installed.id,
            expectedRevision: fixture.installed.revision, machineID: "machine", routeID: "route",
            credentialID: "key", confirmedByUser: true, store: fixture.store, broker: fixture.broker,
            networkActivity: activity, connectionTimeout: .seconds(5), authenticationTimeout: .seconds(5),
            muxClientFactory: { client }, muxAccessMode: accessMode,
            requiresExistingMux: true)
        let recovery = TerminalMuxPhoneSizes(workspace: owner.identity.workspaceID, machine: "machine")
        let shown = TerminalMuxShownPane(workspace: owner.identity.workspaceID, machine: "machine")
        defer { recovery.removeAll(); shown.set(nil, incarnation: nil) }
        for pane in client.panes {
            recovery.set(pane: pane, incarnation: ObservationMuxClient.incarnation,
                desktop: .init(rows: 60, cols: 160, pixelWidth: 1600, pixelHeight: 1200, dpi: 96), rows: 100, columns: 256,
                identity: try ObservationMuxClient.identity(pane: pane.paneId))
        }
        let control = try XCTUnwrap(owner.muxControl)
        control.setVisible(true)
        let coordinator = try TerminalInputCoordinator(), consumer = try ObservationConsumer(identity: owner.identity)
        do {
            if mode == "no-panes" {
                do {
                    _ = try await owner.openTerminal(geometry: .init(columns: 80, rows: 24), coordinator: coordinator) {
                        try await consumer.prepare($0)
                    }
                    XCTFail("Missing existing panes manufactured a terminal")
                } catch { XCTAssertEqual(error as? TerminalMuxError, .noPanes) }
                XCTAssertTrue(client.effects.isEmpty)
                await owner.close()
                XCTAssertEqual(activity.accounting().connections, 0)
                return
            }
            let pump = try await owner.openTerminal(geometry: .init(columns: 80, rows: 24), coordinator: coordinator) {
                try await consumer.prepare($0)
            }
            let run = Task { try await pump.run(consumer: consumer) }
            let watchdog = Task { try await Task.sleep(for: .seconds(8)); pump.requestStop() }
            defer { watchdog.cancel(); pump.requestStop() }
            if mode == "oversize" || mode == "input-only-oversize" || mode == "invalid-grid" {
                _ = try await run.value
                XCTAssertNotNil(control.current.failure)
                XCTAssertEqual(control.current.paneTooLargeToView, mode != "invalid-grid",
                    "Only a real pane bigger than the phone may be taken at the phone's size")
                XCTAssertTrue(client.effects.isEmpty, "An unsupported observed grid must never be asked back to phone size")
                let summary = await consumer.summary()
                XCTAssertEqual(summary.bytes, 0)
            } else {
                try await waitUntil { control.inputRouting.current.ready }
                // No network data or UI action wakes this fixture timer. It
                // must shorten the existing transport wait (ordinary pane
                // polling takes five seconds) and run exactly once.
                try await waitUntil(timeout: .seconds(1)) {
                    client.recorded.filter { $0.name == "input-timer" }.count == 1
                }
                let first = try control.inputRouting.capture()
                let firstSummary = await consumer.summary()
                XCTAssertEqual(firstSummary.geometry.columns, controlsSize ? 80 : 256)
                XCTAssertEqual(firstSummary.geometry.rows, controlsSize ? 24 : 100)
                let stream = try await coordinator.makeStream(sceneID: UUID())
                let focus = try await coordinator.changeFocus(stream, to: pump.source)
                if permitsInput {
                    _ = try await coordinator.submit(focus, encodedBytes: Data("typed".utf8))
                    _ = try await coordinator.paste(focus, text: "pasted", bracketed: true)
                    // Shared-size input uses the remote grid, including cells
                    // beyond the phone's provisional 80 x 24 geometry.
                    let column = controlsSize ? 79 : 255, row = controlsSize ? 23 : 99
                    try control.click(column: column, row: row, destination: first)
                    try control.scroll(steps: -1, column: column, row: row, destination: first)
                    try await waitUntil {
                        client.recorded.filter { $0.name == "write" }.count == 2
                            && client.recorded.contains(.init(name: "click", pane: 1, column: column, row: row))
                            && client.recorded.contains(.init(name: "wheel", pane: 1, column: column, row: row))
                    }
                } else {
                    do { _ = try await coordinator.submit(focus, encodedBytes: Data("typed".utf8)); XCTFail("Observer typed") }
                    catch { XCTAssertEqual(error as? TerminalInputError, .unavailable) }
                    do { _ = try await coordinator.paste(focus, text: "pasted", bracketed: true); XCTFail("Observer pasted") }
                    catch { XCTAssertEqual(error as? TerminalInputError, .unavailable) }
                    do { _ = try await coordinator.cleanup(focus, encodedBytes: Data([0x1b, 0x5b, 0x4f])); XCTFail("Observer queued cleanup") }
                    catch { XCTAssertEqual(error as? TerminalInputError, .unavailable) }
                    XCTAssertThrowsError(try control.click(column: 1, row: 1, destination: first))
                    XCTAssertThrowsError(try control.scroll(steps: -1, column: 1, row: 1, destination: first))
                    XCTAssertFalse(control.inputRouting.hasPendingPointers)
                    let accounting = await coordinator.accounting()
                    XCTAssertEqual(accounting.retainedBytes, 0); XCTAssertEqual(accounting.retainedRecords, 0)
                    XCTAssertEqual(accounting.cleanupRecords, 0); XCTAssertEqual(accounting.cleanupReservationBytes, 0)
                }
                let reply = try await coordinator.reply(pump.source, encodedBytes: Data("answerback".utf8))
                XCTAssertEqual(reply, .droppedWhole)
                if controlsSize {
                    let effects = client.geometryEffects.count
                    let shows = client.recorded.filter { $0.name == "show" }.count
                    control.setSizeControlEnabled(false)
                    try await waitUntil {
                        let summary = await consumer.summary()
                        return client.geometryEffects.count == effects + 1 && summary.geometry.columns == 256
                    }
                    XCTAssertEqual(client.geometryEffects.last?.name, "restore")
                    XCTAssertEqual(client.recorded.filter { $0.name == "show" }.count, shows,
                        "a companion relinquishes size without resubscribing or losing its output visit")
                    XCTAssertTrue(control.current.visible)
                    XCTAssertEqual(control.inputRouting.current.destination, first)
                    try await owner.requestResize(.init(columns: 54, rows: 40))
                    XCTAssertEqual(client.geometryEffects.count, effects + 1,
                        "a late viewport cannot resize the unfocused companion")
                    control.setVisible(false)
                    try await waitUntil { client.recorded.contains(.init(name: "hide", pane: 1)) }
                    control.setVisible(true)
                    try await waitUntil { client.recorded.filter { $0.name == "show" }.count == shows + 1 }
                    XCTAssertEqual(client.geometryEffects.count, effects + 1,
                        "a narrow window restoring its companion does not reacquire size")
                    control.setSizeControlEnabled(true)
                    try await waitUntil { client.geometryEffects.count == effects + 2 }
                    XCTAssertEqual(client.geometryEffects.last?.name, "resize")
                }
                try await owner.requestResize(.init(columns: 54, rows: 40, pixelWidth: 540, pixelHeight: 800))
                let shownBeforeHide = client.recorded.filter { $0 == .init(name: "show", pane: 1) }.count
                let hiddenBeforeHide = client.recorded.filter { $0 == .init(name: "hide", pane: 1) }.count
                control.setVisible(false)
                try await waitUntil { client.recorded.filter { $0 == .init(name: "hide", pane: 1) }.count > hiddenBeforeHide }
                control.setVisible(true)
                try await waitUntil {
                    let summary = await consumer.summary()
                    return client.recorded.filter { $0 == .init(name: "show", pane: 1) }.count >= shownBeforeHide + 1
                        && control.inputRouting.current.ready
                        && (!controlsSize || (summary.geometry.columns == 54 && summary.geometry.rows == 40))
                }
                // Restoring size during hide can leave one oversized frame
                // queued. The interactive owner may request one complete
                // redraw at its current geometry before publishing it.
                XCTAssertLessThanOrEqual(client.recorded.filter { $0 == .init(name: "show", pane: 1) }.count,
                    shownBeforeHide + (controlsSize ? 2 : 1))
                _ = try await coordinator.suspend(focus)
                control.request(pane: 2)
                try await waitUntil { control.current.shown == 2 && control.inputRouting.current.ready }
                _ = control.search(.init(text: "read only search", caseSensitive: false, regex: false), panes: [1, 2])
                try await waitUntil { control.current.search?.pending.isEmpty == true }
                XCTAssertEqual(client.recorded.filter { $0.name == "search" }.map(\.pane), [1, 2])
                if !controlsSize {
                    XCTAssertTrue(client.geometryEffects.isEmpty, "Input permission cannot resize, adopt or restore a shared pane")
                    let summary = await consumer.summary()
                    XCTAssertEqual(summary.geometry.columns, 256)
                    XCTAssertEqual(summary.geometry.rows, 100)
                    let identities = try Dictionary(uniqueKeysWithValues: client.panes.map {
                        ($0.paneId, try ObservationMuxClient.identity(pane: $0.paneId))
                    })
                    XCTAssertEqual(recovery.records(matching: client.panes, incarnation: ObservationMuxClient.incarnation,
                                                    identities: identities).count, 2,
                        "A client without size authority cannot erase another connection's recovery")
                }
                if !permitsInput {
                    XCTAssertTrue(client.effects.isEmpty, "Viewing, rotation, selection and search must remain read only")
                    XCTAssertTrue(control.inputRouting.current.ready, "Denied cleanup must not close the observing pump")
                }
                pump.requestStop()
                _ = try await run.value
            }
            let pending = try await coordinator.pendingOutcomes(pump.source)
            if !pending.outcomes.isEmpty { try await coordinator.acknowledgeOutcomes(pending) }
            try await coordinator.retire(pump.source)
            await owner.close()
            XCTAssertEqual(activity.accounting().connections, 0)
            if controlsSize {
                XCTAssertTrue(client.effects.contains { $0.name == "adopt" })
                XCTAssertTrue(client.effects.contains { $0.name == "resize" })
                XCTAssertTrue(client.effects.contains { $0.name == "restore" })
            } else {
                XCTAssertTrue(client.geometryEffects.isEmpty, "Closing cannot restore someone else's size")
            }
            if permitsInput, mode != "input-only-oversize" {
                XCTAssertTrue(client.effects.contains { $0.name == "write" })
                XCTAssertTrue(client.effects.contains { $0.name == "click" })
                XCTAssertTrue(client.effects.contains { $0.name == "wheel" })
            } else { XCTAssertTrue(client.effects.isEmpty, "An observer or unsupported grid cannot cause input or geometry effects") }
            let accounting = await coordinator.accounting()
            XCTAssertEqual(accounting.connections, 0); XCTAssertEqual(accounting.streams, 0)
            let offline = try activity.beginOfflineRestore()
            withExtendedLifetime(offline) {}
        } catch {
            XCTFail("Mux effect sequence at failure: \(client.recorded)")
            await owner.close(); throw error
        }
    }
}
