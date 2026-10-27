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
    struct Call: Equatable { let name: String, pane: Int }
    private let lock = NSLock()
    private var events: [TerminalMuxEvent]
    private var calls: [Call] = []
    private var sizes: [Int: (rows: Int, columns: Int)]
    private var nextQuery: UInt64 = 0
    let panes: [TerminalMuxPane]

    init(mode: String) {
        let dimensions = mode == "oversize" ? (100, 512) : mode == "invalid-grid" ? (-1, 80) : (100, 256)
        panes = (1...2).map { id in
            TerminalMuxPane(paneId: id, tabId: id, windowId: 0, tabTitle: "Desktop", title: "pane-\(id)",
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
    func receive(_ bytes: Data) throws {}
    func channelClosed() throws {}
    func outgoing(limit: Int) throws -> Data { Data() }
    func nextEvent() throws -> TerminalMuxEvent? { lock.withLock { events.isEmpty ? nil : events.removeFirst() } }
    func refreshPanes() throws { lock.withLock { calls.append(.init(name: "list", pane: 0)) } }
    func show(pane: Int) throws {
        lock.withLock {
            calls.append(.init(name: "show", pane: pane))
            let size = sizes[pane]!
            events.append(.mouseGrab(paneID: pane, grabbed: true))
            events.append(.output(paneID: pane, rows: size.rows, columns: size.columns,
                bytes: Data("pane-\(pane)\r\n".utf8)))
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
            events.append(.phoneSizeRestored(paneID: pane))
        }
    }
    func adoptPhoneSize(pane: Int, desktop: TerminalMuxSize, rows: Int, columns: Int) throws -> Bool {
        lock.withLock { calls.append(.init(name: "adopt", pane: pane)); return true }
    }
    func wheel(pane: Int, steps: Int, column: Int, row: Int) throws {
        lock.withLock { calls.append(.init(name: "wheel", pane: pane)) }
    }
    func click(pane: Int, column: Int, row: Int) throws {
        lock.withLock { calls.append(.init(name: "click", pane: pane)) }
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

@MainActor
final class MuxObservationTests: XCTestCase {
    private func waitUntil(_ predicate: @MainActor () async -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while !(await predicate()) {
            guard ContinuousClock.now < deadline else { throw ObservationFixtureError.timeout }
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

    func testLiveMuxViewOnlyAndInteractiveEffectBoundary() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let port = env["FT_MUX_OBSERVATION_PORT"].flatMap(UInt16.init),
              let host = env["FT_MUX_OBSERVATION_HOSTKEY"].flatMap({ Data(base64Encoded: $0) }),
              let mode = env["FT_MUX_OBSERVATION_MODE"] else {
            throw XCTSkip("Requires the independent mux observation SSH fixture")
        }
        let interactive = mode == "interactive", client = ObservationMuxClient(mode: mode)
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
            muxClientFactory: { client }, muxAccessMode: interactive ? .interactive : .viewOnly,
            requiresExistingMux: true)
        let recovery = TerminalMuxPhoneSizes(workspace: owner.identity.workspaceID, machine: "machine")
        let shown = TerminalMuxShownPane(workspace: owner.identity.workspaceID, machine: "machine")
        defer { recovery.removeAll(); shown.set(nil, incarnation: nil) }
        for pane in client.panes {
            recovery.set(pane: pane, incarnation: ObservationMuxClient.incarnation,
                desktop: .init(rows: 60, cols: 160, pixelWidth: 1600, pixelHeight: 1200, dpi: 96), rows: 100, columns: 256)
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
            if mode == "oversize" || mode == "invalid-grid" {
                _ = try await run.value
                XCTAssertNotNil(control.current.failure)
                XCTAssertTrue(client.effects.isEmpty, "An unsupported observed grid must never be asked back to phone size")
                let summary = await consumer.summary()
                XCTAssertEqual(summary.bytes, 0)
            } else {
                try await waitUntil { control.inputRouting.current.ready }
                let first = try control.inputRouting.capture()
                let firstSummary = await consumer.summary()
                XCTAssertEqual(firstSummary.geometry.columns, interactive ? 80 : 256)
                XCTAssertEqual(firstSummary.geometry.rows, interactive ? 24 : 100)
                let stream = try await coordinator.makeStream(sceneID: UUID())
                let focus = try await coordinator.changeFocus(stream, to: pump.source)
                if interactive {
                    _ = try await coordinator.submit(focus, encodedBytes: Data("typed".utf8))
                    _ = try await coordinator.paste(focus, text: "pasted", bracketed: true)
                    try control.click(column: 1, row: 1, destination: first)
                    try control.scroll(steps: -1, column: 1, row: 1, destination: first)
                    try await waitUntil {
                        client.recorded.filter { $0.name == "write" }.count == 2
                            && client.recorded.contains(.init(name: "click", pane: 1))
                            && client.recorded.contains(.init(name: "wheel", pane: 1))
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
                try await owner.requestResize(.init(columns: 54, rows: 40, pixelWidth: 540, pixelHeight: 800))
                control.setVisible(false)
                try await waitUntil { client.recorded.contains(.init(name: "hide", pane: 1)) }
                control.setVisible(true)
                try await waitUntil { client.recorded.filter { $0 == .init(name: "show", pane: 1) }.count == 2 }
                _ = try await coordinator.suspend(focus)
                control.request(pane: 2)
                try await waitUntil { control.current.shown == 2 && control.inputRouting.current.ready }
                _ = control.search(.init(text: "read only search", caseSensitive: false, regex: false), panes: [1, 2])
                try await waitUntil { control.current.search?.pending.isEmpty == true }
                XCTAssertEqual(client.recorded.filter { $0.name == "search" }.map(\.pane), [1, 2])
                if !interactive {
                    XCTAssertTrue(client.effects.isEmpty, "Viewing, rotation, selection and search must remain read only")
                    XCTAssertEqual(recovery.records(matching: client.panes, incarnation: ObservationMuxClient.incarnation).count, 2,
                        "An observer cannot take over or erase recovery from an earlier interactive connection")
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
            if interactive {
                XCTAssertTrue(client.effects.contains { $0.name == "adopt" })
                XCTAssertTrue(client.effects.contains { $0.name == "resize" })
                XCTAssertTrue(client.effects.contains { $0.name == "restore" })
                XCTAssertTrue(client.effects.contains { $0.name == "write" })
                XCTAssertTrue(client.effects.contains { $0.name == "click" })
                XCTAssertTrue(client.effects.contains { $0.name == "wheel" })
            } else { XCTAssertTrue(client.effects.isEmpty, "Observer close must not restore someone else's size") }
            let accounting = await coordinator.accounting()
            XCTAssertEqual(accounting.connections, 0); XCTAssertEqual(accounting.streams, 0)
            let offline = try activity.beginOfflineRestore()
            withExtendedLifetime(offline) {}
        } catch { await owner.close(); throw error }
    }
}
