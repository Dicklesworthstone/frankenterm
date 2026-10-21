import Foundation
import XCTest
@testable import FrankenTermKit

private actor RenderFrameTestTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    init(identity: TerminalConnectionIdentity) { self.identity = identity }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite { .failed }
    func close() async {}
}

/// Independent FTSF consumer fixtures: these write the documented wire fields
/// directly, without calling the production decoder or a production encoder.
/// Mixed cell widths deliberately differ from UTF-8 and UTF-16 string lengths.
@MainActor
final class TerminalRenderFrameTests: XCTestCase {
    private static let channel = UUID(uuidString: "00112233-4455-6677-8899-AABBCCDDEEFF")!
    private static let transport = UUID(uuidString: "10213243-5465-7687-98A9-BACBDCEDFE0F")!
    private static let link = "https://example.test/端末"
    private static let combining = "e\u{301}"
    private static let emoji = "👩‍💻"

    private func source(routeRevision: UInt64 = 7, channel: UUID? = nil,
                        transport: UUID? = nil) async throws -> TerminalSource {
        let identity = try TerminalConnectionIdentity(
            workspaceID: "render-workspace", machineID: "render-machine", routeID: "render-route",
            routeRevision: routeRevision, transportID: transport ?? Self.transport, channelID: channel ?? Self.channel)
        let coordinator = try TerminalInputCoordinator()
        return try await coordinator.attach(RenderFrameTestTransport(identity: identity))
    }

    private struct Cell {
        var column: UInt32, width: UInt32, text: String, flags: UInt32
        var foreground: (UInt32, UInt32) = (0, 0)
        var background: (UInt32, UInt32) = (0, 0)
        var underline: (UInt32, UInt32) = (0, 0)
        var link: UInt32 = 0
    }

    private func put(_ value: UInt64, at: Int, bytes: Int, in data: inout Data) {
        for index in 0..<bytes { data[at + index] = UInt8(truncatingIfNeeded: value >> (index * 8)) }
    }
    private func u32(_ value: UInt32, at: Int, in data: inout Data) {
        put(UInt64(value), at: at, bytes: 4, in: &data)
    }
    private func u64(_ value: UInt64, at: Int, in data: inout Data) {
        put(value, at: at, bytes: 8, in: &data)
    }
    private func i64(_ value: Int64, at: Int, in data: inout Data) {
        u64(UInt64(bitPattern: value), at: at, in: &data)
    }
    private func read32(_ data: Data, at: Int) -> Int {
        (0..<4).reduce(0) { $0 | (Int(data[at + $1]) << ($1 * 8)) }
    }

    private func fixture(combining: String? = nil) -> Data {
        let cells = [
            Cell(column: 0, width: 1, text: "A", flags: 1,
                 foreground: (1, 1), underline: (2, 0xFFCCBBAA)),
            Cell(column: 1, width: 2, text: "界", flags: (3 << 2) | (1 << 7) | (1 << 8) | (1 << 9),
                 foreground: (2, 0x8038220C), background: (1, 200), link: 1),
            Cell(column: 3, width: 1, text: combining ?? Self.combining, flags: 2 | (1 << 10)),
            Cell(column: 0, width: 2, text: Self.emoji, flags: 2 << 15),
            Cell(column: 2, width: 1, text: "z", flags: (2 << 5) | (1 << 11) | (1 << 12))
        ]
        let rowsOffset = 256, cellsOffset = rowsOffset + 2 * 24
        let linksOffset = cellsOffset + cells.count * 48, textOffset = linksOffset + 8
        let textBytes = cells.reduce(0) { $0 + $1.text.utf8.count } + Self.link.utf8.count
        let paletteOffset = textOffset + textBytes
        var data = Data(repeating: 0, count: paletteOffset + 265 * 4)
        data.replaceSubrange(0..<4, with: [0x46, 0x54, 0x53, 0x46])
        put(1, at: 4, bytes: 2, in: &data)
        put(256, at: 6, bytes: 2, in: &data)
        u32(UInt32(data.count), at: 8, in: &data)
        u32(1, at: 12, in: &data)
        data.replaceSubrange(16..<32, with: [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
                                           0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF])
        data.replaceSubrange(32..<48, with: [0x10, 0x21, 0x32, 0x43, 0x54, 0x65, 0x76, 0x87,
                                           0x98, 0xA9, 0xBA, 0xCB, 0xDC, 0xED, 0xFE, 0x0F])
        for (offset, value): (Int, UInt64) in [(48, 7), (56, 19), (64, 11), (72, 12), (80, 13), (88, 14)] {
            u64(value, at: offset, in: &data)
        }
        u32(2, at: 96, in: &data); u32(5, at: 100, in: &data)
        i64(-4, at: 104, in: &data); i64(-3, at: 112, in: &data)
        u32(2, at: 120, in: &data); u32(6, at: 124, in: &data)
        u32(2, at: 128, in: &data); u32(UInt32(cells.count), at: 132, in: &data)
        u32(UInt32(textBytes), at: 136, in: &data)
        for (offset, value) in [(140, rowsOffset), (144, cellsOffset), (148, textOffset),
                                (152, paletteOffset), (156, 265), (160, 1), (164, linksOffset),
                                (168, 1), (172, 1)] {
            u32(UInt32(value), at: offset, in: &data)
        }
        i64(-4, at: rowsOffset, in: &data); u64(17, at: rowsOffset + 8, in: &data)
        u32(0, at: rowsOffset + 16, in: &data); u32(3, at: rowsOffset + 20, in: &data)
        i64(-3, at: rowsOffset + 24, in: &data); u64(18, at: rowsOffset + 32, in: &data)
        u32(3, at: rowsOffset + 40, in: &data); u32(2, at: rowsOffset + 44, in: &data)
        var textPosition = 0
        for (index, cell) in cells.enumerated() {
            let offset = cellsOffset + index * 48
            let fields = [cell.column, cell.width, UInt32(textPosition), UInt32(cell.text.utf8.count),
                          cell.flags, cell.foreground.0, cell.foreground.1, cell.background.0,
                          cell.background.1, cell.underline.0, cell.underline.1, cell.link]
            for (field, value) in fields.enumerated() { u32(value, at: offset + field * 4, in: &data) }
            let end = textPosition + cell.text.utf8.count
            data.replaceSubrange((textOffset + textPosition)..<(textOffset + end), with: cell.text.utf8)
            textPosition = end
        }
        u32(UInt32(textPosition), at: linksOffset, in: &data)
        u32(UInt32(Self.link.utf8.count), at: linksOffset + 4, in: &data)
        data.replaceSubrange((textOffset + textPosition)..<paletteOffset, with: Self.link.utf8)
        for index in 0..<265 {
            let component = UInt8(truncatingIfNeeded: index)
            data.replaceSubrange((paletteOffset + index * 4)..<(paletteOffset + index * 4 + 4),
                                 with: [component, 255 - component, component ^ 0x55, 255])
        }
        return data
    }

    private func assertRejected(_ data: Data, source: TerminalSource, _ message: String,
                                file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try TerminalRenderFrame.decode(data, expectedSource: source), message,
                             file: file, line: line)
    }

    func testDecodesCanonicalMixedWidthRowsAndAllIdentityRevisions() async throws {
        let source = try await source()
        let frame = try TerminalRenderFrame.decode(fixture(), expectedSource: source)
        XCTAssertEqual(frame.source, source)
        XCTAssertEqual(frame.sequence, 19); XCTAssertEqual(frame.geometryRevision, 11)
        XCTAssertEqual(frame.paletteRevision, 12); XCTAssertEqual(frame.fontRevision, 13)
        XCTAssertEqual(frame.revision, 14); XCTAssertEqual(frame.rows, 2); XCTAssertEqual(frame.columns, 5)
        XCTAssertEqual(frame.viewportFirst, -4)
        XCTAssertEqual(frame.cursor.stableRow, -3); XCTAssertEqual(frame.cursor.column, 2)
        XCTAssertEqual(frame.cursor.shape, 6); XCTAssertEqual(frame.cursor.width, 1)
        XCTAssertTrue(frame.cursor.visible); XCTAssertFalse(frame.cursor.blinks)
        XCTAssertEqual(frame.lines.map(\.stableIndex), [-4, -3])
        XCTAssertEqual(frame.lines.map(\.sequence), [17, 18])
        XCTAssertEqual(frame.lines[0].cells.map(\.column), [0, 1, 3])
        XCTAssertEqual(frame.lines[0].cells.map(\.width), [1, 2, 1])
        XCTAssertEqual(frame.lines[0].cells.map(\.text), ["A", "界", Self.combining])
        XCTAssertEqual(frame.lines[1].cells.map(\.text), [Self.emoji, "z"])
        XCTAssertEqual(frame.lines[1].cells[0].width, 2)
        XCTAssertEqual(frame.lines[0].cells[0].foreground, .palette(1))
        XCTAssertEqual(frame.lines[0].cells[0].underline, .rgba(.init(0xAA, 0xBB, 0xCC, 255)))
        XCTAssertEqual(frame.lines[0].cells[1].foreground, .rgba(.init(12, 34, 56, 128)))
        XCTAssertEqual(frame.lines[0].cells[1].background, .palette(200))
        XCTAssertEqual(frame.lines[0].cells[1].linkID, 1)
        XCTAssertEqual(frame.lines[0].cells[2].flags, 2 | (1 << 10))
        XCTAssertEqual(frame.links, [Self.link]); XCTAssertEqual(frame.palette.count, 265)
        XCTAssertEqual(frame.palette[200], .init(200, 55, 200 ^ 0x55, 255))
    }

    func testReadableTextKeepsRowOrderAndWithholdsTheConcealedCell() async throws {
        let frame = try TerminalRenderFrame.decode(fixture(), expectedSource: try await source())
        XCTAssertEqual(frame.readableText, "A界\n\(Self.emoji)z")
    }

    func testNonzeroDataStartIndexDoesNotChangeWireOffsets() async throws {
        let source = try await source(), bytes = fixture()
        let prefixed = Data(repeating: 0xA5, count: 17) + bytes
        let slice = prefixed.dropFirst(17)
        XCTAssertNotEqual(slice.startIndex, 0)
        let frame = try TerminalRenderFrame.decode(slice, expectedSource: source)
        XCTAssertEqual(frame.lines[1].cells[0].text, Self.emoji)
        XCTAssertEqual(frame.links, [Self.link])
    }

    func testCombiningGraphemeIsNotTruncatedAtAnEightBitLength() async throws {
        let source = try await source()
        let longCluster = "e" + String(repeating: "\u{301}", count: 200)
        XCTAssertEqual(longCluster.count, 1); XCTAssertEqual(longCluster.utf8.count, 401)
        let frame = try TerminalRenderFrame.decode(fixture(combining: longCluster), expectedSource: source)
        XCTAssertEqual(frame.lines[0].cells[2].text, longCluster)
        XCTAssertEqual(frame.lines[0].cells[2].width, 1)
        var limits = TerminalRenderLimits(); limits.maximumCellTextBytes = 400
        XCTAssertThrowsError(try TerminalRenderFrame.decode(fixture(combining: longCluster),
                                                             expectedSource: source, limits: limits)) {
            XCTAssertEqual($0 as? TerminalRenderError, .capacity)
        }
    }

    func testForeignChannelTransportAndRouteRevisionFailBeforePresentation() async throws {
        let bytes = fixture()
        for source in [try await source(channel: UUID()), try await source(transport: UUID()),
                       try await source(routeRevision: 8)] {
            XCTAssertThrowsError(try TerminalRenderFrame.decode(bytes, expectedSource: source)) {
                XCTAssertEqual($0 as? TerminalRenderError, .staleSource)
            }
        }
        let expected = try await source()
        for offset in 16..<48 {
            var changed = bytes; changed[offset] ^= 1
            XCTAssertThrowsError(try TerminalRenderFrame.decode(changed, expectedSource: expected),
                                 "every UUID byte participates in source binding: \(offset)") {
                XCTAssertEqual($0 as? TerminalRenderError, .staleSource)
            }
        }
    }

    func testMalformedHeaderVersionReservedBytesAndIncompleteFramesAreRejected() async throws {
        let source = try await source(), bytes = fixture()
        for offset in [0, 4, 6, 176, 191, 255] {
            var changed = bytes; changed[offset] ^= 0x80
            assertRejected(changed, source: source, "header byte \(offset)")
        }
        for (offset, value): (Int, UInt32) in [(8, UInt32(bytes.count - 1)), (12, 0), (12, 16),
                                               (96, 0), (100, 0), (128, 1), (156, 264)] {
            var changed = bytes; u32(value, at: offset, in: &changed)
            assertRejected(changed, source: source, "header field \(offset) = \(value)")
        }
        for offset in [64, 72, 80, 88] {
            var changed = bytes; u64(0, at: offset, in: &changed)
            assertRejected(changed, source: source, "zero revision \(offset)")
        }
        for cut in [0, 3, 7, 15, 47, 95, 175, 255, 256, 303, 304, 543, 544, 551, 552, bytes.count - 1] {
            var changed = Data(bytes.prefix(cut))
            if changed.count >= 256 { u32(UInt32(changed.count), at: 8, in: &changed) }
            assertRejected(changed, source: source, "truncated length \(cut)")
        }
        var trailing = bytes + Data([0]); u32(UInt32(trailing.count), at: 8, in: &trailing)
        assertRejected(trailing, source: source, "unclaimed trailing byte")
    }

    func testSectionOffsetsCannotOverlapLeaveGapsOrPointOutsideTheFrame() async throws {
        let source = try await source(), bytes = fixture()
        for offset in [140, 144, 148, 152, 164] {
            for value: UInt32 in [0, 255, UInt32(read32(bytes, at: offset) + 1), UInt32.max] {
                var changed = bytes; u32(value, at: offset, in: &changed)
                assertRejected(changed, source: source, "section \(offset) = \(value)")
            }
        }
    }

    func testRowsAndCellsCannotOverlapOrEscapeTheirAuthoritativeColumns() async throws {
        let source = try await source(), bytes = fixture(), cells = 304
        for (offset, value): (Int, UInt32) in [(272, 1), (276, 6), (296, 2), (300, 3),
                                               (cells + 4, 0), (cells + 4, 3),
                                               (cells + 48, 4), (cells + 96, 2),
                                               (cells + 192, 5)] {
            var changed = bytes; u32(value, at: offset, in: &changed)
            assertRejected(changed, source: source, "row/cell field \(offset) = \(value)")
        }
        var duplicateRow = bytes; i64(-4, at: 280, in: &duplicateRow)
        assertRejected(duplicateRow, source: source, "duplicate stable row")
        var futureRow = bytes; u64(20, at: 264, in: &futureRow)
        assertRejected(futureRow, source: source, "row sequence exceeds terminal sequence")
        var overflow = bytes; i64(Int64.max, at: 104, in: &overflow)
        assertRejected(overflow, source: source, "viewport arithmetic overflow")
    }

    func testUTF8RangesAndLinksMustCoverTheDeclaredTextPoolExactly() async throws {
        let source = try await source(), bytes = fixture(), text = read32(bytes, at: 148)
        for offset in [text, text + 1, text + 5, text + 7, read32(bytes, at: 152) - 1] {
            var changed = bytes; changed[offset] = 0xFF
            assertRejected(changed, source: source, "invalid UTF8 at \(offset)")
        }
        for (offset, value): (Int, UInt32) in [(304 + 8, 1), (304 + 12, UInt32.max),
                                               (352 + 8, 2), (352 + 12, 2),
                                               (544, 0), (548, UInt32.max), (352 + 44, 2)] {
            var changed = bytes; u32(value, at: offset, in: &changed)
            assertRejected(changed, source: source, "text/link field \(offset) = \(value)")
        }
        for control in ["\u{00}", "\u{1B}", "\n", "\u{85}"] {
            assertRejected(fixture(combining: control), source: source, "terminal controls are not render text")
        }
    }

    func testUnknownStylesColorsAndCursorShapesAreNotSilentlyCoerced() async throws {
        let source = try await source(), bytes = fixture()
        for flags: UInt32 in [3, 6 << 2, 3 << 5, 3 << 12, 1 << 14, 3 << 15, 1 << 17, UInt32.max] {
            var changed = bytes; u32(flags, at: 304 + 16, in: &changed)
            assertRejected(changed, source: source, "unknown/unsupported cell flags \(flags)")
        }
        for (offset, value): (Int, UInt32) in [(304 + 20, 3), (304 + 24, 256), (304 + 32, 1),
                                               (124, 7), (168, 2), (172, 0), (172, 3), (120, 5)] {
            var changed = bytes; u32(value, at: offset, in: &changed)
            assertRejected(changed, source: source, "color/cursor field \(offset) = \(value)")
        }
        var overflowingCursor = bytes
        u32(4, at: 120, in: &overflowingCursor); u32(2, at: 172, in: &overflowingCursor)
        assertRejected(overflowingCursor, source: source, "cursor span extends past right edge")
    }

    func testCountsAndByteCapsRejectOversizedFramesBeforeAllocatingTheirSections() async throws {
        let source = try await source(), bytes = fixture()
        for (offset, value): (Int, UInt32) in [(96, 513), (100, 513), (128, UInt32.max),
                                               (132, UInt32.max), (136, UInt32.max),
                                               (156, UInt32.max), (160, UInt32.max)] {
            var changed = bytes; u32(value, at: offset, in: &changed)
            assertRejected(changed, source: source, "oversized header field \(offset)")
        }
        let oversized = Data(repeating: 0, count: 8 * 1024 * 1024 + 1)
        assertRejected(oversized, source: source, "frame byte cap")
    }

    func testCallerCanLowerEachIndependentDecodeBudget() async throws {
        let source = try await source(), bytes = fixture()
        var limits = [TerminalRenderLimits](repeating: .init(), count: 8)
        limits[0].maximumFrameBytes = bytes.count - 1
        limits[1].maximumRows = 1; limits[2].maximumColumns = 4
        limits[3].maximumCells = 4; limits[4].maximumTextBytes = 19
        limits[5].maximumLinks = 0; limits[6].maximumLinkBytes = Self.link.utf8.count - 1
        limits[7].maximumCells = 8 // Five records occupy a ten-cell viewport, including blanks.
        for (index, limit) in limits.enumerated() {
            XCTAssertThrowsError(try TerminalRenderFrame.decode(bytes, expectedSource: source, limits: limit),
                                 "independent budget \(index)") {
                XCTAssertEqual($0 as? TerminalRenderError, .capacity)
            }
        }
    }

    func testFullReplacementStillRejectsRegressingOrReusedSemanticRevisions() async throws {
        let source = try await source(), bytes = fixture()
        let previous = try TerminalRenderFrame.decode(bytes, expectedSource: source)
        var successor = bytes; u64(15, at: 88, in: &successor)
        XCTAssertNoThrow(try TerminalRenderFrame.decode(successor, expectedSource: source).validateSuccessor(of: previous))
        for (offset, value): (Int, UInt64) in [(56, 18), (64, 10), (72, 11), (80, 12), (88, 14)] {
            var changed = successor; u64(value, at: offset, in: &changed)
            let next = try TerminalRenderFrame.decode(changed, expectedSource: source)
            XCTAssertThrowsError(try next.validateSuccessor(of: previous), "regressed witness \(offset)") {
                XCTAssertEqual($0 as? TerminalRenderError, .staleFrame)
            }
        }
        var resized = successor; u32(6, at: 100, in: &resized)
        XCTAssertThrowsError(try TerminalRenderFrame.decode(resized, expectedSource: source).validateSuccessor(of: previous))
        u64(12, at: 64, in: &resized)
        XCTAssertNoThrow(try TerminalRenderFrame.decode(resized, expectedSource: source).validateSuccessor(of: previous))
        var alternate = successor; u32(5, at: 12, in: &alternate)
        XCTAssertThrowsError(try TerminalRenderFrame.decode(alternate, expectedSource: source).validateSuccessor(of: previous))
        u64(12, at: 64, in: &alternate)
        XCTAssertNoThrow(try TerminalRenderFrame.decode(alternate, expectedSource: source).validateSuccessor(of: previous))
        var recolored = successor; recolored[read32(bytes, at: 152)] ^= 1
        XCTAssertThrowsError(try TerminalRenderFrame.decode(recolored, expectedSource: source).validateSuccessor(of: previous))
        u64(13, at: 72, in: &recolored)
        XCTAssertNoThrow(try TerminalRenderFrame.decode(recolored, expectedSource: source).validateSuccessor(of: previous))
    }
}

final class TerminalRenderSlotTests: XCTestCase {
    func testSlotReservationAndSubmissionHaveSeparateLifetimes() throws {
        var slots = TerminalRenderSlots()
        let a = try XCTUnwrap(slots.acquire()), b = try XCTUnwrap(slots.acquire())
        let c = try XCTUnwrap(slots.acquire())
        XCTAssertEqual(Set([a.slot, b.slot, c.slot]).count, 3)
        XCTAssertNil(try slots.acquire()); XCTAssertEqual(slots.inFlight, 0)
        XCTAssertThrowsError(try slots.complete(a))
        try slots.submit(a); XCTAssertEqual(slots.inFlight, 1)
        XCTAssertThrowsError(try slots.submit(a)); XCTAssertThrowsError(try slots.abandon(a))
        try slots.abandon(b)
        let replacement = try XCTUnwrap(slots.acquire())
        XCTAssertEqual(replacement.slot, b.slot); XCTAssertNotEqual(replacement, b)
        XCTAssertThrowsError(try slots.abandon(b))
        XCTAssertThrowsError(try slots.submit(b))
        XCTAssertNil(try slots.acquire())
        try slots.complete(a); try slots.abandon(c); try slots.abandon(replacement)
        XCTAssertEqual(slots.inFlight, 0)
    }

    func testOutOfOrderCompletionCannotRetireAnEarlierFrameOrAnABAReplacement() throws {
        var slots = TerminalRenderSlots()
        let a = try XCTUnwrap(slots.acquire()), b = try XCTUnwrap(slots.acquire())
        let c = try XCTUnwrap(slots.acquire())
        try slots.submit(a); try slots.submit(b); try slots.submit(c)
        try slots.complete(c)
        XCTAssertEqual(slots.inFlight, 2)
        let d = try XCTUnwrap(slots.acquire())
        XCTAssertEqual(d.slot, c.slot)
        XCTAssertNil(try slots.acquire(), "completion of C must not free A or B")
        try slots.submit(d)
        XCTAssertThrowsError(try slots.complete(c), "late duplicate C completion cannot retire D")
        XCTAssertEqual(slots.inFlight, 3)
        try slots.complete(b); try slots.complete(a); try slots.complete(d)
        XCTAssertEqual(slots.inFlight, 0)
    }

    func testLeasesAreScopedToTheirResourceOwner() throws {
        var first = TerminalRenderSlots(), second = TerminalRenderSlots()
        let a = try XCTUnwrap(first.acquire()), b = try XCTUnwrap(second.acquire())
        XCTAssertEqual(a.slot, b.slot); XCTAssertEqual(a.serial, b.serial)
        XCTAssertNotEqual(a, b, "matching slot/serial from another owner is not the same resource")
        XCTAssertThrowsError(try first.submit(b))
        XCTAssertThrowsError(try first.abandon(b))
        try first.submit(a); try second.submit(b)
        XCTAssertThrowsError(try first.complete(b))
        XCTAssertEqual(first.inFlight, 1); XCTAssertEqual(second.inFlight, 1)
        try first.complete(a); try second.complete(b)
    }
}

final class TerminalRenderTextLayoutTests: XCTestCase {
    private func cell(_ column: Int, _ width: Int, _ text: String, _ flags: UInt32 = 0) -> TerminalRenderCell {
        .init(column: column, width: width, text: text, flags: flags, foreground: .default,
              background: .default, underline: .default, linkID: 0)
    }

    func testUTF16ShapingIndicesMapToCanonicalWideAndCombiningCellsAcrossGaps() throws {
        let row = TerminalRenderRow(stableIndex: -4, sequence: 9, cells: [
            cell(0, 2, "👩‍💻"), cell(2, 1, "e\u{301}", 1), cell(5, 1, "Z", 1 << 7)
        ])
        let layout = try TerminalRenderTextLayout(row: row)
        XCTAssertEqual(layout.text, "👩‍💻e\u{301}  Z")
        XCTAssertEqual(layout.ranges, [0..<5, 5..<7, 9..<10])
        XCTAssertEqual(layout.cellIndices, [0, 0, 0, 0, 0, 1, 1, nil, nil, 2])
        XCTAssertEqual(row.cells.map(\.column), [0, 2, 5])
        XCTAssertEqual(row.cells.map(\.width), [2, 1, 1])
        XCTAssertEqual(layout.text.utf16.count, 10)
        XCTAssertNoThrow(try TerminalRenderTextLayout(row: row, maximumUTF16: 10))
        XCTAssertThrowsError(try TerminalRenderTextLayout(row: row, maximumUTF16: 9))
    }

    func testReadableRowTextSpacesGapsAndBlanksAndConcealsHiddenCells() {
        let row = TerminalRenderRow(stableIndex: 0, sequence: 1, cells: [
            cell(0, 2, "界"), cell(2, 1, ""), cell(3, 1, "x"), cell(6, 6, "secret", 1 << 10), cell(12, 1, "y"),
            cell(13, 1, " ")
        ])
        XCTAssertEqual(row.readableText, "界 x" + String(repeating: " ", count: 8) + "y")
        XCTAssertEqual(TerminalRenderRow(stableIndex: 1, sequence: 1, cells: []).readableText, "")
    }

    func testShapingBudgetBoundsGapsAndLongGraphemesBeforeAllocation() throws {
        let longCluster = "e" + String(repeating: "\u{301}", count: 200)
        let row = TerminalRenderRow(stableIndex: 0, sequence: 0, cells: [cell(0, 1, longCluster)])
        let layout = try TerminalRenderTextLayout(row: row, maximumUTF16: 201)
        XCTAssertEqual(layout.ranges, [0..<201]); XCTAssertEqual(layout.cellIndices.count, 201)
        XCTAssertTrue(layout.cellIndices.allSatisfy { $0 == 0 })
        XCTAssertThrowsError(try TerminalRenderTextLayout(row: row, maximumUTF16: 200))
        XCTAssertThrowsError(try TerminalRenderTextLayout(row: row, maximumUTF16: 0))
        XCTAssertThrowsError(try TerminalRenderTextLayout(row: row, maximumUTF16: 32769))
        let gap = TerminalRenderRow(stableIndex: 0, sequence: 0, cells: [cell(32768, 1, "a")])
        XCTAssertThrowsError(try TerminalRenderTextLayout(row: gap))
        let overlapping = TerminalRenderRow(stableIndex: 0, sequence: 0,
                                            cells: [cell(0, 2, "界"), cell(1, 1, "a")])
        XCTAssertThrowsError(try TerminalRenderTextLayout(row: overlapping))
    }
}

/// The consumer of these bytes is the checked-in Rust renderer's Metal shader,
/// so derive its offsets from the actual shader declarations rather than from
/// Swift's packing constants or native struct ABI. These tests run on Linux too.
final class TerminalRenderPackingTests: XCTestCase {
    private struct MetalLayout {
        var offsets: [String: Int]
        var size: Int
    }
    private enum FixtureError: Error { case unsupportedDeclaration, missingDeclaration }

    private func rendererSource(_ path: String) throws -> String {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        return try String(contentsOf: root.appendingPathComponent("crates/frankenterm-renderer-metal/src/" + path),
                          encoding: .utf8)
    }

    private func metalLayout(_ name: String, source: String) throws -> MetalLayout {
        guard let begin = source.range(of: "struct \(name) {"),
              let end = source.range(of: "};", range: begin.upperBound..<source.endIndex)
        else { throw FixtureError.missingDeclaration }
        let body = source[begin.upperBound..<end.lowerBound].components(separatedBy: "\n")
            .map { $0.components(separatedBy: "//")[0] }.joined(separator: "\n")
        // Metal vector layout, independent of the Swift producer's offsets.
        let types: [String: (size: Int, alignment: Int)] = [
            "ulong": (8, 8), "uint": (4, 4), "float": (4, 4),
            "uint2": (8, 8), "float2": (8, 8), "float4": (16, 16),
            "ushort2": (4, 4), "short2": (4, 4), "uchar4": (4, 4)
        ]
        var position = 0, alignment = 1, offsets: [String: Int] = [:]
        for declaration in body.split(separator: ";") {
            let fields = declaration.split(whereSeparator: { $0.isWhitespace }).map(String.init)
            if fields.isEmpty { continue }
            guard fields.count == 2, let type = types[fields[0]], offsets[fields[1]] == nil
            else { throw FixtureError.unsupportedDeclaration }
            position = (position + type.alignment - 1) / type.alignment * type.alignment
            offsets[fields[1]] = position
            position += type.size; alignment = max(alignment, type.alignment)
        }
        return MetalLayout(offsets: offsets, size: (position + alignment - 1) / alignment * alignment)
    }

    private func rustSize(_ name: String) throws -> Int {
        let source = try rendererSource("frame.rs")
        let expression = try NSRegularExpression(pattern: "pub const \(name): usize = ([0-9]+);")
        let match = try XCTUnwrap(expression.firstMatch(in: source, range: NSRange(source.startIndex..., in: source)))
        let range = try XCTUnwrap(Range(match.range(at: 1), in: source))
        return try XCTUnwrap(Int(source[range]))
    }

    private func word(_ bytes: [UInt8], at: Int, count: Int = 4) -> UInt64 {
        (0..<count).reduce(0) { $0 | (UInt64(bytes[at + $1]) << ($1 * 8)) }
    }

    func testUniformPackingMatchesBothCanonicalMetalShaderLayouts() throws {
        let text = try metalLayout("FrameUniforms", source: rendererSource("shaders/text.metal"))
        let background = try metalLayout("FrameUniforms", source: rendererSource("shaders/background.metal"))
        XCTAssertEqual(text.size, 192); XCTAssertEqual(background.size, 192)
        for (name, offset) in background.offsets where name != "reserved2" {
            XCTAssertEqual(text.offsets[name], offset, "shared shader field \(name)")
        }
        var value = TerminalRenderUniforms()
        value.frame = 0x0807060504030201; value.viewport = (1920, 1080)
        value.rows = 24; value.columns = 80; value.clear = .init(128, 64, 10, 128)
        value.cellSize = (9.5, 21.25); value.origin = (12.5, 4.25)
        value.rowOffset = 7; value.cursorShape = 3; value.cursorColumn = 43; value.cursorRow = 11
        value.cursorWidth = 2; value.cursorColor = .init(0, 255, 0, 64); value.cursorThickness = 1.5
        value.selection = .init(255, 0, 255, 128); value.search = .init(64, 128, 255)
        value.currentSearch = .init(10, 11, 12, 0)
        value.underlinePosition = 18.25; value.lineThickness = 1.25; value.strikethroughPosition = 10.5
        value.hsb = (0.75, 0.5, 1.25)
        let bytes = try TerminalRenderPacking.uniforms(value)
        XCTAssertEqual(bytes.count, try rustSize("UNIFORMS_BYTES"))
        XCTAssertEqual(word(bytes, at: try XCTUnwrap(text.offsets["frame"]), count: 8), 0x0807060504030201)
        let integers: [(String, [UInt64])] = [
            ("viewport", [1920, 1080]), ("grid", [24, 80]), ("row_offset", [7]),
            ("cursor_shape", [3]), ("cursor_cell", [43, 11]), ("cursor_width", [2])
        ]
        for (name, expected) in integers {
            let offset = try XCTUnwrap(text.offsets[name])
            XCTAssertEqual(expected.indices.map { word(bytes, at: offset + $0 * 4) }, expected, name)
        }
        let floats: [(String, [Float])] = [
            ("clear", [0.108353506, 0.025735258, 0.001523587, 0.501960784]),
            ("cell_size", [9.5, 21.25]), ("grid_origin", [12.5, 4.25]),
            ("cursor_color", [0, 0.250980392, 0, 0.250980392]), ("cursor_thickness", [1.5]),
            ("selection_tint", [0.501960784, 0, 0.501960784, 0.501960784]),
            ("search_tint", [0.051269458, 0.215860500, 1, 1]), ("current_tint", [0, 0, 0, 0]),
            ("underline_position", [18.25]), ("line_thickness", [1.25]),
            ("strikethrough_position", [10.5]), ("hsb", [0.75, 0.5, 1.25, 1])
        ]
        for (name, expected) in floats {
            let offset = try XCTUnwrap(text.offsets[name])
            for (index, wanted) in expected.enumerated() {
                XCTAssertEqual(Float(bitPattern: UInt32(word(bytes, at: offset + index * 4))), wanted,
                               accuracy: 0.0000002, "\(name)[\(index)]")
            }
        }
        for range in [24..<32, 104..<112, 172..<176, 192..<256] {
            XCTAssertTrue(bytes[range].allSatisfy { $0 == 0 }, "reserved GPU bytes \(range)")
        }
    }

    func testGlyphInstanceMatchesCanonicalShaderAndLiteralRustWireFixture() throws {
        let layout = try metalLayout("CellText", source: rendererSource("shaders/text.metal"))
        XCTAssertEqual(layout.offsets, ["grid": 0, "atlas": 4, "size": 8, "offset": 12, "fg": 16, "decoration": 20])
        XCTAssertEqual(layout.size, try rustSize("CELL_TEXT_INSTANCE_BYTES"))
        let actual = try TerminalRenderPacking.text(
            column: 0x0201, row: 0x0403, width: 2,
            foreground: .init(17, 34, 51, 68), underline: .init(85, 102, 119),
            flags: 1 | (3 << 3) | 64 | 128, atlas: (0x0605, 0x0807, 0x0A09, 0x0C0B), offset: (-257, 258))
        // RGBA stays straight sRGB here; only uniforms are linear/premultiplied.
        // Last byte combines gray atlas, wide cell, curly underline, strike and overline.
        XCTAssertEqual(actual, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
                                255, 254, 2, 1, 17, 34, 51, 68, 85, 102, 119, 221])
        XCTAssertEqual(TerminalRenderPacking.backgroundBytes, try rustSize("CELL_BG_BYTES_PER_CELL"))
    }

    func testUniformPackingRejectsInvalidGeometryAndNonfiniteNativeInputs() throws {
        var cases = [TerminalRenderUniforms](repeating: .init(), count: 9)
        cases[0].viewport = (0, 1); cases[1].viewport = (16385, 1)
        cases[2].rows = 513; cases[3].rows = 512; cases[3].columns = 512
        cases[4].rowOffset = 1; cases[5].cursorShape = 5
        cases[6].cellSize = (.nan, 1); cases[7].origin = (0, .infinity)
        cases[8].hsb = (1, -1, 1)
        for (index, value) in cases.enumerated() {
            XCTAssertThrowsError(try TerminalRenderPacking.uniforms(value), "invalid native input \(index)")
        }
        let bytes = try TerminalRenderPacking.uniforms(.init())
        XCTAssertTrue(bytes[176..<256].allSatisfy { $0 == 0 }, "absent HSB and reserved tail")
    }

    func testGlyphPackingRejectsUnrepresentableAtlasAndSignedOffsets() throws {
        for values in [(65536, 0, 1, 0, 0), (0, -1, 1, 0, 0), (0, 0, 3, 0, 0),
                       (0, 0, 1, 16385, 0), (0, 0, 1, 0, 32768), (0, 0, 1, 0, -32769)] {
            XCTAssertThrowsError(try TerminalRenderPacking.text(
                column: values.0, row: values.1, width: values.2, foreground: .init(255, 255, 255),
                underline: .init(255, 255, 255), flags: 0, atlas: (values.3, 0, 0, 0), offset: (values.4, 0)))
        }
    }
}
