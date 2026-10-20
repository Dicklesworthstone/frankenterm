import Foundation

/// Finite recordings contain every source symbol plus the planned repair set.
/// Replaying this schedule cannot create new fountain information.
public struct OpticalAnimationPlan: Sendable {
    public struct Address: Sendable, Equatable { public let page: Int, sequence: UInt32 }
    public let addresses: [Address]
    public init(sender: OpticalSender, maximumFrames: Int = 4096) throws {
        guard maximumFrames > 0, maximumFrames <= 4096 else { throw FTError.limit("animation.frames") }
        let counts = try (0..<sender.identity.pageCount).map { page in
            let n = try sender.sourcePartCount(page: page)
            return n == 1 ? 1 : n + (n + 1) / 2 + 16
        }
        let total = counts.reduce(0, +)
        guard total <= maximumFrames else { throw FTError.limit("animation.use_live_transfer") }
        var positions = Array(repeating: 0, count: counts.count), result: [Address] = []
        result.reserveCapacity(total)
        while result.count < total {
            for page in counts.indices {
                for _ in 0..<min(8, counts[page] - positions[page]) {
                    positions[page] += 1
                    result.append(.init(page: page, sequence: UInt32(positions[page])))
                }
            }
        }
        addresses = result
    }
}

/// Reads only an explicitly selected, already-local regular file, with a hard
/// byte bound. Cloud/provider downloads are not initiated by this helper.
public enum SelectedLocalFile {
    public static func read(_ url: URL, limit: Int) throws -> Data {
        guard url.isFileURL, limit > 0, limit <= FTLimit.archive else { throw FTError.limit("file.input") }
        #if os(iOS) || os(macOS)
        let access = url.startAccessingSecurityScopedResource()
        defer { if access { url.stopAccessingSecurityScopedResource() } }
        let cloud = try url.resourceValues(forKeys: [.isUbiquitousItemKey, .ubiquitousItemDownloadingStatusKey])
        if cloud.isUbiquitousItem == true, cloud.ubiquitousItemDownloadingStatus != .current {
            throw FTError.unsupported("file.not_local")
        }
        #endif
        let values = try url.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey])
        guard values.isRegularFile == true, let size = values.fileSize, size <= limit else { throw FTError.limit("file.bytes") }
        let handle = try FileHandle(forReadingFrom: url)
        defer { try? handle.close() }
        var result = Data()
        while let bytes = try handle.read(upToCount: min(65536, limit + 1 - result.count)), !bytes.isEmpty {
            try Task.checkCancellation()
            result += bytes
            guard result.count <= limit else { throw FTError.limit("file.bytes") }
        }
        return result
    }
}

#if canImport(CoreImage) && canImport(Vision) && canImport(CryptoKit)
import CoreImage
import CoreImage.CIFilterBuiltins
import CoreGraphics
import ImageIO
import UniformTypeIdentifiers
import Vision

public struct QRCodeSymbol: Sendable {
    /// One pixel per module, including an explicit four-module white border.
    /// Native display must use integer backing-pixel scaling and no interpolation.
    public let png: Data, moduleSide: Int
}

/// Native image objects and their buffers remain isolated to this actor. Only
/// owned PNG data crosses to the UI. No screenshot or capture API is involved.
public actor NativeOpticalPresentation {
    private let context = CIContext(options: [.cacheIntermediates: false])
    private let crypto = AppleCryptography()
    public init() {}

    public func symbol(text: String) throws -> QRCodeSymbol {
        let image = try raster(text: text, pixelSide: nil)
        let bytes = NSMutableData()
        guard let encoder = CGImageDestinationCreateWithData(bytes, UTType.png.identifier as CFString, 1, nil) else { throw FTError.storage }
        CGImageDestinationAddImage(encoder, image, nil)
        guard CGImageDestinationFinalize(encoder) else { throw FTError.storage }
        return .init(png: bytes as Data, moduleSide: image.width)
    }
    private func raster(text: String, pixelSide: Int?) throws -> CGImage {
        guard text.utf8.count <= FTLimit.qrText, text.hasPrefix("FTQR1/"), text.utf8.allSatisfy({ $0 < 128 }) else {
            throw FTError.malformed("qr.presentation")
        }
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(text.utf8); filter.correctionLevel = "Q"
        guard let output = filter.outputImage,
              let code = context.createCGImage(output, from: output.extent) else { throw FTError.unsupported("qr.capacity") }
        guard code.width == code.height, code.width <= 185 else { throw FTError.limit("qr.modules") }
        let modules = code.width + 8, side = pixelSide ?? modules
        guard side >= modules, side <= 2048 else { throw FTError.limit("qr.pixels") }
        let scale = side / modules, quiet = 4 * scale
        guard let canvas = CGContext(data: nil, width: side, height: side, bitsPerComponent: 8,
                                     bytesPerRow: side, space: CGColorSpaceCreateDeviceGray(), bitmapInfo: CGImageAlphaInfo.none.rawValue) else {
            throw FTError.storage
        }
        canvas.setFillColor(gray: 1, alpha: 1)
        canvas.fill(CGRect(x: 0, y: 0, width: side, height: side))
        canvas.interpolationQuality = .none; canvas.setShouldAntialias(false)
        let margin = (side - modules * scale) / 2 + quiet
        canvas.draw(code, in: CGRect(x: margin, y: margin, width: code.width * scale, height: code.height * scale))
        guard let image = canvas.makeImage() else { throw FTError.storage }
        return image
    }

    /// A lossless animated PNG is the initial saved optical recording. The MP4
    /// lane remains separate. Frames are rendered from ciphertext only, then every
    /// encoded frame is decoded and compared before publication at the chosen URL.
    /// Existing destinations are never overwritten by this API.
    public func saveAnimation(envelope: Data, to destination: URL, symbolsPerSecond: Int = 5) throws -> Int {
        guard destination.isFileURL, [3, 5, 10].contains(symbolsPerSecond) else { throw FTError.malformed("animation.options") }
        let sender = try OpticalSender(envelope: envelope, using: crypto)
        // Image I/O does not promise constant-memory encoding. Bound the entire
        // raw grayscale input to 256 MiB, not just the final compressed file.
        let plan = try OpticalAnimationPlan(sender: sender, maximumFrames: 256)
        let access = destination.startAccessingSecurityScopedResource()
        defer { if access { destination.stopAccessingSecurityScopedResource() } }
        guard !FileManager.default.fileExists(atPath: destination.path) else { throw FTError.unsupported("animation.choose_new_filename") }
        let temporary = destination.deletingLastPathComponent().appendingPathComponent(".ft-" + UUID().uuidString + ".partial")
        try Data().write(to: temporary, options: .withoutOverwriting)
        var published = false
        defer { if !published { try? FileManager.default.removeItem(at: temporary) } }
        guard let writer = CGImageDestinationCreateWithURL(temporary as CFURL, UTType.png.identifier as CFString, plan.addresses.count, nil) else {
            throw FTError.storage
        }
        CGImageDestinationSetProperties(writer, [kCGImagePropertyPNGDictionary as String: [kCGImagePropertyAPNGLoopCount as String: 0]] as CFDictionary)
        let delay = 1.0 / Double(symbolsPerSecond)
        let frameOptions = [kCGImagePropertyPNGDictionary as String: [kCGImagePropertyAPNGDelayTime as String: delay,
                                                                     kCGImagePropertyAPNGUnclampedDelayTime as String: delay]] as CFDictionary
        for address in plan.addresses {
            try Task.checkCancellation()
            try autoreleasepool {
                let text = try sender.frame(page: address.page, sequence: address.sequence, using: crypto)
                CGImageDestinationAddImage(writer, try raster(text: text, pixelSide: 1024), frameOptions)
            }
        }
        guard CGImageDestinationFinalize(writer) else { throw FTError.storage }
        let file = try FileHandle(forWritingTo: temporary)
        try file.synchronize(); try file.close()
        let size = try temporary.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? Int.max
        guard size <= 128 * 1024 * 1024 else { throw FTError.limit("animation.bytes") }
        guard let source = CGImageSourceCreateWithURL(temporary as CFURL, [kCGImageSourceShouldCache as String: false] as CFDictionary),
              CGImageSourceGetCount(source) == plan.addresses.count else { throw FTError.malformed("animation.frame_count") }
        let properties = CGImageSourceCopyProperties(source, nil) as? [String: Any]
        let png = properties?[kCGImagePropertyPNGDictionary as String] as? [String: Any]
        guard (png?[kCGImagePropertyAPNGLoopCount as String] as? Int) == 0 else { throw FTError.malformed("animation.loop") }
        var receiver: OpticalReceiver?
        for (index, address) in plan.addresses.enumerated() {
            try Task.checkCancellation()
            try autoreleasepool {
                guard let image = CGImageSourceCreateImageAtIndex(source, index, [kCGImageSourceShouldCache as String: false] as CFDictionary) else {
                    throw FTError.malformed("animation.decode")
                }
                let request = VNDetectBarcodesRequest(); request.symbologies = [.qr]
                try VNImageRequestHandler(cgImage: image, options: [:]).perform([request])
                let expected = try sender.frame(page: address.page, sequence: address.sequence, using: crypto)
                guard request.results?.contains(where: { $0.payloadStringValue == expected }) == true else { throw FTError.checksum }
                let properties = CGImageSourceCopyPropertiesAtIndex(source, index, nil) as? [String: Any]
                let png = properties?[kCGImagePropertyPNGDictionary as String] as? [String: Any]
                let actualDelay = (png?[kCGImagePropertyAPNGUnclampedDelayTime as String] ?? png?[kCGImagePropertyAPNGDelayTime as String]) as? Double
                guard let actualDelay, abs(actualDelay - delay) < 0.002 else { throw FTError.malformed("animation.timing") }
                let frame = try OpticalFrame(text: expected)
                if receiver == nil { receiver = OpticalReceiver(selecting: frame) }
                try receiver?.receive(frame, using: crypto)
            }
        }
        guard try receiver?.envelope(using: crypto) == envelope else { throw FTError.checksum }
        try Task.checkCancellation()
        // The final name appears only after full optical round-trip validation.
        try FileManager.default.moveItem(at: temporary, to: destination)
        published = true
        return plan.addresses.count
    }
}
#endif
