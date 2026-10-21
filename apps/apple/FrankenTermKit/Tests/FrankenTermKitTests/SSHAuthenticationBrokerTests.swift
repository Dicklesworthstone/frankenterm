import Foundation
import XCTest
@testable import FrankenTermKit

/// The delayed boundary performs the real production signature after release.
/// Cancellation intentionally does not settle it immediately, modeling a native
/// authentication callback that still owns resources while cancellation arrives.
private final class DelayedSSHSigner: WorkspaceSSHSigning, @unchecked Sendable {
    private struct Pending {
        let id: UUID, challenge: SSHUserAuthenticationChallenge
        let continuation: CheckedContinuation<Data, Error>
    }
    private let lock = NSLock()
    private let container: Data
    private var pending: Pending?
    private var observer: CheckedContinuation<Void, Never>?
    private var callCount = 0, cancellationCount = 0
    init(_ container: Data) { self.container = container }
    var calls: Int { lock.lock(); defer { lock.unlock() }; return callCount }
    var cancellations: Int { lock.lock(); defer { lock.unlock() }; return cancellationCount }
    func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
              challenge: SSHUserAuthenticationChallenge) async throws -> Data {
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
            try SSHUserAuthenticationSigner.sign(selected.challenge, privateContainer: container)
        })
    }
}

private struct ImmediateSSHSigner: WorkspaceSSHSigning {
    let container: Data
    func cancel(requestID: UUID) {}
    func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
              challenge: SSHUserAuthenticationChallenge) async throws -> Data {
        try SSHUserAuthenticationSigner.sign(challenge, privateContainer: container)
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
    private func fixture(mutate: (inout [String: Any]) -> Void = { _ in }) async throws -> Fixture {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let (key, fingerprint) = try sampleKey(using: crypto)
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest(fingerprint: fingerprint)) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        mutate(&workspace); root["workspace"] = workspace
        let document = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
        let archive = try AirGapArchive(workspace: document, keys: [key], using: crypto)
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

    func testNoQualificationNoPermittedCredentialOrWrongActualHostCannotPrompt() async throws {
        let f = try await fixture(), signer = DelayedSSHSigner(try sampleKey(using: crypto).0.container)
        let offline = SSHAuthenticationBroker(store: f.store, signer: signer, support: .offlineOnly)
        await offline.resume()
        await expectFailure { _ = try await self.prepare(offline, f) }
        let broker = SSHAuthenticationBroker(store: f.store, signer: signer, support: support)
        await broker.resume()
        let wrong = try WorkspaceDocument.HostKey(algorithm: "ssh-ed25519",
            fingerprint: "SHA256:ERERERERERERERERERERERERERERERERERERERERERE")
        await expectFailure {
            _ = try await broker.prepare(workspaceID: f.installed.id, expectedRevision: f.installed.revision,
                routeID: "route", credentialID: "key", presentedHostKey: wrong, transportID: UUID(),
                sessionIdentifier: Data(repeating: 9, count: 32), publicBlob: f.key.publicBlob)
        }
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
}
