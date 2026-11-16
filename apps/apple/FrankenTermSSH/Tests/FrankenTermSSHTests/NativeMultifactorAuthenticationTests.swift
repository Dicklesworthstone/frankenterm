import Foundation
import XCTest
@testable import FrankenTermSSH
#if canImport(CryptoKit)
import CryptoKit
#else
import Crypto
#endif

private enum MultifactorFixtureError: Error { case wrongPeer, wrongChallenge, revoked, promptNotReached }

private func mfaField(_ bytes: Data) -> Data {
    var length = UInt32(bytes.count).bigEndian
    return withUnsafeBytes(of: &length) { Data($0) } + bytes
}

/// The independent server verifies this real Ed25519 signature. This provider
/// can revoke the first factor after it has been signed, without changing the
/// separately owned interactive provider or its prompt lifecycle.
private final class MultifactorKeyAuthority: NativeSSHAuthentication, @unchecked Sendable {
    private let lock = NSLock()
    private let endpoint: NativeSSHEndpoint, hostKey: Data
    private let key: Curve25519.Signing.PrivateKey
    private let selected: NativeSSHIdentity
    private let signingDelay: Duration
    private var peer: NativeSSHPeer?
    private var retired = false, revoked = false
    private var signatureCount = 0, cancellationCount = 0

    init(endpoint: NativeSSHEndpoint, hostKey: Data, seed: Data = Data(0..<32), signingDelay: Duration = .zero) throws {
        self.endpoint = endpoint; self.hostKey = hostKey; self.signingDelay = signingDelay
        let signingKey = try Curve25519.Signing.PrivateKey(rawRepresentation: seed)
        key = signingKey
        selected = try .init(user: "operator", publicKey: mfaField(Data("ssh-ed25519".utf8)) + mfaField(signingKey.publicKey.rawRepresentation))
    }
    var counts: (signatures: Int, cancellations: Int) { lock.withLock { (signatureCount, cancellationCount) } }
    func hasSigned(for peer: NativeSSHPeer) -> Bool {
        lock.withLock { self.peer == peer && signatureCount == 1 && !retired && !revoked }
    }
    func identity(for peer: NativeSSHPeer) async throws -> NativeSSHIdentity {
        try lock.withLock {
            guard !retired, !revoked else { throw MultifactorFixtureError.revoked }
            guard self.peer == nil, peer.endpoint == endpoint, peer.hostKey == hostKey else { throw MultifactorFixtureError.wrongPeer }
            self.peer = peer
            return selected
        }
    }
    func signature(for challenge: NativeSSHChallenge, peer: NativeSSHPeer) async throws -> Data {
        try await validate(peer: peer, identity: challenge.identity)
        guard challenge.transportID == peer.transportID, challenge.identity == selected,
              challenge.publicKeyAlgorithm == "ssh-ed25519", challenge.algorithm == "ssh-ed25519" else {
            throw MultifactorFixtureError.wrongChallenge
        }
        try lock.withLock {
            guard signatureCount == 0 else { throw MultifactorFixtureError.wrongChallenge }
            signatureCount += 1
        }
        if signingDelay > .zero { try await Task.sleep(for: signingDelay) }
        try await validate(peer: peer, identity: selected)
        return mfaField(Data("ssh-ed25519".utf8)) + mfaField(try key.signature(for: challenge.signedData))
    }
    func validate(peer: NativeSSHPeer, identity: NativeSSHIdentity) async throws {
        try lock.withLock {
            guard !retired, !revoked else { throw MultifactorFixtureError.revoked }
            guard peer == self.peer, peer.endpoint == endpoint, peer.hostKey == hostKey, identity == selected else {
                throw MultifactorFixtureError.wrongPeer
            }
        }
    }
    func revoke() { lock.withLock { revoked = true } }
    func cancel(transportID: UUID) {
        lock.withLock {
            guard peer == nil || peer?.transportID == transportID else { return }
            retired = true; cancellationCount += 1
        }
    }
}

private final class MultifactorInteractiveAuthority: NativeSSHInteractiveAuthentication, @unchecked Sendable {
    private let lock = NSLock()
    private let key: MultifactorKeyAuthority
    private let mode: String
    private var peer: NativeSSHPeer?
    private var retired = false, revoked = false
    private var accountCount = 0, promptCount = 0, cancellationCount = 0
    private var pending: CheckedContinuation<[Data], Error>?
    init(key: MultifactorKeyAuthority, mode: String) { self.key = key; self.mode = mode }
    var counts: (accounts: Int, prompts: Int, cancellations: Int, waiting: Bool) {
        lock.withLock { (accountCount, promptCount, cancellationCount, pending != nil) }
    }
    private var user: String { mode == "wrong-account" ? "different-account" : "operator" }
    func account(for peer: NativeSSHPeer) async throws -> NativeSSHInteractiveAccount {
        guard key.hasSigned(for: peer) else { throw MultifactorFixtureError.wrongPeer }
        return try lock.withLock {
            guard !retired, !revoked, self.peer == nil else { throw MultifactorFixtureError.revoked }
            self.peer = peer; accountCount += 1
            return try .init(user: user)
        }
    }
    func responses(for challenge: NativeSSHInteractiveChallenge, peer: NativeSSHPeer) async throws -> [Data] {
        try await validate(peer: peer, account: challenge.account)
        guard challenge.transportID == peer.transportID else { throw MultifactorFixtureError.wrongPeer }
        let response: Data
        if challenge.round == 1 {
            guard challenge.name == "Verification", challenge.instruction == "Enter the one-time code.",
                  challenge.prompts.map(\.text) == ["Verification code:"], challenge.prompts.map(\.echo) == [false] else {
                throw MultifactorFixtureError.wrongChallenge
            }
            response = Data((mode == "wrong-response" ? "wrong" : "654321").utf8)
        } else {
            guard challenge.round == 2, challenge.name == "Recovery", challenge.instruction == "Enter the recovery token.",
                  challenge.prompts.map(\.text) == ["Recovery token:"], challenge.prompts.map(\.echo) == [false] else {
                throw MultifactorFixtureError.wrongChallenge
            }
            response = Data("approved".utf8)
        }
        return try await withCheckedThrowingContinuation { continuation in
            let immediate = lock.withLock { () -> Result<[Data], Error>? in
                guard !retired, !revoked else { return .failure(MultifactorFixtureError.revoked) }
                guard challenge.round == UInt32(promptCount + 1) else { return .failure(MultifactorFixtureError.wrongChallenge) }
                promptCount += 1
                if ["cancel", "close-prompt", "combined-deadline", "revoke-key-prompt", "revoke-interactive-prompt"].contains(mode) {
                    pending = continuation; return nil
                }
                return .success([response])
            }
            if let immediate { continuation.resume(with: immediate) }
        }
    }
    func validate(peer: NativeSSHPeer, account: NativeSSHInteractiveAccount) async throws {
        try lock.withLock {
            guard !retired, !revoked else { throw MultifactorFixtureError.revoked }
            guard peer == self.peer, account.user == user else { throw MultifactorFixtureError.wrongPeer }
        }
    }
    func cancel(transportID: UUID) {
        let waiting = lock.withLock { () -> CheckedContinuation<[Data], Error>? in
            guard peer == nil || peer?.transportID == transportID else { return nil }
            retired = true; cancellationCount += 1
            let value = pending; pending = nil; return value
        }
        waiting?.resume(throwing: CancellationError())
    }
    func revoke(releasingPrompt: Bool = false) {
        lock.withLock { revoked = true }
        if releasingPrompt { releasePrompt() }
    }
    func releasePrompt() {
        let waiting = lock.withLock { () -> CheckedContinuation<[Data], Error>? in
            let value = pending; pending = nil; return value
        }
        waiting?.resume(returning: [Data("654321".utf8)])
    }
}

final class NativeMultifactorAuthenticationTests: XCTestCase, @unchecked Sendable {
    /// The runner rejects skips and independently checks the actual signed key,
    /// partial-success transition, subsequent KI requests and terminal effects.
    func testLivePublicKeyInteractiveContinuation() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let port = env["FT_MFA_TEST_PORT"].flatMap(UInt16.init),
              let host = env["FT_MFA_TEST_HOSTKEY"].flatMap({ Data(base64Encoded: $0) }),
              let mode = env["FT_MFA_TEST_MODE"], !mode.hasPrefix("workspace-") else {
            throw XCTSkip("Requires the independent public-key plus keyboard-interactive fixture")
        }
        XCTAssertTrue(NativeSSHSession.supportsPublicKeyInteractive, "The live matrix must run against the evidence-capable backend")
        let endpoint = try NativeSSHEndpoint(host: "127.0.0.1", port: port)
        var expectedHost = host
        if mode == "wrong-host" { expectedHost[expectedHost.startIndex] ^= 1 }
        let key = try MultifactorKeyAuthority(endpoint: endpoint, hostKey: expectedHost,
            seed: mode == "wrong-key" ? Data(repeating: 77, count: 32) : Data(0..<32),
            signingDelay: mode == "combined-deadline" ? .milliseconds(1100) : .zero)
        let interactive = MultifactorInteractiveAuthority(key: key, mode: mode)
        var ancestor: (key: MultifactorKeyAuthority, interactive: MultifactorInteractiveAuthority)?
        let session: NativeSSHSession
        if mode.hasPrefix("hop-") {
            let hopPort = try XCTUnwrap(env["FT_MFA_TEST_HOP_PORT"].flatMap(UInt16.init))
            let hopHost = try XCTUnwrap(env["FT_MFA_TEST_HOP_HOSTKEY"].flatMap { Data(base64Encoded: $0) })
            let hopEndpoint = try NativeSSHEndpoint(host: "127.0.0.1", port: hopPort)
            let hopKey = try MultifactorKeyAuthority(endpoint: hopEndpoint, hostKey: hopHost, seed: Data(repeating: 41, count: 32))
            let hopInteractive = MultifactorInteractiveAuthority(key: hopKey, mode: "success")
            let hop = try await NativeSSHConnector.connect(to: hopEndpoint, timeout: .seconds(5))
            do {
                let authenticated = try await hop.authenticate(using: hopKey, continuingWithInteractive: hopInteractive, timeout: .seconds(5))
                XCTAssertTrue(authenticated.usedKeyboardInteractive)
                session = try await hop.forward(to: endpoint, timeout: .seconds(5))
                await hop.close()
                XCTAssertEqual(hopKey.counts.cancellations, 0)
                XCTAssertEqual(hopInteractive.counts.cancellations, 0)
                ancestor = (hopKey, hopInteractive)
            } catch { await hop.close(); throw error }
        } else { session = try await NativeSSHConnector.connect(to: endpoint, timeout: .seconds(5)) }
        do {
            if ["cancel", "close-prompt", "combined-deadline", "revoke-key-prompt", "revoke-interactive-prompt"].contains(mode) {
                let started = ContinuousClock.now
                let work = Task {
                    try await session.authenticate(using: key, continuingWithInteractive: interactive,
                        timeout: mode == "combined-deadline" ? .seconds(2) : .seconds(5))
                }
                let promptDeadline = ContinuousClock.now + .seconds(3)
                while !interactive.counts.waiting, ContinuousClock.now < promptDeadline {
                    try await Task.sleep(for: .milliseconds(5))
                }
                guard interactive.counts.waiting else {
                    work.cancel(); _ = try? await work.value
                    XCTFail("Additional-factor prompt was not reached")
                    throw MultifactorFixtureError.promptNotReached
                }
                do { try await session.openShell(geometry: .init(columns: 80, rows: 24)); XCTFail("Partial authentication opened a shell") }
                catch { XCTAssertEqual(error as? NativeSSHError, .busy) }
                do { _ = try await session.accounting(); XCTFail("Worker-owned native handle was inspected") }
                catch { XCTAssertEqual(error as? NativeSSHError, .busy) }
                if mode == "cancel" { work.cancel() }
                if mode == "close-prompt" { await session.close() }
                if mode == "revoke-key-prompt" { key.revoke(); interactive.releasePrompt() }
                if mode == "revoke-interactive-prompt" { interactive.revoke(releasingPrompt: true) }
                do { _ = try await work.value; XCTFail("An interrupted additional factor authenticated") }
                catch {
                    if mode == "cancel" { XCTAssertTrue(error is CancellationError) }
                    if mode == "combined-deadline" { XCTAssertEqual(error as? NativeSSHError, .authenticationTimedOut) }
                    if mode.hasPrefix("revoke-") { XCTAssertTrue(error is MultifactorFixtureError) }
                }
                if mode == "combined-deadline" {
                    XCTAssertLessThan(started.duration(to: .now), .milliseconds(2800), "The second factor must not reset the attempt deadline")
                }
                XCTAssertEqual(key.counts.signatures, 1)
                XCTAssertEqual(interactive.counts.accounts, 1); XCTAssertEqual(interactive.counts.prompts, 1)
            } else if ["wrong-host", "wrong-key", "rejected-signature", "unsigned-success", "unsigned-partial",
                       "unsupported-method", "wrong-account", "wrong-response", "interactive-partial", "key-only-api"].contains(mode) {
                do {
                    if mode == "key-only-api" { _ = try await session.authenticate(using: key, timeout: .seconds(5)) }
                    else { _ = try await session.authenticate(using: key, continuingWithInteractive: interactive, timeout: .seconds(5)) }
                    XCTFail("A refused or unauthorized factor completed login")
                } catch {
                    if mode == "wrong-host" { XCTAssertTrue(error is MultifactorFixtureError) }
                    else { XCTAssertTrue(error is NativeSSHError) }
                    if mode == "wrong-key" { XCTAssertTrue((error as? NativeSSHError)?.isCredentialRefused == true) }
                    if ["rejected-signature", "unsigned-success", "unsigned-partial", "unsupported-method", "wrong-account",
                        "wrong-response", "interactive-partial", "key-only-api"].contains(mode) {
                        XCTAssertFalse((error as? NativeSSHError)?.isCredentialRefused == true)
                    }
                }
                let noSignature = ["wrong-host", "wrong-key", "unsigned-success", "unsigned-partial"].contains(mode)
                XCTAssertEqual(key.counts.signatures, noSignature ? 0 : 1)
                let accounts = ["wrong-account", "wrong-response", "interactive-partial"].contains(mode) ? 1 : 0
                XCTAssertEqual(interactive.counts.accounts, accounts)
                XCTAssertEqual(interactive.counts.prompts, mode == "wrong-response" ? 1 : mode == "interactive-partial" ? 2 : 0)
            } else {
                let authenticated = try await session.authenticate(using: key, continuingWithInteractive: interactive, timeout: .seconds(5))
                XCTAssertEqual(authenticated.peer.transportID, session.transportID)
                XCTAssertEqual(authenticated.peer.endpoint, endpoint); XCTAssertEqual(authenticated.peer.hostKey, host)
                XCTAssertEqual(authenticated.usedKeyboardInteractive, mode != "key-only")
                XCTAssertEqual(key.counts.signatures, 1)
                XCTAssertEqual(interactive.counts.accounts, mode == "key-only" ? 0 : 1)
                XCTAssertEqual(interactive.counts.prompts, mode == "key-only" ? 0 : 2)
                if ["revoke-key-live", "revoke-interactive-live", "hop-key-revoked", "hop-interactive-revoked"].contains(mode) {
                    if mode == "revoke-key-live" { key.revoke() }
                    if mode == "revoke-interactive-live" { interactive.revoke() }
                    if mode == "hop-key-revoked" { ancestor?.key.revoke() }
                    if mode == "hop-interactive-revoked" { ancestor?.interactive.revoke() }
                    do { try await session.openShell(geometry: .init(columns: 80, rows: 24)); XCTFail("A revoked authentication factor opened a terminal") }
                    catch { XCTAssertTrue(error is MultifactorFixtureError) }
                } else {
                    try await session.openShell(geometry: .init(columns: 80, rows: 24))
                    let input = Data("Native MFA → owned terminal.\rEND-SESSION\r".utf8)
                    let admission = try await session.admit(input)
                    XCTAssertEqual(admission, .accepted(input.count))
                    var output = Data(), ended = false
                    let deadline = ContinuousClock.now + .seconds(5)
                    while !ended, ContinuousClock.now < deadline {
                        let event = try await session.service(timeout: .seconds(1))
                        output += event.output; ended = event.endOfOutput
                    }
                    XCTAssertTrue(ended)
                    XCTAssertEqual(output, Data("ready\r\nstderr\r\n".utf8) + input)
                }
            }
            await session.close()
            let stats = try await session.accounting()
            XCTAssertEqual(stats.allocatedBytes, 0)
            XCTAssertFalse(interactive.counts.waiting)
            if interactive.counts.accounts == 0 { XCTAssertEqual(interactive.counts.cancellations, 0, "An unused additional-factor provider stays untouched") }
            if let ancestor {
                XCTAssertGreaterThan(ancestor.key.counts.cancellations, 0)
                XCTAssertGreaterThan(ancestor.interactive.counts.cancellations, 0)
            }
        } catch { await session.close(); throw error }
    }
}
