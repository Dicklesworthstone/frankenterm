import Foundation
#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif

/// One immutable encrypted export, with separate ownership of its optional
/// generated code. Live frames and saved media read ciphertext only. A code
/// reveal retires the exact playback lease before returning private text.
public struct OpticalExportSession: Sendable, CustomDebugStringConvertible {
    public enum State: Equatable, Sendable { case paused, playing, revealingCode }
    public struct Playback: Equatable, Sendable {
        private let witness = UUID()
        fileprivate init() {}
    }
    public struct Frame: Sendable {
        public let text: String
        public let emittedSymbolCount: Int
        public let symbolsPerSecond: Int
    }

    public let id = UUID()
    public let envelope: Data
    public let mode: FTAGMode
    public var identity: OpticalFrame.Identity { sender.identity }
    public var hasTransferCode: Bool { code != nil }
    public private(set) var state = State.paused
    public private(set) var emittedSymbolCount = 0
    public private(set) var symbolsPerSecond = 5
    public private(set) var repairPage: Int?
    public var debugDescription: String { "OpticalExportSession([encrypted], [code redacted])" }

    private let code: TransferCode?
    private var sender: OpticalSender
    private var playback: Playback?

    /// A newly sealed code-protected transfer retains its generated code only
    /// in memory. `FTAG.Export` is minted by the actual envelope sealer.
    public init<C: AirGapCryptography>(export: FTAG.Export, using crypto: C) throws {
        try self.init(envelope: export.envelope, code: export.code, using: crypto)
        guard mode == .code else { throw FTError.malformed("export.code_mode") }
    }

    /// Reopen an already-encrypted file, or present a newly sealed recipient
    /// transfer. Parsing its framing does not authenticate its source or recover
    /// its code; those checks still belong to the phone's ordinary restore.
    public init<C: AirGapCryptography>(envelope: Data, using crypto: C) throws {
        try self.init(envelope: envelope, code: nil, using: crypto)
    }

    private init<C: AirGapCryptography>(envelope: Data, code: TransferCode?, using crypto: C) throws {
        try Task.checkCancellation()
        mode = try FTAGHeader(envelope: envelope).mode
        sender = try OpticalSender(envelope: envelope, using: crypto)
        self.envelope = envelope
        self.code = code
    }

    /// Starting, changing speed or selecting a repair page changes presentation
    /// only. Previous callbacks cannot emit or pause a newer playback lease.
    public mutating func beginPlayback(symbolsPerSecond rate: Int = 5, repairPage page: Int? = nil) throws -> Playback {
        try Task.checkCancellation()
        try configure(rate: rate, page: page)
        let lease = Playback()
        playback = lease; state = .playing
        return lease
    }

    public mutating func updatePlayback(_ lease: Playback, symbolsPerSecond rate: Int, repairPage page: Int?) throws {
        try Task.checkCancellation()
        guard state == .playing, playback == lease else { throw FTError.stale }
        try configure(rate: rate, page: page)
    }

    private mutating func configure(rate: Int, page: Int?) throws {
        guard [3, 5, 10].contains(rate) else { throw FTError.malformed("export.symbol_rate") }
        if let page, !(0..<identity.pageCount).contains(page) { throw FTError.malformed("qr.page") }
        symbolsPerSecond = rate; repairPage = page
    }

    public mutating func pause(_ lease: Playback) {
        guard playback == lease else { return }
        playback = nil; state = .paused
    }

    /// The caller covers its QR surface before requesting this text, hides it
    /// on deactivation, and never includes it in a recording or saved file.
    public mutating func revealTransferCode<C: AirGapCryptography>(using crypto: C) throws -> String {
        try Task.checkCancellation()
        guard let code else { throw FTError.unsupported("export.transfer_code_unavailable") }
        playback = nil; state = .revealingCode
        return code.reveal(using: crypto)
    }

    public mutating func next<C: AirGapCryptography>(_ lease: Playback, using crypto: C) throws -> Frame {
        try Task.checkCancellation()
        guard state == .playing, playback == lease else { throw FTError.stale }
        guard emittedSymbolCount < Int.max else { throw FTError.limit("qr.sequence") }
        let text = try sender.next(repairPage: repairPage, using: crypto)
        emittedSymbolCount += 1
        return .init(text: text, emittedSymbolCount: emittedSymbolCount, symbolsPerSecond: symbolsPerSecond)
    }
}

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

/// Reads one stable snapshot of an explicitly selected, already-local regular
/// file, with a hard byte bound. Cloud/provider downloads are not initiated.
/// URL metadata is only a preflight: admission and reads use the same opened
/// descriptor, so replacing a selected path cannot turn the import into a
/// blocking FIFO/device read or follow a newly substituted final symlink.
public enum SelectedLocalFile {
    public static func read(_ url: URL, limit: Int) throws -> Data {
        try read(url, limit: limit, operations: .system)
    }

    /// Fault injection retains real descriptors and filesystem admission, as in
    /// LocalExportFile. Native reads remain synchronous on their owning actor.
    struct Operations {
        var open: (String, Int32) -> Int32
        var read: (Int32, UnsafeMutableRawBufferPointer) -> Int
        var checkCancellation: () throws -> Void

        static var system: Self {
            Self(open: { path, flags in
                path.withCString {
                    #if canImport(Darwin)
                    Darwin.open($0, flags)
                    #else
                    Glibc.open($0, flags)
                    #endif
                }
            }, read: { descriptor, buffer in
                #if canImport(Darwin)
                Darwin.read(descriptor, buffer.baseAddress, buffer.count)
                #else
                Glibc.read(descriptor, buffer.baseAddress, buffer.count)
                #endif
            }, checkCancellation: { try Task.checkCancellation() })
        }
    }

    static func read(_ url: URL, limit: Int, operations: Operations) throws -> Data {
        guard url.isFileURL, limit > 0, limit <= FTLimit.archive else { throw FTError.limit("file.input") }
        guard !url.path.utf8.contains(0) else { throw FTError.malformed("file.path") }
        try operations.checkCancellation()
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
        try operations.checkCancellation()
        // O_NONBLOCK prevents an admitted regular path replaced by a FIFO from
        // hanging inside open before descriptor validation/cancellation can run.
        let descriptor = operations.open(url.path, O_RDONLY | O_CLOEXEC | O_NONBLOCK | O_NOFOLLOW)
        guard descriptor >= 0 else { throw FTError.storage }
        defer { _ = close(descriptor) }
        var initial = stat()
        guard fstat(descriptor, &initial) == 0 else { throw FTError.storage }
        guard initial.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG),
              initial.st_size >= 0, initial.st_size <= off_t(limit) else { throw FTError.limit("file.bytes") }

        var result = Data()
        result.reserveCapacity(Int(initial.st_size))
        var buffer = [UInt8](repeating: 0, count: min(65_536, limit + 1))
        var interruptions = 0
        while true {
            try operations.checkCancellation()
            let amount = min(buffer.count, limit + 1 - result.count)
            let received = buffer.withUnsafeMutableBytes { bytes in
                operations.read(descriptor, UnsafeMutableRawBufferPointer(rebasing: bytes[..<amount]))
            }
            if received < 0, errno == EINTR, interruptions < 64 {
                interruptions += 1
                continue
            }
            guard received >= 0, received <= amount else { throw FTError.storage }
            if received == 0 { break }
            result.append(contentsOf: buffer.prefix(received))
            guard result.count <= limit else { throw FTError.limit("file.bytes") }
        }
        try operations.checkCancellation()
        var completed = stat()
        guard fstat(descriptor, &completed) == 0 else { throw FTError.storage }
        #if canImport(Darwin)
        let unchangedTimes = initial.st_mtimespec.tv_sec == completed.st_mtimespec.tv_sec
            && initial.st_mtimespec.tv_nsec == completed.st_mtimespec.tv_nsec
            && initial.st_ctimespec.tv_sec == completed.st_ctimespec.tv_sec
            && initial.st_ctimespec.tv_nsec == completed.st_ctimespec.tv_nsec
        #else
        let unchangedTimes = initial.st_mtim.tv_sec == completed.st_mtim.tv_sec
            && initial.st_mtim.tv_nsec == completed.st_mtim.tv_nsec
            && initial.st_ctim.tv_sec == completed.st_ctim.tv_sec
            && initial.st_ctim.tv_nsec == completed.st_ctim.tv_nsec
        #endif
        guard completed.st_dev == initial.st_dev, completed.st_ino == initial.st_ino,
              completed.st_size == initial.st_size, result.count == Int(initial.st_size),
              unchangedTimes else { throw FTError.malformed("file.changed_during_read") }
        try operations.checkCancellation()
        return result
    }
}

#if canImport(CoreGraphics) && canImport(Vision) && canImport(CryptoKit)
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
    private let crypto = AppleCryptography()
    public init() {}

    public func symbol(text: String) throws -> QRCodeSymbol {
        try symbol(image: Self.raster(text: text, pixelSide: nil))
    }
    /// A public recipient request has a distinct parser and cannot be confused
    /// with a ciphertext transfer frame or an arbitrary string to display.
    public func recipientSymbol(text: String) throws -> QRCodeSymbol {
        let request = try RecipientRequest(text: text)
        return try symbol(image: Self.image(raster: FixedQRCode(recipientRequest: request).raster()))
    }
    /// The unlock code frame (see TransferCodeFrame), in the transfer's geometry.
    public func codeSymbol(text: String) throws -> QRCodeSymbol {
        try symbol(image: Self.image(raster: FixedQRCode(transferCodeFrame: text).raster()))
    }
    private func symbol(image: CGImage) throws -> QRCodeSymbol {
        let bytes = NSMutableData()
        guard let encoder = CGImageDestinationCreateWithData(bytes, UTType.png.identifier as CFString, 1, nil) else { throw FTError.storage }
        CGImageDestinationAddImage(encoder, image, nil)
        guard CGImageDestinationFinalize(encoder) else { throw FTError.storage }
        return .init(png: bytes as Data, moduleSide: image.width)
    }
    // Pure rasterization is static so the autorelease-pool closures in the
    // actor's loops never capture `self`. A closure capturing the actor is
    // actor-isolated, and passing it to the nonisolated `autoreleasepool` while
    // it holds native image objects is rejected by Swift 6 region isolation.
    private static func raster(text: String, pixelSide: Int?) throws -> CGImage {
        try image(raster: FixedQRCode(text: text).raster(pixelSide: pixelSide))
    }
    private static func image(raster: FixedQRCodeRaster) throws -> CGImage {
        guard let provider = CGDataProvider(data: raster.pixels as CFData),
              let image = CGImage(width: raster.pixelSide, height: raster.pixelSide, bitsPerComponent: 8,
                bitsPerPixel: 8, bytesPerRow: raster.pixelSide, space: CGColorSpaceCreateDeviceGray(),
                bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.none.rawValue), provider: provider,
                decode: nil, shouldInterpolate: false, intent: .defaultIntent) else {
            throw FTError.storage
        }
        return image
    }

    /// A lossless animated PNG is the initial saved optical recording. The MP4
    /// lane remains separate. Frames are rendered from ciphertext only, then every
    /// encoded frame is decoded and compared before publication at the chosen URL.
    /// Existing destinations are never overwritten by this API.
    public func saveAnimation(envelope: Data, to destination: URL, symbolsPerSecond: Int = 5) throws -> Int {
        guard destination.isFileURL, [3, 5, 10].contains(symbolsPerSecond) else { throw FTError.malformed("animation.options") }
        let crypto = self.crypto
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
                CGImageDestinationAddImage(writer, try Self.raster(text: text, pixelSide: 1024), frameOptions)
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
