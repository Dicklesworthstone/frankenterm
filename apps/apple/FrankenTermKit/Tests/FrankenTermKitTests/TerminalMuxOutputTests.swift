import Foundation
import XCTest
@testable import FrankenTermKit

final class TerminalMuxOutputTests: XCTestCase {
    private func geometry(_ columns: UInt32 = 80, _ rows: UInt32 = 24, pixels: UInt32 = 0) throws -> TerminalSessionGeometry {
        try .init(columns: columns, rows: rows, pixelWidth: pixels, pixelHeight: pixels)
    }
    private func selected() throws -> (TerminalMuxInputRouting, TerminalMuxInputRouting.Destination) {
        let routing = TerminalMuxInputRouting()
        try routing.replaceInventory([1, 2])
        return (routing, try routing.select(pane: 1))
    }

    func testExactChunksResizeFirstAndReadinessOnlyOnLast() throws {
        let (routing, destination) = try selected()
        let original = try geometry(), resized = try geometry(100, 40)
        var buffer = TerminalMuxOutputBuffer(geometry: original)
        let bytes = Data((0..<40_001).map { UInt8(truncatingIfNeeded: $0) })
        try buffer.append(bytes, geometry: resized, destination: destination)
        var received = Data(), counts: [Int] = [], sizes: [TerminalSessionGeometry] = []
        while let chunk = buffer.next() {
            received += chunk.output; counts.append(chunk.output.count)
            if let size = chunk.resized { sizes.append(size) }
            if let witness = chunk.muxDestination {
                XCTAssertEqual(received.count, bytes.count)
                XCTAssertFalse(chunk.muxPreparation)
                routing.didApplyOutput(witness)
            } else {
                XCTAssertTrue(chunk.muxPreparation)
                XCTAssertFalse(routing.current.ready)
            }
        }
        XCTAssertEqual(received, bytes)
        XCTAssertEqual(counts, [16_384, 16_384, 7_233])
        XCTAssertEqual(sizes, [resized])
        XCTAssertTrue(routing.current.ready)
        XCTAssertTrue(buffer.isEmpty)
        XCTAssertEqual(buffer.retainedBytes, 0)
    }

    func testFullAllocationRemainsChargedUntilLastChunk() throws {
        var buffer = TerminalMuxOutputBuffer(geometry: try geometry())
        let count = TerminalMuxOutputBuffer.maximumChunkBytes + 1
        try buffer.append(Data(repeating: 7, count: count), geometry: geometry(), destination: nil)
        XCTAssertEqual(buffer.retainedBytes, count)
        XCTAssertEqual(buffer.next()?.output.count, 16_384)
        XCTAssertEqual(buffer.remainingBytes, 1)
        XCTAssertEqual(buffer.retainedBytes, count, "the unsent slice still owns its original allocation")
        XCTAssertEqual(buffer.next()?.output, Data([7]))
        XCTAssertEqual(buffer.retainedBytes, 0)
    }

    func testResetAndPalettePrecedeCompletingOutputWithoutReadiness() throws {
        let (routing, destination) = try selected()
        var buffer = TerminalMuxOutputBuffer(geometry: try geometry())
        let reset = Data([0x1B, 0x63])
        try buffer.append(reset, geometry: geometry(90), destination: nil, preparing: true)
        let colors = Data((0..<TerminalMuxOutputBuffer.paletteBytes).map { UInt8(truncatingIfNeeded: $0) })
        XCTAssertThrowsError(try buffer.appendPalette(colors))
        let prefix = try XCTUnwrap(buffer.next())
        XCTAssertEqual(prefix.output, reset)
        XCTAssertEqual(prefix.resized, try geometry(90))
        XCTAssertTrue(prefix.muxPreparation); XCTAssertNil(prefix.muxDestination)
        try buffer.appendPalette(colors)
        XCTAssertEqual(buffer.retainedBytes, colors.count)
        XCTAssertThrowsError(try buffer.append(Data([1]), geometry: geometry(90), destination: destination))
        let palette = try XCTUnwrap(buffer.next())
        XCTAssertEqual(palette.muxPalette, colors)
        XCTAssertTrue(palette.output.isEmpty); XCTAssertNil(palette.resized)
        XCTAssertTrue(palette.muxPreparation); XCTAssertNil(palette.muxDestination)
        XCTAssertFalse(routing.current.ready); XCTAssertTrue(buffer.isEmpty)
        XCTAssertEqual(buffer.retainedBytes, 0)
        try buffer.append(Data("complete".utf8), geometry: geometry(90), destination: destination)
        let complete = try XCTUnwrap(buffer.next())
        XCTAssertFalse(complete.muxPreparation); XCTAssertNil(complete.muxPalette)
        routing.didApplyOutput(try XCTUnwrap(complete.muxDestination))
        XCTAssertTrue(routing.current.ready)
    }

    func testPaletteBoundsOwnershipAndRetirement() throws {
        var buffer = TerminalMuxOutputBuffer(geometry: try geometry())
        for size in [0, 1, TerminalMuxOutputBuffer.paletteBytes - 1, TerminalMuxOutputBuffer.paletteBytes + 1] {
            XCTAssertThrowsError(try buffer.appendPalette(Data(count: size)))
            XCTAssertTrue(buffer.isEmpty)
        }
        var backing = Data(repeating: 7, count: 20_000)
        try buffer.appendPalette(backing[91..<(91 + TerminalMuxOutputBuffer.paletteBytes)])
        backing[91] = 9
        XCTAssertEqual(buffer.next()?.muxPalette, Data(repeating: 7, count: TerminalMuxOutputBuffer.paletteBytes))
        try buffer.appendPalette(Data(repeating: 3, count: TerminalMuxOutputBuffer.paletteBytes))
        buffer.retire()
        XCTAssertNil(buffer.next()); XCTAssertEqual(buffer.retainedBytes, 0)
        XCTAssertThrowsError(try buffer.appendPalette(Data(count: TerminalMuxOutputBuffer.paletteBytes)))
    }

    func testPreparingEventCannotCarryAnInputReadinessWitness() throws {
        let (_, destination) = try selected()
        var buffer = TerminalMuxOutputBuffer(geometry: try geometry())
        XCTAssertThrowsError(try buffer.append(Data(), geometry: geometry(), destination: destination, preparing: true))
        XCTAssertTrue(buffer.isEmpty)
    }

    func testBackpressureNeverOverwritesEvenAnEmptyEvent() throws {
        let (_, destination) = try selected()
        var buffer = TerminalMuxOutputBuffer(geometry: try geometry())
        try buffer.append(Data(), geometry: geometry(), destination: destination)
        XCTAssertFalse(buffer.isEmpty)
        XCTAssertThrowsError(try buffer.append(Data([2]), geometry: geometry(), destination: nil))
        let update = try XCTUnwrap(buffer.next())
        XCTAssertTrue(update.output.isEmpty); XCTAssertEqual(update.muxDestination, destination)
        XCTAssertNil(buffer.next())
        try buffer.append(Data([3]), geometry: geometry(), destination: nil)
        XCTAssertEqual(buffer.next()?.output, Data([3]))
    }

    func testByteLimitAcceptsExactBoundaryAndRefusesOversizeWithoutPublication() throws {
        var buffer = TerminalMuxOutputBuffer(geometry: try geometry())
        let maximum = TerminalMuxOutputBuffer.maximumEventBytes
        XCTAssertThrowsError(try buffer.append(Data(count: maximum + 1), geometry: geometry(90), destination: nil))
        XCTAssertTrue(buffer.isEmpty); XCTAssertEqual(buffer.queuedGeometry, try geometry())
        try buffer.append(Data(repeating: 0xA5, count: maximum), geometry: geometry(), destination: nil)
        var count = 0
        while let chunk = buffer.next() {
            XCTAssertLessThanOrEqual(chunk.output.count, TerminalMuxOutputBuffer.maximumChunkBytes)
            XCTAssertTrue(chunk.output.allSatisfy { $0 == 0xA5 }); count += chunk.output.count
        }
        XCTAssertEqual(count, maximum); XCTAssertEqual(buffer.retainedBytes, 0)
    }

    func testTenThousandPixelChangesKeepOneTailWithoutRewritingEventGeometry() throws {
        let original = try geometry(), eventSize = try geometry(100, 30, pixels: 100)
        var buffer = TerminalMuxOutputBuffer(geometry: original)
        try buffer.append(Data(repeating: 1, count: 20_000), geometry: eventSize, destination: nil)
        for pixels in UInt32(1)...10_000 { try buffer.requestPixelGeometry(geometry(100, 30, pixels: pixels)) }
        XCTAssertEqual(buffer.retainedBytes, 20_000)
        let first = try XCTUnwrap(buffer.next()), last = try XCTUnwrap(buffer.next()), pixel = try XCTUnwrap(buffer.next())
        XCTAssertEqual(first.resized, eventSize); XCTAssertNil(last.resized)
        XCTAssertEqual(first.output.count + last.output.count, 20_000)
        XCTAssertEqual(pixel.resized, try geometry(100, 30, pixels: 10_000))
        XCTAssertTrue(pixel.output.isEmpty); XCTAssertNil(pixel.muxDestination)
        XCTAssertNil(buffer.next())
    }

    func testCellChangeCannotBeDisguisedAsPixelOnlyResize() throws {
        var buffer = TerminalMuxOutputBuffer(geometry: try geometry())
        XCTAssertThrowsError(try buffer.requestPixelGeometry(geometry(81)))
        XCTAssertThrowsError(try buffer.requestPixelGeometry(geometry(80, 25)))
        XCTAssertTrue(buffer.isEmpty); XCTAssertEqual(buffer.queuedGeometry, try geometry())
    }

    func testPendingPixelChangeMustPrecedeNextOutput() throws {
        var buffer = TerminalMuxOutputBuffer(geometry: try geometry())
        let pixels = try geometry(pixels: 500)
        try buffer.requestPixelGeometry(pixels)
        XCTAssertThrowsError(try buffer.append(Data([1]), geometry: geometry(), destination: nil))
        XCTAssertEqual(buffer.next()?.resized, pixels)
        try buffer.append(Data([2]), geometry: geometry(), destination: nil)
        XCTAssertEqual(buffer.next()?.resized, try geometry())
    }

    func testNonzeroDataSliceIsCopiedExactlyAndDoesNotFollowCallerMutation() throws {
        var large = Data(repeating: 7, count: 500_000)
        large.replaceSubrange(200..<206, with: Data("é🙂".utf8))
        let slice = large[200..<206]
        XCTAssertNotEqual(slice.startIndex, 0)
        var buffer = TerminalMuxOutputBuffer(geometry: try geometry())
        try buffer.append(slice, geometry: geometry(), destination: nil)
        large[200] = 0
        XCTAssertEqual(buffer.retainedBytes, 6)
        XCTAssertEqual(buffer.next()?.output, Data("é🙂".utf8))
    }

    func testOldVisitSuffixCompletesWithoutEnablingReplacement() throws {
        let (routing, original) = try selected()
        var buffer = TerminalMuxOutputBuffer(geometry: try geometry())
        let payload = Data(repeating: 1, count: 16_383) + Data("🙂\u{1b}[31mold".utf8)
        try buffer.append(payload, geometry: geometry(), destination: original)
        let head = try XCTUnwrap(buffer.next())
        XCTAssertEqual(head.output.last, 0xF0, "the consumer owns UTF-8 assembly, not this chunker")
        let replacement = try routing.select(pane: 2)
        let tail = try XCTUnwrap(buffer.next())
        routing.didApplyOutput(try XCTUnwrap(tail.muxDestination))
        XCTAssertFalse(routing.current.ready)
        XCTAssertEqual(head.output + tail.output, payload)
        try buffer.append(Data("new".utf8), geometry: geometry(), destination: replacement)
        let next = try XCTUnwrap(buffer.next())
        routing.didApplyOutput(try XCTUnwrap(next.muxDestination))
        XCTAssertTrue(routing.current.ready)
    }

    func testRetirementReleasesOwnedDataAndRefusesLatePublication() throws {
        var buffer = TerminalMuxOutputBuffer(geometry: try geometry())
        try buffer.append(Data(count: 32_768), geometry: geometry(), destination: nil)
        let inFlight = try XCTUnwrap(buffer.next())
        try buffer.requestPixelGeometry(geometry(pixels: 100))
        buffer.retire(); buffer.retire()
        XCTAssertTrue(buffer.isRetired); XCTAssertTrue(buffer.isEmpty); XCTAssertEqual(buffer.retainedBytes, 0)
        XCTAssertEqual(inFlight.output.count, 16_384)
        XCTAssertNil(buffer.next())
        XCTAssertThrowsError(try buffer.append(Data([1]), geometry: geometry(), destination: nil))
        XCTAssertThrowsError(try buffer.requestPixelGeometry(geometry()))
    }

    func testManyEventsAndLayoutsPreserveEveryByteAndBoundedRetention() throws {
        var buffer = TerminalMuxOutputBuffer(geometry: try geometry())
        var expected = Data(), actual = Data()
        for index in 0..<500 {
            let size = try geometry(UInt32(80 + index % 3), UInt32(24 + index % 5))
            let bytes = Data(repeating: UInt8(truncatingIfNeeded: index), count: 1 + index * 131 % 40_000)
            try buffer.append(bytes, geometry: size, destination: nil); expected += bytes
            try buffer.requestPixelGeometry(geometry(size.columns, size.rows, pixels: UInt32(index)))
            while let chunk = buffer.next() {
                actual += chunk.output
                XCTAssertLessThanOrEqual(buffer.retainedBytes, bytes.count)
            }
            XCTAssertEqual(buffer.retainedBytes, 0)
        }
        XCTAssertEqual(actual, expected)
    }
}
