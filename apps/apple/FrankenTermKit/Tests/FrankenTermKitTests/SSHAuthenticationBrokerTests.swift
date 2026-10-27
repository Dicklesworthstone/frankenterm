import Foundation
import XCTest
@testable import FrankenTermKit

/// The delayed boundary performs the real production signature after release.
/// Cancellation intentionally does not settle it immediately, modeling a native
/// authentication callback that still owns resources while cancellation arrives.
private final class DelayedSSHSigner: WorkspaceSSHSigning, @unchecked Sendable {
    private struct Pending {
        let id: UUID, challenge: SSHUserAuthenticationChallenge?
        let continuation: CheckedContinuation<Data, Error>
    }
    private let lock = NSLock()
    private let container: Data
    private let delayIdentity: Bool
    private var pending: Pending?
    private var observer: CheckedContinuation<Void, Never>?
    private var callCount = 0, cancellationCount = 0
    init(_ container: Data, delayIdentity: Bool = false) { self.container = container; self.delayIdentity = delayIdentity }
    var calls: Int { lock.lock(); defer { lock.unlock() }; return callCount }
    var cancellations: Int { lock.lock(); defer { lock.unlock() }; return cancellationCount }
    func publicIdentity(requestID: UUID, authorization: WorkspaceSSHAuthorization) async throws -> Data {
        if !delayIdentity { return try SSHUserAuthenticationSigner.publicIdentity(privateContainer: container).blob }
        return try await wait(requestID: requestID, challenge: nil)
    }
    func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
              challenge: SSHUserAuthenticationChallenge) async throws -> Data {
        try await wait(requestID: requestID, challenge: challenge)
    }
    private func wait(requestID: UUID, challenge: SSHUserAuthenticationChallenge?) async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            lock.lock()
            guard pending == nil else { lock.unlock(); continuation.resume(throwing: SSHAuthenticationError.busy); return }
            callCount += 1
            pending = Pending(id: requestID, challenge: challenge, continuation: continuation)
            let observer = self.observer; self.observer = nil
            lock.unlock()
            observer?.resume()
        }
    }
    func cancel(requestID: UUID) {
        lock.lock(); defer { lock.unlock() }
        if pending?.id == requestID { cancellationCount += 1 }
    }
    func waitUntilStarted() async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if pending != nil { lock.unlock(); continuation.resume() }
            else { observer = continuation; lock.unlock() }
        }
    }
    func finish() {
        lock.lock(); let selected = pending; pending = nil; lock.unlock()
        guard let selected else { return }
        selected.continuation.resume(with: Result {
            if let challenge = selected.challenge {
                return try SSHUserAuthenticationSigner.sign(challenge, privateContainer: container)
            }
            return try SSHUserAuthenticationSigner.publicIdentity(privateContainer: container).blob
        })
    }
}

private struct ImmediateSSHSigner: WorkspaceSSHSigning {
    let container: Data
    func cancel(requestID: UUID) {}
    func publicIdentity(requestID: UUID, authorization: WorkspaceSSHAuthorization) async throws -> Data {
        try SSHUserAuthenticationSigner.publicIdentity(privateContainer: container).blob
    }
    func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
              challenge: SSHUserAuthenticationChallenge) async throws -> Data {
        try SSHUserAuthenticationSigner.sign(challenge, privateContainer: container)
    }
}

/// A prompt that deliberately retains its continuation after cancellation,
/// proving the broker keeps the sole prompt slot until its owner settles.
private final class DelayedSSHPasswordPrompt: WorkspaceSSHPasswordPrompting, @unchecked Sendable {
    private let lock = NSLock()
    private var pending: (WorkspaceSSHPasswordRequest, CheckedContinuation<Data, Error>)?
    private var observer: CheckedContinuation<Void, Never>?
    private var callCount = 0, cancellationCount = 0
    var calls: Int { lock.lock(); defer { lock.unlock() }; return callCount }
    var cancellations: Int { lock.lock(); defer { lock.unlock() }; return cancellationCount }
    func password(for request: WorkspaceSSHPasswordRequest) async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            lock.lock()
            guard pending == nil else { lock.unlock(); continuation.resume(throwing: SSHAuthenticationError.busy); return }
            callCount += 1; pending = (request, continuation)
            let observer = self.observer; self.observer = nil
            lock.unlock(); observer?.resume()
        }
    }
    func cancel(requestID: UUID) {
        lock.lock(); defer { lock.unlock() }
        if pending?.0.id == requestID { cancellationCount += 1 }
    }
    func waitUntilStarted() async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if pending != nil { lock.unlock(); continuation.resume() }
            else { observer = continuation; lock.unlock() }
        }
    }
    func finish(_ bytes: Data = Data("one-use password".utf8)) {
        lock.lock(); let selected = pending; pending = nil; lock.unlock()
        selected?.1.resume(returning: bytes)
    }
}

@MainActor
final class SSHAuthenticationBrokerTests: XCTestCase {
    private let crypto = TestCryptography()
    private let support = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
        credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"])
    private struct Fixture {
        let store: WorkspaceStore, persistence: StoreTestPersistence, vault: StoreTestVault
        let installed: InstalledWorkspace, archive: AirGapArchive, key: PrivateKeyRecord
        let pin: WorkspaceDocument.HostKey
    }
    private func fixture(includeKeys: Bool = true, mutate: (inout [String: Any]) -> Void = { _ in }) async throws -> Fixture {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let (key, fingerprint) = try sampleKey(using: crypto)
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest(fingerprint: fingerprint)) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        mutate(&workspace); root["workspace"] = workspace
        let document = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
        let archive = try AirGapArchive(workspace: document, keys: includeKeys ? [key] : [], using: crypto)
        let installed = try await store.install(archive, envelopeDigest: Data(repeating: 1, count: 32),
            transferID: Data(repeating: 2, count: 16), expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        return .init(store: store, persistence: persistence, vault: vault, installed: installed,
                     archive: archive, key: key, pin: try XCTUnwrap(document.hostTrust[0].pins.first))
    }
    private func prepare(_ broker: SSHAuthenticationBroker, _ f: Fixture, transportID: UUID = UUID()) async throws -> SSHAuthenticationRequest {
        try await broker.prepare(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
            routeID: "route", credentialID: "key", presentedHostKey: f.pin, transportID: transportID,
            sessionIdentifier: Data(repeating: 0xB7, count: 32), publicBlob: f.key.publicBlob)
    }
    private func expectFailure(_ body: () async throws -> Void, file: StaticString = #filePath, line: UInt = #line) async {
        do { try await body(); XCTFail("Expected an authentication refusal", file: file, line: line) }
        catch {}
    }
    private func waitUntilQueued(_ count: Int, broker: SSHAuthenticationBroker,
                                 file: StaticString = #filePath, line: UInt = #line) async {
        let deadline = ContinuousClock.now + .seconds(3)
        while ContinuousClock.now < deadline {
            if await broker.queuedRequests == count { return }
            await Task.yield()
        }
        XCTFail("Authentication requests did not reach the expected bounded queue", file: file, line: line)
    }

    private var jumpSupport: WorkspaceDocument.RouteSupport {
        .init(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
              credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"], maximumJumpHops: 1)
    }
    private func transitFixture(mode: String = "direct_ssh", mutate: (inout [String: Any]) -> Void = { _ in }) async throws -> Fixture {
        try await fixture { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            var hop = routes[0]
            hop["id"] = "hop"; hop["host"] = "jump.example.invalid"; hop["mode"] = mode
            hop["host_trust_ref"] = "hop-trust"; hop["session_creation"] = nil
            routes[0]["jump_routes"] = ["hop"]
            routes.append(hop); workspace["routes"] = routes
            var trusts = workspace["host_trust"] as! [[String: Any]]
            var trust = trusts[0]; trust["id"] = "hop-trust"; trust["host"] = "jump.example.invalid"
            trusts.append(trust); workspace["host_trust"] = trusts
            mutate(&workspace)
        }
    }

    private var multiJumpSupport: WorkspaceDocument.RouteSupport {
        .init(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
              credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"], maximumJumpHops: 3)
    }

    private func multiTransitFixture(nested: Bool = false) async throws -> Fixture {
        try await fixture { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            var trusts = workspace["host_trust"] as! [[String: Any]]
            let baseRoute = routes[0], baseTrust = trusts[0]
            routes[0]["jump_routes"] = nested ? ["hop3"] : ["hop1", "hop2", "hop3"]
            for index in 1...3 {
                var hop = baseRoute, trust = baseTrust
                hop["id"] = "hop\(index)"; hop["host"] = "jump\(index).example.invalid"
                hop["host_trust_ref"] = "hop-trust\(index)"; hop["session_creation"] = nil
                hop["jump_routes"] = nested && index > 1 ? ["hop\(index - 1)"] : [] as [String]
                trust["id"] = "hop-trust\(index)"; trust["host"] = "jump\(index).example.invalid"
                routes.append(hop); trusts.append(trust)
            }
            // A valid independent route is still not a transit dependency.
            var unrelated = baseRoute
            unrelated["id"] = "unrelated"; routes.append(unrelated)
            workspace["routes"] = routes; workspace["host_trust"] = trusts
        }
    }

    func testThreeHopAuthorizationUsesReviewedOrderAndExactDestinationScope() async throws {
        for nested in [false, true] {
            let f = try await multiTransitFixture(nested: nested)
            XCTAssertEqual(try f.archive.workspace.orderedJumpRoutes(forRouteID: "route").map(\.id),
                           ["hop1", "hop2", "hop3"])
            let broker = SSHAuthenticationBroker(store: f.store, signer: ImmediateSSHSigner(container: f.key.container),
                                                 support: multiJumpSupport)
            await broker.resume()
            for index in 1...3 {
                let authorization = try await broker.authorization(workspaceID: f.installed.id,
                    expectedRevision: f.installed.revision, routeID: "hop\(index)", credentialID: "key",
                    presentedHostKey: f.pin, transitForRouteID: "route")
                XCTAssertEqual(authorization.host, "jump\(index).example.invalid")
                XCTAssertEqual(authorization.transitForRouteID, "route")
                XCTAssertEqual(authorization.jumpRouteIDs, nested && index > 1 ? ["hop\(index - 1)"] : [])
                await expectFailure {
                    _ = try await broker.authorization(workspaceID: f.installed.id,
                        expectedRevision: f.installed.revision, routeID: "hop\(index)", credentialID: "key",
                        presentedHostKey: f.pin)
                }
            }
            await expectFailure {
                _ = try await broker.authorization(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                    routeID: "unrelated", credentialID: "key", presentedHostKey: f.pin, transitForRouteID: "route")
            }
        }
    }

    func testThreeHopTransitCannotBypassSmallerTransportProfile() async throws {
        let f = try await multiTransitFixture()
        let signer = DelayedSSHSigner(f.key.container, delayIdentity: true)
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: jumpSupport)
        await broker.resume()
        for index in 1...3 {
            await expectFailure {
                _ = try await broker.publicIdentity(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                    routeID: "hop\(index)", credentialID: "key", presentedHostKey: f.pin, transportID: UUID(),
                    transitForRouteID: "route")
            }
        }
        XCTAssertEqual(signer.calls, 0)
    }

    func testAnyRevokedAncestorInvalidatesEveryTransitGrantForTheDestination() async throws {
        for revokedIndex in 1...3 {
            let f = try await multiTransitFixture(nested: true)
            let broker = SSHAuthenticationBroker(store: f.store, signer: ImmediateSSHSigner(container: f.key.container),
                                                 support: multiJumpSupport)
            await broker.resume()
            let selected = try await broker.publicIdentity(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                routeID: "hop3", credentialID: "key", presentedHostKey: f.pin, transportID: UUID(), transitForRouteID: "route")
            try await broker.validate(selected)
            try await f.store.revokeHostLocally(workspaceID: f.installed.id, trustID: "hop-trust\(revokedIndex)", key: f.pin)
            await expectFailure { try await broker.validate(selected) }
            for index in 1...3 {
                await expectFailure {
                    _ = try await broker.authorization(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                        routeID: "hop\(index)", credentialID: "key", presentedHostKey: f.pin, transitForRouteID: "route")
                }
            }
        }
    }

    func testTransitAuthorizationRetainsExactDestinationWithoutLeafSessionPolicy() async throws {
        // A mux preference is inert for a transit hop, even when this profile
        // only enables direct destination shells. Neither case needs a PTY.
        for mode in ["direct_ssh", "frankenterm_mux"] {
            let f = try await transitFixture(mode: mode)
            let broker = SSHAuthenticationBroker(store: f.store, signer: ImmediateSSHSigner(container: f.key.container),
                                                 support: jumpSupport)
            await broker.resume()
            await expectFailure {
                _ = try await broker.authorization(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                    routeID: "hop", credentialID: "key", presentedHostKey: f.pin)
            }
            let selected = try await broker.publicIdentity(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                routeID: "hop", credentialID: "key", presentedHostKey: f.pin, transportID: UUID(), transitForRouteID: "route")
            XCTAssertEqual(selected.authorization.transitForRouteID, "route")
            XCTAssertEqual(selected.authorization.host, "jump.example.invalid")
            let request = try await broker.prepare(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                routeID: "hop", credentialID: "key", presentedHostKey: f.pin, transportID: UUID(),
                sessionIdentifier: Data(repeating: 0x91, count: 32), publicBlob: f.key.publicBlob, transitForRouteID: "route")
            let signed = try await broker.confirm(requestID: request.id)
            XCTAssertEqual(signed.authorization, selected.authorization)
            XCTAssertFalse(signed.signatureBlob.isEmpty)
            try await broker.validate(selected)
        }
    }

    func testTransitAuthorizationRejectsUnrelatedScopesAndRequiredPolicyBeforeSigner() async throws {
        for variant in 0..<5 {
            let f = try await transitFixture { workspace in
                var routes = workspace["routes"] as! [[String: Any]]
                if variant == 1 { routes[0]["jump_routes"] = [] as [String] }
                if variant == 2 { routes[1]["required_capabilities"] = ["unimplemented.hop.policy"] }
                if variant == 3 { routes[0]["required_capabilities"] = ["unimplemented.destination.policy"] }
                workspace["routes"] = routes
            }
            let signer = DelayedSSHSigner(f.key.container, delayIdentity: true)
            let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: variant == 4 ? support : jumpSupport)
            await broker.resume()
            await expectFailure {
                _ = try await broker.publicIdentity(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                    routeID: "hop", credentialID: "key", presentedHostKey: f.pin, transportID: UUID(),
                    transitForRouteID: variant == 0 ? "hop" : "route")
            }
            XCTAssertEqual(signer.calls, 0)
        }
    }

    func testTransitSignatureCannotOutliveHopRevocationOrDestinationRevision() async throws {
        for replace in [false, true] {
            let f = try await transitFixture(), signer = DelayedSSHSigner(f.key.container)
            let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: jumpSupport)
            await broker.resume()
            let request = try await broker.prepare(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                routeID: "hop", credentialID: "key", presentedHostKey: f.pin, transportID: UUID(),
                sessionIdentifier: Data(repeating: 0x42, count: 32), publicBlob: f.key.publicBlob, transitForRouteID: "route")
            let signing = Task { try await broker.confirm(requestID: request.id) }
            await signer.waitUntilStarted()
            defer { signer.finish() }
            if replace {
                let replacement = try AirGapArchive(workspace: f.archive.workspace.reissuingBundle(id: "replacement-hop"),
                                                   keys: [f.key], using: crypto)
                _ = try await f.store.install(replacement, envelopeDigest: Data(repeating: 8, count: 32),
                    transferID: Data(repeating: 9, count: 16), expectedRevision: f.installed.revision,
                    sourceAndContentsConfirmed: true, securityChangesConfirmed: true, associateWithExistingWorkspace: true)
            } else {
                try await f.store.revokeHostLocally(workspaceID: f.installed.id, trustID: "hop-trust", key: f.pin)
            }
            signer.finish()
            await expectFailure { _ = try await signing.value }
        }
    }

    func testConcurrentIdentityLookupsUseOneProtectedWorkerInFIFOOrder() async throws {
        let f = try await fixture(), signer = DelayedSSHSigner(try sampleKey(using: crypto).0.container, delayIdentity: true)
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
        await broker.resume()
        func lookup() -> Task<WorkspaceSSHIdentity, Error> {
            Task { try await broker.publicIdentity(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                routeID: "route", credentialID: "key", presentedHostKey: f.pin, transportID: UUID()) }
        }
        let first = lookup()
        await signer.waitUntilStarted()
        let second = lookup()
        await waitUntilQueued(1, broker: broker)
        let third = lookup()
        await waitUntilQueued(2, broker: broker)
        XCTAssertEqual(signer.calls, 1)
        signer.finish()
        _ = try await first.value
        await signer.waitUntilStarted()
        XCTAssertEqual(signer.calls, 2)
        let waiting = await broker.queuedRequests
        XCTAssertEqual(waiting, 1)
        signer.finish()
        _ = try await second.value
        await signer.waitUntilStarted()
        XCTAssertEqual(signer.calls, 3)
        signer.finish()
        _ = try await third.value
        let busy = await broker.isBusy
        XCTAssertFalse(busy)
    }

    func testQueuedCancellationNeverUnlocksOrOvertakesActiveSignature() async throws {
        let f = try await fixture(), signer = DelayedSSHSigner(try sampleKey(using: crypto).0.container)
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
        await broker.resume()
        let current = try await prepare(broker, f)
        let signing = Task { try await broker.confirm(requestID: current.id) }
        await signer.waitUntilStarted()
        let cancelled = Task { try await self.prepare(broker, f) }
        await waitUntilQueued(1, broker: broker)
        let transport = UUID()
        let retired = Task { try await self.prepare(broker, f, transportID: transport) }
        await waitUntilQueued(2, broker: broker)
        cancelled.cancel()
        await expectFailure { _ = try await cancelled.value }
        await broker.cancel(transportID: transport)
        await expectFailure { _ = try await retired.value }
        let waiting = await broker.queuedRequests
        XCTAssertEqual(waiting, 0)
        XCTAssertEqual(signer.calls, 1); XCTAssertEqual(signer.cancellations, 0)
        signer.finish()
        _ = try await signing.value
    }

    func testSuspensionRetiresEveryQueuedRequestAndNeverResumesTheirPrompt() async throws {
        let f = try await fixture(), signer = DelayedSSHSigner(try sampleKey(using: crypto).0.container)
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
        await broker.resume()
        let current = try await prepare(broker, f)
        let signing = Task { try await broker.confirm(requestID: current.id) }
        await signer.waitUntilStarted()
        let waiting = Task { try await self.prepare(broker, f) }
        await waitUntilQueued(1, broker: broker)
        await broker.suspend(); await broker.resume()
        await expectFailure { _ = try await waiting.value }
        await expectFailure { _ = try await self.prepare(broker, f) }
        signer.finish()
        await expectFailure { _ = try await signing.value }
        XCTAssertEqual(signer.calls, 1)
        let next = try await prepare(broker, f)
        await broker.cancel(requestID: next.id)
    }

    func testQueuedAuthorityIsRecheckedAfterWaitingBeforeAnyPrivateKeyAccess() async throws {
        let f = try await fixture(), signer = DelayedSSHSigner(try sampleKey(using: crypto).0.container, delayIdentity: true)
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
        await broker.resume()
        let active = Task { try await broker.publicIdentity(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
            routeID: "route", credentialID: "key", presentedHostKey: f.pin, transportID: UUID()) }
        await signer.waitUntilStarted()
        let queued = Task { try await self.prepare(broker, f) }
        await waitUntilQueued(1, broker: broker)
        try await f.store.revokeHostLocally(workspaceID: f.installed.id, trustID: "trust", key: f.pin)
        signer.finish()
        await expectFailure { _ = try await active.value }
        await expectFailure { _ = try await queued.value }
        XCTAssertEqual(signer.calls, 1)
        let busy = await broker.isBusy
        XCTAssertFalse(busy)
    }

    func testQueueCapacityAndAbandonedRequestDeadlineRemainBounded() async throws {
        let f = try await fixture()
        let broker = SSHAuthenticationBroker(store: f.store, signer: ImmediateSSHSigner(container: f.key.container), support: support)
        await broker.resume()
        let abandoned = try await prepare(broker, f)
        var waiting: [Task<SSHAuthenticationRequest, Error>] = []
        for count in 1...32 {
            waiting.append(Task { try await self.prepare(broker, f) })
            await waitUntilQueued(count, broker: broker)
        }
        await expectFailure { _ = try await self.prepare(broker, f) }
        await broker.expireRequests(at: ContinuousClock.now + .seconds(61))
        for request in waiting { await expectFailure { _ = try await request.value } }
        await expectFailure { _ = try await broker.confirm(requestID: abandoned.id) }
        let count = await broker.queuedRequests, busy = await broker.isBusy
        XCTAssertEqual(count, 0); XCTAssertFalse(busy)
        let fresh = try await prepare(broker, f)
        _ = try await broker.confirm(requestID: fresh.id)
    }

    func testForegroundExplicitOneUseRequestPreservesAllTransportBindings() async throws {
        let f = try await fixture()
        let broker = SSHAuthenticationBroker(store: f.store, signer: ImmediateSSHSigner(container: f.key.container), support: support)
        await expectFailure { _ = try await self.prepare(broker, f) }
        await broker.resume()
        let transport = UUID(), request = try await prepare(broker, f, transportID: transport)
        XCTAssertEqual(request.authorization.host, "host.example.invalid")
        XCTAssertEqual(request.authorization.user, "operator")
        XCTAssertEqual(request.authorization.credentialID, "key")
        let result = try await broker.confirm(requestID: request.id)
        XCTAssertEqual(result.requestID, request.id); XCTAssertEqual(result.transportID, transport)
        XCTAssertEqual(result.authorization, request.authorization)
        XCTAssertEqual(result.sessionIdentifier, Data(repeating: 0xB7, count: 32))
        XCTAssertEqual(result.publicBlob, f.key.publicBlob)
        var signature = ByteCursor(result.signatureBlob)
        XCTAssertEqual(try signature.sshString(), Data("ssh-ed25519".utf8))
        XCTAssertEqual(try signature.sshString().count, 64)
        XCTAssertEqual(signature.remaining, 0)
        let busy = await broker.isBusy
        XCTAssertFalse(busy)
        await expectFailure { _ = try await broker.confirm(requestID: request.id) }
        XCTAssertTrue(f.installed.connectLater, "Signing never changes imported connection preferences.")
        XCTAssertEqual(f.vault.creates, 1, "Signing does not create another private identity.")
    }

    func testPublicIdentityUsesStoredKeyAndCannotSurviveForegroundReplacement() async throws {
        let f = try await fixture()
        let signer = ImmediateSSHSigner(container: f.key.container)
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
        await broker.resume()
        let selected = try await broker.publicIdentity(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
            routeID: "route", credentialID: "key", presentedHostKey: f.pin, transportID: UUID())
        XCTAssertEqual(selected.identity.blob, f.key.publicBlob)
        XCTAssertEqual(selected.identity.fingerprint, f.archive.workspace.credentials[0].fingerprint)
        XCTAssertEqual(selected.authorization.routeMode, "direct_ssh")
        XCTAssertTrue(selected.authorization.jumpRouteIDs.isEmpty)
        try await broker.validate(selected)
        let other = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
        await other.resume()
        await expectFailure { try await other.validate(selected) }
        await broker.suspend(); await broker.resume()
        await expectFailure { try await broker.validate(selected) }
        XCTAssertEqual(f.vault.creates, 1)
    }

    func testRetiredPublicLookupRetainsWorkerUntilItSettles() async throws {
        for revocation in [false, true] {
            let f = try await fixture(), signer = DelayedSSHSigner(try sampleKey(using: crypto).0.container, delayIdentity: true)
            let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
            await broker.resume()
            let transport = UUID()
            let lookup = Task { try await broker.publicIdentity(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                routeID: "route", credentialID: "key", presentedHostKey: f.pin, transportID: transport) }
            await signer.waitUntilStarted()
            if revocation { try await f.store.revokeLocally(fingerprint: f.archive.workspace.credentials[0].fingerprint) }
            else {
                await broker.cancel(transportID: transport)
                await broker.suspend(); await broker.resume()
                await expectFailure { _ = try await self.prepare(broker, f) }
                XCTAssertGreaterThan(signer.cancellations, 0)
            }
            signer.finish()
            await expectFailure { _ = try await lookup.value }
            let busy = await broker.isBusy
            XCTAssertFalse(busy)
        }
    }

    func testUntrustedPublicLookupDoesNotReachProtectedReader() async throws {
        let f = try await fixture(), signer = DelayedSSHSigner(try sampleKey(using: crypto).0.container, delayIdentity: true)
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
        await broker.resume()
        try await f.store.revokeHostLocally(workspaceID: f.installed.id, trustID: "trust", key: f.pin)
        await expectFailure {
            _ = try await broker.publicIdentity(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                routeID: "route", credentialID: "key", presentedHostKey: f.pin, transportID: UUID())
        }
        XCTAssertEqual(signer.calls, 0)
    }

    func testNoQualificationNoPermittedCredentialOrWrongActualHostCannotPrompt() async throws {
        let f = try await fixture(), signer = DelayedSSHSigner(try sampleKey(using: crypto).0.container)
        let offline = SSHAuthenticationBroker(store: f.store, signer: signer, support: .offlineOnly)
        await offline.resume()
        await expectFailure { _ = try await self.prepare(offline, f) }
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
        await broker.resume()
        let wrong = try WorkspaceDocument.HostKey(algorithm: "ssh-ed25519",
            fingerprint: "SHA256:ERERERERERERERERERERERERERERERERERERERERERE")
        do {
            _ = try await broker.prepare(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                routeID: "route", credentialID: "key", presentedHostKey: wrong, transportID: UUID(),
                sessionIdentifier: Data(repeating: 9, count: 32), publicBlob: f.key.publicBlob)
            XCTFail("An unpinned host key was accepted")
        } catch { XCTAssertEqual(error as? SSHAuthenticationError, .hostKeyMismatch) }
        let passwordOnly = try await fixture { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["authentication_methods"] = ["password"]
            workspace["routes"] = routes
        }
        let other = SSHAuthenticationBroker(store: passwordOnly.store, signer: signer, support: support)
        await other.resume()
        await expectFailure { _ = try await self.prepare(other, passwordOnly) }
        XCTAssertEqual(signer.calls, 0)
    }

    func testRequiredPolicyAndLocallyRevokedIdentitiesAreDeniedBeforePrompt() async throws {
        for variant in 0..<3 {
            let f = try await fixture { workspace in
                if variant == 0 {
                    var routes = workspace["routes"] as! [[String: Any]]
                    routes[0]["required_capabilities"] = ["source.unresolved_required_policy"]
                    workspace["routes"] = routes
                }
            }
            if variant == 1 { try await f.store.revokeLocally(fingerprint: f.archive.workspace.credentials[0].fingerprint) }
            if variant == 2 { try await f.store.revokeHostLocally(workspaceID: f.installed.id, trustID: "trust", key: f.pin) }
            let signer = DelayedSSHSigner(f.key.container)
            let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
            await broker.resume()
            await expectFailure { _ = try await self.prepare(broker, f) }
            XCTAssertEqual(signer.calls, 0)
        }
    }

    func testPublicKeyMismatchAndUnboundedChallengeFailBeforePrompt() async throws {
        let f = try await fixture(), signer = DelayedSSHSigner(try sampleKey(using: crypto).0.container)
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
        await broker.resume()
        let different = publicBlob(try crypto.ed25519PublicKey(seed: Data(repeating: 3, count: 32)))
        for (session, blob) in [(Data(repeating: 8, count: 32), different),
                                (Data(repeating: 0, count: 65), f.key.publicBlob),
                                (Data(repeating: 0, count: 32), Data(repeating: 0, count: 513))] {
            await expectFailure {
                _ = try await broker.prepare(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                    routeID: "route", credentialID: "key", presentedHostKey: f.pin, transportID: UUID(),
                    sessionIdentifier: session, publicBlob: blob)
            }
        }
        XCTAssertEqual(signer.calls, 0)
        let request = try await prepare(broker, f)
        await broker.cancel(requestID: request.id)
    }

    func testRevocationBetweenReviewAndConfirmationCannotReachSigner() async throws {
        let f = try await fixture(), signer = DelayedSSHSigner(try sampleKey(using: crypto).0.container)
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
        await broker.resume()
        let request = try await prepare(broker, f)
        try await f.store.revokeLocally(fingerprint: request.authorization.fingerprint)
        await expectFailure { _ = try await broker.confirm(requestID: request.id) }
        XCTAssertEqual(signer.calls, 0)
    }

    func testRevocationDuringUnlockDiscardsTheActualCompletedSignature() async throws {
        for host in [false, true] {
            let f = try await fixture(), signer = DelayedSSHSigner(try sampleKey(using: crypto).0.container)
            let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
            await broker.resume()
            let request = try await prepare(broker, f)
            let task = Task { try await broker.confirm(requestID: request.id) }
            await signer.waitUntilStarted()
            if host { try await f.store.revokeHostKeyLocally(host: "HOST.EXAMPLE.INVALID.", port: 22,
                                                           algorithm: f.pin.algorithm, fingerprint: f.pin.fingerprint) }
            else { try await f.store.revokeLocally(fingerprint: request.authorization.fingerprint) }
            signer.finish()
            await expectFailure { _ = try await task.value }
            let busy = await broker.isBusy
            XCTAssertFalse(busy)
        }
    }

    func testReimportDuringUnlockCannotPublishToTheNewRoute() async throws {
        let f = try await fixture(), signer = DelayedSSHSigner(try sampleKey(using: crypto).0.container)
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
        await broker.resume()
        let request = try await prepare(broker, f)
        let task = Task { try await broker.confirm(requestID: request.id) }
        await signer.waitUntilStarted()
        let replacement = try AirGapArchive(workspace: f.archive.workspace.reissuingBundle(id: "replacement"),
                                             keys: [], using: crypto)
        let replaced = try await f.store.install(replacement, envelopeDigest: Data(repeating: 4, count: 32),
            transferID: Data(repeating: 5, count: 16), expectedRevision: f.installed.revision,
            sourceAndContentsConfirmed: true, securityChangesConfirmed: true, associateWithExistingWorkspace: true).workspace
        XCTAssertNotEqual(replaced.revision, f.installed.revision)
        signer.finish()
        await expectFailure { _ = try await task.value }
    }

    func testSuspendCancelAndResumeRetainTheOnlyWorkerUntilItSettles() async throws {
        let f = try await fixture(), signer = DelayedSSHSigner(try sampleKey(using: crypto).0.container)
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
        await broker.resume()
        let request = try await prepare(broker, f)
        let task = Task { try await broker.confirm(requestID: request.id) }
        await signer.waitUntilStarted()
        await expectFailure { _ = try await broker.confirm(requestID: request.id) }
        await broker.suspend()
        await broker.resume()
        await expectFailure { _ = try await self.prepare(broker, f) }
        let busy = await broker.isBusy
        XCTAssertTrue(busy); XCTAssertEqual(signer.calls, 1); XCTAssertGreaterThan(signer.cancellations, 0)
        signer.finish()
        await expectFailure { _ = try await task.value }
        let next = try await prepare(broker, f)
        await broker.cancel(requestID: request.id) // a stale UI dismissal
        let stillBusy = await broker.isBusy
        XCTAssertTrue(stillBusy)
        await broker.cancel(requestID: next.id)
    }

    func testCallerTaskCancellationCannotLeakAnEventuallyCompletedSignature() async throws {
        let f = try await fixture(), signer = DelayedSSHSigner(try sampleKey(using: crypto).0.container)
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
        await broker.resume()
        let request = try await prepare(broker, f)
        let task = Task { try await broker.confirm(requestID: request.id) }
        await signer.waitUntilStarted()
        task.cancel()
        signer.finish()
        await expectFailure { _ = try await task.value }
        XCTAssertGreaterThan(signer.cancellations, 0)
    }

    func testProtectedMetadataFailureAfterUnlockFailsClosed() async throws {
        let f = try await fixture(), signer = DelayedSSHSigner(try sampleKey(using: crypto).0.container)
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
        await broker.resume()
        let request = try await prepare(broker, f)
        let task = Task { try await broker.confirm(requestID: request.id) }
        await signer.waitUntilStarted()
        f.vault.configure(.readFingerprint)
        signer.finish()
        await expectFailure { _ = try await task.value }
    }

    func testAuthorizationCannotCrossStoreInstancesOrExpectedRevisions() async throws {
        let f = try await fixture()
        let authorization = try await f.store.authorizeSSHAuthentication(workspaceID: f.installed.id,
            expectedRevision: f.installed.revision, routeID: "route", credentialID: "key", presentedHostKey: f.pin, support: support)
        let reopened = WorkspaceStore(persistence: f.persistence, vault: f.vault)
        await expectFailure { try await reopened.validateSSHAuthentication(authorization, support: self.support) }
        await expectFailure {
            _ = try await f.store.authorizeSSHAuthentication(workspaceID: f.installed.id,
                expectedRevision: UUID(), routeID: "route", credentialID: "key", presentedHostKey: f.pin, support: self.support)
        }
    }

    private var passwordSupport: WorkspaceDocument.RouteSupport {
        .init(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey, .password],
              credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"], maximumJumpHops: 3)
    }
    private func passwordFixture() async throws -> Fixture {
        try await fixture(includeKeys: false) { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["authentication_methods"] = ["password"]
            routes[0]["credential_refs"] = [] as [String]
            workspace["routes"] = routes
        }
    }
    private func passwordIdentity(_ broker: SSHAuthenticationBroker, _ f: Fixture) async throws -> WorkspaceSSHPasswordIdentity {
        try await broker.passwordIdentity(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                                          routeID: "route", presentedHostKey: f.pin)
    }

    func testPasswordRouteWithoutImportedKeysAuthorizesOnlyItsVerifiedAccount() async throws {
        let f = try await passwordFixture(), signer = DelayedSSHSigner(f.key.container), prompt = DelayedSSHPasswordPrompt()
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: passwordSupport)
        await broker.resume()
        let selected = try await passwordIdentity(broker, f)
        XCTAssertEqual(selected.authorization.routeID, "route")
        let states = try await f.store.routeReadiness(workspaceID: f.installed.id, support: passwordSupport)
        XCTAssertTrue(try XCTUnwrap(states.first).isLocallyReady)
        XCTAssertTrue(try XCTUnwrap(states.first).usableCredentialIDs.isEmpty)
        let task = Task { try await broker.password(for: selected, transportID: UUID(), prompter: prompt) }
        await prompt.waitUntilStarted()
        prompt.finish()
        let bytes = try await task.value
        XCTAssertEqual(bytes, Data("one-use password".utf8))
        XCTAssertEqual(signer.calls, 0)
        XCTAssertEqual(prompt.calls, 1)
        try await broker.validatePasswordAttempt(selected)
    }

    func testPasswordAuthorizationRequiresPermittedMethodProfilePinAndStoreOwner() async throws {
        let ordinary = try await fixture(), password = try await passwordFixture()
        for (f, profile, pin) in [(ordinary, passwordSupport, ordinary.pin), (password, support, password.pin),
            (password, passwordSupport, try WorkspaceDocument.HostKey(algorithm: "ssh-ed25519",
                fingerprint: "SHA256:" + Data(repeating: 9, count: 32).base64EncodedString().replacingOccurrences(of: "=", with: "")))] {
            await expectFailure {
                _ = try await f.store.authorizeSSHPasswordAuthentication(workspaceID: f.installed.id,
                    expectedRevision: f.installed.revision, routeID: "route", presentedHostKey: pin, support: profile)
            }
        }
        let authorization = try await password.store.authorizeSSHPasswordAuthentication(workspaceID: password.installed.id,
            expectedRevision: password.installed.revision, routeID: "route", presentedHostKey: password.pin, support: passwordSupport)
        let reopened = WorkspaceStore(persistence: password.persistence, vault: password.vault)
        await expectFailure { try await reopened.validateSSHPasswordAuthentication(authorization, support: self.passwordSupport) }
    }

    func testPasswordForegroundRetiresPendingPromptWithoutRevivingItOnResume() async throws {
        let f = try await passwordFixture(), prompt = DelayedSSHPasswordPrompt()
        let broker = SSHAuthenticationBroker(store: f.store, signer: ImmediateSSHSigner(container: f.key.container), support: passwordSupport)
        await broker.resume()
        let selected = try await passwordIdentity(broker, f)
        let task = Task { try await broker.password(for: selected, transportID: UUID(), prompter: prompt) }
        await prompt.waitUntilStarted()
        await broker.setPasswordForeground(false)
        XCTAssertEqual(prompt.cancellations, 1)
        let busy = await broker.isBusy
        XCTAssertTrue(busy)
        await broker.setPasswordForeground(true)
        prompt.finish()
        await expectFailure { _ = try await task.value }
        await expectFailure { try await broker.validatePasswordAttempt(selected) }
        let fresh = try await passwordIdentity(broker, f)
        try await broker.validatePasswordAttempt(fresh)
        // A completed native login may keep the ordinary live policy witness;
        // its adapter, not this broker call, proves the success transition.
        try await broker.validate(selected)
    }

    func testPasswordReimportAndHostRevocationRejectLatePromptResults() async throws {
        for replace in [false, true] {
            let f = try await passwordFixture(), prompt = DelayedSSHPasswordPrompt()
            let broker = SSHAuthenticationBroker(store: f.store, signer: ImmediateSSHSigner(container: f.key.container), support: passwordSupport)
            await broker.resume()
            let selected = try await passwordIdentity(broker, f)
            let task = Task { try await broker.password(for: selected, transportID: UUID(), prompter: prompt) }
            await prompt.waitUntilStarted()
            if replace {
                let replacement = try AirGapArchive(workspace: f.archive.workspace.reissuingBundle(id: "password-replacement"),
                                                    keys: [], using: crypto)
                _ = try await f.store.install(replacement, envelopeDigest: Data(repeating: 8, count: 32),
                    transferID: Data(repeating: 9, count: 16), expectedRevision: f.installed.revision,
                    sourceAndContentsConfirmed: true, securityChangesConfirmed: true, associateWithExistingWorkspace: true)
            } else { try await f.store.revokeHostLocally(workspaceID: f.installed.id, trustID: "trust", key: f.pin) }
            prompt.finish()
            await expectFailure { _ = try await task.value }
        }
    }

    func testPasswordResponsesAreBoundedAndPromptDeadlineRetainsItsOwner() async throws {
        for response in [Data(), Data(repeating: 65, count: 1025), Data([65, 0, 66])] {
            let f = try await passwordFixture(), prompt = DelayedSSHPasswordPrompt()
            let broker = SSHAuthenticationBroker(store: f.store, signer: ImmediateSSHSigner(container: f.key.container), support: passwordSupport)
            await broker.resume()
            let selected = try await passwordIdentity(broker, f)
            let task = Task { try await broker.password(for: selected, transportID: UUID(), prompter: prompt) }
            await prompt.waitUntilStarted(); prompt.finish(response)
            await expectFailure { _ = try await task.value }
        }
        let f = try await passwordFixture(), prompt = DelayedSSHPasswordPrompt()
        let broker = SSHAuthenticationBroker(store: f.store, signer: ImmediateSSHSigner(container: f.key.container), support: passwordSupport)
        await broker.resume()
        let selected = try await passwordIdentity(broker, f)
        let task = Task { try await broker.password(for: selected, transportID: UUID(), prompter: prompt) }
        await prompt.waitUntilStarted()
        await broker.expireRequests(at: .now + .seconds(61))
        XCTAssertEqual(prompt.cancellations, 1)
        let held = await broker.isBusy
        XCTAssertTrue(held)
        prompt.finish()
        await expectFailure { _ = try await task.value }
        let released = await broker.isBusy
        XCTAssertFalse(released)
    }

    func testPasswordAndKeyPromptsShareOneBoundedWorker() async throws {
        let f = try await fixture { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["authentication_methods"] = ["publickey", "password"]; workspace["routes"] = routes
        }
        let signer = DelayedSSHSigner(f.key.container), prompt = DelayedSSHPasswordPrompt()
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: passwordSupport)
        await broker.resume()
        let key = try await prepare(broker, f)
        let signing = Task { try await broker.confirm(requestID: key.id) }
        await signer.waitUntilStarted()
        let selected = try await passwordIdentity(broker, f)
        let password = Task { try await broker.password(for: selected, transportID: UUID(), prompter: prompt) }
        await waitUntilQueued(1, broker: broker)
        XCTAssertEqual(prompt.calls, 0)
        signer.finish(); _ = try await signing.value
        await prompt.waitUntilStarted(); prompt.finish()
        _ = try await password.value
        XCTAssertEqual(prompt.calls, 1)
    }

    func testPasswordHopNeedsExactDestinationGraphAndMandatoryPolicy() async throws {
        for blocked in [false, true] {
            let f = try await transitFixture { workspace in
                var routes = workspace["routes"] as! [[String: Any]]
                routes[1]["authentication_methods"] = ["password"]; routes[1]["credential_refs"] = [] as [String]
                if blocked { routes[0]["required_capabilities"] = ["unimplemented-security-policy"] }
                workspace["routes"] = routes
            }
            let broker = SSHAuthenticationBroker(store: f.store, signer: ImmediateSSHSigner(container: f.key.container), support: passwordSupport)
            await broker.resume()
            if blocked {
                await expectFailure {
                    _ = try await broker.passwordIdentity(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                        routeID: "hop", presentedHostKey: f.pin, transitForRouteID: "route")
                }
            } else {
                let selected = try await broker.passwordIdentity(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                    routeID: "hop", presentedHostKey: f.pin, transitForRouteID: "route")
                XCTAssertEqual(selected.authorization.transitForRouteID, "route")
                await expectFailure {
                    _ = try await broker.passwordIdentity(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                        routeID: "hop", presentedHostKey: f.pin, transitForRouteID: "hop")
                }
            }
        }
    }

    func testAuthenticationChoiceUsesFirstUsablePermittedMethodWithoutInteractiveSubstitution() async throws {
        let cases: [([String], [String], [WorkspaceSSHAuthenticationChoice])] = [
            (["publickey", "password"], ["key"], [.publicKey("key")]),
            (["publickey", "password"], [], [.password]),
            (["password", "publickey"], ["key"], [.password]),
            (["keyboard-interactive"], [], [])
        ]
        for (methods, available, expected) in cases {
            let f = try await fixture { workspace in
                var routes = workspace["routes"] as! [[String: Any]]
                routes[0]["authentication_methods"] = methods; workspace["routes"] = routes
            }
            let route = try XCTUnwrap(f.archive.workspace.routes.first)
            XCTAssertEqual(WorkspaceSSHAuthenticationChoice.choices(for: route, usableCredentialIDs: available), expected)
        }
    }
}
