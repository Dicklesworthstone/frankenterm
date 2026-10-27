import Foundation
import XCTest
@testable import FrankenTermSSH

private enum PasswordAuthorityFailure: Error { case wrongPeer, revoked, duplicatePrompt, promptNotReached }

/// Synthetic prompt ownership; the interop server independently observes which
/// authentication requests and terminal effects actually crossed the socket.
private final class PasswordTestAuthority: NativeSSHPasswordAuthentication, @unchecked Sendable {
    private let lock = NSLock()
    private let endpoint: NativeSSHEndpoint, hostKey: Data, secret: Data
    private let pauses: Bool
    private var peer: NativeSSHPeer?
    private var retired = false, revoked = false
    private var accounts = 0, prompts = 0
    private var pending: CheckedContinuation<Data, Error>?

    init(endpoint: NativeSSHEndpoint, hostKey: Data, secret: Data, pauses: Bool = false) {
        self.endpoint = endpoint; self.hostKey = hostKey; self.secret = secret; self.pauses = pauses
    }
    var counts: (accounts: Int, prompts: Int, waiting: Bool) {
        lock.withLock { (accounts, prompts, pending != nil) }
    }
    func account(for peer: NativeSSHPeer) async throws -> NativeSSHPasswordAccount {
        try lock.withLock {
            guard !retired, !revoked else { throw PasswordAuthorityFailure.revoked }
            accounts += 1
            guard self.peer == nil, peer.endpoint == endpoint, peer.hostKey == hostKey else {
                throw PasswordAuthorityFailure.wrongPeer
            }
            self.peer = peer
            return try NativeSSHPasswordAccount(user: "operator")
        }
    }
    func password(for account: NativeSSHPasswordAccount, peer: NativeSSHPeer) async throws -> Data {
        try await validate(peer: peer, account: account)
        return try await withCheckedThrowingContinuation { continuation in
            let result: Result<Data, Error>? = lock.withLock {
                guard !retired, !revoked else { return .failure(PasswordAuthorityFailure.revoked) }
                guard prompts == 0 else { return .failure(PasswordAuthorityFailure.duplicatePrompt) }
                prompts += 1
                if pauses { pending = continuation; return nil }
                return .success(secret)
            }
            if let result { continuation.resume(with: result) }
        }
    }
    func validate(peer: NativeSSHPeer, account: NativeSSHPasswordAccount) async throws {
        try lock.withLock {
            guard !retired, !revoked else { throw PasswordAuthorityFailure.revoked }
            guard peer == self.peer, peer.endpoint == endpoint, peer.hostKey == hostKey, account.user == "operator" else {
                throw PasswordAuthorityFailure.wrongPeer
            }
        }
    }
    func cancel(transportID: UUID) {
        let waiting: CheckedContinuation<Data, Error>? = lock.withLock {
            guard peer == nil || peer?.transportID == transportID else { return nil }
            retired = true
            let value = pending; pending = nil; return value
        }
        waiting?.resume(throwing: CancellationError())
    }
    func revoke(releasingPrompt: Bool = false) {
        let waiting: CheckedContinuation<Data, Error>? = lock.withLock {
            revoked = true
            guard releasingPrompt else { return nil }
            let value = pending; pending = nil; return value
        }
        // Deliberately deliver a late successful prompt after policy changed.
        waiting?.resume(returning: secret)
    }
}

final class NativePasswordAuthenticationTests: XCTestCase, @unchecked Sendable {
    func testPasswordAccountRejectsInvalidOrOversizedUsers() throws {
        for user in ["", "bad\nuser", "bad\0user", String(repeating: "a", count: 257)] {
            XCTAssertThrowsError(try NativeSSHPasswordAccount(user: user))
        }
        XCTAssertEqual(try NativeSSHPasswordAccount(user: "alice").user, "alice")
    }

    func testPasswordFailureIsNeverUnsignedKeyRetryEvidence() {
        let failure = NativeSSHError.native(-1012)
        XCTAssertTrue(failure.isMachineAnswer)
        XCTAssertFalse(failure.isCredentialRefused)
        XCTAssertFalse(failure.isUnreachable)
    }

    /// Executed by Tests/Interop/test_password.py against a real independent
    /// server. Ordinary package tests skip only when no live fixture is supplied.
    func testLivePasswordPromptAndTerminalOwnership() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let port = env["FT_PASSWORD_TEST_PORT"].flatMap(UInt16.init),
              let host = env["FT_PASSWORD_TEST_HOSTKEY"].flatMap({ Data(base64Encoded: $0) }),
              let mode = env["FT_PASSWORD_TEST_MODE"] else {
            throw XCTSkip("Requires the independent password SSH interop fixture")
        }
        let endpoint = try NativeSSHEndpoint(host: "127.0.0.1", port: port)
        var expectedHost = host
        if mode == "wrong-host" { expectedHost[expectedHost.startIndex] ^= 1 }
        let secret = Data((mode == "wrong-password" ? "wrong-password" : "pw-é🔐").utf8)
        let pauses = ["cancel", "timeout", "revoke-prompt"].contains(mode)
        let authority = PasswordTestAuthority(endpoint: endpoint, hostKey: expectedHost, secret: secret, pauses: pauses)
        var ancestor: PasswordTestAuthority?
        let session: NativeSSHSession
        if mode.hasPrefix("forward-") {
            let hopPort = try XCTUnwrap(env["FT_PASSWORD_TEST_HOP_PORT"].flatMap(UInt16.init))
            let hopHost = try XCTUnwrap(env["FT_PASSWORD_TEST_HOP_HOSTKEY"].flatMap { Data(base64Encoded: $0) })
            let hopEndpoint = try NativeSSHEndpoint(host: "127.0.0.1", port: hopPort)
            let hopAuthority = PasswordTestAuthority(endpoint: hopEndpoint, hostKey: hopHost, secret: secret)
            let hop = try await NativeSSHConnector.connect(to: hopEndpoint, timeout: .seconds(5))
            do {
                _ = try await hop.authenticate(usingPassword: hopAuthority, timeout: .seconds(5))
                session = try await hop.forward(to: endpoint, timeout: .seconds(5))
                await hop.close() // a consumed actor cannot cancel its child's authority
                ancestor = hopAuthority
            } catch { await hop.close(); throw error }
        } else {
            session = try await NativeSSHConnector.connect(to: endpoint, timeout: .seconds(5))
        }
        do {
            if pauses {
                let work = Task {
                    try await session.authenticate(usingPassword: authority,
                        timeout: mode == "timeout" ? .seconds(1) : .seconds(5))
                }
                let deadline = ContinuousClock.now + .seconds(3)
                while !authority.counts.waiting, ContinuousClock.now < deadline {
                    try await Task.sleep(for: .milliseconds(5))
                }
                guard authority.counts.waiting else {
                    work.cancel(); _ = try? await work.value
                    XCTFail("Fixture did not reach its prompt before the bounded wait")
                    throw PasswordAuthorityFailure.promptNotReached
                }
                if mode == "cancel" { work.cancel() }
                if mode == "revoke-prompt" { authority.revoke(releasingPrompt: true) }
                do {
                    _ = try await work.value
                    XCTFail("A cancelled, expired or revoked password prompt authenticated")
                } catch {
                    if mode == "cancel" { XCTAssertTrue(error is CancellationError) }
                    if mode == "timeout" { XCTAssertEqual(error as? NativeSSHError, .authenticationTimedOut) }
                    if mode == "revoke-prompt" { XCTAssertTrue(error is PasswordAuthorityFailure) }
                }
                XCTAssertEqual(authority.counts.prompts, 1)
            } else if ["wrong-host", "wrong-password", "unsupported", "none", "partial", "change"].contains(mode) {
                do {
                    _ = try await session.authenticate(usingPassword: authority, timeout: .seconds(5))
                    XCTFail("A refused authentication published a live session")
                } catch {
                    if mode == "wrong-host" { XCTAssertTrue(error is PasswordAuthorityFailure) }
                    else { XCTAssertTrue(error is NativeSSHError) }
                    XCTAssertFalse((error as? NativeSSHError)?.isCredentialRefused == true)
                }
                XCTAssertEqual(authority.counts.prompts, ["wrong-password", "partial", "change"].contains(mode) ? 1 : 0)
            } else {
                let peer = try await session.authenticate(usingPassword: authority, timeout: .seconds(5))
                XCTAssertEqual(peer.hostKey, host)
                XCTAssertEqual(authority.counts.prompts, 1)
                if mode == "revoke-live" || mode == "forward-revoked" {
                    if mode == "revoke-live" { authority.revoke() } else { ancestor?.revoke() }
                    do {
                        try await session.openShell(geometry: .init(columns: 80, rows: 24))
                        XCTFail("A revoked password authority opened a terminal")
                    } catch { XCTAssertTrue(error is PasswordAuthorityFailure) }
                } else {
                    try await session.openShell(geometry: .init(columns: 80, rows: 24))
                    let input = Data("Password → owned terminal.\rEND-SESSION\r".utf8)
                    let admission = try await session.admit(input)
                    XCTAssertEqual(admission, .accepted(input.count))
                    var output = Data(), completed = false
                    let deadline = ContinuousClock.now + .seconds(5)
                    while !completed, ContinuousClock.now < deadline {
                        let event = try await session.service(timeout: .seconds(1))
                        output += event.output; completed = event.endOfOutput
                    }
                    XCTAssertTrue(completed)
                    XCTAssertEqual(output, Data("ready\r\nstderr\r\n".utf8) + input)
                }
            }
            await session.close()
            let stats = try await session.accounting()
            XCTAssertEqual(stats.allocatedBytes, 0)
            XCTAssertFalse(authority.counts.waiting)
        } catch { await session.close(); throw error }
    }
}
