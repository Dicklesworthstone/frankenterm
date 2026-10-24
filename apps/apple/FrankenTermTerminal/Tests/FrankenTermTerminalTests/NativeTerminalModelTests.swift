import Foundation
import XCTest
import FrankenTermKit
@testable import FrankenTermTerminal

// These tests link the real frankenterm-term native-ffi static library from
// Artifacts/CFrankenTermTerminal.xcframework. Every expectation below depends
// on the canonical Rust parser, encoder or frame capture; an injected model
// could not produce them.

private actor RecordingTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    init() throws {
        identity = try .init(workspaceID: "native-model-test", machineID: "machine", routeID: "direct",
                             routeRevision: 1, transportID: UUID(), channelID: UUID())
    }
    func writeSome(_ bytes: Data) -> TerminalTransportWrite { .accepted(bytes.count) }
    func close() {}
}

final class NativeTerminalModelTests: XCTestCase {
    private func makeModel(columns: UInt32 = 20, rows: UInt32 = 5) async throws -> (NativeTerminalModel, TerminalSource) {
        let coordinator = try TerminalInputCoordinator()
        let source = try await coordinator.attach(RecordingTransport())
        let model = try NativeTerminalModel(source: source, geometry: .init(columns: columns, rows: rows),
                                            scrollbackRows: 16)
        return (model, source)
    }

    private func frame(_ model: NativeTerminalModel, _ source: TerminalSource) throws -> TerminalRenderFrame {
        try TerminalRenderFrame.decode(model.captureFrame(viewportTop: nil), expectedSource: source)
    }

    /// The visible text of one row, placing each cell's grapheme at its column.
    private func text(_ row: TerminalRenderRow, columns: Int) -> String {
        var slots = Array(repeating: " ", count: columns)
        for cell in row.cells where cell.column < columns && !cell.text.isEmpty { slots[cell.column] = cell.text }
        var line = slots.joined()
        while line.hasSuffix(" ") { line.removeLast() }
        return line
    }

    func testParsedOutputBecomesAnOwnedFrameWithColorsAndCursor() async throws {
        let (model, source) = try await makeModel()
        defer { try? model.close() }
        try model.feed(Data("hello \u{1b}[31mred\u{1b}[0m\r\nline two".utf8))
        let decoded = try frame(model, source)
        XCTAssertEqual(decoded.source, source)
        XCTAssertEqual(decoded.columns, 20)
        XCTAssertEqual(decoded.rows, 5)
        XCTAssertEqual(decoded.lines.count, 5)
        XCTAssertEqual(text(decoded.lines[0], columns: 20), "hello red")
        XCTAssertEqual(text(decoded.lines[1], columns: 20), "line two")
        let red = try XCTUnwrap(decoded.lines[0].cells.first { $0.column == 6 })
        XCTAssertEqual(red.text, "r")
        XCTAssertEqual(red.foreground, .palette(1), "SGR 31 is palette red")
        let plain = try XCTUnwrap(decoded.lines[0].cells.first { $0.column == 0 })
        XCTAssertEqual(plain.foreground, .default)
        XCTAssertEqual(decoded.cursor.stableRow, decoded.lines[1].stableIndex)
        XCTAssertEqual(decoded.cursor.column, 8)
    }

    func testCursorReportIsAnExactReplySettledOnce() async throws {
        let (model, _) = try await makeModel()
        defer { try? model.close() }
        XCTAssertNil(try model.copyReply())
        try model.feed(Data("hello\u{1b}[6n".utf8))
        let reply = try XCTUnwrap(model.copyReply())
        XCTAssertEqual(String(decoding: reply.bytes, as: UTF8.self), "\u{1b}[1;6R",
                       "DSR 6 reports the real cursor: row 1, column 6 after five characters")
        try model.settleReply(sequence: reply.sequence, disposition: .accepted)
        XCTAssertNil(try model.copyReply(), "a settled reply is not offered again")
    }

    func testBasePaletteIsTheConfiguredThemeThatQueriesAndResetsSee() async throws {
        let (model, source) = try await makeModel()
        defer { try? model.close() }
        var theme = [UInt8](repeating: 9, count: 1052)
        for index in stride(from: 3, to: theme.count, by: 4) { theme[index] = 0xFF }
        theme.replaceSubrange(4..<8, with: [0xCC, 0x33, 0x33, 0xFF]) // ANSI red
        theme.replaceSubrange(1028..<1032, with: [0x14, 0x21, 0x3D, 0xFF]) // background
        XCTAssertThrowsError(try model.setBasePalette(Array(theme.dropLast())))
        try model.setBasePalette(theme)
        let themed = try frame(model, source)
        XCTAssertEqual(themed.palette[1], .init(0xCC, 0x33, 0x33, 0xFF))
        XCTAssertEqual(themed.palette[257], .init(0x14, 0x21, 0x3D, 0xFF))

        try model.feed(Data("\u{1b}]11;?\u{07}".utf8))
        let reply = try XCTUnwrap(model.copyReply())
        XCTAssertTrue(String(decoding: reply.bytes, as: UTF8.self).contains("rgb:1414/2121/3d3d"),
                      "a background query must report the machine theme")
        try model.settleReply(sequence: reply.sequence, disposition: .accepted)
        try model.feed(Data("\u{1b}]11;rgb:00/00/00\u{07}".utf8))
        XCTAssertEqual(try frame(model, source).palette[257], .init(0, 0, 0, 0xFF))
        try model.feed(Data("\u{1b}]111\u{07}".utf8))
        XCTAssertEqual(try frame(model, source).palette[257], .init(0x14, 0x21, 0x3D, 0xFF),
                       "resetting an OSC override returns to the theme")
    }

    func testKeyboardEncodingFollowsNegotiatedTerminalModes() async throws {
        let (model, _) = try await makeModel()
        defer { try? model.close() }
        let up = try TerminalHardwareKey(usage: 0x52)
        let enter = try TerminalHardwareKey(usage: 0x28)
        XCTAssertEqual(try model.encode(.text("é"), maximumBytes: 64), Data("é".utf8))
        XCTAssertEqual(try model.encode(.key(enter, .press), maximumBytes: 64), Data("\r".utf8))
        XCTAssertEqual(try model.encode(.key(up, .press), maximumBytes: 64), Data("\u{1b}[A".utf8))
        try model.feed(Data("\u{1b}[?1h".utf8)) // DECCKM: application cursor keys
        XCTAssertEqual(try model.encode(.key(up, .press), maximumBytes: 64), Data("\u{1b}OA".utf8))
    }

    func testPastePreparationFollowsBracketedPasteMode() async throws {
        let (model, _) = try await makeModel()
        defer { try? model.close() }
        XCTAssertFalse(try model.preparePaste("ls", maximumBytes: 1024).bracketed)
        try model.feed(Data("\u{1b}[?2004h".utf8))
        let prepared = try model.preparePaste("ls -la", maximumBytes: 1024)
        XCTAssertTrue(prepared.bracketed)
        XCTAssertEqual(prepared.text, "ls -la")
    }

    func testResizeChangesTheCapturedGeometry() async throws {
        let (model, source) = try await makeModel()
        defer { try? model.close() }
        let before = try frame(model, source)
        try model.resize(.init(columns: 30, rows: 6))
        let after = try frame(model, source)
        XCTAssertEqual(after.columns, 30)
        XCTAssertEqual(after.rows, 6)
        XCTAssertGreaterThan(after.geometryRevision, before.geometryRevision)
    }

    func testCloseIsIdempotentAndRetiresTheModel() async throws {
        let (model, _) = try await makeModel()
        try model.close()
        try model.close()
        XCTAssertThrowsError(try model.feed(Data("x".utf8))) { error in
            guard case TerminalModelError.closed = error else { return XCTFail("unexpected \(error)") }
        }
    }
}
