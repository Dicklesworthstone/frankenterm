import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class WorkspaceMigrationTests: XCTestCase {
    private let extensionMarker = "LEGACY-OPAQUE-EXTENSION-MATERIAL"
    private let annotationMarker = "LEGACY-OPAQUE-ANNOTATION-MATERIAL"

    private func legacyFixture() async throws -> (StoreTestPersistence, StoreTestVault, InstalledWorkspace) {
        let persistence = StoreTestPersistence(), vault = StoreTestVault(), crypto = TestCryptography()
        let (key, fingerprint) = try sampleKey(using: crypto)
        let document = try WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint))
        let archive = try AirGapArchive(workspace: document, keys: [key], using: crypto)
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let installed = try await store.install(archive, envelopeDigest: Data(repeating: 71, count: 32),
            transferID: Data(repeating: 72, count: 16), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        var catalog = try catalogFields(persistence)
        catalog["version"] = 1
        catalog.removeValue(forKey: "acceptedImports")
        catalog.removeValue(forKey: "hostRevocations")
        var records = try XCTUnwrap(catalog["workspaces"] as? [[String: Any]])
        var manifest = try XCTUnwrap(JSONSerialization.jsonObject(with: installed.manifest) as? [String: Any])
        var workspace = try XCTUnwrap(manifest["workspace"] as? [String: Any])
        var routes = try XCTUnwrap(workspace["routes"] as? [[String: Any]])
        for index in routes.indices {
            let keys = try XCTUnwrap(routes[index].removeValue(forKey: "credential_refs") as? [String])
            routes[index]["credential_ref"] = try XCTUnwrap(keys.first)
            routes[index].removeValue(forKey: "authentication_methods")
            routes[index].removeValue(forKey: "session_creation")
        }
        var trusts = try XCTUnwrap(workspace["host_trust"] as? [[String: Any]])
        for index in trusts.indices { trusts[index].removeValue(forKey: "revoked_keys") }
        workspace["routes"] = routes; workspace["host_trust"] = trusts
        manifest["workspace"] = workspace
        manifest["extensions"] = ["example.legacy": ["opaque": extensionMarker]]
        records[0]["manifest"] = try JSONSerialization.data(withJSONObject: manifest, options: [.sortedKeys]).base64EncodedString()
        records[0]["inactiveSettings"] = [["entityID": document.id, "setting": "example.legacy_configuration",
            "original": Data(annotationMarker.utf8).base64EncodedString()]]
        catalog["workspaces"] = records
        persistence.seed(try JSONSerialization.data(withJSONObject: catalog, options: [.sortedKeys]), slot: .catalog)
        return (persistence, vault, installed)
    }

    private func catalogFields(_ persistence: StoreTestPersistence) throws -> [String: Any] {
        try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(persistence.bytes(.catalog))) as? [String: Any])
    }

    private func editManifest(_ persistence: StoreTestPersistence, _ edit: (inout [String: Any]) throws -> Void) throws {
        var catalog = try catalogFields(persistence)
        var records = try XCTUnwrap(catalog["workspaces"] as? [[String: Any]])
        let encoded = try XCTUnwrap(records[0]["manifest"] as? String)
        let data = try XCTUnwrap(Data(base64Encoded: encoded))
        var manifest = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        try edit(&manifest)
        records[0]["manifest"] = try JSONSerialization.data(withJSONObject: manifest, options: [.sortedKeys]).base64EncodedString()
        catalog["workspaces"] = records
        persistence.seed(try JSONSerialization.data(withJSONObject: catalog, options: [.sortedKeys]), slot: .catalog)
    }

    private func persistedWorkspace(_ persistence: StoreTestPersistence) throws -> InstalledWorkspace {
        let records = try XCTUnwrap(try catalogFields(persistence)["workspaces"] as? [[String: Any]])
        return try JSONDecoder().decode(InstalledWorkspace.self,
            from: JSONSerialization.data(withJSONObject: XCTUnwrap(records.first)))
    }

    private func assertSanitized(_ workspace: InstalledWorkspace, original: InstalledWorkspace,
                                 file: StaticString = #filePath, line: UInt = #line) throws {
        XCTAssertEqual(workspace.id, original.id, file: file, line: line)
        XCTAssertEqual(workspace.revision, original.revision, file: file, line: line)
        XCTAssertEqual(workspace.envelopeDigest, original.envelopeDigest, file: file, line: line)
        XCTAssertEqual(workspace.bindings, original.bindings, file: file, line: line)
        XCTAssertEqual(workspace.connectLater, original.connectLater, file: file, line: line)
        XCTAssertNil(workspace.manifest.range(of: Data(extensionMarker.utf8)), file: file, line: line)
        XCTAssertNil(workspace.manifest.range(of: Data(annotationMarker.utf8)), file: file, line: line)
        XCTAssertTrue(workspace.inactiveSettings.isEmpty, file: file, line: line)
        XCTAssertEqual(workspace.findings.count, 1, file: file, line: line)
        XCTAssertEqual(workspace.findings.first?.setting, "frankenterm.discarded_annotations", file: file, line: line)
        XCTAssertEqual(workspace.findings.first?.disposition, 0, file: file, line: line)
        let document = try workspace.document()
        XCTAssertEqual(document.routes.first?.credentialIDs, ["key"], file: file, line: line)
        XCTAssertEqual(document.routes.first?.authenticationMethods, [.publicKey], file: file, line: line)
        XCTAssertNil(document.routes.first?.sessionCreation, "migration cannot invent a shell-creation preference", file: file, line: line)
        XCTAssertTrue(document.hostTrust.allSatisfy { $0.revokedKeys.isEmpty }, file: file, line: line)
        XCTAssertTrue(document.connectOnOpen && document.machines.allSatisfy(\.autoConnect), file: file, line: line)
        XCTAssertNil(try StrictJSON.decode(workspace.manifest).object()["extensions"], file: file, line: line)
    }

    func testTrueVersionOneManifestAndOpaqueMetadataMigrateDurablyWithoutChangingIdentity() async throws {
        let (persistence, vault, original) = try await legacyFixture()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let loaded = try await store.load()
        try assertSanitized(XCTUnwrap(loaded.first), original: original)
        // Decode the persisted manifest: searching outer catalog bytes alone
        // would miss private text hidden by Codable's base64 Data representation.
        let disk = try persistedWorkspace(persistence)
        try assertSanitized(disk, original: original)
        let catalog = try catalogFields(persistence)
        XCTAssertEqual(catalog["version"] as? Int, 3)
        let accepted = try XCTUnwrap((catalog["acceptedImports"] as? [[String: Any]])?.first)
        XCTAssertEqual(accepted["firstAcceptedRevision"] as? String, original.revision.uuidString)
        XCTAssertEqual(accepted["envelopeDigest"] as? String, original.envelopeDigest.base64EncodedString())
        XCTAssertNil(accepted["transferID"], "migration must not invent an authenticated header identity")
        XCTAssertEqual(vault.creates, 1); XCTAssertEqual(vault.count, 1)
        let available = try await store.availableCredentialIDs(workspaceID: original.id)
        XCTAssertEqual(available, ["key"])
        let savedBytes = persistence.bytes(.catalog)
        let relaunched = WorkspaceStore(persistence: persistence, vault: vault)
        let reloaded = try await relaunched.load()
        try assertSanitized(XCTUnwrap(reloaded.first), original: original)
        XCTAssertEqual(persistence.bytes(.catalog), savedBytes, "a completed migration is stable on relaunch")
        XCTAssertNil(persistence.bytes(.journal))
    }

    func testMigrationPublicationFailureKeepsEveryOwnedKeyAndJournalUntilRecovery() async throws {
        for fault in [StoreTestPersistence.Fault.before(.catalog), .after(.catalog)] {
            let (persistence, vault, original) = try await legacyFixture()
            let transaction = UUID(), pending = "ft-import:\(transaction.uuidString):0"
            try vault.create(account: pending, privateContainer: Data([91]), fingerprint: "abandoned-owned-key")
            let retained = try XCTUnwrap(original.bindings.first)
            persistence.seed(try JSONSerialization.data(withJSONObject: ["version": 1, "transaction": transaction.uuidString,
                "targetRevision": UUID().uuidString, "newAccounts": [pending], "retiredAccounts": [retained.account]]), slot: .journal)
            persistence.configure(fault)
            let store = WorkspaceStore(persistence: persistence, vault: vault)
            do { _ = try await store.load(); XCTFail("migration publication fault was not exercised") }
            catch { XCTAssertEqual(error as? FTError, .storage) }
            XCTAssertEqual(vault.count, 2)
            XCTAssertEqual(try vault.fingerprint(account: retained.account), retained.fingerprint)
            XCTAssertEqual(try vault.fingerprint(account: pending), "abandoned-owned-key")
            XCTAssertNotNil(persistence.bytes(.journal))
            let relaunched = WorkspaceStore(persistence: persistence, vault: vault)
            let recovered = try await relaunched.load()
            try assertSanitized(XCTUnwrap(recovered.first), original: original)
            try assertSanitized(persistedWorkspace(persistence), original: original)
            XCTAssertEqual(try catalogFields(persistence)["version"] as? Int, 3)
            XCTAssertEqual(vault.count, 1); XCTAssertEqual(vault.creates, 2)
            XCTAssertEqual(try vault.fingerprint(account: retained.account), retained.fingerprint)
            XCTAssertNil(try vault.fingerprint(account: pending))
            XCTAssertNil(persistence.bytes(.journal))
        }
    }

    func testAmbiguousLegacyAuthenticationSchemasRejectWithoutChangingCatalogOrKeys() async throws {
        for field in ["credential_refs", "authentication_methods"] {
            let (persistence, vault, _) = try await legacyFixture()
            try editManifest(persistence) { manifest in
                var workspace = try XCTUnwrap(manifest["workspace"] as? [String: Any])
                var routes = try XCTUnwrap(workspace["routes"] as? [[String: Any]])
                routes[0][field] = field == "credential_refs" ? ["key"] : ["password"]
                workspace["routes"] = routes; manifest["workspace"] = workspace
            }
            let before = persistence.bytes(.catalog)
            do { _ = try await WorkspaceStore(persistence: persistence, vault: vault).load(); XCTFail("ambiguous legacy authentication was migrated") }
            catch { XCTAssertEqual(error as? WorkspaceStoreError, .corruptStore) }
            XCTAssertEqual(persistence.bytes(.catalog), before)
            XCTAssertEqual(vault.count, 1); XCTAssertEqual(vault.creates, 1)
        }
    }

    func testUnknownPrivateCoreFieldRejectsInsteadOfBeingSanitizedAsAnExtension() async throws {
        let (persistence, vault, _) = try await legacyFixture()
        try editManifest(persistence) { manifest in
            var workspace = try XCTUnwrap(manifest["workspace"] as? [String: Any])
            var routes = try XCTUnwrap(workspace["routes"] as? [[String: Any]])
            routes[0]["private_key"] = "UNRECOGNIZED-PRIVATE-CORE-FIELD"
            workspace["routes"] = routes; manifest["workspace"] = workspace
        }
        let before = persistence.bytes(.catalog)
        do { _ = try await WorkspaceStore(persistence: persistence, vault: vault).load(); XCTFail("unknown private core field was accepted") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .corruptStore) }
        XCTAssertEqual(persistence.bytes(.catalog), before)
        XCTAssertEqual(vault.count, 1); XCTAssertEqual(vault.creates, 1)
    }
}
