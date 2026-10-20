import Foundation
import XCTest
@testable import FrankenTermKit

final class WorkspacePolicyTests: XCTestCase {
    private func workspace() throws -> WorkspaceDocument {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest()) as? [String: Any])
        var content = try XCTUnwrap(root["workspace"] as? [String: Any])
        content["groups"] = [
            ["id": "fleet", "name": "Fleet", "machine_order": ["build", "gpu"]],
            ["id": "archives", "name": "Archives", "machine_order": ["archive"]],
            ["id": "empty", "name": "Empty", "machine_order": []]
        ]
        content["machines"] = [
            ["id": "build", "name": "Build", "theme_ref": "theme", "route_refs": ["build-route", "fallback-route"], "preferred_route_ref": "build-route"],
            ["id": "gpu", "name": "GPU", "theme_ref": "theme", "route_refs": ["gpu-route"], "preferred_route_ref": "gpu-route"],
            ["id": "archive", "name": "Archive", "theme_ref": "archive-theme", "route_refs": ["archive-route"], "preferred_route_ref": "archive-route"]
        ]
        func route(_ id: String, trust: String, keys: [String], hops: [String] = []) -> [String: Any] {
            ["id": id, "mode": "direct_ssh", "host": trust + ".example.invalid", "port": 22, "user": "operator",
             "host_trust_ref": trust, "credential_refs": keys, "authentication_methods": keys.isEmpty ? ["password"] : ["publickey"],
             "jump_routes": hops, "session_creation": "on_select"]
        }
        // Array order deliberately puts destinations before their dependencies.
        content["routes"] = [
            route("build-route", trust: "build-trust", keys: ["shared-key", "alternative-key"], hops: ["first-hop"]),
            route("fallback-route", trust: "build-trust", keys: []),
            route("gpu-route", trust: "gpu-trust", keys: ["shared-key"]),
            route("archive-route", trust: "archive-trust", keys: ["archive-key"]),
            route("first-hop", trust: "first-trust", keys: ["jump-key"], hops: ["second-hop"]),
            route("second-hop", trust: "second-trust", keys: ["jump-key"])
        ]
        func fingerprint(_ byte: UInt8) -> String {
            "SHA256:" + Data(repeating: byte, count: 32).base64EncodedString().replacingOccurrences(of: "=", with: "")
        }
        content["credentials"] = ["shared-key", "alternative-key", "archive-key", "jump-key", "spare-key"].enumerated().map { index, id in
            ["id": id, "algorithm": "ssh-ed25519", "public_fingerprint": fingerprint(UInt8(index)), "private_material": "not_included"]
        }
        content["host_trust"] = ["build-trust", "gpu-trust", "archive-trust", "first-trust", "second-trust", "spare-trust"].map { id -> [String: Any] in
            ["id": id, "host": id + ".example.invalid", "port": 22,
             "pins": [["algorithm": "ssh-ed25519", "fingerprint": fingerprint(20)]], "revoked_keys": []]
        }
        let originalThemes = try XCTUnwrap(content["themes"] as? [[String: Any]])
        let originalTheme = try XCTUnwrap(originalThemes.first)
        var archiveTheme = originalTheme, spareTheme = originalTheme
        archiveTheme["id"] = "archive-theme"; spareTheme["id"] = "spare-theme"
        content["themes"] = [originalTheme, archiveTheme, spareTheme]
        content["preferences"] = ["initial_machine_ref": "build", "initial_group_ref": "fleet", "connect_on_open": false]
        root["workspace"] = content
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }

    func testWorkspaceAndUnknownRequiredScopesBlockEveryRoute() throws {
        let document = try workspace(), all = Set(document.routes.map(\.id))
        let targetSets: [Set<String>] = [[document.id], ["unknown-entity"], [""], ["build", "unknown-entity"]]
        for targets in targetSets {
            XCTAssertEqual(WorkspacePortabilityPolicy.blockedRouteIDs(in: document, requiredEntityIDs: targets), all)
        }
    }

    func testAbsentRequirementsAndKnownUnusedEntitiesDoNotBlockUnrelatedRoutes() throws {
        let document = try workspace()
        XCTAssertTrue(WorkspacePortabilityPolicy.blockedRouteIDs(in: document, requiredEntityIDs: []).isEmpty)
        XCTAssertTrue(WorkspacePortabilityPolicy.blockedRouteIDs(in: document,
            requiredEntityIDs: ["empty", "spare-key", "spare-trust", "spare-theme"]).isEmpty)
    }

    func testMachineGroupAndThemeScopesIncludeEveryAssociatedRoute() throws {
        let document = try workspace()
        XCTAssertEqual(WorkspacePortabilityPolicy.blockedRouteIDs(in: document, requiredEntityIDs: ["build"]),
                       ["build-route", "fallback-route"])
        for target in ["fleet", "theme"] {
            XCTAssertEqual(WorkspacePortabilityPolicy.blockedRouteIDs(in: document, requiredEntityIDs: [target]),
                           ["build-route", "fallback-route", "gpu-route"])
        }
        for target in ["archive", "archives", "archive-theme"] {
            XCTAssertEqual(WorkspacePortabilityPolicy.blockedRouteIDs(in: document, requiredEntityIDs: [target]), ["archive-route"])
        }
    }

    func testCredentialAndHostTrustScopesRespectSharingAndOrderedAlternatives() throws {
        let document = try workspace()
        XCTAssertEqual(WorkspacePortabilityPolicy.blockedRouteIDs(in: document, requiredEntityIDs: ["shared-key"]),
                       ["build-route", "gpu-route"])
        XCTAssertEqual(WorkspacePortabilityPolicy.blockedRouteIDs(in: document, requiredEntityIDs: ["alternative-key"]), ["build-route"])
        XCTAssertEqual(WorkspacePortabilityPolicy.blockedRouteIDs(in: document, requiredEntityIDs: ["build-trust"]),
                       ["build-route", "fallback-route"])
        XCTAssertEqual(WorkspacePortabilityPolicy.blockedRouteIDs(in: document, requiredEntityIDs: ["archive-key", "gpu-trust"]),
                       ["archive-route", "gpu-route"])
    }

    func testBlockedJumpDependenciesPropagateTransitivelyWithoutBlockingIndependentRoutes() throws {
        let document = try workspace()
        for target in ["second-hop", "second-trust", "jump-key"] {
            XCTAssertEqual(WorkspacePortabilityPolicy.blockedRouteIDs(in: document, requiredEntityIDs: [target]),
                           ["build-route", "first-hop", "second-hop"])
        }
        XCTAssertEqual(WorkspacePortabilityPolicy.blockedRouteIDs(in: document, requiredEntityIDs: ["first-hop"]),
                       ["build-route", "first-hop"])
        XCTAssertEqual(WorkspacePortabilityPolicy.blockedRouteIDs(in: document, requiredEntityIDs: ["build-route"]), ["build-route"])
    }
}
