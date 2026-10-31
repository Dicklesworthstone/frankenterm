import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class WorkspaceSSHHostKeyMismatchTests: XCTestCase {
    private let crypto = TestCryptography()
    private let methods: [WorkspaceDocument.AuthenticationMethod] = [.publicKey, .password, .keyboardInteractive]
    private let support = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"],
        authenticationMethods: [.publicKey, .password, .keyboardInteractive],
        credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"], maximumJumpHops: 3)

    private struct Fixture {
        let store: WorkspaceStore
        let installed: InstalledWorkspace
        let persistence: StoreTestPersistence
        let vault: StoreTestVault
    }

    private func key(_ byte: UInt8, algorithm: String = "ssh-ed25519") throws -> WorkspaceDocument.HostKey {
        try .init(algorithm: algorithm,
            fingerprint: "SHA256:" + Data(repeating: byte, count: 32).base64EncodedString().replacingOccurrences(of: "=", with: ""))
    }

    private func fields(_ key: WorkspaceDocument.HostKey) -> [String: String] {
        ["algorithm": key.algorithm, "fingerprint": key.fingerprint]
    }

    private func fixture(mutate: (inout [String: Any]) -> Void = { _ in }) async throws -> Fixture {
        let (privateKey, fingerprint) = try sampleKey(using: crypto)
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest(fingerprint: fingerprint)) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        var routes = workspace["routes"] as! [[String: Any]]
        var trusts = workspace["host_trust"] as! [[String: Any]]
        routes[0]["authentication_methods"] = ["publickey", "password", "keyboard-interactive"]
        routes[0]["host"] = "actual-peer.example.invalid"
        routes[0]["port"] = 2222
        routes[0]["host_key_alias"] = "reviewed-identity.example.invalid"
        trusts[0]["host"] = routes[0]["host"]
        trusts[0]["port"] = routes[0]["port"]
        trusts[0]["host_key_alias"] = routes[0]["host_key_alias"]
        workspace["routes"] = routes
        workspace["host_trust"] = trusts
        mutate(&workspace)
        root["workspace"] = workspace
        let document = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
        let archive = try AirGapArchive(workspace: document, keys: [privateKey], using: crypto)
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let installed = try await store.install(archive, envelopeDigest: Data(repeating: 61, count: 32),
            transferID: Data(repeating: 62, count: 16), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        return .init(store: store, installed: installed, persistence: persistence, vault: vault)
    }

    private func authorize(_ method: WorkspaceDocument.AuthenticationMethod, _ f: Fixture,
        presented: WorkspaceDocument.HostKey, routeID: String = "route", transitForRouteID: String? = nil,
        revision: UUID? = nil) async throws {
        let revision = revision ?? f.installed.revision
        switch method {
        case .publicKey:
            _ = try await f.store.authorizeSSHAuthentication(workspaceID: f.installed.id, expectedRevision: revision,
                routeID: routeID, credentialID: "key", presentedHostKey: presented, support: support,
                transitForRouteID: transitForRouteID)
        case .password:
            _ = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: f.installed.id, expectedRevision: revision,
                routeID: routeID, presentedHostKey: presented, support: support, transitForRouteID: transitForRouteID)
        case .keyboardInteractive:
            _ = try await f.store.authorizeSSHKeyboardInteractiveAuthentication(workspaceID: f.installed.id, expectedRevision: revision,
                routeID: routeID, presentedHostKey: presented, support: support, transitForRouteID: transitForRouteID)
        }
    }

    private func mismatch(_ method: WorkspaceDocument.AuthenticationMethod, _ f: Fixture,
        presented: WorkspaceDocument.HostKey, routeID: String = "route",
        transitForRouteID: String? = nil) async throws -> WorkspaceSSHHostKeyMismatch {
        do {
            try await authorize(method, f, presented: presented, routeID: routeID, transitForRouteID: transitForRouteID)
            XCTFail("An unpinned peer was authorized")
            throw SSHAuthenticationError.denied
        } catch let mismatch as WorkspaceSSHHostKeyMismatch {
            return mismatch
        }
    }

    func testEveryAuthenticationMethodRetainsExactPeerWithoutChangingTrust() async throws {
        let f = try await fixture(), expected = try key(0), presented = try key(9)
        let saved = try XCTUnwrap(f.persistence.bytes(.catalog))
        for method in methods {
            let details = try await mismatch(method, f, presented: presented)
            XCTAssertEqual(details.workspaceID, f.installed.id)
            XCTAssertEqual(details.expectedRevision, f.installed.revision)
            XCTAssertEqual(details.routeID, "route")
            XCTAssertNil(details.transitForRouteID)
            XCTAssertEqual(details.host, "actual-peer.example.invalid")
            XCTAssertEqual(details.port, 2222)
            XCTAssertEqual(details.user, "operator")
            XCTAssertEqual(details.hostKeyAlias, "reviewed-identity.example.invalid")
            XCTAssertEqual(details.expectedKeys, [expected])
            XCTAssertEqual(details.presentedKey, presented)
            let connection = try WorkspaceSSHConnectionHostKeyMismatch(details,
                machineID: "build", destinationRouteID: "route")
            XCTAssertNil(connection.hopIndex)
            XCTAssertEqual(connection.machineID, "build")
            XCTAssertEqual(connection.destinationRouteID, "route")
            XCTAssertEqual(connection.workspaceID, f.installed.id)
            XCTAssertEqual(connection.expectedRevision, f.installed.revision)
            XCTAssertEqual(connection.presentedKey, presented)
            XCTAssertEqual(f.persistence.bytes(.catalog), saved)
            try await authorize(method, f, presented: expected)
        }
        XCTAssertEqual(f.vault.creates, 1)
        let decision = try await f.store.verifyHostKey(workspaceID: f.installed.id,
            routeID: "route", presented: presented, support: support)
        XCTAssertEqual(decision, .pinMismatch)
    }

    func testExpectedKeysExcludeCurrentRevocationsAndUnsupportedPins() async throws {
        let original = try key(0), permitted = try key(1), unsupported = try key(2, algorithm: "ssh-rsa")
        let f = try await fixture { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["pins"] = [fields(original), fields(permitted), fields(unsupported)]
            workspace["host_trust"] = trusts
        }
        try await f.store.revokeHostLocally(workspaceID: f.installed.id, trustID: "trust", key: original)
        for method in methods {
            let details = try await mismatch(method, f, presented: key(9))
            XCTAssertEqual(details.expectedKeys, [permitted])
            XCTAssertEqual(details.expectedRevision, f.installed.revision, "Local negative trust changes without a new import revision")
        }
    }

    func testMismatchTracksDeliberatelyRotatedPhonePinWithoutChangingImportedTrust() async throws {
        let original = try await fixture { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["pins"] = [] as [[String: String]]
            workspace["host_trust"] = trusts
        }
        let firstKey = try key(1), replacementKey = try key(2), observedKey = try key(9)
        let firstReview = try await original.store.reviewHostKeyPin(workspaceID: original.installed.id,
            expectedRevision: original.installed.revision, routeID: "route", key: firstKey, support: support)
        let first = try await original.store.acceptHostKeyPin(firstReview, confirmedByUser: true, support: support)
        let firstFixture = Fixture(store: original.store, installed: first,
            persistence: original.persistence, vault: original.vault)
        XCTAssertNotEqual(first.revision, original.installed.revision)
        let firstMismatch = try await mismatch(.password, firstFixture, presented: observedKey)
        XCTAssertEqual(firstMismatch.expectedRevision, first.revision)
        XCTAssertEqual(firstMismatch.expectedKeys, [firstKey])
        XCTAssertEqual(firstMismatch.presentedKey, observedKey)

        // A mismatch grants no rotation authority. Independently review the
        // replacement against the exact previous phone pin and current revision.
        let previousPin = try XCTUnwrap(first.phoneHostKeyPin(forRouteID: "route"))
        let replacementReview = try await original.store.reviewHostKeyPin(workspaceID: first.id,
            expectedRevision: first.revision, routeID: "route", key: replacementKey,
            support: support, replacing: previousPin)
        let replacement = try await original.store.acceptHostKeyPin(replacementReview,
            confirmedByUser: true, support: support)
        XCTAssertNotEqual(replacement.revision, first.revision)
        do {
            try await authorize(.password, firstFixture, presented: observedKey)
            XCTFail("A pre-rotation revision retained authentication authority")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }

        let replacementFixture = Fixture(store: original.store, installed: replacement,
            persistence: original.persistence, vault: original.vault)
        let nextMismatch = try await mismatch(.password, replacementFixture, presented: observedKey)
        XCTAssertEqual(nextMismatch.expectedRevision, replacement.revision)
        XCTAssertEqual(nextMismatch.expectedKeys, [replacementKey])
        XCTAssertEqual(nextMismatch.presentedKey, observedKey)
        XCTAssertEqual(firstMismatch.expectedKeys, [firstKey], "Captured details remain bound to their failed exchange")
        XCTAssertEqual(first.manifest, original.installed.manifest)
        XCTAssertEqual(replacement.manifest, original.installed.manifest)
        XCTAssertTrue(try replacement.importedDocument().hostTrust[0].pins.isEmpty)
        XCTAssertEqual(try replacement.document().hostTrust[0].pins, [replacementKey])
        try await authorize(.password, replacementFixture, presented: replacementKey)
    }

    func testRelatedTrustDoesNotSupplyPositivePinsForSelectedRoute() async throws {
        let otherPin = try key(3)
        let f = try await fixture { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            var alias = trusts[0]
            alias["id"] = "related-trust"
            alias["pins"] = [fields(otherPin)]
            trusts.append(alias)
            workspace["host_trust"] = trusts
        }
        for method in methods {
            let details = try await mismatch(method, f, presented: otherPin)
            XCTAssertEqual(details.expectedKeys, [try key(0)])
            XCTAssertEqual(details.presentedKey, otherPin)
        }
    }

    func testRevokedUnsupportedAndUnconfiguredPeersStayDenied() async throws {
        let presented = try key(9)
        for variant in 0..<6 {
            let f = try await fixture { workspace in
                var trusts = workspace["host_trust"] as! [[String: Any]]
                var routes = workspace["routes"] as! [[String: Any]]
                if variant == 0 { trusts[0]["revoked_keys"] = [fields(presented)] }
                if variant == 1 { routes[0]["required_capabilities"] = ["missing.route.policy"] }
                if variant == 2 { trusts[0]["required_capabilities"] = ["missing.host.policy"] }
                if variant == 3 { trusts[0]["pins"] = [] as [[String: String]] }
                workspace["host_trust"] = trusts
                workspace["routes"] = routes
            }
            let observed = variant == 4 ? try key(9, algorithm: "ssh-rsa")
                : variant == 5 ? try key(9, algorithm: "ssh-ed25519-cert-v01@openssh.com") : presented
            for method in methods {
                do {
                    try await authorize(method, f, presented: observed)
                    XCTFail("A forbidden peer was authorized")
                } catch {
                    XCTAssertEqual(error as? SSHAuthenticationError, .denied,
                        "Revoked, unsupported and missing trust must not become changed-key reviews")
                }
            }
        }
        let f = try await fixture()
        try await f.store.revokeHostLocally(workspaceID: f.installed.id, trustID: "trust", key: presented)
        for method in methods {
            do {
                try await authorize(method, f, presented: presented)
                XCTFail("A locally revoked peer was authorized")
            } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
        }
    }

    func testStaleRevisionAndUnrelatedTransitNeverProduceMismatchDetails() async throws {
        let f = try await fixture(), presented = try key(9)
        for method in methods {
            do {
                try await authorize(method, f, presented: presented, revision: UUID())
                XCTFail("A stale workspace was authorized")
            } catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
            do {
                try await authorize(method, f, presented: presented, transitForRouteID: "route")
                XCTFail("An unrelated transit scope was authorized")
            } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
        }
    }

    func testEveryTransitOccurrenceRetainsDestinationAndActualHop() async throws {
        let f = try await fixture { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            var trusts = workspace["host_trust"] as! [[String: Any]]
            for index in 1...3 {
                var route = routes[0], trust = trusts[0]
                route["id"] = "hop\(index)"
                route["host"] = "hop\(index).example.invalid"
                route["user"] = "hop-user\(index)"
                route["port"] = 2200 + index
                route["host_key_alias"] = "hop-identity\(index).example.invalid"
                route["host_trust_ref"] = "hop-trust\(index)"
                trust["id"] = route["host_trust_ref"]
                trust["host"] = route["host"]
                trust["port"] = route["port"]
                trust["host_key_alias"] = route["host_key_alias"]
                routes.append(route); trusts.append(trust)
            }
            routes[0]["jump_routes"] = ["hop1", "hop2", "hop3"]
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }
        for method in methods {
            for index in 0..<3 {
                let details = try await mismatch(method, f, presented: key(9),
                    routeID: "hop\(index + 1)", transitForRouteID: "route")
                let connection = try WorkspaceSSHConnectionHostKeyMismatch(details,
                    machineID: "build", destinationRouteID: "route", hopIndex: index)
                XCTAssertEqual(connection.routeID, "hop\(index + 1)")
                XCTAssertEqual(connection.destinationRouteID, "route")
                XCTAssertEqual(connection.hopIndex, index)
                XCTAssertEqual(connection.host, "hop\(index + 1).example.invalid")
                XCTAssertEqual(connection.port, UInt16(2201 + index))
                XCTAssertEqual(connection.user, "hop-user\(index + 1)")
                XCTAssertEqual(connection.hostKeyAlias, "hop-identity\(index + 1).example.invalid")
                XCTAssertEqual(connection.expectedKeys, [try key(0)])
            }
            let leaf = try await mismatch(method, f, presented: key(9))
            XCTAssertNil(try WorkspaceSSHConnectionHostKeyMismatch(leaf,
                machineID: "build", destinationRouteID: "route").hopIndex)
        }
    }

    func testConnectionContextRejectsWrongRoleAndKeepsRepeatedOccurrencesDistinct() async throws {
        let f = try await fixture(), presented = try key(9)
        let destination = try await mismatch(.password, f, presented: presented)
        let route = try XCTUnwrap(f.installed.document().routes.first)
        let transit = WorkspaceSSHHostKeyMismatch(workspaceID: f.installed.id,
            expectedRevision: f.installed.revision, route: route, transitForRouteID: "destination",
            expectedKeys: [try key(0)], presentedKey: presented)
        let first = try WorkspaceSSHConnectionHostKeyMismatch(transit,
            machineID: "build", destinationRouteID: "destination", hopIndex: 0)
        let repeated = try WorkspaceSSHConnectionHostKeyMismatch(transit,
            machineID: "build", destinationRouteID: "destination", hopIndex: 1)
        XCTAssertNotEqual(first, repeated)
        XCTAssertEqual(first.routeID, repeated.routeID)
        XCTAssertThrowsError(try WorkspaceSSHConnectionHostKeyMismatch(destination,
            machineID: "build", destinationRouteID: "other"))
        XCTAssertThrowsError(try WorkspaceSSHConnectionHostKeyMismatch(destination,
            machineID: "build", destinationRouteID: "route", hopIndex: 0))
        for index in [nil, -1, 3] as [Int?] {
            XCTAssertThrowsError(try WorkspaceSSHConnectionHostKeyMismatch(transit,
                machineID: "build", destinationRouteID: "destination", hopIndex: index))
        }
        XCTAssertThrowsError(try WorkspaceSSHConnectionHostKeyMismatch(transit,
            machineID: "build", destinationRouteID: "other", hopIndex: 0))
        XCTAssertThrowsError(try WorkspaceSSHConnectionHostKeyMismatch(destination,
            machineID: "invalid machine", destinationRouteID: "route"))
    }

    func testGenericErrorFormattingDoesNotDisclosePeerDetails() async throws {
        let f = try await fixture()
        let peer = try await mismatch(.keyboardInteractive, f, presented: key(9))
        let connection = try WorkspaceSSHConnectionHostKeyMismatch(peer,
            machineID: "build", destinationRouteID: "route")
        let messages = [String(describing: peer), String(reflecting: peer), peer.localizedDescription,
            String(describing: connection), String(reflecting: connection), connection.localizedDescription,
            (peer as NSError).localizedDescription, (connection as NSError).localizedDescription]
        for message in messages {
            for field in [peer.workspaceID.uuidString, peer.expectedRevision.uuidString, peer.host, peer.user,
                          try XCTUnwrap(peer.hostKeyAlias), peer.presentedKey.fingerprint,
                          try XCTUnwrap(peer.expectedKeys.first).fingerprint] {
                XCTAssertFalse(message.contains(field))
            }
        }
    }
}
