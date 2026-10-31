import Foundation
import XCTest
@testable import FrankenTermKit
#if canImport(CryptoKit)
import CryptoKit
#else
import Crypto
#endif

private final class EnrollmentReportVault: WorkspaceCredentialVault, @unchecked Sendable {
    private let lock = NSLock()
    private var items: [String: (fingerprint: String, identity: SSHPublicIdentity)] = [:]
    private var created = 0
    var creates: Int { lock.lock(); defer { lock.unlock() }; return created }

    func create(account: String, privateContainer: Data, fingerprint: String) throws {
        let identity = try SSHUserAuthenticationSigner.publicIdentity(privateContainer: privateContainer)
        lock.lock(); defer { lock.unlock() }
        guard items[account] == nil, identity.fingerprint == fingerprint else { throw FTError.storage }
        items[account] = (fingerprint, identity); created += 1
    }
    func fingerprint(account: String) throws -> String? {
        lock.lock(); defer { lock.unlock() }; return items[account]?.fingerprint
    }
    func publicIdentity(account: String) throws -> SSHPublicIdentity? {
        lock.lock(); defer { lock.unlock() }; return items[account]?.identity
    }
    func remove(account: String) throws {
        lock.lock(); defer { lock.unlock() }; items[account] = nil
    }
    func substitutePublicIdentity(account: String, identity: SSHPublicIdentity) {
        lock.lock(); defer { lock.unlock() }
        guard let previous = items[account] else { return }
        items[account] = (previous.fingerprint, identity)
    }
}

@MainActor
final class WorkspaceKeyEnrollmentReportTests: XCTestCase {
    private struct Spec {
        let id: String, host: String
        var user = "operator"
        var port: UInt16 = 22
        var jumps: [String] = []
        var alias: String?
        var required: [String] = []
        init(_ id: String, _ host: String, jumps: [String] = [], alias: String? = nil) {
            self.id = id; self.host = host; self.jumps = jumps; self.alias = alias
        }
    }
    private struct Fixture {
        let store: WorkspaceStore, disk: StoreTestPersistence, vault: EnrollmentReportVault
        let saved: InstalledWorkspace, key: WorkspaceDeviceKey
    }
    private struct Entry {
        let target: WorkspaceKeyEnrollmentTarget
        let state: WorkspaceKeyEnrollmentState?
        var mutation: WorkspaceKeyEnrollmentAction?
        init(_ target: WorkspaceKeyEnrollmentTarget, _ state: WorkspaceKeyEnrollmentState?,
             mutation: WorkspaceKeyEnrollmentAction? = nil) {
            self.target = target; self.state = state; self.mutation = mutation
        }
    }
    private let one = Spec("one", "one.fixture.invalid")
    private let two = Spec("two", "two.fixture.invalid")
    private let pin = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"

    /// A standalone public workspace fixture, not the production review's
    /// matcher or a copy of the matcher's context-enumeration algorithm.
    private func document(_ specs: [Spec], machines: [(String, [String])]? = nil,
                          bundle: String = "bundle", name: String = "Fleet") throws -> WorkspaceDocument {
        let members = machines ?? [("machine", specs.map(\.id))]
        let routes: [[String: Any]] = specs.map { spec in
            var value: [String: Any] = [
                "id": spec.id, "mode": "direct_ssh", "session_creation": "on_select",
                "host": spec.host, "port": Int(spec.port), "user": spec.user,
                "credential_refs": ["mac-key"], "authentication_methods": ["publickey"],
                "host_trust_ref": "trust-" + spec.id, "jump_routes": spec.jumps
            ]
            if let alias = spec.alias { value["host_key_alias"] = alias }
            if !spec.required.isEmpty { value["required_capabilities"] = spec.required }
            return value
        }
        let trusts: [[String: Any]] = specs.map { spec in
            var value: [String: Any] = ["id": "trust-" + spec.id, "host": spec.host, "port": Int(spec.port),
                "pins": [["algorithm": "ssh-ed25519", "fingerprint": pin]], "revoked_keys": []]
            if let alias = spec.alias { value["host_key_alias"] = alias }
            return value
        }
        let theme: [String: Any] = [
            "id": "theme", "name": "Fixture", "foreground": "#FFFFFF", "background": "#10223C",
            "accent": "#3B82F6", "ansi": Array(repeating: "#123456", count: 16),
            "cursor_foreground": "#000000", "cursor_background": "#FFFFFF", "cursor_border": "#FFFFFF",
            "selection_foreground": "#FFFFFF", "selection_background": "#345D91"
        ]
        let workspace: [String: Any] = [
            "id": "fleet", "name": name,
            "groups": [["id": "all", "name": "All", "machine_order": members.map(\.0), "auto_connect": true]],
            "machines": members.map { id, routeIDs in
                ["id": id, "name": id, "theme_ref": "theme", "route_refs": routeIDs,
                 "preferred_route_ref": routeIDs[0], "auto_connect": true] as [String: Any]
            },
            "routes": routes,
            "credentials": [["id": "mac-key", "algorithm": "ssh-ed25519",
                "public_fingerprint": pin, "private_material": "not_included"]],
            "host_trust": trusts, "themes": [theme],
            "preferences": ["initial_machine_ref": members[0].0, "initial_group_ref": "all", "connect_on_open": true]
        ]
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: [
            "format": "ftworkspace", "version": 1, "bundle_id": bundle,
            "source": ["exporter_id": "fixture-mac"], "required_capabilities": [], "workspace": workspace
        ], options: [.sortedKeys]))
    }

    private func fixture(_ specs: [Spec]? = nil, machines: [(String, [String])]? = nil,
                         findings: [AirGapArchive.Portability] = []) async throws -> Fixture {
        let source = try document(specs ?? [one], machines: machines)
        let archive = try AirGapArchive(workspace: source, keys: [], portability: findings, using: TestCryptography())
        let disk = StoreTestPersistence(), vault = EnrollmentReportVault()
        let store = WorkspaceStore(persistence: disk, vault: vault)
        let installed = try await store.install(archive, envelopeDigest: Data(repeating: 161, count: 32),
            expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let created = try await store.createDeviceKey(workspaceID: installed.id,
            expectedRevision: installed.revision, confirmedByUser: true)
        return .init(store: store, disk: disk, vault: vault, saved: created.workspace, key: created.key)
    }

    private func target(_ spec: Spec) throws -> WorkspaceKeyEnrollmentTarget {
        try .init(host: spec.host, port: spec.port, user: spec.user)
    }
    private func path(_ destination: Spec, through hops: [Spec]) throws -> WorkspaceKeyEnrollmentSSHPlan {
        try .init(target: target(destination), hostKeyAlias: destination.alias,
            jumpHosts: hops.map { try .init(target: target($0), hostKeyAlias: $0.alias) })
    }
    private func report(_ key: WorkspaceDeviceKey, _ entries: [Entry]) throws -> Data {
        let request = try WorkspaceKeyEnrollmentRequest(key: key,
            requestID: XCTUnwrap(UUID(uuidString: "22222222-2222-2222-2222-222222222222")))
        var journal = try WorkspaceKeyEnrollmentJournal(request: request, targets: entries.map(\.target))
        for entry in entries {
            guard let state = entry.state else { continue }
            let action: WorkspaceKeyEnrollmentAction
            switch state {
            case .installed, .uncertain: action = .install
            case .removed: action = .remove
            default: action = .inspect
            }
            if let mutation = entry.mutation ?? (action == .inspect ? nil : action) {
                try journal.beginMutation(targetID: entry.target.id, action: mutation)
            }
            try journal.record(.init(operationID: request.operationID(for: entry.target),
                target: entry.target, state: state, detail: "Observation supplied by the selected public file."), action: action)
        }
        return try journal.encoded()
    }
    private func review(_ f: Fixture, _ bytes: Data, saved: InstalledWorkspace? = nil) async throws
        -> WorkspaceKeyEnrollmentReportReview {
        let saved = saved ?? f.saved
        return try await f.store.reviewEnrollmentResults(workspaceID: saved.id, expectedRevision: saved.revision,
            credentialID: f.key.id, data: bytes)
    }
    private func row(_ review: WorkspaceKeyEnrollmentReportReview, _ id: String) throws
        -> WorkspaceKeyEnrollmentReportReview.Route {
        try XCTUnwrap(review.routes.first { $0.id == id })
    }
    private func replace(_ f: Fixture, saved: InstalledWorkspace, document: WorkspaceDocument) async throws
        -> InstalledWorkspace {
        try await f.store.install(AirGapArchive(workspace: document, keys: [], using: TestCryptography()),
            envelopeDigest: Data(repeating: 162, count: 32), expectedRevision: saved.revision,
            sourceAndContentsConfirmed: true, allowMachineRemoval: true, securityChangesConfirmed: true,
            associateWithExistingWorkspace: true).workspace
    }

    func testOpeningResultsIsObservationOnlyAndChangesNoConnectionApproval() async throws {
        let f = try await fixture()
        let approved = try await f.store.approveConnections(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, confirmedByUser: true)
        let before = f.disk.bytes(.catalog), creates = f.vault.creates
        let bytes = try report(f.key, [.init(target(one), .installed)])
        let reviewed = try await review(f, bytes, saved: approved)
        XCTAssertTrue(try row(reviewed, "one").canAssign)
        XCTAssertEqual(reviewed.key, f.key)
        XCTAssertEqual(reviewed.reportDigest, SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined())
        XCTAssertEqual(f.disk.bytes(.catalog), before)
        XCTAssertEqual(f.vault.creates, creates)
        let loaded = try await f.store.load()
        XCTAssertTrue(try XCTUnwrap(loaded.first).deviceKeyAssignments.isEmpty)
        XCTAssertFalse(try XCTUnwrap(loaded.first).connectLater)
    }

    func testSelectedRoutesPublishTogetherPreservingUnselectedKeysAndImportedPolicy() async throws {
        let third = Spec("three", "three.fixture.invalid")
        let f = try await fixture([one, two, third])
        let other = try await f.store.createDeviceKey(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, confirmedByUser: true)
        let prior = try await f.store.assignDeviceKey(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, credentialID: other.key.id,
            routeID: "three", confirmedByUser: true)
        let approved = try await f.store.approveConnections(workspaceID: prior.id,
            expectedRevision: prior.revision, confirmedByUser: true)
        let bytes = try report(f.key, [.init(target(one), .installed), .init(target(two), .alreadyPresent),
                                     .init(target(third), .installed)])
        let reviewed = try await review(f, bytes, saved: approved)
        let applied = try await f.store.applyEnrollmentResults(reviewed, routeIDs: ["one", "two"], confirmedByUser: true)
        XCTAssertNotEqual(applied.revision, approved.revision)
        XCTAssertTrue(applied.connectLater)
        XCTAssertEqual(applied.manifest, approved.manifest)
        XCTAssertEqual(applied.envelopeDigest, approved.envelopeDigest)
        XCTAssertEqual(applied.bindings, approved.bindings)
        XCTAssertEqual(applied.hostKeyPins, approved.hostKeyPins)
        let effective = try applied.document()
        XCTAssertEqual(effective.routes.first { $0.id == "one" }?.credentialIDs, [f.key.id])
        XCTAssertEqual(effective.routes.first { $0.id == "two" }?.credentialIDs, [f.key.id])
        XCTAssertEqual(effective.routes.first { $0.id == "three" }?.credentialIDs, [other.key.id])
        XCTAssertEqual(try applied.importedDocument().routes.first?.credentialIDs, ["mac-key"])
        let restored = try await WorkspaceStore(persistence: f.disk, vault: f.vault).load()
        XCTAssertEqual(try XCTUnwrap(restored.first).deviceKeyAssignments, applied.deviceKeyAssignments)
        XCTAssertTrue(try XCTUnwrap(restored.first).connectLater)
    }

    func testDestinationReceiptCannotAlsoAssignItsTransitAccounts() async throws {
        let first = Spec("first", "first.fixture.invalid")
        let second = Spec("second", "10.0.0.8")
        let final = Spec("final", "10.0.0.9", jumps: ["first", "second"])
        let f = try await fixture([first, second, final], machines: [("machine", ["final"])])
        let bytes = try report(f.key, [.init(path(final, through: [first, second]).target, .installed)])
        let reviewed = try await review(f, bytes)
        XCTAssertTrue(try row(reviewed, "final").canAssign)
        XCTAssertFalse(try row(reviewed, "first").canAssign)
        XCTAssertFalse(try row(reviewed, "second").canAssign)
        let before = f.disk.bytes(.catalog)
        do {
            _ = try await f.store.applyEnrollmentResults(reviewed, routeIDs: ["final", "second"], confirmedByUser: true)
            XCTFail("a destination receipt also selected an unreported transit account")
        } catch { XCTAssertEqual(error as? WorkspaceKeyEnrollmentReportError, .unavailableSelection) }
        XCTAssertEqual(f.disk.bytes(.catalog), before)
        let applied = try await f.store.applyEnrollmentResults(reviewed, routeIDs: ["final"], confirmedByUser: true)
        XCTAssertEqual(applied.deviceKeyAssignments.map(\.routeID), ["final"])
    }

    func testTransitOnlyRouteUsesItsActualPrefixWithoutInventingADirectAccount() async throws {
        let bastion = Spec("bastion", "gateway.fixture.invalid"), jump = Spec("jump", "10.0.0.8")
        let final = Spec("final", "10.0.0.9", jumps: ["bastion", "jump"])
        let f = try await fixture([bastion, jump, final], machines: [("machine", ["final"])])
        let receipt = try path(jump, through: [bastion]).target
        let reviewed = try await review(f, report(f.key, [.init(receipt, .installed)]))
        let selected = try row(reviewed, "jump")
        XCTAssertTrue(selected.canAssign)
        XCTAssertEqual(selected.paths.map(\.plan.target), [receipt])
        XCTAssertEqual(selected.paths.first?.plan.jumpHosts.map(\.target.host), [bastion.host])
        let applied = try await f.store.applyEnrollmentResults(reviewed, routeIDs: ["jump"], confirmedByUser: true)
        XCTAssertEqual(applied.deviceKeyAssignments.map(\.routeID), ["jump"])
        XCTAssertEqual(try applied.document().routes.first { $0.id == "bastion" }?.credentialIDs, ["mac-key"])
    }

    func testSharedPrivateJumpRequiresAllPrefixesAndAnActuallyExposedDirectUse() async throws {
        let left = Spec("left", "left.fixture.invalid"), right = Spec("right", "right.fixture.invalid")
        let jump = Spec("jump", "10.0.0.8")
        let first = Spec("first", "10.0.0.9", jumps: ["left", "jump"])
        let second = Spec("second", "10.0.0.9", jumps: ["right", "jump"])
        let f = try await fixture([left, right, jump, first, second],
            machines: [("left-machine", ["first"]), ("right-machine", ["second"]), ("jump-machine", ["jump"])])
        let leftTarget = try path(jump, through: [left]).target
        let rightTarget = try path(jump, through: [right]).target
        XCTAssertNotEqual(leftTarget.id, rightTarget.id)
        let partial = try await review(f, report(f.key, [.init(leftTarget, .installed), .init(rightTarget, .installed)]))
        XCTAssertEqual(try row(partial, "jump").paths.count, 3)
        XCTAssertTrue(try row(partial, "jump").issues.contains(.uncoveredPath))
        let complete = try await review(f, report(f.key, [
            .init(leftTarget, .installed), .init(rightTarget, .installed), .init(target(jump), .alreadyPresent)
        ]))
        XCTAssertTrue(try row(complete, "jump").canAssign)
        XCTAssertEqual(Set(try row(complete, "jump").machineNames), ["left-machine", "right-machine", "jump-machine"])
    }

    func testAliasAndOrderedPrefixMismatchesRemainUnmatched() async throws {
        let first = Spec("first", "first.fixture.invalid"), second = Spec("second", "second.fixture.invalid")
        let final = Spec("final", "10.0.0.9", jumps: ["first", "second"], alias: "final-trust.fixture.invalid")
        let f = try await fixture([first, second, final], machines: [("machine", ["final"])])
        let oldDirect = try target(final), reversed = try path(final, through: [second, first]).target
        let reviewed = try await review(f, report(f.key, [.init(oldDirect, .installed), .init(reversed, .installed)]))
        XCTAssertFalse(try row(reviewed, "final").canAssign)
        XCTAssertEqual(Set(reviewed.unmatchedObservations.map(\.target)), [oldDirect, reversed])
        let correct = try await review(f, report(f.key, [.init(path(final, through: [first, second]).target, .installed)]))
        XCTAssertTrue(try row(correct, "final").canAssign)
        XCTAssertEqual(try row(correct, "final").paths.first?.plan.destination.hostKeyAlias, final.alias)
    }

    func testRepeatedPrivateEndpointsAtDifferentPositionsDoNotCollapse() async throws {
        let first = Spec("first", "127.0.0.1"), second = Spec("second", "127.0.0.1")
        let final = Spec("final", "127.0.0.1", jumps: ["first", "second"])
        let f = try await fixture([first, second, final], machines: [("machine", ["final"])])
        let entries = try [
            Entry(target(first), .installed),
            Entry(path(second, through: [first]).target, .installed),
            Entry(path(final, through: [first, second]).target, .installed)
        ]
        XCTAssertEqual(Set(entries.map(\.target.id)).count, 3)
        let reviewed = try await review(f, report(f.key, entries))
        XCTAssertEqual(reviewed.routes.filter(\.canAssign).count, 3)
        let applied = try await f.store.applyEnrollmentResults(reviewed,
            routeIDs: ["first", "second", "final"], confirmedByUser: true)
        XCTAssertEqual(Set(applied.deviceKeyAssignments.map(\.routeID)), ["first", "second", "final"])
    }

    func testRestrictedMissingUncertainAndRemovalObservationsCannotSelectRoutes() async throws {
        let f = try await fixture(), account = try target(one)
        let before = f.disk.bytes(.catalog)
        let states: [WorkspaceKeyEnrollmentState?] = [nil, .absent, .restricted, .blocked, .uncertain, .removed, .stillAuthorized]
        for state in states {
            let reviewed = try await review(f, report(f.key, [.init(account, state)]))
            XCTAssertFalse(try row(reviewed, "one").canAssign, "\(String(describing: state))")
            do {
                _ = try await f.store.applyEnrollmentResults(reviewed, routeIDs: ["one"], confirmedByUser: true)
                XCTFail("an unconfirmed observation selected a key: \(String(describing: state))")
            } catch { XCTAssertEqual(error as? WorkspaceKeyEnrollmentReportError, .unavailableSelection) }
        }
        let removing = try await review(f, report(f.key, [.init(account, .alreadyPresent, mutation: .remove)]))
        XCTAssertTrue(try row(removing, "one").issues.contains(.removalRequested))
        XCTAssertFalse(try row(removing, "one").canAssign)
        XCTAssertEqual(f.disk.bytes(.catalog), before)
    }

    func testReportsCannotReplaceAnotherPhoneKeyIdentityOrForgeAnOperationBinding() async throws {
        let f = try await fixture(), other = try WorkspaceDeviceKey.generate().key
        let changedID = WorkspaceDeviceKey(id: other.id, fingerprint: f.key.fingerprint, publicBlob: f.key.publicBlob)
        let changedMaterial = WorkspaceDeviceKey(id: f.key.id, fingerprint: other.fingerprint, publicBlob: other.publicBlob)
        for key in [changedID, changedMaterial] {
            do {
                _ = try await review(f, report(key, [.init(target(one), .installed)]))
                XCTFail("the report substituted a different canonical phone identity")
            } catch { XCTAssertEqual(error as? WorkspaceKeyEnrollmentReportError, .phoneKeyMismatch) }
        }
        let bytes = try report(f.key, [.init(target(one), .installed)])
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: bytes) as? [String: Any])
        var results = try XCTUnwrap(root["results"] as? [[String: Any]])
        results[0]["operation_id"] = String(repeating: "0", count: 64); root["results"] = results
        do {
            _ = try await review(f, JSONSerialization.data(withJSONObject: root))
            XCTFail("a report detached its result from the exact request and target")
        } catch { }
        let loaded = try await f.store.load()
        XCTAssertTrue(try XCTUnwrap(loaded.first).deviceKeyAssignments.isEmpty)
    }

    func testStrictReportBoundsAndFieldsRejectBeforeAnyAssignment() async throws {
        let f = try await fixture()
        let bytes = try report(f.key, [.init(target(one), .installed)])
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: bytes) as? [String: Any])
        root["unexpected"] = true
        let invalid = [
            Data(), Data(repeating: 32, count: WorkspaceKeyEnrollmentJournal.maximumBytes + 1),
            try JSONSerialization.data(withJSONObject: root),
            Data(#"{"format":"ft-phone-key-enrollment-journal","format":"other"}"#.utf8)
        ]
        let before = f.disk.bytes(.catalog)
        for candidate in invalid {
            do { _ = try await review(f, candidate); XCTFail("malformed or oversized report was accepted") }
            catch { }
        }
        XCTAssertEqual(f.disk.bytes(.catalog), before)
    }

    func testLegacyDirectReportStillRequiresExplicitRouteApproval() async throws {
        let f = try await fixture()
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with:
            report(f.key, [.init(target(one), .installed)])) as? [String: Any])
        root["version"] = 1
        var results = try XCTUnwrap(root["results"] as? [[String: Any]])
        results[0].removeValue(forKey: "action"); results[0].removeValue(forKey: "last_mutation")
        root["results"] = results
        let reviewed = try await review(f, JSONSerialization.data(withJSONObject: root))
        XCTAssertTrue(try row(reviewed, "one").canAssign)
        for (selection, confirmed) in [(Set<String>(), true), (["one"], false), (["unknown"], true)] {
            do {
                _ = try await f.store.applyEnrollmentResults(reviewed, routeIDs: selection, confirmedByUser: confirmed)
                XCTFail("missing, unconfirmed or unknown selection published")
            } catch { }
        }
        let loaded = try await f.store.load()
        XCTAssertTrue(try XCTUnwrap(loaded.first).deviceKeyAssignments.isEmpty)
    }

    func testStoreIssuerStaleRevisionAndKeyRevocationFenceAReviewedFile() async throws {
        let f = try await fixture()
        let reviewed = try await review(f, report(f.key, [.init(target(one), .installed)]))
        let anotherStore = WorkspaceStore(persistence: f.disk, vault: f.vault)
        do {
            _ = try await anotherStore.applyEnrollmentResults(reviewed, routeIDs: ["one"], confirmedByUser: true)
            XCTFail("a different store accepted a foreign review token")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
        _ = try await f.store.assignDeviceKey(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            credentialID: f.key.id, routeID: "one", confirmedByUser: true)
        do {
            _ = try await f.store.applyEnrollmentResults(reviewed, routeIDs: ["one"], confirmedByUser: true)
            XCTFail("a changed authorization revision accepted an old review")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
        let g = try await fixture()
        let beforeRevocation = try await review(g, report(g.key, [.init(target(one), .installed)]))
        try await g.store.revokeLocally(fingerprint: g.key.fingerprint)
        do {
            _ = try await g.store.applyEnrollmentResults(beforeRevocation, routeIDs: ["one"], confirmedByUser: true)
            XCTFail("a revoked phone key became a route credential")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .revokedIdentity) }
    }

    func testApplyRechecksActualVaultAvailabilityAndPublicIdentity() async throws {
        for substitute in [false, true] {
            let f = try await fixture()
            let reviewed = try await review(f, report(f.key, [.init(target(one), .installed)]))
            let account = try XCTUnwrap(f.saved.bindings.first { $0.credentialID == f.key.id }).account
            if substitute {
                let other = try WorkspaceDeviceKey.generate().key
                f.vault.substitutePublicIdentity(account: account, identity: try SSHPublicIdentity(blob: other.publicBlob))
            } else { try f.vault.remove(account: account) }
            do {
                _ = try await f.store.applyEnrollmentResults(reviewed, routeIDs: ["one"], confirmedByUser: true)
                XCTFail("missing or substituted protected identity was accepted")
            } catch {
                XCTAssertEqual(error as? SSHAuthenticationError, substitute ? .identityMismatch : .unavailable)
            }
            let loaded = try await f.store.load()
            XCTAssertTrue(try XCTUnwrap(loaded.first).deviceKeyAssignments.isEmpty)
        }
    }

    func testMandatoryRouteAndSourcePoliciesCannotBeDiscardedByAReceipt() async throws {
        var required = one; required.required = ["fixture-required-policy"]
        let f = try await fixture([required])
        let blocked = try await review(f, report(f.key, [.init(target(one), .installed)]))
        XCTAssertTrue(try row(blocked, "one").issues.contains(.unsupportedPolicy))
        let g = try await fixture(findings: [.init(entityID: "mac-key", setting: "authentication_policy",
            disposition: 1, reason: "A mandatory source identity policy is unavailable.")])
        let unresolved = try await review(g, report(g.key, [.init(target(one), .installed)]))
        XCTAssertTrue(try row(unresolved, "one").issues.contains(.unsupportedPolicy))
        do {
            _ = try await g.store.applyEnrollmentResults(unresolved, routeIDs: ["one"], confirmedByUser: true)
            XCTFail("public receipt erased a mandatory source policy")
        } catch { XCTAssertEqual(error as? WorkspaceKeyEnrollmentReportError, .unavailableSelection) }
    }

    func testHostRevocationAfterReviewCannotBecomeTrustApproval() async throws {
        let f = try await fixture()
        let reviewed = try await review(f, report(f.key, [.init(target(one), .installed)]))
        let trust = try XCTUnwrap(try f.saved.importedDocument().hostTrust.first)
        try await f.store.revokeHostLocally(workspaceID: f.saved.id, trustID: trust.id,
            key: XCTUnwrap(trust.pins.first))
        do {
            _ = try await f.store.applyEnrollmentResults(reviewed, routeIDs: ["one"], confirmedByUser: true)
            XCTFail("a report review survived a newer local host rejection")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
        let updated = try await review(f, report(f.key, [.init(target(one), .installed)]))
        XCTAssertFalse(try row(updated, "one").canAssign)
    }

    func testBeforeAndAfterPublicationFailuresHaveAllOrNoneRouteChoices() async throws {
        for written in [false, true] {
            let f = try await fixture([one, two])
            let reviewed = try await review(f, report(f.key, [.init(target(one), .installed), .init(target(two), .installed)]))
            f.disk.configure(written ? .after(.catalog) : .before(.catalog))
            do {
                _ = try await f.store.applyEnrollmentResults(reviewed, routeIDs: ["one", "two"], confirmedByUser: true)
                XCTFail("the configured catalog write fault was ignored")
            } catch { }
            let loaded = try await WorkspaceStore(persistence: f.disk, vault: f.vault).load()
            let actual = try XCTUnwrap(loaded.first)
            XCTAssertEqual(Set(actual.deviceKeyAssignments.map(\.routeID)), written ? ["one", "two"] : [])
            XCTAssertEqual(actual.revision == f.saved.revision, !written)
            if written {
                XCTAssertTrue(actual.connectLater)
                do {
                    _ = try await f.store.applyEnrollmentResults(reviewed, routeIDs: ["one", "two"], confirmedByUser: true)
                    XCTFail("a published-but-unacknowledged write was automatically replayable")
                } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
            }
        }
    }

    func testNewIncomingPrefixOnReimportRequiresFreshPhoneKeyReview() async throws {
        let left = Spec("left", "left.fixture.invalid"), right = Spec("right", "right.fixture.invalid")
        let jump = Spec("jump", "10.0.0.8")
        let first = Spec("first", "10.0.0.9", jumps: ["left", "jump"])
        let second = Spec("second", "10.0.0.9", jumps: ["right", "jump"])
        let f = try await fixture([left, jump, first], machines: [("left-machine", ["first"])])
        let reviewed = try await review(f, report(f.key, [.init(path(jump, through: [left]).target, .installed)]))
        let applied = try await f.store.applyEnrollmentResults(reviewed, routeIDs: ["jump"], confirmedByUser: true)
        XCTAssertFalse(try applied.deviceKeyAssignmentNeedsReview(routeID: "jump"))
        let next = try document([left, right, jump, first, second],
            machines: [("left-machine", ["first"]), ("right-machine", ["second"])], bundle: "new-context")
        let replaced = try await replace(f, saved: applied, document: next)
        XCTAssertTrue(try replaced.deviceKeyAssignmentNeedsReview(routeID: "jump"))
        let effective = try XCTUnwrap(try replaced.document().routes.first { $0.id == "jump" })
        XCTAssertEqual(effective.credentialIDs, ["mac-key"])
        XCTAssertTrue(effective.requiredCapabilities.contains(WorkspaceDeviceKeyOverlay.reviewCapability))
        let partial = try await review(f, report(f.key, [.init(path(jump, through: [left]).target, .installed)]), saved: replaced)
        XCTAssertFalse(try row(partial, "jump").canAssign)
    }

    func testChangingAnIncomingBastionOrExposingStandaloneInvalidatesTheChoice() throws {
        let left = Spec("left", "left.fixture.invalid"), jump = Spec("jump", "10.0.0.8")
        let final = Spec("final", "10.0.0.9", jumps: ["left", "jump"])
        let initial = try document([left, jump, final], machines: [("machine", ["final"])])
        let witness = try WorkspaceDeviceKeyOverlay.witness(routeID: "jump", in: initial)
        let replacement = Spec("left", "replacement.fixture.invalid")
        let changed = try document([replacement, jump, final], machines: [("machine", ["final"])])
        XCTAssertNotEqual(try WorkspaceDeviceKeyOverlay.witness(routeID: "jump", in: changed), witness)
        let exposed = try document([left, jump, final], machines: [("machine", ["final", "jump"])])
        XCTAssertNotEqual(try WorkspaceDeviceKeyOverlay.witness(routeID: "jump", in: exposed), witness)
    }

    func testCosmeticReimportPreservesReviewedIncomingContexts() async throws {
        let left = Spec("left", "left.fixture.invalid"), jump = Spec("jump", "10.0.0.8")
        let final = Spec("final", "10.0.0.9", jumps: ["left", "jump"])
        let f = try await fixture([left, jump, final], machines: [("machine", ["final"])])
        let reviewed = try await review(f, report(f.key, [.init(path(jump, through: [left]).target, .installed)]))
        let applied = try await f.store.applyEnrollmentResults(reviewed, routeIDs: ["jump"], confirmedByUser: true)
        let renamed = try document([left, jump, final], machines: [("machine", ["final"])],
            bundle: "cosmetic", name: "Renamed Fleet")
        let replaced = try await replace(f, saved: applied, document: renamed)
        XCTAssertEqual(replaced.deviceKeyAssignments, applied.deviceKeyAssignments)
        XCTAssertFalse(try replaced.deviceKeyAssignmentNeedsReview(routeID: "jump"))
        XCTAssertEqual(try replaced.document().routes.first { $0.id == "jump" }?.credentialIDs, [f.key.id])
    }

    func testLegacyDirectWitnessRemainsCompatible() throws {
        let source = try document([one])
        // The published v1 direct witness, computed independently from the
        // fixed public fixture with sorted UTF-8 JSON and SHA-256.
        let expected = try Data(ftHex: "a706977b4342f630b5a6ec5293f05e951f02d773ce678e8814c9667f4b45cec8")
        XCTAssertEqual(try WorkspaceDeviceKeyOverlay.witness(routeID: "one", in: source), expected)
    }
}
