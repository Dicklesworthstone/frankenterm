import Foundation
import XCTest
@testable import FrankenTermKit
@testable import FrankenTermSSH
@testable import FrankenTermSSHIntegration

private enum MultipleTerminalTestError: Error { case unexpected, timeout }

/// Pause a real signature without substituting identity or signature bytes.
private actor MultipleTerminalSigner: WorkspaceSSHSigning {
    let base: SSHWorkspaceTestVault
    private var pending: (UUID, CheckedContinuation<Void, Error>)?
    private var hasPaused = false
    var isPaused: Bool { pending != nil }
    init(_ base: SSHWorkspaceTestVault) { self.base = base }
    func publicIdentity(requestID: UUID, authorization: WorkspaceSSHAuthorization) async throws -> Data {
        try await base.publicIdentity(requestID: requestID, authorization: authorization)
    }
    func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
              challenge: SSHUserAuthenticationChallenge) async throws -> Data {
        if !hasPaused {
            hasPaused = true
            try await withCheckedThrowingContinuation { pending = (requestID, $0) }
        }
        try Task.checkCancellation()
        return try await base.sign(requestID: requestID, authorization: authorization, challenge: challenge)
    }
    nonisolated func cancel(requestID: UUID) { Task { await cancelPending(requestID) } }
    private func cancelPending(_ id: UUID) {
        guard let saved = pending, saved.0 == id else { return }
        pending = nil; saved.1.resume(throwing: CancellationError())
    }
    func release() { let saved = pending; pending = nil; saved?.1.resume() }
}

private final class MultipleTerminalPersistence: WorkspacePersistence, @unchecked Sendable {
    private let base = SSHWorkspaceTestPersistence(), lock = NSLock()
    private var count = 0
    var accesses: Int { lock.withLock { count } }
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T {
        lock.withLock { count += 1 }; return try base.withExclusiveAccess(body)
    }
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? { try base.read(slot, limit: limit) }
    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws { try base.write(data, to: slot) }
    func removeJournal() throws { try base.removeJournal() }
}

private actor MultipleTerminalConsumer: TerminalSessionConsumer {
    let source: TerminalSource
    private let connection: WorkspaceSSHConnection
    private(set) var output = Data()
    private(set) var outcomes: [TerminalInputOutcome] = []
    init(source: TerminalSource, connection: WorkspaceSSHConnection) {
        self.source = source; self.connection = connection
    }
    func receive(_ update: TerminalSessionUpdate, source: TerminalSource) throws {
        guard source == self.source, output.count + update.output.count <= 8192 else {
            throw MultipleTerminalTestError.unexpected
        }
        output += update.output
    }
    func receive(_ values: [TerminalInputOutcome], source: TerminalSource) async throws {
        guard source == self.source, outcomes.count + values.count <= 32,
              values.allSatisfy({ $0.receipt.source == source }) else { throw MultipleTerminalTestError.unexpected }
        try await connection.recordInput(values, source: source)
        outcomes += values
    }
    func waitForOutput(_ expected: Data) async throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while output.count < expected.count {
            guard ContinuousClock.now < deadline else { throw MultipleTerminalTestError.timeout }
            try await Task.sleep(for: .milliseconds(5))
        }
        guard output == expected else { throw MultipleTerminalTestError.unexpected }
    }
}

@MainActor
final class WorkspaceMultipleTerminalTests: XCTestCase {
    private struct Running {
        let connection: WorkspaceSSHConnection
        let pump: TerminalSessionPump
        let consumer: MultipleTerminalConsumer
        let task: Task<TerminalSessionEnd, Error>
    }
    private let banner = Data("ready\r\nstderr\r\n".utf8)

    func testAdditionalTerminalIdentifiersRejectIncompatiblePurposesBeforeAuthorityAccess() async throws {
        let disk = MultipleTerminalPersistence()
        let f = try await SSHWorkspaceTestFixture.make(persistence: disk)
        let activity = WorkspaceNetworkActivity(), lease = try activity.beginOfflineRestore()
        let journal = try TerminalSessionJournal(persistence: SSHWorkspaceTestPersistence())
        let manager = try WorkspaceSSHConnectionManager(store: f.store, broker: f.broker, journal: journal,
            coordinator: TerminalInputCoordinator(), networkActivity: activity)
        await manager.resume()
        let before = disk.accesses
        for (attachMux, requiresExistingMux, purpose) in [
            (true, false, WorkspaceSSHConnectionPurpose.terminal),
            (false, true, .terminal), (false, false, .fileTransfer)
        ] {
            do {
                _ = try await manager.connect(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                    machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true,
                    attachMux: attachMux, requiresExistingMux: requiresExistingMux, purpose: purpose,
                    terminalSessionID: UUID())
                XCTFail("An additional direct terminal identifier authorized an incompatible connection")
            } catch { XCTAssertEqual(error as? NativeSSHError, .invalidInput) }
        }
        XCTAssertEqual(disk.accesses, before)
        XCTAssertEqual(f.vault.counts.reads, 0); XCTAssertEqual(f.vault.counts.signatures, 0)
        XCTAssertEqual(activity.accounting().connections, 0)
        let pending = await manager.pendingCount, retained = await manager.connections()
        XCTAssertEqual(pending, 0); XCTAssertTrue(retained.isEmpty)
        await manager.suspend()
        withExtendedLifetime(lease) {}
    }

    /// The Python parent presents one endpoint backed by independent real SSH
    /// peers. Both owners use the same workspace, machine, route and credential.
    func testLiveSameMachineTerminalOwnersRemainIndependent() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let mode = env["FT_MULTIPLE_TERMINALS_MODE"],
              let port = env["FT_MULTIPLE_TERMINALS_PORT"].flatMap(UInt16.init),
              let host = env["FT_MULTIPLE_TERMINALS_HOSTKEY"].flatMap({ Data(base64Encoded: $0) }) else {
            throw XCTSkip("Run Tests/Interop/test_multiple_terminals.py with the actual native adapter and this XCTest product.")
        }
        guard ["isolation", "suspend", "revoke-credential", "revoke-host", "pending", "capacity"].contains(mode) else {
            throw MultipleTerminalTestError.unexpected
        }
        let f = try await SSHWorkspaceTestFixture.make(hostname: "127.0.0.1", port: port, hostBlob: host)
        let activity = WorkspaceNetworkActivity(), coordinator = try TerminalInputCoordinator()
        let disk = SSHWorkspaceTestPersistence(), journal = try TerminalSessionJournal(persistence: disk)
        let signer = MultipleTerminalSigner(f.vault)
        let broker = mode == "pending" ? SSHAuthenticationBroker(store: f.store, signer: signer,
            support: WorkspaceNativeSSHAuthentication.routeSupport) : f.broker
        let manager = WorkspaceSSHConnectionManager(store: f.store, broker: broker, journal: journal,
            coordinator: coordinator, networkActivity: activity)
        await manager.resume()
        if mode == "pending" {
            try await checkPendingSlot(f, manager: manager, signer: signer, activity: activity, journal: journal)
            return
        }
        var running: [Running] = []
        var settled: Set<UUID> = []
        var focus: TerminalFocus?
        do {
            let slotIDs: [UUID?] = mode == "capacity" ? [nil, UUID(), UUID(), UUID()] : [nil, UUID()]
            for (index, slot) in slotIDs.enumerated() {
                let connection = try await connect(f, manager: manager, slot: slot)
                XCTAssertEqual(connection.terminalSessionID, slot)
                XCTAssertEqual(connection.scope.terminalSessionID, slot)
                let pump = try await connection.openTerminal(geometry: .init(columns: index == 0 ? 80 : 100,
                    rows: index == 0 ? 24 : 28))
                let consumer = MultipleTerminalConsumer(source: pump.source, connection: connection)
                let task = Task { try await pump.run(consumer: consumer) }
                running.append(.init(connection: connection, pump: pump, consumer: consumer, task: task))
                try await consumer.waitForOutput(banner)
            }
            let owners = await manager.connections(), records = try await journal.records()
            XCTAssertEqual(owners.count, running.count)
            XCTAssertEqual(Set(owners.map(\.id)).count, running.count)
            XCTAssertEqual(Set(owners.map { $0.identity.transportID }).count, running.count)
            XCTAssertEqual(Set(owners.map { $0.identity.channelID }).count, running.count)
            XCTAssertEqual(Set(records.map { $0.scope.terminalSessionID }), Set(slotIDs))
            XCTAssertEqual(records.count, running.count)
            XCTAssertTrue(records.allSatisfy { !$0.canResolve && $0.state == .creationStarted })
            if mode == "capacity" {
                do { _ = try await connect(f, manager: manager, slot: UUID()); XCTFail("A fifth terminal bypassed the machine limit") }
                catch { XCTAssertEqual(error as? WorkspaceSSHConnectionError, .capacity) }
                XCTAssertEqual(activity.accounting().connections, 4)
            } else {
                let left = running[0], right = running[1]
                let leftBytes = Data("first terminal α\r".utf8), rightBytes = Data("second terminal β\r".utf8)
                let stream = try await coordinator.makeStream(sceneID: UUID())
                let leftFocus = try await coordinator.changeFocus(stream, to: left.pump.source)
                focus = leftFocus
                let leftReceipt = try await coordinator.submit(leftFocus, encodedBytes: leftBytes)
                try await left.consumer.waitForOutput(banner + leftBytes)
                let rightFocus = try await coordinator.changeFocus(leftFocus, to: right.pump.source)
                focus = rightFocus
                do { _ = try await coordinator.submit(leftFocus, encodedBytes: Data("wrong-owner".utf8)); XCTFail("Retired focus was reusable") }
                catch { XCTAssertEqual(error as? TerminalInputError, .stale) }
                let rightReceipt = try await coordinator.submit(rightFocus, encodedBytes: rightBytes)
                try await right.consumer.waitForOutput(banner + rightBytes)
                XCTAssertEqual(leftReceipt.source, left.pump.source); XCTAssertEqual(rightReceipt.source, right.pump.source)
                if mode == "isolation" {
                    try await left.connection.requestResize(.init(columns: 111, rows: 31), source: left.pump.source)
                    try await right.connection.requestResize(.init(columns: 99, rows: 29), source: right.pump.source)
                    do {
                        try await left.connection.requestResize(.init(columns: 73, rows: 21), source: right.pump.source)
                        XCTFail("A resize crossed its owning connection")
                    } catch {}
                    let reply = Data("\u{1B}[0n".utf8)
                    _ = try await coordinator.reply(left.pump.source, encodedBytes: reply)
                    try await left.consumer.waitForOutput(banner + leftBytes + reply)
                    try await finish(left, coordinator: coordinator, manager: manager)
                    settled.insert(left.connection.id)
                    try await manager.close(connectionID: left.connection.id)
                    let survivors = await manager.connections(), rightClosed = await right.connection.isClosed
                    XCTAssertEqual(survivors.map(\.id), [right.connection.id]); XCTAssertFalse(rightClosed)
                    let after = Data("still-running-B\rEND-SESSION\r".utf8)
                    _ = try await coordinator.submit(rightFocus, encodedBytes: after)
                    try await right.consumer.waitForOutput(banner + rightBytes + after)
                    let ended = try await right.task.value
                    XCTAssertEqual(ended, .completed(.init(remoteExit: nil, channelClosed: true)))
                } else {
                    switch mode {
                    case "suspend": await manager.suspend()
                    case "revoke-credential":
                        let fingerprint = try XCTUnwrap(f.archive.workspace.credentials.first).fingerprint
                        let receipt = try await manager.revokeCredentialLocally(fingerprint: fingerprint, confirmedByUser: true)
                        XCTAssertTrue(receipt.durableDenialConfirmed)
                        XCTAssertEqual(receipt.affectedWorkspaceIDs, [f.installed.id])
                    case "revoke-host":
                        let receipt = try await manager.revokeHostLocally(workspaceID: f.installed.id,
                            trustID: "trust", key: f.host, confirmedByUser: true)
                        XCTAssertEqual(receipt.affectedWorkspaceIDs, [f.installed.id])
                    default: throw MultipleTerminalTestError.unexpected
                    }
                    for terminal in running {
                        let closed = await terminal.connection.isClosed
                        XCTAssertTrue(closed, "A global lifecycle or revocation event missed another slot")
                    }
                    do { _ = try await coordinator.submit(rightFocus, encodedBytes: Data("after-denial".utf8)); XCTFail("Closed owner accepted input") }
                    catch { XCTAssertNotNil(error as? TerminalInputError) }
                }
            }
            if let focus { _ = try await coordinator.suspend(focus) }
            for terminal in running where !settled.contains(terminal.connection.id) {
                try await finish(terminal, coordinator: coordinator, manager: manager)
                settled.insert(terminal.connection.id)
            }
            let remaining = await manager.connections(), input = await coordinator.accounting()
            let history = try await journal.records()
            XCTAssertTrue(remaining.isEmpty)
            XCTAssertEqual(activity.accounting().connections, 0)
            XCTAssertEqual(input.connections, 0); XCTAssertEqual(input.streams, 0)
            XCTAssertEqual(input.retainedBytes, 0); XCTAssertEqual(input.inFlightBytes, 0)
            XCTAssertEqual(history.count, running.count)
            XCTAssertTrue(history.allSatisfy { $0.state == .interrupted && $0.canResolve })
            await manager.suspend()
        } catch {
            for terminal in running { terminal.pump.requestStop() }
            if let focus { _ = try? await coordinator.suspend(focus) }
            for terminal in running where !settled.contains(terminal.connection.id) {
                try? await finish(terminal, coordinator: coordinator, manager: manager)
            }
            await manager.suspend()
            throw error
        }
    }

    private func connect(_ fixture: SSHWorkspaceTestFixture, manager: WorkspaceSSHConnectionManager,
                         slot: UUID?) async throws -> WorkspaceSSHConnection {
        try await manager.connect(workspaceID: fixture.installed.id, expectedRevision: fixture.installed.revision,
            machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true,
            connectionTimeout: .seconds(5), authenticationTimeout: .seconds(6), attachMux: false,
            terminalSessionID: slot)
    }

    private func finish(_ terminal: Running, coordinator: TerminalInputCoordinator,
                        manager: WorkspaceSSHConnectionManager) async throws {
        terminal.pump.requestStop()
        _ = try await terminal.task.value
        // Native ownership must be closed before a durable input seal can be
        // minted. Undelivered outcomes keep this exact owner for the retry.
        do { try await manager.close(connectionID: terminal.connection.id) }
        catch let error as TerminalInputError where error == .unavailable {}
        try await terminal.pump.reconcileStoppedOutcomes { outcomes, source in
            try await terminal.connection.recordInput(outcomes, source: source)
        }
        try await terminal.connection.prepareInputSettlement(source: terminal.pump.source)
        try await coordinator.retire(terminal.pump.source)
        try await manager.close(connectionID: terminal.connection.id)
    }

    private func checkPendingSlot(_ fixture: SSHWorkspaceTestFixture, manager: WorkspaceSSHConnectionManager,
                                  signer: MultipleTerminalSigner, activity: WorkspaceNetworkActivity,
                                  journal: TerminalSessionJournal) async throws {
        let slot = UUID()
        let attempt = Task { try await connect(fixture, manager: manager, slot: slot) }
        do {
            let deadline = ContinuousClock.now + .seconds(5)
            while await !signer.isPaused {
                guard ContinuousClock.now < deadline else { throw MultipleTerminalTestError.timeout }
                try await Task.sleep(for: .milliseconds(5))
            }
            let pending = await manager.pendingCount
            XCTAssertEqual(pending, 1); XCTAssertEqual(activity.accounting().connections, 1)
            do { _ = try await connect(fixture, manager: manager, slot: slot); XCTFail("The same pending slot admitted a second connection") }
            catch { XCTAssertEqual(error as? WorkspaceSSHConnectionError, .busy) }
            XCTAssertEqual(fixture.vault.counts.signatures, 0)
            await signer.release()
            let connection = try await attempt.value
            XCTAssertEqual(connection.terminalSessionID, slot)
            XCTAssertEqual(fixture.vault.counts.signatures, 1)
            try await manager.close(connectionID: connection.id)
            let records = try await journal.records(), owners = await manager.connections()
            XCTAssertTrue(records.isEmpty); XCTAssertTrue(owners.isEmpty)
            XCTAssertEqual(activity.accounting().connections, 0)
            await manager.suspend()
        } catch {
            attempt.cancel(); await signer.release()
            if let connection = try? await attempt.value { try? await manager.close(connectionID: connection.id) }
            await manager.suspend(); throw error
        }
    }
}
