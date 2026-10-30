import Foundation
import XCTest
@testable import FrankenTermKit

/// Exercise the real import, local-policy, recovery and authorization paths.
/// Only persistence and the protected-key boundary inject faults here.
@MainActor
final class WorkspaceHostKeyPinTests: XCTestCase {
    private struct Fixture {
        let store: WorkspaceStore
        let disk: StoreTestPersistence
        let vault: StoreTestVault
        let archive: AirGapArchive
        let saved: InstalledWorkspace
    }

    private final class ProbeClock: @unchecked Sendable {
        private let lock = NSLock()
        private var value = ContinuousClock.now
        func now() -> ContinuousClock.Instant { lock.withLock { value } }
        func advance(_ duration: Duration) { lock.withLock { value += duration } }
    }

    private let crypto = TestCryptography()
    private let digest = Data(repeating: 201, count: 32)
    private var support: WorkspaceDocument.RouteSupport {
        .init(routeModes: ["direct_ssh", "frankenterm_mux"], authenticationMethods: [.publicKey],
              credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"], maximumJumpHops: 1)
    }

    private func hostKey(_ byte: UInt8 = 0, algorithm: String = "ssh-ed25519") throws -> WorkspaceDocument.HostKey {
        try .init(algorithm: algorithm,
                  fingerprint: "SHA256:" + Data(repeating: byte, count: 32).base64EncodedString().replacingOccurrences(of: "=", with: ""))
    }

    private func keyFields(_ key: WorkspaceDocument.HostKey) -> [String: Any] {
        ["algorithm": key.algorithm, "fingerprint": key.fingerprint]
    }

    private func changed(_ document: WorkspaceDocument, bundle: String,
                         sourceKind: String? = nil, edit: (inout [String: Any]) -> Void = { _ in }) throws -> WorkspaceDocument {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: document.raw) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        edit(&workspace); root["workspace"] = workspace; root["bundle_id"] = bundle
        if let sourceKind {
            var source = try XCTUnwrap(root["source"] as? [String: Any])
            source["kind"] = sourceKind; root["source"] = source
        }
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }

    private func archive(bundle: String = "bundle", keys: Bool = true,
                         findings: [AirGapArchive.Portability] = [],
                         edit: (inout [String: Any]) -> Void = { _ in }) throws -> AirGapArchive {
        let (key, fingerprint) = try sampleKey(using: crypto)
        let source = try WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint, bundleID: bundle))
        let document = try changed(source, bundle: bundle) { workspace in
            var trust = workspace["host_trust"] as! [[String: Any]]
            trust[0]["pins"] = []; workspace["host_trust"] = trust
            edit(&workspace)
        }
        return try AirGapArchive(workspace: document, keys: keys ? [key] : [], portability: findings, using: crypto)
    }

    private func fixture(_ source: AirGapArchive? = nil) async throws -> Fixture {
        let archive = try source ?? self.archive()
        let disk = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: disk, vault: vault)
        let saved = try await store.install(archive, envelopeDigest: digest,
            expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        return .init(store: store, disk: disk, vault: vault, archive: archive, saved: saved)
    }

    private func jumpArchive(methods: [String] = ["publickey"], destinationCredential: Bool = true,
                             keys: Bool = true) throws -> AirGapArchive {
        let pin = keyFields(try hostKey(17))
        return try archive(keys: keys) { workspace in
            var routes = workspace["routes"] as! [[String: Any]], hop = routes[0]
            hop["id"] = "jump"; hop["host"] = "jump.example.invalid"; hop["host_trust_ref"] = "hop-trust"
            hop["authentication_methods"] = methods
            // Transit authentication must not require a terminal creation policy.
            hop.removeValue(forKey: "session_creation")
            var unrelated = hop; unrelated["id"] = "unrelated"
            routes[0]["jump_routes"] = ["jump"]
            if !destinationCredential { routes[0]["credential_refs"] = [] }
            routes += [hop, unrelated]; workspace["routes"] = routes
            var trusts = workspace["host_trust"] as! [[String: Any]], trust = trusts[0]
            trust["id"] = "hop-trust"; trust["host"] = "jump.example.invalid"; trust["pins"] = [pin]
            trusts.append(trust); workspace["host_trust"] = trusts
        }
    }

    private func probe(_ f: Fixture, support: WorkspaceDocument.RouteSupport? = nil) async throws -> WorkspaceHostKeyProbe {
        try await f.store.beginHostKeyVerification(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: "route", confirmedByUser: true, support: support ?? self.support)
    }

    private func hopAuthorization(_ f: Fixture, probe: WorkspaceHostKeyProbe?,
        routeID: String = "jump", destination: String? = "route",
        support: WorkspaceDocument.RouteSupport? = nil) async throws -> WorkspaceSSHAuthorization {
        try await f.store.authorizeSSHAuthentication(workspaceID: f.saved.id, expectedRevision: f.saved.revision,
            routeID: routeID, credentialID: "key", presentedHostKey: hostKey(17), support: support ?? self.support,
            transitForRouteID: destination, hostKeyProbe: probe)
    }

    private func review(_ fixture: Fixture, saved: InstalledWorkspace? = nil,
                        key: WorkspaceDocument.HostKey? = nil) async throws -> WorkspaceHostKeyPinReview {
        let saved = saved ?? fixture.saved
        return try await fixture.store.reviewHostKeyPin(workspaceID: saved.id, expectedRevision: saved.revision,
            routeID: "route", key: key ?? hostKey(), support: support)
    }

    private func accept(_ fixture: Fixture, saved: InstalledWorkspace? = nil) async throws -> InstalledWorkspace {
        let review = try await review(fixture, saved: saved)
        return try await fixture.store.acceptHostKeyPin(review, confirmedByUser: true, support: support)
    }

    private func replace(_ fixture: Fixture, saved: InstalledWorkspace, document: WorkspaceDocument,
                         digestByte: UInt8, allowRollback: Bool = false) async throws -> InstalledWorkspace {
        let source = try AirGapArchive(workspace: document, keys: [], using: crypto)
        return try await fixture.store.install(source, envelopeDigest: Data(repeating: digestByte, count: 32),
            expectedRevision: saved.revision, sourceAndContentsConfirmed: true,
            allowRollback: allowRollback, transferID: nil, securityChangesConfirmed: true,
            associateWithExistingWorkspace: true).workspace
    }

    private func catalog(_ disk: StoreTestPersistence) throws -> [String: Any] {
        try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(disk.bytes(.catalog))) as? [String: Any])
    }

    func testAcceptedPinPersistsWithoutChangingImportedBytesAndRequiresFreshConnect() async throws {
        let f = try await fixture()
        let approved = try await f.store.approveConnections(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, confirmedByUser: true)
        XCTAssertFalse(approved.connectLater)
        let before = f.disk.bytes(.catalog)
        let document = try await f.store.hostKeyVerificationDocument(workspaceID: approved.id,
            expectedRevision: approved.revision, routeID: "route", support: support)
        XCTAssertTrue(document.hostTrust[0].pins.isEmpty)
        let checked = try await review(f, saved: approved)
        XCTAssertEqual(checked.host, "host.example.invalid")
        XCTAssertEqual(checked.port, 22)
        XCTAssertEqual(checked.key, try hostKey())
        XCTAssertEqual(f.disk.bytes(.catalog), before, "preflight and review must not publish trust")

        let pinned = try await f.store.acceptHostKeyPin(checked, confirmedByUser: true, support: support)
        XCTAssertEqual(pinned.id, f.saved.id)
        XCTAssertNotEqual(pinned.revision, approved.revision)
        XCTAssertTrue(pinned.connectLater)
        XCTAssertEqual(pinned.manifest, f.archive.workspace.raw)
        XCTAssertEqual(pinned.envelopeDigest, digest)
        XCTAssertEqual(pinned.bindings, f.saved.bindings)
        XCTAssertTrue(try pinned.importedDocument().hostTrust[0].pins.isEmpty)
        XCTAssertEqual(try pinned.document().hostTrust[0].pins, [try hostKey()])
        XCTAssertEqual(pinned.hostKeyPins.count, 1)
        XCTAssertEqual(f.vault.count, 1); XCTAssertEqual(f.vault.creates, 1)
        XCTAssertNil(f.disk.bytes(.journal))

        let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault)
        let rows = try await reopened.loadWithReadiness(support: support)
        let row = try XCTUnwrap(rows.first)
        XCTAssertEqual(row.workspace.revision, pinned.revision)
        XCTAssertEqual(row.workspace.hostKeyPins, pinned.hostKeyPins)
        XCTAssertTrue(row.workspace.connectLater)
        XCTAssertTrue(try XCTUnwrap(row.routes.first).isLocallyReady)
        let decision = try await reopened.verifyHostKey(workspaceID: pinned.id, routeID: "route",
            presented: hostKey(), support: support)
        XCTAssertEqual(decision, .trustedPin)
        do {
            _ = try await reopened.approveConnections(workspaceID: pinned.id,
                expectedRevision: approved.revision, confirmedByUser: true)
            XCTFail("an old connection approval authorized the new trust revision")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
    }

    func testVerificationNeedsNeitherAUsableCredentialNorVaultAccess() async throws {
        let f = try await fixture(archive(keys: false))
        f.vault.configure(.readFingerprint)
        _ = try await f.store.hostKeyVerificationDocument(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, routeID: "route", support: support)
        let pinned = try await accept(f)
        XCTAssertEqual(f.vault.count, 0); XCTAssertEqual(f.vault.creates, 0)
        let readiness = try await f.store.routeReadiness(workspaceID: pinned.id, support: support)
        XCTAssertEqual(readiness.first?.issues, [.credentialUnavailable])
        XCTAssertTrue(pinned.connectLater)
    }

    func testConfirmationAndStoreIssuedReviewAreRequired() async throws {
        let f = try await fixture(), checked = try await review(f)
        let before = f.disk.bytes(.catalog)
        do {
            _ = try await f.store.acceptHostKeyPin(checked, confirmedByUser: false, support: support)
            XCTFail("unconfirmed host trust was published")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .reviewRequired) }
        let anotherOwner = WorkspaceStore(persistence: f.disk, vault: f.vault)
        do {
            _ = try await anotherOwner.acceptHostKeyPin(checked, confirmedByUser: true, support: support)
            XCTFail("a different store accepted an old review authority")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
        XCTAssertEqual(f.disk.bytes(.catalog), before)
        XCTAssertEqual(f.vault.creates, 1)
    }

    func testPinsCannotReplaceEitherImportedTrustOrAnAcceptedLocalPin() async throws {
        let key = try hostKey(), different = try hostKey(17)
        let imported = try archive { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["pins"] = [keyFields(key)]; workspace["host_trust"] = trusts
        }
        let importedFixture = try await fixture(imported)
        let localFixture = try await fixture()
        let local = try await accept(localFixture)
        for (f, saved) in [(importedFixture, importedFixture.saved), (localFixture, local)] {
            let before = f.disk.bytes(.catalog)
            for offered in [key, different] {
                do {
                    _ = try await review(f, saved: saved, key: offered)
                    XCTFail("first-use review replaced an already configured host")
                } catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .alreadyConfigured) }
            }
            XCTAssertEqual(f.disk.bytes(.catalog), before)
            let mismatch = try await f.store.verifyHostKey(workspaceID: saved.id, routeID: "route",
                presented: different, support: support)
            XCTAssertEqual(mismatch, .pinMismatch)
        }
    }

    func testReviewIsInvalidatedByReplacementAndBySuccessfulAcceptance() async throws {
        let f = try await fixture(), checked = try await review(f)
        let renamed = try changed(f.archive.workspace, bundle: "renamed") { $0["name"] = "Renamed machines" }
        let replacement = try await replace(f, saved: f.saved, document: renamed, digestByte: 202)
        do {
            _ = try await f.store.acceptHostKeyPin(checked, confirmedByUser: true, support: support)
            XCTFail("a review for an older revision was accepted")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
        let fresh = try await review(f, saved: replacement)
        let pinned = try await f.store.acceptHostKeyPin(fresh, confirmedByUser: true, support: support)
        do {
            _ = try await f.store.acceptHostKeyPin(fresh, confirmedByUser: true, support: support)
            XCTFail("a consumed review was replayed")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
        let saved = try await f.store.load()
        XCTAssertEqual(saved.first?.revision, pinned.revision)
    }

    func testUnknownWorkspaceRouteAndStaleRevisionCannotBeginVerification() async throws {
        let f = try await fixture()
        for (id, revision, route) in [(UUID(), f.saved.revision, "route"),
                                      (f.saved.id, UUID(), "route"),
                                      (f.saved.id, f.saved.revision, "unknown-route")] {
            do {
                _ = try await f.store.hostKeyVerificationDocument(workspaceID: id,
                    expectedRevision: revision, routeID: route, support: support)
                XCTFail("unbound verification preflight was accepted")
            } catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
        }
    }

    func testLocalRevocationAfterReviewWinsEvenWhenItsWriteFails() async throws {
        let faults: [StoreTestPersistence.Fault?] = [nil, .before(.catalog), .after(.catalog)]
        for fault in faults {
            let f = try await fixture(), checked = try await review(f)
            let before = f.disk.bytes(.catalog)
            f.disk.configure(fault)
            do {
                try await f.store.revokeHostLocally(workspaceID: f.saved.id, trustID: "trust", key: hostKey())
                XCTAssertNil(fault, "the requested rejection-publication fault did not occur")
            } catch { XCTAssertEqual(error as? FTError, .storage) }
            if case .some(.before(_)) = fault { XCTAssertEqual(f.disk.bytes(.catalog), before) }
            let refused = f.disk.bytes(.catalog)
            do {
                _ = try await f.store.acceptHostKeyPin(checked, confirmedByUser: true, support: support)
                XCTFail("a late host rejection was replaced by earlier positive review")
            } catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .revoked) }
            do {
                _ = try await review(f)
                XCTFail("a fresh review cleared a persistent or process-local refusal")
            } catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .revoked) }
            XCTAssertEqual(f.disk.bytes(.catalog), refused)
            XCTAssertEqual(f.vault.count, 1); XCTAssertEqual(f.vault.creates, 1)
        }
    }

    func testImportedSameScopeRevocationAndAnotherWorkspaceRejectionBlockPins() async throws {
        let key = try hostKey()
        let source = try archive { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]], denial = trusts[0]
            denial["id"] = "negative-trust"
            // The fingerprint remains a denial even with a different algorithm label.
            denial["revoked_keys"] = [["algorithm": "ssh-rsa", "fingerprint": key.fingerprint]]
            trusts.append(denial); workspace["host_trust"] = trusts
        }
        let denied = try await fixture(source)
        do { _ = try await review(denied); XCTFail("a same-scope imported rejection was bypassed") }
        catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .revoked) }

        let f = try await fixture(), checked = try await review(f)
        let other = try archive(bundle: "other-workspace") { workspace in
            workspace["id"] = "other-workspace"
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["revoked_keys"] = [keyFields(key)]; workspace["host_trust"] = trusts
        }
        _ = try await f.store.install(other, envelopeDigest: Data(repeating: 203, count: 32),
            expectedRevision: nil, sourceAndContentsConfirmed: true)
        do {
            _ = try await f.store.acceptHostKeyPin(checked, confirmedByUser: true, support: support)
            XCTFail("another workspace's endpoint rejection was bypassed")
        } catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .revoked) }
        XCTAssertEqual(f.vault.count, 1, "host rejection must not duplicate or delete a shared SSH identity")
    }

    func testRequiredPoliciesAndUnsupportedAlgorithmsCannotBeSatisfiedByPinning() async throws {
        for field in ["host_trust", "routes"] {
            let source = try archive { workspace in
                var records = workspace[field] as! [[String: Any]]
                records[0]["required_capabilities"] = ["unavailable_host_policy"]
                workspace[field] = records
            }
            let f = try await fixture(source)
            do { _ = try await review(f); XCTFail("a required \(field) policy was bypassed") }
            catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .unsupportedPolicy) }
        }
        let source = try archive(findings: [.init(entityID: "route", setting: "desktop_policy",
            disposition: 1, reason: "The source has an unresolved mandatory security policy.")])
        let blocked = try await fixture(source)
        do { _ = try await review(blocked); XCTFail("an unresolved portability condition was bypassed") }
        catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .unsupportedPolicy) }

        let f = try await fixture()
        do { _ = try await review(f, key: hostKey(0, algorithm: "ssh-rsa")); XCTFail("an unsupported host algorithm was accepted") }
        catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .unsupportedPolicy) }
        do {
            _ = try await f.store.hostKeyVerificationDocument(workspaceID: f.saved.id,
                expectedRevision: f.saved.revision, routeID: "route", support: .offlineOnly)
            XCTFail("an unavailable transport was enabled for verification")
        } catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .unsupportedPolicy) }
    }

    func testRemovedRequiredHostPolicyRemainsABlockerAfterReimport() async throws {
        let source = try archive { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["required_capabilities"] = ["unavailable_host_policy"]; workspace["host_trust"] = trusts
        }
        let f = try await fixture(source)
        let narrowed = try changed(source.workspace, bundle: "without-policy") { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0].removeValue(forKey: "required_capabilities"); workspace["host_trust"] = trusts
        }
        let saved = try await replace(f, saved: f.saved, document: narrowed, digestByte: 204)
        do { _ = try await review(f, saved: saved); XCTFail("reimport erased mandatory host-policy history") }
        catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .unsupportedPolicy) }
    }

    func testDestinationThroughAnUnverifiedJumpCannotBeginVerification() async throws {
        let source = try archive { workspace in
            var routes = workspace["routes"] as! [[String: Any]], hop = routes[0]
            hop["id"] = "jump"
            routes[0]["jump_routes"] = ["jump"]; routes.append(hop); workspace["routes"] = routes
        }
        let f = try await fixture(source)
        do { _ = try await review(f); XCTFail("a jump destination was probed as a direct endpoint") }
        catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .jumpNotReady) }
    }

    func testProbeRequiresExplicitConsentAndGrantsOnlyExactTrustedTransit() async throws {
        let f = try await fixture(jumpArchive())
        let before = f.disk.bytes(.catalog)
        do {
            _ = try await f.store.beginHostKeyVerification(workspaceID: f.saved.id,
                expectedRevision: f.saved.revision, routeID: "route", confirmedByUser: false, support: support)
            XCTFail("unconfirmed probe granted transit authentication")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .reviewRequired) }
        do { _ = try await hopAuthorization(f, probe: nil); XCTFail("normal transit bypassed the destination's missing trust") }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }

        let permit = try await probe(f)
        let auth = try await hopAuthorization(f, probe: permit)
        XCTAssertEqual(auth.hostKeyProbe, permit)
        XCTAssertEqual(auth.transitForRouteID, "route")
        XCTAssertEqual(auth.presentedHostKey, try hostKey(17))
        try await f.store.validateSSHAuthentication(auth, support: support)
        let forbidden: [(String, String?)] = [("route", nil), ("jump", nil), ("unrelated", "route"), ("jump", "jump")]
        for (route, destination) in forbidden {
            do {
                _ = try await hopAuthorization(f, probe: permit, routeID: route, destination: destination)
                XCTFail("probe authorized unrelated or destination authentication: \(route)")
            } catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
        }
        XCTAssertEqual(f.disk.bytes(.catalog), before, "probing must not publish trust or connection consent")
        let saved = try await f.store.load()
        XCTAssertTrue(try XCTUnwrap(saved.first).connectLater)
        await f.store.endHostKeyVerification(permit)
        do { try await f.store.validateSSHAuthentication(auth, support: support); XCTFail("ended probe kept signing authority") }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
        do { _ = try await hopAuthorization(f, probe: nil); XCTFail("ending probe made normal login ready") }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
    }

    func testJumpProbeNeedsHopCredentialsButNoDestinationCredential() async throws {
        let f = try await fixture(jumpArchive(destinationCredential: false))
        let permit = try await probe(f)
        _ = try await hopAuthorization(f, probe: permit)
        await f.store.endHostKeyVerification(permit)
        let pinned = try await accept(f)
        XCTAssertEqual(pinned.manifest, f.saved.manifest)
        XCTAssertEqual(pinned.envelopeDigest, f.saved.envelopeDigest)
        XCTAssertTrue(pinned.connectLater)
        let states = try await f.store.routeReadiness(workspaceID: pinned.id, support: support)
        XCTAssertEqual(states.first(where: { $0.routeID == "route" })?.issues, [.credentialUnavailable])
        let missing = try await fixture(jumpArchive(keys: false))
        do { _ = try await probe(missing); XCTFail("a missing jump credential was ignored") }
        catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .jumpNotReady) }
    }

    func testProbeOwnsPasswordAndInteractiveTransitWithoutGrantingEitherAtDestination() async throws {
        let f = try await fixture(jumpArchive(methods: ["publickey", "password", "keyboard-interactive"]))
        let all = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh", "frankenterm_mux"],
            authenticationMethods: [.publicKey, .password, .keyboardInteractive], credentialAlgorithms: ["ssh-ed25519"],
            hostKeyAlgorithms: ["ssh-ed25519"], maximumJumpHops: 1)
        let permit = try await probe(f, support: all)
        let password = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, routeID: "jump", presentedHostKey: hostKey(17),
            support: all, transitForRouteID: "route", hostKeyProbe: permit)
        let interactive = try await f.store.authorizeSSHKeyboardInteractiveAuthentication(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, routeID: "jump", presentedHostKey: hostKey(17),
            support: all, transitForRouteID: "route", hostKeyProbe: permit)
        XCTAssertEqual(password.hostKeyProbe, permit); XCTAssertEqual(interactive.hostKeyProbe, permit)
        try await f.store.validateSSHPasswordAuthentication(password, support: all)
        try await f.store.validateSSHKeyboardInteractiveAuthentication(interactive, support: all)
        await f.store.endHostKeyVerification(permit)
        do { try await f.store.validateSSHPasswordAuthentication(password, support: all); XCTFail("ended probe retained a password grant") }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
        do { try await f.store.validateSSHKeyboardInteractiveAuthentication(interactive, support: all); XCTFail("ended probe retained an MFA grant") }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
    }

    func testProbeIsStoreOwnedBoundedAndExpiresWithoutRenewal() async throws {
        let original = try await fixture(jumpArchive()), clock = ProbeClock()
        let store = WorkspaceStore(persistence: original.disk, vault: original.vault, hostKeyProbeNow: { clock.now() })
        let f = Fixture(store: store, disk: original.disk, vault: original.vault, archive: original.archive, saved: original.saved)
        var permits: [WorkspaceHostKeyProbe] = []
        for _ in 0..<4 { permits.append(try await probe(f)) }
        let first = permits[0], auth = try await hopAuthorization(f, probe: first)
        do { _ = try await probe(f); XCTFail("probe capacity was unbounded") }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .busy) }
        await original.store.endHostKeyVerification(first)
        do { _ = try await original.store.validateHostKeyVerification(first, support: support); XCTFail("a different store accepted probe authority") }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
        clock.advance(.seconds(299))
        try await f.store.validateSSHAuthentication(auth, support: support)
        clock.advance(.seconds(1))
        do { try await f.store.validateSSHAuthentication(auth, support: support); XCTFail("validation extended the original probe deadline") }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
        let fresh = try await probe(f)
        XCTAssertNotEqual(fresh.id, first.id)
        _ = try await hopAuthorization(f, probe: fresh)
        await f.store.endHostKeyVerification(fresh)
    }

    func testProbeCannotSurviveHostOrSelectedCredentialRevocation() async throws {
        for host in [true, false] {
            let f = try await fixture(jumpArchive()), permit = try await probe(f)
            let auth = try await hopAuthorization(f, probe: permit)
            if host {
                f.disk.configure(.before(.catalog))
                do { try await f.store.revokeHostLocally(workspaceID: f.saved.id, trustID: "hop-trust", key: hostKey(17)); XCTFail("fault did not run") }
                catch { XCTAssertEqual(error as? FTError, .storage) }
            } else {
                try await f.store.revokeLocally(fingerprint: auth.fingerprint)
            }
            do { try await f.store.validateSSHAuthentication(auth, support: support); XCTFail("revocation left probe signing authority live") }
            catch { XCTAssertTrue(error is WorkspaceHostKeyPinError || error is SSHAuthenticationError) }
            do { _ = try await probe(f); XCTFail("revoked transit path received a new probe") }
            catch { XCTAssertEqual(error as? WorkspaceHostKeyPinError, .jumpNotReady) }
            await f.store.endHostKeyVerification(permit)
        }
    }

    func testProbeCannotSurviveWorkspaceReplacementOrHostPinPublication() async throws {
        for publishPin in [true, false] {
            let f = try await fixture(jumpArchive()), permit = try await probe(f)
            let auth = try await hopAuthorization(f, probe: permit)
            if publishPin { _ = try await accept(f) }
            else {
                let next = try changed(f.archive.workspace, bundle: "replaced-route") { $0["name"] = "Replaced" }
                _ = try await replace(f, saved: f.saved, document: next, digestByte: 234)
            }
            do { try await f.store.validateSSHAuthentication(auth, support: support); XCTFail("old probe survived a revision change") }
            catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
            do { _ = try await f.store.validateHostKeyVerification(permit, support: support); XCTFail("old probe context survived") }
            catch { XCTAssertEqual(error as? WorkspaceStoreError, .staleReview) }
            await f.store.endHostKeyVerification(permit)
        }
    }

    func testUnchangedReimportPreviewAndPublicationKeepTheSameVerifiedPin() async throws {
        let f = try await fixture(), pinned = try await accept(f)
        let next = try changed(f.archive.workspace, bundle: "same-policy") { $0["name"] = "Renamed workspace" }
        let source = try AirGapArchive(workspace: next, keys: [], using: crypto)
        let preview = try await f.store.previewImport(source, envelopeDigest: Data(repeating: 205, count: 32), support: support)
        XCTAssertTrue(try XCTUnwrap(preview.routes.first).isLocallyReady,
                      "preview must include the retained phone-local pin just as publication does")
        let replacement = try await replace(f, saved: pinned, document: next, digestByte: 205)
        XCTAssertEqual(replacement.hostKeyPins, pinned.hostKeyPins)
        XCTAssertEqual(replacement.manifest, next.raw)
        XCTAssertTrue(try replacement.importedDocument().hostTrust[0].pins.isEmpty)
        XCTAssertEqual(try replacement.document().hostTrust[0].pins, [try hostKey()])
        XCTAssertTrue(replacement.connectLater)
        let readiness = try await f.store.routeReadiness(workspaceID: replacement.id, support: support)
        XCTAssertTrue(try XCTUnwrap(readiness.first).isLocallyReady)
    }

    func testEndpointAliasAndSourceChangesRequireANewHostReview() async throws {
        for change in ["host", "port", "alias", "trust-id", "source-kind"] {
            let f = try await fixture(), pinned = try await accept(f)
            let next = try changed(f.archive.workspace, bundle: "changed-\(change)",
                sourceKind: change == "source-kind" ? "another_source" : nil) { workspace in
                var routes = workspace["routes"] as! [[String: Any]], trusts = workspace["host_trust"] as! [[String: Any]]
                switch change {
                case "host": routes[0]["host"] = "other.example.invalid"; trusts[0]["host"] = "other.example.invalid"
                case "port": routes[0]["port"] = 2222; trusts[0]["port"] = 2222
                case "alias": routes[0]["host_key_alias"] = "different-alias"; trusts[0]["host_key_alias"] = "different-alias"
                case "trust-id": routes[0]["host_trust_ref"] = "renamed-trust"; trusts[0]["id"] = "renamed-trust"
                default: break
                }
                workspace["routes"] = routes; workspace["host_trust"] = trusts
            }
            let source = try AirGapArchive(workspace: next, keys: [], using: crypto)
            let preview = try await f.store.previewImport(source, envelopeDigest: Data(repeating: 206, count: 32), support: support)
            XCTAssertEqual(preview.routes.first?.issues, [.hostTrustMissing], change)
            let replacement = try await replace(f, saved: pinned, document: next, digestByte: 206)
            XCTAssertTrue(try replacement.document().hostTrust[0].pins.isEmpty, change)
            XCTAssertEqual(replacement.hostKeyPins, pinned.hostKeyPins, "inactive local choices remain bounded metadata")
            let fresh = try await review(f, saved: replacement, key: hostKey(17))
            let reverified = try await f.store.acceptHostKeyPin(fresh, confirmedByUser: true, support: support)
            XCTAssertEqual(try reverified.document().hostTrust[0].pins, [try hostKey(17)], change)
            XCTAssertTrue(reverified.connectLater)
        }
    }

    func testNewImportedPinsTakePrecedenceOverStoredFirstUsePins() async throws {
        let f = try await fixture(), pinned = try await accept(f), replacementKey = try hostKey(17)
        let next = try changed(f.archive.workspace, bundle: "configured-by-source") { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["pins"] = [keyFields(replacementKey)]; workspace["host_trust"] = trusts
        }
        let replacement = try await replace(f, saved: pinned, document: next, digestByte: 207)
        XCTAssertEqual(replacement.hostKeyPins, pinned.hostKeyPins)
        XCTAssertEqual(try replacement.document().hostTrust[0].pins, [replacementKey])
        let old = try await f.store.verifyHostKey(workspaceID: replacement.id, routeID: "route", presented: hostKey(), support: support)
        let new = try await f.store.verifyHostKey(workspaceID: replacement.id, routeID: "route", presented: replacementKey, support: support)
        XCTAssertEqual(old, .pinMismatch); XCTAssertEqual(new, .trustedPin)
    }

    func testRollbackCannotResurrectARevokedPhonePin() async throws {
        let f = try await fixture(), pinned = try await accept(f)
        let changedHost = try changed(f.archive.workspace, bundle: "different-host") { workspace in
            var routes = workspace["routes"] as! [[String: Any]], trusts = workspace["host_trust"] as! [[String: Any]]
            routes[0]["host"] = "other.example.invalid"; trusts[0]["host"] = "other.example.invalid"
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }
        let later = try await replace(f, saved: pinned, document: changedHost, digestByte: 208)
        let key = try hostKey()
        try await f.store.revokeHostKeyLocally(host: "HOST.EXAMPLE.INVALID.", port: 22,
            algorithm: key.algorithm, fingerprint: key.fingerprint)
        let rollback = try await replace(f, saved: later, document: f.archive.workspace, digestByte: 201, allowRollback: true)
        let decision = try await f.store.verifyHostKey(workspaceID: rollback.id, routeID: "route", presented: key, support: support)
        XCTAssertEqual(decision, .revoked)
        let states = try await f.store.routeReadiness(workspaceID: rollback.id, support: support)
        XCTAssertEqual(states.first?.issues, [.hostIdentityRevoked])
        XCTAssertTrue(rollback.connectLater)
    }

    func testHostPinningPreservesAnExplicitPhoneIdentityAssignment() async throws {
        let f = try await fixture()
        let generated = try await f.store.createDeviceKey(workspaceID: f.saved.id,
            expectedRevision: f.saved.revision, confirmedByUser: true)
        let assigned = try await f.store.assignDeviceKey(workspaceID: f.saved.id,
            expectedRevision: generated.workspace.revision, credentialID: generated.key.id,
            routeID: "route", confirmedByUser: true)
        let pinned = try await accept(f, saved: assigned)
        XCTAssertEqual(pinned.deviceKeyAssignments, assigned.deviceKeyAssignments)
        XCTAssertEqual(try pinned.document().routes[0].credentialIDs, [generated.key.id])
        XCTAssertEqual(try pinned.importedDocument().routes[0].credentialIDs, ["key"])
        let states = try await f.store.routeReadiness(workspaceID: pinned.id, support: support)
        XCTAssertTrue(try XCTUnwrap(states.first).isLocallyReady)
        XCTAssertEqual(f.vault.creates, 2)
    }

    func testInterruptedPinPublicationLeavesOldOrCompleteNewCatalogAndNeverTouchesKeys() async throws {
        for (fault, published) in [(StoreTestPersistence.Fault.before(.catalog), false), (.after(.catalog), true)] {
            let f = try await fixture()
            let approved = try await f.store.approveConnections(workspaceID: f.saved.id,
                expectedRevision: f.saved.revision, confirmedByUser: true)
            let checked = try await review(f, saved: approved)
            f.disk.configure(fault)
            do {
                _ = try await f.store.acceptHostKeyPin(checked, confirmedByUser: true, support: support)
                XCTFail("the selected catalog publication fault did not occur")
            } catch { XCTAssertEqual(error as? FTError, .storage) }
            let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault)
            let saved = try await reopened.load()
            let current = try XCTUnwrap(saved.first)
            XCTAssertEqual(current.hostKeyPins.count, published ? 1 : 0)
            XCTAssertEqual(current.revision == approved.revision, !published)
            XCTAssertEqual(current.connectLater, published)
            XCTAssertEqual(current.manifest, f.saved.manifest)
            XCTAssertEqual(current.bindings, f.saved.bindings)
            XCTAssertEqual(f.vault.count, 1); XCTAssertEqual(f.vault.creates, 1)
            XCTAssertNil(f.disk.bytes(.journal))
        }
    }

    func testVersionSevenMigratesWithoutInventingHostTrustOrChangingHistory() async throws {
        let f = try await fixture()
        var legacy = try catalog(f.disk), workspaces = try XCTUnwrap(legacy["workspaces"] as? [[String: Any]])
        legacy["version"] = 7
        workspaces[0].removeValue(forKey: "hostKeyPins"); legacy["workspaces"] = workspaces
        let history = try JSONSerialization.data(withJSONObject: XCTUnwrap(legacy["acceptedImports"]), options: [.sortedKeys])
        f.disk.seed(try JSONSerialization.data(withJSONObject: legacy, options: [.sortedKeys]), slot: .catalog)
        let reopened = WorkspaceStore(persistence: f.disk, vault: f.vault)
        let loaded = try await reopened.load(), saved = try XCTUnwrap(loaded.first)
        XCTAssertEqual(saved.revision, f.saved.revision)
        XCTAssertEqual(saved.bindings, f.saved.bindings)
        XCTAssertEqual(saved.manifest, f.saved.manifest)
        XCTAssertTrue(saved.hostKeyPins.isEmpty)
        XCTAssertTrue(try saved.document().hostTrust[0].pins.isEmpty)
        let migrated = try catalog(f.disk)
        XCTAssertEqual(migrated["version"] as? Int, 8)
        XCTAssertEqual(try JSONSerialization.data(withJSONObject: XCTUnwrap(migrated["acceptedImports"]), options: [.sortedKeys]), history)
        let stable = f.disk.bytes(.catalog)
        _ = try await WorkspaceStore(persistence: f.disk, vault: f.vault).load()
        XCTAssertEqual(f.disk.bytes(.catalog), stable)
        XCTAssertEqual(f.vault.count, 1); XCTAssertEqual(f.vault.creates, 1)
    }

    func testCorruptOrOverBudgetLocalPinMetadataFailsClosedWithoutKeyCleanup() async throws {
        for corruption in ["duplicate", "count", "witness", "fingerprint", "identifier", "legacy-version"] {
            let f = try await fixture()
            _ = try await accept(f)
            var fields = try catalog(f.disk), workspaces = try XCTUnwrap(fields["workspaces"] as? [[String: Any]])
            var pins = try XCTUnwrap(workspaces[0]["hostKeyPins"] as? [[String: Any]])
            switch corruption {
            case "duplicate": pins.append(pins[0])
            case "count":
                let original = pins[0]
                pins = (0...256).map { index in var pin = original; pin["trustID"] = "trust-\(index)"; return pin }
            case "witness": pins[0]["sourceWitness"] = Data(repeating: 0, count: 31).base64EncodedString()
            case "fingerprint": pins[0]["key"] = ["algorithm": "ssh-ed25519", "fingerprint": "not-a-fingerprint"]
            case "identifier": pins[0]["trustID"] = "invalid\nidentifier"
            default: fields["version"] = 7
            }
            workspaces[0]["hostKeyPins"] = pins; fields["workspaces"] = workspaces
            let corrupt = try JSONSerialization.data(withJSONObject: fields, options: [.sortedKeys])
            f.disk.seed(corrupt, slot: .catalog)
            do {
                _ = try await WorkspaceStore(persistence: f.disk, vault: f.vault).load()
                XCTFail("\(corruption) host-pin metadata was accepted")
            } catch {}
            XCTAssertEqual(f.disk.bytes(.catalog), corrupt, corruption)
            XCTAssertEqual(f.vault.count, 1, corruption); XCTAssertEqual(f.vault.creates, 1, corruption)
            XCTAssertNil(f.disk.bytes(.journal))
        }
    }
}
