import Foundation
import XCTest
@testable import FrankenTermKit

final class FixedQRCodeTests: XCTestCase {
    let crypto = TestCryptography()

    func testMaximumProfileIncludesEveryWrapperAndCounterByteAtLevelQ() throws {
        let text = maximumProfileFrame()
        XCTAssertEqual(text.utf8.count, 767)
        let frame = try OpticalFrame(text: text)
        XCTAssertEqual(frame.pageIndex, 127)
        XCTAssertEqual(frame.identity.pageCount, 128)
        let part = try URPart(cbor: Bytewords.decode(try XCTUnwrap(frame.ur.split(separator: "/").last)))
        XCTAssertEqual(part.sequence, UInt32.max)
        XCTAssertEqual(part.count, 128)
        XCTAssertEqual(part.payload.count, 256)
        let qr = try FixedQRCode(text: text)
        XCTAssertEqual(qr.version, 22)
        XCTAssertEqual(qr.moduleSide, 105)
        XCTAssertEqual(qr.profile.rasterSide, 113)
        XCTAssertEqual(qr.profile.maximumTextBytes, 767)
        XCTAssertEqual(qr.profile.quietZoneModules, 4)
        XCTAssertEqual(qr.profile.correctionLevel, "Q")
        try assertFormatIsQ(qr)
        XCTAssertThrowsError(try FixedQRCode(text: text + "A")) {
            XCTAssertEqual($0 as? FTError, .limit("qr.profile_capacity"))
        }
    }

    func testActualSenderKeepsOneVersionAcrossPagesAndAllSequenceWidths() throws {
        for size in [100, 32750, 70000, FTLimit.envelope - 8192] {
            let sender = try OpticalSender(envelope: envelope(size: size), using: crypto)
            let pages = Set([0, sender.identity.pageCount - 1, max(0, sender.identity.pageCount - 2)])
            for page in pages.sorted() {
                for sequence in [UInt32(1), 255, 256, 65535, 65536, UInt32.max] {
                    let text = try sender.frame(page: page, sequence: sequence, using: crypto)
                    XCTAssertLessThanOrEqual(text.utf8.count, 763)
                    let qr = try FixedQRCode(text: text)
                    XCTAssertEqual(qr.version, 22)
                    XCTAssertEqual(qr.moduleSide, 105)
                    try assertFormatIsQ(qr)
                }
            }
            if sender.identity.pageCount == 128 {
                let longest = try sender.frame(page: 126, sequence: UInt32.max, using: crypto)
                XCTAssertEqual(longest.utf8.count, 763, "The bound includes three-digit page/part counts and ten-digit repair sequence.")
            }
        }
    }

    func testRasterPreservesWholeModulesAndQuietZoneAtEveryPresentationSize() throws {
        let qr = try FixedQRCode(text: maximumProfileFrame())
        for side in [113, 226, 341, 452, 1024] {
            let raster = try qr.raster(pixelSide: side)
            XCTAssertEqual(raster.pixelSide, side)
            XCTAssertEqual(raster.moduleScale, side / 113)
            XCTAssertEqual(raster.pixels.count, side * side)
            let scale = raster.moduleScale, margin = (side - 105 * scale) / 2
            var wholeModules = true, whiteBorder = true
            for y in 0..<side {
                for x in 0..<side {
                    let value = raster.pixels[y * side + x]
                    if x < margin || y < margin || x >= margin + 105 * scale || y >= margin + 105 * scale {
                        whiteBorder = whiteBorder && value == 255
                    } else {
                        let expected: UInt8 = qr.isDark(x: (x - margin) / scale, y: (y - margin) / scale) ? 0 : 255
                        wholeModules = wholeModules && value == expected
                    }
                }
            }
            XCTAssertTrue(whiteBorder, "No dark pixel enters the quiet zone at \(side) pixels.")
            XCTAssertTrue(wholeModules, "No module is resampled or clipped at \(side) pixels.")
            XCTAssertGreaterThanOrEqual(margin, 4 * scale)
            XCTAssertGreaterThanOrEqual(side - margin - 105 * scale, 4 * scale)
        }
        XCTAssertEqual(try qr.raster().pixelSide, 113)
        XCTAssertEqual(try qr.raster(pixelSide: 2048).pixels.count, 2048 * 2048)
        for side in [Int.min, -1, 0, 112, 2049, Int.max] { XCTAssertThrowsError(try qr.raster(pixelSide: side)) }
        for (x, y) in [(Int.min, 0), (-1, 0), (0, -1), (105, 0), (0, 105), (Int.max, Int.max)] {
            XCTAssertFalse(qr.isDark(x: x, y: y))
        }
    }

    func testProfileRejectsNonFramingAndByteModeAlphabetWithoutChangingCapacity() throws {
        let valid = maximumProfileFrame()
        let prefix = try XCTUnwrap(valid.range(of: "UR:BYTES/"))
        let wrapper = String(valid[..<prefix.lowerBound])
        for text in ["", "https://example.invalid", valid.lowercased(), wrapper + "UR:BYTES/Aa", wrapper + "UR:BYTES/A_A",
                     wrapper + "UR:BYTES/A\0A", wrapper + "UR:BYTES/ÉA", wrapper + "UR:BYTES/A?A"] {
            XCTAssertThrowsError(try FixedQRCode(text: text))
        }
    }

    func testIndependentDecoderFixturesCoverMaximumFramesAndACompleteWorkspace() throws {
        var cases: [DecoderCase] = []
        for side in [113, 226, 452, 1024] {
            cases.append(.init(name: "maximum-767-\(side)", text: maximumProfileFrame(), pixelSide: side))
        }
        let large = try OpticalSender(envelope: envelope(size: FTLimit.envelope - 8192), using: crypto)
        let longest = try large.frame(page: 126, sequence: UInt32.max, using: crypto)
        XCTAssertEqual(longest.utf8.count, 763)
        for side in [113, 454, 1024] { cases.append(.init(name: "sender-763-\(side)", text: longest, pixelSide: side)) }

        let archive = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [], using: crypto)
        let exported = try FTAG.seal(archive, using: crypto)
        let sender = try OpticalSender(envelope: exported.envelope, using: crypto)
        var receiver = OpticalReceiver(selecting: try OpticalFrame(text: sender.frame(page: 0, sequence: 1, using: crypto)))
        var recoveryNames: [String] = []
        for page in 0..<sender.identity.pageCount {
            for sequence in 1...(try sender.sourcePartCount(page: page)) {
                let text = try sender.frame(page: page, sequence: UInt32(sequence), using: crypto)
                let name = "workspace-\(page)-\(sequence)"
                cases.append(.init(name: name, text: text, pixelSide: 452)); recoveryNames.append(name)
                try receiver.receive(OpticalFrame(text: text), using: crypto)
            }
        }
        XCTAssertTrue(receiver.isComplete)
        XCTAssertEqual(try receiver.envelope(using: crypto), exported.envelope)
        XCTAssertEqual(try FTAG.open(receiver.envelope(using: crypto), code: exported.code, using: crypto).workspace.raw, archive.workspace.raw)
        for item in cases {
            let qr = try FixedQRCode(text: item.text)
            try assertFormatIsQ(qr)
            XCTAssertEqual(try qr.raster(pixelSide: item.pixelSide).pixelSide, item.pixelSide)
        }
        // The optional independent gate consumes production raster pixels,
        // encodes PNGs with Pillow, and decodes with zxing-cpp, never this encoder.
        if let directory = ProcessInfo.processInfo.environment["FT_QR_VALIDATION_DIRECTORY"] {
            guard directory.hasPrefix("/") else { throw FTError.malformed("test.qr_artifact_directory") }
            let root = URL(fileURLWithPath: directory, isDirectory: true)
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
            var records: [[String: Any]] = []
            for item in cases {
                let raster = try FixedQRCode(text: item.text).raster(pixelSide: item.pixelSide)
                let filename = item.name + ".pgm"
                let pgm = Data("P5\n\(raster.pixelSide) \(raster.pixelSide)\n255\n".utf8) + raster.pixels
                try pgm.write(to: root.appendingPathComponent(filename), options: .withoutOverwriting)
                records.append(["name": item.name, "text": item.text, "pgm": filename,
                                "pixelSide": raster.pixelSide, "moduleScale": raster.moduleScale])
            }
            let manifest: [String: Any] = ["version": 22, "correctionLevel": "Q", "moduleSide": 105,
                "rasterSide": 113, "cases": records, "recoveryFrameNames": recoveryNames,
                "recoveredEnvelopeSHA256": crypto.sha256(exported.envelope).ftHex]
            try JSONSerialization.data(withJSONObject: manifest, options: [.sortedKeys, .prettyPrinted])
                .write(to: root.appendingPathComponent("manifest.json"), options: .withoutOverwriting)
        }
    }

    private struct DecoderCase { let name: String, text: String, pixelSide: Int }

    private func maximumProfileFrame() -> String {
        // Conservative public-frame bound, including a 256-byte nominal part.
        // Current FTAG padding produces <=255-byte parts, tested separately.
        let part = URPart(sequence: UInt32.max, count: 128, messageLength: 32768,
                          checksum: UInt32.max, payload: Data(repeating: 255, count: 256))
        return "FTQR1/" + String(repeating: "F", count: 32) + "/4194304/127-128/"
            + String(repeating: "F", count: 64) + "/" + String(repeating: "F", count: 64)
            + "/UR:BYTES/4294967295-128/" + Bytewords.encode(part.encoded())
    }

    private func envelope(size: Int) throws -> Data {
        try FTAG.sealArchiveBytes(Data(repeating: 0xA5, count: size),
            code: TransferCode(secret: Data(repeating: 0x11, count: 16)), transferID: Data(repeating: 0x22, count: 16),
            salt: Data(repeating: 0x33, count: 16), nonce: Data(repeating: 0x44, count: 12), compress: false, using: crypto)
    }

    private func assertFormatIsQ(_ qr: FixedQRCode, file: StaticString = #filePath, line: UInt = #line) throws {
        // Independently read both QR format-information copies, undo its mask,
        // check BCH(15,5), and inspect the encoded ECC bits (Q = binary 11).
        let first = (0..<6).map { (8, $0) } + [(8, 7), (8, 8), (7, 8)] + (9..<15).map { (14 - $0, 8) }
        let second = (0..<8).map { (qr.moduleSide - 1 - $0, 8) } + (8..<15).map { (8, qr.moduleSide - 15 + $0) }
        func value(_ positions: [(Int, Int)]) -> Int {
            positions.enumerated().reduce(0) { $0 | (qr.isDark(x: $1.element.0, y: $1.element.1) ? 1 << $1.offset : 0) }
        }
        let encoded = value(first)
        XCTAssertEqual(encoded, value(second), file: file, line: line)
        let unmasked = encoded ^ 0x5412
        var remainder = unmasked
        for bit in stride(from: 14, through: 10, by: -1) where remainder & (1 << bit) != 0 { remainder ^= 0x537 << (bit - 10) }
        XCTAssertEqual(remainder, 0, file: file, line: line)
        XCTAssertEqual(unmasked >> 13, 3, "ECC must remain Q even when a shorter payload could fit H.", file: file, line: line)
    }
}
