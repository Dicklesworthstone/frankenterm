import XCTest
import Foundation
import FrankenTermKit
import FrankenTermSSH
import FrankenTermSSHIntegration
@testable import FrankenTerm

/// Native picker/staging ownership, with real protected local files and a
/// recording file client. Transport interoperability is tested separately.
@MainActor
final class WorkspaceFilesScreenTests: XCTestCase {
    func testAnOldPickerResultCannotBorrowTheNextPickerTicket() async throws {
        let (controller, client) = try makeController()
        await controller.refresh()
        let native = WorkspaceFilesNativeState(controller: controller)
        native.beginImport(listingID: controller.listingID)
        let old = try XCTUnwrap(native.importRequest?.id)
        native.cancelImport(requestID: old)
        native.beginImport(listingID: controller.listingID)
        let current = try XCTUnwrap(native.importRequest?.id)

        // This nonexistent synthetic URL must never reach a provider read.
        native.receiveImport(.success([URL(fileURLWithPath: "/expired-picker-synthetic-file")]), requestID: old)
        XCTAssertEqual(native.importRequest?.id, current)
        XCTAssertFalse(native.hasWork)
        XCTAssertFalse(native.hasUpload)

        native.receiveImport(.failure(NSError(domain: NSCocoaErrorDomain, code: NSUserCancelledError)), requestID: current)
        XCTAssertNil(native.importRequest)
        XCTAssertTrue(native.canBrowse)
        let writes = await client.receivedUploads()
        XCTAssertTrue(writes.isEmpty)
        native.close(); await controller.close()
    }

    func testASelectedFileReturningWhileInactivePreparesAfterForegrounding() async throws {
        let source = try makeProviderFile(contents: Data("selected while temporarily inactive".utf8))
        defer { try? FileManager.default.removeItem(at: source.deletingLastPathComponent()) }
        let (controller, client) = try makeController()
        await controller.refresh()
        let native = WorkspaceFilesNativeState(controller: controller)
        native.beginImport(listingID: controller.listingID)
        let request = try XCTUnwrap(native.importRequest?.id)
        native.setActive(false)
        native.receiveImport(.success([source]), requestID: request)
        XCTAssertNil(native.importRequest, "A completed system picker cannot strand its old request.")
        XCTAssertFalse(native.hasWork)
        XCTAssertFalse(native.hasUpload)

        native.setActive(true)
        try await waitForWork(native)
        XCTAssertTrue(native.hasUpload, native.message ?? "The selected file should reach review.")
        XCTAssertEqual(controller.review?.direction, .upload)
        XCTAssertEqual(controller.review?.remotePath, "/remote/selected.txt")
        let writes = await client.receivedUploads()
        XCTAssertTrue(writes.isEmpty, "Selecting and reviewing a file cannot create it remotely.")
        native.discardReview()
        XCTAssertTrue(native.canBrowse)
        native.close(); await controller.close()
    }

    func testUploadReviewKeepsTheSelectedBytesWhenItsProviderFileChanges() async throws {
        let selected = Data("bytes reviewed for this upload".utf8)
        let replacement = Data("different bytes now at the provider URL".utf8)
        let source = try makeProviderFile(contents: selected)
        defer { try? FileManager.default.removeItem(at: source.deletingLastPathComponent()) }
        let (controller, client) = try makeController()
        await controller.refresh()
        let native = WorkspaceFilesNativeState(controller: controller)
        native.beginImport(listingID: controller.listingID)
        native.receiveImport(.success([source]), requestID: try XCTUnwrap(native.importRequest?.id))
        try await waitForWork(native)
        let originalReview = try XCTUnwrap(controller.review)
        try replacement.write(to: source, options: .atomic)

        native.editUploadName("reviewed-name.txt")
        let reviewed = try XCTUnwrap(controller.review)
        XCTAssertNotEqual(reviewed.id, originalReview.id)
        XCTAssertEqual(reviewed.remotePath, "/remote/reviewed-name.txt")
        native.confirm(originalReview.id)
        XCTAssertFalse(native.hasWork, "Editing the filename retires the old confirmation.")
        native.confirm(reviewed.id)
        try await waitForWork(native)
        let writes = await client.receivedUploads()
        XCTAssertEqual(writes, [selected])
        XCTAssertEqual(try Data(contentsOf: source), replacement, "The provider's file remains under its own ownership.")
        native.close(); await controller.close()
    }

    func testClosingBeforeQueuedConfirmationStartsCreatesNoRemoteFile() async throws {
        let source = try makeProviderFile(contents: Data("never admitted to remote upload".utf8))
        defer { try? FileManager.default.removeItem(at: source.deletingLastPathComponent()) }
        let (controller, client) = try makeController()
        await controller.refresh()
        let native = WorkspaceFilesNativeState(controller: controller)
        native.beginImport(listingID: controller.listingID)
        native.receiveImport(.success([source]), requestID: try XCTUnwrap(native.importRequest?.id))
        try await waitForWork(native)
        let review = try XCTUnwrap(controller.review)
        native.confirm(review.id)
        native.close() // Same main-actor turn, before the queued confirmation.
        try await waitForWork(native)
        await controller.close()
        let writes = await client.receivedUploads()
        XCTAssertTrue(writes.isEmpty)
        XCTAssertTrue(controller.isClosed)
        XCTAssertNil(controller.completion)
    }

    private func makeController() throws -> (WorkspaceFilesController, NativeFilesRecordingClient) {
        let client = NativeFilesRecordingClient()
        let scope = try TerminalSessionScope(workspaceID: UUID(), workspaceRevision: UUID(),
            machineID: "files-machine", routeID: "files-route", transportID: UUID(), channelID: UUID())
        return (WorkspaceFilesController(client: client, scope: scope, machineName: "Synthetic Files Machine",
            account: "files-account", host: "example.invalid", port: 22), client)
    }

    private func makeProviderFile(contents: Data) throws -> URL {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("native-files-provider-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false,
            attributes: [.posixPermissions: 0o700])
        let file = directory.appendingPathComponent("selected.txt")
        try contents.write(to: file)
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: file.path)
        return file
    }

    private func waitForWork(_ native: WorkspaceFilesNativeState) async throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while native.hasWork, ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertFalse(native.hasWork, "Bounded native provider work did not settle.")
    }
}

private actor NativeFilesRecordingClient: WorkspaceFilesClient {
    private enum ProbeFinished: Error { case recorded }
    private var uploads: [Data] = []
    func receivedUploads() -> [Data] { uploads }
    func listDirectory(path: String, limit: Int) async throws -> NativeSFTPDirectoryListing {
        .init(path: "/remote", entries: [], isTruncated: false)
    }
    func download(path: String, to destination: URL,
                  onProgress: @Sendable (UInt64, UInt64?) async -> Void) async throws -> WorkspaceFileTransferReceipt {
        throw ProbeFinished.recorded
    }
    func upload(from source: URL, to path: String,
                onProgress: @Sendable (UInt64, UInt64?) async -> Void) async throws -> WorkspaceFileTransferReceipt {
        uploads.append(try Data(contentsOf: source))
        // End after recording the exact file the native review handed to the
        // real portable controller. No fabricated successful receipt is needed.
        throw ProbeFinished.recorded
    }
    func close() async {}
}
