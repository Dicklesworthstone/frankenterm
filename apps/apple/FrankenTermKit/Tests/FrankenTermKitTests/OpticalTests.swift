import Foundation
import XCTest
@testable import FrankenTermKit

final class OpticalTests: XCTestCase {
    let crypto = TestCryptography()
    func testPublishedConsensusVectors() throws {
        XCTAssertEqual(URCRC.checksum(Data("Wolf".utf8)), 0x598c84dc)
        XCTAssertEqual(Bytewords.encode(Data("Hello\n".utf8)), "FDIHJZJZJLBKEHMTECCM")
        XCTAssertEqual(try Bytewords.decode("FDIHJZJZJLBKEHMTECCM"), Data("Hello\n".utf8))
        let all = Data((0...255).map(UInt8.init))
        XCTAssertEqual(try Bytewords.decode(Substring(Bytewords.encode(all))), all)
        var rng = URRandom(seed: Data("Wolf".utf8), using: crypto)
        XCTAssertEqual((0..<20).map { _ in rng.next() % 100 }, [42,81,85,8,82,84,76,73,70,88,2,74,40,48,77,54,88,7,5,88])
        var degreeRNG = URRandom(seed: Data("Wolf".utf8), using: crypto)
        let degree = URDegree(count: 11)
        XCTAssertEqual(try (0..<20).map { _ in try degree.next(using: &degreeRNG) }, [7,9,2,1,4,2,1,1,3,10,7,1,1,4,3,8,6,2,3,2])
        var messageRNG = URRandom(seed: Data("Wolf".utf8), using: crypto)
        let message = Data(try (0..<1024).map { _ in UInt8(try messageRNG.index(count: 256)) })
        let crc = URCRC.checksum(message)
        let expected = [[9], [2,5,6,8,9,10], [8], [1,5], [1], [0,2,4,5,8,10], [5], [2], [2], [0,1,3,4,5,7,9,10]]
        XCTAssertEqual(try (12...21).map { try degree.indexes(sequence: UInt32($0), checksum: crc, using: crypto).sorted() }, expected)
        let example = URPart(sequence: 12, count: 8, messageLength: 100, checksum: 0x12345678, payload: Data([1,5,3,3,5]))
        XCTAssertEqual(example.encoded().ftHex, "850C0818641A12345678450105030305")
    }
    func testBytewordsIntegrityAndBounds() throws {
        for text in ["", "AAAA", "FDIHJZJZJLBKEHMTECCC", "FDIHJZJZJLBKEHMTECC", "!!!!!!!!!!!!!!!!!!!!", String(repeating: "AA", count: 1025)] {
            XCTAssertThrowsError(try Bytewords.decode(Substring(text)))
        }
    }
    func makeEnvelope(size: Int) throws -> Data {
        var rng = URRandom(seed: Data("optical synthetic archive".utf8), using: crypto)
        let archive = Data(try (0..<size).map { _ in UInt8(try rng.index(count: 256)) })
        return try FTAG.sealArchiveBytes(archive, code: TransferCode(secret: Data(repeating: 17, count: 16)),
                                        transferID: Data(repeating: 34, count: 16), salt: Data(repeating: 51, count: 16),
                                        nonce: Data(repeating: 68, count: 12), compress: false, using: crypto)
    }
    func testSystematicTransferReordersAndDeduplicates() throws {
        let envelope = try makeEnvelope(size: 70000), sender = try OpticalSender(envelope: envelope, using: crypto)
        let initial = try OpticalFrame(text: sender.frame(page: 1, sequence: 5, using: crypto))
        var receiver = OpticalReceiver(selecting: initial)
        for page in (0..<sender.identity.pageCount).reversed() {
            for seq in (1...(try sender.sourcePartCount(page: page))).reversed() {
                let text = try sender.frame(page: page, sequence: UInt32(seq), using: crypto)
                XCTAssertLessThanOrEqual(text.utf8.count, FTLimit.qrText)
                let frame = try OpticalFrame(text: text)
                try receiver.receive(frame, using: crypto)
                let rank = receiver.independentParts, verified = receiver.verifiedBytes
                try receiver.receive(frame, using: crypto)
                XCTAssertEqual(receiver.independentParts, rank)
                XCTAssertEqual(receiver.verifiedBytes, verified)
            }
        }
        XCTAssertTrue(receiver.isComplete)
        XCTAssertEqual(receiver.verifiedBytes, envelope.count)
        XCTAssertEqual(try receiver.envelope(using: crypto), envelope)
    }
    func testRepairOnlyRecoveryWithDeterministicLossAndInterruption() throws {
        let envelope = try makeEnvelope(size: 40000), sender = try OpticalSender(envelope: envelope, using: crypto)
        var receiver = OpticalReceiver(selecting: try OpticalFrame(text: sender.frame(page: 0, sequence: 999, using: crypto)))
        // Miss *all* systematic symbols, start midstream, and discard bursts.
        for seq in 150...1800 {
            if seq % 11 < 4 { continue }
            for page in 0..<sender.identity.pageCount {
                let frame = try OpticalFrame(text: sender.frame(page: page, sequence: UInt32(seq), using: crypto))
                try receiver.receive(frame, using: crypto)
            }
            if receiver.isComplete { break }
        }
        XCTAssertTrue(receiver.isComplete)
        XCTAssertEqual(try receiver.envelope(using: crypto), envelope)
    }
    func testSinglePartFinalPageUsesStandardSyntax() throws {
        // Minimal padding makes a 32768+74-byte envelope: a single-part final page.
        let envelope = try makeEnvelope(size: 32750), sender = try OpticalSender(envelope: envelope, using: crypto)
        XCTAssertEqual(envelope.count, 32768 + 74)
        let text = try sender.frame(page: 1, sequence: 123, using: crypto)
        let frame = try OpticalFrame(text: text)
        XCTAssertEqual(frame.expectedPageLength, 74)
        XCTAssertEqual(frame.ur.split(separator: "/").count, 2)
        var receiver = OpticalReceiver(selecting: frame)
        try receiver.receive(frame, using: crypto)
        XCTAssertEqual(receiver.verifiedBytes, 74)
        XCTAssertFalse(receiver.isComplete)
        receiver.resetIncompletePage(1)
        XCTAssertEqual(receiver.verifiedBytes, 74)
    }
    func testWrapperRejectsAmbiguityAndNeverSwitchesSource() throws {
        let sender = try OpticalSender(envelope: makeEnvelope(size: 100), using: crypto)
        let text = try sender.frame(page: 0, sequence: 1, using: crypto)
        let frame = try OpticalFrame(text: text)
        var receiver = OpticalReceiver(selecting: frame)
        for bad in [text.lowercased(), "https://example.invalid/" + text,
                    text.replacingOccurrences(of: "/4170/", with: "/04170/"),
                    text.replacingOccurrences(of: "/0-1/", with: "/1-1/"),
                    text.replacingOccurrences(of: "/0-1/", with: "/0-128/"), text + String(repeating: "A", count: 2049)] {
            XCTAssertNotEqual(bad, text)
            XCTAssertThrowsError(try OpticalFrame(text: bad))
        }
        let other = try OpticalFrame(text: text.replacingOccurrences(of: frame.identity.transferID.ftHex, with: String(repeating: "A", count: 32)))
        XCTAssertThrowsError(try receiver.receive(other, using: crypto))
        XCTAssertEqual(receiver.identity, frame.identity)
        XCTAssertEqual(receiver.verifiedBytes, 0)
        let changedSeq = text.replacingOccurrences(of: "UR:BYTES/1-", with: "UR:BYTES/2-")
        XCTAssertThrowsError(try receiver.receive(OpticalFrame(text: changedSeq), using: crypto))
        XCTAssertEqual(receiver.independentParts, 0)
    }
    func testPagePoisonIsRejectedWithoutPublishingCiphertext() throws {
        let sender = try OpticalSender(envelope: makeEnvelope(size: 100), using: crypto)
        let first = try OpticalFrame(text: sender.frame(page: 0, sequence: 1, using: crypto))
        var receiver = OpticalReceiver(selecting: first)
        let fakeHash = String(repeating: "A", count: 64)
        for seq in 1...(try sender.sourcePartCount(page: 0)) {
            let text = try sender.frame(page: 0, sequence: UInt32(seq), using: crypto)
            var fields = text.split(separator: "/", maxSplits: 6, omittingEmptySubsequences: false).map(String.init)
            fields[5] = fakeHash
            let poisoned = try OpticalFrame(text: fields.joined(separator: "/"))
            if seq == (try sender.sourcePartCount(page: 0)) { XCTAssertThrowsError(try receiver.receive(poisoned, using: crypto)) }
            else { try receiver.receive(poisoned, using: crypto) }
        }
        XCTAssertEqual(receiver.verifiedBytes, 0)
        XCTAssertEqual(receiver.independentParts, 0)
        XCTAssertThrowsError(try receiver.envelope(using: crypto))
        for seq in 1...(try sender.sourcePartCount(page: 0)) {
            try receiver.receive(OpticalFrame(text: sender.frame(page: 0, sequence: UInt32(seq), using: crypto)), using: crypto)
        }
        XCTAssertTrue(receiver.isComplete)
    }
    func testScanStallDistinguishesLostVisibilityFromDependentFountainFrames() throws {
        let envelope = try makeEnvelope(size: 100), sender = try OpticalSender(envelope: envelope, using: crypto)
        let first = try OpticalFrame(text: sender.frame(page: 0, sequence: 1, using: crypto))
        var receiver = OpticalReceiver(selecting: first)
        let count = try sender.sourcePartCount(page: 0)
        for sequence in 1..<count {
            try receiver.receive(OpticalFrame(text: sender.frame(page: 0, sequence: UInt32(sequence), using: crypto)), using: crypto)
        }
        let start = ContinuousClock.now, generation = UUID()
        var monitor = try XCTUnwrap(OpticalScanStallMonitor(identity: receiver.identity,
            generation: generation, receivedParts: receiver.receivedParts, at: start))
        XCTAssertNil(monitor.guidance(at: start.advanced(by: .seconds(7))))
        XCTAssertEqual(monitor.guidance(at: start.advanced(by: .seconds(8))), .noRecentFrames)

        // These are different, well-formed repair symbols, but each equation is
        // dependent on pieces already held. Counting callbacks or distinct texts
        // as progress would hide the stalled last piece indefinitely.
        let message = URPageEncoder(page: envelope)
        let degree = URDegree(count: count)
        var sequence = UInt32(count)
        for second in 1...20 {
            let dependent = try ((sequence + 1)...(sequence + 512)).first { candidate in
                try !degree.indexes(sequence: candidate, checksum: message.checksum, using: crypto).contains(count - 1)
            }
            sequence = try XCTUnwrap(dependent)
            let frame = try OpticalFrame(text: sender.frame(page: 0, sequence: sequence, using: crypto))
            try receiver.receive(frame, using: crypto)
            XCTAssertEqual(receiver.receivedParts, count - 1)
            XCTAssertTrue(monitor.record(identity: frame.identity, generation: generation,
                receivedParts: receiver.receivedParts, matchingFrame: true, at: start.advanced(by: .seconds(second))))
            if second < 15 { XCTAssertNil(monitor.guidance(at: start.advanced(by: .seconds(second)))) }
            else { XCTAssertEqual(monitor.guidance(at: start.advanced(by: .seconds(second))), .noNewInformation) }
        }
        // A held duplicate is still visible, but still does not recover a piece.
        try receiver.receive(first, using: crypto)
        monitor.record(identity: first.identity, generation: generation, receivedParts: receiver.receivedParts,
                       matchingFrame: true, at: start.advanced(by: .seconds(21)))
        XCTAssertEqual(monitor.guidance(at: start.advanced(by: .seconds(21))), .noNewInformation)
        XCTAssertEqual(monitor.guidance(at: start.advanced(by: .seconds(29))), .noRecentFrames)

        // Guidance never times out or discards the decoder. A fresh independent
        // source symbol finishes the same immutable encrypted transfer.
        try receiver.receive(OpticalFrame(text: sender.frame(page: 0, sequence: UInt32(count), using: crypto)), using: crypto)
        monitor.record(identity: receiver.identity, generation: generation, receivedParts: receiver.receivedParts,
                       matchingFrame: true, at: start.advanced(by: .seconds(30)))
        XCTAssertEqual(try receiver.envelope(using: crypto), envelope)
        XCTAssertNil(monitor.guidance(at: start.advanced(by: .seconds(300))))
    }

    func testScanStallIgnoresForeignRetiredAndReversedClockSamples() throws {
        let sender = try OpticalSender(envelope: makeEnvelope(size: 100), using: crypto)
        let text = try sender.frame(page: 0, sequence: 1, using: crypto)
        let frame = try OpticalFrame(text: text)
        let foreign = try OpticalFrame(text: text.replacingOccurrences(of: frame.identity.transferID.ftHex,
                                                                       with: String(repeating: "A", count: 32)))
        let start = ContinuousClock.now, generation = UUID()
        var monitor = try XCTUnwrap(OpticalScanStallMonitor(identity: frame.identity, generation: generation,
                                                           receivedParts: 1, at: start))
        XCTAssertFalse(monitor.record(identity: foreign.identity, generation: generation, receivedParts: 2,
                                      matchingFrame: true, at: start.advanced(by: .seconds(100))))
        XCTAssertFalse(monitor.record(identity: frame.identity, generation: UUID(), receivedParts: 2,
                                      matchingFrame: true, at: start.advanced(by: .seconds(100))))
        XCTAssertFalse(monitor.record(identity: frame.identity, generation: generation, receivedParts: Int.max,
                                      matchingFrame: true, at: start.advanced(by: .seconds(100))))
        XCTAssertFalse(monitor.record(identity: frame.identity, generation: generation, receivedParts: -1,
                                      matchingFrame: true, at: start.advanced(by: .seconds(100))))
        XCTAssertEqual(monitor.guidance(at: start.advanced(by: .seconds(8))), .noRecentFrames)
        XCTAssertTrue(monitor.record(identity: frame.identity, generation: generation, receivedParts: 1,
                                     matchingFrame: true, at: start.advanced(by: .seconds(15))))
        XCTAssertEqual(monitor.guidance(at: start.advanced(by: .seconds(15))), .noNewInformation)
        XCTAssertFalse(monitor.record(identity: frame.identity, generation: generation, receivedParts: 2,
                                      matchingFrame: true, at: start.advanced(by: .seconds(14))))
        XCTAssertNil(monitor.guidance(at: start.advanced(by: .seconds(14))))
        XCTAssertEqual(monitor.guidance(at: start.advanced(by: .seconds(16))), .noNewInformation)
        XCTAssertTrue(monitor.record(identity: frame.identity, generation: generation, receivedParts: 2,
                                     matchingFrame: true, at: start.advanced(by: .seconds(17))))
        XCTAssertNil(monitor.guidance(at: start.advanced(by: .seconds(17))))
    }

    func testScanStallResetsOnlyForAnExplicitNewGenerationOrCameraBaseline() throws {
        let sender = try OpticalSender(envelope: makeEnvelope(size: 100), using: crypto)
        let identity = sender.identity, start = ContinuousClock.now, generation = UUID()
        var monitor = try XCTUnwrap(OpticalScanStallMonitor(identity: identity, generation: generation,
                                                           receivedParts: 3, at: start))
        // A damaged page may lose equations. Regaining its old rank is not new
        // recovery and must not keep a repeated poisoning/reset cycle healthy.
        for (second, count) in [(15, 0), (16, 1), (17, 2), (18, 3)] {
            monitor.record(identity: identity, generation: generation, receivedParts: count,
                           matchingFrame: true, at: start.advanced(by: .seconds(second)))
            XCTAssertEqual(monitor.guidance(at: start.advanced(by: .seconds(second))), .noNewInformation)
        }
        let repairedGeneration = UUID(), resumed = start.advanced(by: .seconds(1000))
        var repaired = try XCTUnwrap(OpticalScanStallMonitor(identity: identity, generation: repairedGeneration,
                                                            receivedParts: 0, at: resumed))
        XCTAssertFalse(repaired.record(identity: identity, generation: generation, receivedParts: 3,
                                       matchingFrame: true, at: resumed.advanced(by: .seconds(100))))
        XCTAssertNil(repaired.guidance(at: resumed))
        XCTAssertNil(repaired.guidance(at: resumed.advanced(by: .seconds(7))))
        XCTAssertEqual(repaired.guidance(at: resumed.advanced(by: .seconds(8))), .noRecentFrames)
        XCTAssertTrue(repaired.record(identity: identity, generation: repairedGeneration, receivedParts: 1,
                                      matchingFrame: true, at: resumed.advanced(by: .seconds(9))))
        XCTAssertNil(repaired.guidance(at: resumed.advanced(by: .seconds(9))))
        XCTAssertNil(OpticalScanStallMonitor(identity: identity, generation: generation,
                                           receivedParts: identity.sourcePartCount, at: start))
        XCTAssertNil(OpticalScanStallMonitor(identity: identity, generation: generation, receivedParts: -1, at: start))
    }

    func testSenderSchedulingAndBounds() throws {
        var sender = try OpticalSender(envelope: makeEnvelope(size: 40000), using: crypto)
        let expectedPages = Array(repeating: 0, count: 8) + Array(repeating: 1, count: 16)
            + Array(repeating: 0, count: 8) + [0, 1, 0, 1, 0]
        for expected in expectedPages + expectedPages {
            let frame = try OpticalFrame(text: sender.next(using: crypto))
            XCTAssertEqual(frame.pageIndex, expected)
        }
        XCTAssertEqual(try OpticalFrame(text: sender.next(repairPage: 1, using: crypto)).pageIndex, 1)
        XCTAssertThrowsError(try sender.next(repairPage: -1, using: crypto))
        XCTAssertThrowsError(try sender.frame(page: 0, sequence: 0, using: crypto))
        XCTAssertThrowsError(try OpticalSender(envelope: Data(repeating: 0, count: FTLimit.envelope + 1), using: crypto))
    }
    func testPageScheduleIsBoundedFairAndVisitsEveryPositionUnderPeriodicCapture() throws {
        for count in 1...128 {
            let schedule = try OpticalPageSchedule(pageCount: count)
            XCTAssertGreaterThanOrEqual(schedule.cycleLength, 16 * count)
            XCTAssertLessThanOrEqual(schedule.cycleLength, 32 * count)
            let cycle = (0..<schedule.cycleLength).map { schedule.page(at: $0) }
            XCTAssertEqual(Set(cycle), Set(0..<count))
            let visits = (0..<count).map { page in cycle.filter { $0 == page }.count }
            XCTAssertGreaterThanOrEqual(try XCTUnwrap(visits.min()), 16)
            XCTAssertLessThanOrEqual(try XCTUnwrap(visits.max()) - XCTUnwrap(visits.min()), 1)
            // Begin mid-cycle. Coprimality must expose every position, not just
            // statistically plausible numbers of symbols from each page.
            for step in [2, 3, 4, 5, 6, 8, 16, 24, 32, 64] where step < schedule.cycleLength {
                let sampled = Set((0..<schedule.cycleLength).map {
                    (step - 1 + $0 * step) % schedule.cycleLength
                })
                XCTAssertEqual(sampled.count, schedule.cycleLength, "pages=\(count), stride=\(step)")
                XCTAssertEqual(Set(sampled.map { schedule.page(at: $0) }), Set(0..<count))
            }
            XCTAssertTrue((0..<count).contains(schedule.page(at: Int.max)))
        }
        for count in [-1, 0, 129, Int.max] {
            XCTAssertThrowsError(try OpticalPageSchedule(pageCount: count))
        }
    }

    func testLiveCaptureNoLongerStarvesAnEntirePageAtTheOldBurstPeriod() throws {
        let envelope = try makeEnvelope(size: 40000)
        var sender = try OpticalSender(envelope: envelope, using: crypto)
        let first = try OpticalFrame(text: sender.frame(page: 0, sequence: 1, using: crypto))
        let patterns = [(step: 16, phase: 0), (step: 16, phase: 15),
                        (step: 32, phase: 0), (step: 32, phase: 31)]
        var receivers = patterns.map { _ in OpticalReceiver(selecting: first) }
        for emission in 0..<16000 {
            let text = try sender.next(using: crypto)
            let interested = patterns.indices.filter {
                !receivers[$0].isComplete && emission % patterns[$0].step == patterns[$0].phase
            }
            if interested.isEmpty { continue }
            let frame = try OpticalFrame(text: text)
            for index in interested { try receivers[index].receive(frame, using: crypto) }
            if receivers.allSatisfy(\.isComplete) { break }
        }
        for receiver in receivers { XCTAssertEqual(try receiver.envelope(using: crypto), envelope) }

        // Negative control: the previous (emission / 8) % pageCount schedule
        // always chooses page zero at stride 16, regardless of how many fresh
        // repair parts it emits. Even granting all its source parts cannot
        // reconstruct page one.
        var legacy = OpticalReceiver(selecting: first)
        let count = try sender.sourcePartCount(page: 0)
        for sample in 0..<count {
            let page = ((sample * 16) / 8) % sender.identity.pageCount
            XCTAssertEqual(page, 0)
            try legacy.receive(OpticalFrame(text: sender.frame(page: page,
                sequence: UInt32(sample + 1), using: crypto)), using: crypto)
        }
        XCTAssertEqual(legacy.verifiedPages, 1)
        XCTAssertFalse(legacy.isComplete)
        XCTAssertThrowsError(try legacy.envelope(using: crypto))
    }

    func testTargetedRepairAndNormalPlaybackKeepPerPageSequencesMonotonic() throws {
        let envelope = try makeEnvelope(size: 40000)
        var sender = try OpticalSender(envelope: envelope, using: crypto)
        var lastRepair = [UInt32](repeating: 0, count: sender.identity.pageCount)
        var observed = [Int](repeating: 0, count: sender.identity.pageCount)
        for emission in 0..<800 {
            let target: Int? = emission % 11 == 0 ? 1 : nil
            let frame = try OpticalFrame(text: sender.next(repairPage: target, using: crypto))
            if let target { XCTAssertEqual(frame.pageIndex, target) }
            observed[frame.pageIndex] += 1
            let part = try URPart(cbor: Bytewords.decode(frame.ur.split(separator: "/")[2]))
            let sourceCount = try sender.sourcePartCount(page: frame.pageIndex)
            if part.sequence > sourceCount {
                XCTAssertGreaterThan(part.sequence, lastRepair[frame.pageIndex])
                lastRepair[frame.pageIndex] = part.sequence
            }
        }
        for page in observed.indices {
            XCTAssertGreaterThan(observed[page], 300)
            XCTAssertGreaterThan(lastRepair[page], UInt32(try sender.sourcePartCount(page: page)))
        }
        var unchanged = sender
        XCTAssertThrowsError(try sender.next(repairPage: sender.identity.pageCount, using: crypto))
        XCTAssertEqual(try sender.next(using: crypto), try unchanged.next(using: crypto))
    }

    func testSourceReplayCadenceEscapesEveryThirdReplayCapture() throws {
        let envelope = try makeEnvelope(size: 5000)
        var sender = try OpticalSender(envelope: envelope, using: crypto)
        let count = try sender.sourcePartCount(page: 0)
        XCTAssertEqual(count, 33)
        let first = try OpticalFrame(text: sender.frame(page: 0, sequence: 1, using: crypto))
        var receiver = OpticalReceiver(selecting: first), legacy = OpticalReceiver(selecting: first)
        let period = try XCTUnwrap(OpticalCadence.primeLength(atLeast: 9 * count, atMost: 10 * count))
        XCTAssertEqual(period, 307)
        for emission in 0..<(count + 8 + 27 * period) {
            let text = try sender.next(using: crypto)
            guard emission >= count + 8, (emission - count - 8).isMultiple(of: 27) else { continue }
            let frame = try OpticalFrame(text: text)
            let part = try URPart(cbor: Bytewords.decode(frame.ur.split(separator: "/")[2]))
            // Deliberately discard every repair. The bounded replay cycle alone
            // must eventually expose all 33 original fragments.
            if part.sequence <= count { try receiver.receive(frame, using: crypto) }
        }
        XCTAssertEqual(try receiver.envelope(using: crypto), envelope)
        for sample in 0..<period {
            // With a constant nine-emission interval, stride 27 reaches only
            // source 1, 4, 7, ... forever, so replaying cannot repair this loss.
            let sequence = UInt32(1 + (sample * 3) % count)
            try legacy.receive(OpticalFrame(text: sender.frame(page: 0, sequence: sequence, using: crypto)), using: crypto)
        }
        XCTAssertEqual(legacy.independentParts, 11)
        XCTAssertThrowsError(try legacy.envelope(using: crypto))
        for sourceCount in 2...129 {
            let length = try XCTUnwrap(OpticalCadence.primeLength(atLeast: 9 * sourceCount, atMost: 10 * sourceCount))
            XCTAssertLessThanOrEqual(length - 9 * sourceCount, sourceCount)
        }
    }

    func testLateReceiverCanRecoverFromRepeatedSystematicPartsWhileFreshRepairsContinue() throws {
        let envelope = try makeEnvelope(size: 100)
        var sender = try OpticalSender(envelope: envelope, using: crypto)
        let count = try sender.sourcePartCount(page: 0)
        // This camera arrives after the entire original systematic pass and is
        // assumed to lose every mixed repair frame. Replayed source information
        // must still deliver the exact complete encrypted envelope.
        for _ in 0..<count { _ = try sender.next(using: crypto) }
        var receiver: OpticalReceiver?
        var lastRepair = UInt32(count), repairCount = 0
        var replayed = Set<UInt32>()
        for _ in 0..<(count * 10) {
            let frame = try OpticalFrame(text: sender.next(using: crypto))
            let components = frame.ur.split(separator: "/")
            let sequence = try XCTUnwrap(UInt32(components[1].split(separator: "-")[0]))
            if sequence > count {
                XCTAssertGreaterThan(sequence, lastRepair, "systematic replay must not reset the independent repair sequence")
                lastRepair = sequence; repairCount += 1
                continue
            }
            replayed.insert(sequence)
            if receiver == nil { receiver = OpticalReceiver(selecting: frame) }
            try receiver?.receive(frame, using: crypto)
            if receiver?.isComplete == true { break }
        }
        XCTAssertEqual(replayed, Set((1...count).map(UInt32.init)))
        XCTAssertGreaterThan(repairCount, count)
        XCTAssertEqual(try receiver?.envelope(using: crypto), envelope)
    }
}
