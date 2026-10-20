import Foundation
import Dispatch
import CFrankenTermSSH
#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

/// A resolved workspace supplies a canonical ASCII DNS name or unbracketed IP
/// literal. This layer does not reinterpret aliases, execute SSH configuration,
/// convert an arbitrary URL into a destination, or weaken host verification.
public struct NativeSSHEndpoint: Sendable, Hashable {
    public let host: String, port: UInt16
    public init(host: String, port: UInt16 = 22) throws {
        guard !host.isEmpty, host.utf8.count <= 253, port != 0,
              host.utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) ||
                  (97...122).contains($0) || [45, 46, 58, 37, 95].contains($0) }) else {
            throw NativeSSHError.invalidInput
        }
        self.host = host; self.port = port
    }
}

public struct NativeSSHConnectorAccounting: Sendable {
    public let resolvingHosts: Int, connectingSockets: Int
}

/// Explicit foreground connection only. The caller must enforce its persistent
/// Connect Later / network-inhibition / route-revision policy before invoking it.
/// It returns an unauthenticated session; no key offer or shell starts here.
public enum NativeSSHConnector {
    /// Individually synchronized process-wide counters; not a joint transaction.
    public static func accounting() -> NativeSSHConnectorAccounting {
        .init(resolvingHosts: ConnectionBudget.resolvers.current, connectingSockets: ConnectionBudget.sockets.current)
    }
    public static func connect(to endpoint: NativeSSHEndpoint, timeout: Duration = .seconds(15),
                               maximumNativeBytes: Int = 2 * 1024 * 1024) async throws -> NativeSSHSession {
        guard timeout > .zero, timeout <= .seconds(120), (65536...4 * 1024 * 1024).contains(maximumNativeBytes) else {
            throw NativeSSHError.invalidInput
        }
        try Task.checkCancellation()
        let deadline = ContinuousClock.now + timeout
        let addresses = try await HostResolution(endpoint: endpoint, deadline: deadline).value()
        try Task.checkCancellation()
        guard ContinuousClock.now < deadline else { throw NativeSSHError.timedOut }
        let result = await race(addresses, deadline: deadline)
        try Task.checkCancellation()
        guard ContinuousClock.now < deadline else { throw NativeSSHError.timedOut }
        guard let socket = result.0 else { throw result.1 }
        let session = try socket.promote(endpoint: endpoint, maximumNativeBytes: maximumNativeBytes)
        if Task.isCancelled { await session.close(); throw CancellationError() }
        return session
    }

    private static func race(_ addresses: [SocketAddress], deadline: ContinuousClock.Instant) async -> (ConnectionSocket?, NativeSSHError) {
        await withTaskGroup(of: SocketAttempt.self) { group in
            var next = 0, error = NativeSSHError.native(Int32(FTSSH_IO))
            func add(_ delay: Duration) {
                let address = addresses[next]; next += 1
                group.addTask {
                    do {
                        if delay > .zero { try await Task.sleep(for: delay) }
                        try Task.checkCancellation()
                        let limit = min(deadline, ContinuousClock.now + .seconds(5))
                        return .connected(try await ConnectionSocket.connect(address, deadline: limit))
                    } catch let failure as NativeSSHError { return .failed(failure) }
                    catch { return .failed(.closed) }
                }
            }
            add(.zero)
            if next < addresses.count { add(.milliseconds(250)) }
            while let result = await group.next() {
                switch result {
                case .connected(let socket):
                    group.cancelAll()
                    return (socket, error)
                case .failed(let failure):
                    error = failure
                    if next < addresses.count, !Task.isCancelled, ContinuousClock.now < deadline { add(.zero) }
                }
            }
            return (nil, error)
        }
    }
}

private struct SocketAddress: Sendable {
    let family: Int32, bytes: Data
}
private enum SocketAttempt: Sendable { case connected(ConnectionSocket), failed(NativeSSHError) }

/// Includes cancelled but still-running getaddrinfo jobs. There is no unbounded
/// waiting queue, semaphore wait on a Swift executor, or one thread per host.
private final class ConnectionBudget: @unchecked Sendable {
    static let resolvers = ConnectionBudget(limit: 4)
    static let sockets = ConnectionBudget(limit: 32)
    private let lock = NSLock(), limit: Int
    private var count = 0
    init(limit: Int) { self.limit = limit }
    var current: Int { lock.lock(); defer { lock.unlock() }; return count }
    func acquire() -> Bool {
        lock.lock(); defer { lock.unlock() }
        guard count < limit else { return false }; count += 1; return true
    }
    func release() { lock.lock(); count -= 1; lock.unlock() }
}

/// POSIX name resolution is not cancellable. Its bounded worker retains its slot
/// until the native call returns, even after the requesting task has been resumed
/// with cancellation/timeout. Late results are freed and never used to connect.
private final class HostResolution: @unchecked Sendable {
    private static let queue = DispatchQueue(label: "org.frankenterm.ssh.resolve", qos: .userInitiated, attributes: .concurrent)
    private let endpoint: NativeSSHEndpoint, deadline: ContinuousClock.Instant
    private let lock = NSLock()
    private var continuation: CheckedContinuation<[SocketAddress], Error>?
    private var outcome: Result<[SocketAddress], Error>?
    private var timer: DispatchSourceTimer?
    init(endpoint: NativeSSHEndpoint, deadline: ContinuousClock.Instant) { self.endpoint = endpoint; self.deadline = deadline }
    func value() async throws -> [SocketAddress] {
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in start(continuation) }
        } onCancel: { self.finish(.failure(CancellationError())) }
    }
    private func start(_ continuation: CheckedContinuation<[SocketAddress], Error>) {
        lock.lock()
        if let outcome { lock.unlock(); continuation.resume(with: outcome); return }
        self.continuation = continuation
        lock.unlock()
        guard ConnectionBudget.resolvers.acquire() else { finish(.failure(NativeSSHError.busy)); return }
        let remaining = ContinuousClock.now.duration(to: deadline)
        if remaining <= .zero {
            ConnectionBudget.resolvers.release(); finish(.failure(NativeSSHError.timedOut)); return
        }
        let timer = DispatchSource.makeTimerSource(queue: Self.queue)
        let parts = remaining.components
        let nanos = parts.seconds * 1_000_000_000 + parts.attoseconds / 1_000_000_000
        timer.schedule(deadline: .now() + .nanoseconds(Int(max(1, nanos))))
        timer.setEventHandler { self.finish(.failure(NativeSSHError.timedOut)) }
        lock.lock()
        let retired = outcome != nil
        if !retired { self.timer = timer }
        lock.unlock()
        timer.resume()
        if retired { timer.cancel(); ConnectionBudget.resolvers.release(); return }
        Self.queue.async {
            defer { ConnectionBudget.resolvers.release() }
            self.lock.lock(); let live = self.outcome == nil; self.lock.unlock()
            guard live else { return }
            let result = Result { try Self.resolve(self.endpoint) }
            if ContinuousClock.now >= self.deadline { self.finish(.failure(NativeSSHError.timedOut)) }
            else { self.finish(result) }
        }
    }
    private func finish(_ result: Result<[SocketAddress], Error>) {
        lock.lock()
        guard outcome == nil else { lock.unlock(); return }
        outcome = result
        let continuation = self.continuation, timer = self.timer
        self.continuation = nil; self.timer = nil
        lock.unlock()
        timer?.cancel(); continuation?.resume(with: result)
    }
    private static func resolve(_ endpoint: NativeSSHEndpoint) throws -> [SocketAddress] {
        var hints = addrinfo(), result: UnsafeMutablePointer<addrinfo>?
        hints.ai_family = AF_UNSPEC; hints.ai_protocol = Int32(IPPROTO_TCP)
#if os(Linux)
        hints.ai_socktype = Int32(SOCK_STREAM.rawValue)
#else
        hints.ai_socktype = SOCK_STREAM
#endif
        hints.ai_flags = AI_NUMERICSERV
        let code = getaddrinfo(endpoint.host, String(endpoint.port), &hints, &result)
        guard code == 0 else { throw NativeSSHError.resolution(code) }
        defer { if let result { freeaddrinfo(result) } }
        var cursor = result, addresses: [SocketAddress] = [], seen = Set<Data>(), visited = 0
        while let current = cursor, visited < 64, addresses.count < 8 {
            visited += 1; cursor = current.pointee.ai_next
            let item = current.pointee
            let expected = item.ai_family == AF_INET ? MemoryLayout<sockaddr_in>.size : MemoryLayout<sockaddr_in6>.size
            guard item.ai_family == AF_INET || item.ai_family == AF_INET6,
                  item.ai_addrlen == expected, let address = item.ai_addr else { continue }
            let data = Data(bytes: address, count: expected)
            if seen.insert(data).inserted { addresses.append(.init(family: item.ai_family, bytes: data)) }
        }
        guard let first = addresses.first else { throw NativeSSHError.resolution(EAI_NONAME) }
        // Preserve resolver preference within each family while alternating the
        // two families. At most two candidates run; losers release all resources.
        var preferred = addresses.filter { $0.family == first.family }
        var other = addresses.filter { $0.family != first.family }
        var ordered: [SocketAddress] = []
        while !preferred.isEmpty || !other.isEmpty {
            if !preferred.isEmpty { ordered.append(preferred.removeFirst()) }
            if !other.isEmpty { ordered.append(other.removeFirst()) }
        }
        return ordered
    }
}

/// This owner travels from one connection task to the winning caller. No mutable
/// descriptor access overlaps; the lock additionally makes take/close idempotent.
private final class ConnectionSocket: @unchecked Sendable {
    private let lock = NSLock()
    private var descriptor: Int32
    private var admitted = true
    private init(_ descriptor: Int32) { self.descriptor = descriptor }
    deinit { close() }
    private func close() {
        lock.lock()
        let fd = descriptor, release = admitted
        descriptor = -1; admitted = false
        lock.unlock()
        if fd >= 0 { closeSocket(fd) }
        if release { ConnectionBudget.sockets.release() }
    }
    func promote(endpoint: NativeSSHEndpoint, maximumNativeBytes: Int) throws -> NativeSSHSession {
        lock.lock(); defer { lock.unlock() }
        guard descriptor >= 0, admitted else { throw NativeSSHError.closed }
        let session = try NativeSSHSession(connectedDescriptor: descriptor, endpoint: endpoint, maximumNativeBytes: maximumNativeBytes)
        descriptor = -1; admitted = false; ConnectionBudget.sockets.release()
        return session
    }
    static func connect(_ address: SocketAddress, deadline: ContinuousClock.Instant) async throws -> ConnectionSocket {
        guard ContinuousClock.now < deadline else { throw NativeSSHError.timedOut }
        guard ConnectionBudget.sockets.acquire() else { throw NativeSSHError.busy }
#if os(Linux)
        let kind = Int32(SOCK_STREAM.rawValue)
#else
        let kind = SOCK_STREAM
#endif
        let fd = socket(address.family, kind, Int32(IPPROTO_TCP))
        guard fd >= 0 else { ConnectionBudget.sockets.release(); throw NativeSSHError.connection(errno) }
        let owner = ConnectionSocket(fd)
        do {
            guard fcntl(fd, F_SETFL, O_NONBLOCK) == 0, fcntl(fd, F_SETFD, FD_CLOEXEC) == 0 else {
                throw NativeSSHError.connection(errno)
            }
            var storage = sockaddr_storage()
            guard address.bytes.count <= MemoryLayout.size(ofValue: storage) else { throw NativeSSHError.invalidInput }
            withUnsafeMutableBytes(of: &storage) { $0.copyBytes(from: address.bytes) }
            let code = withUnsafePointer(to: &storage) {
                $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
#if canImport(Darwin)
                    Darwin.connect(fd, $0, socklen_t(address.bytes.count))
#else
                    Glibc.connect(fd, $0, socklen_t(address.bytes.count))
#endif
                }
            }
            if code != 0 {
                guard errno == EINPROGRESS || errno == EWOULDBLOCK || errno == EINTR else { throw NativeSSHError.connection(errno) }
                let readiness = try SocketReadiness(descriptor: fd, directions: Int32(FTSSH_WRITE), deadline: deadline)
                try await readiness.wait()
                var error: Int32 = 0, size = socklen_t(MemoryLayout<Int32>.size)
                guard getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &size) == 0 else { throw NativeSSHError.connection(errno) }
                guard error == 0 else { throw NativeSSHError.connection(error) }
            }
            try Task.checkCancellation()
            guard ContinuousClock.now < deadline else { throw NativeSSHError.timedOut }
            return owner
        } catch { owner.close(); throw error }
    }
}

private func closeSocket(_ fd: Int32) {
#if canImport(Darwin)
    _ = Darwin.close(fd)
#else
    _ = Glibc.close(fd)
#endif
}
