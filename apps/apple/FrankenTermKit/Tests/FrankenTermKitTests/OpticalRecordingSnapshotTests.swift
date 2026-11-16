import Foundation
import XCTest
#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif
@testable import FrankenTermKit

final class OpticalRecordingSnapshotTests: XCTestCase {
    private func withSource(_ bytes: Data, _ body: (URL, URL) throws -> Void) throws {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("ft-recording-snapshot-test-" + UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false,
                                                attributes: [.posixPermissions: 0o700])
        defer { try? FileManager.default.removeItem(at: root) }
        let staging = root.appendingPathComponent("staging", isDirectory: true)
        try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: false,
                                                attributes: [.posixPermissions: 0o700])
        let source = root.appendingPathComponent("selected.mp4")
        try bytes.write(to: source, options: .withoutOverwriting)
        try body(source, staging)
    }

    private func assertEmpty(_ directory: URL, file: StaticString = #filePath, line: UInt = #line) throws {
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path), [],
                       file: file, line: line)
    }

    func testShortReadsProduceExactPrivateReadOnlySnapshotWithBoundedIO() throws {
        let bytes = Data((0..<151_007).map { UInt8(truncatingIfNeeded: $0) })
        try withSource(bytes) { source, staging in
            var operations = OpticalRecordingSnapshot.Operations.system
            let read = operations.read, write = operations.write
            var reads = 0, writes = 0
            operations.read = { handle, requested in
                XCTAssertLessThanOrEqual(requested, 65_536)
                reads += 1
                return try read(handle, min(requested, 997))
            }
            operations.write = { handle, chunk in
                var status = stat()
                XCTAssertEqual(fstat(handle.fileDescriptor, &status), 0)
                XCTAssertEqual(status.st_mode & 0o077, 0, "private before any recording byte is written")
                XCTAssertLessThanOrEqual(chunk.count, 65_536)
                writes += 1
                try write(handle, chunk)
            }
            let snapshot = try OpticalRecordingSnapshot.capture(source, in: staging, operations: operations)
            defer { snapshot.discard() }
            XCTAssertGreaterThan(reads, 3); XCTAssertGreaterThan(writes, 3)
            XCTAssertEqual(try Data(contentsOf: snapshot.url), bytes)
            XCTAssertEqual(try Data(contentsOf: source), bytes)
            XCTAssertNotEqual(snapshot.url, source)
            var file = stat(), directory = stat()
            XCTAssertEqual(snapshot.url.path.withCString { lstat($0, &file) }, 0)
            XCTAssertEqual(snapshot.url.deletingLastPathComponent().path.withCString { lstat($0, &directory) }, 0)
            XCTAssertEqual(file.st_mode & 0o777, 0o400)
            XCTAssertEqual(directory.st_mode & 0o777, 0o700)
            snapshot.discard()
            try assertEmpty(staging)
            XCTAssertEqual(try Data(contentsOf: source), bytes)
        }
    }

    func testLaterProviderReplacementCannotChangeTheAdmittedRecording() throws {
        let bytes = Data(repeating: 71, count: 90_031)
        try withSource(bytes) { source, staging in
            let snapshot = try OpticalRecordingSnapshot.capture(source, in: staging)
            defer { snapshot.discard() }
            let replacement = Data("the provider now serves a different file".utf8)
            try replacement.write(to: source, options: .atomic)
            XCTAssertEqual(try Data(contentsOf: snapshot.url), bytes)
            snapshot.discard()
            XCTAssertEqual(try Data(contentsOf: source), replacement)
            try assertEmpty(staging)
        }
    }

    func testGrowthTruncationAndSameSizeRewriteDuringCopyAreRejectedAndCleaned() throws {
        for mutation in ["grow", "truncate", "rewrite"] {
            try withSource(Data(repeating: 3, count: 131_111)) { source, staging in
                // A fixed old timestamp makes same-size modification detection
                // independent of the test filesystem's timestamp resolution.
                try FileManager.default.setAttributes([.modificationDate: Date(timeIntervalSince1970: 10_000)],
                                                      ofItemAtPath: source.path)
                var operations = OpticalRecordingSnapshot.Operations.system
                let read = operations.read
                var changed = false
                operations.read = { handle, requested in
                    let bytes = try read(handle, requested)
                    if !changed {
                        changed = true
                        let writer = try FileHandle(forWritingTo: source)
                        defer { try? writer.close() }
                        switch mutation {
                        case "grow":
                            _ = try writer.seekToEnd()
                            try writer.write(contentsOf: Data([1]))
                        case "truncate":
                            try writer.truncate(atOffset: 73)
                        default:
                            try writer.write(contentsOf: Data([9]))
                            try FileManager.default.setAttributes([.modificationDate: Date(timeIntervalSince1970: 20_000)],
                                                                  ofItemAtPath: source.path)
                        }
                    }
                    return bytes
                }
                XCTAssertThrowsError(try OpticalRecordingSnapshot.capture(source, in: staging, operations: operations)) {
                    XCTAssertEqual($0 as? FTError, .malformed("recording.file_changed"))
                }
                XCTAssertTrue(changed)
                try assertEmpty(staging)
                XCTAssertTrue(FileManager.default.fileExists(atPath: source.path))
            }
        }
    }

    func testCancellationAndPartialWriteFailureLeaveNoRecordingCopy() throws {
        for cancelled in [true, false] {
            let bytes = Data(repeating: 5, count: 131_111)
            try withSource(bytes) { source, staging in
                var operations = OpticalRecordingSnapshot.Operations.system
                var checks = 0
                operations.checkCancellation = {
                    checks += 1
                    if cancelled && checks == 3 { throw CancellationError() }
                }
                if !cancelled {
                    operations.write = { handle, bytes in
                        try handle.write(contentsOf: Data(bytes.prefix(11)))
                        throw FTError.storage
                    }
                }
                XCTAssertThrowsError(try OpticalRecordingSnapshot.capture(source, in: staging, operations: operations)) {
                    if cancelled { XCTAssertTrue($0 is CancellationError) }
                    else { XCTAssertEqual($0 as? FTError, .storage) }
                }
                try assertEmpty(staging)
                XCTAssertEqual(try Data(contentsOf: source), bytes)
            }
        }
    }

    func testEmptyAndOversizedFilesAreRefusedBeforeAnyPayloadReadOrCopy() throws {
        for length in [0, OpticalRecordingLimits.maximumBytes + 1] {
            try withSource(Data([1])) { source, staging in
                let writer = try FileHandle(forWritingTo: source)
                try writer.truncate(atOffset: UInt64(length))
                try writer.close()
                var operations = OpticalRecordingSnapshot.Operations.system
                operations.read = { _, _ in XCTFail("invalid input must not be read"); return nil }
                XCTAssertThrowsError(try OpticalRecordingSnapshot.capture(source, in: staging, operations: operations)) {
                    XCTAssertEqual($0 as? FTError, .limit("recording.file_bytes"))
                }
                try assertEmpty(staging)
            }
        }
    }

    func testSymlinksDirectoriesAndFIFOsCannotEnterTheNativeDecoder() throws {
        try withSource(Data([1])) { source, staging in
            let root = source.deletingLastPathComponent()
            let symbolic = root.appendingPathComponent("linked.mp4")
            try FileManager.default.createSymbolicLink(at: symbolic, withDestinationURL: source)
            let directory = root.appendingPathComponent("directory.mp4", isDirectory: true)
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
            let fifo = root.appendingPathComponent("pipe.mp4")
            XCTAssertEqual(fifo.path.withCString { mkfifo($0, 0o600) }, 0)
            for candidate in [symbolic, directory, fifo] {
                XCTAssertThrowsError(try OpticalRecordingSnapshot.capture(candidate, in: staging))
                try assertEmpty(staging)
            }
        }
    }

    func testCleanupPreservesAReplacementAtTheTemporaryFilename() throws {
        try withSource(Data(repeating: 3, count: 71)) { source, staging in
            let snapshot = try OpticalRecordingSnapshot.capture(source, in: staging)
            let replacement = Data("unrelated replacement".utf8)
            try replacement.write(to: snapshot.url, options: .atomic)
            snapshot.discard()
            XCTAssertEqual(try Data(contentsOf: snapshot.url), replacement)
            XCTAssertEqual(try Data(contentsOf: source), Data(repeating: 3, count: 71))
        }
    }
}
