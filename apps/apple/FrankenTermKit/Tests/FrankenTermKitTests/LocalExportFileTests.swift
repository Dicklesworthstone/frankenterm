import Foundation
import XCTest
#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif
@testable import FrankenTermKit

final class LocalExportFileTests: XCTestCase {
    private func withDestination(_ body: (URL, URL) throws -> Void) throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("ft-export-test-" + UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false,
                                                attributes: [.posixPermissions: 0o700])
        defer { try? FileManager.default.removeItem(at: directory) }
        try body(directory, directory.appendingPathComponent("workspace.ftairgap"))
    }

    private func assertEmpty(_ directory: URL, file: StaticString = #filePath, line: UInt = #line) throws {
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path), [], file: file, line: line)
    }

    func testShortWritesAndInterruptionsPublishExactBytesWithPrivateMode() throws {
        try withDestination { directory, destination in
            let bytes = Data((0..<131_151).map { UInt8(truncatingIfNeeded: $0) })
            var operations = LocalExportFile.Operations.system
            let write = operations.write, synchronize = operations.synchronize, publish = operations.publish
            var attempts = 0, fileSynchronized = false, published = false, directorySynchronized = false
            operations.write = { descriptor, buffer in
                attempts += 1
                XCTAssertFalse(FileManager.default.fileExists(atPath: destination.path))
                var status = stat()
                XCTAssertEqual(fstat(descriptor, &status), 0)
                XCTAssertEqual(status.st_mode & 0o077, 0, "private before the first payload byte")
                XCTAssertLessThanOrEqual(buffer.count, 65_536)
                if attempts == 1 || attempts == 3 { errno = EINTR; return -1 }
                return write(descriptor, UnsafeRawBufferPointer(rebasing: buffer.prefix(997)))
            }
            operations.synchronize = { descriptor, stage in
                switch stage {
                case .file:
                    XCTAssertFalse(published)
                    fileSynchronized = true
                case .directory:
                    XCTAssertTrue(published)
                    directorySynchronized = true
                }
                return synchronize(descriptor, stage)
            }
            operations.publish = { descriptor, temporary, name in
                XCTAssertTrue(fileSynchronized)
                let result = publish(descriptor, temporary, name)
                published = result == 0
                return result
            }
            try LocalExportFile.write(bytes, to: destination, operations: operations)
            XCTAssertGreaterThan(attempts, 3)
            XCTAssertTrue(directorySynchronized)
            XCTAssertEqual(try Data(contentsOf: destination), bytes)
            XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path), [destination.lastPathComponent])
        }
    }

    func testExistingFileAndDanglingSymlinkAreNeverReplaced() throws {
        try withDestination { directory, destination in
            let original = Data("an existing backup".utf8)
            try original.write(to: destination, options: .withoutOverwriting)
            XCTAssertThrowsError(try LocalExportFile.write(Data("new export".utf8), to: destination)) {
                XCTAssertEqual($0 as? LocalExportFileError, .destinationExists)
            }
            XCTAssertEqual(try Data(contentsOf: destination), original)

            let link = directory.appendingPathComponent("alias.ftairgap")
            try FileManager.default.createSymbolicLink(at: link, withDestinationURL: directory.appendingPathComponent("absent"))
            XCTAssertThrowsError(try LocalExportFile.write(Data("new export".utf8), to: link)) {
                XCTAssertEqual($0 as? LocalExportFileError, .destinationExists)
            }
            XCTAssertEqual(try FileManager.default.destinationOfSymbolicLink(atPath: link.path), directory.appendingPathComponent("absent").path)
        }
    }

    func testFileCreatedDuringSaveWinsExclusivePublication() throws {
        try withDestination { directory, destination in
            let original = Data("another exporter won".utf8)
            var operations = LocalExportFile.Operations.system
            let publish = operations.publish
            operations.publish = { descriptor, temporary, name in
                do { try original.write(to: destination, options: .withoutOverwriting) }
                catch { XCTFail("could not create the competing destination"); errno = EIO; return -1 }
                return publish(descriptor, temporary, name)
            }
            XCTAssertThrowsError(try LocalExportFile.write(Data("losing export".utf8), to: destination, operations: operations)) {
                XCTAssertEqual($0 as? LocalExportFileError, .destinationExists)
            }
            XCTAssertEqual(try Data(contentsOf: destination), original)
            XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path), [destination.lastPathComponent])
        }
    }

    func testDiskFullAfterPartialWriteLeavesNoCompletedBackup() throws {
        try withDestination { directory, destination in
            var operations = LocalExportFile.Operations.system
            let write = operations.write
            var attempts = 0
            operations.write = { descriptor, buffer in
                attempts += 1
                if attempts == 1 { return write(descriptor, UnsafeRawBufferPointer(rebasing: buffer.prefix(3))) }
                errno = ENOSPC
                return -1
            }
            XCTAssertThrowsError(try LocalExportFile.write(Data("incomplete export".utf8), to: destination, operations: operations)) {
                XCTAssertEqual($0 as? LocalExportFileError, .writeFailed)
            }
            XCTAssertEqual(attempts, 2)
            try assertEmpty(directory)
        }
    }

    func testZeroProgressAndRepeatedInterruptionsAreBounded() throws {
        for interrupted in [false, true] {
            try withDestination { directory, destination in
                var operations = LocalExportFile.Operations.system
                var attempts = 0
                operations.write = { _, _ in
                    attempts += 1
                    errno = interrupted ? EINTR : 0
                    return interrupted ? -1 : 0
                }
                XCTAssertThrowsError(try LocalExportFile.write(Data([1]), to: destination, operations: operations)) {
                    XCTAssertEqual($0 as? LocalExportFileError, .writeFailed)
                }
                XCTAssertEqual(attempts, interrupted ? 65 : 1)
                try assertEmpty(directory)
            }
        }
    }

    func testFileSyncAndPublicationFailuresNeverPublishPartialSuccess() throws {
        for failSync in [false, true] {
            try withDestination { directory, destination in
                var operations = LocalExportFile.Operations.system
                let synchronize = operations.synchronize
                operations.synchronize = { descriptor, stage in
                    if failSync { errno = EIO; return -1 }
                    return synchronize(descriptor, stage)
                }
                operations.publish = { _, _, _ in errno = EIO; return -1 }
                XCTAssertThrowsError(try LocalExportFile.write(Data("complete bytes".utf8), to: destination, operations: operations)) {
                    XCTAssertEqual($0 as? LocalExportFileError, failSync ? .writeFailed : .publicationFailed)
                }
                try assertEmpty(directory)
            }
        }
    }

    func testDirectorySyncFailureReportsPublishedFileAndPreservesItsBytes() throws {
        try withDestination { directory, destination in
            let bytes = Data("a complete saved export".utf8)
            var operations = LocalExportFile.Operations.system
            let synchronize = operations.synchronize
            operations.synchronize = { descriptor, stage in
                switch stage {
                case .file: return synchronize(descriptor, stage)
                case .directory: errno = EIO; return -1
                }
            }
            XCTAssertThrowsError(try LocalExportFile.write(bytes, to: destination, operations: operations)) {
                XCTAssertEqual($0 as? LocalExportFileError, .publishedDurabilityUnconfirmed)
            }
            XCTAssertEqual(try Data(contentsOf: destination), bytes)
            XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path), [destination.lastPathComponent])
        }
    }

    func testCancellationBeforePublicationLeavesNoBackupButCannotUndoPublication() throws {
        for cancelAfterPublication in [false, true] {
            try withDestination { directory, destination in
                let bytes = Data("a complete saved export".utf8)
                var operations = LocalExportFile.Operations.system
                let synchronize = operations.synchronize, publish = operations.publish
                var cancelled = false
                operations.checkCancellation = { if cancelled { throw CancellationError() } }
                operations.synchronize = { descriptor, stage in
                    let result = synchronize(descriptor, stage)
                    if !cancelAfterPublication { cancelled = true }
                    return result
                }
                operations.publish = { descriptor, temporary, name in
                    let result = publish(descriptor, temporary, name)
                    cancelled = true
                    return result
                }
                if cancelAfterPublication {
                    try LocalExportFile.write(bytes, to: destination, operations: operations)
                    XCTAssertEqual(try Data(contentsOf: destination), bytes)
                } else {
                    XCTAssertThrowsError(try LocalExportFile.write(bytes, to: destination, operations: operations)) {
                        XCTAssertTrue($0 is CancellationError)
                    }
                    try assertEmpty(directory)
                }
            }
        }
    }

    func testPayloadAndDestinationBoundsRejectBeforeCreatingFiles() throws {
        try withDestination { directory, destination in
            for bytes in [Data(), Data(repeating: 0, count: FTLimit.envelope + 1)] {
                XCTAssertThrowsError(try LocalExportFile.write(bytes, to: destination)) {
                    XCTAssertEqual($0 as? LocalExportFileError, .invalidPayload)
                }
                try assertEmpty(directory)
            }
            XCTAssertThrowsError(try LocalExportFile.write(Data([1]), to: XCTUnwrap(URL(string: "https://example.invalid/workspace.ftairgap")))) {
                XCTAssertEqual($0 as? LocalExportFileError, .invalidDestination)
            }
        }
    }
}
