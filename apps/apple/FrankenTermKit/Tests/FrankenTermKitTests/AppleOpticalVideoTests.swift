#if canImport(AVFoundation) && canImport(CoreImage) && canImport(Vision) && canImport(CryptoKit) && canImport(Darwin)
import Foundation
import XCTest
@preconcurrency import AVFoundation
import CoreImage
import CoreMedia
import CoreVideo
import Vision
@testable import FrankenTermKit

/// These tests require an Apple SDK and actual native H.264/Vision execution.
/// A passing Linux suite does not run or qualify this movie pipeline.
@MainActor
final class AppleOpticalVideoTests: XCTestCase {
    private let crypto = AppleCryptography()

    private func directory() throws -> URL {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("ft-video-test-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
        return root
    }

    private func source(bytes: Int) throws -> OpticalVideoSource {
        try OpticalVideoSource(envelope: FTAG.sealArchiveBytes(Data(repeating: 7, count: bytes),
            code: TransferCode(secret: Data(repeating: 1, count: 16)), transferID: Data(repeating: 2, count: 16),
            salt: Data(repeating: 3, count: 16), nonce: Data(repeating: 4, count: 12), compress: false, using: crypto))
    }

    /// Independently reread the published file, converting decoded pixels to
    /// CGImage before Vision. Production verifies CVPixelBuffer directly.
    private func assertPublishedMovie(_ url: URL, source: OpticalVideoSource, rate: Int) async throws {
        let sender = try OpticalSender(envelope: source.envelope, using: crypto)
        let plan = try OpticalVideoPlan(sender: sender, symbolsPerSecond: rate)
        let asset = AVURLAsset(url: url)
        let tracks = try await asset.load(.tracks)
        let duration = try await asset.load(.duration)
        let metadata = try await asset.load(.metadata)
        XCTAssertEqual(tracks.count, 1)
        let track = try XCTUnwrap(tracks.first)
        XCTAssertEqual(track.mediaType, .video)
        XCTAssertTrue(metadata.isEmpty)
        XCTAssertEqual(CMTimeCompare(duration, CMTime(value: Int64(plan.frameCount), timescale: 30)), 0)
        let reader = try AVAssetReader(asset: asset)
        let output = AVAssetReaderTrackOutput(track: track,
            outputSettings: [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA])
        XCTAssertTrue(reader.canAdd(output)); reader.add(output)
        XCTAssertTrue(reader.startReading())
        let context = CIContext(options: [.cacheIntermediates: false])
        var received = 0, receiver: OpticalReceiver?
        while true {
            let present = try autoreleasepool {
                guard let sample = output.copyNextSampleBuffer() else { return false }
                guard received < plan.frameCount else { throw FTError.malformed("test.video_extra_frame") }
                XCTAssertEqual(CMSampleBufferGetNumSamples(sample), 1)
                XCTAssertEqual(CMTimeCompare(CMSampleBufferGetPresentationTimeStamp(sample), CMTime(value: Int64(received), timescale: 30)), 0)
                XCTAssertEqual(CMTimeCompare(CMSampleBufferGetDuration(sample), CMTime(value: 1, timescale: 30)), 0)
                let pixels = try XCTUnwrap(CMSampleBufferGetImageBuffer(sample))
                XCTAssertEqual(CVPixelBufferGetWidth(pixels), 1024)
                XCTAssertEqual(CVPixelBufferGetHeight(pixels), 1024)
                let decoded = CIImage(cvPixelBuffer: pixels)
                let image = try XCTUnwrap(context.createCGImage(decoded, from: decoded.extent))
                let request = VNDetectBarcodesRequest(); request.symbologies = [.qr]
                try VNImageRequestHandler(cgImage: image, options: [:]).perform([request])
                let results = try XCTUnwrap(request.results)
                XCTAssertEqual(results.count, 1)
                let text = try XCTUnwrap(results.first?.payloadStringValue)
                let address = plan.addresses[received / plan.framesPerSymbol]
                XCTAssertEqual(text, try sender.frame(page: address.page, sequence: address.sequence, using: crypto))
                let frame = try OpticalFrame(text: text)
                if receiver == nil { receiver = OpticalReceiver(selecting: frame) }
                try receiver?.receive(frame, using: crypto)
                received += 1
                return true
            }
            if !present { break }
        }
        XCTAssertEqual(reader.status, .completed)
        XCTAssertEqual(received, plan.frameCount)
        XCTAssertEqual(try receiver?.envelope(using: crypto), source.envelope)
    }

    func testH264MoviesHoldSinglePartSymbolsForExactlyTenSixAndThreeFrames() async throws {
        let root = try directory(), encrypted = try source(bytes: 20)
        let renderer = NativeOpticalVideo()
        for (rate, frames) in [(3, 10), (5, 6), (10, 3)] {
            let url = root.appendingPathComponent("recording-\(rate).mp4")
            let receipt = try await renderer.save(source: encrypted, to: url, symbolsPerSecond: rate)
            XCTAssertEqual(receipt.symbolCount, 1)
            XCTAssertEqual(receipt.frameCount, frames)
            XCTAssertEqual(receipt.framesPerSecond, 30)
            XCTAssertEqual(receipt.symbolsPerSecond, rate)
            XCTAssertEqual(receipt.pixelSide, 1024)
            XCTAssertEqual(receipt.durationSeconds, 1.0 / Double(rate), accuracy: 0.000001)
            XCTAssertEqual(receipt.byteCount, try Data(contentsOf: url).count)
            try await assertPublishedMovie(url, source: encrypted, rate: rate)
        }
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: nil).count, 3)
    }

    func testMultipartWorkspaceMovieReconstructsAndNeverOverwritesAnExistingFile() async throws {
        let root = try directory(), renderer = NativeOpticalVideo()
        let archive = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [], using: crypto)
        let exported = try FTAG.seal(archive, using: crypto)
        let encrypted = try OpticalVideoSource(envelope: exported.envelope)
        let url = root.appendingPathComponent("workspace-backup.mp4")
        let receipt = try await renderer.save(source: encrypted, to: url, symbolsPerSecond: 10)
        XCTAssertGreaterThan(receipt.symbolCount, 1)
        XCTAssertEqual(receipt.frameCount, receipt.symbolCount * 3)
        try await assertPublishedMovie(url, source: encrypted, rate: 10)
        let original = try Data(contentsOf: url)
        XCTAssertNil(original.range(of: Data(exported.code.reveal(using: crypto).utf8)))
        do { _ = try await renderer.save(source: encrypted, to: url); XCTFail("overwrote a validated backup") }
        catch {}
        XCTAssertEqual(try Data(contentsOf: url), original)
        let occupied = root.appendingPathComponent("existing-user-file.mp4"), sentinel = Data("preserve this existing file".utf8)
        try sentinel.write(to: occupied, options: .withoutOverwriting)
        do { _ = try await renderer.save(source: encrypted, to: occupied); XCTFail("overwrote the user's existing file") }
        catch {}
        XCTAssertEqual(try Data(contentsOf: occupied), sentinel)
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: nil).count, 2)
    }

    func testCancellationRetiresTheCandidateAndAllowsAFreshExport() async throws {
        let root = try directory(), renderer = NativeOpticalVideo(), encrypted = try source(bytes: 40000)
        let cancelledURL = root.appendingPathComponent("cancelled.mp4")
        let task = Task { try await renderer.save(source: encrypted, to: cancelledURL, symbolsPerSecond: 3) }
        defer { task.cancel() }
        let deadline = ContinuousClock.now.advanced(by: .seconds(30))
        while !(await renderer.isExporting) {
            guard ContinuousClock.now < deadline else { XCTFail("export did not begin"); return }
            await Task.yield()
        }
        task.cancel()
        do { _ = try await task.value; XCTFail("a cancelled export was published") }
        catch {}
        XCTAssertFalse(FileManager.default.fileExists(atPath: cancelledURL.path))
        XCTAssertTrue(try FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: nil).isEmpty)
        let stillExporting = await renderer.isExporting
        XCTAssertFalse(stillExporting)
        let replacement = root.appendingPathComponent("replacement.mp4")
        let receipt = try await renderer.save(source: source(bytes: 20), to: replacement, symbolsPerSecond: 5)
        XCTAssertEqual(receipt.frameCount, 6)
        XCTAssertTrue(FileManager.default.fileExists(atPath: replacement.path))
    }

    func testDestinationCreatedDuringEncodingSurvivesFinalPublication() async throws {
        let root = try directory(), renderer = NativeOpticalVideo(), encrypted = try source(bytes: 1000)
        let destination = root.appendingPathComponent("contested.mp4")
        let task = Task { try await renderer.save(source: encrypted, to: destination, symbolsPerSecond: 10) }
        defer { task.cancel() }
        let deadline = ContinuousClock.now.advanced(by: .seconds(30))
        while !(try FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)
            .contains(where: { $0.lastPathComponent.hasPrefix(".ft-video-") })) {
            guard ContinuousClock.now < deadline else { XCTFail("exclusive candidate was not created"); return }
            try await Task.sleep(for: .milliseconds(5))
        }
        let sentinel = Data("another file appeared while the movie was being encoded".utf8)
        try sentinel.write(to: destination, options: .withoutOverwriting)
        do { _ = try await task.value; XCTFail("exclusive publication replaced a concurrent file") }
        catch {}
        XCTAssertEqual(try Data(contentsOf: destination), sentinel)
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: nil), [destination])
    }
}
#endif
