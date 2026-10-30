#if os(macOS) || os(Linux)
import Foundation
import XCTest
#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif
@testable import FrankenTermKit

final class LocalHelperProcessTests: XCTestCase {
    private let shell = URL(fileURLWithPath: "/bin/sh")

    func testInheritedSourceRequiresCompleteBoundedPipeAndRestoresFlags() throws {
        let pipe = Pipe()
        defer { try? pipe.fileHandleForReading.close(); try? pipe.fileHandleForWriting.close() }
        let expected = Data("{\"snapshot\":\"complete\"}".utf8)
        try pipe.fileHandleForWriting.write(contentsOf: expected)
        try pipe.fileHandleForWriting.close()
        let descriptor = pipe.fileHandleForReading.fileDescriptor
        let flags = fcntl(descriptor, F_GETFL)
        XCTAssertEqual(try LocalHelperProcess.readInheritedInput(from: descriptor, maximumBytes: expected.count), expected)
        XCTAssertEqual(fcntl(descriptor, F_GETFL), flags)
    }

    func testInheritedSourceHandlesPressureLargerThanPipeCapacity() async throws {
        let pipe = Pipe()
        defer { try? pipe.fileHandleForReading.close(); try? pipe.fileHandleForWriting.close() }
        let expected = Data(repeating: 0x61, count: 512 * 1024)
        let writer = Task.detached {
            try pipe.fileHandleForWriting.write(contentsOf: expected)
            try pipe.fileHandleForWriting.close()
        }
        let actual = try LocalHelperProcess.readInheritedInput(from: pipe.fileHandleForReading.fileDescriptor,
            maximumBytes: expected.count, timeout: 3)
        try await writer.value
        XCTAssertEqual(actual, expected)
    }

    func testInheritedSourceRejectsOverflowTimeoutCancellationAndRegularFiles() throws {
        let pipe = Pipe()
        defer { try? pipe.fileHandleForReading.close(); try? pipe.fileHandleForWriting.close() }
        try pipe.fileHandleForWriting.write(contentsOf: Data("partial".utf8))
        XCTAssertThrowsError(try LocalHelperProcess.readInheritedInput(from: pipe.fileHandleForReading.fileDescriptor,
            maximumBytes: 3)) { XCTAssertEqual($0 as? LocalHelperProcessError, .outputLimit) }
        let flags = fcntl(pipe.fileHandleForReading.fileDescriptor, F_GETFL)
        XCTAssertThrowsError(try LocalHelperProcess.readInheritedInput(from: pipe.fileHandleForReading.fileDescriptor,
            maximumBytes: 100, timeout: 0.05)) { XCTAssertEqual($0 as? LocalHelperProcessError, .timedOut) }
        XCTAssertEqual(fcntl(pipe.fileHandleForReading.fileDescriptor, F_GETFL), flags)
        XCTAssertThrowsError(try LocalHelperProcess.readInheritedInput(from: pipe.fileHandleForReading.fileDescriptor,
            isCancelled: { true })) { XCTAssertTrue($0 is CancellationError) }
        let regular = try FileHandle(forReadingFrom: URL(fileURLWithPath: "/dev/null"))
        defer { try? regular.close() }
        XCTAssertThrowsError(try LocalHelperProcess.readInheritedInput(from: regular.fileDescriptor)) {
            XCTAssertEqual($0 as? LocalHelperProcessError, .invalidInvocation)
        }
    }

    func testCapturesBothPipesWithoutMixingDiagnosticsIntoTheDocument() throws {
        let bytes = try LocalHelperProcess.run(executable: shell,
            arguments: ["-c", "printf '{\"version\":1}'; printf 'diagnostic' >&2"], maximumOutputBytes: 13)
        XCTAssertEqual(String(decoding: bytes, as: UTF8.self), "{\"version\":1}")
    }

    func testOutputAndDiagnosticLimitsRejectPartialResults() throws {
        XCTAssertThrowsError(try LocalHelperProcess.run(executable: shell,
            arguments: ["-c", "printf abcd"], maximumOutputBytes: 3)) {
            XCTAssertEqual($0 as? LocalHelperProcessError, .outputLimit)
        }
        XCTAssertThrowsError(try LocalHelperProcess.run(executable: shell,
            arguments: ["-c", "printf abcd >&2"], maximumOutputBytes: 0, maximumDiagnosticBytes: 3)) {
            XCTAssertEqual($0 as? LocalHelperProcessError, .diagnosticLimit)
        }
        XCTAssertEqual(try LocalHelperProcess.run(executable: shell,
            arguments: ["-c", "exit 0"], maximumOutputBytes: 0), Data())
    }

    func testConcurrentPipePressureDoesNotDeadlock() throws {
        let chunk = String(repeating: "e", count: 4096)
        let script = "i=0; while [ $i -lt 32 ]; do printf '%s' '\(chunk)' >&2; i=$((i+1)); done; printf done"
        let bytes = try LocalHelperProcess.run(executable: shell, arguments: ["-c", script],
            maximumOutputBytes: 4, maximumDiagnosticBytes: 131_072, timeout: 3)
        XCTAssertEqual(bytes, Data("done".utf8))
    }

    func testNonzeroExitReportsBoundedDiagnosticsAndNeverReturnsStdout() throws {
        XCTAssertThrowsError(try LocalHelperProcess.run(executable: shell,
            arguments: ["-c", "printf 'incomplete document'; printf 'broken helper' >&2; exit 7"],
            maximumOutputBytes: 1024)) {
            XCTAssertEqual($0 as? LocalHelperProcessError, .failed(status: 7, diagnostic: "broken helper"))
        }
    }

    func testDeadlineStopsAHelperThatIgnoresTermination() throws {
        let start = Date()
        XCTAssertThrowsError(try LocalHelperProcess.run(executable: shell,
            arguments: ["-c", "trap '' TERM; while :; do :; done"], maximumOutputBytes: 1024, timeout: 0.15)) {
            XCTAssertEqual($0 as? LocalHelperProcessError, .timedOut)
        }
        XCTAssertLessThan(Date().timeIntervalSince(start), 3)
    }

    func testInheritedOpenPipesCannotHoldAnExitedHelperForever() throws {
        let start = Date()
        XCTAssertThrowsError(try LocalHelperProcess.run(executable: shell,
            arguments: ["-c", "/bin/sleep 1 & exit 0"], maximumOutputBytes: 1024, timeout: 0.15)) {
            XCTAssertEqual($0 as? LocalHelperProcessError, .timedOut)
        }
        XCTAssertLessThan(Date().timeIntervalSince(start), 3)
    }

    func testCancellationStopsTheOwnedChildAndPrecancelDoesNotLaunch() throws {
        var checkpoints = 0
        XCTAssertThrowsError(try LocalHelperProcess.run(executable: shell,
            arguments: ["-c", "while :; do :; done"], maximumOutputBytes: 1024, timeout: 3,
            isCancelled: { checkpoints += 1; return checkpoints > 3 })) {
            XCTAssertTrue($0 is CancellationError)
        }
        XCTAssertThrowsError(try LocalHelperProcess.run(executable: URL(fileURLWithPath: "/not/a/helper"),
            arguments: [], maximumOutputBytes: 1024, isCancelled: { true })) {
            XCTAssertTrue($0 is CancellationError)
        }
    }

    func testInvalidLimitsFailBeforeLaunch() throws {
        for timeout in [0, -1, Double.infinity, Double.nan, 301] {
            XCTAssertThrowsError(try LocalHelperProcess.run(executable: shell,
                arguments: ["-c", "exit 0"], maximumOutputBytes: 1024, timeout: timeout)) {
                XCTAssertEqual($0 as? LocalHelperProcessError, .invalidLimits)
            }
        }
    }

    func testSpawnFailureAndInvalidArgumentsFailWithoutWaitingForOutput() throws {
        XCTAssertThrowsError(try LocalHelperProcess.run(executable: URL(fileURLWithPath: "/not/a/helper"),
            arguments: [], maximumOutputBytes: 1024)) {
            guard case .io = $0 as? LocalHelperProcessError else {
                return XCTFail("Expected a spawn error, got \($0)")
            }
        }
        XCTAssertThrowsError(try LocalHelperProcess.run(executable: shell,
            arguments: ["-c", "exit 0\0ignored"], maximumOutputBytes: 1024)) {
            XCTAssertEqual($0 as? LocalHelperProcessError, .invalidInvocation)
        }
    }

    func testArgumentsRemainLiteralAndStandardInputIsClosed() throws {
        let argument = "quotes'\"; $HOME $(printf unexpected)\nsecond line"
        let bytes = try LocalHelperProcess.run(executable: shell,
            arguments: ["-c", "if read value; then exit 9; fi; printf '%s' \"$1\"", "helper", argument],
            maximumOutputBytes: 1024)
        XCTAssertEqual(bytes, Data(argument.utf8))
    }
}
#endif
