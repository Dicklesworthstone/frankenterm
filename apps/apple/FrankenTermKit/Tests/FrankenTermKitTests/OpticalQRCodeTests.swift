import Foundation
import XCTest
@testable import FrankenTermKit

private enum FixedQRFixtures {
    // Synthetic public framing: the final page of a 4 MiB-minus-three-byte
    // envelope has 32,765 bytes. Its 32,768-byte CBOR message needs 128 complete
    // 256-byte fragments. UInt32.max sequence/checksum attain the full 767-byte
    // presentation bound; no real ciphertext or private material is included.
    static let transfer = "FTQR1/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA/4194301/127-128/"
        + "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB/"
        + "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC/"
        + "UR:BYTES/4294967295-128/"
        + "LPCYZMZMZMZMCSLACFLAAECYZMZMZMZMHKADAEAEADAOAXAAAHAMATAYASBKBDBNBTBABSBEBYBGBWBBBZCMCHCSCFCYCWCECACKCTCXCLCPCNDKDADSDIDEDTDRDNDWDPDMDLDYEHEYEOEEECENEMETESFTFRFNFSFMFHFZFPFWFXFYFEFGFLFDGAGEGRGSGTGLGWGDGYGMGUGHGOHFHGHDHKHTHPHHHLHYHEHNHSIDIAIEIHIYIOISINIMJEJZJNJTJLJOJSJPJKJYKPKOKTKSKKKNKGKEKIKBLBLALYLFLSLRLPLNLTLOLDLELULKLGMNMYMHMEMOMUMWMDMTMSMKNLNYNDNSNTNNNENBOYOEOTOXONOLOSPDPTPKPYPSPMPLPEPFPAPRQDQZRERPRLRORHRDRKRFRYRNRSRTSESASRSSSKSWSTSPSOSGSBSFSNTOTKTITTTDTETYTLTBTSTPTATNUYUOUTUEURVTVYVOVLVEVWVAVDVSWLWDWMWPWEWYWSWTWNWZWFWKYKYNYLYAYTZSZOZTZCZEZMHNSAVTSA"
    static let recipient = "FTREQ1:UR:BYTES/HDFTLRADGDBYBYBYBYBYBYBYBYBYBYBYBYBYBYBYBYHDCXCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCPCYAEFZAEAEWPDKPYBK"

    // Independent python-qrcode 8.2 fixtures (https://github.com/lincolnloop/python-qrcode).
    // For each mask 0...7: QRCode(version=22, error_correction=ERROR_CORRECT_Q,
    // border=0, mask_pattern=mask); add_data(QRData(text, MODE_ALPHA_NUM),
    // optimize=0); make(fit=False). SHA-256 covers 105x105 row-major bytes,
    // dark=0 and light=255. Accept any valid mask; never derive expectations
    // from the production encoder. ZXing-C++ 2.3.0 independently decoded all
    // sixteen matrices back to the exact texts above during fixture generation.
    static let transferMatrixHashes: Set<String> = [
        "C2AA55A6F82E59012D004A61B7E4158192BF05CDF0574F38D6A32FAE0E1B7562",
        "7D74FD12A53FFC76BDE98C11A87030C98E66116509371D4004FBD102E70C2A67",
        "E6C052F22FBED66FDB8540803DF65355B1C030BD51E200346FC0C848C49C2A51",
        "27C87F1A2E27070DCD92F00E8BEB5DCB6D03EA44DA3D3539B38602948B9F15AF",
        "9B152CA0D7B012B99BA57316326F002223FFA7B883BA3ADC7CD6AAC2D38BC623",
        "85BD8A35B8167F8F70CD1EC3385389C3DFC8B5AAE82D55418E394C22E522DF8F",
        "1CC93CFBE7E8983F932D861D89A876C51EC983CEA04EC73A5DA0812EE1BB52B8",
        "C75C133172DD528AF7760E4D4C72194A4A51246A03C0965C0A2CD3FE085144C5"
    ]
    static let recipientMatrixHashes: Set<String> = [
        "FBE5673C27DD63EE537F0AC906C7BD804C359C513CDB48F44095C8260088D110",
        "02A7AE617064F5D2F8D3380F9F3F1CE828A053CF0125D8297F1B31AD0AC487A3",
        "FCA4409DA716BD9A8B619EA14A8D94F7594EC59BD477510124FEFD724ACBDB7B",
        "2E72173ED4180CDBA962CBA651A9429414F61A99B0D0E7286F9BE058CB689AFA",
        "7FE41E80C96DA4FA4E7117A560A0E819FC1C4004F88C4AC98262ED633374DA8C",
        "BF2B8B79FFE83B4195F2125370C2ACD3BD3890080E345A967C3CE7D579250E9D",
        "9A27E7C53C53131AD7F4C92BEFCFF4FFFF751D73D701BF383740359AF8D3B350",
        "2C9F659A426907E82D0EE00E47F2405E17B65A6353397B3373181B3BF81BB05B"
    ]
}

final class OpticalQRCodeTests: XCTestCase {
    private let crypto = TestCryptography()

    private func matrixBytes(_ qr: FixedQRCode) -> Data {
        Data((0..<(qr.moduleSide * qr.moduleSide)).map {
            qr.isDark(x: $0 % qr.moduleSide, y: $0 / qr.moduleSide) ? UInt8(0) : UInt8(255)
        })
    }

    func testMaximumUInt32FrameMatchesIndependentFixedVersionQuartileMatrix() throws {
        let frame = try OpticalFrame(text: FixedQRFixtures.transfer)
        let part = try URPart(cbor: Bytewords.decode(try XCTUnwrap(frame.ur.split(separator: "/").last)))
        XCTAssertEqual(frame.expectedPageLength, 32_765)
        XCTAssertEqual(part.messageLength, frame.expectedPageLength + 3)
        XCTAssertEqual(part.count, 128)
        XCTAssertEqual(part.payload, Data(0...255))
        XCTAssertEqual(part.sequence, UInt32.max)
        XCTAssertEqual(part.checksum, UInt32.max)
        XCTAssertEqual(FixedQRFixtures.transfer.utf8.count, 767)
        XCTAssertEqual(FixedQRFixtures.transfer.utf8.count, FixedQRCodeProfile.ftqr1.maximumTextBytes)
        let matrix = try FixedQRCode(text: FixedQRFixtures.transfer)
        XCTAssertEqual(matrix.moduleSide, 105)
        let bytes = matrixBytes(matrix)
        XCTAssertEqual(bytes.count, 105 * 105)
        XCTAssertTrue(FixedQRFixtures.transferMatrixHashes.contains(crypto.sha256(bytes).ftHex))
    }

    func testRecipientRequestMatchesIndependentFixedVersionQuartileMatrix() throws {
        let request = try RecipientRequest(text: FixedQRFixtures.recipient)
        XCTAssertEqual(request.encoded.count, 58)
        XCTAssertEqual(request.requestID, Data(repeating: 0x11, count: 16))
        XCTAssertEqual(request.publicKey, Data(repeating: 0x22, count: 32))
        XCTAssertEqual(request.maximumEnvelopeBytes, FTLimit.envelope)
        XCTAssertEqual(request.text, FixedQRFixtures.recipient)
        let matrix = try FixedQRCode(recipientRequest: request)
        XCTAssertEqual(matrix.moduleSide, 105)
        XCTAssertTrue(FixedQRFixtures.recipientMatrixHashes.contains(crypto.sha256(matrixBytes(matrix)).ftHex))
    }

    func testVersionDoesNotChangeAcrossFragmentOrSequenceBoundaries() throws {
        // Include single-part UR, the bstr/header boundaries around 256 bytes,
        // both maximal fragment geometries, and every CBOR sequence-width step.
        let sequences: [UInt32] = [1, 23, 24, 255, 256, 65_535, 65_536, .max]
        for pageLength in [1, 253, 254, 256, 32_765, 32_768] {
            let page = URPageEncoder(page: Data(repeating: 7, count: pageLength))
            let prefix = "FTQR1/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA/\(FTLimit.page + pageLength)/1-2/"
                + String(repeating: "B", count: 64) + "/" + String(repeating: "C", count: 64) + "/"
            for sequence in sequences {
                let text = try prefix + page.text(sequence: sequence, using: crypto)
                XCTAssertLessThanOrEqual(text.utf8.count, FixedQRCodeProfile.ftqr1.maximumTextBytes)
                let matrix = try FixedQRCode(text: text)
                XCTAssertEqual(matrix.moduleSide, 105)
                XCTAssertEqual(try matrix.raster().pixelSide, 113)
            }
        }
    }

    func testEveryRasterPixelHasIntegerScaleAndFourModuleWhiteMargin() throws {
        let matrix = try FixedQRCode(text: FixedQRFixtures.transfer)
        let modules = matrixBytes(matrix)
        for side in [113, 114, 226, 1024, 2048] {
            let raster = try matrix.raster(pixelSide: side)
            let origin = (side - matrix.moduleSide * raster.moduleScale) / 2
            XCTAssertEqual(raster.pixelSide, side)
            XCTAssertEqual(raster.pixels.count, side * side)
            XCTAssertEqual(raster.moduleScale, side / 113)
            XCTAssertGreaterThanOrEqual(origin, 4 * raster.moduleScale)
            XCTAssertGreaterThanOrEqual(side - origin - 105 * raster.moduleScale, 4 * raster.moduleScale)
            XCTAssertLessThanOrEqual(abs(side - 105 * raster.moduleScale - 2 * origin), 1)
            XCTAssertTrue(raster.pixels.enumerated().allSatisfy { index, value in
                let x = index % side - origin, y = index / side - origin
                guard x >= 0, y >= 0, x < 105 * raster.moduleScale, y < 105 * raster.moduleScale else { return value == 255 }
                return value == modules[(y / raster.moduleScale) * 105 + x / raster.moduleScale]
            }, "All modules must remain binary uniform squares, with no interpolation or quiet-zone contamination")
        }
        let recording = try matrix.raster(pixelSide: 1024)
        XCTAssertEqual(recording.moduleScale, 9)
        XCTAssertEqual((recording.pixelSide - matrix.moduleSide * recording.moduleScale) / 2, 39)
        for side in [Int.min, -1, 0, 112, 2049, Int.max] {
            XCTAssertThrowsError(try matrix.raster(pixelSide: side))
        }
    }

    func testRendererDoesNotAcceptOtherContentOrTruncatedCapacity() throws {
        for text in ["", "https://example.invalid/", "private key text", "FT1:TRANSFER-CODE",
                     FixedQRFixtures.recipient, FixedQRFixtures.transfer.lowercased(),
                     FixedQRFixtures.transfer + "\0", FixedQRFixtures.transfer + "A",
                     FixedQRFixtures.transfer + String(repeating: "A", count: FTLimit.qrText)] {
            XCTAssertThrowsError(try FixedQRCode(text: text))
        }
        for text in ["", FixedQRFixtures.transfer, FixedQRFixtures.recipient.lowercased(),
                     FixedQRFixtures.recipient + "AE", "FTREQ1:UR:BYTES/1-2/AEAEAEAEAE",
                     "FTREQ1:UR:BYTES/" + String(repeating: "A", count: 4096)] {
            XCTAssertThrowsError(try FixedQRCode(recipientRequest: RecipientRequest(text: text)))
        }
    }
}

#if canImport(CoreGraphics) && canImport(Vision) && canImport(CryptoKit)
import CoreGraphics
import ImageIO
import Vision

/// These execute only with an Apple SDK. Linux fixtures qualify the portable
/// matrix/raster; they do not claim to qualify Vision, a camera, or H.264.
@MainActor
final class AppleFixedQRCodeTests: XCTestCase {
    func testNativeTransferAndRecipientPNGsHaveIdenticalGeometryAndDecode() async throws {
        let renderer = NativeOpticalPresentation()
        let transfers = [
            (try await renderer.symbol(text: FixedQRFixtures.transfer), FixedQRFixtures.transfer),
            (try await renderer.recipientSymbol(text: FixedQRFixtures.recipient), FixedQRFixtures.recipient)
        ]
        for (symbol, text) in transfers {
            XCTAssertEqual(symbol.moduleSide, 113)
            let source = try XCTUnwrap(CGImageSourceCreateWithData(symbol.png as CFData, nil))
            let image = try XCTUnwrap(CGImageSourceCreateImageAtIndex(source, 0, nil))
            XCTAssertEqual(image.width, 113); XCTAssertEqual(image.height, 113)
            // Decode the actual native PNG at an integer display scale, keeping
            // its included quiet zone; no independent raster bypasses the PNG.
            let side = 113 * 9
            let canvas = try XCTUnwrap(CGContext(data: nil, width: side, height: side, bitsPerComponent: 8,
                bytesPerRow: side, space: CGColorSpaceCreateDeviceGray(), bitmapInfo: CGImageAlphaInfo.none.rawValue))
            canvas.interpolationQuality = .none; canvas.setShouldAntialias(false)
            canvas.draw(image, in: CGRect(x: 0, y: 0, width: side, height: side))
            let scaled = try XCTUnwrap(canvas.makeImage())
            let request = VNDetectBarcodesRequest(); request.symbologies = [.qr]
            try VNImageRequestHandler(cgImage: scaled, options: [:]).perform([request])
            let results = try XCTUnwrap(request.results)
            XCTAssertEqual(results.count, 1)
            XCTAssertEqual(results.first?.payloadStringValue, text)
        }
    }
}
#endif
