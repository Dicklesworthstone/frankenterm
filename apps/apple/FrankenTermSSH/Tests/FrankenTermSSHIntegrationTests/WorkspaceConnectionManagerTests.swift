import Foundation
import XCTest
import FrankenTermKit
import FrankenTermSSHIntegration

private final class ConnectionManagerPersistence: WorkspacePersistence, @unchecked Sendable {
    private let lock = NSRecursiveLock()
    private var slots: [String: Data] = [:]
    private var leases = 0
    var accesses: Int { lock.withLock { leases } }
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T {
        try lock.withLock { leases += 1; return try body() }
    }
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? {
        try lock.withLock {
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
    private func fixture() throws -> Fixture {
        let storePersistence = ConnectionManagerPersistence(), sessionPersistence = ConnectionManagerPersistence()
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
}
