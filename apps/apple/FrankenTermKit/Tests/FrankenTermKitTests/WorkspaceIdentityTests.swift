import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class WorkspaceIdentityTests: XCTestCase {
    private let crypto = TestCryptography()
    private let digest = Data(repeating: 11, count: 32)
    private let transfer = Data(repeating: 11, count: 16)
    private let support = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh", "frankenterm_mux"],
        authenticationMethods: [.publicKey, .password, .keyboardInteractive],
        credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"])

    private func candidate(bundle: String = "bundle", keys: Bool = true,
                           edit: (inout [String: Any]) -> Void = { _ in }) throws -> AirGapArchive {
        let (key, fingerprint) = try sampleKey(using: crypto)
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest(fingerprint: fingerprint, bundleID: bundle)) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        edit(&workspace); root["workspace"] = workspace
        return try AirGapArchive(workspace: WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys])),
                                 keys: keys ? [key] : [], using: crypto)
    }

    func testStoredKeyCannotBeRelabeledByAnImportWithoutPrivateMaterial() async throws {
        let disk = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: disk, vault: vault)
        let first = try await store.install(candidate(), envelopeDigest: digest, transferID: transfer,
            expectedRevision: nil, sourceAndContentsConfirmed: true)
        let falseIdentity = try candidate(bundle: "relabel", keys: false) { workspace in
            var credentials = workspace["credentials"] as! [[String: Any]]
            credentials[0]["algorithm"] = "ecdsa-sha2-nistp256"; workspace["credentials"] = credentials
        }
        let before = disk.bytes(.catalog), nextDigest = Data(repeating: 12, count: 32), nextTransfer = Data(repeating: 12, count: 16)
        do {
            _ = try await store.previewImport(falseIdentity, envelopeDigest: nextDigest, transferID: nextTransfer, support: support)
            XCTFail("preview reused a key under a false algorithm")
        } catch { XCTAssertEqual(error as? FTError, .malformed("store.credential_identity")) }
        do {
            _ = try await store.install(falseIdentity, envelopeDigest: nextDigest, transferID: nextTransfer,
                expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
            XCTFail("install reused a key under a false algorithm")
        } catch { XCTAssertEqual(error as? FTError, .malformed("store.credential_identity")) }
        XCTAssertEqual(disk.bytes(.catalog), before); XCTAssertNil(disk.bytes(.journal))
        XCTAssertEqual(vault.creates, 1)
    }

    func testSubsetAliasBindingsRemainAvailableAndIdempotentAfterMigration() async throws {
        let disk = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: disk, vault: vault)
        let archive = try candidate { workspace in
            var credentials = workspace["credentials"] as! [[String: Any]], alias = credentials[0]
            alias["id"] = "alias"; credentials.append(alias); workspace["credentials"] = credentials
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["credential_refs"] = ["alias", "key"]; workspace["routes"] = routes
        }
        let first = try await store.install(archive, envelopeDigest: digest, transferID: transfer,
            expectedRevision: nil, sourceAndContentsConfirmed: true)
        var catalog = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(disk.bytes(.catalog))) as? [String: Any])
        var saved = try XCTUnwrap(catalog["workspaces"] as? [[String: Any]])
        let bindings = try XCTUnwrap(saved[0]["bindings"] as? [[String: Any]])
        saved[0]["bindings"] = bindings.filter { $0["credentialID"] as? String == "key" }
        catalog["workspaces"] = saved; catalog["version"] = 2; catalog.removeValue(forKey: "hostRevocations")
        disk.seed(try JSONSerialization.data(withJSONObject: catalog), slot: .catalog)
        let reopened = WorkspaceStore(persistence: disk, vault: vault)
        let available = try await reopened.availableCredentialIDs(workspaceID: first.workspace.id)
        XCTAssertEqual(available, ["key", "alias"])
        let snapshot = try await reopened.loadWithReadiness(support: support)
        XCTAssertEqual(snapshot.first?.credentialIDs, ["key", "alias"])
        XCTAssertEqual(snapshot.first?.routes.first?.usableCredentialIDs, ["alias", "key"])
        XCTAssertEqual(snapshot.first?.routes.first?.isLocallyReady, true)
        let preview = try await reopened.previewImport(archive, envelopeDigest: digest, transferID: transfer, support: support)
        XCTAssertEqual(preview.routes.first?.usableCredentialIDs, ["alias", "key"])
        let receipt = try await reopened.install(archive, envelopeDigest: digest, transferID: transfer,
            expectedRevision: first.workspace.revision, sourceAndContentsConfirmed: true)
        XCTAssertTrue(receipt.missingCredentialIDs.isEmpty)
        XCTAssertEqual(receipt.workspace.revision, first.workspace.revision)
        XCTAssertEqual(vault.creates, 1)
    }

    func testPreviewCountsIncludedAndReusedKeysWithoutStagingThem() async throws {
        let disk = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: disk, vault: vault), archive = try candidate()
        let fresh = try await store.previewImport(archive, envelopeDigest: digest, transferID: transfer, support: support)
        XCTAssertEqual(fresh.routes.first?.isLocallyReady, true)
        XCTAssertEqual(vault.creates, 0); XCTAssertNil(disk.bytes(.catalog)); XCTAssertNil(disk.bytes(.journal))
        let saved = try await store.install(archive, envelopeDigest: digest, transferID: transfer,
            expectedRevision: nil, sourceAndContentsConfirmed: true)
        let alias = try candidate(bundle: "alias-export", keys: false) { workspace in
            var credentials = workspace["credentials"] as! [[String: Any]]
            credentials[0]["id"] = "renamed"; workspace["credentials"] = credentials
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["credential_refs"] = ["renamed"]; workspace["routes"] = routes
        }
        let next = try await store.previewImport(alias, envelopeDigest: Data(repeating: 12, count: 32),
            transferID: Data(repeating: 12, count: 16), support: support)
        XCTAssertEqual(next.assessment.existingWorkspace?.revision, saved.workspace.revision)
        XCTAssertEqual(next.routes.first?.usableCredentialIDs, ["renamed"])
        XCTAssertEqual(next.routes.first?.isLocallyReady, true); XCTAssertEqual(vault.creates, 1)
        try await store.revokeLocally(fingerprint: archive.workspace.credentials[0].fingerprint)
        do {
            _ = try await store.previewImport(archive, envelopeDigest: digest, transferID: transfer, support: support)
            XCTFail("preview accepted a locally revoked private identity")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .revokedIdentity) }
    }

    func testRequiredPortabilityBlocksOnlyItsRoutesInPreviewSnapshotAndHostVerification() async throws {
        let disk = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: disk, vault: vault)
        let base = try candidate { workspace in
            var routes = workspace["routes"] as! [[String: Any]], independent = routes[0]
            independent["id"] = "independent"; routes.append(independent); workspace["routes"] = routes
        }
        let archive = try AirGapArchive(workspace: base.workspace, keys: base.keys,
            portability: [.init(entityID: "route", setting: "proxy_command", disposition: 1,
                                 reason: "The required proxy behavior has no portable representation.")], using: crypto)
        let preview = try await store.previewImport(archive, envelopeDigest: digest, transferID: transfer, support: support)
        XCTAssertEqual(preview.routes.first(where: { $0.routeID == "route" })?.issues, [.unsupportedRequirement])
        XCTAssertEqual(preview.routes.first(where: { $0.routeID == "independent" })?.issues, [])
        let saved = try await store.install(archive, envelopeDigest: digest, transferID: transfer,
            expectedRevision: nil, sourceAndContentsConfirmed: true)
        let snapshot = try await store.loadWithReadiness(support: support)
        XCTAssertEqual(snapshot.first?.routes.first(where: { $0.routeID == "route" })?.issues, [.unsupportedRequirement])
        let pin = try XCTUnwrap(archive.workspace.hostTrust.first?.pins.first)
        let blocked = try await store.verifyHostKey(workspaceID: saved.workspace.id, routeID: "route", presented: pin, support: support)
        let independent = try await store.verifyHostKey(workspaceID: saved.workspace.id, routeID: "independent", presented: pin, support: support)
        XCTAssertEqual(blocked, .unsupportedPolicy); XCTAssertEqual(independent, .trustedPin)
    }

    func testBundleCannotBeReboundToAnotherWorkspaceEvenWithFreshEnvelopeIdentity() async throws {
        let disk = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: disk, vault: vault)
        _ = try await store.install(candidate(), envelopeDigest: digest, transferID: transfer,
            expectedRevision: nil, sourceAndContentsConfirmed: true)
        let rebound = try candidate { $0["id"] = "another-workspace" }
        let before = disk.bytes(.catalog)
        do {
            _ = try await store.install(rebound, envelopeDigest: Data(repeating: 12, count: 32),
                transferID: Data(repeating: 12, count: 16), expectedRevision: nil, sourceAndContentsConfirmed: true)
            XCTFail("bundle identity moved between workspaces")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .conflictingExport) }
        XCTAssertEqual(disk.bytes(.catalog), before); XCTAssertNil(disk.bytes(.journal)); XCTAssertEqual(vault.creates, 1)
    }
}
