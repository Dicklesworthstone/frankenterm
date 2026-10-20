import Foundation

/// A video accepts an immutable, already-encrypted FTAG envelope. There is no
/// archive, private-key, transfer-code, image, or window-capture input here.
public struct OpticalVideoSource: Sendable {
    let envelope: Data
    public var envelopeByteCount: Int { envelope.count }

    public init(envelope: Data) throws {
        _ = try FTAGHeader(envelope: envelope)
        self.envelope = envelope
    }
}

/// A finite movie uses an exact 30 fps timeline. Each symbol occupies 10, 6, or
/// 3 whole frames; exceeding a limit rejects the movie instead of truncating it.
public struct OpticalVideoPlan: Sendable {
    public static let framesPerSecond = 30
    public static let pixelSide = 1024
    public static let maximumSymbols = 1024
    public static let maximumVideoFrames = 9000
    public static let maximumVideoBytes = 256 * 1024 * 1024
    public static let maximumProcessingSeconds = 600

    public let addresses: [OpticalAnimationPlan.Address]
    public let symbolsPerSecond: Int
    public let framesPerSymbol: Int
    public var symbolCount: Int { addresses.count }
    public var frameCount: Int { symbolCount * framesPerSymbol }
    public var durationSeconds: Double { Double(frameCount) / Double(Self.framesPerSecond) }

    public init(sender: OpticalSender, symbolsPerSecond: Int = 5) throws {
        guard [3, 5, 10].contains(symbolsPerSecond) else { throw FTError.malformed("video.symbol_rate") }
        let animation = try OpticalAnimationPlan(sender: sender, maximumFrames: Self.maximumSymbols)
        let hold = Self.framesPerSecond / symbolsPerSecond
        guard animation.addresses.count * hold <= Self.maximumVideoFrames else { throw FTError.limit("video.use_live_transfer") }
        addresses = animation.addresses
        self.symbolsPerSecond = symbolsPerSecond
        framesPerSymbol = hold
    }
}

public struct OpticalVideoReceipt: Sendable {
    public let symbolCount: Int
    public let frameCount: Int
    public let symbolsPerSecond: Int
    public let byteCount: Int
    public var framesPerSecond: Int { OpticalVideoPlan.framesPerSecond }
    public var pixelSide: Int { OpticalVideoPlan.pixelSide }
    public var durationSeconds: Double { Double(frameCount) / Double(framesPerSecond) }
}

/// Consumes actual decoded payloads in presentation order. Timestamp units are
/// exact 1/30-second ticks; the native adapter rejects lossy time conversion.
/// The complete expected text set is bounded to 1024 * FTLimit.qrText bytes.
struct OpticalVideoVerifier: Sendable {
    let source: OpticalVideoSource
    let plan: OpticalVideoPlan
    let expectedTexts: [String]
    private(set) var receivedFrameCount = 0
    private var receiver: OpticalReceiver?

    init<C: AirGapCryptography>(source: OpticalVideoSource, symbolsPerSecond: Int, using crypto: C) throws {
        let sender = try OpticalSender(envelope: source.envelope, using: crypto)
        self.source = source
        plan = try OpticalVideoPlan(sender: sender, symbolsPerSecond: symbolsPerSecond)
        expectedTexts = try plan.addresses.map { address in
            try Task.checkCancellation()
            return try sender.frame(page: address.page, sequence: address.sequence, using: crypto)
        }
    }

    mutating func receive<C: AirGapCryptography>(decodedText: String, presentationFrame: Int,
                                                durationFrames: Int, using crypto: C) throws {
        guard receivedFrameCount < plan.frameCount, presentationFrame == receivedFrameCount,
              durationFrames == 1 else { throw FTError.malformed("video.frame_timing") }
        let symbolIndex = receivedFrameCount / plan.framesPerSymbol
        guard decodedText.utf8.elementsEqual(expectedTexts[symbolIndex].utf8) else { throw FTError.checksum }
        if receivedFrameCount.isMultiple(of: plan.framesPerSymbol) {
            // Feed the recovered text, never the source text, to reconstruction.
            let frame = try OpticalFrame(text: decodedText)
            if receiver == nil { receiver = OpticalReceiver(selecting: frame) }
            try receiver?.receive(frame, using: crypto)
        }
        receivedFrameCount += 1
    }

    func finish<C: AirGapCryptography>(using crypto: C) throws {
        guard receivedFrameCount == plan.frameCount else { throw FTError.malformed("video.frame_count") }
        guard try receiver?.envelope(using: crypto) == source.envelope else { throw FTError.checksum }
    }
}

#if canImport(AVFoundation) && canImport(CoreImage) && canImport(Vision) && canImport(CryptoKit) && canImport(Darwin)
// AVFoundation's macOS 14-compatible writer/reader APIs predate Swift actor
// annotations. Every native object below stays on this actor, including cancel.
@preconcurrency import AVFoundation
import CoreImage
import CoreImage.CIFilterBuiltins
import CoreGraphics
import CoreMedia
import CoreVideo
import Darwin
import Vision

/// Streams ciphertext QR rasters to H.264, decodes every produced video frame,
/// and publishes only after exact timing, symbol, and FTAG reconstruction checks.
/// Source buffers are limited to four 1024-square BGRA buffers. Native codec and
/// Vision calls run serially with a cooperative ten-minute operation deadline.
public actor NativeOpticalVideo {
    private let context = CIContext(options: [.cacheIntermediates: false])
    private let crypto = AppleCryptography()
    private var operationID: UUID?
    private var expired = false
    private var didPublish = false
    private var activeWriter: AVAssetWriter?
    private var activeReader: AVAssetReader?
    private var activeAsset: AVURLAsset?
    var isExporting: Bool { operationID != nil }

    public init() {}

    public func save(source: OpticalVideoSource, to destination: URL, symbolsPerSecond: Int = 5) async throws -> OpticalVideoReceipt {
        try Task.checkCancellation()
        guard operationID == nil else { throw FTError.unsupported("video.export_in_progress") }
        guard destination.isFileURL, destination.pathExtension.lowercased() == "mp4" else { throw FTError.malformed("video.destination") }
        let id = UUID()
        operationID = id; expired = false; didPublish = false
        let deadline = ContinuousClock.now.advanced(by: .seconds(OpticalVideoPlan.maximumProcessingSeconds))
        let watchdog = Task.detached { [weak self] in
            do { try await Task.sleep(for: .seconds(OpticalVideoPlan.maximumProcessingSeconds)) }
            catch { return }
            await self?.expire(id)
        }
        defer {
            watchdog.cancel()
            cancelNativeWork()
            activeWriter = nil; activeReader = nil; activeAsset = nil
            operationID = nil
        }
        do {
            return try await withTaskCancellationHandler {
                try await produce(source: source, destination: destination, symbolsPerSecond: symbolsPerSecond, deadline: deadline)
            } onCancel: {
                Task { await self.cancel(id) }
            }
        } catch {
            // After exclusive rename the final movie is valid and owned by the
            // user. Report durability failure precisely; never call it cancelled
            // or delete a published file when directory synchronization fails.
            if didPublish { throw error }
            if Task.isCancelled { throw FTError.cancelled }
            if expired || ContinuousClock.now >= deadline { throw FTError.limit("video.processing_time") }
            throw error
        }
    }

    private func expire(_ id: UUID) {
        guard operationID == id else { return }
        expired = true
        cancelNativeWork()
    }

    private func cancel(_ id: UUID) {
        guard operationID == id else { return }
        cancelNativeWork()
    }

    private func cancelNativeWork() {
        if activeWriter?.status == .writing { activeWriter?.cancelWriting() }
        if activeReader?.status == .reading { activeReader?.cancelReading() }
        activeAsset?.cancelLoading()
    }

    private func checkpoint(_ deadline: ContinuousClock.Instant) throws {
        try Task.checkCancellation()
        guard !expired, ContinuousClock.now < deadline else { throw FTError.limit("video.processing_time") }
    }

    private func produce(source: OpticalVideoSource, destination: URL, symbolsPerSecond: Int,
                         deadline: ContinuousClock.Instant) async throws -> OpticalVideoReceipt {
        var verifier = try OpticalVideoVerifier(source: source, symbolsPerSecond: symbolsPerSecond, using: crypto)
        let plan = verifier.plan
        var largestCode = 0
        for text in verifier.expectedTexts {
            try checkpoint(deadline)
            largestCode = max(largestCode, try autoreleasepool { try qrCode(text).width })
        }
        // Core Image chooses each QR version itself. Preflight every symbol and
        // use one common integer module scale, fixed canvas, and >=4-module quiet
        // zone. This does not claim that the SDK can force a fixed QR version.
        let scale = OpticalVideoPlan.pixelSide / (largestCode + 8)
        guard scale >= 4 else { throw FTError.limit("video.qr_scale") }
        let access = destination.startAccessingSecurityScopedResource()
        defer { if access { destination.stopAccessingSecurityScopedResource() } }
        let parent = destination.deletingLastPathComponent()
        guard !FileManager.default.fileExists(atPath: destination.path) else { throw FTError.unsupported("video.choose_new_filename") }
        let volume = try parent.resourceValues(forKeys: [.volumeSupportsExclusiveRenamingKey])
        guard volume.volumeSupportsExclusiveRenaming == true else { throw FTError.unsupported("video.atomic_publication") }

        // AVAssetWriter requires a nonexistent output file. An exclusively
        // created, owner-only directory reserves its candidate without a race.
        let temporary = parent.appendingPathComponent(".ft-video-" + UUID().uuidString + ".partial", isDirectory: true)
        let created = temporary.withUnsafeFileSystemRepresentation { path in
            guard let path else { return false }
            return Darwin.mkdir(path, 0o700) == 0
        }
        guard created else { throw FTError.storage }
        defer {
            cancelNativeWork()
            try? FileManager.default.removeItem(at: temporary)
        }
        let candidate = temporary.appendingPathComponent("candidate.mp4")
        let writer = try AVAssetWriter(outputURL: candidate, fileType: .mp4)
        activeWriter = writer
        writer.metadata = []
        writer.movieTimeScale = CMTimeScale(OpticalVideoPlan.framesPerSecond)
        writer.shouldOptimizeForNetworkUse = false
        let settings: [String: Any] = [
            AVVideoCodecKey: AVVideoCodecType.h264,
            AVVideoWidthKey: OpticalVideoPlan.pixelSide,
            AVVideoHeightKey: OpticalVideoPlan.pixelSide,
            AVVideoColorPropertiesKey: [AVVideoColorPrimariesKey: AVVideoColorPrimaries_ITU_R_709_2,
                                       AVVideoTransferFunctionKey: AVVideoTransferFunction_ITU_R_709_2,
                                       AVVideoYCbCrMatrixKey: AVVideoYCbCrMatrix_ITU_R_709_2],
            AVVideoCompressionPropertiesKey: [AVVideoAverageBitRateKey: 6_000_000,
                AVVideoProfileLevelKey: AVVideoProfileLevelH264HighAutoLevel,
                AVVideoExpectedSourceFrameRateKey: OpticalVideoPlan.framesPerSecond,
                AVVideoMaxKeyFrameIntervalKey: plan.framesPerSymbol,
                AVVideoAllowFrameReorderingKey: false]
        ]
        guard writer.canApply(outputSettings: settings, forMediaType: .video) else { throw FTError.unsupported("video.h264_profile") }
        let input = AVAssetWriterInput(mediaType: .video, outputSettings: settings)
        input.expectsMediaDataInRealTime = false
        input.mediaTimeScale = CMTimeScale(OpticalVideoPlan.framesPerSecond)
        input.metadata = []; input.transform = .identity
        let attributes: [String: Any] = [
            kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA,
            kCVPixelBufferWidthKey as String: OpticalVideoPlan.pixelSide,
            kCVPixelBufferHeightKey as String: OpticalVideoPlan.pixelSide,
            kCVPixelBufferCGImageCompatibilityKey as String: true,
            kCVPixelBufferCGBitmapContextCompatibilityKey as String: true,
            kCVPixelBufferIOSurfacePropertiesKey as String: [:]
        ]
        let adaptor = AVAssetWriterInputPixelBufferAdaptor(assetWriterInput: input, sourcePixelBufferAttributes: attributes)
        guard writer.canAdd(input) else { throw FTError.unsupported("video.writer_input") }
        writer.add(input)
        guard writer.startWriting() else { throw FTError.unsupported("video.encoder_unavailable") }
        writer.startSession(atSourceTime: .zero)
        guard let pool = adaptor.pixelBufferPool else { throw FTError.unsupported("video.pixel_buffer_pool") }
        for (symbolIndex, text) in verifier.expectedTexts.enumerated() {
            try checkpoint(deadline)
            let buffer = try await buffer(from: pool, deadline: deadline)
            try autoreleasepool { try draw(text: text, into: buffer, scale: scale) }
            // The buffer is immutable for every hold and while retained by the
            // encoder. There is no interpolation or crossfade between symbols.
            for heldFrame in 0..<plan.framesPerSymbol {
                while !input.isReadyForMoreMediaData {
                    try checkpoint(deadline)
                    guard writer.status == .writing else { throw FTError.storage }
                    try await Task.sleep(for: .milliseconds(5))
                }
                try checkpoint(deadline)
                let frame = symbolIndex * plan.framesPerSymbol + heldFrame
                guard adaptor.append(buffer, withPresentationTime: time(frame)) else { throw FTError.storage }
                _ = try fileSize(candidate)
            }
        }
        input.markAsFinished()
        writer.endSession(atSourceTime: time(plan.frameCount))
        // The callback captures no native state. Polling the terminal status
        // permits bounded cancellation instead of an uninterruptible await.
        writer.finishWriting(completionHandler: {})
        while writer.status == .writing {
            try checkpoint(deadline)
            _ = try fileSize(candidate)
            try await Task.sleep(for: .milliseconds(10))
        }
        guard writer.status == .completed else { throw FTError.storage }
        let file = try FileHandle(forWritingTo: candidate)
        do { try file.synchronize(); try file.close() }
        catch { try? file.close(); throw error }
        let bytes = try fileSize(candidate)
        try await verify(candidate: candidate, verifier: &verifier, deadline: deadline)
        try checkpoint(deadline)
        guard try fileSize(candidate) == bytes else { throw FTError.checksum }
        // RENAME_EXCL combines publication and destination absence in one
        // filesystem operation, preserving a file created during encoding too.
        let published = candidate.withUnsafeFileSystemRepresentation { from in
            destination.withUnsafeFileSystemRepresentation { to in
                guard let from, let to else { return false }
                return Darwin.renamex_np(from, to, UInt32(RENAME_EXCL)) == 0
            }
        }
        guard published else { throw FTError.unsupported("video.atomic_publication") }
        didPublish = true
        try synchronizePublishedDirectory(parent)
        return .init(symbolCount: plan.symbolCount, frameCount: plan.frameCount,
                     symbolsPerSecond: plan.symbolsPerSecond, byteCount: bytes)
    }

    private func time(_ frame: Int) -> CMTime {
        CMTime(value: Int64(frame), timescale: CMTimeScale(OpticalVideoPlan.framesPerSecond))
    }

    private func synchronizePublishedDirectory(_ directory: URL) throws {
        let descriptor = directory.withUnsafeFileSystemRepresentation { path -> Int32 in
            guard let path else { return -1 }
            return Darwin.open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC)
        }
        guard descriptor >= 0 else { throw FTError.unsupported("video.published_durability_unconfirmed") }
        defer { _ = Darwin.close(descriptor) }
        guard Darwin.fsync(descriptor) == 0 else { throw FTError.unsupported("video.published_durability_unconfirmed") }
    }

    private func fileSize(_ url: URL) throws -> Int {
        // URL resource values may be cached while the encoder grows the file.
        let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
        guard attributes[.type] as? FileAttributeType == .typeRegular,
              let size = attributes[.size] as? NSNumber,
              size.uint64Value <= UInt64(OpticalVideoPlan.maximumVideoBytes) else { throw FTError.limit("video.bytes") }
        return Int(size.uint64Value)
    }

    private func qrCode(_ text: String) throws -> CGImage {
        guard text.hasPrefix("FTQR1/"), text.utf8.count <= FTLimit.qrText,
              text.utf8.allSatisfy({ $0 < 128 }) else { throw FTError.malformed("video.qr_payload") }
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(text.utf8); filter.correctionLevel = "Q"
        guard let output = filter.outputImage, let code = context.createCGImage(output, from: output.extent),
              code.width == code.height, code.width >= 21, code.width <= 185 else { throw FTError.unsupported("video.qr_capacity") }
        return code
    }

    private func buffer(from pool: CVPixelBufferPool, deadline: ContinuousClock.Instant) async throws -> CVPixelBuffer {
        let bound = [kCVPixelBufferPoolAllocationThresholdKey as String: 4] as CFDictionary
        while true {
            try checkpoint(deadline)
            var result: CVPixelBuffer?
            let status = CVPixelBufferPoolCreatePixelBufferWithAuxAttributes(kCFAllocatorDefault, pool, bound, &result)
            if status == kCVReturnSuccess, let result { return result }
            guard status == kCVReturnWouldExceedAllocationThreshold else { throw FTError.storage }
            guard activeWriter?.status == .writing else { throw FTError.storage }
            try await Task.sleep(for: .milliseconds(5))
        }
    }

    private func draw(text: String, into buffer: CVPixelBuffer, scale: Int) throws {
        let code = try qrCode(text), side = OpticalVideoPlan.pixelSide
        guard (code.width + 8) * scale <= side,
              CVPixelBufferLockBaseAddress(buffer, []) == kCVReturnSuccess else { throw FTError.storage }
        defer { CVPixelBufferUnlockBaseAddress(buffer, []) }
        guard let address = CVPixelBufferGetBaseAddress(buffer),
              let canvas = CGContext(data: address, width: side, height: side, bitsPerComponent: 8,
                bytesPerRow: CVPixelBufferGetBytesPerRow(buffer), space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGBitmapInfo.byteOrder32Little.rawValue | CGImageAlphaInfo.premultipliedFirst.rawValue) else { throw FTError.storage }
        canvas.setFillColor(gray: 1, alpha: 1)
        canvas.fill(CGRect(x: 0, y: 0, width: side, height: side))
        canvas.interpolationQuality = .none; canvas.setShouldAntialias(false)
        let margin = (side - code.width * scale) / 2
        guard margin >= 4 * scale else { throw FTError.limit("video.quiet_zone") }
        canvas.draw(code, in: CGRect(x: margin, y: margin, width: code.width * scale, height: code.height * scale))
    }

    private func verify(candidate: URL, verifier: inout OpticalVideoVerifier,
                        deadline: ContinuousClock.Instant) async throws {
        _ = try fileSize(candidate)
        let asset = AVURLAsset(url: candidate, options: [AVURLAssetPreferPreciseDurationAndTimingKey: true])
        activeAsset = asset
        let tracks = try await asset.load(.tracks)
        let duration = try await asset.load(.duration)
        let metadata = try await asset.load(.metadata)
        try checkpoint(deadline)
        guard tracks.count == 1, let track = tracks.first, track.mediaType == .video,
              metadata.isEmpty, CMTimeCompare(duration, time(verifier.plan.frameCount)) == 0 else { throw FTError.malformed("video.tracks_or_duration") }
        let size = try await track.load(.naturalSize)
        let transform = try await track.load(.preferredTransform)
        let trackMetadata = try await track.load(.metadata)
        let descriptions = try await track.load(.formatDescriptions)
        let range = try await track.load(.timeRange)
        try checkpoint(deadline)
        guard size == CGSize(width: OpticalVideoPlan.pixelSide, height: OpticalVideoPlan.pixelSide), transform.isIdentity,
              trackMetadata.isEmpty, descriptions.count == 1, let format = descriptions.first,
              CMFormatDescriptionGetMediaSubType(format) == kCMVideoCodecType_H264,
              CMTimeCompare(range.start, .zero) == 0, CMTimeCompare(range.duration, duration) == 0 else { throw FTError.malformed("video.track_profile") }
        let reader = try AVAssetReader(asset: asset)
        activeReader = reader
        let output = AVAssetReaderTrackOutput(track: track, outputSettings: [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA])
        output.alwaysCopiesSampleData = false
        guard reader.canAdd(output) else { throw FTError.unsupported("video.reader_output") }
        reader.add(output)
        guard reader.startReading() else { throw FTError.unsupported("video.decoder_unavailable") }
        while true {
            try checkpoint(deadline)
            let hasFrame: Bool = try autoreleasepool {
                guard let sample = output.copyNextSampleBuffer() else { return false }
                guard CMSampleBufferGetNumSamples(sample) == 1,
                      let pixels = CMSampleBufferGetImageBuffer(sample),
                      CVPixelBufferGetWidth(pixels) == OpticalVideoPlan.pixelSide,
                      CVPixelBufferGetHeight(pixels) == OpticalVideoPlan.pixelSide,
                      CVPixelBufferGetDataSize(pixels) <= OpticalVideoPlan.pixelSide * OpticalVideoPlan.pixelSide * 8 else { throw FTError.malformed("video.decoded_frame") }
                let presentation = try exactTick(CMSampleBufferGetPresentationTimeStamp(sample))
                let hold = try exactTick(CMSampleBufferGetDuration(sample))
                let request = VNDetectBarcodesRequest(); request.symbologies = [.qr]
                try VNImageRequestHandler(cvPixelBuffer: pixels, options: [:]).perform([request])
                guard let results = request.results, results.count == 1,
                      let decoded = results[0].payloadStringValue else { throw FTError.malformed("video.qr_decode") }
                try verifier.receive(decodedText: decoded, presentationFrame: presentation, durationFrames: hold, using: crypto)
                return true
            }
            if !hasFrame { break }
            // Permit the deadline/cancel actor messages to run even when native
            // decoding can deliver many frames without waiting for input.
            await Task.yield()
        }
        guard reader.status == .completed else { throw FTError.malformed("video.decode_incomplete") }
        try verifier.finish(using: crypto)
    }

    private func exactTick(_ value: CMTime) throws -> Int {
        guard value.isNumeric, value.epoch == 0 else { throw FTError.malformed("video.frame_timing") }
        let tick = CMTimeConvertScale(value, timescale: CMTimeScale(OpticalVideoPlan.framesPerSecond), method: .default)
        guard tick.value >= 0, tick.value <= Int64(OpticalVideoPlan.maximumVideoFrames),
              CMTimeCompare(value, tick) == 0 else { throw FTError.malformed("video.frame_timing") }
        return Int(tick.value)
    }
}
#endif
