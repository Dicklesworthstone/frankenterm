import Foundation
import XCTest
@testable import FrankenTermKit

final class OpticalVideoTests: XCTestCase {
    private let crypto = TestCryptography()

    private func source(bytes: Int, transferByte: UInt8 = 2) throws -> OpticalVideoSource {
        let envelope = try FTAG.sealArchiveBytes(Data(repeating: 7, count: bytes),
            code: TransferCode(secret: Data(repeating: 1, count: 16)),
            transferID: Data(repeating: transferByte, count: 16), salt: Data(repeating: 3, count: 16),
            nonce: Data(repeating: 4, count: 12), compress: false, using: crypto)
        return try OpticalVideoSource(envelope: envelope)
    }

    func testMovieInputAcceptsOnlyBoundedEncryptedEnvelopes() throws {
        let encrypted = try source(bytes: 100)
        XCTAssertGreaterThan(encrypted.envelopeByteCount, 100)
        for plaintext in [try sampleManifest(), Data("-----BEGIN OPENSSH PRIVATE KEY-----".utf8), Data(repeating: 1, count: 16)] {
            XCTAssertThrowsError(try OpticalVideoSource(envelope: plaintext))
        }
        XCTAssertThrowsError(try OpticalVideoSource(envelope: encrypted.envelope + Data([0])))
        XCTAssertThrowsError(try OpticalVideoSource(envelope: Data(encrypted.envelope.dropLast())))
        var invalid = encrypted.envelope; invalid[0] ^= 1
        XCTAssertThrowsError(try OpticalVideoSource(envelope: invalid))
        XCTAssertThrowsError(try OpticalVideoSource(envelope: Data(repeating: 0, count: FTLimit.envelope + 1)))
    }

    func testEverySupportedRatePreservesWholeFrameHoldsAndReconstructsAllPages() throws {
        let input = try source(bytes: 40000)
        let sender = try OpticalSender(envelope: input.envelope, using: crypto)
        XCTAssertEqual(sender.identity.pageCount, 2)
        var originalAddresses: [OpticalAnimationPlan.Address]?
        for (rate, hold) in [(3, 10), (5, 6), (10, 3)] {
            var verifier = try OpticalVideoVerifier(source: input, symbolsPerSecond: rate, using: crypto)
            let plan = verifier.plan
            XCTAssertEqual(plan.framesPerSymbol, hold)
            XCTAssertEqual(plan.frameCount, plan.symbolCount * hold)
            XCTAssertEqual(plan.durationSeconds, Double(plan.symbolCount) / Double(rate), accuracy: 0.000001)
            if let originalAddresses { XCTAssertEqual(plan.addresses, originalAddresses, "changing speed keeps the immutable symbol set") }
            else { originalAddresses = plan.addresses }
            for page in 0..<sender.identity.pageCount {
                let count = try sender.sourcePartCount(page: page)
                let expectedCount = count == 1 ? 1 : count + (count + 1) / 2 + 16
                let numbers = plan.addresses.filter { $0.page == page }.map(\.sequence)
                XCTAssertGreaterThanOrEqual(numbers.count, expectedCount)
                XCTAssertEqual(numbers, Array(UInt32(1)...UInt32(numbers.count)))
                if count == 1 { XCTAssertEqual(numbers, [1]) }
            }
            for frame in 0..<plan.frameCount {
                try verifier.receive(decodedText: verifier.expectedTexts[frame / hold],
                                     presentationFrame: frame, durationFrames: 1, using: crypto)
            }
            try verifier.finish(using: crypto)
            XCTAssertEqual(verifier.receivedFrameCount, plan.frameCount)
            XCTAssertThrowsError(try verifier.receive(decodedText: verifier.expectedTexts[0],
                presentationFrame: plan.frameCount, durationFrames: 1, using: crypto), "extra frames also reject publication")
        }
    }

    func testDecodedMovieRejectsTruncationReorderingDuplicatesAndIncorrectTiming() throws {
        let original = try OpticalVideoVerifier(source: source(bytes: 1000), symbolsPerSecond: 5, using: crypto)
        XCTAssertGreaterThan(original.expectedTexts.count, 1)
        XCTAssertThrowsError(try original.finish(using: crypto))
        for (timestamp, duration) in [(-1, 1), (1, 1), (0, 0), (0, 2)] {
            var invalid = original
            XCTAssertThrowsError(try invalid.receive(decodedText: original.expectedTexts[0],
                presentationFrame: timestamp, durationFrames: duration, using: crypto))
        }
        var reordered = original
        XCTAssertThrowsError(try reordered.receive(decodedText: original.expectedTexts[1],
            presentationFrame: 0, durationFrames: 1, using: crypto))
        var heldTooLong = original
        for frame in 0..<original.plan.framesPerSymbol {
            try heldTooLong.receive(decodedText: original.expectedTexts[0], presentationFrame: frame, durationFrames: 1, using: crypto)
        }
        XCTAssertThrowsError(try heldTooLong.receive(decodedText: original.expectedTexts[0],
            presentationFrame: original.plan.framesPerSymbol, durationFrames: 1, using: crypto))
        var missingLast = original
        for frame in 0..<original.plan.frameCount - 1 {
            try missingLast.receive(decodedText: original.expectedTexts[frame / original.plan.framesPerSymbol],
                presentationFrame: frame, durationFrames: 1, using: crypto)
        }
        XCTAssertThrowsError(try missingLast.finish(using: crypto), "complete fountain recovery does not excuse a truncated movie")
    }

    func testMovieValidationDoesNotAcceptPayloadsFromAnotherEncryptedTransfer() throws {
        var expected = try OpticalVideoVerifier(source: source(bytes: 1000), symbolsPerSecond: 10, using: crypto)
        let foreign = try OpticalVideoVerifier(source: source(bytes: 1000, transferByte: 9), symbolsPerSecond: 10, using: crypto)
        XCTAssertThrowsError(try expected.receive(decodedText: foreign.expectedTexts[0], presentationFrame: 0, durationFrames: 1, using: crypto))
        XCTAssertEqual(expected.receivedFrameCount, 0)
    }

    func testPeriodicSamplingOfWholeVideoFramesRecoversAtEverySupportedRate() throws {
        let input = try source(bytes: 40000)
        for rate in [3, 5, 10] {
            let verifier = try OpticalVideoVerifier(source: input, symbolsPerSecond: rate, using: crypto)
            let plan = verifier.plan, hold = plan.framesPerSymbol
            let symbols = try verifier.expectedTexts.map { try OpticalFrame(text: $0) }
            for step in [hold + 1, 2 * hold, 3 * hold] {
                for phase in [0, hold - 1] {
                    var receiver = OpticalReceiver(selecting: symbols[0])
                    for sample in 0..<plan.frameCount {
                        let position = (phase + sample * step) % plan.frameCount
                        try receiver.receive(symbols[position / hold], using: crypto)
                        if receiver.isComplete { break }
                    }
                    XCTAssertEqual(try receiver.envelope(using: crypto), input.envelope,
                                   "rate=\(rate), capture stride=\(step), phase=\(phase)")
                }
            }
        }
    }

    func testCadencePaddingRespectsBothTheSlowMovieAndSymbolBudgets() throws {
        let slow = try OpticalSender(envelope: source(bytes: 135000).envelope, using: crypto)
        let slowPlan = try OpticalVideoPlan(sender: slow, symbolsPerSecond: 3)
        XCTAssertEqual(slowPlan.symbolCount, 883)
        XCTAssertLessThanOrEqual(slowPlan.frameCount, OpticalVideoPlan.maximumVideoFrames)
        let tooLong = try OpticalSender(envelope: source(bytes: 139000).envelope, using: crypto)
        XCTAssertThrowsError(try OpticalVideoPlan(sender: tooLong, symbolsPerSecond: 3)) {
            XCTAssertEqual($0 as? FTError, .limit("video.use_live_transfer"))
        }
        let nearSymbolCap = try OpticalSender(envelope: source(bytes: 155000).envelope, using: crypto)
        for rate in [5, 10] {
            let plan = try OpticalVideoPlan(sender: nearSymbolCap, symbolsPerSecond: rate)
            XCTAssertEqual(plan.symbolCount, 1009)
            XCTAssertLessThanOrEqual(plan.symbolCount, OpticalVideoPlan.maximumSymbols)
            XCTAssertLessThanOrEqual(plan.frameCount, OpticalVideoPlan.maximumVideoFrames)
        }
    }

    func testRecordingLimitsRejectInsteadOfDroppingSourceOrRepairSymbols() throws {
        let sender = try OpticalSender(envelope: source(bytes: 150000).envelope, using: crypto)
        let normal = try OpticalVideoPlan(sender: sender, symbolsPerSecond: 5)
        XCTAssertGreaterThan(normal.symbolCount, OpticalVideoPlan.maximumVideoFrames / 10)
        XCTAssertLessThanOrEqual(normal.symbolCount, OpticalVideoPlan.maximumSymbols)
        XCTAssertThrowsError(try OpticalVideoPlan(sender: sender, symbolsPerSecond: 3), "slow mode must stay within the five-minute frame budget")
        XCTAssertEqual(try OpticalVideoPlan(sender: sender, symbolsPerSecond: 10).addresses, normal.addresses)
        for rate in [-1, 0, 1, 4, 6, 30, Int.max] {
            XCTAssertThrowsError(try OpticalVideoPlan(sender: sender, symbolsPerSecond: rate))
        }
        let tooLarge = try OpticalSender(envelope: source(bytes: 200000).envelope, using: crypto)
        XCTAssertThrowsError(try OpticalVideoPlan(sender: tooLarge, symbolsPerSecond: 10))
    }
}
