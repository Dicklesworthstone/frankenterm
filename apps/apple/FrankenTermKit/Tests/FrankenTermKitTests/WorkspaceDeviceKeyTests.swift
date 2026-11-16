import Foundation
import XCTest
@testable import FrankenTermKit
#if canImport(CryptoKit)
import CryptoKit
#else
import Crypto
#endif

/// The normal fault-injected vault plus a test-only view of the exact generated
/// container. No production API returns or exports that protected material.
private final class DeviceKeyCaptureVault: WorkspaceCredentialVault, @unchecked Sendable {
    let storage = StoreTestVault()
    private let lock = NSLock()
    private var materials: [String: Data] = [:]
    func create(account: String, privateContainer: Data, fingerprint: String) throws {
        try storage.create(account: account, privateContainer: privateContainer, fingerprint: fingerprint)
        lock.lock(); defer { lock.unlock() }; materials[account] = privateContainer
    }
    func fingerprint(account: String) throws -> String? { try storage.fingerprint(account: account) }
    func publicIdentity(account: String) throws -> SSHPublicIdentity? {
        guard let bytes = material(account: account) else { return nil }
        return try SSHUserAuthenticationSigner.publicIdentity(privateContainer: bytes)
    }
    func remove(account: String) throws {
        try storage.remove(account: account)
        lock.lock(); defer { lock.unlock() }; materials[account] = nil
    }
    func material(account: String) -> Data? {
        lock.lock(); defer { lock.unlock() }; return materials[account]
    }
}

@MainActor
final class WorkspaceDeviceKeyTests: XCTestCase {
    private struct Fixture {
        let store: WorkspaceStore, disk: StoreTestPersistence, vault: DeviceKeyCaptureVault
        let archive: AirGapArchive, saved: InstalledWorkspace
    }
    private let crypto = TestCryptography()
    private let digest = Data(repeating: 181, count: 32)
    private var support: WorkspaceDocument.RouteSupport {
        .init(routeModes: ["direct_ssh", "frankenterm_mux"], authenticationMethods: [.publicKey],
              credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"])
    }

    private func edited(_ document: WorkspaceDocument, bundle: String,
                        change: (inout [String: Any]) -> Void) throws -> WorkspaceDocument {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: document.raw) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        change(&workspace); root["workspace"] = workspace; root["bundle_id"] = bundle
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }

    private func fixture(finding: Bool = false, secondRoute: Bool = false,
                         hops: Bool = false) async throws -> Fixture {
        let (key, fingerprint) = try sampleKey(using: crypto)
        let source = try WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint))
        let document = try edited(source, bundle: "bundle") { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            if secondRoute {
                var secondary = routes[0]; secondary["id"] = "secondary"; routes.append(secondary)
            }
            if hops {
                var first = routes[0], second = routes[0]
                first["id"] = "jump-one"; first["jump_routes"] = ["jump-two"]
                second["id"] = "jump-two"
                routes[0]["jump_routes"] = ["jump-one"]
                routes += [first, second]
            }
            workspace["routes"] = routes
        }
        let archive = try AirGapArchive(workspace: document, keys: [key],
            portability: finding ? [.init(entityID: "key", setting: "authentication_policy", disposition: 1,
                reason: "The source requires an authentication policy unavailable on this device.")] : [], using: crypto)
        let disk = StoreTestPersistence(), vault = DeviceKeyCaptureVault()
        let store = WorkspaceStore(persistence: disk, vault: vault)
        let result = try await store.install(archive, envelopeDigest: digest,
            expectedRevision: nil, sourceAndContentsConfirmed: true)
        return .init(store: store, disk: disk, vault: vault, archive: archive, saved: result.workspace)
    }

    private func generate(_ f: Fixture, saved: InstalledWorkspace? = nil) async throws -> WorkspaceDeviceKeyReceipt {
        try await f.store.createDeviceKey(workspaceID: f.saved.id, expectedRevision: saved?.revision ?? f.saved.revision,
                                         confirmedByUser: true)
    }
    private func assign(_ f: Fixture, created: WorkspaceDeviceKeyReceipt) async throws -> InstalledWorkspace {
        try await f.store.assignDeviceKey(workspaceID: created.workspace.id, expectedRevision: created.workspace.revision,
            credentialID: created.key.id, routeID: "route", confirmedByUser: true)
    }
    private func authorization(_ f: Fixture, saved: InstalledWorkspace, credentialID: String) async throws -> WorkspaceSSHAuthorization {
        try await f.store.authorizeSSHAuthentication(workspaceID: saved.id, expectedRevision: saved.revision,
            routeID: "route", credentialID: credentialID,
            presentedHostKey: XCTUnwrap(f.archive.workspace.hostTrust.first?.pins.first), support: support)
    }
    private func replace(_ f: Fixture, saved: InstalledWorkspace, document: WorkspaceDocument,
                         digestByte: UInt8) async throws -> InstalledWorkspace {
        let archive = try AirGapArchive(workspace: document, keys: [], using: crypto)
        return try await f.store.install(archive, envelopeDigest: Data(repeating: digestByte, count: 32),
            expectedRevision: saved.revision, sourceAndContentsConfirmed: true,
            allowMachineRemoval: true, securityChangesConfirmed: true, associateWithExistingWorkspace: true).workspace
    }

    func testGeneratedKeyIsProtectedDistinctAndCannotAuthenticateBeforeAssignment() async throws {
        let f = try await fixture()
        let originalAuthorization = try await authorization(f, saved: f.saved, credentialID: "key")
        let created = try await generate(f)
        XCTAssertEqual(created.workspace.revision, f.saved.revision, "creating an unused key must not interrupt live sessions")
        try await f.store.validateSSHAuthentication(originalAuthorization, support: support)
        XCTAssertEqual(created.workspace.manifest, f.saved.manifest)
        XCTAssertEqual(created.workspace.envelopeDigest, f.saved.envelopeDigest)
        XCTAssertTrue(created.workspace.deviceKeyAssignments.isEmpty)
        XCTAssertEqual(try created.workspace.document().routes[0].credentialIDs, ["key"])
        XCTAssertNotEqual(created.key.fingerprint, f.archive.workspace.credentials[0].fingerprint)
        let fields = created.key.authorizedKey.split(separator: " ")
        XCTAssertEqual(fields.count, 3); XCTAssertEqual(fields[0], "ssh-ed25519")
        XCTAssertEqual(Data(base64Encoded: String(fields[1])), created.key.publicBlob)
        XCTAssertEqual(String(fields[2]), created.key.id)
        let binding = try XCTUnwrap(created.workspace.bindings.first { $0.credentialID == created.key.id })
        let material = try XCTUnwrap(f.vault.material(account: binding.account))
        // TestCryptography independently derives the public key (OpenSSL on Linux).
        XCTAssertEqual(try PrivateKeyRecord.validateContainer(material, using: crypto), created.key.publicBlob)
        let challenge = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(0..<32),
            user: "operator", publicBlob: created.key.publicBlob)
        let signature = try SSHUserAuthenticationSigner.sign(challenge, privateContainer: material)
        let publicKey = try Curve25519.Signing.PublicKey(rawRepresentation: created.key.publicBlob.suffix(32))
        XCTAssertTrue(publicKey.isValidSignature(signature.suffix(64), for: challenge.signedData))
        XCTAssertFalse(publicKey.isValidSignature(signature.suffix(64), for: challenge.signedData + Data([0])))
        let catalog = try XCTUnwrap(f.disk.bytes(.catalog))
        XCTAssertNil(catalog.range(of: material))
        XCTAssertNil(catalog.range(of: Data(material.base64EncodedString().utf8)))
        do {
            _ = try await authorization(f, saved: created.workspace, credentialID: created.key.id)
            XCTFail("generation alone granted a route the new private identity")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
    }

    func testAssignmentReplacesCandidatesAndRetiresOldAuthorizationWithoutChangingSource() async throws {
        let f = try await fixture(secondRoute: true)
        let oldAuthority = try await authorization(f, saved: f.saved, credentialID: "key")
        let created = try await generate(f), assigned = try await assign(f, created: created)
        XCTAssertNotEqual(assigned.revision, created.workspace.revision)
        XCTAssertEqual(assigned.manifest, f.saved.manifest)
        XCTAssertEqual(try assigned.importedDocument().routes[0].credentialIDs, ["key"])
        let effective = try assigned.document()
        XCTAssertEqual(effective.routes[0].credentialIDs, [created.key.id])
        XCTAssertEqual(effective.routes[1].credentialIDs, ["key"])
        XCTAssertEqual(effective.routes[0].authenticationMethods, [.publicKey])
        let freshAuthority = try await authorization(f, saved: assigned, credentialID: created.key.id)
        do {
            try await f.store.validateSSHAuthentication(oldAuthority, support: support)
            XCTFail("old authorization survived a route credential change")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
        do {
            _ = try await authorization(f, saved: assigned, credentialID: "key")
            XCTFail("copied Mac key remained an implicit fallback")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
        let restored = try await f.store.removeDeviceKeyAssignment(workspaceID: assigned.id,
            expectedRevision: assigned.revision, routeID: "route", confirmedByUser: true)
        XCTAssertEqual(try restored.document().routes[0].credentialIDs, ["key"])
        XCTAssertEqual(restored.deviceKeys, [created.key])
        _ = try await authorization(f, saved: restored, credentialID: "key")
        do {
            try await f.store.validateSSHAuthentication(freshAuthority, support: support)
            XCTFail("device-key authority survived explicit restoration of source candidates")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
    }

    func testExactAndCosmeticReimportsPreserveIdentityAndImmutableAcceptance() async throws {
        let f = try await fixture(), created = try await generate(f), assigned = try await assign(f, created: created)
        let beforeReload = f.disk.bytes(.catalog)
        let loaded = try await f.store.load()
        XCTAssertEqual(try XCTUnwrap(loaded.first).manifest, f.saved.manifest)
        XCTAssertEqual(f.disk.bytes(.catalog), beforeReload, "catalog sanitation must not publish the effective overlay as the source")
        let preview = try await f.store.previewImport(f.archive, envelopeDigest: digest, support: support)
        XCTAssertFalse(preview.assessment.changeReview?.requiresSecurityApproval ?? false)
        XCTAssertTrue(try XCTUnwrap(preview.routes.first).isLocallyReady)
        let same = try await f.store.install(f.archive, envelopeDigest: digest,
            expectedRevision: assigned.revision, sourceAndContentsConfirmed: true).workspace
        XCTAssertEqual(same.revision, assigned.revision)
        XCTAssertEqual(same.deviceKeys, assigned.deviceKeys)
        XCTAssertEqual(same.deviceKeyAssignments, assigned.deviceKeyAssignments)
        let renamed = try edited(f.archive.workspace, bundle: "cosmetic") { $0["name"] = "Renamed group" }
        let newer = try await replace(f, saved: same, document: renamed, digestByte: 182)
        XCTAssertEqual(newer.manifest, renamed.raw)
        XCTAssertEqual(try newer.document().routes[0].credentialIDs, [created.key.id])
        XCTAssertFalse(try newer.deviceKeyAssignmentNeedsReview(routeID: "route"))
        let history = try await f.store.preview(f.archive.workspace, envelopeDigest: digest)
        XCTAssertEqual(history.firstAcceptedRevision, f.saved.revision)
        XCTAssertTrue(history.isRollback)
        XCTAssertEqual(f.vault.storage.creates, 2)
    }

    func testChangedRouteRequiresFreshReviewAndCannotAppearTrusted() async throws {
        let f = try await fixture(), created = try await generate(f), assigned = try await assign(f, created: created)
        let changed = try edited(f.archive.workspace, bundle: "new-account") { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["user"] = "another-operator"; workspace["routes"] = routes
        }
        let incoming = try AirGapArchive(workspace: changed, keys: [], using: crypto)
        let preview = try await f.store.previewImport(incoming, envelopeDigest: Data(repeating: 183, count: 32), support: support)
        XCTAssertFalse(try XCTUnwrap(preview.routes.first).isLocallyReady)
        let replaced = try await replace(f, saved: assigned, document: changed, digestByte: 183)
        XCTAssertTrue(try replaced.deviceKeyAssignmentNeedsReview(routeID: "route"))
        let readiness = try await f.store.routeReadiness(workspaceID: replaced.id, support: support)
        XCTAssertFalse(try XCTUnwrap(readiness.first).isLocallyReady)
        let trust = try await f.store.verifyHostKey(workspaceID: replaced.id, routeID: "route",
            presented: XCTUnwrap(f.archive.workspace.hostTrust.first?.pins.first), support: support)
        XCTAssertEqual(trust, .unsupportedPolicy)
        let claimedMarker = WorkspaceDocument.RouteSupport(routeModes: support.routeModes,
            authenticationMethods: support.authenticationMethods, credentialAlgorithms: support.credentialAlgorithms,
            hostKeyAlgorithms: support.hostKeyAlgorithms, capabilities: [WorkspaceDeviceKeyOverlay.reviewCapability])
        let markerReadiness = try await f.store.routeReadiness(workspaceID: replaced.id, support: claimedMarker)
        XCTAssertFalse(try XCTUnwrap(markerReadiness.first).isLocallyReady,
            "advertising the local marker cannot approve a changed route")
        let markerTrust = try await f.store.verifyHostKey(workspaceID: replaced.id, routeID: "route",
            presented: XCTUnwrap(f.archive.workspace.hostTrust.first?.pins.first), support: claimedMarker)
        XCTAssertEqual(markerTrust, .unsupportedPolicy)
        let reviewed = try await f.store.assignDeviceKey(workspaceID: replaced.id, expectedRevision: replaced.revision,
            credentialID: created.key.id, routeID: "route", confirmedByUser: true)
        XCTAssertFalse(try reviewed.deviceKeyAssignmentNeedsReview(routeID: "route"))
        let granted = try await authorization(f, saved: reviewed, credentialID: created.key.id)
        XCTAssertEqual(granted.user, "another-operator")
    }

    func testRemovedRouteDoesNotBlockOtherMachinesAndReturnsWithItsDeviceKey() async throws {
        let f = try await fixture(secondRoute: true), created = try await generate(f)
        let assigned = try await assign(f, created: created)
        let omitted = try edited(f.archive.workspace, bundle: "omitted-route") { workspace in
            workspace["routes"] = (workspace["routes"] as! [[String: Any]]).filter { $0["id"] as? String != "route" }
            var machines = workspace["machines"] as! [[String: Any]]
            for i in machines.indices {
                machines[i]["route_refs"] = ["secondary"]; machines[i]["preferred_route_ref"] = "secondary"
            }
            workspace["machines"] = machines
        }
        let removed = try await replace(f, saved: assigned, document: omitted, digestByte: 184)
        XCTAssertEqual(removed.deviceKeyAssignments.count, 1, "removed routes retain bounded local intent")
        XCTAssertTrue(try removed.deviceKeyAssignmentNeedsReview(routeID: "route"))
        let remaining = try await f.store.routeReadiness(workspaceID: removed.id, support: support)
        XCTAssertEqual(remaining.count, 1)
        XCTAssertTrue(try XCTUnwrap(remaining.first).isLocallyReady, "an orphan choice must not block unrelated routes")
        let returnedSource = try f.archive.workspace.reissuingBundle(id: "returned-route")
        let returned = try await replace(f, saved: removed, document: returnedSource, digestByte: 185)
        XCTAssertEqual(try returned.document().routes.first { $0.id == "route" }?.credentialIDs, [created.key.id])
        XCTAssertFalse(try returned.deviceKeyAssignmentNeedsReview(routeID: "route"))
        _ = try await authorization(f, saved: returned, credentialID: created.key.id)
    }

    func testRequiredSourceCredentialFindingCannotBeEvadedByADeviceKey() async throws {
        let f = try await fixture(finding: true), created = try await generate(f), assigned = try await assign(f, created: created)
        XCTAssertEqual(try assigned.document().routes[0].credentialIDs, [created.key.id])
        let states = try await f.store.routeReadiness(workspaceID: assigned.id, support: support)
        XCTAssertFalse(try XCTUnwrap(states.first).isLocallyReady)
        XCTAssertTrue(try XCTUnwrap(states.first).issues.contains(.unsupportedRequirement))
        let trust = try await f.store.verifyHostKey(workspaceID: assigned.id, routeID: "route",
            presented: XCTUnwrap(f.archive.workspace.hostTrust.first?.pins.first), support: support)
        XCTAssertEqual(trust, .unsupportedPolicy)
        do {
            _ = try await authorization(f, saved: assigned, credentialID: created.key.id)
            XCTFail("replacing credential_refs bypassed a mandatory source authentication constraint")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
    }

    func testTransitiveHopAndSameScopePolicyChangesFenceTheSelectedKey() async throws {
        for changeHop in [true, false] {
            let f = try await fixture(hops: true), created = try await generate(f), assigned = try await assign(f, created: created)
            let changed = try edited(f.archive.workspace, bundle: "changed-transit") { workspace in
                if changeHop {
                    var routes = workspace["routes"] as! [[String: Any]]
                    let i = routes.firstIndex { $0["id"] as? String == "jump-two" }!
                    routes[i]["user"] = "another-hop-account"; workspace["routes"] = routes
                } else {
                    var trusts = workspace["host_trust"] as! [[String: Any]]
                    var related = trusts[0]; related["id"] = "same-scope-policy"
                    related["required_capabilities"] = ["mandatory-host-policy"]
                    trusts.append(related); workspace["host_trust"] = trusts
                }
            }
            let updated = try await replace(f, saved: assigned, document: changed, digestByte: 186)
            XCTAssertTrue(try updated.deviceKeyAssignmentNeedsReview(routeID: "route"))
            XCTAssertEqual(updated.deviceKeys, [created.key])
        }
    }

    func testRevocationAndLostVaultItemNeverRestoreCopiedKeyFallback() async throws {
        for revoke in [true, false] {
            let f = try await fixture(), created = try await generate(f), assigned = try await assign(f, created: created)
            let authority = try await authorization(f, saved: assigned, credentialID: created.key.id)
            if revoke {
                try await f.store.revokeLocally(fingerprint: created.key.fingerprint)
            } else {
                let binding = try XCTUnwrap(assigned.bindings.first { $0.credentialID == created.key.id })
                try f.vault.remove(account: binding.account) // Simulated device-only Keychain loss.
            }
            do {
                try await f.store.validateSSHAuthentication(authority, support: support)
                XCTFail("revoked or missing protected material retained signing authority")
            } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
            let newer = try f.archive.workspace.reissuingBundle(id: "after-key-loss")
            let replaced = try await replace(f, saved: assigned, document: newer, digestByte: 187)
            XCTAssertEqual(try replaced.document().routes[0].credentialIDs, [created.key.id])
            let states = try await f.store.routeReadiness(workspaceID: replaced.id, support: support)
            XCTAssertFalse(try XCTUnwrap(states.first).isLocallyReady)
            do {
                _ = try await authorization(f, saved: replaced, credentialID: "key")
                XCTFail("reimport restored the copied key as a fallback")
            } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
        }
    }

    func testEveryCreationPublicationBoundaryRecoversOldOrPublishedKeyOwnership() async throws {
        for boundary in 0..<6 {
            let f = try await fixture()
            switch boundary {
            case 0: f.disk.configure(.before(.journal))
            case 1: f.disk.configure(.after(.journal))
            case 2: f.disk.configure(.before(.catalog))
            case 3: f.disk.configure(.after(.catalog))
            case 4: f.vault.storage.configure(.beforeInsert)
            default: f.vault.storage.configure(.afterInsert)
            }
            do { _ = try await generate(f); XCTFail("injected publication failure was not reached") }
            catch { XCTAssertEqual(error as? FTError, .storage) }
            let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault)
            let loaded = try await reopened.load()
            let saved = try XCTUnwrap(loaded.first)
            XCTAssertEqual(saved.manifest, f.saved.manifest)
            XCTAssertEqual(saved.revision, f.saved.revision)
            XCTAssertTrue(saved.deviceKeyAssignments.isEmpty)
            XCTAssertEqual(saved.deviceKeys.count, boundary == 3 ? 1 : 0)
            XCTAssertEqual(f.vault.storage.count, 1 + saved.deviceKeys.count)
            XCTAssertNil(f.disk.bytes(.journal))
        }
    }

    func testConsentStalenessAndCorruptPublicMetadataDoNotRemoveProtectedKeys() async throws {
        let f = try await fixture()
        let original = f.disk.bytes(.catalog)
        do {
            _ = try await f.store.createDeviceKey(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
                                                  confirmedByUser: false)
            XCTFail("unreviewed generation")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .reviewRequired) }
        do {
            _ = try await f.store.createDeviceKey(workspaceID: f.saved.id, expectedRevision: UUID(), confirmedByUser: true)
            XCTFail("stale generation")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
        XCTAssertEqual(f.disk.bytes(.catalog), original)
        XCTAssertEqual(f.vault.storage.creates, 1)
        _ = try await generate(f)
        var catalog = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(f.disk.bytes(.catalog))) as? [String: Any])
        var workspaces = catalog["workspaces"] as! [[String: Any]]
        var keys = workspaces[0]["deviceKeys"] as! [[String: Any]]
        keys[0]["publicBlob"] = Data(repeating: 0, count: 51).base64EncodedString()
        workspaces[0]["deviceKeys"] = keys; catalog["workspaces"] = workspaces
        let corrupted = try JSONSerialization.data(withJSONObject: catalog, options: [.sortedKeys])
        f.disk.seed(corrupted, slot: .catalog)
        do { _ = try await f.store.load(); XCTFail("corrupt public identity was accepted") } catch {}
        XCTAssertEqual(f.vault.storage.count, 2)
        XCTAssertEqual(f.disk.bytes(.catalog), corrupted)
    }
}
