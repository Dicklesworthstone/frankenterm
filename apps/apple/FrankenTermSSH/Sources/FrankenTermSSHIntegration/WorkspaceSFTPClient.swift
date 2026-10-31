import Foundation
import FrankenTermSSH
#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

public struct WorkspaceFileTransferReceipt: Sendable, Equatable {
    public enum Direction: Sendable, Equatable { case download, upload }
    public let direction: Direction
    public let remotePath: String
    public let transferredBytes: UInt64

    public init(direction: Direction, remotePath: String, transferredBytes: UInt64) {
        self.direction = direction; self.remotePath = remotePath; self.transferredBytes = transferredBytes
    }
}

/// Counts only bytes received from the server or acknowledged by its SFTP
/// WRITE response. An incomplete upload is never removed, retried, or resumed.
/// Neither paths nor file contents are included in the localized error.
public struct WorkspaceFileTransferFailure: Error, Sendable, LocalizedError {
    public enum Reason: Sendable {
        case cancelled, unavailable, invalidLocalFile, unsupportedRemoteFile
        case limitExceeded, changedDuringTransfer, localIO, timedOut
    }
    public let reason: Reason
    public let transferredBytes: UInt64
    public let remoteFileMayRemain: Bool

    public var errorDescription: String? {
        let message: String
        switch reason {
        case .cancelled: message = "File transfer was cancelled."
        case .unavailable: message = "The file connection is unavailable or the server refused this operation."
        case .invalidLocalFile: message = "Select an accessible regular file and a new local destination."
        case .unsupportedRemoteFile: message = "Only regular files can be transferred."
        case .limitExceeded: message = "This file exceeds the 256 MiB transfer limit."
        case .changedDuringTransfer: message = "The file changed during transfer. Review it before trying again."
        case .localIO: message = "The local file could not be read or saved completely."
        case .timedOut: message = "File transfer timed out."
        }
        return message + (remoteFileMayRemain
            ? " The remote path may contain an incomplete upload. Inspect it before choosing a new name; this app will not overwrite or retry it."
            : "")
    }
}

/// One bounded operation on a separately admitted SSH file connection. Remote
/// data stays in 64-KiB chunks; completion is published only after the remote
/// handle closes and current route/foreground authority is revalidated.
public actor WorkspaceSFTPClient {
    public nonisolated static let maximumFileBytes: UInt64 = 256 * 1_024 * 1_024
    private static let chunkBytes = 65_536
    private let session: NativeSSHSession
    private let validate: @Sendable () async throws -> Void
    private let retire: @Sendable () async -> Void
    private var busy = false, closed = false

    init(session: NativeSSHSession, validate: @escaping @Sendable () async throws -> Void,
         retire: @escaping @Sendable () async -> Void) {
        self.session = session; self.validate = validate; self.retire = retire
    }

    public func listDirectory(path: String, limit: Int = 512) async throws -> NativeSFTPDirectoryListing {
        try Self.checkPath(path, absolute: false)
        guard (1...512).contains(limit) else { throw NativeSFTPError.invalidLimit }
        try begin()
        defer { busy = false }
        do {
            try await check()
            let listing = try await session.listSFTPDirectory(path: path, limit: limit, timeout: .seconds(15))
            try await check()
            return listing
        } catch {
            await close()
            throw error
        }
    }

    /// The destination must not exist. The caller owns this private local file
    /// and may export it only after receiving a successful receipt.
    public func download(path: String, to destination: URL,
                         onProgress: @Sendable (UInt64, UInt64?) async -> Void = { _, _ in }) async throws
        -> WorkspaceFileTransferReceipt {
        try Self.checkPath(path, absolute: true)
        try begin()
        defer { busy = false }
        var count: UInt64 = 0
        let deadline = ContinuousClock.now + .seconds(600)
        do {
            try await check()
            let local = try Self.openLocal(destination, writing: true)
            defer { try? local.handle.close() }
            let initial = try await session.openSFTPFile(path: path, mode: .read, timeout: try remaining(deadline))
            try await check()
            guard initial.kind == .regularFile else { throw Failure.unsupportedRemoteFile }
            guard initial.size.map({ $0 <= Self.maximumFileBytes }) ?? true else { throw Failure.limitExceeded }
            await onProgress(0, initial.size)
            while true {
                try await check()
                let bytes = try await session.readSFTPFile(maximumBytes: Self.chunkBytes, timeout: try remaining(deadline))
                try await check()
                if bytes.isEmpty { break }
                guard bytes.count <= Self.chunkBytes,
                      UInt64(bytes.count) <= Self.maximumFileBytes - count else { throw Failure.limitExceeded }
                do { try local.handle.write(contentsOf: bytes) }
                catch { throw Failure.localIO }
                count += UInt64(bytes.count)
                await onProgress(count, initial.size)
            }
            let final = try await session.statSFTPFile(timeout: try remaining(deadline))
            try await check()
            guard final.kind == .regularFile,
                  initial.size.map({ $0 == count }) ?? true,
                  final.size.map({ $0 == count }) ?? true,
                  initial.modificationTime == nil || final.modificationTime == nil
                    || initial.modificationTime == final.modificationTime else {
                throw Failure.changedDuringTransfer
            }
            try await session.closeSFTPFile(timeout: try remaining(deadline))
            try await check()
            guard fsync(local.handle.fileDescriptor) == 0 else { throw Failure.localIO }
            try local.handle.close()
            try await check()
            return .init(direction: .download, remotePath: path, transferredBytes: count)
        } catch {
            await close()
            throw failure(error, count: count, remoteFileMayRemain: false)
        }
    }

    /// The remote file is created atomically with EXCL|CREAT and restrictive
    /// permissions. No existing file is opened for writing and no rename,
    /// removal, shell command, or automatic retry is used on any path.
    public func upload(from source: URL, to path: String,
                       onProgress: @Sendable (UInt64, UInt64?) async -> Void = { _, _ in }) async throws
        -> WorkspaceFileTransferReceipt {
        try Self.checkPath(path, absolute: true)
        try begin()
        defer { busy = false }
        var count: UInt64 = 0, remoteMayRemain = false
        let deadline = ContinuousClock.now + .seconds(600)
        do {
            try await check()
            let local = try Self.openLocal(source, writing: false)
            defer { try? local.handle.close() }
            let expected = UInt64(local.metadata.st_size)
            guard expected <= Self.maximumFileBytes else { throw Failure.limitExceeded }
            try await check()
            // An interrupted OPEN may have created the file without returning
            // its handle. Retain that uncertainty even when zero bytes are ACKed.
            remoteMayRemain = true
            let remote = try await session.openSFTPFile(path: path, mode: .createExclusive,
                                                       timeout: try remaining(deadline))
            try await check()
            guard remote.kind == .regularFile else { throw Failure.unsupportedRemoteFile }
            await onProgress(0, expected)
            while true {
                try await check()
                let bytes: Data
                do { bytes = try local.handle.read(upToCount: Self.chunkBytes) ?? Data() }
                catch { throw Failure.localIO }
                if bytes.isEmpty { break }
                guard UInt64(bytes.count) <= expected - min(count, expected),
                      UInt64(bytes.count) <= Self.maximumFileBytes - count else {
                    throw Failure.changedDuringTransfer
                }
                var offset = 0
                while offset < bytes.count {
                    try await check()
                    let written = try await session.writeSFTPFile(Data(bytes[offset...]), timeout: try remaining(deadline))
                    guard written > 0, written <= bytes.count - offset else { throw Failure.unavailable }
                    count += UInt64(written); offset += written
                    try await check()
                    await onProgress(count, expected)
                }
            }
            var finalLocal = stat()
            guard fstat(local.handle.fileDescriptor, &finalLocal) == 0 else { throw Failure.localIO }
            guard count == expected, Self.sameFile(local.metadata, finalLocal) else {
                throw Failure.changedDuringTransfer
            }
            let finalRemote = try await session.statSFTPFile(timeout: try remaining(deadline))
            try await check()
            guard finalRemote.kind == .regularFile,
                  finalRemote.size.map({ $0 == count }) ?? true else { throw Failure.changedDuringTransfer }
            try await session.closeSFTPFile(timeout: try remaining(deadline))
            try await check()
            return .init(direction: .upload, remotePath: path, transferredBytes: count)
        } catch {
            await close()
            throw failure(error, count: count, remoteFileMayRemain: remoteMayRemain)
        }
    }

    /// Idempotently retires this file owner; an old client can never reopen it.
    public func close() async {
        closed = true
        await retire()
    }

    private func begin() throws {
        try Task.checkCancellation()
        guard !closed else { throw NativeSSHError.closed }
        guard !busy else { throw NativeSSHError.busy }
        busy = true
    }

    private func check() async throws {
        try Task.checkCancellation()
        guard !closed else { throw NativeSSHError.closed }
        try await validate()
        guard !closed else { throw NativeSSHError.closed }
        try Task.checkCancellation()
    }

    private func remaining(_ deadline: ContinuousClock.Instant) throws -> Duration {
        let duration = ContinuousClock.now.duration(to: deadline)
        guard duration > .zero else { throw Failure.timedOut }
        return min(duration, .seconds(15))
    }

    private typealias Failure = WorkspaceFileTransferFailure.Reason

    private func failure(_ error: any Error, count: UInt64, remoteFileMayRemain: Bool) -> WorkspaceFileTransferFailure {
        let reason: Failure
        if error is CancellationError || Task.isCancelled { reason = .cancelled }
        else if let known = error as? Failure { reason = known }
        else if error as? NativeSSHError == .timedOut { reason = .timedOut }
        else if error as? NativeSFTPError == .unsupportedFile { reason = .unsupportedRemoteFile }
        else { reason = .unavailable }
        return .init(reason: reason, transferredBytes: count, remoteFileMayRemain: remoteFileMayRemain)
    }

    private static func checkPath(_ path: String, absolute: Bool) throws {
        guard !path.isEmpty, path.utf8.count <= 4_096, !path.utf8.contains(0),
              !absolute || path.hasPrefix("/") else { throw NativeSFTPError.invalidPath }
    }

    private static func openLocal(_ url: URL, writing: Bool) throws -> (handle: FileHandle, metadata: stat) {
        guard url.isFileURL, !url.path.utf8.contains(0) else { throw Failure.invalidLocalFile }
        let flags = O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK
            | (writing ? O_WRONLY | O_CREAT | O_EXCL : O_RDONLY)
        let descriptor = url.withUnsafeFileSystemRepresentation { path -> Int32 in
            guard let path else { return -1 }
            return open(path, flags, mode_t(0o600))
        }
        guard descriptor >= 0 else { throw Failure.invalidLocalFile }
        let handle = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
        var metadata = stat()
        guard fstat(descriptor, &metadata) == 0,
              metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG), metadata.st_size >= 0 else {
            try? handle.close()
            throw Failure.invalidLocalFile
        }
        return (handle, metadata)
    }

    private static func sameFile(_ before: stat, _ after: stat) -> Bool {
        guard before.st_dev == after.st_dev, before.st_ino == after.st_ino,
              before.st_size == after.st_size, before.st_mode == after.st_mode else { return false }
#if canImport(Darwin)
        return before.st_mtimespec.tv_sec == after.st_mtimespec.tv_sec
            && before.st_mtimespec.tv_nsec == after.st_mtimespec.tv_nsec
            && before.st_ctimespec.tv_sec == after.st_ctimespec.tv_sec
            && before.st_ctimespec.tv_nsec == after.st_ctimespec.tv_nsec
#else
        return before.st_mtim.tv_sec == after.st_mtim.tv_sec
            && before.st_mtim.tv_nsec == after.st_mtim.tv_nsec
            && before.st_ctim.tv_sec == after.st_ctim.tv_sec
            && before.st_ctim.tv_nsec == after.st_ctim.tv_nsec
#endif
    }
}

extension WorkspaceFileTransferFailure.Reason: Error {}
