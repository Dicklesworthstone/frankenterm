import Foundation
import XCTest
import FrankenTermKit
import FrankenTermSSH
import FrankenTermSSHIntegration

/// Park the manager's real store read, then finish with a retryable-looking
/// error even if the task was cancelled. A completed task does not replace its
/// original error when cancellation arrives; the manager must fence that result.
private final class ConnectionManagerFailureGate: @unchecked Sendable {
    private let condition = NSCondition()
    private var started = false, released = false, cancelled = false
    private var startObserver: CheckedContinuation<Void, Never>?
    private var cancellationObserver: CheckedContinuation<Bool, Never>?

    func pauseAndFail() throws {
        condition.lock()
        guard !started else { condition.unlock(); return }
        started = true
        let observer = startObserver; startObserver = nil
        condition.unlock(); observer?.resume(); condition.lock()
        let deadline = Date().addingTimeInterval(3)
        while !released {
            if Task.isCancelled, !cancelled {
                cancelled = true
                let observer = cancellationObserver; cancellationObserver = nil
                condition.unlock(); observer?.resume(returning: true); condition.lock()
            }
            if Date() >= deadline { released = true; break }
            _ = condition.wait(until: min(Date().addingTimeInterval(0.01), deadline))
        }
        let cancellation = cancellationObserver; cancellationObserver = nil
        let observed = cancelled
        condition.unlock(); cancellation?.resume(returning: observed)
        throw NativeSSHError.native(-1006)
    }
    func waitUntilStarted() async {
        await withCheckedContinuation { observer in
            condition.lock()
            if started { condition.unlock(); observer.resume() }
            else { startObserver = observer; condition.unlock() }
        }
    }
    func waitUntilCancelled() async -> Bool {
        await withCheckedContinuation { observer in
            condition.lock()
            if cancelled || released {
                let value = cancelled; condition.unlock(); observer.resume(returning: value)
            } else { cancellationObserver = observer; condition.unlock() }
        }
    }
    func release() { condition.lock(); released = true; condition.broadcast(); condition.unlock() }
}

private final class ConnectionManagerPersistence: WorkspacePersistence, @unchecked Sendable {
    private let lock = NSRecursiveLock()
    private let failure: ConnectionManagerFailureGate?
    private var slots: [String: Data] = [:]
    private var leases = 0
    init(failure: ConnectionManagerFailureGate? = nil) { self.failure = failure }
    var accesses: Int { lock.withLock { leases } }
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T {
        try lock.withLock { leases += 1; return try body() }
    }
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? {
        if slot == .catalog { try failure?.pauseAndFail() }
        return try lock.withLock {
            guard let value = slots[slot.rawValue] else { return nil }
            guard value.count <= limit else { throw FTError.storage }
            return value
        }
    }
    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws {
        lock.withLock { slots[slot.rawValue] = data }
    }
    func removeJournal() throws { lock.withLock { _ = slots.removeValue(forKey: WorkspaceStorageSlot.journal.rawValue) } }
}

private struct ConnectionManagerVault: WorkspaceCredentialVault, WorkspaceSSHSigning {
    func create(account: String, privateContainer: Data, fingerprint: String) throws { throw FTError.storage }
    func fingerprint(account: String) throws -> String? { nil }
    func remove(account: String) throws { throw FTError.storage }
    func publicIdentity(requestID: UUID, authorization: WorkspaceSSHAuthorization) async throws -> Data {
        throw SSHAuthenticationError.unavailable
    }
    func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
              challenge: SSHUserAuthenticationChallenge) async throws -> Data {
        throw SSHAuthenticationError.unavailable
    }
    func cancel(requestID: UUID) {}
}

final class WorkspaceConnectionManagerTests: XCTestCase, @unchecked Sendable {
    private struct Fixture {
        let manager: WorkspaceSSHConnectionManager
        let storePersistence: ConnectionManagerPersistence
        let sessionPersistence: ConnectionManagerPersistence
        let journal: TerminalSessionJournal
        let networkActivity: WorkspaceNetworkActivity
    }
    private func fixture(failure: ConnectionManagerFailureGate? = nil) throws -> Fixture {
        let storePersistence = ConnectionManagerPersistence(failure: failure), sessionPersistence = ConnectionManagerPersistence()
        let vault = ConnectionManagerVault(), store = WorkspaceStore(persistence: storePersistence, vault: vault)
        let support = WorkspaceNativeSSHAuthentication.routeSupport
        let broker = SSHAuthenticationBroker(store: store, signer: vault, support: support)
        let journal = try TerminalSessionJournal(persistence: sessionPersistence)
        let networkActivity = WorkspaceNetworkActivity()
        return try .init(manager: WorkspaceSSHConnectionManager(store: store, broker: broker,
            journal: journal, coordinator: TerminalInputCoordinator(), networkActivity: networkActivity),
            storePersistence: storePersistence, sessionPersistence: sessionPersistence,
            journal: journal, networkActivity: networkActivity)
    }

    func testRestorationCannotStartAnAttemptBeforeExplicitForegroundResume() async throws {
        let fixture = try fixture()
        do {
            _ = try await fixture.manager.connect(workspaceID: UUID(), expectedRevision: UUID(),
                machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true)
            XCTFail("offline manager admitted a connection")
        } catch { XCTAssertEqual(error as? WorkspaceSSHConnectionError, .offline) }
        XCTAssertEqual(fixture.storePersistence.accesses, 0)
        XCTAssertEqual(fixture.sessionPersistence.accesses, 0)
        let pending = await fixture.manager.pendingCount, connections = await fixture.manager.connections()
        XCTAssertEqual(pending, 0); XCTAssertTrue(connections.isEmpty)
    }

    func testFailedSelectionReleasesAttemptAndSuspensionRestoresNetworkInhibition() async throws {
        let fixture = try fixture()
        await fixture.manager.resume()
        do {
            _ = try await fixture.manager.connect(workspaceID: UUID(), expectedRevision: UUID(),
                machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true)
            XCTFail("missing workspace reached connection")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
        XCTAssertGreaterThan(fixture.storePersistence.accesses, 0)
        let records = try await fixture.journal.records()
        XCTAssertTrue(records.isEmpty, "No creation record without an eligible route")
        XCTAssertEqual(fixture.networkActivity.accounting().connections, 0)
        let pending = await fixture.manager.pendingCount
        XCTAssertEqual(pending, 0)
        await fixture.manager.suspend()
        let before = fixture.storePersistence.accesses
        await fixture.manager.invalidate(workspaceID: UUID())
        do {
            _ = try await fixture.manager.connect(workspaceID: UUID(), expectedRevision: UUID(),
                machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true)
            XCTFail("suspended manager admitted a connection")
        } catch { XCTAssertEqual(error as? WorkspaceSSHConnectionError, .offline) }
        XCTAssertEqual(fixture.storePersistence.accesses, before)
        let permitted = await fixture.manager.permitsConnections
        XCTAssertFalse(permitted)
    }

    func testForegroundDoesNotReplaceExplicitConnectConfirmation() async throws {
        let fixture = try fixture()
        await fixture.manager.resume()
        do {
            _ = try await fixture.manager.connect(workspaceID: UUID(), expectedRevision: UUID(),
                machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: false)
            XCTFail("imported preferences authorized a connection")
        } catch { XCTAssertEqual(error as? WorkspaceNetworkError, .confirmationRequired) }
        XCTAssertEqual(fixture.storePersistence.accesses, 0)
        XCTAssertEqual(fixture.sessionPersistence.accesses, 0)
        XCTAssertEqual(fixture.networkActivity.accounting().connections, 0)
    }

    func testExplicitSelectionCannotBeReboundBeforeAnyStoreOrNetworkAccess() async throws {
        let fixture = try fixture(), workspaceID = UUID(), revision = UUID()
        await fixture.manager.resume()
        let variants: [(UUID, UUID, String, String, WorkspaceSSHAuthenticationChoice)] = [
            (UUID(), revision, "machine", "route", .password),
            (workspaceID, UUID(), "machine", "route", .password),
            (workspaceID, revision, "other-machine", "route", .password),
            (workspaceID, revision, "machine", "other-route", .password),
            (workspaceID, revision, "machine", "route", .keyboardInteractive)]
        for (workspace, selectedRevision, machine, route, authentication) in variants {
            let selection = WorkspaceSSHConnectionSelection(workspaceID: workspace, expectedRevision: selectedRevision,
                machineID: machine, routeID: route, authentication: authentication)
            do {
                _ = try await fixture.manager.connect(workspaceID: workspaceID, expectedRevision: revision,
                    machineID: "machine", routeID: "route", authentication: .password,
                    confirmedByUser: true, selection: selection)
                XCTFail("Mismatched explicit selection was admitted")
            } catch { XCTAssertEqual(error as? WorkspaceSSHConnectionSelectionError, .stale) }
        }
        XCTAssertEqual(fixture.storePersistence.accesses, 0)
        XCTAssertEqual(fixture.sessionPersistence.accesses, 0)
        XCTAssertEqual(fixture.networkActivity.accounting().connections, 0)
        let pending = await fixture.manager.pendingCount, connections = await fixture.manager.connections()
        XCTAssertEqual(pending, 0); XCTAssertTrue(connections.isEmpty)
    }

    func testExplicitSelectionCannotExpandTheQualifiedHopBudget() async throws {
        let fixture = try fixture(), workspaceID = UUID(), revision = UUID()
        await fixture.manager.resume()
        let selection = WorkspaceSSHConnectionSelection(workspaceID: workspaceID, expectedRevision: revision,
            machineID: "machine", routeID: "route", authentication: .password,
            hops: (0..<4).map { .init(routeID: "hop-\($0)", authentication: .password) })
        do {
            _ = try await fixture.manager.connect(workspaceID: workspaceID, expectedRevision: revision,
                machineID: "machine", routeID: "route", authentication: .password,
                confirmedByUser: true, selection: selection)
            XCTFail("Explicit selection expanded the native hop budget")
        } catch { XCTAssertEqual(error as? WorkspaceSSHConnectionSelectionError, .hopMismatch) }
        XCTAssertEqual(fixture.storePersistence.accesses, 0)
        XCTAssertEqual(fixture.sessionPersistence.accesses, 0)
        XCTAssertEqual(fixture.networkActivity.accounting().connections, 0)
    }

    func testExplicitSelectionIsNotConnectConsentOrForegroundAuthority() async throws {
        let fixture = try fixture(), workspaceID = UUID(), revision = UUID()
        let selection = WorkspaceSSHConnectionSelection(workspaceID: workspaceID, expectedRevision: revision,
            machineID: "machine", routeID: "route", authentication: .password)
        do {
            _ = try await fixture.manager.connect(workspaceID: workspaceID, expectedRevision: revision,
                machineID: "machine", routeID: "route", authentication: .password,
                confirmedByUser: true, selection: selection)
            XCTFail("Explicit selection bypassed foreground admission")
        } catch { XCTAssertEqual(error as? WorkspaceSSHConnectionError, .offline) }
        await fixture.manager.resume()
        do {
            _ = try await fixture.manager.connect(workspaceID: workspaceID, expectedRevision: revision,
                machineID: "machine", routeID: "route", authentication: .password,
                confirmedByUser: false, selection: selection)
            XCTFail("Explicit selection bypassed Connect confirmation")
        } catch { XCTAssertEqual(error as? WorkspaceNetworkError, .confirmationRequired) }
        XCTAssertEqual(fixture.storePersistence.accesses, 0)
        XCTAssertEqual(fixture.sessionPersistence.accesses, 0)
        XCTAssertEqual(fixture.networkActivity.accounting().connections, 0)
    }

    func testMatchingExplicitSelectionStillRequiresAFreshInstalledRevision() async throws {
        let fixture = try fixture(), workspaceID = UUID(), revision = UUID()
        await fixture.manager.resume()
        let selection = WorkspaceSSHConnectionSelection(workspaceID: workspaceID, expectedRevision: revision,
            machineID: "machine", routeID: "route", authentication: .password)
        do {
            _ = try await fixture.manager.connect(workspaceID: workspaceID, expectedRevision: revision,
                machineID: "machine", routeID: "route", authentication: .password,
                confirmedByUser: true, selection: selection)
            XCTFail("Selection metadata substituted for an installed workspace")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
        XCTAssertGreaterThan(fixture.storePersistence.accesses, 0)
        XCTAssertEqual(fixture.sessionPersistence.accesses, 0)
        XCTAssertEqual(fixture.networkActivity.accounting().connections, 0)
        let pending = await fixture.manager.pendingCount
        XCTAssertEqual(pending, 0)
    }

    func testRetiredAttemptCannotPublishItsOriginalFallbackEligibleError() async throws {
        for action in ["cancel", "invalidate", "suspend"] {
            let failure = ConnectionManagerFailureGate(), fixture = try fixture(failure: failure)
            await fixture.manager.resume()
            let workspaceID = UUID()
            let attempt = Task {
                try await fixture.manager.connect(workspaceID: workspaceID, expectedRevision: UUID(),
                    machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true)
            }
            await failure.waitUntilStarted()
            let retirement: Task<Void, Never>?
            switch action {
            case "invalidate": retirement = Task { await fixture.manager.invalidate(workspaceID: workspaceID) }
            case "suspend": retirement = Task { await fixture.manager.suspend() }
            default: attempt.cancel(); retirement = nil
            }
            let cancelled = await failure.waitUntilCancelled()
            failure.release()
            XCTAssertTrue(cancelled, action)
            do { _ = try await attempt.value; XCTFail("Retired attempt succeeded: \(action)") }
            catch { XCTAssertTrue(error is CancellationError, "\(action): \(error)") }
            await retirement?.value
            let pending = await fixture.manager.pendingCount, connections = await fixture.manager.connections()
            XCTAssertEqual(pending, 0); XCTAssertTrue(connections.isEmpty)
            XCTAssertEqual(fixture.networkActivity.accounting().connections, 0)
            XCTAssertEqual(fixture.sessionPersistence.accesses, 0, "No shell marker before authentication")
        }
    }

    func testCurrentAttemptPreservesItsActualFailureAfterCleanup() async throws {
        let failure = ConnectionManagerFailureGate(), fixture = try fixture(failure: failure)
        await fixture.manager.resume()
        let attempt = Task {
            try await fixture.manager.connect(workspaceID: UUID(), expectedRevision: UUID(),
                machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true)
        }
        await failure.waitUntilStarted(); failure.release()
        do { _ = try await attempt.value; XCTFail("Faulted attempt succeeded") }
        catch { XCTAssertEqual(error as? NativeSSHError, .native(-1006)) }
        let pending = await fixture.manager.pendingCount, connections = await fixture.manager.connections()
        XCTAssertEqual(pending, 0); XCTAssertTrue(connections.isEmpty)
        XCTAssertEqual(fixture.networkActivity.accounting().connections, 0)
    }
}

final class WorkspaceRouteDiagnosisTests: XCTestCase {
    func testAddressesAreClassifiedByTheNetworkThatReachesThem() {
        for host in ["100.64.0.1", "100.91.120.17", "100.127.255.254", "fd7a:115c:a1e0::1", "trj.tail1234.ts.net",
                     "TRJ.tail1234.TS.NET."] {
            XCTAssertEqual(WorkspaceRouteNetwork(host: host), .tailnet, host)
        }
        for host in ["10.10.10.1", "172.16.0.1", "172.31.255.1", "192.168.1.240", "169.254.1.1", "fe80::1",
                     "fe80::1%en0", "fd00::1", "mmax.local", "mmax.local."] {
            XCTAssertEqual(WorkspaceRouteNetwork(host: host), .localNetwork, host)
        }
        for host in ["100.63.255.255", "100.128.0.1", "172.32.0.1", "209.145.54.164", "2606:4700::1", "example.com"] {
            XCTAssertEqual(WorkspaceRouteNetwork(host: host), .internet, host)
        }
        XCTAssertTrue(WorkspaceRouteNetwork.isLikelyReachable("209.145.54.164"))
        XCTAssertTrue(WorkspaceRouteNetwork.isLikelyReachable("mmax.local"))
    }

    func testOnlyAnUnansweredAddressIsExplainedAndTailscaleIsTheFix() throws {
        let tailscale = try XCTUnwrap(WorkspaceConnectionHelp(failure: NativeSSHError.timedOut, machine: "trj",
            host: "100.91.120.17", port: 22, onTailnet: false, device: "this iPhone"))
        XCTAssertEqual(tailscale.remedy, .turnOnTailscale)
        XCTAssertTrue(tailscale.message.contains("this iPhone isn't connected to Tailscale"))
        let asleep = try XCTUnwrap(WorkspaceConnectionHelp(failure: NativeSSHError.timedOut, machine: "trj",
            host: "100.91.120.17", port: 22, onTailnet: true, device: "this iPhone"))
        XCTAssertEqual(asleep.remedy, .none)
        XCTAssertTrue(asleep.message.hasPrefix("This iPhone is on Tailscale"))
        // Refused before leaving the device: possibly the Local Network permission.
        let lan = try XCTUnwrap(WorkspaceConnectionHelp(failure: NativeSSHError.connection(EHOSTUNREACH), machine: "mold",
            host: "192.168.1.233", port: 22, onTailnet: true))
        XCTAssertEqual(lan.remedy, .allowLocalNetwork)
        // Silence from a local address: another network, not a permission.
        let away = try XCTUnwrap(WorkspaceConnectionHelp(failure: NativeSSHError.timedOut, machine: "mold",
            host: "192.168.1.233", port: 22, onTailnet: true))
        XCTAssertEqual(away.remedy, .none)
        XCTAssertTrue(away.message.contains("same Wi-Fi"))
        let refused = try XCTUnwrap(WorkspaceConnectionHelp(failure: NativeSSHError.connection(ECONNREFUSED), machine: "css",
            host: "209.145.54.164", port: 2222, onTailnet: false))
        XCTAssertEqual(refused.title, "css refused the connection")
        XCTAssertTrue(refused.message.contains("port 2222"))
        let offline = try XCTUnwrap(WorkspaceConnectionHelp(failure: NativeSSHError.connection(ENETUNREACH), machine: "css",
            host: "209.145.54.164", port: 22, onTailnet: false))
        XCTAssertEqual(offline.title, "No network connection")
        XCTAssertNil(WorkspaceConnectionHelp(failure: NativeSSHError.native(-1006), machine: "css",
            host: "209.145.54.164", port: 22, onTailnet: false))
        XCTAssertNil(WorkspaceConnectionHelp(failure: SSHAuthenticationError.hostKeyMismatch, machine: "css",
            host: "209.145.54.164", port: 22, onTailnet: false))
        XCTAssertEqual(WorkspaceConnectionHelp.best([refused, lan, tailscale]), tailscale)
        XCTAssertEqual(WorkspaceConnectionHelp.best([refused, lan]), refused)
        XCTAssertEqual(WorkspaceConnectionHelp.best([refused]), refused)
        XCTAssertNil(WorkspaceConnectionHelp.best([]))
    }
}
