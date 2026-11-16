import Foundation
import XCTest
@testable import FrankenTermSSH
#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

private final class TransportPolicyForbiddenAuthentication: NativeSSHAuthentication, @unchecked Sendable {
    private let lock = NSLock()
    private var invocations = 0
    var calls: Int { lock.lock(); defer { lock.unlock() }; return invocations }
    private func called() { lock.lock(); invocations += 1; lock.unlock() }
    func identity(for peer: NativeSSHPeer) throws -> NativeSSHIdentity {
        called(); throw NativeSSHError.invalidInput
    }
    func signature(for challenge: NativeSSHChallenge, peer: NativeSSHPeer) throws -> Data {
        called(); throw NativeSSHError.invalidInput
    }
    func validate(peer: NativeSSHPeer, identity: NativeSSHIdentity) throws {
        called(); throw NativeSSHError.invalidInput
    }
    func cancel(transportID: UUID) {}
}

final class NativeTransportAlgorithmTests: XCTestCase, @unchecked Sendable {
    private func pair() throws -> [Int32] {
        var sockets: [Int32] = [-1, -1]
        #if os(Linux)
        let kind = Int32(SOCK_STREAM.rawValue)
        #else
        let kind = SOCK_STREAM
        #endif
        guard socketpair(AF_UNIX, kind, 0, &sockets) == 0 else { throw NativeSSHError.invalidInput }
        _ = fcntl(sockets[1], F_SETFL, O_NONBLOCK)
        return sockets
    }

    private func closePeer(_ descriptor: Int32) {
        #if canImport(Darwin)
        _ = Darwin.close(descriptor)
        #else
        _ = Glibc.close(descriptor)
        #endif
    }

    func testInvalidTransportListsDoNotStartNetworkActivityOrWidenTheQualifiedProfile() async throws {
        XCTAssertTrue(NativeSSHSession.supportsTransportAlgorithmPolicy)
        let sockets = try pair(); defer { closePeer(sockets[1]) }
        let session = try NativeSSHSession(connectedDescriptor: sockets[0])
        let defaults = [NativeSSHSession.supportedKeyExchangeAlgorithms,
                        NativeSSHSession.supportedCiphers, NativeSSHSession.supportedMACs]
        do {
            XCTAssertEqual(defaults.map(\.count), [5, 2, 2])
            for dimension in 0..<3 {
                for invalid in [[], [defaults[dimension][0], defaults[dimension][0]], [""], ["unknown"],
                                ["+" + defaults[dimension][0]], [defaults[dimension][0] + "\0"],
                                [String(repeating: "x", count: 4096)]] {
                    var lists = defaults; lists[dimension] = invalid
                    do {
                        try await session.setTransportAlgorithms(keyExchangeAlgorithms: lists[0], ciphers: lists[1], macs: lists[2])
                        XCTFail("Malformed or unsupported transport policy was accepted")
                    } catch { XCTAssertEqual(error as? NativeSSHError, .invalidInput) }
                }
            }
            try await session.setTransportAlgorithms(keyExchangeAlgorithms: Array(defaults[0].reversed()),
                ciphers: Array(defaults[1].reversed()), macs: Array(defaults[2].reversed()))
            try await session.setHostKeyPolicy(.rawKeys)
            var byte: UInt8 = 0
            XCTAssertEqual(recv(sockets[1], &byte, 1, 0), -1, "Policy selection itself must not start key exchange")
            XCTAssertTrue(errno == EAGAIN || errno == EWOULDBLOCK)
            let accounting = try await session.accounting()
            XCTAssertEqual(accounting.locallyAcceptedBytes, 0)
            await session.close()
        } catch { await session.close(); throw error }
    }

    func testHandshakeFreezesTransportPolicyAndCancellationClosesOnlyItsExactOwner() async throws {
        let first = try pair(), second = try pair()
        defer { closePeer(first[1]); closePeer(second[1]) }
        let session = try NativeSSHSession(connectedDescriptor: first[0])
        let sibling = try NativeSSHSession(connectedDescriptor: second[0])
        let authority = TransportPolicyForbiddenAuthentication()
        let exchanges = NativeSSHSession.supportedKeyExchangeAlgorithms
        let ciphers = NativeSSHSession.supportedCiphers, macs = NativeSSHSession.supportedMACs
        try await session.setTransportAlgorithms(keyExchangeAlgorithms: exchanges, ciphers: ciphers, macs: macs)
        let pending = Task { try await session.authenticate(using: authority, timeout: .seconds(5)) }
        do {
            let deadline = ContinuousClock.now + .seconds(2)
            var bytes = [UInt8](repeating: 0, count: 1024), sawHandshake = false
            while ContinuousClock.now < deadline {
                if recv(first[1], &bytes, bytes.count, 0) > 0 { sawHandshake = true; break }
                try await Task.sleep(for: .milliseconds(2))
            }
            XCTAssertTrue(sawHandshake)
            do {
                try await session.setTransportAlgorithms(keyExchangeAlgorithms: exchanges, ciphers: ciphers, macs: macs)
                XCTFail("Policy changed after handshake admission")
            } catch { XCTAssertEqual(error as? NativeSSHError, .invalidInput) }
            pending.cancel()
            do { _ = try await pending.value; XCTFail("A cancelled silent peer authenticated") }
            catch { XCTAssertTrue(error is CancellationError) }
            XCTAssertEqual(authority.calls, 0)
            let closed = try await session.accounting(), retained = try await sibling.accounting()
            XCTAssertEqual(closed.allocatedBytes, 0)
            XCTAssertGreaterThan(retained.allocatedBytes, 0)
            try await sibling.setTransportAlgorithms(keyExchangeAlgorithms: [exchanges[0]], ciphers: [ciphers[1]], macs: [macs[1]])
            await sibling.close()
        } catch {
            pending.cancel(); _ = try? await pending.value
            await session.close(); await sibling.close(); throw error
        }
    }
}
