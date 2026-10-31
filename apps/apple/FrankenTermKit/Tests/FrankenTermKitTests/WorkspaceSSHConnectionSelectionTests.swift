import Foundation
import XCTest
@testable import FrankenTermKit

final class WorkspaceSSHConnectionSelectionTests: XCTestCase, @unchecked Sendable {
    private var support: WorkspaceDocument.RouteSupport {
        .init(routeModes: ["direct_ssh", "frankenterm_mux"],
              authenticationMethods: [.publicKey, .password, .keyboardInteractive],
              credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"], maximumJumpHops: 3)
    }

    /// Public metadata only; actual credential availability and trust policy
    /// are supplied separately, as they are in the store's atomic local view.
    private func state(jumps: Bool = false, repeated: Bool = false,
                       available: Set<String> = ["primary", "backup", "hop-key"],
                       workspaceID: UUID = UUID(), revision: UUID = UUID(),
                       using backend: WorkspaceDocument.RouteSupport? = nil,
                       deniedTrust: String? = nil, requiredTrust: String? = nil,
                       blockedRoute: String? = nil,
                       mutate: (inout [String: Any]) -> Void = { _ in }) throws -> WorkspaceLocalState {
        func fingerprint(_ byte: UInt8) -> String {
            "SHA256:" + Data(repeating: byte, count: 32).base64EncodedString().replacingOccurrences(of: "=", with: "")
        }
        func route(_ id: String, keys: [String], hops: [String] = [], standalone: Bool = true) -> [String: Any] {
            var value: [String: Any] = ["id": id, "mode": "direct_ssh", "host": id + ".example.invalid",
                "port": 22, "user": "operator", "credential_refs": keys,
                "authentication_methods": ["publickey", "password", "keyboard-interactive"],
                "host_trust_ref": id + "-trust", "jump_routes": hops]
            if standalone { value["session_creation"] = "on_select" }
            return value
        }
        let theme: [String: Any] = ["id": "theme", "name": "Terminal", "foreground": "#FFFFFF", "background": "#000000",
            "accent": "#3388FF", "ansi": Array(repeating: "#123456", count: 16),
            "cursor_foreground": "#000000", "cursor_background": "#FFFFFF", "cursor_border": "#FFFFFF",
            "selection_foreground": "#FFFFFF", "selection_background": "#345D91"]
        var workspace: [String: Any] = ["id": "workspace", "name": "Explicit routes",
            "groups": [["id": "group", "name": "Machines", "machine_order": ["machine"]]],
            "machines": [["id": "machine", "name": "Build", "theme_ref": "theme",
                          "route_refs": ["destination", "alternate"], "preferred_route_ref": "destination"]],
            "routes": [route("destination", keys: ["primary", "backup"],
                             hops: repeated ? ["first", "second"] : (jumps ? ["first"] : [])),
                       route("alternate", keys: ["backup"]),
                       route("first", keys: ["hop-key", "backup"], standalone: false),
                       route("second", keys: ["hop-key"], hops: repeated ? ["first"] : [])],
            "credentials": ["primary", "backup", "hop-key"].enumerated().map { index, id in
                ["id": id, "algorithm": "ssh-ed25519", "public_fingerprint": fingerprint(UInt8(index + 1)),
                 "private_material": "not_included"]
            },
            "host_trust": ["destination", "alternate", "first", "second"].map { id in
                ["id": id + "-trust", "host": id + ".example.invalid", "port": 22, "revoked_keys": [],
                 "pins": [["algorithm": "ssh-ed25519", "fingerprint": fingerprint(20)]]] as [String: Any]
            },
            "themes": [theme], "preferences": ["initial_machine_ref": "machine", "initial_group_ref": "group", "connect_on_open": false]]
        mutate(&workspace)
        let manifest = try JSONSerialization.data(withJSONObject: ["format": "ftworkspace", "version": 1,
            "bundle_id": "selection-test", "source": ["exporter_id": "mac"], "required_capabilities": [], "workspace": workspace], options: [.sortedKeys])
        let installed = InstalledWorkspace(id: workspaceID, revision: revision, manifest: manifest,
            envelopeDigest: Data(repeating: 0, count: 32), bindings: [], inactiveSettings: [], findings: [], connectLater: true)
        let document = try installed.document(), backend = backend ?? support
        var denied: [String: Set<WorkspaceDocument.HostKey>] = [:], required: [String: Set<String>] = [:]
        if let trust = document.hostTrust.first(where: { $0.id == deniedTrust }) {
            denied[trust.canonicalTrustScope] = Set(trust.pins)
        }
        if let trust = document.hostTrust.first(where: { $0.id == requiredTrust }) {
            required[trust.canonicalTrustScope] = ["unsupported-selection-policy"]
        }
        let readiness = try document.routes.map { route -> WorkspaceDocument.RouteReadiness in
            let ready = try document.readiness(forRouteID: route.id, availableCredentialIDs: available,
                reviewedTrustIDs: Set(document.hostTrust.map(\.id)), support: backend,
                revokedHostKeysByTrustScope: denied, requiredHostCapabilitiesByTrustScope: required)
            return .init(routeID: ready.routeID, usableCredentialIDs: ready.usableCredentialIDs,
                         issues: ready.issues + (route.id == blockedRoute ? [.unsupportedRequirement] : []))
        }
        return .init(workspace: installed, connectionPreferences: .init(autoConnectOverrides: [:]),
            navigationPreferences: .init(), credentialIDs: available,
            revokedHostKeysByTrustScope: denied, unsavedHostKeysByTrustScope: [:],
            requiredHostCapabilitiesByTrustScope: required, routes: readiness)
    }

    private func options(_ state: WorkspaceLocalState, routeID: String = "destination") throws -> WorkspaceSSHRouteOptions {
        try .init(localState: state, machineID: "machine", routeID: routeID, support: support)
    }

    func testExplicitChoicesPreservePermittedMethodsAndKeyOrderWithoutChangingAutomaticChoice() throws {
        let local = try state(), route = try XCTUnwrap(local.workspace.document().routes.first)
        XCTAssertEqual(WorkspaceSSHAuthenticationChoice.permittedChoices(for: route,
            usableCredentialIDs: ["foreign", "backup", "primary"]),
            [.publicKey("primary"), .publicKey("backup"), .password, .keyboardInteractive])
        XCTAssertEqual(WorkspaceSSHAuthenticationChoice.choices(for: route,
            usableCredentialIDs: ["primary", "backup"]), [.publicKey("primary"), .publicKey("backup")])
        XCTAssertEqual(WorkspaceSSHAuthenticationChoice.permittedChoices(for: route,
            usableCredentialIDs: ["foreign", "backup"]), [.publicKey("backup"), .password, .keyboardInteractive])
        let passwordFirst = try state { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["authentication_methods"] = ["password", "publickey", "keyboard-interactive"]
            workspace["routes"] = routes
        }
        let first = try XCTUnwrap(passwordFirst.workspace.document().routes.first)
        XCTAssertEqual(WorkspaceSSHAuthenticationChoice.permittedChoices(for: first, usableCredentialIDs: ["primary"]),
                       [.password, .publicKey("primary"), .keyboardInteractive])
        XCTAssertEqual(WorkspaceSSHAuthenticationChoice.choices(for: first, usableCredentialIDs: ["primary"]), [.password])
    }

    func testPasswordAndInteractiveAreExplicitlySelectableWhileAKeyIsAvailable() throws {
        let review = try options(state())
        for choice in [WorkspaceSSHAuthenticationChoice.password, .keyboardInteractive, .publicKey("backup")] {
            let selection = try review.select(authentication: choice, hopAuthentications: [])
            XCTAssertEqual(selection.authentication, choice)
            XCTAssertEqual(selection.expectedRevision, review.expectedRevision)
            XCTAssertTrue(selection.hops.isEmpty)
            XCTAssertNoThrow(try review.validate(selection))
        }
    }

    func testScopeCannotBeReboundToAnotherWorkspaceRevisionMachineOrRoute() throws {
        let review = try options(state())
        let variants: [(UUID, UUID, String, String)] = [
            (UUID(), review.expectedRevision, "machine", "destination"),
            (review.workspaceID, UUID(), "machine", "destination"),
            (review.workspaceID, review.expectedRevision, "different-machine", "destination"),
            (review.workspaceID, review.expectedRevision, "machine", "alternate")]
        for (workspace, revision, machine, route) in variants {
            let changed = WorkspaceSSHConnectionSelection(workspaceID: workspace, expectedRevision: revision,
                machineID: machine, routeID: route, authentication: .password)
            XCTAssertThrowsError(try review.validate(changed)) {
                XCTAssertEqual($0 as? WorkspaceSSHConnectionSelectionError, .stale)
            }
        }
    }

    func testUnavailableOrUnpermittedCredentialsAndMethodsNeverBecomeAlternatives() throws {
        let review = try options(state(available: ["backup", "hop-key"]))
        for key in ["primary", "hop-key", "foreign"] {
            XCTAssertThrowsError(try review.select(authentication: .publicKey(key), hopAuthentications: [])) {
                XCTAssertEqual($0 as? WorkspaceSSHConnectionSelectionError, .authenticationUnavailable)
            }
        }
        let keyOnly = try options(state { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["authentication_methods"] = ["publickey"]; workspace["routes"] = routes
        })
        for method in [WorkspaceSSHAuthenticationChoice.password, .keyboardInteractive] {
            XCTAssertThrowsError(try keyOnly.select(authentication: method, hopAuthentications: [])) {
                XCTAssertEqual($0 as? WorkspaceSSHConnectionSelectionError, .authenticationUnavailable)
            }
        }
    }

    func testFreshCredentialAvailabilityInvalidatesChosenKeyWithoutSelectingPassword() throws {
        let original = try state(), review = try options(original)
        let selection = try review.select(authentication: .publicKey("primary"), hopAuthentications: [])
        let fresh = try options(state(available: ["backup", "hop-key"], workspaceID: original.workspace.id,
                                      revision: original.workspace.revision))
        XCTAssertTrue(fresh.destination.authenticationChoices.contains(.password))
        XCTAssertThrowsError(try fresh.validate(selection)) {
            XCTAssertEqual($0 as? WorkspaceSSHConnectionSelectionError, .authenticationUnavailable)
        }
    }

    func testEachOrderedHopOccurrenceRetainsItsOwnDeliberateChoice() throws {
        let review = try options(state(repeated: true))
        XCTAssertEqual(review.hops.map { $0.route.id }, ["first", "first", "second"])
        let selected = try review.select(authentication: .password,
            hopAuthentications: [.password, .keyboardInteractive, .publicKey("hop-key")])
        XCTAssertEqual(selected.hops, [.init(routeID: "first", authentication: .password),
            .init(routeID: "first", authentication: .keyboardInteractive),
            .init(routeID: "second", authentication: .publicKey("hop-key"))])
        XCTAssertNoThrow(try review.validate(selected))
    }

    func testMissingAddedAndReorderedHopOccurrencesAreRejected() throws {
        let review = try options(state(repeated: true))
        for count in [0, 1, 2, 4] {
            XCTAssertThrowsError(try review.select(authentication: .password,
                hopAuthentications: Array(repeating: .password, count: count))) {
                XCTAssertEqual($0 as? WorkspaceSSHConnectionSelectionError, .hopMismatch)
            }
        }
        for ids in [["first", "second", "first"], ["second", "first", "first"], ["first", "first", "alternate"]] {
            let changed = WorkspaceSSHConnectionSelection(workspaceID: review.workspaceID,
                expectedRevision: review.expectedRevision, machineID: review.machineID, routeID: review.routeID,
                authentication: .password, hops: ids.map { .init(routeID: $0, authentication: .password) })
            XCTAssertThrowsError(try review.validate(changed)) {
                XCTAssertEqual($0 as? WorkspaceSSHConnectionSelectionError, .hopMismatch)
            }
        }
    }

    func testDestinationCredentialCannotBeOfferedToAHopThatDoesNotPermitIt() throws {
        let review = try options(state(jumps: true))
        XCTAssertThrowsError(try review.select(authentication: .publicKey("primary"),
            hopAuthentications: [.publicKey("primary")])) {
            XCTAssertEqual($0 as? WorkspaceSSHConnectionSelectionError, .authenticationUnavailable)
        }
    }

    func testTransitOnlyHopDoesNotRequireItsOwnTerminalCreationPolicy() throws {
        let local = try state(jumps: true)
        XCTAssertEqual(local.routes.first(where: { $0.routeID == "first" })?.issues, [.sessionPolicyMissing])
        let review = try options(local)
        XCTAssertEqual(review.hops.first?.authenticationChoices, [.publicKey("hop-key"), .publicKey("backup"), .password, .keyboardInteractive])
        XCTAssertNoThrow(try review.select(authentication: .password, hopAuthentications: [.password]))
    }

    func testOnlyRoutesOwnedByTheMachineCanBeReviewed() throws {
        let local = try state()
        for route in ["first", "second", "unrelated"] {
            XCTAssertThrowsError(try options(local, routeID: route)) {
                XCTAssertEqual($0 as? WorkspaceSSHConnectionSelectionError, .routeUnavailable)
            }
        }
        XCTAssertEqual(try options(local, routeID: "alternate").routeID, "alternate")
    }

    func testDurablePortabilityReadinessCannotBeBypassedBySelectingPassword() throws {
        XCTAssertThrowsError(try options(state(blockedRoute: "destination"))) {
            XCTAssertEqual($0 as? WorkspaceSSHConnectionSelectionError, .routeUnavailable)
        }
    }

    func testNegativeHostTrustAndMandatoryPolicyBlockEveryPeer() throws {
        for trust in ["destination-trust", "first-trust"] {
            XCTAssertThrowsError(try options(state(jumps: true, deniedTrust: trust))) {
                XCTAssertEqual($0 as? WorkspaceSSHConnectionSelectionError, .routeUnavailable)
            }
            XCTAssertThrowsError(try options(state(jumps: true, requiredTrust: trust))) {
                XCTAssertEqual($0 as? WorkspaceSSHConnectionSelectionError, .routeUnavailable)
            }
        }
    }

    func testBackendMethodQualificationAndHopBoundConstrainExplicitChoices() throws {
        let keyOnly = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
            credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"], maximumJumpHops: 0)
        let review = try WorkspaceSSHRouteOptions(localState: state(using: keyOnly), machineID: "machine",
                                                  routeID: "destination", support: keyOnly)
        XCTAssertEqual(review.destination.authenticationChoices, [.publicKey("primary"), .publicKey("backup")])
        XCTAssertThrowsError(try review.select(authentication: .password, hopAuthentications: [])) {
            XCTAssertEqual($0 as? WorkspaceSSHConnectionSelectionError, .authenticationUnavailable)
        }
        XCTAssertThrowsError(try WorkspaceSSHRouteOptions(localState: state(jumps: true, using: keyOnly),
            machineID: "machine", routeID: "destination", support: keyOnly)) {
            XCTAssertEqual($0 as? WorkspaceSSHConnectionSelectionError, .routeUnavailable)
        }
    }

    func testCancelledReviewAndRevalidationGrantNoSelection() async throws {
        let local = try state(), review = try options(local), backend = support
        let selection = try review.select(authentication: .password, hopAuthentications: [])
        try await Task {
            withUnsafeCurrentTask { $0?.cancel() }
            XCTAssertThrowsError(try WorkspaceSSHRouteOptions(localState: local, machineID: "machine",
                                                              routeID: "destination", support: backend)) {
                XCTAssertTrue($0 is CancellationError)
            }
            XCTAssertThrowsError(try review.validate(selection)) { XCTAssertTrue($0 is CancellationError) }
        }.value
    }
}
