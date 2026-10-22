import Foundation
import Dispatch
import XCTest
@testable import FrankenTermKit
@testable import FrankenTermSSH
import FrankenTermSSHIntegration

/// Only the synchronous persistence boundary is paused. The connection factory,
/// workspace validation, foreground broker and network gate are production code.
private final class SSHConnectionReadHookPersistence: WorkspacePersistence, @unchecked Sendable {
    private let storage = SSHWorkspaceTestPersistence()
    private let lock = NSLock()
    private var readHook: (@Sendable () throws -> Void)?

    func beforeNextCatalogRead(_ hook: @escaping @Sendable () throws -> Void) {
        lock.lock(); defer { lock.unlock() }
        readHook = hook
    }
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T {
        try storage.withExclusiveAccess(body)
    }
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? {
        lock.lock()
        let hook = slot == .catalog ? readHook : nil
        if slot == .catalog { readHook = nil }
        lock.unlock()
        try hook?()
        return try storage.read(slot, limit: limit)
    }
    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws { try storage.write(data, to: slot) }
    func removeJournal() throws { try storage.removeJournal() }
}

@MainActor
final class WorkspaceConnectionTests: XCTestCase {
    private enum Denial {
        case network(WorkspaceNetworkError)
        case authentication(SSHAuthenticationError)
        case cancellation
    }

    private func connect(_ fixture: SSHWorkspaceTestFixture, activity: WorkspaceNetworkActivity,
                         workspaceID: UUID? = nil, revision: UUID? = nil,
                         machineID: String = "machine", routeID: String = "route",
                         credentialID: String = "key", confirmed: Bool = true) async throws -> WorkspaceNativeSSHConnection {
        try await WorkspaceNativeSSHConnection.connect(workspaceID: workspaceID ?? fixture.installed.id,
            expectedRevision: revision ?? fixture.installed.revision, machineID: machineID,
            routeID: routeID, credentialID: credentialID, confirmedByUser: confirmed,
            store: fixture.store, broker: fixture.broker, networkActivity: activity,
            // A missed admission check fails with the connector's invalidInput
            // before DNS; these negative tests never need a reachable server.
            connectionTimeout: .zero)
    }

    private func assertDenied(_ expected: Denial, fixture: SSHWorkspaceTestFixture,
                              activity: WorkspaceNetworkActivity,
                              file: StaticString = #filePath, line: UInt = #line,
                              operation: () async throws -> WorkspaceNativeSSHConnection) async {
        let nativeBefore = NativeSSHConnector.accounting()
        let networkBefore = activity.accounting(), vaultBefore = fixture.vault.counts
        do {
            let connection = try await operation()
            await connection.close()
            XCTFail("A denied request reached a live connection", file: file, line: line)
        } catch {
            switch expected {
            case .network(let failure):
                XCTAssertEqual(error as? WorkspaceNetworkError, failure, file: file, line: line)
            case .authentication(let failure):
                XCTAssertEqual(error as? SSHAuthenticationError, failure, file: file, line: line)
            case .cancellation:
                XCTAssertTrue(error is CancellationError, file: file, line: line)
            }
        }
        let nativeAfter = NativeSSHConnector.accounting(), vaultAfter = fixture.vault.counts
        XCTAssertEqual(nativeAfter.resolvingHosts, nativeBefore.resolvingHosts, file: file, line: line)
        XCTAssertEqual(nativeAfter.connectingSockets, nativeBefore.connectingSockets, file: file, line: line)
        XCTAssertEqual(activity.accounting(), networkBefore, file: file, line: line)
        XCTAssertEqual(vaultAfter.reads, vaultBefore.reads, file: file, line: line)
        XCTAssertEqual(vaultAfter.signatures, vaultBefore.signatures, file: file, line: line)
    }

    func testExplicitConfirmationPrecedesForegroundRestoreAndImportedPreferences() async throws {
        for policy in ["on_select", "manual"] {
            let fixture = try await SSHWorkspaceTestFixture.make { workspace in
                var routes = workspace["routes"] as! [[String: Any]]
                routes[0]["session_creation"] = policy
                workspace["routes"] = routes
            }
            let activity = WorkspaceNetworkActivity(), offline = try activity.beginOfflineRestore()
            await fixture.broker.suspend()
            XCTAssertTrue(fixture.installed.connectLater)
            await assertDenied(.network(.confirmationRequired), fixture: fixture, activity: activity) {
                try await self.connect(fixture, activity: activity, confirmed: false)
            }
            // Both imported auto-connect switches and an explicit manual policy
            // remain inert without the user's separate connection decision.
            let saved = try await fixture.store.load()
            XCTAssertTrue(try XCTUnwrap(saved.first).connectLater)
            withExtendedLifetime(offline) {}
        }
    }

    func testOfflineRestoreRefusesConfirmedConnectionBeforeResolution() async throws {
        let fixture = try await SSHWorkspaceTestFixture.make(), activity = WorkspaceNetworkActivity()
        let offline = try activity.beginOfflineRestore()
        XCTAssertEqual(activity.accounting().offlineRestores, 1)
        await assertDenied(.network(.offlineRestoreActive), fixture: fixture, activity: activity) {
            try await self.connect(fixture, activity: activity)
        }
        XCTAssertEqual(activity.accounting().connections, 0)
        withExtendedLifetime(offline) {}
    }

    func testSuspensionCancellationAndForegroundEpochChangeNeverDial() async throws {
        let fixture = try await SSHWorkspaceTestFixture.make(), activity = WorkspaceNetworkActivity()
        await fixture.broker.suspend()
        await assertDenied(.authentication(.unavailable), fixture: fixture, activity: activity) {
            try await self.connect(fixture, activity: activity)
        }
        await fixture.broker.resume()
        await assertDenied(.cancellation, fixture: fixture, activity: activity) {
            // Inherit MainActor and cancel before yielding it to the new task.
            let attempt = Task { try await self.connect(fixture, activity: activity) }
            attempt.cancel()
            return try await attempt.value
        }

        let persistence = SSHConnectionReadHookPersistence()
        let store = WorkspaceStore(persistence: persistence, vault: fixture.vault)
        let installed = try await store.install(fixture.archive, envelopeDigest: Data(repeating: 2, count: 32),
            expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let broker = SSHAuthenticationBroker(store: store, signer: fixture.vault,
            support: WorkspaceNativeSSHAuthentication.routeSupport)
        await broker.resume()
        let crossing = SSHWorkspaceTestFixture(store: store, broker: broker, vault: fixture.vault,
            installed: installed, archive: fixture.archive, publicBlob: fixture.publicBlob,
            hostBlob: fixture.hostBlob, host: fixture.host, endpoint: fixture.endpoint)
        persistence.beforeNextCatalogRead {
            // Pause the store's synchronous read only after connect captured its
            // foreground witness. Resume the real broker in a different epoch
            // before allowing the readiness snapshot to return to connect.
            let resumed = DispatchSemaphore(value: 0)
            Task.detached {
                await broker.suspend()
                await broker.resume()
                resumed.signal()
            }
            guard resumed.wait(timeout: .now() + 5) == .success else { throw FTError.storage }
        }
        await assertDenied(.authentication(.stale), fixture: crossing, activity: activity) {
            try await self.connect(crossing, activity: activity)
        }
    }

    func testMissingWorkspaceAndStaleRevisionReleaseAdmissionWithoutKeyAccess() async throws {
        let fixture = try await SSHWorkspaceTestFixture.make(), activity = WorkspaceNetworkActivity()
        await assertDenied(.authentication(.stale), fixture: fixture, activity: activity) {
            try await self.connect(fixture, activity: activity, workspaceID: UUID())
        }
        await assertDenied(.authentication(.stale), fixture: fixture, activity: activity) {
            try await self.connect(fixture, activity: activity, revision: UUID())
        }
        // A failed admission must not leave a lease that prevents later restore.
        let offline = try activity.beginOfflineRestore()
        XCTAssertEqual(activity.accounting().offlineRestores, 1)
        withExtendedLifetime(offline) {}
    }

    func testMachineRouteAndCredentialMustBelongToSelectedWorkspace() async throws {
        let fixture = try await SSHWorkspaceTestFixture.make { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            var otherRoute = routes[0]; otherRoute["id"] = "other-route"
            routes.append(otherRoute); workspace["routes"] = routes
            var machines = workspace["machines"] as! [[String: Any]]
            var otherMachine = machines[0]
            otherMachine["id"] = "other-machine"
            otherMachine["route_refs"] = ["other-route"]
            otherMachine["preferred_route_ref"] = "other-route"
            machines.append(otherMachine); workspace["machines"] = machines
            var groups = workspace["groups"] as! [[String: Any]]
            groups[0]["machine_order"] = ["machine", "other-machine"]
            workspace["groups"] = groups
        }
        let activity = WorkspaceNetworkActivity()
        for (machine, route, credential) in [
            ("missing-machine", "route", "key"),
            ("machine", "missing-route", "key"),
            ("machine", "other-route", "key"),
            ("other-machine", "route", "key"),
            ("machine", "route", "missing-credential")
        ] {
            await assertDenied(.authentication(.denied), fixture: fixture, activity: activity) {
                try await self.connect(fixture, activity: activity, machineID: machine,
                    routeID: route, credentialID: credential)
            }
        }
    }

    func testUnsupportedModeJumpAndMissingSessionOrAuthenticationPoliciesNeverDial() async throws {
        for variant in 0..<4 {
            let fixture = try await SSHWorkspaceTestFixture.make { workspace in
                var routes = workspace["routes"] as! [[String: Any]]
                switch variant {
                case 0:
                    routes[0]["mode"] = "frankenterm_mux"
                    routes[0]["session_creation"] = nil
                    routes[0]["attach_policy"] = "existing_only"
                case 1:
                    var jump = routes[0]
                    jump["id"] = "jump-route"; jump["session_creation"] = nil
                    routes[0]["jump_routes"] = ["jump-route"]
                    routes.append(jump)
                case 2: routes[0]["session_creation"] = nil
                default: routes[0]["authentication_methods"] = ["password"]
                }
                workspace["routes"] = routes
            }
            let activity = WorkspaceNetworkActivity()
            await assertDenied(.authentication(.denied), fixture: fixture, activity: activity) {
                try await self.connect(fixture, activity: activity)
            }
        }
    }
}
