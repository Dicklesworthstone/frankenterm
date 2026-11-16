import Foundation
import Observation
import FrankenTermKit
import FrankenTermSSH
#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

/// The browser owns one independently authenticated file connection. The
/// concrete client rechecks current workspace, trust and foreground authority
/// before and after each operation; a listing never grants transfer authority.
public protocol WorkspaceFilesClient: Sendable {
    func listDirectory(path: String, limit: Int) async throws -> NativeSFTPDirectoryListing
    func download(path: String, to destination: URL,
                  onProgress: @Sendable (UInt64, UInt64?) async -> Void) async throws -> WorkspaceFileTransferReceipt
    func upload(from source: URL, to path: String,
                onProgress: @Sendable (UInt64, UInt64?) async -> Void) async throws -> WorkspaceFileTransferReceipt
    func close() async
}

extension WorkspaceSFTPClient: WorkspaceFilesClient {}

public enum WorkspaceFilesControllerError: Error, Sendable, Equatable, LocalizedError {
    case closed, busy, staleReview, invalidPath, unavailableEntry
    case invalidLocalFile, fileTooLarge, localStorageUnavailable, destinationExists

    public var errorDescription: String? {
        switch self {
        case .closed: "This file connection is closed. Open Files again to make a new connection."
        case .busy: "Finish or cancel the current file operation first."
        case .staleReview: "This file review is no longer current. Review the destination again."
        case .invalidPath: "Choose a valid bounded directory path or a single filename."
        case .unavailableEntry: "This directory entry is no longer available or is not a supported file."
        case .invalidLocalFile: "Select an accessible regular file. Links and special files cannot be uploaded."
        case .fileTooLarge: "This file exceeds the 256 MiB transfer limit."
        case .localStorageUnavailable: "A private temporary download could not be prepared."
        case .destinationExists: "A file already has that name in this directory. Choose a new name."
        }
    }
}

public enum WorkspaceFilesActivity: Sendable, Equatable { case listing, upload, download }

public struct WorkspaceFilesProgress: Sendable, Equatable {
    public let transferredBytes: UInt64
    public let totalBytes: UInt64?
    public var fraction: Double? {
        guard let totalBytes, totalBytes > 0 else { return nil }
        return min(1, Double(transferredBytes) / Double(totalBytes))
    }
}

public struct WorkspaceFilesFailure: Sendable, Equatable {
    public let message: String
    public let transferredBytes: UInt64
    public let remoteFileMayRemain: Bool
    public let wasCancelled: Bool
    public let remotePath: String?

    fileprivate init(message: String, transferredBytes: UInt64, remoteFileMayRemain: Bool,
                     wasCancelled: Bool, remotePath: String? = nil) {
        self.message = message; self.transferredBytes = transferredBytes
        self.remoteFileMayRemain = remoteFileMayRemain; self.wasCancelled = wasCancelled
        self.remotePath = remotePath
    }
}

/// Takes ownership of a COMPLETED private staging copy, never a Files-provider
/// original. The native picker first coordinates and freezes its source into
/// such a copy. Moving that copy avoids a second full-file read and gives the
/// review a hidden, read-only path that provider changes cannot replace.
public final class WorkspaceFilesUploadSource: @unchecked Sendable {
    public let name: String
    public let byteCount: UInt64
    fileprivate let storage: WorkspaceFilesDownloadStorage
    private let identity: WorkspaceFilesLocalIdentity

    public init(takingOwnershipOfStagedFile source: URL, name: String,
                storageRoot: URL = FileManager.default.temporaryDirectory) throws {
        guard source.isFileURL, !source.path.utf8.contains(0), !name.isEmpty,
              name != ".", name != "..", name.utf8.count <= 255,
              !name.utf8.contains(0), !name.utf8.contains(47) else { throw WorkspaceFilesControllerError.invalidLocalFile }
        var parent = stat()
        let parentStatus = source.deletingLastPathComponent().withUnsafeFileSystemRepresentation { path -> Int32 in
            guard let path else { return -1 }
            return lstat(path, &parent)
        }
        guard parentStatus == 0, parent.st_mode & mode_t(S_IFMT) == mode_t(S_IFDIR),
              parent.st_uid == geteuid(), parent.st_mode & 0o077 == 0 else { throw WorkspaceFilesControllerError.invalidLocalFile }
        let descriptor = source.withUnsafeFileSystemRepresentation { path -> Int32 in
            guard let path else { return -1 }
            return open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK)
        }
        guard descriptor >= 0 else { throw WorkspaceFilesControllerError.invalidLocalFile }
        defer { _ = close(descriptor) }
        var before = stat()
        guard fstat(descriptor, &before) == 0, before.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG),
              before.st_uid == geteuid(), before.st_nlink == 1, before.st_mode & 0o077 == 0,
              before.st_size >= 0 else { throw WorkspaceFilesControllerError.invalidLocalFile }
        guard UInt64(before.st_size) <= WorkspaceSFTPClient.maximumFileBytes else { throw WorkspaceFilesControllerError.fileTooLarge }
        let storage = try WorkspaceFilesDownloadStorage(root: storageRoot, name: name)
        guard fchmod(descriptor, mode_t(0o400)) == 0 else { throw WorkspaceFilesControllerError.invalidLocalFile }
        let moved = source.withUnsafeFileSystemRepresentation { original -> Int32 in
            storage.file.withUnsafeFileSystemRepresentation { destination -> Int32 in
                guard let original, let destination else { return -1 }
                return rename(original, destination)
            }
        }
        guard moved == 0 else {
            // No copy fallback, replacement or removal: if volumes differ or
            // the move fails, the caller still owns its original staging file.
            _ = fchmod(descriptor, before.st_mode & 0o777)
            throw WorkspaceFilesControllerError.localStorageUnavailable
        }
        // No throwing work after ownership commits. A retained descriptor
        // supplies the post-rename witness without reopening an external path.
        var after = stat()
        if fstat(descriptor, &after) != 0 { after = before; after.st_mode = (before.st_mode & mode_t(S_IFMT)) | 0o400 }
        self.name = name; byteCount = UInt64(before.st_size)
        self.storage = storage; identity = .init(metadata: after)
    }

    fileprivate func validate() throws {
        guard try WorkspaceFilesLocalIdentity.read(storage.file) == identity else {
            throw WorkspaceFilesControllerError.invalidLocalFile
        }
    }
}

/// A review is bound to the exact connection, directory generation and remote
/// path. Its private file references cannot be used by the UI to publish a
/// partial download, or substituted when the confirmation button is pressed.
public struct WorkspaceFilesTransferReview: Identifiable, Sendable {
    public let id: UUID
    public let scope: TerminalSessionScope
    public let direction: WorkspaceFileTransferReceipt.Direction
    public let remotePath: String, localName: String
    public let expectedBytes: UInt64?
    fileprivate let generation: UUID
    fileprivate let source: WorkspaceFilesUploadSource?
    fileprivate let download: WorkspaceFilesDownloadStorage?
}

/// Retain this whole value while presenting a URL to a share/save UI. Its
/// private storage lease survives connection closure and removes only this
/// operation's temporary directory after its final owner releases it.
public struct WorkspaceFilesCompletion: Sendable {
    public let receipt: WorkspaceFileTransferReceipt
    public var downloadURL: URL? { download?.file }
    fileprivate let download: WorkspaceFilesDownloadStorage?
}

@MainActor @Observable
public final class WorkspaceFilesController {
    public let scope: TerminalSessionScope
    public let machineName: String, account: String, host: String
    public let port: UInt16
    public private(set) var listing: NativeSFTPDirectoryListing?
    public private(set) var listingID = UUID()
    public private(set) var review: WorkspaceFilesTransferReview?
    public private(set) var activity: WorkspaceFilesActivity?
    public private(set) var progress: WorkspaceFilesProgress?
    public private(set) var completion: WorkspaceFilesCompletion?
    public private(set) var failure: WorkspaceFilesFailure?
    public private(set) var isClosed = false
    public var isBusy: Bool { activity != nil }

    @ObservationIgnored private let client: any WorkspaceFilesClient
    @ObservationIgnored private let downloadRoot: URL
    @ObservationIgnored private let onClose: @Sendable () async -> Void
    @ObservationIgnored private var generation = UUID()
    @ObservationIgnored private var operation: Task<Void, Never>?
    @ObservationIgnored private var shutdown: Task<Void, Never>?
    @ObservationIgnored private var activeTransferPath: String?

    public init(client: any WorkspaceFilesClient, scope: TerminalSessionScope,
                machineName: String, account: String, host: String, port: UInt16,
                downloadRoot: URL = FileManager.default.temporaryDirectory,
                onClose: @escaping @Sendable () async -> Void = {}) {
        self.client = client; self.scope = scope; self.machineName = machineName
        self.account = account; self.host = host; self.port = port
        self.downloadRoot = downloadRoot; self.onClose = onClose
    }

    public func refresh() async { await openPath(listing?.path ?? ".") }

    /// The server resolves an explicit path. All subsequent actions use its
    /// validated absolute listing path, never a UI-built shell command.
    public func openPath(_ path: String) async {
        guard !isClosed, !isBusy else { return }
        guard Self.validPath(path), path == "." || path.hasPrefix("/") else {
            failure = Self.failure(WorkspaceFilesControllerError.invalidPath); return
        }
        let token = begin(.listing)
        let client = self.client
        let task = Task { [weak self] in
            guard self?.isCurrent(token) == true, !Task.isCancelled else { return }
            do {
                let received = try await client.listDirectory(path: path, limit: 512)
                try Task.checkCancellation()
                let checked = try Self.checkedListing(received)
                guard let self, self.isCurrent(token) else { return }
                self.listing = checked; self.listingID = UUID()
                self.finish(token)
            } catch {
                await self?.fail(error, token: token)
            }
        }
        operation = task
        await wait(for: task, token: token)
    }

    public func openDirectory(entryID: Data, listingID expected: UUID) async {
        guard !isClosed, !isBusy else { return }
        guard listingID == expected, let listing, let entry = listing.entries.first(where: { $0.id == entryID }),
              entry.attributes.kind == .directory,
              let path = Self.child(entry.name, in: listing.path) else {
            failure = Self.failure(WorkspaceFilesControllerError.unavailableEntry); return
        }
        await openPath(path)
    }

    public func goUp(listingID expected: UUID) async {
        guard listingID == expected, let path = listing?.path, path != "/", let separator = path.lastIndex(of: "/") else { return }
        let parent = separator == path.startIndex ? "/" : String(path[..<separator])
        await openPath(parent)
    }

    @discardableResult
    public func prepareDownload(entryID: Data, listingID expected: UUID) throws -> WorkspaceFilesTransferReview {
        try canPrepare()
        guard listingID == expected else { throw WorkspaceFilesControllerError.staleReview }
        guard let listing, let entry = listing.entries.first(where: { $0.id == entryID }),
              entry.attributes.kind == .regularFile,
              let path = Self.child(entry.name, in: listing.path) else {
            throw WorkspaceFilesControllerError.unavailableEntry
        }
        if let size = entry.attributes.size, size > WorkspaceSFTPClient.maximumFileBytes {
            throw WorkspaceFilesControllerError.fileTooLarge
        }
        let storage = try WorkspaceFilesDownloadStorage(root: downloadRoot, name: entry.name)
        let issued = WorkspaceFilesTransferReview(id: UUID(), scope: scope, direction: .download,
            remotePath: path, localName: entry.name, expectedBytes: entry.attributes.size,
            generation: generation, source: nil, download: storage)
        review = issued; failure = nil
        return issued
    }

    @discardableResult
    public func prepareUpload(source: WorkspaceFilesUploadSource, remoteName: String, listingID expected: UUID) throws -> WorkspaceFilesTransferReview {
        try canPrepare()
        guard listingID == expected else { throw WorkspaceFilesControllerError.staleReview }
        guard let listing, let path = Self.child(remoteName, in: listing.path) else {
            throw WorkspaceFilesControllerError.invalidPath
        }
        guard !listing.entries.contains(where: { $0.name.utf8.elementsEqual(remoteName.utf8) }) else {
            throw WorkspaceFilesControllerError.destinationExists
        }
        try source.validate()
        let issued = WorkspaceFilesTransferReview(id: UUID(), scope: scope, direction: .upload,
            remotePath: path, localName: source.name, expectedBytes: source.byteCount,
            generation: generation, source: source, download: nil)
        review = issued; failure = nil
        return issued
    }

    public func dismissReview() { if !isBusy { review = nil } }

    /// One explicit confirmation consumes one review. Refresh, navigation,
    /// cancellation and a different controller make an old review unusable.
    public func confirm(reviewID: UUID) async {
        guard !isClosed, !isBusy else { return }
        guard let selected = review, selected.id == reviewID,
              selected.generation == generation, selected.scope == scope else {
            failure = Self.failure(WorkspaceFilesControllerError.staleReview); return
        }
        if let source = selected.source {
            do {
                try source.validate()
            } catch {
                review = nil; failure = Self.failure(error); return
            }
        }
        let token = begin(selected.direction == .upload ? .upload : .download)
        activeTransferPath = selected.remotePath
        progress = .init(transferredBytes: 0, totalBytes: selected.expectedBytes)
        let client = self.client
        let task = Task { [weak self] in
            guard self?.isCurrent(token) == true, !Task.isCancelled else { return }
            do {
                let update: @Sendable (UInt64, UInt64?) async -> Void = { [weak self] bytes, total in
                    await self?.updateProgress(bytes, total: total, token: token)
                }
                let receipt: WorkspaceFileTransferReceipt
                switch selected.direction {
                case .download:
                    guard let storage = selected.download else { throw WorkspaceFilesControllerError.staleReview }
                    receipt = try await client.download(path: selected.remotePath, to: storage.file, onProgress: update)
                case .upload:
                    guard let source = selected.source else { throw WorkspaceFilesControllerError.staleReview }
                    try source.validate()
                    receipt = try await client.upload(from: source.storage.file, to: selected.remotePath, onProgress: update)
                }
                try Task.checkCancellation()
                guard let self, self.isCurrent(token) else { return }
                guard receipt.direction == selected.direction,
                      receipt.remotePath.utf8.elementsEqual(selected.remotePath.utf8),
                      receipt.transferredBytes <= WorkspaceSFTPClient.maximumFileBytes,
                      receipt.transferredBytes >= (self.progress?.transferredBytes ?? 0),
                      selected.expectedBytes.map({ $0 == receipt.transferredBytes }) ?? true else {
                    throw NativeSFTPError.malformedResponse
                }
                if let storage = selected.download {
                    let local = try WorkspaceFilesLocalIdentity.read(storage.file)
                    guard local.size == receipt.transferredBytes else { throw NativeSFTPError.malformedResponse }
                }
                self.progress = .init(transferredBytes: receipt.transferredBytes, totalBytes: receipt.transferredBytes)
                self.completion = .init(receipt: receipt, download: selected.download)
                self.finish(token)
            } catch {
                await self?.fail(error, token: token)
            }
        }
        operation = task
        await wait(for: task, token: token)
    }

    /// An interrupted SFTP operation cannot safely be reused. Retire its
    /// connection immediately; no stale callback may publish a receipt afterward.
    public func cancel() async {
        guard !isClosed else { await close(); return }
        let uploading = activity == .upload
        failure = .init(message: uploading
            ? "File transfer was cancelled. The remote path may contain an incomplete upload. Inspect it before choosing a new name."
            : "File operation was cancelled.",
            transferredBytes: progress?.transferredBytes ?? 0,
            remoteFileMayRemain: uploading, wasCancelled: true, remotePath: activeTransferPath)
        await close()
    }

    public func close() async {
        if let shutdown { await shutdown.value; return }
        if activity == .upload, completion == nil, failure == nil {
            failure = .init(message: "Upload stopped before confirmed completion. The remote path may contain a partial or complete file. Inspect it before choosing a new name.",
                transferredBytes: progress?.transferredBytes ?? 0, remoteFileMayRemain: true,
                wasCancelled: true, remotePath: activeTransferPath)
        }
        isClosed = true; generation = UUID()
        operation?.cancel(); operation = nil; review = nil; activity = nil
        let client = self.client, onClose = self.onClose
        let closing = Task { await client.close(); await onClose() }
        shutdown = closing
        await closing.value
    }

    private func canPrepare() throws {
        guard !isClosed else { throw WorkspaceFilesControllerError.closed }
        guard !isBusy, review == nil else { throw WorkspaceFilesControllerError.busy }
        guard listing != nil else { throw WorkspaceFilesControllerError.unavailableEntry }
    }

    private func begin(_ activity: WorkspaceFilesActivity) -> UUID {
        generation = UUID(); review = nil; failure = nil; completion = nil; progress = nil
        activeTransferPath = nil
        self.activity = activity
        return generation
    }

    private func isCurrent(_ token: UUID) -> Bool { !isClosed && token == generation }

    private func finish(_ token: UUID) {
        guard isCurrent(token) else { return }
        activity = nil; operation = nil; activeTransferPath = nil
    }

    private func wait(for task: Task<Void, Never>, token: UUID) async {
        await withTaskCancellationHandler {
            await task.value
        } onCancel: {
            task.cancel()
            Task { @MainActor [weak self] in
                guard let self, self.isCurrent(token) else { return }
                await self.cancel()
            }
        }
    }

    private func updateProgress(_ bytes: UInt64, total: UInt64?, token: UUID) {
        guard isCurrent(token), activity == .download || activity == .upload,
              bytes <= WorkspaceSFTPClient.maximumFileBytes,
              bytes >= (progress?.transferredBytes ?? 0),
              total.map({ $0 <= WorkspaceSFTPClient.maximumFileBytes && bytes <= $0 }) ?? true else { return }
        // A server cannot make the UI jump backwards or silently change a
        // known file size. The final receipt still has to match the review.
        if let known = progress?.totalBytes, let total, known != total { return }
        let boundedTotal = progress?.totalBytes ?? total
        guard boundedTotal.map({ bytes <= $0 }) ?? true else { return }
        progress = .init(transferredBytes: bytes, totalBytes: boundedTotal)
    }

    private func fail(_ error: any Error, token: UUID) async {
        guard isCurrent(token) else { return }
        failure = Self.failure(error, acknowledged: progress?.transferredBytes ?? 0,
                               uploading: activity == .upload, remotePath: activeTransferPath)
        await close()
    }

    private static func failure(_ error: any Error, acknowledged: UInt64 = 0,
                                uploading: Bool = false, remotePath: String? = nil) -> WorkspaceFilesFailure {
        if let transfer = error as? WorkspaceFileTransferFailure {
            return .init(message: transfer.localizedDescription,
                transferredBytes: min(WorkspaceSFTPClient.maximumFileBytes, max(acknowledged, transfer.transferredBytes)),
                remoteFileMayRemain: transfer.remoteFileMayRemain,
                wasCancelled: transfer.reason == .cancelled, remotePath: remotePath)
        }
        let message: String
        if let known = error as? WorkspaceFilesControllerError { message = known.localizedDescription }
        else if let known = error as? NativeSFTPError { message = known.localizedDescription }
        else if error is CancellationError { message = "File operation was cancelled." }
        else { message = "The file connection is unavailable or the server refused this operation." }
        return .init(message: message + (uploading
            ? " The remote path may contain an incomplete upload. Inspect it before choosing a new name." : ""),
            transferredBytes: min(acknowledged, WorkspaceSFTPClient.maximumFileBytes),
            remoteFileMayRemain: uploading, wasCancelled: error is CancellationError, remotePath: remotePath)
    }

    private static func validPath(_ path: String) -> Bool {
        !path.isEmpty && path.utf8.count <= 4_096 && !path.utf8.contains(0)
    }

    private static func child(_ name: String, in directory: String) -> String? {
        guard !name.isEmpty, name != ".", name != "..", !name.utf8.contains(0), !name.utf8.contains(47) else { return nil }
        let path = directory == "/" ? "/" + name : directory + "/" + name
        return validPath(path) ? path : nil
    }

    private static func checkedListing(_ listing: NativeSFTPDirectoryListing) throws -> NativeSFTPDirectoryListing {
        guard validPath(listing.path), listing.path.hasPrefix("/"), listing.entries.count <= 512,
              listing.path == "/" || listing.path.dropFirst().split(separator: "/", omittingEmptySubsequences: false)
                .allSatisfy({ !$0.isEmpty && $0 != "." && $0 != ".." }) else { throw NativeSFTPError.malformedResponse }
        var names: Set<Data> = [], cost = listing.path.utf8.count
        for entry in listing.entries {
            guard child(entry.name, in: listing.path) != nil, names.insert(entry.id).inserted else {
                throw NativeSFTPError.malformedResponse
            }
            let addition = entry.name.utf8.count + 64
            guard addition <= 1_048_576 - cost else { throw NativeSFTPError.invalidLimit }
            cost += addition
        }
        return .init(path: listing.path, entries: listing.entries.sorted {
            if ($0.attributes.kind == .directory) != ($1.attributes.kind == .directory) {
                return $0.attributes.kind == .directory
            }
            return $0.name.utf8.lexicographicallyPrecedes($1.name.utf8)
        }, isTruncated: listing.isTruncated)
    }
}

fileprivate struct WorkspaceFilesLocalIdentity: Sendable, Equatable {
    let size: UInt64
    private let device: UInt64, inode: UInt64, mode: UInt32
    private let modifiedSeconds: Int64, modifiedNanos: Int64, changedSeconds: Int64, changedNanos: Int64

    static func read(_ url: URL) throws -> Self {
        guard url.isFileURL, !url.path.utf8.contains(0) else { throw WorkspaceFilesControllerError.invalidLocalFile }
        var metadata = stat()
        let status = url.withUnsafeFileSystemRepresentation { path -> Int32 in
            guard let path else { return -1 }
            return lstat(path, &metadata)
        }
        guard status == 0, metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG), metadata.st_size >= 0 else {
            throw WorkspaceFilesControllerError.invalidLocalFile
        }
        guard UInt64(metadata.st_size) <= WorkspaceSFTPClient.maximumFileBytes else { throw WorkspaceFilesControllerError.fileTooLarge }
        return .init(metadata: metadata)
    }

    init(metadata: stat) {
        #if canImport(Darwin)
        let modified = metadata.st_mtimespec, changed = metadata.st_ctimespec
        #else
        let modified = metadata.st_mtim, changed = metadata.st_ctim
        #endif
        size = UInt64(metadata.st_size); device = UInt64(truncatingIfNeeded: metadata.st_dev); inode = UInt64(metadata.st_ino)
        mode = UInt32(metadata.st_mode); modifiedSeconds = Int64(modified.tv_sec); modifiedNanos = Int64(modified.tv_nsec)
        changedSeconds = Int64(changed.tv_sec); changedNanos = Int64(changed.tv_nsec)
    }
}

/// Only this randomly named, exclusively created private directory is removed.
/// A completed download retains this lease independently of the SSH lifecycle.
fileprivate final class WorkspaceFilesDownloadStorage: @unchecked Sendable {
    let directory: URL, file: URL

    init(root: URL, name: String) throws {
        guard root.isFileURL, !root.path.utf8.contains(0) else { throw WorkspaceFilesControllerError.localStorageUnavailable }
        directory = root.appendingPathComponent("frankenterm-download-" + UUID().uuidString, isDirectory: true)
        // A remote server may permit names beyond the local component limit.
        // Keep the exact remote name in review; its local safe name stays bounded.
        file = directory.appendingPathComponent(name.utf8.count <= 240 ? name : "download", isDirectory: false)
        let created = directory.withUnsafeFileSystemRepresentation { path -> Int32 in
            guard let path else { return -1 }
            return mkdir(path, mode_t(0o700))
        }
        guard created == 0 else { throw WorkspaceFilesControllerError.localStorageUnavailable }
        #if os(iOS)
        do {
            try FileManager.default.setAttributes([.protectionKey: FileProtectionType.complete], ofItemAtPath: directory.path)
            var mutable = directory, values = URLResourceValues()
            values.isExcludedFromBackup = true
            try mutable.setResourceValues(values)
        } catch {
            try? FileManager.default.removeItem(at: directory)
            throw WorkspaceFilesControllerError.localStorageUnavailable
        }
        #endif
    }

    deinit { try? FileManager.default.removeItem(at: directory) }
}
