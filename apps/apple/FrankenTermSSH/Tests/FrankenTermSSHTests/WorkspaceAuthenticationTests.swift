import Foundation
import XCTest
@testable import FrankenTermKit
@testable import FrankenTermSSH
import FrankenTermSSHIntegration
#if canImport(CryptoKit)
import CryptoKit
#else
import Crypto
#endif

// Real import and production signing with synthetic memory-backed side effects.
// Only the encryption operations irrelevant to this authentication fixture refuse.
struct SSHWorkspaceTestCryptography: AirGapCryptography {
    func sha256(_ bytes: Data) -> Data { Data(SHA256.hash(data: bytes)) }
    func ed25519PublicKey(seed: Data) throws -> Data {
        try Curve25519.Signing.PrivateKey(rawRepresentation: seed).publicKey.rawRepresentation
    }
    func ecdsaPublicKey(scalar: Data, curve: SSHECDSACurve) throws -> Data {
        switch curve {
        case .nistp256: try P256.Signing.PrivateKey(rawRepresentation: scalar).publicKey.x963Representation
        case .nistp384: try P384.Signing.PrivateKey(rawRepresentation: scalar).publicKey.x963Representation
        case .nistp521: try P521.Signing.PrivateKey(rawRepresentation: scalar).publicKey.x963Representation
        }
    }
    func hmacSHA1(key: Data, message: Data) throws -> Data { throw SSHAuthenticationError.unavailable }
    func randomBytes(count: Int) throws -> Data { throw SSHAuthenticationError.unavailable }
    func seal(_ plaintext: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data { throw SSHAuthenticationError.unavailable }
    func open(_ ciphertextAndTag: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data { throw SSHAuthenticationError.unavailable }
    func bcryptPBKDF(passphrase: Data, salt: Data, rounds: UInt32, outputByteCount: Int) throws -> Data { throw SSHAuthenticationError.unavailable }
    func decryptAESCTR(_ ciphertext: Data, key: Data, iv: Data) throws -> Data { throw SSHAuthenticationError.unavailable }
}

final class SSHWorkspaceTestPersistence: WorkspacePersistence, @unchecked Sendable {
    private let lock = NSRecursiveLock()
    private var slots: [WorkspaceStorageSlot: Data] = [:]
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T {
        lock.lock(); defer { lock.unlock() }; return try body()
    }
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? {
        try withExclusiveAccess {
            if let bytes = slots[slot], bytes.count > limit { throw FTError.storage }
            return slots[slot]
        }
    }
    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws { try withExclusiveAccess { slots[slot] = data } }
    func removeJournal() throws { try withExclusiveAccess { slots[.journal] = nil } }
}

final class SSHWorkspaceTestVault: WorkspaceCredentialVault, WorkspaceSSHSigning, @unchecked Sendable {
    private let lock = NSLock()
    private var items: [String: (Data, String)] = [:]
    private var reads = 0, signatures = 0
    var counts: (reads: Int, signatures: Int) { lock.lock(); defer { lock.unlock() }; return (reads, signatures) }
    func create(account: String, privateContainer: Data, fingerprint: String) throws {
        lock.lock(); defer { lock.unlock() }
        guard items[account] == nil else { throw FTError.storage }
        items[account] = (privateContainer, fingerprint)
    }
    func fingerprint(account: String) throws -> String? { lock.lock(); defer { lock.unlock() }; return items[account]?.1 }
    func remove(account: String) throws { lock.lock(); defer { lock.unlock() }; items[account] = nil }
    private func material(_ authorization: WorkspaceSSHAuthorization, signing: Bool) throws -> Data {
        lock.lock(); defer { lock.unlock() }
        guard let item = items[authorization.account], item.1 == authorization.fingerprint else { throw SSHAuthenticationError.denied }
        if signing { signatures += 1 } else { reads += 1 }
        return item.0
    }
    func publicIdentity(requestID: UUID, authorization: WorkspaceSSHAuthorization) async throws -> Data {
        try SSHUserAuthenticationSigner.publicIdentity(privateContainer: material(authorization, signing: false)).blob
    }
    func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization, challenge: SSHUserAuthenticationChallenge) async throws -> Data {
        try SSHUserAuthenticationSigner.sign(challenge, privateContainer: material(authorization, signing: true))
    }
    func cancel(requestID: UUID) {}
}

struct SSHWorkspaceTestFixture: Sendable {
    let store: WorkspaceStore, broker: SSHAuthenticationBroker, vault: SSHWorkspaceTestVault
    let installed: InstalledWorkspace, archive: AirGapArchive, publicBlob: Data, host: WorkspaceDocument.HostKey
    let endpoint: NativeSSHEndpoint

    @MainActor static func make(hostname: String = "localhost", port: UInt16 = 22, hostBlob: Data? = nil,
                     seed: Data = Data(0..<32), persistence: any WorkspacePersistence = SSHWorkspaceTestPersistence(),
                     mutate: (inout [String: Any]) -> Void = { _ in }) async throws -> Self {
        let crypto = SSHWorkspaceTestCryptography()
        let key = try crypto.ed25519PublicKey(seed: seed)
        func field(_ bytes: Data) -> Data {
            var length = UInt32(bytes.count).bigEndian
            return withUnsafeBytes(of: &length) { Data($0) } + bytes
        }
        let blob = field(Data("ssh-ed25519".utf8)) + field(key)
        var part = Data([1, 2, 3, 4, 1, 2, 3, 4]) + field(Data("ssh-ed25519".utf8)) + field(key) + field(seed + key) + field(Data())
        var pad: UInt8 = 1
        while !part.count.isMultiple(of: 8) { part.append(pad); pad += 1 }
        let container = Data("openssh-key-v1\0".utf8) + field(Data("none".utf8)) + field(Data("none".utf8)) + field(Data())
            + Data([0, 0, 0, 1]) + field(blob) + field(part)
        let identity = try SSHPublicIdentity(blob: blob)
        let peerIdentity = try SSHPublicIdentity(blob: hostBlob ?? blob)
        let theme: [String: Any] = ["id": "theme", "name": "Test", "foreground": "#FFFFFF", "background": "#10223C", "accent": "#3B82F6",
            "ansi": Array(repeating: "#123456", count: 16), "cursor_foreground": "#000000", "cursor_background": "#FFFFFF",
            "cursor_border": "#FFFFFF", "selection_foreground": "#FFFFFF", "selection_background": "#345D91"]
        var workspace: [String: Any] = [
            "id": "work", "name": "Synthetic", "groups": [["id": "group", "name": "Test", "machine_order": ["machine"], "auto_connect": true]],
            "machines": [["id": "machine", "name": "Test", "theme_ref": "theme", "route_refs": ["route"], "preferred_route_ref": "route", "auto_connect": true]],
            "routes": [["id": "route", "mode": "direct_ssh", "session_creation": "on_select", "host": hostname, "port": Int(port), "user": "operator",
                "credential_refs": ["key"], "authentication_methods": ["publickey"], "host_trust_ref": "trust", "jump_routes": []]],
            "credentials": [["id": "key", "algorithm": identity.algorithm, "public_fingerprint": identity.fingerprint,
                "requirement": "matching_private_key", "private_material": "not_included"]],
            "host_trust": [["id": "trust", "host": hostname, "port": Int(port), "revoked_keys": [],
                "pins": [["algorithm": peerIdentity.algorithm, "fingerprint": peerIdentity.fingerprint]]]],
            "themes": [theme], "preferences": ["initial_machine_ref": "machine", "initial_group_ref": "group", "connect_on_open": true]]
        mutate(&workspace)
        let manifest = try JSONSerialization.data(withJSONObject: ["format": "ftworkspace", "version": 1, "bundle_id": "bundle",
            "source": ["exporter_id": "test"], "required_capabilities": [], "workspace": workspace], options: [.sortedKeys])
        let archive = try AirGapArchive(workspace: WorkspaceDocument(data: manifest),
            keys: [PrivateKeyRecord(credentialID: "key", container: container, publicBlob: blob)], using: crypto)
        let vault = SSHWorkspaceTestVault(), store = WorkspaceStore(persistence: persistence, vault: vault)
        let installed = try await store.install(archive, envelopeDigest: Data(repeating: 1, count: 32),
            expectedRevision: nil, sourceAndContentsConfirmed: true).workspace
        let broker = SSHAuthenticationBroker(store: store, signer: vault, support: WorkspaceNativeSSHAuthentication.routeSupport)
        await broker.resume()
        return .init(store: store, broker: broker, vault: vault, installed: installed, archive: archive, publicBlob: blob,
            host: try WorkspaceDocument.HostKey(algorithm: peerIdentity.algorithm, fingerprint: peerIdentity.fingerprint),
            endpoint: try NativeSSHEndpoint(host: hostname, port: port))
    }

    func peer(transport: UUID, endpoint: NativeSSHEndpoint? = nil, hostKey: Data? = nil, cipher: String = "aes256-ctr") -> NativeSSHPeer {
        .init(transportID: transport, endpoint: endpoint ?? self.endpoint, hostKey: hostKey ?? publicBlob,
            keyExchange: "curve25519-sha256", hostSignature: "ssh-ed25519", outboundCipher: cipher, inboundCipher: cipher,
            outboundMAC: "hmac-sha2-256-etm@openssh.com", inboundMAC: "hmac-sha2-256-etm@openssh.com",
            outboundCompression: "none", inboundCompression: "none")
    }
    func authority(transport: UUID) -> WorkspaceNativeSSHAuthentication {
        .init(broker: broker, workspaceID: installed.id, revision: installed.revision, routeID: "route", credentialID: "key",
              transportID: transport, endpoint: endpoint)
    }
}

private actor SSHWorkspaceTerminalProbe: TerminalSessionConsumer {
    let identity: TerminalConnectionIdentity
    private var prepared: TerminalConnectionIdentity?
    private var output = Data()
    private var eof = false
    private var resized: TerminalSessionGeometry?
    init(identity: TerminalConnectionIdentity) { self.identity = identity }
    // The test supplies the app-owned durability boundary, without claiming a
    // memory record proves physical-device persistence or a real terminal core.
    func prepare(_ identity: TerminalConnectionIdentity, fail: Bool) throws {
        guard identity == self.identity, prepared == nil else { throw TerminalInputError.stale }
        if fail { throw FTError.storage }
        prepared = identity
    }
    func receive(_ update: TerminalSessionUpdate, source: TerminalSource) throws {
        guard prepared == identity, source.identity == identity, update.output.count <= 16_384,
              output.count + update.output.count <= 65_536 else { throw TerminalInputError.stale }
        output += update.output; eof = eof || update.endOfOutput; resized = update.resized ?? resized
    }
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) throws {
        guard prepared == identity, source.identity == identity, outcomes.count <= 64,
              outcomes.allSatisfy({ $0.receipt.source == source }) else { throw TerminalInputError.stale }
    }
    func summary() -> (TerminalConnectionIdentity?, Data, Bool, TerminalSessionGeometry?) {
        (prepared, output, eof, resized)
    }
}

@MainActor
final class WorkspaceAuthenticationTests: XCTestCase {
    private func refuse(_ body: () async throws -> Void, file: StaticString = #filePath, line: UInt = #line) async {
        do { try await body(); XCTFail("Authority should have been refused", file: file, line: line) } catch {}
    }
    private func signed(_ fixture: SSHWorkspaceTestFixture, transport: UUID) async throws -> (WorkspaceNativeSSHAuthentication, NativeSSHPeer, NativeSSHIdentity) {
        let authority = fixture.authority(transport: transport), peer = fixture.peer(transport: transport)
        let identity = try await authority.identity(for: peer)
        let typed = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(repeating: 7, count: 32), user: "operator", publicBlob: identity.publicKey)
        let challenge = try NativeSSHChallenge(bytes: typed.signedData, transportID: transport, identity: identity)
        let signature = try await authority.signature(for: challenge, peer: peer)
        // Verify the actual broker signature independently of its production
        // SSH blob packaging, over the exact driver-provided RFC 4252 preimage.
        let payload = signature.suffix(64)
        let publicKey = try Curve25519.Signing.PublicKey(rawRepresentation: fixture.publicBlob.suffix(32))
        XCTAssertTrue(publicKey.isValidSignature(payload, for: typed.signedData))
        try await authority.validate(peer: peer, identity: identity)
        try await authority.validateCurrent()
        return (authority, peer, identity)
    }

    func testImportedIdentityAuthenticatesExactRouteAndSession() async throws {
        let f = try await SSHWorkspaceTestFixture.make(), transport = UUID()
        let (authority, peer, identity) = try await signed(f, transport: transport)
        XCTAssertEqual(identity.publicKey, f.publicBlob)
        XCTAssertEqual(f.vault.counts.reads, 1); XCTAssertEqual(f.vault.counts.signatures, 1)
        XCTAssertTrue(f.installed.connectLater)
        await refuse { _ = try await authority.identity(for: peer) }
        let typed = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(repeating: 7, count: 32), user: identity.user, publicBlob: identity.publicKey)
        let replay = try NativeSSHChallenge(bytes: typed.signedData, transportID: transport, identity: identity)
        await refuse { _ = try await authority.signature(for: replay, peer: peer) }
    }

    func testMuxRouteSignsInLikeADirectOne() async throws {
        let f = try await SSHWorkspaceTestFixture.make { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["mode"] = "frankenterm_mux"
            routes[0]["session_creation"] = nil
            routes[0]["attach_policy"] = "existing_only"
            workspace["routes"] = routes
        }
        let (_, _, identity) = try await signed(f, transport: UUID())
        XCTAssertEqual(identity.publicKey, f.publicBlob)
        XCTAssertEqual(f.vault.counts.signatures, 1)
    }

    func testWrongHostEndpointTransportAndMethodsFailBeforeKeyRead() async throws {
        let f = try await SSHWorkspaceTestFixture.make()
        var other = f.publicBlob; other[other.count - 1] ^= 1
        for variant in 0..<4 {
            let transport = UUID(), authority = f.authority(transport: transport)
            let peer = f.peer(transport: variant == 0 ? UUID() : transport,
                endpoint: variant == 1 ? try NativeSSHEndpoint(host: "elsewhere.invalid") : nil,
                hostKey: variant == 2 ? other : nil, cipher: variant == 3 ? "aes128-cbc" : "aes256-ctr")
            await refuse { _ = try await authority.identity(for: peer) }
        }
        XCTAssertEqual(f.vault.counts.reads, 0); XCTAssertEqual(f.vault.counts.signatures, 0)
    }

    func testRevocationReimportAndBackgroundRetireSuccessfulAuthentication() async throws {
        for variant in 0..<4 {
            let f = try await SSHWorkspaceTestFixture.make(), transport = UUID()
            let (authority, peer, identity) = try await signed(f, transport: transport)
            switch variant {
            case 0: try await f.store.revokeLocally(fingerprint: f.host.fingerprint)
            case 1: try await f.store.revokeHostLocally(workspaceID: f.installed.id, trustID: "trust", key: f.host)
            case 2: await f.broker.suspend(); await f.broker.resume()
            default:
                let replacement = try AirGapArchive(workspace: f.archive.workspace.reissuingBundle(id: "replacement"), keys: [], using: SSHWorkspaceTestCryptography())
                _ = try await f.store.install(replacement, envelopeDigest: Data(repeating: 3, count: 32),
                    expectedRevision: f.installed.revision, sourceAndContentsConfirmed: true,
                    securityChangesConfirmed: true, associateWithExistingWorkspace: true)
            }
            await refuse { try await authority.validate(peer: peer, identity: identity) }
            await refuse { try await authority.validateCurrent() }
        }
    }

    func testCancellationAndBackgroundBetweenOfferAndSignCannotPrompt() async throws {
        for background in [false, true] {
            let f = try await SSHWorkspaceTestFixture.make(), transport = UUID()
            let authority = f.authority(transport: transport), peer = f.peer(transport: transport)
            let identity = try await authority.identity(for: peer)
            let typed = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(repeating: 5, count: 32), user: identity.user, publicBlob: identity.publicKey)
            let challenge = try NativeSSHChallenge(bytes: typed.signedData, transportID: transport, identity: identity)
            if background { await f.broker.suspend(); await f.broker.resume() }
            else { authority.cancel(transportID: transport) }
            await refuse { _ = try await authority.signature(for: challenge, peer: peer) }
            XCTAssertEqual(f.vault.counts.signatures, 0)
        }
    }

    func testCallerCancellationBeforeActorAdmissionCannotReadOrSign() async throws {
        let f = try await SSHWorkspaceTestFixture.make(), firstTransport = UUID()
        let first = f.authority(transport: firstTransport), firstPeer = f.peer(transport: firstTransport)
        // These tasks inherit this test's MainActor. Cancel before yielding it,
        // so no work has yet reached the adapter or protected broker.
        let lookup = Task { try await first.identity(for: firstPeer) }
        lookup.cancel()
        await refuse { _ = try await lookup.value }
        await refuse { _ = try await first.identity(for: firstPeer) }
        XCTAssertEqual(f.vault.counts.reads, 0)

        let transport = UUID(), authority = f.authority(transport: transport)
        let peer = f.peer(transport: transport), identity = try await authority.identity(for: peer)
        let typed = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(repeating: 3, count: 32),
            user: identity.user, publicBlob: identity.publicKey)
        let challenge = try NativeSSHChallenge(bytes: typed.signedData, transportID: transport, identity: identity)
        let signing = Task { try await authority.signature(for: challenge, peer: peer) }
        signing.cancel()
        await refuse { _ = try await signing.value }
        await refuse { _ = try await authority.signature(for: challenge, peer: peer) }
        XCTAssertEqual(f.vault.counts.reads, 1)
        XCTAssertEqual(f.vault.counts.signatures, 0)
    }

    /// The companion Python harness supplies one independent loopback SSH server
    /// per scenario. Normal unit runs explicitly skip this optional native lane.
    func testLiveImportedWorkspaceAuthenticationAndPTY() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard let rawPort = environment["FT_WORKSPACE_SSH_TEST_PORT"], let port = UInt16(rawPort),
              let rawHost = environment["FT_WORKSPACE_SSH_TEST_HOSTKEY"], let host = Data(base64Encoded: rawHost),
              let mode = environment["FT_WORKSPACE_SSH_TEST_MODE"] else {
            throw XCTSkip("Run Tests/Interop/test_workspace.py with the built XCTest bundle and native driver.")
        }
        let f = try await SSHWorkspaceTestFixture.make(port: port, hostBlob: mode == "wrong-host" ? nil : host)
        if mode.hasPrefix("owner-") {
            try await exerciseLiveOwner(f, mode: mode)
            return
        }
        let session = try await NativeSSHConnector.connect(to: f.endpoint, timeout: .seconds(5))
        do {
            let authority = f.authority(transport: session.transportID)
            if mode == "revoked" { try await f.store.revokeHostLocally(workspaceID: f.installed.id, trustID: "trust", key: f.host) }
            if mode == "wrong-host" || mode == "revoked" {
                await refuse { _ = try await session.authenticate(using: authority, timeout: .seconds(5)) }
                XCTAssertEqual(f.vault.counts.reads, 0); XCTAssertEqual(f.vault.counts.signatures, 0)
            } else {
                _ = try await session.authenticate(using: authority, timeout: .seconds(5))
                XCTAssertEqual(f.vault.counts.reads, 1); XCTAssertEqual(f.vault.counts.signatures, 1)
                if mode == "revoke-before-shell" {
                    try await f.store.revokeLocally(fingerprint: f.archive.workspace.credentials[0].fingerprint)
                    await refuse { try await session.openShell(geometry: .init(columns: 80, rows: 24), timeout: .seconds(5)) }
                } else {
                    XCTAssertEqual(mode, "success")
                    try await session.openShell(geometry: .init(columns: 80, rows: 24), timeout: .seconds(5))
                    let bytes = Data("Imported key → real SSH.\rEND-SESSION\r".utf8)
                    let admission = try await session.admit(bytes)
                    XCTAssertEqual(admission, .accepted(bytes.count))
                    var received = Data(), eof = false
                    let deadline = ContinuousClock.now + .seconds(5)
                    while !eof, ContinuousClock.now < deadline {
                        let event = try await session.service(timeout: .seconds(1))
                        received += event.output; eof = event.endOfOutput
                        XCTAssertLessThanOrEqual(received.count, 16384)
                    }
                    XCTAssertTrue(eof)
                    XCTAssertEqual(received, Data("ready\r\nstderr\r\n".utf8) + bytes)
                }
            }
            await session.close()
            let statistics = try await session.accounting()
            XCTAssertEqual(statistics.allocatedBytes, 0)
        } catch { await session.close(); throw error }
    }

    private func exerciseLiveOwner(_ f: SSHWorkspaceTestFixture, mode: String) async throws {
        let activity = WorkspaceNetworkActivity()
        let owner = try await WorkspaceNativeSSHConnection.connect(workspaceID: f.installed.id,
            expectedRevision: f.installed.revision, machineID: "machine", routeID: "route", credentialID: "key",
            confirmedByUser: true, store: f.store, broker: f.broker, networkActivity: activity,
            connectionTimeout: .seconds(5), authenticationTimeout: .seconds(5))
        do {
            XCTAssertEqual(activity.accounting().connections, 1)
            XCTAssertThrowsError(try activity.beginOfflineRestore()) {
                XCTAssertEqual($0 as? WorkspaceNetworkError, .connectionsActive)
            }
            XCTAssertEqual(owner.workspaceRevision, f.installed.revision)
            XCTAssertEqual(f.vault.counts.reads, 1); XCTAssertEqual(f.vault.counts.signatures, 1)
            let coordinator = try TerminalInputCoordinator(), sink = SSHWorkspaceTerminalProbe(identity: owner.identity)
            if mode == "owner-marker-failure" {
                await refuse {
                    _ = try await owner.openTerminal(geometry: .init(columns: 80, rows: 24), coordinator: coordinator) {
                        try await sink.prepare($0, fail: true)
                    }
                }
            } else {
                let pump = try await owner.openTerminal(geometry: .init(columns: 80, rows: 24), coordinator: coordinator) {
                    try await sink.prepare($0, fail: false)
                }
                XCTAssertEqual(pump.source.identity, owner.identity)
                let stream = try await coordinator.makeStream(sceneID: UUID())
                let focus = try await coordinator.changeFocus(stream, to: pump.source)
                if mode == "owner-revoked" {
                    _ = try await coordinator.submit(focus, encodedBytes: Data("must-not-leave\r".utf8))
                    try await f.store.revokeLocally(fingerprint: f.archive.workspace.credentials[0].fingerprint)
                    let result = try await coordinator.drainOne(pump.source)
                    XCTAssertEqual(result, .connectionClosed)
                    try await pump.disconnect()
                } else if mode == "owner-close-before-run" {
                    await owner.close()
                    await refuse { _ = try await coordinator.submit(focus, encodedBytes: Data("must-not-queue\r".utf8)) }
                } else {
                    XCTAssertEqual(mode, "owner-success")
                    let bytes = Data("Imported key → owned terminal.\rEND-SESSION\r".utf8)
                    _ = try await coordinator.submit(focus, encodedBytes: bytes)
                    try await owner.requestResize(.init(columns: 132, rows: 42))
                    let run = Task { try await pump.run(consumer: sink) }
                    let watchdog = Task { try await Task.sleep(for: .seconds(10)); pump.requestStop() }
                    defer { watchdog.cancel() }
                    let result = try await run.value, summary = await sink.summary()
                    XCTAssertEqual(result, .completed(.init(remoteExit: nil, channelClosed: true)))
                    XCTAssertEqual(summary.0, owner.identity)
                    XCTAssertEqual(summary.1, Data("ready\r\nstderr\r\n".utf8) + bytes)
                    XCTAssertTrue(summary.2)
                    XCTAssertEqual(summary.3?.columns, 132); XCTAssertEqual(summary.3?.rows, 42)
                }
                _ = try await coordinator.suspend(focus)
                let remaining = try await coordinator.pendingOutcomes(pump.source)
                if !remaining.outcomes.isEmpty { try await coordinator.acknowledgeOutcomes(remaining) }
                try await coordinator.retire(pump.source)
            }
            await owner.close()
            XCTAssertEqual(activity.accounting().connections, 0)
            let offline = try activity.beginOfflineRestore()
            XCTAssertEqual(activity.accounting().offlineRestores, 1)
            withExtendedLifetime(offline) {}
        } catch { await owner.close(); throw error }
    }
}
