import Foundation
import XCTest
@testable import FrankenTermSSH
#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

private func closeTestDescriptor(_ fd: Int32) {
#if canImport(Darwin)
    _ = Darwin.close(fd)
#else
    _ = Glibc.close(fd)
#endif
}
private final class Listener {
    let fd: Int32, port: UInt16
    var peers: [Int32] = []
    init(host: String) throws {
        var hints = addrinfo(), address: UnsafeMutablePointer<addrinfo>?
        hints.ai_family = AF_UNSPEC
#if os(Linux)
        hints.ai_socktype = Int32(SOCK_STREAM.rawValue)
#else
        hints.ai_socktype = SOCK_STREAM
#endif
        hints.ai_flags = AI_NUMERICHOST | AI_NUMERICSERV
        guard getaddrinfo(host, "0", &hints, &address) == 0, let address else { throw NativeSSHError.invalidInput }
        defer { freeaddrinfo(address) }
        let info = address.pointee
        fd = socket(info.ai_family, info.ai_socktype, info.ai_protocol)
        guard fd >= 0 else { throw NativeSSHError.invalidInput }
        guard fcntl(fd, F_SETFL, O_NONBLOCK) == 0, bind(fd, info.ai_addr, info.ai_addrlen) == 0, listen(fd, 32) == 0 else {
            closeTestDescriptor(fd); throw NativeSSHError.invalidInput
        }
        var actual = sockaddr_storage(), length = socklen_t(MemoryLayout<sockaddr_storage>.size)
        var service = [CChar](repeating: 0, count: 32)
        let socketFD = fd
        let result = withUnsafeMutablePointer(to: &actual) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                guard getsockname(socketFD, $0, &length) == 0 else { return Int32(-1) }
                return getnameinfo($0, length, nil, 0, &service, 32, NI_NUMERICSERV)
            }
        }
        guard result == 0, let port = UInt16(String(decoding: service.prefix(while: { $0 != 0 }).map(UInt8.init(bitPattern:)), as: UTF8.self)) else {
            closeTestDescriptor(fd); throw NativeSSHError.invalidInput
        }
        self.port = port
    }
    func acceptPeer() throws -> Int32 {
        let peer = accept(fd, nil, nil)
        guard peer >= 0 else { throw NativeSSHError.connection(errno) }
        _ = fcntl(peer, F_SETFL, O_NONBLOCK); peers.append(peer); return peer
    }
    deinit { for peer in peers { closeTestDescriptor(peer) }; closeTestDescriptor(fd) }
}

final class ConnectorTests: XCTestCase, @unchecked Sendable {
    private func connected(host: String, listenerHost: String) async throws {
        let listener = try Listener(host: listenerHost)
        let endpoint = try NativeSSHEndpoint(host: host, port: listener.port)
        let session = try await NativeSSHConnector.connect(to: endpoint, timeout: .seconds(2))
        XCTAssertEqual(session.endpoint, endpoint)
        let peer = try listener.acceptPeer()
        var byte: UInt8 = 0
        XCTAssertEqual(recv(peer, &byte, 1, 0), -1)
        XCTAssertEqual(errno, EAGAIN, "TCP creation must not offer a key or begin SSH")
        await session.close()
        let stats = try await session.accounting()
        XCTAssertEqual(stats.allocatedBytes, 0)
    }
    func testNumericIPv4OwnsExactlyOneSocketWithoutImplicitAuthentication() async throws {
        try await connected(host: "127.0.0.1", listenerHost: "127.0.0.1")
    }
    func testIPv6LiteralConnection() async throws {
        try await connected(host: "::1", listenerHost: "::1")
    }
    func testLocalhostFallsBackToListeningAddressFamily() async throws {
        try await connected(host: "localhost", listenerHost: "127.0.0.1")
    }
    func testInvalidEndpointsAndTimeoutsFailBeforeNetworkUse() async throws {
        for host in ["", "user@host", "https://host", "host/path", "[::1]", "host\0", " host", "界.example", String(repeating: "x", count: 254)] {
            XCTAssertThrowsError(try NativeSSHEndpoint(host: host))
        }
        XCTAssertThrowsError(try NativeSSHEndpoint(host: "localhost", port: 0))
        for timeout: Duration in [.zero, .seconds(-1), .seconds(121)] {
            do { _ = try await NativeSSHConnector.connect(to: NativeSSHEndpoint(host: "localhost"), timeout: timeout); XCTFail("Invalid timeout accepted") }
            catch { XCTAssertEqual(error as? NativeSSHError, .invalidInput) }
        }
    }
    func testExpiredResolutionDoesNotCreateTCPConnection() async throws {
        let listener = try Listener(host: "127.0.0.1")
        do {
            _ = try await NativeSSHConnector.connect(to: NativeSSHEndpoint(host: "127.0.0.1", port: listener.port), timeout: .nanoseconds(1))
            XCTFail("Expired connection accepted")
        } catch { XCTAssertEqual(error as? NativeSSHError, .timedOut) }
        XCTAssertThrowsError(try listener.acceptPeer())
    }
    func testConnectionRefusalIsNotReportedAsAuthenticationFailure() async throws {
        var listener: Listener? = try Listener(host: "127.0.0.1")
        let port = try XCTUnwrap(listener?.port)
        listener = nil
        do { _ = try await NativeSSHConnector.connect(to: NativeSSHEndpoint(host: "127.0.0.1", port: port)); XCTFail("Closed listener connected") }
        catch { XCTAssertEqual(error as? NativeSSHError, .connection(ECONNREFUSED)) }
    }
    func testAlreadyCancelledTaskCannotResolveOrConnect() async throws {
        let listener = try Listener(host: "127.0.0.1")
        let endpoint = try NativeSSHEndpoint(host: "localhost", port: listener.port)
        let task = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await NativeSSHConnector.connect(to: endpoint)
        }
        do { _ = try await task.value; XCTFail("Cancelled connector succeeded") }
        catch { XCTAssertTrue(error is CancellationError) }
        XCTAssertThrowsError(try listener.acceptPeer())
    }
    func testConcurrentCancellationAndBudgetRecovery() async throws {
        let listener = try Listener(host: "127.0.0.1")
        let endpoint = try NativeSSHEndpoint(host: "localhost", port: listener.port)
        await withTaskGroup(of: Void.self) { group in
            for _ in 0..<64 {
                group.addTask {
                    do {
                        let session = try await NativeSSHConnector.connect(to: endpoint, timeout: .milliseconds(100))
                        await session.close()
                    } catch { }
                }
            }
            group.cancelAll()
        }
        let deadline = ContinuousClock.now + .seconds(2)
        while ContinuousClock.now < deadline {
            let state = NativeSSHConnector.accounting()
            if state.resolvingHosts == 0 && state.connectingSockets == 0 { break }
            try await Task.sleep(for: .milliseconds(1))
        }
        let state = NativeSSHConnector.accounting()
        XCTAssertEqual(state.resolvingHosts, 0)
        XCTAssertEqual(state.connectingSockets, 0)
        try await connected(host: "localhost", listenerHost: "127.0.0.1")
    }
#if os(Linux)
    func testHostnameRaceLosingDescriptorsAreReleased() async throws {
        try await connected(host: "localhost", listenerHost: "127.0.0.1")
        let before = try FileManager.default.contentsOfDirectory(atPath: "/proc/self/fd").count
        for _ in 0..<20 { try await connected(host: "localhost", listenerHost: "127.0.0.1") }
        let after = try FileManager.default.contentsOfDirectory(atPath: "/proc/self/fd").count
        XCTAssertLessThanOrEqual(after, before + 1)
    }
#endif
}
