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

    private func edited(_ original: WorkspaceDocument, _ edit: (inout [String: Any]) throws -> Void) throws -> WorkspaceDocument {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: original.raw) as? [String: Any])
        try edit(&root)
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }

    func testReimportComparisonMatchesStableEntitiesAcrossSerializationReorder() throws {
        let original = try workspace()
        let reordered = try edited(original) { root in
            root["bundle_id"] = "new-immutable-export"
            var value = try XCTUnwrap(root["workspace"] as? [String: Any])
            for section in ["groups", "machines", "routes", "credentials", "host_trust", "themes"] {
                value[section] = Array(try XCTUnwrap(value[section] as? [[String: Any]]).reversed())
            }
            root["workspace"] = value
        }
        let review = try WorkspaceChangeReview(previous: original, incoming: reordered)
        XCTAssertTrue(review.changes.isEmpty)
        XCTAssertFalse(review.requiresSecurityApproval)
    }

    func testReimportPreviewExposesEndpointAccountMethodKeyAndTrustChanges() throws {
        let original = try workspace()
        let changed = try edited(original) { root in
            var value = try XCTUnwrap(root["workspace"] as? [String: Any])
            var routes = try XCTUnwrap(value["routes"] as? [[String: Any]])
            routes[0]["host"] = "replacement.example.invalid"
            routes[0]["user"] = "another-account"
            routes[0]["port"] = 2222
            routes[0]["authentication_methods"] = ["publickey", "password"]
            routes[0]["credential_refs"] = ["alternative-key", "shared-key"]
            routes[0]["jump_routes"] = [] as [String]
            value["routes"] = routes
            var trusts = try XCTUnwrap(value["host_trust"] as? [[String: Any]])
            trusts[0]["host"] = "replacement.example.invalid"; trusts[0]["port"] = 2222
            trusts[0]["pins"] = [["algorithm": "ssh-ed25519", "fingerprint": "SHA256:" + String(repeating: "A", count: 43)]]
            value["host_trust"] = trusts
            // The fallback route shares the endpoint's trust record.
            routes[1]["host"] = "replacement.example.invalid"; routes[1]["port"] = 2222
            value["routes"] = routes
            root["workspace"] = value
        }
        let review = try WorkspaceChangeReview(previous: original, incoming: changed)
        XCTAssertTrue(review.requiresSecurityApproval)
        let routeChanges = review.changes.filter { $0.entity == .route && $0.entityID == "build-route" }
        XCTAssertEqual(Set(routeChanges.map(\.field)), ["host", "port", "user", "authentication_methods", "credential_refs", "jump_routes"])
        XCTAssertTrue(routeChanges.allSatisfy(\.affectsSecurity))
        let user = try XCTUnwrap(routeChanges.first { $0.field == "user" })
        XCTAssertEqual(user.previous, .string("operator")); XCTAssertEqual(user.incoming, .string("another-account"))
        let keys = try XCTUnwrap(routeChanges.first { $0.field == "credential_refs" })
        XCTAssertEqual(keys.previous, .array([.string("shared-key"), .string("alternative-key")]))
        XCTAssertEqual(keys.incoming, .array([.string("alternative-key"), .string("shared-key")]))
        XCTAssertTrue(review.securityChanges.contains { $0.entity == .hostTrust && $0.field == "pins" })
    }

    func testNavigationReorderAndRenameStaySeparateFromMembershipAndAutoConnectChanges() throws {
        let original = try workspace()
        let cosmetic = try edited(original) { root in
            var value = try XCTUnwrap(root["workspace"] as? [String: Any])
            var groups = try XCTUnwrap(value["groups"] as? [[String: Any]])
            groups[0]["machine_order"] = ["gpu", "build"]; value["groups"] = groups
            var machines = try XCTUnwrap(value["machines"] as? [[String: Any]])
            machines[0]["name"] = "Build renamed"; value["machines"] = machines
            root["workspace"] = value
        }
        let harmless = try WorkspaceChangeReview(previous: original, incoming: cosmetic)
        XCTAssertEqual(harmless.changes.count, 2); XCTAssertFalse(harmless.requiresSecurityApproval)
        let changed = try edited(cosmetic) { root in
            var value = try XCTUnwrap(root["workspace"] as? [String: Any])
            var groups = try XCTUnwrap(value["groups"] as? [[String: Any]])
            groups[0]["machine_order"] = ["gpu", "build", "archive"]
            groups[0]["auto_connect"] = true; value["groups"] = groups
            root["workspace"] = value
        }
        let security = try WorkspaceChangeReview(previous: cosmetic, incoming: changed)
        XCTAssertEqual(Set(security.securityChanges.map(\.field)), ["machine_order", "auto_connect"])
    }

    func testReviewRetainsExactLargeNumbersAndRejectsDifferentNamespace() throws {
        let original = try workspace()
        var before = try StrictJSON.decode(original.raw).object()
        var source = try before.required("source").object()
        source["export_sequence"] = .number("9007199254740993"); before["source"] = .object(source)
        let old = try WorkspaceDocument(data: StrictJSON.encode(.object(before)))
        source["export_sequence"] = .number("9007199254740994"); before["source"] = .object(source)
        let new = try WorkspaceDocument(data: StrictJSON.encode(.object(before)))
        let review = try WorkspaceChangeReview(previous: old, incoming: new)
        let change = try XCTUnwrap(review.changes.first)
        XCTAssertEqual(review.changes.count, 1)
        XCTAssertEqual(WorkspaceChangeReview.Change.description(of: change.previous), "9007199254740993")
        XCTAssertEqual(WorkspaceChangeReview.Change.description(of: change.incoming), "9007199254740994")
        let foreign = try edited(new) { root in
            var value = try XCTUnwrap(root["source"] as? [String: Any])
            value["exporter_id"] = "a-different-mac"; root["source"] = value
        }
        XCTAssertThrowsError(try WorkspaceChangeReview(previous: original, incoming: foreign)) {
            XCTAssertEqual($0 as? FTError, .malformed("workspace.review_namespace"))
        }
    }
}
