import Foundation
import XCTest
@testable import FrankenTermSSH
#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

private final class NeverAuthenticate: NativeSSHAuthentication, @unchecked Sendable {
    private let lock = NSLock()
    private var calls = 0
    var cancellations: Int { lock.lock(); defer { lock.unlock() }; return calls }
    func identity(for peer: NativeSSHPeer) async throws -> NativeSSHIdentity {
        XCTFail("A silent peer cannot reach host approval"); throw NativeSSHError.invalidInput
    }
    func signature(for challenge: NativeSSHChallenge, peer: NativeSSHPeer) async throws -> Data {
        XCTFail("A silent peer cannot reach signing"); throw NativeSSHError.invalidInput
    }
    func validate(peer: NativeSSHPeer, identity: NativeSSHIdentity) async throws { XCTFail("Not authenticated") }
    func cancel(transportID: UUID) { lock.lock(); calls += 1; lock.unlock() }
}

private func pair() throws -> [Int32] {
    var descriptors = [Int32](repeating: -1, count: 2)
#if os(Linux)
    let kind = Int32(SOCK_STREAM.rawValue)
#else
    let kind = SOCK_STREAM
#endif
    guard socketpair(AF_UNIX, kind, 0, &descriptors) == 0 else { throw NativeSSHError.invalidInput }
    _ = fcntl(descriptors[1], F_SETFL, O_NONBLOCK)
    return descriptors
}
private func closeFD(_ value: Int32) {
#if canImport(Darwin)
    _ = Darwin.close(value)
#else
    _ = Glibc.close(value)
#endif
}
private func banner(_ peer: Int32) async throws {
    let deadline = ContinuousClock.now + .seconds(2)
    var bytes = [UInt8](repeating: 0, count: 256)
    while ContinuousClock.now < deadline {
        if recv(peer, &bytes, bytes.count, 0) > 0 { return }
        try await Task.sleep(for: .milliseconds(1))
    }
    throw NativeSSHError.timedOut
}

final class SessionTests: XCTestCase, @unchecked Sendable {
    func testHandshakeDeadlineClosesAndReleasesNativeOwnership() async throws {
        let sockets = try pair(); defer { closeFD(sockets[1]) }
        let session = try NativeSSHSession(connectedDescriptor: sockets[0])
        let authority = NeverAuthenticate()
        do {
            _ = try await session.authenticate(using: authority, timeout: .milliseconds(40))
            XCTFail("Silent peer authenticated")
        } catch { XCTAssertEqual(error as? NativeSSHError, .timedOut) }
        let stats = try await session.accounting()
        XCTAssertEqual(stats.allocatedBytes, 0)
        XCTAssertGreaterThanOrEqual(authority.cancellations, 1)
        do { _ = try await session.authenticate(using: authority); XCTFail("Revived closed source") }
        catch { XCTAssertEqual(error as? NativeSSHError, .closed) }
    }

    func testConcurrentCloseSettlesReadinessAndLateAuthentication() async throws {
        let sockets = try pair(); defer { closeFD(sockets[1]) }
        let session = try NativeSSHSession(connectedDescriptor: sockets[0])
        let authority = NeverAuthenticate()
        let attempt = Task { try await session.authenticate(using: authority) }
        try await banner(sockets[1])
        async let first: Void = session.close()
        async let second: Void = session.close()
        _ = await (first, second)
        do { _ = try await attempt.value; XCTFail("Closed handshake succeeded") }
        catch { XCTAssertEqual(error as? NativeSSHError, .closed) }
        let stats = try await session.accounting()
        XCTAssertEqual(stats.allocatedBytes, 0)
        await session.close()
    }

    func testTaskCancellationWakesSocketAndClosesNativeSource() async throws {
        let sockets = try pair(); defer { closeFD(sockets[1]) }
        let session = try NativeSSHSession(connectedDescriptor: sockets[0])
        let authority = NeverAuthenticate()
        let attempt = Task { try await session.authenticate(using: authority) }
        try await banner(sockets[1]); attempt.cancel()
        do { _ = try await attempt.value; XCTFail("Cancelled handshake succeeded") }
        catch { XCTAssertTrue(error is CancellationError || error as? NativeSSHError == .closed) }
        let stats = try await session.accounting()
        XCTAssertEqual(stats.allocatedBytes, 0)
        XCTAssertGreaterThanOrEqual(authority.cancellations, 1)
    }

    func testSecondAuthenticationCannotOverwritePendingOperation() async throws {
        let sockets = try pair(); defer { closeFD(sockets[1]) }
        let session = try NativeSSHSession(connectedDescriptor: sockets[0])
        let authority = NeverAuthenticate()
        let attempt = Task { try await session.authenticate(using: authority) }
        try await banner(sockets[1])
        do { _ = try await session.authenticate(using: authority); XCTFail("Allowed competing authentication") }
        catch { XCTAssertEqual(error as? NativeSSHError, .busy) }
        await session.close()
        _ = try? await attempt.value
    }

    func testInvalidTimeoutDoesNotStartAnOperation() async throws {
        let sockets = try pair(); defer { closeFD(sockets[1]) }
        let session = try NativeSSHSession(connectedDescriptor: sockets[0])
        let authority = NeverAuthenticate()
        for timeout: Duration in [.zero, .seconds(-1), .seconds(121)] {
            do { _ = try await session.authenticate(using: authority, timeout: timeout); XCTFail("Invalid deadline accepted") }
            catch { XCTAssertEqual(error as? NativeSSHError, .invalidInput) }
        }
        XCTAssertEqual(authority.cancellations, 0)
        await session.close()
    }

    func testPreAuthenticationCannotAdmitOrCreateShell() async throws {
        let sockets = try pair(); defer { closeFD(sockets[1]) }
        let session = try NativeSSHSession(connectedDescriptor: sockets[0])
        do { _ = try await session.admit(Data("secret".utf8)); XCTFail("Premature input") }
        catch { XCTAssertEqual(error as? NativeSSHError, .closed) }
        do { try await session.openShell(geometry: NativeSSHGeometry(columns: 80, rows: 24)); XCTFail("Premature shell") }
        catch { XCTAssertEqual(error as? NativeSSHError, .busy) }
        let stats = try await session.accounting()
        XCTAssertEqual(stats.locallyAcceptedBytes, 0)
        await session.close()
    }

    func testRetainedClosedSessionsReleaseGlobalNativeAdmission() async throws {
        var retained: [NativeSSHSession] = []
        for _ in 0..<65 {
            let sockets = try pair()
            let session = try NativeSSHSession(connectedDescriptor: sockets[0])
            await session.close(); closeFD(sockets[1])
            retained.append(session)
        }
        for session in retained {
            let stats = try await session.accounting()
            XCTAssertEqual(stats.allocatedBytes, 0)
            XCTAssertEqual(stats.terminalError, 0)
            XCTAssertGreaterThan(stats.peakAllocatedBytes, 0)
        }
    }

    func testGeometryAndIdentityBounds() throws {
        for (c, r): (UInt32, UInt32) in [(0, 24), (80, 0), (513, 24), (256, 257), (.max, .max)] {
            XCTAssertThrowsError(try NativeSSHGeometry(columns: c, rows: r))
        }
        XCTAssertThrowsError(try NativeSSHGeometry(columns: 80, rows: 24, pixelWidth: 65536))
        XCTAssertEqual(try NativeSSHGeometry(columns: 512, rows: 128).rows, 128)
        for user in ["", "a\0b", "a\nb", String(repeating: "é", count: 129)] {
            XCTAssertThrowsError(try NativeSSHIdentity(user: user, publicKey: Data([1])))
        }
        XCTAssertThrowsError(try NativeSSHIdentity(user: "operator", publicKey: Data()))
        XCTAssertThrowsError(try NativeSSHIdentity(user: "operator", publicKey: Data(repeating: 1, count: 513)))
    }

    func testTypedChallengeRequiresEveryExactFieldAndNoTrailingData() throws {
        func string(_ value: Data) -> Data {
            var n = UInt32(value.count).bigEndian
            return withUnsafeBytes(of: &n) { Data($0) } + value
        }
        let key = string(Data("ssh-ed25519".utf8)) + string(Data(repeating: 7, count: 32))
        let identity = try NativeSSHIdentity(user: "operator", publicKey: key)
        let sid = Data(repeating: 9, count: 32)
        let bytes = string(sid) + Data([50]) + string(Data("operator".utf8)) + string(Data("ssh-connection".utf8))
            + string(Data("publickey".utf8)) + Data([1]) + string(Data("ssh-ed25519".utf8)) + string(key)
        let id = UUID()
        let challenge = try NativeSSHChallenge(bytes: bytes, transportID: id, identity: identity)
        XCTAssertEqual(challenge.transportID, id)
        XCTAssertEqual(challenge.sessionIdentifier, sid)
        XCTAssertEqual(challenge.signedData, bytes)
        for size in 0..<bytes.count {
            XCTAssertThrowsError(try NativeSSHChallenge(bytes: Data(bytes.prefix(size)), transportID: id, identity: identity))
        }
        XCTAssertThrowsError(try NativeSSHChallenge(bytes: bytes + Data([0]), transportID: id, identity: identity))
        XCTAssertThrowsError(try NativeSSHChallenge(bytes: bytes, transportID: id,
                                                   identity: NativeSSHIdentity(user: "other", publicKey: key)))
    }

#if os(Linux)
    func testRepeatedCancellationDoesNotLeakReadinessDescriptors() async throws {
        // Warm Dispatch once before taking the fd baseline.
        try await testTaskCancellationWakesSocketAndClosesNativeSource()
        let before = try FileManager.default.contentsOfDirectory(atPath: "/proc/self/fd").count
        for _ in 0..<64 { try await testConcurrentCloseSettlesReadinessAndLateAuthentication() }
        let after = try FileManager.default.contentsOfDirectory(atPath: "/proc/self/fd").count
        XCTAssertLessThanOrEqual(after, before + 1)
    }
#endif
}
