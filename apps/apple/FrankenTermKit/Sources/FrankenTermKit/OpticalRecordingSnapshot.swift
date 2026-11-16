import Foundation
#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif

/// Native media decoders reopen and lazily read a URL. Pin a selected recording
/// to an app-owned, read-only copy before inspecting metadata or decoding it:
/// a provider replacing or changing the original must not change admitted bytes.
/// The temporary copy has a fixed byte ceiling, uses 64-KiB I/O, and is removed
/// after the native reader settles. It is not a saved scan or a durable backup.
struct OpticalRecordingSnapshot: Sendable {
    let url: URL
    private let directory: URL
    private let directoryIdentity: Identity
    private let fileIdentity: Identity

    private struct Identity: Sendable {
        let device: dev_t
        let inode: ino_t
        init(_ status: stat) { device = status.st_dev; inode = status.st_ino }
        func matches(_ status: stat) -> Bool { device == status.st_dev && inode == status.st_ino }
    }

    /// Injection keeps the actual descriptors, limits, ownership and cleanup.
    /// Native callers only replace the cancellation check to include a deadline.
    struct Operations {
        var read: (FileHandle, Int) throws -> Data?
        var write: (FileHandle, Data) throws -> Void
        var checkCancellation: () throws -> Void

        static var system: Self {
            .init(read: { try $0.read(upToCount: $1) },
                  write: { try $0.write(contentsOf: $1) },
                  checkCancellation: { try Task.checkCancellation() })
        }
    }

    static func capture(_ source: URL,
                        in parent: URL = FileManager.default.temporaryDirectory,
                        operations: Operations = .system) throws -> Self {
        guard OpticalRecordingLimits.accepts(source), parent.isFileURL,
              !source.path.utf8.contains(0), !parent.path.utf8.contains(0) else {
            throw FTError.malformed("recording.local_file")
        }
        try operations.checkCancellation()
        // Nonblocking open prevents a substituted FIFO from hanging before its
        // type is checked. Never follow a final-component symbolic link.
        let descriptor = source.path.withCString {
            open($0, O_RDONLY | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK)
        }
        guard descriptor >= 0 else { throw FTError.storage }
        let input = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
        defer { try? input.close() }
        var original = stat()
        guard fstat(descriptor, &original) == 0,
              original.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG),
              original.st_size > 0, original.st_size <= OpticalRecordingLimits.maximumBytes else {
            throw FTError.limit("recording.file_bytes")
        }

        let directory = parent.appendingPathComponent("ft-recording-input-" + UUID().uuidString, isDirectory: true)
        guard directory.path.withCString({ mkdir($0, 0o700) }) == 0 else { throw FTError.storage }
        var owned: Self?
        var admitted = false
        defer {
            if !admitted {
                if let owned { owned.discard() }
                else { _ = directory.path.withCString { rmdir($0) } }
            }
        }
        let directoryFD = directory.path.withCString {
            open($0, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW)
        }
        guard directoryFD >= 0 else { throw FTError.storage }
        defer { _ = close(directoryFD) }
        var directoryStatus = stat()
        guard fstat(directoryFD, &directoryStatus) == 0,
              directoryStatus.st_mode & mode_t(S_IFMT) == mode_t(S_IFDIR) else { throw FTError.storage }
        #if os(iOS)
        try FileManager.default.setAttributes([.protectionKey: FileProtectionType.complete],
                                              ofItemAtPath: directory.path)
        #endif
        #if canImport(Darwin)
        var protectedDirectory = directory
        var attributes = URLResourceValues(); attributes.isExcludedFromBackup = true
        try protectedDirectory.setResourceValues(attributes)
        #endif

        let name = "recording." + source.pathExtension.lowercased()
        let outputFD = name.withCString {
            openat(directoryFD, $0, O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC | O_NOFOLLOW, 0o600)
        }
        guard outputFD >= 0 else { throw FTError.storage }
        let output = FileHandle(fileDescriptor: outputFD, closeOnDealloc: true)
        defer { try? output.close() }
        var outputStatus = stat()
        guard fstat(outputFD, &outputStatus) == 0,
              outputStatus.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG) else { throw FTError.storage }
        let snapshot = Self(url: directory.appendingPathComponent(name), directory: directory,
                            directoryIdentity: Identity(directoryStatus), fileIdentity: Identity(outputStatus))
        owned = snapshot
        #if os(iOS)
        try FileManager.default.setAttributes([.protectionKey: FileProtectionType.complete],
                                              ofItemAtPath: snapshot.url.path)
        #endif

        var remaining = Int(original.st_size)
        while remaining > 0 {
            try operations.checkCancellation()
            let requested = min(65_536, remaining)
            guard let bytes = try operations.read(input, requested), !bytes.isEmpty,
                  bytes.count <= requested else { throw FTError.malformed("recording.file_changed") }
            try operations.write(output, bytes)
            remaining -= bytes.count
        }
        try operations.checkCancellation()
        // The size checked before opening a codec is the actual copied extent,
        // not a pathname's earlier metadata. Reject both growth and truncation.
        guard try operations.read(input, 1)?.isEmpty != false else {
            throw FTError.malformed("recording.file_changed")
        }
        var finalSource = stat(), finalOutput = stat()
        guard fstat(descriptor, &finalSource) == 0, sameSource(original, finalSource),
              fstat(outputFD, &finalOutput) == 0, snapshot.fileIdentity.matches(finalOutput),
              finalOutput.st_size == original.st_size else {
            throw FTError.malformed("recording.file_changed")
        }
        guard fchmod(outputFD, 0o400) == 0 else { throw FTError.storage }
        try output.close()
        try operations.checkCancellation()
        admitted = true
        return snapshot
    }

    private static func sameSource(_ before: stat, _ after: stat) -> Bool {
        guard before.st_dev == after.st_dev, before.st_ino == after.st_ino,
              before.st_size == after.st_size else { return false }
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

    /// Remove only the file and directory created by this capture. No recursive
    /// deletion: replacements and unrelated contents survive cleanup.
    func discard() {
        let descriptor = directory.path.withCString {
            open($0, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW)
        }
        guard descriptor >= 0 else { return }
        defer { _ = close(descriptor) }
        var currentDirectory = stat()
        guard fstat(descriptor, &currentDirectory) == 0,
              directoryIdentity.matches(currentDirectory) else { return }
        var currentFile = stat()
        url.lastPathComponent.withCString {
            if fstatat(descriptor, $0, &currentFile, AT_SYMLINK_NOFOLLOW) == 0,
               currentFile.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG),
               fileIdentity.matches(currentFile) {
                _ = unlinkat(descriptor, $0, 0)
            }
        }
        directory.path.withCString {
            var current = stat()
            if lstat($0, &current) == 0, directoryIdentity.matches(current) { _ = rmdir($0) }
        }
    }
}
