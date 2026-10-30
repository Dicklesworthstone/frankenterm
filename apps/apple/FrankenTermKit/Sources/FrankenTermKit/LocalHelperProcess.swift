#if os(macOS) || os(Linux)
import Foundation
#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

/// Failures of a local, explicitly selected helper. No partial stdout is ever
/// returned as a successful configuration snapshot.
public enum LocalHelperProcessError: Error, LocalizedError, Equatable {
    case invalidLimits
    case invalidInvocation
    case outputLimit
    case diagnosticLimit
    case timedOut
    case failed(status: Int32, diagnostic: String)
    case io(Int32)
    case terminationUnconfirmed

    public var errorDescription: String? {
        switch self {
        case .invalidLimits: return "The local helper's execution limits are invalid."
        case .invalidInvocation: return "The local helper's executable or arguments are invalid."
        case .outputLimit: return "The local helper's output exceeded the allowed size."
        case .diagnosticLimit: return "The local helper's diagnostics exceeded the allowed size."
        case .timedOut: return "The local helper did not finish within the allowed time."
        case let .failed(status, diagnostic):
            return diagnostic.isEmpty ? "The local helper failed (status \(status))."
                : "The local helper failed (status \(status)): \(diagnostic)"
        case let .io(code): return "The local helper could not complete (error \(code))."
        case .terminationUnconfirmed: return "The local helper's termination could not be confirmed."
        }
    }
}

/// Bounded capture for local machine/configuration discovery. Both pipes are
/// drained on the calling worker without reader tasks or delayed watchdogs.
/// The deadline includes waiting for EOF: an inherited pipe cannot keep an
/// already-exited helper's request alive indefinitely.
public enum LocalHelperProcess {
    /// Consume one explicitly requested desktop handoff. Only an inherited pipe
    /// is accepted: opening the app normally cannot read a terminal or a file.
    /// EOF is mandatory, and cancellation, timeout and overflow return no prefix.
    /// The caller owns and closes the descriptor after this operation.
    public static func readInheritedInput(from descriptor: Int32 = STDIN_FILENO,
                                         maximumBytes: Int = MachineCompanionSource.maximumBytes,
                                         timeout: TimeInterval = 15,
                                         isCancelled: () -> Bool = { Task<Never, Never>.isCancelled }) throws -> Data {
        guard (1...MachineCompanionSource.maximumBytes).contains(maximumBytes), timeout.isFinite,
              timeout > 0, timeout <= 30 else { throw LocalHelperProcessError.invalidLimits }
        if isCancelled() { throw CancellationError() }
        var metadata = stat()
        guard fstat(descriptor, &metadata) == 0,
              metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFIFO) else {
            throw LocalHelperProcessError.invalidInvocation
        }
        let flags = fcntl(descriptor, F_GETFL)
        guard flags >= 0, fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) == 0 else {
            throw LocalHelperProcessError.io(errno)
        }
        defer { _ = fcntl(descriptor, F_SETFL, flags) }
        let start = DispatchTime.now().uptimeNanoseconds
        let budget = UInt64(timeout * 1_000_000_000)
        var bytes = Data()
        while true {
            if isCancelled() { throw CancellationError() }
            guard DispatchTime.now().uptimeNanoseconds &- start < budget else {
                throw LocalHelperProcessError.timedOut
            }
            if try !drain(descriptor, into: &bytes, limit: maximumBytes, overflow: .outputLimit) {
                if isCancelled() { throw CancellationError() }
                return bytes
            }
            var event = pollfd(fd: descriptor, events: Int16(POLLIN), revents: 0)
            let ready = poll(&event, 1, 20)
            if ready < 0 && errno != EINTR { throw LocalHelperProcessError.io(errno) }
            if event.revents & Int16(POLLNVAL) != 0 { throw LocalHelperProcessError.io(EBADF) }
        }
    }

    public static func run(executable: URL, arguments: [String], maximumOutputBytes: Int,
                           maximumDiagnosticBytes: Int = 65_536, timeout: TimeInterval = 30,
                           isCancelled: () -> Bool = { Task<Never, Never>.isCancelled }) throws -> Data {
        guard (0...67_108_864).contains(maximumOutputBytes),
              (0...1_048_576).contains(maximumDiagnosticBytes), timeout.isFinite,
              timeout > 0, timeout <= 300 else { throw LocalHelperProcessError.invalidLimits }
        guard executable.isFileURL, !executable.path.utf8.contains(0),
              !arguments.contains(where: { $0.utf8.contains(0) }) else {
            throw LocalHelperProcessError.invalidInvocation
        }
        if isCancelled() { throw CancellationError() }

        let output = Pipe(), errors = Pipe()
        defer {
            try? output.fileHandleForReading.close(); try? output.fileHandleForWriting.close()
            try? errors.fileHandleForReading.close(); try? errors.fileHandleForWriting.close()
        }
        let outputFD = output.fileHandleForReading.fileDescriptor
        let errorFD = errors.fileHandleForReading.fileDescriptor
        try nonblocking(outputFD); try nonblocking(errorFD)
        let started = DispatchTime.now().uptimeNanoseconds
        let budget = UInt64(timeout * 1_000_000_000)
        let child = try spawn(executable: executable.path, arguments: arguments,
                              output: output, errors: errors)
        // Only the helper and its descendants may keep the writing ends open.
        try? output.fileHandleForWriting.close(); try? errors.fileHandleForWriting.close()
        var result = Data(), diagnostics = Data()
        var outputOpen = true, errorOpen = true
        do {
            while true {
                if isCancelled() { throw CancellationError() }
                let exitStatus = try child.pollExit()
                if !outputOpen && !errorOpen && exitStatus != nil { break }
                let elapsed = DispatchTime.now().uptimeNanoseconds &- started
                guard elapsed < budget else { throw LocalHelperProcessError.timedOut }
                // One fixed-size read per pipe per turn preserves fairness even
                // when one stream is continuously writable.
                if outputOpen {
                    outputOpen = try drain(outputFD, into: &result, limit: maximumOutputBytes,
                                           overflow: .outputLimit)
                }
                if errorOpen {
                    errorOpen = try drain(errorFD, into: &diagnostics, limit: maximumDiagnosticBytes,
                                          overflow: .diagnosticLimit)
                }
                if !outputOpen && !errorOpen && exitStatus != nil { break }
                var descriptors = [
                    pollfd(fd: outputOpen ? outputFD : -1, events: Int16(POLLIN), revents: 0),
                    pollfd(fd: errorOpen ? errorFD : -1, events: Int16(POLLIN), revents: 0)
                ]
                let remaining = budget - min(budget, DispatchTime.now().uptimeNanoseconds &- started)
                let wait = Int32(min(20, (remaining + 999_999) / 1_000_000))
                let ready = poll(&descriptors, nfds_t(descriptors.count), wait)
                if ready < 0 && errno != EINTR { throw LocalHelperProcessError.io(errno) }
                if descriptors.contains(where: { $0.revents & Int16(POLLNVAL) != 0 }) {
                    throw LocalHelperProcessError.io(EBADF)
                }
            }
            if isCancelled() { throw CancellationError() }
            guard child.exitStatus == 0 else {
                let detail = String(decoding: diagnostics.prefix(2048), as: UTF8.self)
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                throw LocalHelperProcessError.failed(status: child.exitStatus ?? -1, diagnostic: detail)
            }
            return result
        } catch {
            // This object owns only this child. Never signal a process group or
            // a descendant discovered by a system-wide process search.
            guard child.stop() else {
                child.reapWhenExited()
                throw LocalHelperProcessError.terminationUnconfirmed
            }
            throw error
        }
    }

    /// Own waitpid rather than Foundation Process's termination notification:
    /// the direct child's exit and a descendant's inherited pipes are separate
    /// events. Until we reap this PID, it cannot be recycled under our signals.
    private final class Child {
        let pid: pid_t
        private(set) var exitStatus: Int32?
        private var ownsPID = true

        init(pid: pid_t) { self.pid = pid }

        func pollExit() throws -> Int32? {
            if let exitStatus { return exitStatus }
            guard ownsPID else { throw LocalHelperProcessError.io(ECHILD) }
            var status: Int32 = 0
            let result = waitpid(pid, &status, WNOHANG)
            if result == pid {
                ownsPID = false
                let signal = status & 0x7f
                exitStatus = signal == 0 ? (status >> 8) & 0xff : 128 + signal
                return exitStatus
            }
            if result < 0 && errno != EINTR {
                let code = errno
                // Another reaper would remove our authority to signal this PID.
                if code == ECHILD { ownsPID = false }
                throw LocalHelperProcessError.io(code)
            }
            return nil
        }

        func stop() -> Bool {
            do {
                if try pollExit() != nil { return true }
                guard ownsPID else { return false }
                _ = kill(pid, SIGTERM)
                if try awaitExit(for: 100_000_000) { return true }
                guard ownsPID else { return false }
                _ = kill(pid, SIGKILL)
                return try awaitExit(for: 500_000_000)
            } catch { return false }
        }

        func reapWhenExited() {
            guard ownsPID else { return }
            ownsPID = false
            // A child stuck in the kernel may outlive even SIGKILL. Preserve
            // its sole reap owner after returning the bounded failure, without
            // blocking a worker or ever signalling this PID again.
            _ = ExitReaper(pid: pid)
        }

        private func awaitExit(for nanoseconds: UInt64) throws -> Bool {
            let started = DispatchTime.now().uptimeNanoseconds
            repeat {
                if try pollExit() != nil { return true }
                Thread.sleep(forTimeInterval: 0.005)
            } while DispatchTime.now().uptimeNanoseconds &- started < nanoseconds
            return try pollExit() != nil
        }
    }

    /// The timer retains this owner until waitpid consumes the exact child's
    /// exit (or confirms that ownership was already lost). Its immutable state
    /// is accessed only by the serial source handler after initialization.
    private final class ExitReaper: @unchecked Sendable {
        private let pid: pid_t
        private let timer: DispatchSourceTimer

        init(pid: pid_t) {
            self.pid = pid
            timer = DispatchSource.makeTimerSource(queue: .global(qos: .utility))
            timer.setEventHandler { [self] in
                var status: Int32 = 0
                let result = waitpid(pid, &status, WNOHANG)
                if result == pid || (result < 0 && errno == ECHILD) {
                    timer.setEventHandler(handler: nil)
                    timer.cancel()
                }
            }
            timer.schedule(deadline: .now(), repeating: .milliseconds(100), leeway: .milliseconds(20))
            timer.resume()
        }
    }

    private static func spawn(executable: String, arguments: [String], output: Pipe,
                              errors: Pipe) throws -> Child {
        #if canImport(Darwin)
        var actions: posix_spawn_file_actions_t?
        var attributes: posix_spawnattr_t?
        #else
        var actions = posix_spawn_file_actions_t()
        var attributes = posix_spawnattr_t()
        #endif
        func checked(_ code: Int32) throws {
            guard code == 0 else { throw LocalHelperProcessError.io(code) }
        }
        try checked(posix_spawn_file_actions_init(&actions))
        defer { posix_spawn_file_actions_destroy(&actions) }
        try checked(posix_spawnattr_init(&attributes))
        defer { posix_spawnattr_destroy(&attributes) }
        try checked(posix_spawn_file_actions_addopen(&actions, STDIN_FILENO, "/dev/null", O_RDONLY, 0))
        try checked(posix_spawn_file_actions_adddup2(&actions, output.fileHandleForWriting.fileDescriptor, STDOUT_FILENO))
        try checked(posix_spawn_file_actions_adddup2(&actions, errors.fileHandleForWriting.fileDescriptor, STDERR_FILENO))
        for descriptor in [output.fileHandleForReading.fileDescriptor, output.fileHandleForWriting.fileDescriptor,
                           errors.fileHandleForReading.fileDescriptor, errors.fileHandleForWriting.fileDescriptor]
            where descriptor > STDERR_FILENO {
            try checked(posix_spawn_file_actions_addclose(&actions, descriptor))
        }

        var mask = sigset_t(), defaults = sigset_t()
        sigemptyset(&mask); sigemptyset(&defaults)
        for signal in [SIGTERM, SIGINT, SIGPIPE] { sigaddset(&defaults, signal) }
        try checked(posix_spawnattr_setsigmask(&attributes, &mask))
        try checked(posix_spawnattr_setsigdefault(&attributes, &defaults))
        var flags = Int16(POSIX_SPAWN_SETSIGMASK | POSIX_SPAWN_SETSIGDEF)
        #if canImport(Darwin)
        flags |= Int16(POSIX_SPAWN_CLOEXEC_DEFAULT)
        #endif
        try checked(posix_spawnattr_setflags(&attributes, flags))

        var argv: [UnsafeMutablePointer<CChar>?] = []
        var environment: [UnsafeMutablePointer<CChar>?] = []
        defer {
            for pointer in argv { free(pointer) }
            for pointer in environment { free(pointer) }
        }
        for value in [executable] + arguments {
            guard let pointer = strdup(value) else { throw LocalHelperProcessError.io(ENOMEM) }
            argv.append(pointer)
        }
        for (key, value) in ProcessInfo.processInfo.environment {
            guard let pointer = strdup("\(key)=\(value)") else { throw LocalHelperProcessError.io(ENOMEM) }
            environment.append(pointer)
        }
        argv.append(nil); environment.append(nil)
        var pid: pid_t = 0
        let code = argv.withUnsafeMutableBufferPointer { argv in
            environment.withUnsafeMutableBufferPointer { environment in
                posix_spawn(&pid, executable, &actions, &attributes, argv.baseAddress!, environment.baseAddress!)
            }
        }
        try checked(code)
        return Child(pid: pid)
    }

    private static func nonblocking(_ descriptor: Int32) throws {
        let flags = fcntl(descriptor, F_GETFL)
        guard flags >= 0, fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) == 0 else {
            throw LocalHelperProcessError.io(errno)
        }
    }

    /// Returns false only at EOF. Allocation and retained bytes are bounded
    /// before appending, including the chunk that would cross a limit.
    private static func drain(_ descriptor: Int32, into data: inout Data, limit: Int,
                              overflow: LocalHelperProcessError) throws -> Bool {
        var bytes = [UInt8](repeating: 0, count: 16_384)
        let count = read(descriptor, &bytes, bytes.count)
        if count > 0 {
            guard count <= limit - data.count else { throw overflow }
            data.append(contentsOf: bytes.prefix(count))
            return true
        }
        if count == 0 { return false }
        if errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK { return true }
        throw LocalHelperProcessError.io(errno)
    }

}
#endif
