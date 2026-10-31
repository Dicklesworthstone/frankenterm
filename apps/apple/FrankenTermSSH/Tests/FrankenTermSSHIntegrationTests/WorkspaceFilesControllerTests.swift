import Foundation
import XCTest
import FrankenTermKit
import FrankenTermSSH
@testable import FrankenTermSSHIntegration

private actor FilesControllerGate {
    private var entered = false, released = false
    private var start: CheckedContinuation<Void, Never>?
    private var resume: CheckedContinuation<Void, Never>?
    func pause() async {
        entered = true; start?.resume(); start = nil
        if !released { await withCheckedContinuation { resume = $0 } }
    }
    func waitUntilEntered() async {
        if !entered { await withCheckedContinuation { start = $0 } }
    }
    func release() { released = true; resume?.resume(); resume = nil }
}

private struct FilesControllerProgress: Sendable {
    let bytes: UInt64, total: UInt64?
    init(_ bytes: UInt64, _ total: UInt64?) { self.bytes = bytes; self.total = total }
}

/// Deliberately finishes after cancellation, so tests exercise the controller's
/// owner/generation fence independently of the production client's checks.
private actor FilesControllerClient: WorkspaceFilesClient {
    private let listings: [NativeSFTPDirectoryListing]
    private let listingGate: FilesControllerGate?, transferGate: FilesControllerGate?
    private let bytes: Data, updates: [FilesControllerProgress]
    private let transferFailure: WorkspaceFileTransferFailure?
    private let receiptPath: String?, receiptDirection: WorkspaceFileTransferReceipt.Direction?, receiptBytes: UInt64?
    private(set) var requestedDirectories: [String] = []
    private(set) var downloads: [String] = []
    private(set) var uploads: [String] = []
    private(set) var uploadedData: Data?
    private(set) var closes = 0

    init(listings: [NativeSFTPDirectoryListing], bytes: Data = Data([1, 2, 3]),
         listingGate: FilesControllerGate? = nil, transferGate: FilesControllerGate? = nil,
         updates: [FilesControllerProgress] = [], transferFailure: WorkspaceFileTransferFailure? = nil,
         receiptPath: String? = nil, receiptDirection: WorkspaceFileTransferReceipt.Direction? = nil,
         receiptBytes: UInt64? = nil) {
        self.listings = listings; self.bytes = bytes; self.listingGate = listingGate; self.transferGate = transferGate
        self.updates = updates; self.transferFailure = transferFailure
        self.receiptPath = receiptPath; self.receiptDirection = receiptDirection; self.receiptBytes = receiptBytes
    }
    func listDirectory(path: String, limit: Int) async throws -> NativeSFTPDirectoryListing {
        let index = requestedDirectories.count; requestedDirectories.append(path)
        await listingGate?.pause()
        return listings[min(index, listings.count - 1)]
    }
    func download(path: String, to destination: URL,
                  onProgress: @Sendable (UInt64, UInt64?) async -> Void) async throws -> WorkspaceFileTransferReceipt {
        downloads.append(path)
        try bytes.write(to: destination, options: .withoutOverwriting)
        for update in updates { await onProgress(update.bytes, update.total) }
        await transferGate?.pause()
        await onProgress(UInt64(bytes.count), UInt64(bytes.count))
        if let transferFailure { throw transferFailure }
        return .init(direction: receiptDirection ?? .download, remotePath: receiptPath ?? path,
                     transferredBytes: receiptBytes ?? UInt64(bytes.count))
    }
    func upload(from source: URL, to path: String,
                onProgress: @Sendable (UInt64, UInt64?) async -> Void) async throws -> WorkspaceFileTransferReceipt {
        uploads.append(path)
        for update in updates { await onProgress(update.bytes, update.total) }
        await transferGate?.pause()
        let received = try Data(contentsOf: source); uploadedData = received
        await onProgress(UInt64(received.count), UInt64(received.count))
        if let transferFailure { throw transferFailure }
        return .init(direction: receiptDirection ?? .upload, remotePath: receiptPath ?? path,
                     transferredBytes: receiptBytes ?? UInt64(received.count))
    }
    func close() { closes += 1 }
}

private actor FilesControllerCloseCounter {
    private(set) var value = 0
    func increment() { value += 1 }
}

final class WorkspaceFilesControllerTests: XCTestCase, @unchecked Sendable {
    private func entry(_ name: String = "report.txt", size: UInt64? = 3,
                       kind: UInt32 = 0o100600) -> NativeSFTPDirectoryEntry {
        .init(name: name, attributes: .init(size: size, permissions: kind, modificationTime: 1))
    }
    private func directory(_ path: String = "/home/user", entries: [NativeSFTPDirectoryEntry]? = nil,
                           truncated: Bool = false) -> NativeSFTPDirectoryListing {
        .init(path: path, entries: entries ?? [entry()], isTruncated: truncated)
    }
    private func temporaryDirectory() throws -> URL {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("files-controller-tests-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false,
                                               attributes: [.posixPermissions: 0o700])
        return root
    }
    @MainActor
    private func controller(_ client: FilesControllerClient, root: URL,
                            counter: FilesControllerCloseCounter? = nil) throws -> WorkspaceFilesController {
        let scope = try TerminalSessionScope(workspaceID: UUID(), workspaceRevision: UUID(),
            machineID: "machine", routeID: "route", transportID: UUID(), channelID: UUID())
        return .init(client: client, scope: scope, machineName: "Build", account: "user", host: "host.invalid", port: 2222,
                     downloadRoot: root, onClose: { await counter?.increment() })
    }
    private func stagedFile(in root: URL, bytes: Data = Data([1, 2, 3])) throws -> URL {
        let source = root.appendingPathComponent("staged-" + UUID().uuidString)
        try bytes.write(to: source, options: .withoutOverwriting)
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: source.path)
        return source
    }

    @MainActor
    func testDownloadNeedsExactReviewBeforeTransferAndPublishesOnlyMatchingReceipt() async throws {
        let root = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let client = FilesControllerClient(listings: [directory()])
        let files = try controller(client, root: root)
        await files.refresh()
        let review = try files.prepareDownload(entryID: entry().id, listingID: files.listingID)
        XCTAssertEqual(review.scope, files.scope)
        XCTAssertEqual(review.remotePath, "/home/user/report.txt")
        XCTAssertEqual(review.expectedBytes, 3)
        XCTAssertNil(files.completion)
        let before = await client.downloads
        XCTAssertTrue(before.isEmpty)
        await files.confirm(reviewID: review.id)
        let completed = try XCTUnwrap(files.completion)
        XCTAssertEqual(try Data(contentsOf: XCTUnwrap(completed.downloadURL)), Data([1, 2, 3]))
        XCTAssertEqual(completed.receipt.transferredBytes, 3)
        XCTAssertFalse(files.isBusy); XCTAssertFalse(files.isClosed)
        await files.confirm(reviewID: review.id)
        let after = await client.downloads
        XCTAssertEqual(after, ["/home/user/report.txt"])
        await files.close()
    }

    @MainActor
    func testUnicodeByteDistinctNamesAndStaleDirectoryCallbacksNeverRebind() async throws {
        let root = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let composed = entry("caf\u{e9}"), decomposed = entry("cafe\u{301}")
        let child = entry("folder", size: nil, kind: 0o040700)
        let client = FilesControllerClient(listings: [directory(entries: [composed, child, decomposed]), directory("/other")])
        let files = try controller(client, root: root)
        await files.refresh()
        XCTAssertEqual(files.listing?.entries.first?.name, "folder")
        XCTAssertEqual(Set(files.listing?.entries.map(\.id) ?? []).count, 3)
        let oldListing = files.listingID
        let oldReview = try files.prepareDownload(entryID: decomposed.id, listingID: oldListing)
        XCTAssertTrue(oldReview.remotePath.utf8.elementsEqual("/home/user/cafe\u{301}".utf8))
        await files.openPath("/other")
        await files.confirm(reviewID: oldReview.id)
        XCTAssertThrowsError(try files.prepareDownload(entryID: entry().id, listingID: oldListing)) {
            XCTAssertEqual($0 as? WorkspaceFilesControllerError, .staleReview)
        }
        await files.openDirectory(entryID: child.id, listingID: oldListing)
        await files.goUp(listingID: oldListing)
        let transfers = await client.downloads, paths = await client.requestedDirectories
        XCTAssertTrue(transfers.isEmpty); XCTAssertEqual(paths, [".", "/other"])
        await files.close()
    }

    @MainActor
    func testMalformedOrUnboundedListingsCloseWithoutPublishing() async throws {
        let root = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let invalid = [directory("relative"), directory("/a/../b"), directory(entries: [entry("../x")]),
                       directory(entries: [entry(), entry()]),
                       directory(entries: (0..<513).map { entry("file-\($0)") })]
        for listing in invalid {
            let client = FilesControllerClient(listings: [listing])
            let files = try controller(client, root: root)
            await files.refresh()
            XCTAssertTrue(files.isClosed); XCTAssertNil(files.listing); XCTAssertNotNil(files.failure)
        }
        let client = FilesControllerClient(listings: [directory(truncated: true)])
        let files = try controller(client, root: root)
        await files.refresh()
        XCTAssertEqual(files.listing?.isTruncated, true)
        await files.close()
    }

    @MainActor
    func testWrongReceiptCannotExportUnicodeAliasDirectionOrUnboundedBytes() async throws {
        let root = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let listed = directory(entries: [entry("caf\u{e9}")])
        let clients = [
            FilesControllerClient(listings: [listed], receiptPath: "/home/user/cafe\u{301}"),
            FilesControllerClient(listings: [listed], receiptDirection: .upload),
            FilesControllerClient(listings: [listed], receiptBytes: UInt64.max),
            FilesControllerClient(listings: [listed], receiptBytes: 2)]
        for client in clients {
            let files = try controller(client, root: root)
            await files.refresh()
            let id = try files.prepareDownload(entryID: entry("caf\u{e9}").id, listingID: files.listingID).id
            await files.confirm(reviewID: id)
            XCTAssertTrue(files.isClosed); XCTAssertNil(files.completion)
            XCTAssertNotNil(files.failure)
        }
    }

    @MainActor
    func testUploadLeaseFreezesBytesAcrossPriorPathReplacementAndConfirmationSuspension() async throws {
        let root = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let original = try stagedFile(in: root)
        let source = try WorkspaceFilesUploadSource(takingOwnershipOfStagedFile: original, name: "local.txt", storageRoot: root)
        XCTAssertFalse(FileManager.default.fileExists(atPath: original.path))
        let gate = FilesControllerGate()
        let client = FilesControllerClient(listings: [directory()], transferGate: gate)
        let files = try controller(client, root: root)
        await files.refresh()
        let review = try files.prepareUpload(source: source, remoteName: "new.txt", listingID: files.listingID)
        let upload = Task { await files.confirm(reviewID: review.id) }
        await gate.waitUntilEntered()
        // Simulates the previous provider/staging URL being replaced AFTER
        // confirmation but BEFORE the backend opens its input descriptor.
        try Data([9, 9, 9]).write(to: original)
        await gate.release(); await upload.value
        let sent = await client.uploadedData
        XCTAssertEqual(sent, Data([1, 2, 3])); XCTAssertEqual(source.byteCount, 3)
        XCTAssertEqual(files.completion?.receipt.remotePath, "/home/user/new.txt")
        XCTAssertNil(files.completion?.downloadURL)
        await files.close()
    }

    @MainActor
    func testUploadLeaseRejectsLinksPublicSourcesAndFailedMovePreservesCallerFile() throws {
        let root = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let original = try stagedFile(in: root)
        let symbolic = root.appendingPathComponent("symlink"), hard = root.appendingPathComponent("hardlink")
        try FileManager.default.createSymbolicLink(at: symbolic, withDestinationURL: original)
        XCTAssertThrowsError(try WorkspaceFilesUploadSource(takingOwnershipOfStagedFile: symbolic, name: "copy", storageRoot: root))
        try FileManager.default.linkItem(at: original, to: hard)
        XCTAssertThrowsError(try WorkspaceFilesUploadSource(takingOwnershipOfStagedFile: hard, name: "copy", storageRoot: root))
        let publicFile = try stagedFile(in: root)
        try FileManager.default.setAttributes([.posixPermissions: 0o644], ofItemAtPath: publicFile.path)
        XCTAssertThrowsError(try WorkspaceFilesUploadSource(takingOwnershipOfStagedFile: publicFile, name: "copy", storageRoot: root))
        let untouched = try stagedFile(in: root)
        XCTAssertThrowsError(try WorkspaceFilesUploadSource(takingOwnershipOfStagedFile: untouched, name: "copy",
            storageRoot: root.appendingPathComponent("missing-root")))
        XCTAssertEqual(try Data(contentsOf: untouched), Data([1, 2, 3]))
        XCTAssertEqual(try Data(contentsOf: original), Data([1, 2, 3]))
    }

    @MainActor
    func testUploadRejectsUnsafeExistingNamesAndStaleOrForeignReview() async throws {
        let root = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let source = try WorkspaceFilesUploadSource(takingOwnershipOfStagedFile: stagedFile(in: root), name: "local.txt", storageRoot: root)
        let client = FilesControllerClient(listings: [directory()]), otherClient = FilesControllerClient(listings: [directory()])
        let files = try controller(client, root: root), other = try controller(otherClient, root: root)
        await files.refresh(); await other.refresh()
        for name in ["", ".", "..", "nested/name", "bad\0name", String(repeating: "x", count: 4097), "report.txt"] {
            XCTAssertThrowsError(try files.prepareUpload(source: source, remoteName: name, listingID: files.listingID))
        }
        let review = try files.prepareUpload(source: source, remoteName: "new.txt", listingID: files.listingID)
        await other.confirm(reviewID: review.id)
        files.dismissReview()
        await files.confirm(reviewID: review.id)
        let first = await client.uploads, second = await otherClient.uploads
        XCTAssertTrue(first.isEmpty); XCTAssertTrue(second.isEmpty)
        await files.close(); await other.close()
    }

    @MainActor
    func testOneBusyTransferAndMonotonicBoundedProgress() async throws {
        let root = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let gate = FilesControllerGate(), payload = Data(repeating: 7, count: 10)
        let client = FilesControllerClient(listings: [directory(entries: [entry(size: 10)])], bytes: payload, transferGate: gate,
            updates: [.init(0, 10), .init(6, 10), .init(4, 10), .init(UInt64.max, nil), .init(8, UInt64.max), .init(9, 10)])
        let files = try controller(client, root: root)
        await files.refresh()
        let review = try files.prepareDownload(entryID: entry().id, listingID: files.listingID)
        let transfer = Task { await files.confirm(reviewID: review.id) }
        await gate.waitUntilEntered()
        XCTAssertEqual(files.progress?.transferredBytes, 9); XCTAssertEqual(files.progress?.totalBytes, 10)
        XCTAssertTrue(files.isBusy); XCTAssertNil(files.completion)
        await files.refresh(); await files.confirm(reviewID: review.id)
        XCTAssertThrowsError(try files.prepareDownload(entryID: entry().id, listingID: files.listingID))
        let paths = await client.requestedDirectories, attempts = await client.downloads
        XCTAssertEqual(paths, ["."]); XCTAssertEqual(attempts.count, 1)
        await gate.release(); await transfer.value
        XCTAssertEqual(files.progress?.fraction, 1); XCTAssertEqual(files.completion?.receipt.transferredBytes, 10)
        await files.close()
    }

    @MainActor
    func testCancellationRetiresLateListingAndClosesEachOwnerOnce() async throws {
        let root = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let gate = FilesControllerGate(), counter = FilesControllerCloseCounter()
        let client = FilesControllerClient(listings: [directory()], listingGate: gate)
        let files = try controller(client, root: root, counter: counter)
        let loading = Task { await files.refresh() }
        await gate.waitUntilEntered()
        await files.cancel(); await files.close()
        await gate.release(); await loading.value
        XCTAssertTrue(files.isClosed); XCTAssertNil(files.listing)
        XCTAssertEqual(files.failure?.wasCancelled, true)
        let closes = await client.closes, callbacks = await counter.value
        XCTAssertEqual(closes, 1); XCTAssertEqual(callbacks, 1)
    }

    @MainActor
    func testCancelledTransferCannotPublishLateReceiptAndPreservesUploadUncertainty() async throws {
        let root = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        for uploading in [false, true] {
            let gate = FilesControllerGate()
            let client = FilesControllerClient(listings: [directory()], transferGate: gate, updates: [.init(2, 3)])
            let files = try controller(client, root: root)
            await files.refresh()
            let id: UUID
            if uploading {
                let source = try WorkspaceFilesUploadSource(takingOwnershipOfStagedFile: stagedFile(in: root), name: "local.txt", storageRoot: root)
                id = try files.prepareUpload(source: source, remoteName: "new.txt", listingID: files.listingID).id
            } else {
                id = try files.prepareDownload(entryID: entry().id, listingID: files.listingID).id
            }
            let transfer = Task { await files.confirm(reviewID: id) }
            await gate.waitUntilEntered()
            await files.cancel()
            XCTAssertEqual(files.failure?.transferredBytes, 2)
            XCTAssertEqual(files.failure?.remoteFileMayRemain, uploading)
            await gate.release(); await transfer.value
            XCTAssertTrue(files.isClosed); XCTAssertNil(files.completion)
            XCTAssertEqual(files.failure?.transferredBytes, 2, "late progress cannot rewrite cancellation evidence")
            XCTAssertEqual(files.failure?.wasCancelled, true)
        }
    }

    @MainActor
    func testBackgroundCloseRetainsExactUncertainUploadBeforeAndAfterAcknowledgements() async throws {
        let root = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        for acknowledged: UInt64 in [0, 2] {
            let gate = FilesControllerGate()
            let client = FilesControllerClient(listings: [directory()], transferGate: gate,
                updates: [.init(acknowledged, 3)])
            let files = try controller(client, root: root)
            await files.refresh()
            let source = try WorkspaceFilesUploadSource(takingOwnershipOfStagedFile: stagedFile(in: root), name: "local.txt", storageRoot: root)
            let id = try files.prepareUpload(source: source, remoteName: "new.txt", listingID: files.listingID).id
            let transfer = Task { await files.confirm(reviewID: id) }
            await gate.waitUntilEntered()
            await files.close()
            XCTAssertEqual(files.failure?.remotePath, "/home/user/new.txt")
            XCTAssertEqual(files.failure?.transferredBytes, acknowledged)
            XCTAssertEqual(files.failure?.remoteFileMayRemain, true)
            await gate.release(); await transfer.value
            XCTAssertNil(files.completion)
            XCTAssertEqual(files.failure?.transferredBytes, acknowledged)
            XCTAssertEqual(files.failure?.remotePath, "/home/user/new.txt")
        }
    }

    @MainActor
    func testFailedDownloadNeverExportsPartialBytesAndReceiptLeaseOutlivesConnection() async throws {
        let root = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let failed = FilesControllerClient(listings: [directory()], transferFailure:
            .init(reason: .changedDuringTransfer, transferredBytes: 3, remoteFileMayRemain: false))
        let failedFiles = try controller(failed, root: root)
        await failedFiles.refresh()
        let failedID = try failedFiles.prepareDownload(entryID: entry().id, listingID: failedFiles.listingID).id
        await failedFiles.confirm(reviewID: failedID)
        XCTAssertNil(failedFiles.completion); XCTAssertTrue(failedFiles.isClosed)
        XCTAssertEqual(failedFiles.failure?.transferredBytes, 3)
        let client = FilesControllerClient(listings: [directory()])
        var files: WorkspaceFilesController? = try controller(client, root: root)
        await files?.refresh()
        let id = try XCTUnwrap(files).prepareDownload(entryID: entry().id, listingID: XCTUnwrap(files).listingID).id
        await files?.confirm(reviewID: id)
        var completed = files?.completion
        let url = try XCTUnwrap(completed?.downloadURL)
        await files?.close(); files = nil
        XCTAssertEqual(try Data(contentsOf: url), Data([1, 2, 3]), "share UI retains completed file after background closes SSH")
        completed = nil
        XCTAssertFalse(FileManager.default.fileExists(atPath: url.path), "final lease removes only its private completed staging file")
    }
}
