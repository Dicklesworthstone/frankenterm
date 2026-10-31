import Foundation
import XCTest
import FrankenTermKit
import FrankenTermSSH
import FrankenTermSSHIntegration

/// Faults occur at the actual catalog publication boundary, both before and
/// after bytes become observable. Neither case proves durable synchronization.
private final class RevocationFaultPersistence: WorkspacePersistence, @unchecked Sendable {
    private let base = SSHWorkspaceTestPersistence(), lock = NSRecursiveLock()
    private var publishBeforeFailure: Bool?
    func failNextWrite(afterPublication: Bool) { lock.withLock { publishBeforeFailure = afterPublication } }
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T { try lock.withLock(body) }
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? { try base.read(slot, limit: limit) }
    func removeJournal() throws { try base.removeJournal() }
    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws {
        try lock.withLock {
            if slot == .catalog, let publish = publishBeforeFailure {
                publishBeforeFailure = nil
                if publish { try base.write(data, to: slot) }
                throw FTError.storage
            }
            try base.write(data, to: slot)
        }
    }
}

@MainActor
final class WorkspaceCredentialRevocationTests: XCTestCase {
    private func manager(_ fixture: SSHWorkspaceTestFixture) throws -> WorkspaceSSHConnectionManager {
        try WorkspaceSSHConnectionManager(store: fixture.store, broker: fixture.broker,
            journal: TerminalSessionJournal(persistence: SSHWorkspaceTestPersistence()),
            coordinator: TerminalInputCoordinator(), networkActivity: WorkspaceNetworkActivity())
    }

    private func importDistinct(_ other: SSHWorkspaceTestFixture, into store: WorkspaceStore,
                                bundle: String, digest: UInt8) async throws -> InstalledWorkspace {
        let archive = try AirGapArchive(workspace: other.archive.workspace.reissuingBundle(id: bundle),
            keys: other.archive.keys, using: SSHWorkspaceTestCryptography())
        return try await store.install(archive, envelopeDigest: Data(repeating: digest, count: 32),
            expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
    }

    func testOneConfirmedFingerprintDeniesAllAliasesButPreservesUnrelatedAuthority() async throws {
        let fixture = try await SSHWorkspaceTestFixture.make()
        let alias = try await SSHWorkspaceTestFixture.make { $0["id"] = "alias-workspace" }
        let other = try await SSHWorkspaceTestFixture.make(seed: Data(repeating: 41, count: 32)) {
            $0["id"] = "unrelated-workspace"
        }
        let aliasInstalled = try await importDistinct(alias, into: fixture.store, bundle: "alias-bundle", digest: 2)
        let otherInstalled = try await importDistinct(other, into: fixture.store, bundle: "other-bundle", digest: 3)
        let manager = try manager(fixture)
        await manager.resume()
        let foreground = try await fixture.broker.foregroundLease()
        let fingerprint = try SSHPublicIdentity(blob: fixture.publicBlob).fingerprint
        let receipt = try await manager.revokeCredentialLocally(fingerprint: fingerprint, confirmedByUser: true)
        XCTAssertEqual(receipt.fingerprint, fingerprint)
        XCTAssertEqual(receipt.affectedWorkspaceIDs, [fixture.installed.id, aliasInstalled.id])
        XCTAssertTrue(receipt.durableDenialConfirmed)
        XCTAssertTrue(receipt.retainedConnectionIDs.isEmpty)
        let available = try await fixture.store.loadWithCredentialAvailability()
        XCTAssertTrue(try XCTUnwrap(available.first { $0.workspace.id == fixture.installed.id }).credentialIDs.isEmpty)
        XCTAssertTrue(try XCTUnwrap(available.first { $0.workspace.id == aliasInstalled.id }).credentialIDs.isEmpty)
        XCTAssertEqual(try XCTUnwrap(available.first { $0.workspace.id == otherInstalled.id }).credentialIDs, ["key"])
        // Revocation never suspends the global broker or invalidates an
        // unrelated workspace's still-current foreground witness.
        try await fixture.broker.validateForeground(foreground)
        let authorization = try await fixture.broker.authorization(workspaceID: otherInstalled.id,
            expectedRevision: otherInstalled.revision, routeID: "route", credentialID: "key",
            presentedHostKey: other.host)
        XCTAssertEqual(authorization.fingerprint, try SSHPublicIdentity(blob: other.publicBlob).fingerprint)
        let permitted = await manager.permitsConnections
        XCTAssertTrue(permitted)
        XCTAssertEqual(fixture.vault.counts.reads, 0)
        XCTAssertEqual(fixture.vault.counts.signatures, 0)
    }

    func testAmbiguousWritesKeepProcessDenialAcrossResumeAndReimportUntilReaffirmed() async throws {
        for afterPublication in [false, true] {
            let persistence = RevocationFaultPersistence()
            let fixture = try await SSHWorkspaceTestFixture.make(persistence: persistence)
            let manager = try manager(fixture)
            await manager.resume()
            let fingerprint = try SSHPublicIdentity(blob: fixture.publicBlob).fingerprint
            persistence.failNextWrite(afterPublication: afterPublication)
            let receipt = try await manager.revokeCredentialLocally(fingerprint: fingerprint, confirmedByUser: true)
            XCTAssertFalse(receipt.durableDenialConfirmed)
            XCTAssertEqual(receipt.affectedWorkspaceIDs, [fixture.installed.id])
            let observed = try await fixture.store.locallyRevokedCredentialFingerprints()
            XCTAssertEqual(observed.contains(fingerprint), afterPublication,
                "Observable readback must never be mistaken for confirmed durability")
            await manager.suspend()
            await manager.resume()
            let replacement = try AirGapArchive(workspace: fixture.archive.workspace.reissuingBundle(id: "reimport"),
                keys: [], using: SSHWorkspaceTestCryptography())
            let imported: InstalledWorkspace
            do {
                imported = try await fixture.store.install(replacement, envelopeDigest: Data(repeating: 8, count: 32),
                    expectedRevision: fixture.installed.revision, sourceAndContentsConfirmed: true,
                    securityChangesConfirmed: true, associateWithExistingWorkspace: true).workspace
                XCTAssertFalse(afterPublication, "An observable durable denial must reject the reimport itself")
            } catch {
                guard afterPublication else { throw error }
                XCTAssertEqual(error as? WorkspaceStoreError, .revokedIdentity)
                imported = fixture.installed
            }
            let before = NativeSSHConnector.accounting()
            do {
                _ = try await manager.connect(workspaceID: imported.id, expectedRevision: imported.revision,
                    machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true)
                XCTFail("An uncertain persistence result revived the locally denied fingerprint")
            } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
            XCTAssertEqual(NativeSSHConnector.accounting().resolvingHosts, before.resolvingHosts)
            XCTAssertEqual(NativeSSHConnector.accounting().connectingSockets, before.connectingSockets)
            XCTAssertEqual(fixture.vault.counts.reads, 0)
            XCTAssertEqual(fixture.vault.counts.signatures, 0)
            let reaffirmed = try await manager.revokeCredentialLocally(fingerprint: fingerprint, confirmedByUser: true)
            XCTAssertTrue(reaffirmed.durableDenialConfirmed)
            let reopened = WorkspaceStore(persistence: persistence, vault: fixture.vault)
            let saved = try await reopened.locallyRevokedCredentialFingerprints()
            XCTAssertEqual(saved, [fingerprint])
            let readiness = try await reopened.loadWithCredentialAvailability()
            XCTAssertTrue(try XCTUnwrap(readiness.first).credentialIDs.isEmpty)
        }
    }

    func testFailedDurableDenialBlocksAutomaticAndExplicitHopKeysBeforeAuthentication() async throws {
        let persistence = RevocationFaultPersistence()
        let fixture = try await SSHWorkspaceTestFixture.make(persistence: persistence) { workspace in
            var routes = workspace["routes"] as! [[String: Any]], hop = routes[0]
            hop["id"] = "jump"; hop["jump_routes"] = []
            hop["authentication_methods"] = ["publickey", "password"]
            routes[0]["credential_refs"] = []
            routes[0]["authentication_methods"] = ["password"]
            routes[0]["jump_routes"] = ["jump"]
            workspace["routes"] = routes + [hop]
        }
        let manager = try manager(fixture), fingerprint = try SSHPublicIdentity(blob: fixture.publicBlob).fingerprint
        await manager.resume()
        persistence.failNextWrite(afterPublication: false)
        let receipt = try await manager.revokeCredentialLocally(fingerprint: fingerprint, confirmedByUser: true)
        XCTAssertFalse(receipt.durableDenialConfirmed)
        let saved = try await fixture.store.locallyRevokedCredentialFingerprints()
        XCTAssertTrue(saved.isEmpty, "Only the manager remembers the failed-write refusal")
        await manager.suspend(); await manager.resume()
        let states = try await fixture.store.loadWithLocalPolicy(support: WorkspaceNativeSSHAuthentication.routeSupport)
        let local = try XCTUnwrap(states.first)
        XCTAssertEqual(local.credentialIDs, ["key"], "The underlying key remains available after the failed write")
        let options = try WorkspaceSSHRouteOptions(localState: local, machineID: "machine", routeID: "route",
            support: WorkspaceNativeSSHAuthentication.routeSupport)
        XCTAssertEqual(options.destination.authenticationChoices, [.password])
        XCTAssertEqual(options.hops.first?.authenticationChoices, [.publicKey("key"), .password])
        let keySelection = try options.select(authentication: .password, hopAuthentications: [.publicKey("key")])
        let before = NativeSSHConnector.accounting()
        for selection in [nil, keySelection] as [WorkspaceSSHConnectionSelection?] {
            do {
                _ = try await manager.connect(workspaceID: fixture.installed.id, expectedRevision: fixture.installed.revision,
                    machineID: "machine", routeID: "route", authentication: .password, confirmedByUser: true,
                    attachMux: false, selection: selection)
                XCTFail("An unsaved local revocation revived the hop's private key")
            } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
        }
        // A deliberate keyless path does not offer the denied key. This
        // fixture has no password prompter, so reaching unavailable proves
        // method admission succeeded while still preventing any TCP traffic.
        let keyless = try options.select(authentication: .password, hopAuthentications: [.password])
        do {
            _ = try await manager.connect(workspaceID: fixture.installed.id, expectedRevision: fixture.installed.revision,
                machineID: "machine", routeID: "route", authentication: .password, confirmedByUser: true,
                attachMux: false, selection: keyless)
            XCTFail("A password connection proceeded without a prompter")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .unavailable) }
        XCTAssertEqual(NativeSSHConnector.accounting().resolvingHosts, before.resolvingHosts)
        XCTAssertEqual(NativeSSHConnector.accounting().connectingSockets, before.connectingSockets)
        XCTAssertEqual(fixture.vault.counts.reads, 0)
        XCTAssertEqual(fixture.vault.counts.signatures, 0)
        let pending = await manager.pendingCount, connections = await manager.connections()
        XCTAssertEqual(pending, 0); XCTAssertTrue(connections.isEmpty)
    }

    func testRevocationRequiresExactInstalledIdentityAndFreshExplicitConfirmation() async throws {
        let fixture = try await SSHWorkspaceTestFixture.make(), manager = try manager(fixture)
        let fingerprint = try SSHPublicIdentity(blob: fixture.publicBlob).fingerprint
        do {
            _ = try await manager.revokeCredentialLocally(fingerprint: fingerprint, confirmedByUser: false)
            XCTFail("Revocation omitted its explicit confirmation")
        } catch { XCTAssertEqual(error as? WorkspaceNetworkError, .confirmationRequired) }
        let cancelled = Task { try await manager.revokeCredentialLocally(fingerprint: fingerprint, confirmedByUser: true) }
        cancelled.cancel()
        do { _ = try await cancelled.value; XCTFail("A cancelled caller admitted revocation") }
        catch { XCTAssertTrue(error is CancellationError) }
        let foreign = "SHA256:" + Data(repeating: 255, count: 32).base64EncodedString().dropLast()
        do {
            _ = try await manager.revokeCredentialLocally(fingerprint: foreign, confirmedByUser: true)
            XCTFail("A foreign identity consumed revocation inventory")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
        let saved = try await fixture.store.locallyRevokedCredentialFingerprints()
        XCTAssertTrue(saved.isEmpty)
        let available = try await fixture.store.loadWithCredentialAvailability()
        XCTAssertEqual(try XCTUnwrap(available.first).credentialIDs, ["key"])
    }

    private func refuses(_ store: WorkspaceStore, _ workspaceID: UUID, _ key: WorkspaceDocument.HostKey) async throws -> Bool {
        try await store.revokedHostKeys(workspaceID: workspaceID).values.contains { $0.contains(key) }
    }

    /// Whether Keys shows `key`'s block as not saved.
    private func unsaved(_ store: WorkspaceStore, _ workspaceID: UUID, _ key: WorkspaceDocument.HostKey) async throws -> Bool {
        try await store.loadWithLocalPolicy().first { $0.workspace.id == workspaceID }?
            .unsavedHostKeysByTrustScope.values.contains { $0.contains(key) } == true
    }

    func testOneConfirmedHostKeyRefusalCoversItsEndpointInEveryWorkspace() async throws {
        let fixture = try await SSHWorkspaceTestFixture.make()
        let alias = try await SSHWorkspaceTestFixture.make { $0["id"] = "alias-workspace" }
        // The same pinned key at another host stays trusted there.
        let other = try await SSHWorkspaceTestFixture.make(hostname: "example.net") { $0["id"] = "unrelated-workspace" }
        XCTAssertEqual(other.host, fixture.host)
        let aliasInstalled = try await importDistinct(alias, into: fixture.store, bundle: "alias-bundle", digest: 2)
        let otherInstalled = try await importDistinct(other, into: fixture.store, bundle: "other-bundle", digest: 3)
        let manager = try manager(fixture)
        await manager.resume()
        let receipt = try await manager.revokeHostLocally(workspaceID: fixture.installed.id, trustID: "trust",
            key: fixture.host, confirmedByUser: true)
        XCTAssertEqual(receipt.host, "localhost")
        XCTAssertEqual(receipt.port, 22)
        XCTAssertEqual(receipt.key, fixture.host)
        XCTAssertEqual(receipt.affectedWorkspaceIDs, [fixture.installed.id, aliasInstalled.id])
        XCTAssertTrue(receipt.retainedConnectionIDs.isEmpty)
        let refusedHere = try await refuses(fixture.store, fixture.installed.id, fixture.host)
        let refusedByAlias = try await refuses(fixture.store, aliasInstalled.id, fixture.host)
        let refusedElsewhere = try await refuses(fixture.store, otherInstalled.id, fixture.host)
        XCTAssertTrue(refusedHere)
        XCTAssertTrue(refusedByAlias)
        XCTAssertFalse(refusedElsewhere)
        let readiness = try await fixture.store.loadWithLocalPolicy(support: WorkspaceNativeSSHAuthentication.routeSupport)
        for entry in readiness {
            XCTAssertEqual(entry.routes.first?.issues == [.hostIdentityRevoked], entry.workspace.id != otherInstalled.id)
        }
        do {
            _ = try await fixture.broker.authorization(workspaceID: aliasInstalled.id,
                expectedRevision: aliasInstalled.revision, routeID: "route", credentialID: "key",
                presentedHostKey: alias.host)
            XCTFail("The alias workspace still trusted the refused host key")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
        let authorization = try await fixture.broker.authorization(workspaceID: otherInstalled.id,
            expectedRevision: otherInstalled.revision, routeID: "route", credentialID: "key",
            presentedHostKey: other.host)
        XCTAssertEqual(authorization.host, "example.net")
        // Route readiness refuses before dialing: localhost:22 would fail
        // with a socket error or a pin mismatch, never this denial.
        do {
            _ = try await manager.connect(workspaceID: fixture.installed.id, expectedRevision: fixture.installed.revision,
                machineID: "machine", routeID: "route", credentialID: "key", confirmedByUser: true)
            XCTFail("A refused host key admitted a connection")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
        let permitted = await manager.permitsConnections
        XCTAssertTrue(permitted)
        XCTAssertEqual(fixture.vault.counts.signatures, 0)
    }

    func testAFailedHostKeyRefusalWriteIsReportedAndARetrySavesIt() async throws {
        for afterPublication in [false, true] {
            let persistence = RevocationFaultPersistence()
            let fixture = try await SSHWorkspaceTestFixture.make(persistence: persistence)
            let manager = try manager(fixture)
            await manager.resume()
            persistence.failNextWrite(afterPublication: afterPublication)
            do {
                _ = try await manager.revokeHostLocally(workspaceID: fixture.installed.id, trustID: "trust",
                    key: fixture.host, confirmedByUser: true)
                XCTFail("A failed write was reported as a saved refusal")
            } catch { XCTAssertEqual(error as? FTError, .storage) }
            // This run of the app refuses the key whether or not it was saved;
            // a relaunch knows only what was saved.
            let observed = try await refuses(fixture.store, fixture.installed.id, fixture.host)
            XCTAssertTrue(observed, "a refusal whose write failed still holds")
            let relaunched = try await refuses(WorkspaceStore(persistence: persistence, vault: fixture.vault),
                                               fixture.installed.id, fixture.host)
            XCTAssertEqual(relaunched, afterPublication)
            // Keys offers the retry exactly when a relaunch would forget it.
            let notSaved = try await unsaved(fixture.store, fixture.installed.id, fixture.host)
            XCTAssertEqual(notSaved, !afterPublication)
            let receipt = try await manager.revokeHostLocally(workspaceID: fixture.installed.id, trustID: "trust",
                key: fixture.host, confirmedByUser: true)
            XCTAssertEqual(receipt.affectedWorkspaceIDs, [fixture.installed.id])
            let stillUnsaved = try await unsaved(fixture.store, fixture.installed.id, fixture.host)
            XCTAssertFalse(stillUnsaved, "a saved block is shown as saved")
            let reopened = WorkspaceStore(persistence: persistence, vault: fixture.vault)
            let saved = try await refuses(reopened, fixture.installed.id, fixture.host)
            XCTAssertTrue(saved)
        }
    }

    func testHostKeyRefusalNeedsConfirmationAndAKeyTheWorkspacePins() async throws {
        let fixture = try await SSHWorkspaceTestFixture.make(), manager = try manager(fixture)
        do {
            _ = try await manager.revokeHostLocally(workspaceID: fixture.installed.id, trustID: "trust",
                key: fixture.host, confirmedByUser: false)
            XCTFail("A host-key refusal omitted its explicit confirmation")
        } catch { XCTAssertEqual(error as? WorkspaceNetworkError, .confirmationRequired) }
        let unpinned = try WorkspaceDocument.HostKey(algorithm: fixture.host.algorithm,
            fingerprint: "SHA256:" + Data(repeating: 255, count: 32).base64EncodedString().dropLast())
        for (trustID, key, workspaceID) in [("trust", unpinned, fixture.installed.id),
                                            ("missing", fixture.host, fixture.installed.id),
                                            ("trust", fixture.host, UUID())] {
            do {
                _ = try await manager.revokeHostLocally(workspaceID: workspaceID, trustID: trustID, key: key,
                    confirmedByUser: true)
                XCTFail("An unpinned key or unknown trust consumed the saved refusal inventory")
            } catch { XCTAssertEqual(error as? FTError, .stale) }
        }
        let saved = try await fixture.store.revokedHostKeys(workspaceID: fixture.installed.id)
        XCTAssertTrue(saved.values.allSatisfy(\.isEmpty))
    }
}
