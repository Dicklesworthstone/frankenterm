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

    func testOwnedGroupIsCreatedAtSpawnAndDefaultGroupIsUnchanged() throws {
        let script = "printf '%s ' \"$$\"; /bin/ps -o pgid= -p \"$$\""
        let isolated = try LocalHelperProcess.run(executable: shell, arguments: ["-c", script],
            maximumOutputBytes: 128, timeout: 3, ownedProcessGroup: true)
        let isolatedIDs = String(decoding: isolated, as: UTF8.self)
            .split(whereSeparator: { $0.isWhitespace }).compactMap { Int32($0) }
        XCTAssertEqual(isolatedIDs.count, 2)
        guard isolatedIDs.count == 2 else { return }
        XCTAssertEqual(isolatedIDs[0], isolatedIDs[1])
        XCTAssertNotEqual(isolatedIDs[1], getpgrp())

        let ordinary = try LocalHelperProcess.run(executable: shell, arguments: ["-c", script],
            maximumOutputBytes: 128, timeout: 3)
        let ordinaryIDs = String(decoding: ordinary, as: UTF8.self)
            .split(whereSeparator: { $0.isWhitespace }).compactMap { Int32($0) }
        XCTAssertEqual(ordinaryIDs.count, 2)
        XCTAssertEqual(ordinaryIDs.last, getpgrp())
    }

    func testOwnedGroupSuccessRetiresClosedPipeGrandchildAndLeavesUnrelatedChildAlone() throws {
        let directory = try groupTestDirectory()
        let control = directory.appendingPathComponent("control-effect")
        let controlled = directory.appendingPathComponent("owned-effect")
        // The ready witness is written after TERM is ignored and both output
        // pipes are closed. Neither parent exit, EOF nor TERM alone can pass
        // this test; the final group KILL must prevent the delayed effect.
        let script = "(trap '' TERM; printf ready > \"$2\"; /bin/sleep 1; printf alive > \"$1\") >/dev/null 2>&1 & "
            + "while [ ! -f \"$2\" ]; do :; done; printf complete"
        let ordinary = try LocalHelperProcess.run(executable: shell,
            arguments: ["-c", script, "helper", control.path, directory.appendingPathComponent("control-ready").path],
            maximumOutputBytes: 8, timeout: 3)
        XCTAssertEqual(ordinary, Data("complete".utf8))
        let isolated = try LocalHelperProcess.run(executable: shell,
            arguments: ["-c", script, "helper", controlled.path, directory.appendingPathComponent("owned-ready").path],
            maximumOutputBytes: 8, timeout: 3, ownedProcessGroup: true)
        XCTAssertEqual(isolated, Data("complete".utf8))
        Thread.sleep(forTimeInterval: 1.2)
        XCTAssertEqual(try Data(contentsOf: control), Data("alive".utf8), "The unrelated default-group child must finish normally")
        XCTAssertFalse(FileManager.default.fileExists(atPath: controlled.path), "Closed pipes must not hide a surviving owned grandchild")
    }

    func testOwnedGroupDeadlineRetiresGrandchildAfterLeaderExit() throws {
        let directory = try groupTestDirectory()
        let effect = directory.appendingPathComponent("effect")
        let ready = directory.appendingPathComponent("ready")
        let script = "(trap '' TERM; printf ready > \"$2\"; /bin/sleep 1; printf alive > \"$1\") & "
            + "while [ ! -f \"$2\" ]; do :; done; exit 0"
        let started = Date()
        XCTAssertThrowsError(try LocalHelperProcess.run(executable: shell,
            arguments: ["-c", script, "helper", effect.path, ready.path],
            maximumOutputBytes: 32, timeout: 0.2, ownedProcessGroup: true)) {
            XCTAssertEqual($0 as? LocalHelperProcessError, .timedOut)
        }
        XCTAssertLessThan(Date().timeIntervalSince(started), 3)
        XCTAssertTrue(FileManager.default.fileExists(atPath: ready.path))
        Thread.sleep(forTimeInterval: 1.2)
        XCTAssertFalse(FileManager.default.fileExists(atPath: effect.path))
    }

    func testOwnedGroupCancellationOverflowAndFailureRetireGrandchildren() throws {
        let directory = try groupTestDirectory()
        var effects: [URL] = []
        for kind in ["cancel", "stdout", "stderr", "nonzero"] {
            let effect = directory.appendingPathComponent(kind + "-effect")
            let ready = directory.appendingPathComponent(kind + "-ready")
            effects.append(effect)
            let ending: String
            switch kind {
            case "cancel": ending = "wait"
            case "stdout": ending = "printf overflow"
            case "stderr": ending = "printf overflow >&2"
            default: ending = "exit 7"
            }
            let script = "(trap '' TERM; printf ready > \"$2\"; /bin/sleep 1; printf alive > \"$1\") >/dev/null 2>&1 & "
                + "while [ ! -f \"$2\" ]; do :; done; " + ending
            XCTAssertThrowsError(try LocalHelperProcess.run(executable: shell,
                arguments: ["-c", script, "helper", effect.path, ready.path],
                maximumOutputBytes: 0, maximumDiagnosticBytes: 0, timeout: 3, ownedProcessGroup: true,
                isCancelled: { kind == "cancel" && FileManager.default.fileExists(atPath: ready.path) })) {
                switch kind {
                case "cancel": XCTAssertTrue($0 is CancellationError)
                case "stdout": XCTAssertEqual($0 as? LocalHelperProcessError, .outputLimit)
                case "stderr": XCTAssertEqual($0 as? LocalHelperProcessError, .diagnosticLimit)
                default: XCTAssertEqual($0 as? LocalHelperProcessError, .failed(status: 7, diagnostic: ""))
                }
            }
            XCTAssertTrue(FileManager.default.fileExists(atPath: ready.path))
        }
        Thread.sleep(forTimeInterval: 1.2)
        for effect in effects { XCTAssertFalse(FileManager.default.fileExists(atPath: effect.path), effect.lastPathComponent) }
        XCTAssertThrowsError(try LocalHelperProcess.run(executable: URL(fileURLWithPath: "/not/a/helper"),
            arguments: [], maximumOutputBytes: 0, ownedProcessGroup: true, isCancelled: { true })) {
            XCTAssertTrue($0 is CancellationError)
        }
    }

    func testEnvironmentOverridesApplyOnlyToChildAndRejectInvalidEntries() throws {
        let previousShell = ProcessInfo.processInfo.environment["SHELL"]
        let previousValue = ProcessInfo.processInfo.environment["FT_HELPER_CHILD_ONLY"]
        let selected = "quotes'\"; $HOME $(printf forbidden)\nsecond line"
        let data = try LocalHelperProcess.run(executable: shell,
            arguments: ["-c", "printf '%s\\n%s' \"$SHELL\" \"$FT_HELPER_CHILD_ONLY\""],
            maximumOutputBytes: 1024, timeout: 3, ownedProcessGroup: true,
            environmentOverrides: ["SHELL": "/bin/sh", "FT_HELPER_CHILD_ONLY": selected])
        XCTAssertEqual(data, Data(("/bin/sh\n" + selected).utf8))
        XCTAssertEqual(ProcessInfo.processInfo.environment["SHELL"], previousShell)
        XCTAssertEqual(ProcessInfo.processInfo.environment["FT_HELPER_CHILD_ONLY"], previousValue)
        for overrides in [["": "x"], ["BAD=NAME": "x"], ["BAD\0NAME": "x"], ["GOOD": "bad\0value"]] {
            XCTAssertThrowsError(try LocalHelperProcess.run(executable: URL(fileURLWithPath: "/not/a/helper"),
                arguments: [], maximumOutputBytes: 0, environmentOverrides: overrides)) {
                XCTAssertEqual($0 as? LocalHelperProcessError, .invalidInvocation)
            }
        }
    }

    private func groupTestDirectory() throws -> URL {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("ft-helper-group-" + UUID().uuidString.lowercased(), isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false,
            attributes: [.posixPermissions: 0o700])
        return directory
    }
}
#endif
