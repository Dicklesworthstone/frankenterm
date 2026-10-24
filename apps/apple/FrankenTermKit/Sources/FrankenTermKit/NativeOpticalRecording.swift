import Foundation

/// Admission limits apply to local recordings before decoding, and again to
/// actual decoded frames. Reading a finite recording never promises recovery.
public enum OpticalRecordingLimits {
    public static let maximumBytes = OpticalVideoPlan.maximumVideoBytes
    public static let maximumFrames = OpticalVideoPlan.maximumVideoFrames
    public static let maximumImageFrames = 4096
    public static let maximumDurationSeconds = 300.0
    public static let maximumProcessingSeconds = OpticalVideoPlan.maximumProcessingSeconds
    public static let maximumPixelSide = 4096
    public static let maximumPixels = 8_388_608
    public static let maximumCodesPerFrame = 8
    static let pngCanvasHeaderBytes = 24

    public static func accepts(_ url: URL) -> Bool {
        url.isFileURL && ["mp4", "mov", "m4v", "png", "apng"].contains(url.pathExtension.lowercased())
    }

    static func checkDimensions(width: Int, height: Int) throws {
        guard width > 0, height > 0, width <= maximumPixelSide, height <= maximumPixelSide,
              width * height <= maximumPixels else { throw FTError.limit("recording.dimensions") }
    }

    /// The PNG signature and first IHDR chunk expose the canvas before any
    /// image is decoded. APNG uses this same canvas, even when its individual
    /// update rectangles are smaller. This is only a resource preflight;
    /// ImageIO still processes the remaining image format and frame data.
    static func checkPNGCanvasHeader(_ header: Data) throws {
        guard header.count == pngCanvasHeaderBytes else { throw FTError.malformed("recording.png") }
        var reader = ByteCursor(header)
        guard try reader.take(8) == Data([137, 80, 78, 71, 13, 10, 26, 10]),
              try reader.uint(4) == 13,
              try reader.take(4) == Data("IHDR".utf8) else { throw FTError.malformed("recording.png") }
        guard let width = Int(exactly: try reader.uint(4)),
              let height = Int(exactly: try reader.uint(4)) else { throw FTError.limit("recording.dimensions") }
        try checkDimensions(width: width, height: height)
    }
}

public struct OpticalRecordingProgress: Sendable {
    public let framesRead: Int
    /// Exact for image sequences; movie progress uses elapsed presentation time.
    public let totalFrames: Int?
    public let elapsedSeconds: Double?
    public let durationSeconds: Double?
}

public struct OpticalRecordingReadResult: Sendable {
    public let framesRead: Int
    /// False means the consumer stopped after reconstructing the ciphertext.
    /// True means all frames were read, not that the transfer is complete.
    public let reachedEnd: Bool
}

#if canImport(AVFoundation) && canImport(ImageIO) && canImport(Vision) && canImport(Darwin)
@preconcurrency import AVFoundation
import CoreGraphics
import CoreMedia
import CoreVideo
import ImageIO
import Vision

/// One serial reader owns the native codec and lends only bounded QR text to
/// its awaited consumer. No camera images, movie copies, codes or plaintext are
/// retained. A selected cloud placeholder is refused without requesting it.
public actor NativeOpticalRecording {
    private var operation: UUID?
    private var expired = false
    private var activeAsset: AVURLAsset?
    private var activeReader: AVAssetReader?

    public init() {}

    public func read(_ url: URL,
        onFrame: @escaping @Sendable ([String], OpticalRecordingProgress) async throws -> Bool
    ) async throws -> OpticalRecordingReadResult {
        try Task.checkCancellation()
        guard operation == nil else { throw WorkspaceStoreError.busy }
        guard OpticalRecordingLimits.accepts(url) else { throw FTError.unsupported("recording.file_type") }
        let access = url.startAccessingSecurityScopedResource()
        defer { if access { url.stopAccessingSecurityScopedResource() } }
        try Self.checkLocalFile(url)
        let id = UUID()
        operation = id; expired = false
        let deadline = ContinuousClock.now.advanced(by: .seconds(OpticalRecordingLimits.maximumProcessingSeconds))
        let watchdog = Task.detached { [weak self] in
            do { try await Task.sleep(for: .seconds(OpticalRecordingLimits.maximumProcessingSeconds)) }
            catch { return }
            await self?.expire(id)
        }
        defer {
            watchdog.cancel(); cancelNativeWork()
            activeReader = nil; activeAsset = nil; operation = nil
        }
        do {
            return try await withTaskCancellationHandler {
                if ["png", "apng"].contains(url.pathExtension.lowercased()) {
                    return try await readImages(url, deadline: deadline, onFrame: onFrame)
                }
                return try await readMovie(url, deadline: deadline, onFrame: onFrame)
            } onCancel: { Task { await self.cancel(id) } }
        } catch {
            if Task.isCancelled { throw FTError.cancelled }
            if expired || ContinuousClock.now >= deadline { throw FTError.limit("recording.processing_time") }
            throw error
        }
    }

    private func cancel(_ id: UUID) {
        guard operation == id else { return }
        cancelNativeWork()
    }
    private func expire(_ id: UUID) {
        guard operation == id else { return }
        expired = true; cancelNativeWork()
    }
    private func cancelNativeWork() {
        if activeReader?.status == .reading { activeReader?.cancelReading() }
        activeAsset?.cancelLoading()
    }
    private func checkpoint(_ deadline: ContinuousClock.Instant) throws {
        try Task.checkCancellation()
        guard !expired, ContinuousClock.now < deadline else { throw FTError.limit("recording.processing_time") }
    }

    private static func checkLocalFile(_ url: URL) throws {
        let values = try url.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey,
            .isUbiquitousItemKey, .ubiquitousItemDownloadingStatusKey])
        guard values.isUbiquitousItem != true || values.ubiquitousItemDownloadingStatus == .current else {
            throw FTError.unsupported("file.not_local")
        }
        guard values.isRegularFile == true, let count = values.fileSize, count > 0,
              count <= OpticalRecordingLimits.maximumBytes else { throw FTError.limit("recording.file_bytes") }
    }

    private func readMovie(_ url: URL, deadline: ContinuousClock.Instant,
        onFrame: @escaping @Sendable ([String], OpticalRecordingProgress) async throws -> Bool
    ) async throws -> OpticalRecordingReadResult {
        // A file URL alone does not make a reference movie offline. Forbid
        // references outside its own container, including local sidecar media.
        let asset = AVURLAsset(url: url, options: [
            AVURLAssetReferenceRestrictionsKey: NSNumber(value: AVAssetReferenceRestrictions.forbidAll.rawValue),
            AVURLAssetAllowsCellularAccessKey: false,
            AVURLAssetAllowsExpensiveNetworkAccessKey: false,
            AVURLAssetAllowsConstrainedNetworkAccessKey: false
        ])
        activeAsset = asset
        let tracks = try await asset.load(.tracks)
        try checkpoint(deadline)
        let video = tracks.filter { $0.mediaType == .video }
        guard tracks.count <= 8, video.count == 1, let track = video.first else {
            throw FTError.unsupported("recording.video_tracks")
        }
        let duration = try await asset.load(.duration)
        let seconds = duration.seconds
        guard seconds.isFinite, seconds > 0, seconds <= OpticalRecordingLimits.maximumDurationSeconds else {
            throw FTError.limit("recording.duration")
        }
        let dimensions = try await track.load(.naturalSize)
        guard dimensions.width.isFinite, dimensions.height.isFinite,
              dimensions.width > 0, dimensions.height > 0,
              dimensions.width <= Double(OpticalRecordingLimits.maximumPixelSide),
              dimensions.height <= Double(OpticalRecordingLimits.maximumPixelSide) else {
            throw FTError.limit("recording.dimensions")
        }
        try OpticalRecordingLimits.checkDimensions(width: Int(dimensions.width.rounded(.up)),
                                                   height: Int(dimensions.height.rounded(.up)))
        // A movie can carry multiple sample descriptions. Bound every declared
        // coded size before starting a decoder, not only the presentation size
        // of the first sample. Actual pixel buffers are checked separately.
        let formats = try await track.load(.formatDescriptions)
        guard !formats.isEmpty, formats.count <= 8 else { throw FTError.limit("recording.sample_descriptions") }
        for format in formats {
            guard CMFormatDescriptionGetMediaType(format) == kCMMediaType_Video else { throw FTError.malformed("recording.sample_description") }
            let coded = CMVideoFormatDescriptionGetDimensions(format)
            try OpticalRecordingLimits.checkDimensions(width: Int(coded.width), height: Int(coded.height))
        }
        try checkpoint(deadline)
        let reader = try AVAssetReader(asset: asset)
        activeReader = reader
        let output = AVAssetReaderTrackOutput(track: track,
            outputSettings: [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA])
        output.alwaysCopiesSampleData = false
        guard reader.canAdd(output) else { throw FTError.unsupported("recording.decoder") }
        reader.add(output)
        guard reader.startReading() else { throw FTError.malformed("recording.video") }
        var count = 0, priorTexts: [String] = []
        while true {
            try checkpoint(deadline)
            // Native sample buffers and Vision requests die before the actor
            // awaits its consumer. At most one decoded frame is in flight.
            guard let frame = try Self.nextVideoFrame(output) else { break }
            count += 1
            guard count <= OpticalRecordingLimits.maximumFrames else { throw FTError.limit("recording.frames") }
            guard frame.seconds.isFinite, frame.seconds >= 0,
                  frame.seconds <= OpticalRecordingLimits.maximumDurationSeconds else {
                throw FTError.malformed("recording.frame_time")
            }
            let changed = frame.texts != priorTexts
            priorTexts = frame.texts
            if changed || count == 1 || count.isMultiple(of: 30) {
                let progress = OpticalRecordingProgress(framesRead: count, totalFrames: nil,
                    elapsedSeconds: min(frame.seconds, seconds), durationSeconds: seconds)
                if try await !onFrame(changed ? frame.texts : [], progress) {
                    try checkpoint(deadline)
                    return .init(framesRead: count, reachedEnd: false)
                }
            }
            if count.isMultiple(of: 30) { try Self.checkLocalFile(url) }
            await Task.yield()
        }
        try checkpoint(deadline)
        guard reader.status == .completed, count > 0 else { throw FTError.malformed("recording.video") }
        return .init(framesRead: count, reachedEnd: true)
    }

    private static func nextVideoFrame(_ output: AVAssetReaderTrackOutput) throws -> (texts: [String], seconds: Double)? {
        try autoreleasepool {
            guard let sample = output.copyNextSampleBuffer() else { return nil }
            guard CMSampleBufferGetNumSamples(sample) == 1,
                  let pixels = CMSampleBufferGetImageBuffer(sample) else { throw FTError.malformed("recording.frame") }
            try OpticalRecordingLimits.checkDimensions(width: CVPixelBufferGetWidth(pixels), height: CVPixelBufferGetHeight(pixels))
            guard CVPixelBufferGetDataSize(pixels) <= OpticalRecordingLimits.maximumPixels * 4 + 65_536 else {
                throw FTError.limit("recording.frame_bytes")
            }
            let request = VNDetectBarcodesRequest(); request.symbologies = [.qr]
            try VNImageRequestHandler(cvPixelBuffer: pixels, options: [:]).perform([request])
            return (try texts(request.results ?? []), CMSampleBufferGetPresentationTimeStamp(sample).seconds)
        }
    }

    private func readImages(_ url: URL, deadline: ContinuousClock.Instant,
        onFrame: @escaping @Sendable ([String], OpticalRecordingProgress) async throws -> Bool
    ) async throws -> OpticalRecordingReadResult {
        // Global ImageIO properties need not include PNG canvas dimensions.
        // Read only its fixed header and admit the canvas before creating an
        // image source; never substitute a smaller APNG frame rectangle.
        var header = Data()
        do {
            let handle = try FileHandle(forReadingFrom: url)
            defer { try? handle.close() }
            while header.count < OpticalRecordingLimits.pngCanvasHeaderBytes,
                  let bytes = try handle.read(upToCount: OpticalRecordingLimits.pngCanvasHeaderBytes - header.count),
                  !bytes.isEmpty {
                header += bytes
            }
        }
        try OpticalRecordingLimits.checkPNGCanvasHeader(header)
        try checkpoint(deadline)
        let options = [kCGImageSourceShouldCache: false] as CFDictionary
        guard let source = CGImageSourceCreateWithURL(url as CFURL, options),
              CGImageSourceGetType(source) as String? == "public.png" else {
            throw FTError.malformed("recording.png")
        }
        let count = CGImageSourceGetCount(source)
        guard count > 0, count <= OpticalRecordingLimits.maximumImageFrames else { throw FTError.limit("recording.frames") }
        var priorTexts: [String] = []
        for index in 0..<count {
            try checkpoint(deadline)
            let decoded = try Self.imageFrame(source, index: index)
            let changed = decoded != priorTexts
            priorTexts = decoded
            let progress = OpticalRecordingProgress(framesRead: index + 1, totalFrames: count,
                                                     elapsedSeconds: nil, durationSeconds: nil)
            if try await !onFrame(changed ? decoded : [], progress) {
                try checkpoint(deadline)
                return .init(framesRead: index + 1, reachedEnd: false)
            }
            if index.isMultiple(of: 30) { try Self.checkLocalFile(url) }
            await Task.yield()
        }
        try checkpoint(deadline)
        return .init(framesRead: count, reachedEnd: true)
    }

    private static func imageFrame(_ source: CGImageSource, index: Int) throws -> [String] {
        try autoreleasepool {
            guard let properties = CGImageSourceCopyPropertiesAtIndex(source, index, nil) as? [CFString: Any],
                  let width = properties[kCGImagePropertyPixelWidth] as? NSNumber,
                  let height = properties[kCGImagePropertyPixelHeight] as? NSNumber else { throw FTError.malformed("recording.image") }
            try OpticalRecordingLimits.checkDimensions(width: width.intValue, height: height.intValue)
            guard let image = CGImageSourceCreateImageAtIndex(source, index,
                [kCGImageSourceShouldCache: false] as CFDictionary) else { throw FTError.malformed("recording.image") }
            try OpticalRecordingLimits.checkDimensions(width: image.width, height: image.height)
            let request = VNDetectBarcodesRequest(); request.symbologies = [.qr]
            try VNImageRequestHandler(cgImage: image, options: [:]).perform([request])
            return try texts(request.results ?? [])
        }
    }

    private static func texts(_ results: [VNBarcodeObservation]) throws -> [String] {
        guard results.count <= OpticalRecordingLimits.maximumCodesPerFrame else { throw FTError.limit("recording.qr_count") }
        // A recording can contain the visible code UI of an old screen capture.
        // Never consume FTCODE1: unlocking still requires the separate secret.
        return results.compactMap(\.payloadStringValue).filter {
            $0.hasPrefix("FTQR1/") && $0.utf8.count <= FTLimit.qrText
        }.sorted()
    }
}
#endif
