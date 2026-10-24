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
}
