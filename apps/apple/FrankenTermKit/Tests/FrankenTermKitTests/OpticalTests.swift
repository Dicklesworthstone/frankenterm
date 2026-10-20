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
    func testSenderSchedulingAndBounds() throws {
        var sender = try OpticalSender(envelope: makeEnvelope(size: 40000), using: crypto)
        for i in 0..<24 {
            let frame = try OpticalFrame(text: sender.next(using: crypto))
            XCTAssertEqual(frame.pageIndex, (i / 8) % 2)
        }
        XCTAssertEqual(try OpticalFrame(text: sender.next(repairPage: 1, using: crypto)).pageIndex, 1)
        XCTAssertThrowsError(try sender.next(repairPage: -1, using: crypto))
        XCTAssertThrowsError(try sender.frame(page: 0, sequence: 0, using: crypto))
        XCTAssertThrowsError(try OpticalSender(envelope: Data(repeating: 0, count: FTLimit.envelope + 1), using: crypto))
    }
}
