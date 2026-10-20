import Foundation
import XCTest
@testable import FrankenTermKit

final class WorkspaceExportTests: XCTestCase {
    private let crypto = TestCryptography()

    private func fixture(aliasAlgorithm: String = "ssh-ed25519", aliasFingerprint: String? = nil) throws -> WorkspaceDocument {
        let (_, identity) = try sampleKey(using: crypto)
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest(fingerprint: identity)) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        var source = try XCTUnwrap(root["source"] as? [String: Any])
        source["export_sequence"] = NSNumber(value: UInt64.max)
        root["source"] = source
        workspace["groups"] = [
            ["id": "all", "name": "All machines", "machine_order": ["archive", "gpu", "build"], "auto_connect": false],
            ["id": "selected", "name": "Build fleet", "machine_order": ["build", "gpu"], "auto_connect": true],
            ["id": "other", "name": "Archive", "machine_order": ["archive"], "auto_connect": false],
            ["id": "empty", "name": "Empty", "machine_order": [], "auto_connect": false]
        ]
        workspace["machines"] = [
            ["id": "gpu", "name": "GPU", "theme_ref": "theme", "route_refs": ["gpu-route"], "preferred_route_ref": "gpu-route", "auto_connect": false],
            ["id": "archive", "name": "Archive", "theme_ref": "unused-theme", "route_refs": ["archive-route"], "preferred_route_ref": "archive-route", "auto_connect": true],
            ["id": "build", "name": "Build", "theme_ref": "theme", "route_refs": ["build-route", "fallback-route"], "preferred_route_ref": "build-route", "auto_connect": true]
        ]
        func route(_ id: String, credentials: [String], hops: [String] = [], methods: [String] = ["publickey"], required: [String] = []) -> [String: Any] {
            ["id": id, "mode": "direct_ssh", "host": id + ".example.invalid", "port": 22, "user": "operator",
             "credential_refs": credentials, "authentication_methods": methods, "host_trust_ref": id + "-trust",
             "jump_routes": hops, "session_creation": "on_select", "required_capabilities": required]
        }
        let routes = [
            route("second-hop", credentials: ["jump-key"]),
            route("archive-route", credentials: ["key", "unselected-only"]),
            route("build-route", credentials: ["key-alias", "alternative-key"], hops: ["first-hop"], methods: ["publickey", "keyboard-interactive"]),
            route("fallback-route", credentials: [], methods: ["password"], required: ["reviewed-password-policy"]),
            route("gpu-route", credentials: ["key-alias"]),
            route("first-hop", credentials: ["jump-key"], hops: ["second-hop"])
        ]
        workspace["routes"] = routes
        func fingerprint(_ byte: UInt8) -> String {
            "SHA256:" + Data(repeating: byte, count: 32).base64EncodedString().replacingOccurrences(of: "=", with: "")
        }
        func credential(_ id: String, fingerprint: String, algorithm: String = "ssh-ed25519") -> [String: Any] {
            ["id": id, "algorithm": algorithm, "public_fingerprint": fingerprint,
             "requirement": "matching_private_key", "private_material": "not_included"]
        }
        workspace["credentials"] = [
            credential("key", fingerprint: identity),
            credential("alternative-key", fingerprint: fingerprint(1)),
            credential("unselected-only", fingerprint: fingerprint(2)),
            credential("key-alias", fingerprint: aliasFingerprint ?? identity, algorithm: aliasAlgorithm),
            credential("jump-key", fingerprint: fingerprint(3))
        ]
        workspace["host_trust"] = routes.map { route -> [String: Any] in
            ["id": route["host_trust_ref"]!, "host": route["host"]!, "port": route["port"]!,
             "claimed_source": "mac_known_hosts", "pins": [["algorithm": "ssh-ed25519", "fingerprint": fingerprint(4)]],
             "revoked_keys": [["algorithm": "ssh-ed25519", "fingerprint": fingerprint(5)]]]
        }
        let themes = try XCTUnwrap(workspace["themes"] as? [[String: Any]])
        var unused = try XCTUnwrap(themes.first)
        unused["id"] = "unused-theme"; unused["name"] = "Not selected"
        workspace["themes"] = [unused] + themes
        workspace["preferences"] = ["initial_machine_ref": "archive", "initial_group_ref": "all", "connect_on_open": true]
        root["workspace"] = workspace
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }

    func testSelectionIncludesTheCompleteRouteClosureAndPreservesOrderAndPolicy() throws {
        let original = try fixture(), raw = original.raw
        let selection = try WorkspaceExportSelection(workspace: original, groupID: "selected")
        let selected = selection.document
        XCTAssertEqual(selected.groups.map(\.id), ["selected"])
        XCTAssertEqual(selected.groups[0].machineOrder, ["build", "gpu"])
        XCTAssertTrue(selected.groups[0].autoConnect)
        XCTAssertEqual(selection.orderedMachines.map(\.id), ["build", "gpu"])
        XCTAssertEqual(selected.machines.map(\.id), ["gpu", "build"], "retain record order separately from navigation order")
        XCTAssertEqual(selected.routes.map(\.id), ["second-hop", "build-route", "fallback-route", "gpu-route", "first-hop"])
        XCTAssertEqual(selected.credentials.map(\.id), ["alternative-key", "key-alias", "jump-key"])
        XCTAssertEqual(selected.themes.map(\.id), ["theme"])
        XCTAssertEqual(selected.hostTrust.map(\.id), selected.routes.map { $0.id + "-trust" })
        XCTAssertEqual(selected.routes.first(where: { $0.id == "build-route" })?.credentialIDs, ["key-alias", "alternative-key"])
        XCTAssertEqual(selected.routes.first(where: { $0.id == "build-route" })?.authenticationMethods, [.publicKey, .keyboardInteractive])
        XCTAssertEqual(selected.routes.first(where: { $0.id == "fallback-route" })?.requiredCapabilities, ["reviewed-password-policy"])
        XCTAssertEqual(selected.initialGroupID, "selected")
        XCTAssertEqual(selected.initialMachineID, "build")
        XCTAssertEqual(selected.exporterID, original.exporterID)
        XCTAssertEqual(selected.id, original.id)
        XCTAssertNotEqual(selected.bundleID, original.bundleID)
        XCTAssertEqual(original.raw, raw, "selection never mutates the source snapshot")

        let before = try StrictJSON.decode(original.raw).object()
        let after = try StrictJSON.decode(selected.raw).object()
        XCTAssertEqual(before["source"], after["source"])
        XCTAssertEqual(try after.required("source").object().required("export_sequence").uint(), UInt64.max)
        let beforeWorkspace = try before.required("workspace").object()
        let afterWorkspace = try after.required("workspace").object()
        for field in ["groups", "machines", "routes", "credentials", "host_trust", "themes"] {
            let originalRecords = try beforeWorkspace.required(field).array()
            for retained in try afterWorkspace.required(field).array() {
                XCTAssertTrue(originalRecords.contains(retained), "\(field) lost an original policy or palette value")
            }
        }
    }

    func testInitialSelectionIsKeptWhenItBelongsToTheChosenGroup() throws {
        let selection = try WorkspaceExportSelection(workspace: fixture(), groupID: "other")
        XCTAssertEqual(selection.document.initialMachineID, "archive")
        XCTAssertEqual(selection.document.initialGroupID, "other")
        XCTAssertEqual(selection.orderedMachines.map(\.id), ["archive"])
        XCTAssertThrowsError(try WorkspaceExportSelection(workspace: fixture(), groupID: "empty"))
        XCTAssertThrowsError(try WorkspaceExportSelection(workspace: fixture(), groupID: "absent"))
    }

    func testSelectionAndPreparationPreserveExactFontNumberTokens() throws {
        var root = try StrictJSON.decode(fixture().raw).object()
        var content = try root.required("workspace").object()
        var themes = try content.required("themes").array()
        let index = try XCTUnwrap(themes.firstIndex { try $0.object().text("id") == "theme" })
        var theme = try themes[index].object()
        let font = JSONValue.object([
            "family": .string("Monospace"), "point_size": .number("13.5000"),
            "variations": .object(["wght": .number("4.5e2"), "opsz": .number("1.234567890123456789")])
        ])
        theme["font"] = font; themes[index] = .object(theme)
        content["themes"] = .array(themes); root["workspace"] = .object(content)
        let source = try WorkspaceDocument(data: StrictJSON.encode(.object(root)))
        let selection = try WorkspaceExportSelection(workspace: source, groupID: "selected")
        let prepared = try selection.archive(including: [], using: crypto)
        for document in [selection.document, prepared.workspace] {
            let exported = try StrictJSON.decode(document.raw).object().required("workspace").object()
                .required("themes").array()
            XCTAssertEqual(try XCTUnwrap(exported.first).object().required("font"), font)
        }
    }

    func testLoadedKeyIsReboundToSelectedAliasWithoutDuplicatingPrivateMaterial() throws {
        let original = try fixture(), selection = try WorkspaceExportSelection(workspace: original, groupID: "selected")
        let (key, _) = try sampleKey(using: crypto)
        let selected = try selection.selectedKeys(from: [key], using: crypto)
        XCTAssertEqual(selected.count, 1)
        XCTAssertEqual(selected.first?.credentialID, "key-alias")
        XCTAssertEqual(selected.first?.container, key.container)
        XCTAssertEqual(selected.first?.publicBlob, key.publicBlob)
        XCTAssertFalse(selection.document.credentials.contains { $0.id == key.credentialID })
        let archive = try selection.archive(including: [key], using: crypto)
        XCTAssertEqual(archive.keys.map(\.credentialID), ["key-alias"])
        XCTAssertEqual(archive.workspace.groups.map(\.id), ["selected"])
    }

    func testUnselectedKeysAreExcludedAndContradictoryAliasesAreRejected() throws {
        XCTAssertThrowsError(try fixture(aliasAlgorithm: "ecdsa-sha2-nistp256"))
        let unrelated = "SHA256:" + Data(repeating: 42, count: 32).base64EncodedString().replacingOccurrences(of: "=", with: "")
        let source = try fixture(aliasFingerprint: unrelated)
        let selection = try WorkspaceExportSelection(workspace: source, groupID: "selected")
        let (key, _) = try sampleKey(using: crypto)
        XCTAssertTrue(try selection.selectedKeys(from: [key], using: crypto).isEmpty)
        let archive = try selection.archive(including: [key], using: crypto)
        XCTAssertTrue(archive.keys.isEmpty)
        XCTAssertFalse(archive.workspace.credentials.contains { $0.id == "key" })
        XCTAssertNil(try archive.encoded().range(of: key.container), "unselected private material cannot reach the archive")
    }

    func testForgedAndDuplicateLoadedKeysAreRejectedBeforeSelection() throws {
        let selection = try WorkspaceExportSelection(workspace: fixture(), groupID: "selected")
        let (key, _) = try sampleKey(using: crypto)
        let forged = PrivateKeyRecord(credentialID: key.credentialID, container: key.container, publicBlob: Data(repeating: 0, count: key.publicBlob.count))
        XCTAssertThrowsError(try selection.selectedKeys(from: [forged], using: crypto))
        XCTAssertThrowsError(try selection.selectedKeys(from: [key, key], using: crypto))
        let stale = PrivateKeyRecord(credentialID: "removed-credential", container: key.container, publicBlob: key.publicBlob)
        XCTAssertThrowsError(try selection.selectedKeys(from: [stale], using: crypto))
    }

    func testEveryPreparationGetsANewBundleIDAndDecryptsToTheSelectedGroup() throws {
        let selection = try WorkspaceExportSelection(workspace: fixture(), groupID: "selected")
        let (key, _) = try sampleKey(using: crypto)
        let first = try selection.archive(including: [key], using: crypto)
        let second = try selection.archive(including: [key], using: crypto)
        XCTAssertNotEqual(first.workspace.bundleID, second.workspace.bundleID)
        XCTAssertNotEqual(first.workspace.bundleID, selection.document.bundleID)
        let exported = try FTAG.seal(first, using: crypto)
        let restored = try FTAG.open(exported.envelope, code: exported.code, using: crypto)
        XCTAssertEqual(restored.workspace.bundleID, first.workspace.bundleID)
        XCTAssertEqual(restored.workspace.groups.map(\.id), ["selected"])
        XCTAssertEqual(restored.workspace.groups[0].machineOrder, ["build", "gpu"])
        XCTAssertEqual(restored.keys.map(\.credentialID), ["key-alias"])
    }
}

@MainActor
final class WorkspaceMultiGroupExportTests: XCTestCase {
    private let crypto = TestCryptography()
    private var support: WorkspaceDocument.RouteSupport {
        .init(routeModes: ["direct_ssh", "frankenterm_mux"], authenticationMethods: [.publicKey, .password, .keyboardInteractive],
              credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"])
    }

    private func fixture(fingerprint: String = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                         change: ((inout [String: Any]) -> Void)? = nil) throws -> WorkspaceDocument {
        var root = try JSONSerialization.jsonObject(with: sampleManifest(fingerprint: fingerprint)) as! [String: Any]
        root["source"] = ["exporter_id": "mac", "kind": "mac_snapshot", "configuration_revision": "frozen-7", "export_sequence": 7]
        var workspace = root["workspace"] as! [String: Any]
        workspace["groups"] = [
            ["id": "gpu-group", "name": "GPU", "machine_order": ["gpu"], "auto_connect": false],
            ["id": "build-group", "name": "Build", "machine_order": ["build"], "auto_connect": true],
            ["id": "both-group", "name": "Both", "machine_order": ["gpu", "build"], "auto_connect": true],
            ["id": "empty-group", "name": "Empty", "machine_order": [], "auto_connect": false]
        ]
        var machines = workspace["machines"] as! [[String: Any]]
        machines[0]["route_refs"] = ["route", "alternate"]
        machines[1]["route_refs"] = ["unused-route"]; machines[1]["preferred_route_ref"] = "unused-route"
        machines[1]["theme_ref"] = "unused-theme"
        workspace["machines"] = machines
        var route = (workspace["routes"] as! [[String: Any]])[0]
        route["credential_refs"] = ["hop-key", "selected-key"]; route["jump_routes"] = ["hop-one"]
        var alternate = route; alternate["id"] = "alternate"; alternate["jump_routes"] = []
        alternate["credential_refs"] = ["selected-key"]
        var first = route; first["id"] = "hop-one"; first["host"] = "bastion-one.invalid"
        first["host_trust_ref"] = "trust-one"; first["jump_routes"] = ["hop-two"]; first["credential_refs"] = ["hop-key"]
        var second = first; second["id"] = "hop-two"; second["host"] = "bastion-two.invalid"
        second["host_trust_ref"] = "trust-two"; second["jump_routes"] = []; second["credential_refs"] = ["selected-key"]
        var unused = alternate; unused["id"] = "unused-route"; unused["host"] = "unused.invalid"
        unused["host_trust_ref"] = "unused-trust"; unused["credential_refs"] = ["unused-key"]
        workspace["routes"] = [unused, second, route, first, alternate]
        let key = (workspace["credentials"] as! [[String: Any]])[0]
        var selected = key; selected["id"] = "selected-key"
        var hop = key; hop["id"] = "hop-key"
        var unusedKey = key; unusedKey["id"] = "unused-key"
        unusedKey["public_fingerprint"] = "SHA256:ERERERERERERERERERERERERERERERERERERERERERE"
        workspace["credentials"] = [unusedKey, selected, key, hop]
        let trust = (workspace["host_trust"] as! [[String: Any]])[0]
        var firstTrust = trust; firstTrust["id"] = "trust-one"; firstTrust["host"] = "bastion-one.invalid"
        var secondTrust = trust; secondTrust["id"] = "trust-two"; secondTrust["host"] = "bastion-two.invalid"
        var unusedTrust = trust; unusedTrust["id"] = "unused-trust"; unusedTrust["host"] = "unused.invalid"
        workspace["host_trust"] = [unusedTrust, secondTrust, trust, firstTrust]
        let theme = (workspace["themes"] as! [[String: Any]])[0]
        var unusedTheme = theme; unusedTheme["id"] = "unused-theme"; unusedTheme["name"] = "Unused"
        workspace["themes"] = [unusedTheme, theme]
        workspace["preferences"] = ["initial_machine_ref": "gpu", "initial_group_ref": "gpu-group", "connect_on_open": true]
        change?(&workspace); root["workspace"] = workspace
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }

    func testSelectedGroupKeepsEveryRouteAndTransitiveHopInSourceOrder() async throws {
        let source = try fixture(), selection = try WorkspaceExportSelection(workspace: source, groupIDs: ["build-group"])
        let document = selection.workspace
        XCTAssertEqual(document.id, source.id); XCTAssertEqual(document.exporterID, source.exporterID)
        XCTAssertEqual(document.configurationRevision, "frozen-7"); XCTAssertEqual(document.exportSequence, 7)
        XCTAssertEqual(document.groups.map(\.id), ["build-group"])
        XCTAssertEqual(document.groups[0].machineOrder, ["build"]); XCTAssertTrue(document.groups[0].autoConnect)
        XCTAssertEqual(document.machines.map(\.id), ["build"])
        XCTAssertEqual(document.machines[0].routeIDs, ["route", "alternate"])
        XCTAssertEqual(document.routes.map(\.id), ["hop-two", "route", "hop-one", "alternate"])
        XCTAssertEqual(document.routes.first(where: { $0.id == "route" })?.credentialIDs, ["hop-key", "selected-key"])
        XCTAssertEqual(document.credentials.map(\.id), ["selected-key", "hop-key"])
        XCTAssertEqual(document.hostTrust.map(\.id), ["trust-two", "trust", "trust-one"])
        XCTAssertEqual(document.themes.map(\.id), ["theme"])
        XCTAssertEqual(document.initialGroupID, "build-group"); XCTAssertEqual(document.initialMachineID, "build")
        XCTAssertTrue(document.connectOnOpen, "connection intent stays inert public data")
        XCTAssertEqual(selection.identities.count, 1)
        XCTAssertEqual(selection.identities[0].credentialIDs, ["selected-key", "hop-key"])
        XCTAssertNil(document.raw.range(of: Data("unused.invalid".utf8)))
    }

    func testMultipleGroupsPreserveOrderAndExistingInitialSelection() async throws {
        let source = try fixture()
        let selection = try WorkspaceExportSelection(workspace: source, groupIDs: ["both-group", "gpu-group"])
        XCTAssertEqual(selection.workspace.groups.map(\.id), ["gpu-group", "both-group"])
        XCTAssertEqual(selection.workspace.groups[1].machineOrder, ["gpu", "build"])
        XCTAssertEqual(selection.workspace.initialGroupID, "gpu-group"); XCTAssertEqual(selection.workspace.initialMachineID, "gpu")
        XCTAssertEqual(selection.workspace.raw, try WorkspaceExportSelection(workspace: source, groupIDs: ["gpu-group", "both-group"]).workspace.raw)
        XCTAssertThrowsError(try WorkspaceExportSelection(workspace: source, groupIDs: []))
        XCTAssertThrowsError(try WorkspaceExportSelection(workspace: source, groupIDs: ["unknown"]))
        XCTAssertThrowsError(try WorkspaceExportSelection(workspace: source, groupIDs: ["empty-group"]))
    }

    func testPruningCannotEraseSameScopeNegativeTrustOrMandatoryExtension() async throws {
        let source = try fixture { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]], detached = trusts[2]
            detached["id"] = "detached-negative"; detached["revoked_keys"] = detached["pins"]
            detached["extensions"] = ["org.example.krl": ["required": true, "opaque_private": "removed"]]
            trusts.append(detached); workspace["host_trust"] = trusts
        }
        let selection = try WorkspaceExportSelection(workspace: source, groupIDs: ["build-group"])
        let document = selection.workspace, fingerprint = selection.identities[0].fingerprint
        XCTAssertTrue(document.hostTrust.contains(where: { $0.id == "detached-negative" }))
        let readiness = try document.readiness(forRouteID: "route", availableCredentialIDs: Set(document.credentials.map(\.id)),
            reviewedTrustIDs: Set(document.hostTrust.map(\.id)), support: support)
        XCTAssertTrue(readiness.issues.contains(.hostIdentityRevoked)); XCTAssertTrue(readiness.issues.contains(.unsupportedRequirement))
        let reviewed = try selection.review(decisions: [fingerprint: .keepReferenceOnly], loadedFingerprints: [])
        XCTAssertTrue(reviewed.blockedRouteIDs.contains("route"))
        let archive = try selection.archive(decisions: [fingerprint: .keepReferenceOnly], keys: [], bundleID: "narrower-export", using: crypto)
        XCTAssertTrue(archive.portability.contains(where: { $0.entityID == "route" && $0.disposition == 1 }))
        XCTAssertNil(archive.workspace.raw.range(of: Data("opaque_private".utf8)))
        XCTAssertEqual(archive.workspace.hostTrust.last?.requiredPolicyRequirements, ["extension:org.example.krl"])
    }

    func testCanonicalIdentityConsentDeduplicatesAndRebindsAnUnselectedSourceAlias() async throws {
        let (key, fingerprint) = try sampleKey(using: crypto)
        let source = try fixture(fingerprint: fingerprint), selection = try WorkspaceExportSelection(workspace: source, groupIDs: ["build-group"])
        XCTAssertFalse(selection.workspace.credentials.contains(where: { $0.id == key.credentialID }))
        let archive = try selection.archive(decisions: [fingerprint: .includePrivateKey], keys: [key], bundleID: "selected-export", using: crypto)
        XCTAssertEqual(archive.keys.count, 1); XCTAssertEqual(archive.keys[0].credentialID, "selected-key")
        XCTAssertEqual(archive.keys[0].publicBlob, key.publicBlob)
        XCTAssertEqual(archive.workspace.bundleID, "selected-export")
        XCTAssertEqual(archive.workspace.id, source.id)
        XCTAssertNil(archive.workspace.raw.range(of: key.container))
        XCTAssertTrue(archive.portability.isEmpty)
        let sealed = try FTAG.seal(archive, using: crypto), decoded = try FTAG.open(sealed.envelope, code: sealed.code, using: crypto)
        XCTAssertEqual(decoded.workspace.raw, archive.workspace.raw)
        XCTAssertEqual(decoded.keys[0].container, key.container)
        XCTAssertEqual(decoded.workspace.routes.first(where: { $0.id == "route" })?.credentialIDs, ["hop-key", "selected-key"])
    }

    func testEveryIdentityNeedsExplicitDecisionAndActualApprovedKeyMaterial() async throws {
        let (key, fingerprint) = try sampleKey(using: crypto)
        let selection = try WorkspaceExportSelection(workspace: fixture(fingerprint: fingerprint), groupIDs: ["build-group"])
        XCTAssertFalse(try selection.review(decisions: [:], loadedFingerprints: []).canPrepare)
        XCTAssertEqual(try selection.review(decisions: [fingerprint: .includePrivateKey], loadedFingerprints: []).missingApprovedKeyCount, 1)
        XCTAssertThrowsError(try selection.archive(decisions: [:], keys: [], bundleID: "candidate", using: crypto))
        XCTAssertThrowsError(try selection.archive(decisions: [fingerprint: .includePrivateKey], keys: [], bundleID: "candidate", using: crypto))
        XCTAssertThrowsError(try selection.archive(decisions: [fingerprint: .keepReferenceOnly], keys: [key], bundleID: "candidate", using: crypto))
        XCTAssertThrowsError(try selection.archive(decisions: [fingerprint: .includePrivateKey], keys: [key, key], bundleID: "candidate", using: crypto))
        XCTAssertThrowsError(try selection.archive(decisions: [fingerprint: .includePrivateKey], keys: [key], bundleID: selection.workspace.bundleID, using: crypto))
        XCTAssertThrowsError(try selection.review(decisions: ["unselected": .keepReferenceOnly], loadedFingerprints: []))
        let forged = PrivateKeyRecord(credentialID: key.credentialID, container: key.container, publicBlob: Data(repeating: 0, count: key.publicBlob.count))
        XCTAssertThrowsError(try selection.archive(decisions: [fingerprint: .includePrivateKey], keys: [forged], bundleID: "candidate", using: crypto))
    }

    func testOmittedIdentitiesHaveConcreteFindingsAndCanUseExistingPhoneKeys() async throws {
        let selection = try WorkspaceExportSelection(workspace: fixture(), groupIDs: ["build-group"])
        let fingerprint = selection.identities[0].fingerprint
        let review = try selection.review(decisions: [fingerprint: .keepReferenceOnly], loadedFingerprints: [])
        XCTAssertTrue(review.canPrepare); XCTAssertEqual(review.omittedIdentityCount, 1)
        XCTAssertEqual(Set(review.missingIdentityRouteIDs), Set(selection.workspace.routes.map(\.id)))
        let archive = try selection.archive(decisions: [fingerprint: .keepReferenceOnly], keys: [], bundleID: "reference-only", using: crypto)
        XCTAssertTrue(archive.keys.isEmpty)
        XCTAssertEqual(archive.portability.filter { $0.setting == "private_identity" }.map(\.entityID), ["selected-key", "hop-key"])
        XCTAssertTrue(archive.portability.allSatisfy { $0.disposition == 0 })
        let document = archive.workspace
        XCTAssertTrue(try document.readiness(forRouteID: "route", availableCredentialIDs: Set(document.credentials.map(\.id)),
            reviewedTrustIDs: Set(document.hostTrust.map(\.id)), support: support).isLocallyReady)
        let persisted = try AirGapArchive(data: archive.encoded(), using: crypto)
        XCTAssertEqual(persisted.portability.map(\.reason), archive.portability.map(\.reason))
    }

    func testInteractivePolicyDoesNotBecomeAFalseMissingKeyBlocker() async throws {
        var root = try JSONSerialization.jsonObject(with: sampleManifest()) as! [String: Any]
        var workspace = root["workspace"] as! [String: Any], routes = workspace["routes"] as! [[String: Any]]
        routes[0]["authentication_methods"] = ["publickey", "keyboard-interactive"]
        workspace["routes"] = routes; root["workspace"] = workspace
        let selection = try WorkspaceExportSelection(workspace: WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root)), groupIDs: ["group"])
        let reviewed = try selection.review(decisions: [selection.identities[0].fingerprint: .keepReferenceOnly], loadedFingerprints: [])
        XCTAssertTrue(reviewed.missingIdentityRouteIDs.isEmpty)
        XCTAssertTrue(reviewed.blockedRouteIDs.isEmpty)
        XCTAssertEqual(reviewed.findings.map(\.setting), ["private_identity"])
    }

    func testOmittedCredentialPolicyStaysVisibleWithoutBlockingAUsableAlternative() async throws {
        var root = try JSONSerialization.jsonObject(with: sampleManifest()) as! [String: Any]
        var workspace = root["workspace"] as! [String: Any]
        var credentials = workspace["credentials"] as! [[String: Any]], routes = workspace["routes"] as! [[String: Any]]
        credentials[0]["required_capabilities"] = ["ssh.user_certificate"]
        workspace["credentials"] = credentials; root["workspace"] = workspace
        let source = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))
        var selection = try WorkspaceExportSelection(workspace: source, groupIDs: ["group"])
        var choices = Dictionary(uniqueKeysWithValues: selection.identities.map { ($0.fingerprint, WorkspaceExportSelection.IdentityDecision.keepReferenceOnly) })
        let blocked = try selection.review(decisions: choices, loadedFingerprints: [])
        XCTAssertEqual(blocked.blockedRouteIDs, ["route"])
        XCTAssertTrue(blocked.findings.contains(where: { $0.entityID == "key" && $0.setting == "credential_policy" && $0.disposition == 0 }))

        var alternative = credentials[0]; alternative["id"] = "alternative"
        alternative.removeValue(forKey: "required_capabilities")
        alternative["public_fingerprint"] = "SHA256:ERERERERERERERERERERERERERERERERERERERERERE"
        credentials.append(alternative); routes[0]["credential_refs"] = ["key", "alternative"]
        workspace["credentials"] = credentials; workspace["routes"] = routes; root["workspace"] = workspace
        selection = try WorkspaceExportSelection(workspace: WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root)), groupIDs: ["group"])
        choices = Dictionary(uniqueKeysWithValues: selection.identities.map { ($0.fingerprint, .keepReferenceOnly) })
        let alternativeReview = try selection.review(decisions: choices, loadedFingerprints: [])
        XCTAssertTrue(alternativeReview.blockedRouteIDs.isEmpty)
        XCTAssertEqual(alternativeReview.missingIdentityRouteIDs, ["route"])
    }

    func testPhonePreviewAndInstalledReadinessAllowAPermittedAlternativeCandidate() async throws {
        let (key, fingerprint) = try sampleKey(using: crypto)
        var root = try JSONSerialization.jsonObject(with: sampleManifest()) as! [String: Any]
        var content = root["workspace"] as! [String: Any]
        var credentials = content["credentials"] as! [[String: Any]], routes = content["routes"] as! [[String: Any]]
        credentials[0]["required_capabilities"] = ["ssh.user_certificate"]
        var alternative = credentials[0]
        alternative["id"] = "permitted"; alternative["public_fingerprint"] = fingerprint
        alternative.removeValue(forKey: "required_capabilities")
        credentials.append(alternative); routes[0]["credential_refs"] = ["key", "permitted"]
        content["credentials"] = credentials; content["routes"] = routes; root["workspace"] = content
        let document = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))
        let selection = try WorkspaceExportSelection(workspace: document, groupIDs: ["group"])
        let choices = Dictionary(uniqueKeysWithValues: selection.identities.map {
            ($0.fingerprint, $0.fingerprint == fingerprint
                ? WorkspaceExportSelection.IdentityDecision.includePrivateKey : .keepReferenceOnly)
        })
        let included = PrivateKeyRecord(credentialID: "permitted", container: key.container, publicBlob: key.publicBlob)
        let archive = try selection.archive(decisions: choices, keys: [included], bundleID: "permitted-export", using: crypto)
        let sealed = try FTAG.seal(archive, using: crypto)
        let transferred = try FTAG.open(sealed.envelope, code: sealed.code, using: crypto)
        XCTAssertEqual(transferred.workspace.credentials.first?.requiredCapabilities, ["ssh.user_certificate"])
        XCTAssertTrue(transferred.portability.contains(where: { $0.entityID == "key" && $0.setting == "credential_policy" && $0.disposition == 0 }))
        let store = WorkspaceStore(persistence: StoreTestPersistence(), vault: StoreTestVault())
        let digest = crypto.sha256(sealed.envelope)
        let preview = try await store.previewImport(transferred, envelopeDigest: digest, support: support)
        XCTAssertTrue(try XCTUnwrap(preview.routes.first).isLocallyReady)
        XCTAssertEqual(preview.routes.first?.usableCredentialIDs, ["permitted"])
        let installed = try await store.install(transferred, envelopeDigest: digest, expectedRevision: nil,
            sourceAndContentsConfirmed: true)
        let readiness = try await store.routeReadiness(workspaceID: installed.workspace.id, support: support)
        XCTAssertTrue(try XCTUnwrap(readiness.first).isLocallyReady)
        XCTAssertEqual(readiness.first?.usableCredentialIDs, ["permitted"])

        // Losing the policy-free alternative still blocks the whole route, and
        // the exported finding reaches the phone with that required disposition.
        credentials[1]["required_capabilities"] = ["ssh.user_certificate"]
        content["credentials"] = credentials; root["workspace"] = content
        let blockedSelection = try WorkspaceExportSelection(
            workspace: WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root)), groupIDs: ["group"])
        let blockedArchive = try blockedSelection.archive(decisions: choices, keys: [included],
            bundleID: "all-candidates-blocked", using: crypto)
        XCTAssertTrue(blockedArchive.portability.contains(where: { $0.entityID == "route" && $0.setting == "route_readiness" && $0.disposition == 1 }))
        let blockedPreview = try await store.previewImport(blockedArchive, envelopeDigest: Data(repeating: 9, count: 32), support: support)
        XCTAssertTrue(try XCTUnwrap(blockedPreview.routes.first).issues.contains(.unsupportedRequirement))
    }
}
