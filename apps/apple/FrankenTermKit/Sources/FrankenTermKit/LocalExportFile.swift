import Foundation
#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif

public enum LocalExportFileError: Error, Equatable, Sendable, LocalizedError {
    case invalidDestination, invalidPayload, destinationExists, writeFailed, publicationFailed
    case publishedDurabilityUnconfirmed

    public var errorDescription: String? {
        switch self {
        case .invalidDestination:
            "Choose a local destination and a new filename for this export."
        case .invalidPayload:
            "The export is empty or exceeds the supported workspace-file size."
        case .destinationExists:
            "A file already exists at this destination. Choose a new filename; the existing file was not replaced."
        case .writeFailed:
            "The export could not be written completely. No completed backup was published."
        case .publicationFailed:
            "The complete export could not be published at this destination. Choose a local folder that supports exclusive file publication."
        case .publishedDurabilityUnconfirmed:
            "The complete export was saved, but the filesystem could not confirm it would survive a sudden system failure. The file remains at the selected destination."
        }
    }
}

/// Publishes already-prepared public workspace or encrypted FTAG bytes under an
/// explicitly selected new filename. Partial writes never use the final name.
/// The caller owns security-scoped URL access for the duration of this call.
public enum LocalExportFile {
    public static func write(_ bytes: Data, to destination: URL) throws {
        try write(bytes, to: destination, operations: .system)
    }

    enum Synchronization { case file, directory }

    /// Fault injection retains the real file descriptors and publication path.
    /// No implementation can promote a partially written candidate to success.
    struct Operations {
        var write: (Int32, UnsafeRawBufferPointer) -> Int
        var synchronize: (Int32, Synchronization) -> Int32
        var publish: (Int32, String, String) -> Int32
        var checkCancellation: () throws -> Void

        static var system: Self {
            Self(write: { descriptor, buffer in
                #if canImport(Darwin)
                Darwin.write(descriptor, buffer.baseAddress, buffer.count)
                #else
                Glibc.write(descriptor, buffer.baseAddress, buffer.count)
                #endif
            }, synchronize: { descriptor, _ in fsync(descriptor) }, publish: { directory, candidate, destination in
                candidate.withCString { from in
                    destination.withCString { to in
                        #if canImport(Darwin)
                        // Same exclusive rename used by the native MP4 exporter,
                        // anchored to the directory opened for this operation.
                        Darwin.renameatx_np(directory, from, directory, to, UInt32(RENAME_EXCL))
                        #else
                        // POSIX hard-link publication is atomic and exclusive.
                        // Both names are in the same directory/filesystem; only
                        // the owned temporary link is removed afterwards.
                        Glibc.linkat(directory, from, directory, to, 0)
                        #endif
                    }
                }
            }, checkCancellation: { try Task.checkCancellation() })
        }
    }

    static func write(_ bytes: Data, to destination: URL, operations: Operations) throws {
        guard !bytes.isEmpty, bytes.count <= FTLimit.envelope else { throw LocalExportFileError.invalidPayload }
        let name = destination.lastPathComponent
        guard destination.isFileURL, !name.isEmpty, name != ".", name != "..",
              !destination.path.utf8.contains(0) else { throw LocalExportFileError.invalidDestination }
        try operations.checkCancellation()
        let directory = destination.deletingLastPathComponent().path.withCString {
            open($0, O_RDONLY | O_DIRECTORY | O_CLOEXEC)
        }
        guard directory >= 0 else { throw LocalExportFileError.invalidDestination }
        defer { _ = close(directory) }

        var existing = stat()
        let found = name.withCString { fstatat(directory, $0, &existing, AT_SYMLINK_NOFOLLOW) }
        guard found != 0 else { throw LocalExportFileError.destinationExists }
        guard errno == ENOENT else { throw LocalExportFileError.publicationFailed }

        let candidate = ".ft-export-" + UUID().uuidString + ".partial"
        var descriptor = candidate.withCString {
            openat(directory, $0, O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC | O_NOFOLLOW, 0o600)
        }
        guard descriptor >= 0 else { throw LocalExportFileError.writeFailed }
        var owned = stat()
        let identified = fstat(descriptor, &owned) == 0
        defer {
            if descriptor >= 0 { _ = close(descriptor) }
            if identified { removeOwnedCandidate(candidate, directory: directory, identity: owned) }
        }
        guard identified, owned.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG) else { throw LocalExportFileError.writeFailed }

        try bytes.withUnsafeBytes { buffer in
            var offset = 0, interruptions = 0
            while offset < buffer.count {
                try operations.checkCancellation()
                let amount = min(65_536, buffer.count - offset)
                let chunk = UnsafeRawBufferPointer(rebasing: buffer[offset..<offset + amount])
                let written = operations.write(descriptor, chunk)
                if written < 0, errno == EINTR, interruptions < 64 {
                    interruptions += 1
                    continue
                }
                guard written > 0, written <= amount else { throw LocalExportFileError.writeFailed }
                offset += written
            }
        }
        var completed = stat()
        guard fstat(descriptor, &completed) == 0, completed.st_size == bytes.count,
              completed.st_dev == owned.st_dev, completed.st_ino == owned.st_ino else {
            throw LocalExportFileError.writeFailed
        }
        guard synchronize(descriptor, stage: .file, operations: operations) else { throw LocalExportFileError.writeFailed }
        // Never retry close: on some platforms an interrupted close has already
        // released the descriptor, which another task could immediately reuse.
        let closing = descriptor; descriptor = -1
        guard close(closing) == 0 else { throw LocalExportFileError.writeFailed }
        try operations.checkCancellation()

        guard operations.publish(directory, candidate, name) == 0 else {
            if errno == EEXIST { throw LocalExportFileError.destinationExists }
            throw LocalExportFileError.publicationFailed
        }
        // Publication is the commit point. Cancellation after it cannot turn a
        // complete saved file into a failed/cancelled export or delete that file.
        removeOwnedCandidate(candidate, directory: directory, identity: owned)
        guard synchronize(directory, stage: .directory, operations: operations) else {
            throw LocalExportFileError.publishedDurabilityUnconfirmed
        }
    }

    private static func synchronize(_ descriptor: Int32, stage: Synchronization, operations: Operations) -> Bool {
        for _ in 0..<64 {
            if operations.synchronize(descriptor, stage) == 0 { return true }
            if errno != EINTR { return false }
        }
        return false
    }

    private static func removeOwnedCandidate(_ name: String, directory: Int32, identity: stat) {
        var current = stat()
        name.withCString {
            guard fstatat(directory, $0, &current, AT_SYMLINK_NOFOLLOW) == 0,
                  current.st_dev == identity.st_dev, current.st_ino == identity.st_ino,
                  current.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG) else { return }
            _ = unlinkat(directory, $0, 0)
        }
    }
}
