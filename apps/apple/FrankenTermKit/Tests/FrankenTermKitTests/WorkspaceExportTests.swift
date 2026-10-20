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
