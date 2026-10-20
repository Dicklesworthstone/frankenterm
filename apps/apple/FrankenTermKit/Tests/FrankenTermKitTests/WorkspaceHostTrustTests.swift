import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class WorkspaceHostTrustTests: XCTestCase {
    private let crypto = TestCryptography()
    private let hostFingerprint = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
    private let otherFingerprint = "SHA256:ERERERERERERERERERERERERERERERERERERERERERE"
    private let support = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh", "frankenterm_mux"],
        authenticationMethods: [.publicKey], credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"])

    private func archive(_ bundleID: String, host: String = "host.example.invalid", port: Int = 22,
                         trustID: String = "trust", mutate: (inout [String: Any]) -> Void = { _ in }) throws -> AirGapArchive {
        let (key, fingerprint) = try sampleKey(using: crypto)
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest(fingerprint: fingerprint, bundleID: bundleID)) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        var routes = try XCTUnwrap(workspace["routes"] as? [[String: Any]])
        var trusts = try XCTUnwrap(workspace["host_trust"] as? [[String: Any]])
        routes[0]["host"] = host; routes[0]["port"] = port; routes[0]["host_trust_ref"] = trustID
        trusts[0]["host"] = host; trusts[0]["port"] = port; trusts[0]["id"] = trustID
        workspace["routes"] = routes; workspace["host_trust"] = trusts
        mutate(&workspace); root["workspace"] = workspace
        return try AirGapArchive(workspace: WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys])),
                                 keys: [key], using: crypto)
    }

    private func install(_ archive: AirGapArchive, in store: WorkspaceStore, identity: UInt8,
                         replacing previous: InstalledWorkspace? = nil, allowRollback: Bool = false) async throws -> InstalledWorkspace {
        try await store.install(archive, envelopeDigest: Data(repeating: identity, count: 32),
            transferID: Data(repeating: identity, count: 16), expectedRevision: previous?.revision,
            sourceAndContentsConfirmed: true, allowHistoricalRollback: allowRollback).workspace
    }

    private func addRevocation(_ workspace: inout [String: Any]) {
        var trusts = workspace["host_trust"] as! [[String: Any]]
        trusts[0]["revoked_keys"] = trusts[0]["pins"]
        workspace["host_trust"] = trusts
    }

    func testImportedRevocationSurvivesReplacementRenamingRelaunchAndApprovedRollback() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let pin = try WorkspaceDocument.HostKey(algorithm: "ssh-ed25519", fingerprint: hostFingerprint)
        let original = try archive("original")
        let first = try await install(original, in: store, identity: 1)
        let denied = try await install(archive("denied", mutate: addRevocation), in: store, identity: 2, replacing: first)
        let replacement = try await install(archive("renamed", trustID: "renamed-trust"), in: store, identity: 3, replacing: denied)
        XCTAssertTrue(try replacement.document().hostTrust[0].revokedKeys.isEmpty)

        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        let saved = try await reopened.load()
        XCTAssertEqual(saved.first?.revision, replacement.revision)
        let decision = try await reopened.verifyHostKey(workspaceID: replacement.id, routeID: "route", presented: pin, support: support)
        XCTAssertEqual(decision, .revoked)
        let readiness = try await reopened.routeReadiness(workspaceID: replacement.id, support: support)
        XCTAssertEqual(readiness.first?.issues, [.hostIdentityRevoked])

        // Even deliberate rollback to accepted older bytes restores configuration
        // only. It does not authorize clearing the phone's negative trust.
        let rolledBack = try await install(original, in: reopened, identity: 1, replacing: replacement, allowRollback: true)
        let afterRollback = try await reopened.verifyHostKey(workspaceID: rolledBack.id, routeID: "route", presented: pin, support: support)
        XCTAssertEqual(afterRollback, .revoked)
        XCTAssertEqual(try rolledBack.document().bundleID, "original")
        XCTAssertTrue(rolledBack.connectLater)
        XCTAssertEqual(vault.creates, 1)
    }

    func testLocalRevocationUsesCanonicalHostIdentityAndExactPort() async throws {
        for (canonical, rejectedSpelling) in [
            ("host.example.invalid", "HOST.EXAMPLE.INVALID."),
            ("xn--bcher-kva.example", "BÜCHER.example。"),
            ("192.0.2.1", "[::FFFF:C000:0201]"),
            ("2001:db8::1", "[2001:0DB8:0:0:0:0:0:1]")
        ] {
            let persistence = StoreTestPersistence(), vault = StoreTestVault()
            let store = WorkspaceStore(persistence: persistence, vault: vault)
            let pin = try WorkspaceDocument.HostKey(algorithm: "ssh-ed25519", fingerprint: hostFingerprint)
            let first = try await install(archive("first", host: canonical), in: store, identity: 1)
            try await store.revokeHostKeyLocally(host: rejectedSpelling, port: 22, algorithm: pin.algorithm, fingerprint: pin.fingerprint)
            let reopened = WorkspaceStore(persistence: persistence, vault: vault)
            let decision = try await reopened.verifyHostKey(workspaceID: first.id, routeID: "route", presented: pin, support: support)
            XCTAssertEqual(decision, .revoked, rejectedSpelling)

            let otherPort = try await install(archive("other-port", host: canonical, port: 2222, trustID: "new-trust"),
                in: reopened, identity: 2, replacing: first)
            let otherDecision = try await reopened.verifyHostKey(workspaceID: otherPort.id, routeID: "route", presented: pin, support: support)
            XCTAssertEqual(otherDecision, .trustedPin, "The rejection belongs to one host/port endpoint.")
            let readiness = try await reopened.routeReadiness(workspaceID: otherPort.id, support: support)
            XCTAssertTrue(try XCTUnwrap(readiness.first).isLocallyReady)

            let restoredPort = try await install(archive("original-port", host: rejectedSpelling, trustID: "another-trust"),
                in: reopened, identity: 3, replacing: otherPort)
            let restoredDecision = try await reopened.verifyHostKey(workspaceID: restoredPort.id, routeID: "route", presented: pin, support: support)
            XCTAssertEqual(restoredDecision, .revoked)
            XCTAssertEqual(vault.creates, 1)
        }
    }

    func testImportedNegativeTrustAndWorkspacePublishInTheSameGeneration() async throws {
        for (fault, published) in [(StoreTestPersistence.Fault.before(.journal), false), (.after(.journal), false),
                                  (.before(.catalog), false), (.after(.catalog), true)] {
            let persistence = StoreTestPersistence(), vault = StoreTestVault()
            let store = WorkspaceStore(persistence: persistence, vault: vault)
            let pin = try WorkspaceDocument.HostKey(algorithm: "ssh-ed25519", fingerprint: hostFingerprint)
            let first = try await install(archive("positive"), in: store, identity: 1)
            persistence.configure(fault)
            do {
                _ = try await install(archive("negative", mutate: addRevocation), in: store, identity: 2, replacing: first)
                XCTFail("The requested publication boundary did not fail.")
            } catch {}
            let reopened = WorkspaceStore(persistence: persistence, vault: vault)
            let saved = try await reopened.load()
            let current = try XCTUnwrap(saved.first)
            XCTAssertEqual(try current.document().bundleID, published ? "negative" : "positive")
            XCTAssertEqual(current.revision == first.revision, !published)
            let decision = try await reopened.verifyHostKey(workspaceID: current.id, routeID: "route", presented: pin, support: support)
            XCTAssertEqual(decision, published ? .revoked : .trustedPin)
            let readiness = try await reopened.routeReadiness(workspaceID: current.id, support: support)
            XCTAssertEqual(readiness.first?.issues, published ? [.hostIdentityRevoked] : [])
            XCTAssertNil(persistence.bytes(.journal))

            // A failed candidate must not leak its rejection into a later valid
            // import; a published rejection must survive that same replacement.
            let next = try await install(archive("later-positive"), in: reopened, identity: 3, replacing: current)
            let nextDecision = try await reopened.verifyHostKey(workspaceID: next.id, routeID: "route", presented: pin, support: support)
            XCTAssertEqual(nextDecision, published ? .revoked : .trustedPin)
            XCTAssertEqual(vault.count, 1)
        }
    }

    func testPersistentFingerprintDenialPrecedesAlgorithmChoiceAndCertificatePolicy() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let pin = try WorkspaceDocument.HostKey(algorithm: "ssh-ed25519", fingerprint: hostFingerprint)
        func mixedPins(_ workspace: inout [String: Any], includeNegative: Bool) {
            var trusts = workspace["host_trust"] as! [[String: Any]], pins = trusts[0]["pins"] as! [[String: Any]]
            pins.append(["algorithm": "ssh-rsa", "fingerprint": otherFingerprint])
            trusts[0]["pins"] = pins
            if includeNegative {
                // A self-asserted algorithm label cannot weaken a canonical
                // fingerprint denial when that declaration leaves the manifest.
                trusts[0]["revoked_keys"] = [["algorithm": "ssh-rsa", "fingerprint": hostFingerprint]]
            }
            workspace["host_trust"] = trusts
        }
        let original = try archive("negative") { mixedPins(&$0, includeNegative: true) }
        let first = try await install(original, in: store, identity: 1)
        let replacement = try archive("replacement", trustID: "renamed-trust") { mixedPins(&$0, includeNegative: false) }
        let second = try await install(replacement, in: store, identity: 2, replacing: first)
        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        let decision = try await reopened.verifyHostKey(workspaceID: second.id, routeID: "route", presented: pin, support: support)
        XCTAssertEqual(decision, .revoked)
        let readiness = try await reopened.routeReadiness(workspaceID: second.id, support: support)
        XCTAssertEqual(readiness.first?.issues, [.hostAlgorithmUnavailable], "Only the unqualified RSA pin remains unrevoked.")
        let other = try WorkspaceDocument.HostKey(algorithm: "ssh-ed25519", fingerprint: otherFingerprint)
        let certificate = try await reopened.verifyHostKey(workspaceID: second.id, routeID: "route", presented: other,
            certificateAuthority: pin, support: support)
        XCTAssertEqual(certificate, .revoked, "A revoked CA must be refused before unsupported-certificate handling.")
    }

    func testRevokedJumpBlocksItsDependentsWithoutBlockingIndependentRoutes() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let source = try archive("jump-workspace") { workspace in
            var routes = workspace["routes"] as! [[String: Any]], trusts = workspace["host_trust"] as! [[String: Any]]
            for (id, host, trustID) in [("jump", "jump.example.invalid", "jump-trust"), ("independent", "safe.example.invalid", "safe-trust")] {
                var route = routes[0], trust = trusts[0]
                route["id"] = id; route["host"] = host; route["host_trust_ref"] = trustID
                trust["id"] = trustID; trust["host"] = host
                routes.append(route); trusts.append(trust)
            }
            routes[0]["jump_routes"] = ["jump"]
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }
        let installed = try await install(source, in: store, identity: 1)
        let pin = try WorkspaceDocument.HostKey(algorithm: "ssh-ed25519", fingerprint: hostFingerprint)
        try await store.revokeHostKeyLocally(host: "JUMP.EXAMPLE.INVALID.", port: 22, algorithm: pin.algorithm, fingerprint: pin.fingerprint)
        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        let routes = try await reopened.routeReadiness(workspaceID: installed.id, support: support)
        XCTAssertEqual(routes.first(where: { $0.routeID == "route" })?.issues, [.blockedJumpRoute("jump")])
        XCTAssertEqual(routes.first(where: { $0.routeID == "jump" })?.issues, [.hostIdentityRevoked])
        XCTAssertTrue(try XCTUnwrap(routes.first(where: { $0.routeID == "independent" })).isLocallyReady)
        let jump = try await reopened.verifyHostKey(workspaceID: installed.id, routeID: "jump", presented: pin, support: support)
        let target = try await reopened.verifyHostKey(workspaceID: installed.id, routeID: "route", presented: pin, support: support)
        XCTAssertEqual(jump, .revoked); XCTAssertEqual(target, .trustedPin)
    }

    func testVersionTwoCatalogMigrationPublishesImportedNegativeTrustBeforeReplacement() async throws {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let first = try await install(archive("old-negative", mutate: addRevocation), in: store, identity: 1)
        var oldCatalog = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(persistence.bytes(.catalog))) as? [String: Any])
        oldCatalog["version"] = 2; oldCatalog.removeValue(forKey: "hostRevocations")
        persistence.seed(try JSONSerialization.data(withJSONObject: oldCatalog, options: [.sortedKeys]), slot: .catalog)

        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        let loaded = try await reopened.load()
        XCTAssertEqual(loaded.first?.revision, first.revision)
        let migrated = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(persistence.bytes(.catalog))) as? [String: Any])
        XCTAssertEqual(migrated["version"] as? Int, 3)
        XCTAssertEqual((migrated["hostRevocations"] as? [Any])?.count, 1)
        let next = try await install(archive("new-positive", trustID: "renamed"), in: reopened, identity: 2, replacing: first)
        let pin = try WorkspaceDocument.HostKey(algorithm: "ssh-ed25519", fingerprint: hostFingerprint)
        let decision = try await reopened.verifyHostKey(workspaceID: next.id, routeID: "route", presented: pin, support: support)
        XCTAssertEqual(decision, .revoked)
        XCTAssertEqual(vault.creates, 1)
    }
}
