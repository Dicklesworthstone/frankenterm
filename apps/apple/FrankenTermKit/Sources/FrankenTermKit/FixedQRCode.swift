import Foundation
import CQRCodeGen

/// The FTQR1 presentation profile is fixed for the entire encrypted transfer,
/// including source replay, long-running repair, and either recording format.
/// FTQR1's uppercase alphabet fits one QR alphanumeric segment. Its largest
/// wrapper is 185 bytes, its multipart UR prefix is 24 bytes (UInt32.max and
/// three-digit part counts), and a 256-byte fragment plus CBOR and CRC is at
/// most 558 Bytewords characters: 767 in total. The current padded FTAG sender
/// has a tighter 763-character maximum because its nominal parts are <=255.
/// Version 22 is the smallest version that holds either bound at level Q.
public struct FixedQRCodeProfile: Sendable, Equatable {
    public static let ftqr1 = Self(version: 22, maximumTextBytes: 767)

    public let version: Int
    public let maximumTextBytes: Int
    public var correctionLevel: String { "Q" }
    public var moduleSide: Int { version * 4 + 17 }
    public var quietZoneModules: Int { 4 }
    public var rasterSide: Int { moduleSide + quietZoneModules * 2 }

    private init(version: Int, maximumTextBytes: Int) {
        self.version = version
        self.maximumTextBytes = maximumTextBytes
    }
}

/// An owned QR matrix with its origin at the top left, excluding the quiet
/// zone. The maintained encoder never selects another version or boosts ECC.
/// No native image object, mutable C buffer, or borrowed pointer escapes.
public struct FixedQRCode: Sendable {
    public let profile: FixedQRCodeProfile
    public var version: Int { profile.version }
    public var moduleSide: Int { profile.moduleSide }
    private let modules: [UInt8]

    public init(text: String, profile: FixedQRCodeProfile = .ftqr1) throws {
        guard text.utf8.count <= profile.maximumTextBytes else { throw FTError.limit("qr.profile_capacity") }
        _ = try OpticalFrame(text: text)
        // Reject byte-mode inputs, embedded NULs, and case changes instead of
        // silently changing the capacity assumptions behind this profile.
        guard text.withCString({ qrcodegen_isAlphanumeric($0) }), !text.utf8.contains(0) else {
            throw FTError.malformed("qr.profile_alphabet")
        }
        let side = profile.moduleSide
        let bufferSize = (side * side + 7) / 8 + 1
        var scratch = [UInt8](repeating: 0, count: bufferSize)
        var encoded = [UInt8](repeating: 0, count: bufferSize)
        let success = text.withCString { source in
            scratch.withUnsafeMutableBufferPointer { temporary in
                encoded.withUnsafeMutableBufferPointer { output in
                    qrcodegen_encodeText(source, temporary.baseAddress, output.baseAddress,
                        qrcodegen_Ecc_QUARTILE, Int32(profile.version), Int32(profile.version),
                        qrcodegen_Mask_AUTO, false)
                }
            }
        }
        guard success, encoded.withUnsafeBufferPointer({ Int(qrcodegen_getSize($0.baseAddress)) }) == side else {
            throw FTError.unsupported("qr.profile_capacity")
        }
        var matrix = [UInt8](repeating: 0, count: side * side)
        encoded.withUnsafeBufferPointer { source in
            for y in 0..<side {
                for x in 0..<side {
                    matrix[y * side + x] = qrcodegen_getModule(source.baseAddress, Int32(x), Int32(y)) ? 1 : 0
                }
            }
        }
        self.profile = profile
        modules = matrix
    }

    public func isDark(x: Int, y: Int) -> Bool {
        guard x >= 0, y >= 0, x < moduleSide, y < moduleSide else { return false }
        return modules[y * moduleSide + x] != 0
    }

    /// Integer scaling only, centered on a white canvas. A nil size gives one
    /// pixel per module plus the full four-module border. Odd leftover pixels
    /// belong to the outer margin; no module or finder pattern is resampled.
    public func raster(pixelSide: Int? = nil) throws -> FixedQRCodeRaster {
        let side = pixelSide ?? profile.rasterSide
        guard side >= profile.rasterSide, side <= 2048 else { throw FTError.limit("qr.pixels") }
        let scale = side / profile.rasterSide
        let margin = (side - moduleSide * scale) / 2
        guard margin >= profile.quietZoneModules * scale else { throw FTError.limit("qr.quiet_zone") }
        var pixels = [UInt8](repeating: 255, count: side * side)
        for y in 0..<moduleSide {
            for x in 0..<moduleSide where modules[y * moduleSide + x] != 0 {
                let left = margin + x * scale, top = margin + y * scale
                for row in top..<(top + scale) {
                    let start = row * side + left
                    for offset in 0..<scale { pixels[start + offset] = 0 }
                }
            }
        }
        return FixedQRCodeRaster(pixelSide: side, moduleScale: scale, pixels: Data(pixels))
    }
}

/// Row-major 8-bit grayscale, top row first. Native image/video adapters share
/// this exact raster, so profile, border, module alignment, and payload cannot
/// diverge between live playback and saved optical recordings.
public struct FixedQRCodeRaster: Sendable {
    public let pixelSide: Int
    public let moduleScale: Int
    public let pixels: Data
}
