import Foundation
import XCTest
@testable import FrankenTermKit

final class OpticalCheckpointTests: XCTestCase {
    private let crypto = TestCryptography()
    private let domain = Data("FrankenTerm/FTDC/v1/checksum\0".utf8)

    /// Deliberately envelope-shaped opaque bytes, not an authenticated archive.
    /// These tests exercise the actual framing, fountain codec and checksums;
    /// decrypt/import authorization remains a separate boundary.
    private func envelope(blocks: Int = 1, mode: FTAGMode = .code, tag: UInt8 = 23) throws -> Data {
        var data = Data("FTAG".utf8) + Data([1, mode.rawValue, 0, 0]) + Data(repeating: tag, count: 16)
        data.appendBE(UInt64(blocks * 4096 + 16), bytes: 4)
        data.appendBE(UInt64(mode.modeHeaderLength), bytes: 2)
        data += Data(repeating: 85, count: mode.modeHeaderLength)
        data += Data((0..<(blocks * 4096 + 16)).map { UInt8(truncatingIfNeeded: $0 * 37 + $0 / 127) })
        _ = try FTAGHeader(envelope: data)
        return data
    }
    private func frame(_ sender: OpticalSender, _ page: Int = 0, _ sequence: UInt32 = 1) throws -> OpticalFrame {
        try OpticalFrame(text: sender.frame(page: page, sequence: sequence, using: crypto))
    }
    private func finish(_ receiver: inout OpticalReceiver, _ sender: OpticalSender) throws {
        for page in 0..<sender.identity.pageCount {
            for sequence in 1...(try sender.sourcePartCount(page: page)) {
                try receiver.receive(frame(sender, page, UInt32(sequence)), using: crypto)
            }
        }
    }
    private func sealed(_ body: Data) -> Data { body + crypto.sha256(domain + body) }
    private func wrapper(identity: OpticalFrame.Identity, entries: [Data], version: UInt64 = 1) -> Data {
        var body = Data("FTDC".utf8) + CBORWriter.array(5) + CBORWriter.uint(version)
        body += CBORWriter.bytes(identity.transferID) + CBORWriter.bytes(identity.envelopeDigest)
        body += CBORWriter.uint(UInt64(identity.envelopeLength)) + CBORWriter.array(entries.count)
        for entry in entries { body += entry }
        return sealed(body)
    }
    private func entry(index: UInt64, digest: Data, kind: UInt64, payload: Data) -> Data {
        CBORWriter.array(4) + CBORWriter.uint(index) + CBORWriter.bytes(digest) + CBORWriter.uint(kind) + payload
    }
    private func row(_ pivot: UInt64, _ words: [UInt64], bytes: Int = 251) -> Data {
        var mask = Data()
        for word in words { mask.appendBE(word, bytes: 8) }
        return CBORWriter.array(3) + CBORWriter.uint(pivot) + CBORWriter.bytes(mask)
            + CBORWriter.bytes(Data(repeating: 17, count: bytes))
    }
    private func partial(_ rows: [Data], count: UInt64 = 4, length: UInt64 = 1003, checksum: UInt64 = 123) -> Data {
        CBORWriter.array(4) + CBORWriter.uint(count) + CBORWriter.uint(length) + CBORWriter.uint(checksum)
            + CBORWriter.array(rows.count) + rows.reduce(into: Data()) { $0 += $1 }
    }
    private func decodePartial(_ bytes: Data, expected: Int = 1003) throws -> URPageDecoder {
        var reader = CBORReader(bytes)
        let decoder = try URPageDecoder(checkpoint: &reader, expectedMessageLength: expected)
        try reader.finish()
        return decoder
    }

    func testSHA256OracleControl() {
        XCTAssertEqual(crypto.sha256(Data("abc".utf8)).ftHex,
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD")
    }
    func testVerifiedPageCheckpointStillOmitsIncompleteEquations() throws {
        let bytes = try envelope(blocks: 8), sender = try OpticalSender(envelope: bytes, using: crypto)
        var receiver = OpticalReceiver(selecting: try frame(sender))
        for sequence in 1...4 { try receiver.receive(frame(sender, 0, UInt32(sequence)), using: crypto) }
        XCTAssertNil(try receiver.checkpoint(using: crypto))
        try receiver.receive(frame(sender, 1), using: crypto)
        let saved = try XCTUnwrap(receiver.checkpoint(using: crypto))
        XCTAssertEqual(saved.verifiedBytes, 74)
        let encoded = try saved.encoded(using: crypto)
        XCTAssertEqual(Data(encoded.prefix(8)), Data([70, 84, 83, 67, 0, 1, 0, 0]))
        let verifiedOnly = try OpticalReceiver(resuming: OpticalScanCheckpoint(data: encoded, using: crypto), using: crypto)
        XCTAssertEqual(verifiedOnly.independentParts, 0)
        XCTAssertEqual(verifiedOnly.verifiedPages, 1)
        let partial = try OpticalReceiver(decoderCheckpoint: receiver.decoderCheckpoint(using: crypto), using: crypto)
        XCTAssertEqual(partial.independentParts, 4)
        XCTAssertEqual(try XCTUnwrap(partial.checkpoint(using: crypto)).encoded(using: crypto), encoded)
    }
    func testCheckpointFormatsCannotBeMisinterpreted() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        var receiver = OpticalReceiver(selecting: try frame(sender))
        try finish(&receiver, sender)
        let verified = try XCTUnwrap(receiver.checkpoint(using: crypto)).encoded(using: crypto)
        let decoder = try receiver.decoderCheckpoint(using: crypto)
        XCTAssertThrowsError(try OpticalScanCheckpoint(data: decoder, using: crypto))
        XCTAssertThrowsError(try OpticalReceiver(decoderCheckpoint: verified, using: crypto))
        var substituted = Data(decoder.dropLast(32))
        substituted.replaceSubrange(0..<4, with: Data("FTSC".utf8))
        substituted += crypto.sha256(Data("FrankenTerm/FTSC/v1/checkpoint\0".utf8) + substituted)
        XCTAssertThrowsError(try OpticalScanCheckpoint(data: substituted, using: crypto))
        var other = Data(verified.dropLast(32))
        other.replaceSubrange(0..<4, with: Data("FTDC".utf8))
        XCTAssertThrowsError(try OpticalReceiver(decoderCheckpoint: sealed(other), using: crypto))
    }
    func testSinglePageMidScanResumeKeepsIndependentParts() throws {
        let bytes = try envelope(), sender = try OpticalSender(envelope: bytes, using: crypto)
        var receiver = OpticalReceiver(selecting: try frame(sender))
        for sequence in 1...8 { try receiver.receive(frame(sender, 0, UInt32(sequence)), using: crypto) }
        XCTAssertEqual(receiver.verifiedPages, 0)
        XCTAssertEqual(receiver.independentParts, 8)
        let checkpoint = try receiver.decoderCheckpoint(using: crypto)
        var resumed = try OpticalReceiver(decoderCheckpoint: checkpoint, using: crypto)
        XCTAssertEqual(resumed.identity, receiver.identity)
        XCTAssertEqual(resumed.receivedParts, 8)
        XCTAssertEqual(try resumed.decoderCheckpoint(using: crypto), checkpoint)
        try resumed.receive(frame(sender, 0, 1), using: crypto)
        XCTAssertEqual(resumed.receivedParts, 8, "a duplicate after resume is not progress")
        for sequence in 9...(try sender.sourcePartCount(page: 0)) {
            try resumed.receive(frame(sender, 0, UInt32(sequence)), using: crypto)
        }
        XCTAssertEqual(try resumed.envelope(using: crypto), bytes)
    }
    func testRepairOnlyResumeDoesNotRequireOriginalSystematicFrames() throws {
        let bytes = try envelope(blocks: 10), sender = try OpticalSender(envelope: bytes, using: crypto)
        var receiver = OpticalReceiver(selecting: try frame(sender, 0, 200))
        for sequence in 200..<240 where sequence % 3 != 0 {
            for page in 0..<sender.identity.pageCount {
                try receiver.receive(frame(sender, page, UInt32(sequence)), using: crypto)
            }
        }
        XCTAssertGreaterThan(receiver.independentParts, 0)
        XCTAssertFalse(receiver.isComplete)
        let before = receiver.receivedParts
        receiver = try OpticalReceiver(decoderCheckpoint: receiver.decoderCheckpoint(using: crypto), using: crypto)
        XCTAssertEqual(receiver.receivedParts, before)
        for sequence in 240...2000 where sequence % 11 >= 4 {
            for page in 0..<sender.identity.pageCount {
                try receiver.receive(frame(sender, page, UInt32(sequence)), using: crypto)
            }
            if receiver.isComplete { break }
        }
        XCTAssertEqual(try receiver.envelope(using: crypto), bytes)
    }
    func testVerifiedAndPartialPagesSurviveTogetherInAnyArrivalOrder() throws {
        let bytes = try envelope(blocks: 20), sender = try OpticalSender(envelope: bytes, using: crypto)
        var receiver = OpticalReceiver(selecting: try frame(sender, 2, 4))
        for sequence in 1...(try sender.sourcePartCount(page: 1)) {
            try receiver.receive(frame(sender, 1, UInt32(sequence)), using: crypto)
        }
        for sequence in [UInt32(4), 2, 9] { try receiver.receive(frame(sender, 2, sequence), using: crypto) }
        for sequence in [UInt32(7), 1, 3] { try receiver.receive(frame(sender, 0, sequence), using: crypto) }
        let checkpoint = try receiver.decoderCheckpoint(using: crypto)
        var resumed = try OpticalReceiver(decoderCheckpoint: checkpoint, using: crypto)
        XCTAssertEqual(resumed.verifiedPages, 1)
        XCTAssertEqual(resumed.verifiedBytes, FTLimit.page)
        XCTAssertEqual(resumed.independentParts, 6)
        XCTAssertEqual(resumed.incompletePageIndices, [0, 2])
        XCTAssertEqual(try resumed.decoderCheckpoint(using: crypto), checkpoint)
        try finish(&resumed, sender)
        XCTAssertEqual(try resumed.envelope(using: crypto), bytes)
    }
    func testCompletedCheckpointRevalidatesEntireEnvelope() throws {
        let bytes = try envelope(blocks: 2), sender = try OpticalSender(envelope: bytes, using: crypto)
        var receiver = OpticalReceiver(selecting: try frame(sender))
        try finish(&receiver, sender)
        let resumed = try OpticalReceiver(decoderCheckpoint: receiver.decoderCheckpoint(using: crypto), using: crypto)
        XCTAssertTrue(resumed.isComplete)
        XCTAssertEqual(try resumed.envelope(using: crypto), bytes)
    }
    func testSinglePartLastPageCanResumeWithoutTheFirstPage() throws {
        let bytes = try envelope(blocks: 8), sender = try OpticalSender(envelope: bytes, using: crypto)
        var receiver = OpticalReceiver(selecting: try frame(sender, 1))
        try receiver.receive(frame(sender, 1), using: crypto)
        var resumed = try OpticalReceiver(decoderCheckpoint: receiver.decoderCheckpoint(using: crypto), using: crypto)
        XCTAssertEqual(resumed.verifiedBytes, 74)
        XCTAssertEqual(resumed.receivedParts, 1)
        try finish(&resumed, sender)
        XCTAssertEqual(try resumed.envelope(using: crypto), bytes)
    }
    func testEmptySelectedReceiverRetainsOnlyItsExactIdentity() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        let receiver = OpticalReceiver(selecting: try frame(sender))
        let resumed = try OpticalReceiver(decoderCheckpoint: receiver.decoderCheckpoint(using: crypto), using: crypto)
        XCTAssertEqual(resumed.identity, receiver.identity)
        XCTAssertEqual(resumed.receivedParts, 0)
        XCTAssertEqual(resumed.incompletePageIndices, [0])
    }
    func testDataSliceWithNonzeroStartIndex() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        var receiver = OpticalReceiver(selecting: try frame(sender))
        try receiver.receive(frame(sender), using: crypto)
        let bytes = try receiver.decoderCheckpoint(using: crypto)
        let prefixed = Data(repeating: 0xCD, count: 19) + bytes
        let resumed = try OpticalReceiver(decoderCheckpoint: prefixed.dropFirst(19), using: crypto)
        XCTAssertEqual(try resumed.decoderCheckpoint(using: crypto), bytes)
    }
    func testEachSingleByteCorruptionAndEveryTruncationIsRejected() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        var receiver = OpticalReceiver(selecting: try frame(sender))
        for sequence in 1...5 { try receiver.receive(frame(sender, 0, UInt32(sequence)), using: crypto) }
        let bytes = try receiver.decoderCheckpoint(using: crypto)
        for index in bytes.indices {
            var altered = bytes; altered[index] ^= 1
            XCTAssertThrowsError(try OpticalReceiver(decoderCheckpoint: altered, using: crypto), "byte \(index)")
        }
        for count in 0..<bytes.count {
            XCTAssertThrowsError(try OpticalReceiver(decoderCheckpoint: Data(bytes.prefix(count)), using: crypto), "length \(count)")
        }
        XCTAssertThrowsError(try OpticalReceiver(decoderCheckpoint: bytes + Data([0]), using: crypto))
    }
    func testRecomputedChecksumCannotHideTruncatedCBOROrTrailingData() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        var receiver = OpticalReceiver(selecting: try frame(sender))
        try receiver.receive(frame(sender), using: crypto)
        let body = Data(try receiver.decoderCheckpoint(using: crypto).dropLast(32))
        for count in 4..<body.count {
            XCTAssertThrowsError(try OpticalReceiver(decoderCheckpoint: sealed(Data(body.prefix(count))), using: crypto))
        }
        XCTAssertThrowsError(try OpticalReceiver(decoderCheckpoint: sealed(body + Data([0])), using: crypto))
    }
    func testWrongVersionAndOversizedCheckpointAreRejected() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        XCTAssertThrowsError(try OpticalReceiver(decoderCheckpoint: wrapper(identity: sender.identity, entries: [], version: 2), using: crypto))
        XCTAssertThrowsError(try OpticalReceiver(decoderCheckpoint: Data(repeating: 0, count: OpticalReceiver.decoderCheckpointByteLimit + 1), using: crypto)) {
            XCTAssertEqual($0 as? FTError, .limit("scan.bytes"))
        }
        let bad = OpticalFrame.Identity(transferID: Data(repeating: 1, count: 15),
            envelopeDigest: sender.identity.envelopeDigest, envelopeLength: sender.identity.envelopeLength)
        XCTAssertThrowsError(try OpticalReceiver(decoderCheckpoint: wrapper(identity: bad, entries: []), using: crypto))
        let huge = OpticalFrame.Identity(transferID: sender.identity.transferID,
            envelopeDigest: sender.identity.envelopeDigest, envelopeLength: FTLimit.envelope + 1)
        XCTAssertThrowsError(try OpticalReceiver(decoderCheckpoint: wrapper(identity: huge, entries: []), using: crypto))
    }
    func testPageIndexesKindsAndLengthsAreValidatedBeforePublication() throws {
        let bytes = try envelope(blocks: 8), sender = try OpticalSender(envelope: bytes, using: crypto)
        let page = Data(bytes.suffix(74)), digest = crypto.sha256(page)
        let valid = entry(index: 1, digest: digest, kind: 0, payload: CBORWriter.bytes(page))
        for entries in [
            [valid, valid],
            [entry(index: 2, digest: digest, kind: 0, payload: CBORWriter.bytes(page))],
            [entry(index: UInt64.max, digest: digest, kind: 0, payload: CBORWriter.bytes(page))],
            [entry(index: 1, digest: digest, kind: 2, payload: CBORWriter.bytes(page))],
            [entry(index: 1, digest: Data(digest.dropLast()), kind: 0, payload: CBORWriter.bytes(page))],
            [entry(index: 1, digest: digest, kind: 0, payload: CBORWriter.bytes(Data(page.dropLast())))],
            [entry(index: 1, digest: digest, kind: 0, payload: CBORWriter.bytes(page + Data([0])))],
            [entry(index: 1, digest: Data(repeating: 0, count: 32), kind: 0, payload: CBORWriter.bytes(page))]
        ] {
            XCTAssertThrowsError(try OpticalReceiver(decoderCheckpoint: wrapper(identity: sender.identity, entries: entries), using: crypto))
        }
    }
    func testCompletePagesCannotSubstituteTransferIDOrOuterDigest() throws {
        let bytes = try envelope(), sender = try OpticalSender(envelope: bytes, using: crypto)
        let page = entry(index: 0, digest: crypto.sha256(bytes), kind: 0, payload: CBORWriter.bytes(bytes))
        for identity in [
            OpticalFrame.Identity(transferID: Data(repeating: 42, count: 16), envelopeDigest: sender.identity.envelopeDigest,
                                  envelopeLength: bytes.count),
            OpticalFrame.Identity(transferID: sender.identity.transferID, envelopeDigest: Data(repeating: 42, count: 32),
                                  envelopeLength: bytes.count)
        ] {
            XCTAssertThrowsError(try OpticalReceiver(decoderCheckpoint: wrapper(identity: identity, entries: [page]), using: crypto))
        }
    }
    func testPartialGeometryAndRankAreValidated() throws {
        for data in [partial([], count: 0), partial([], count: UInt64.max), partial([], length: UInt64.max),
                     partial([], checksum: UInt64.max), partial([]),
                     partial([row(0, [1]), row(1, [2]), row(2, [4]), row(3, [8])]),
                     partial([row(0, [1])], length: 1002)] {
            XCTAssertThrowsError(try decodePartial(data))
        }
        XCTAssertThrowsError(try decodePartial(partial([row(0, [1])]), expected: -1))
        XCTAssertThrowsError(try decodePartial(partial([row(0, [1])]), expected: Int.max))
    }
    func testPartialRowsRequireUniqueSortedPivotsAndExactFragments() throws {
        for rows in [[row(0, [1]), row(0, [1])], [row(1, [2]), row(0, [1])], [row(4, [16])],
                     [row(UInt64.max, [1])], [row(0, [])], [row(0, [1, 0])],
                     [row(0, [1], bytes: 250)], [row(0, [1], bytes: 252)]] {
            XCTAssertThrowsError(try decodePartial(partial(rows)))
        }
    }
    func testPartialMasksAreActuallyUpperEchelonAndDoNotSetUnusedBits() throws {
        for rows in [[row(0, [0])], [row(1, [1])], [row(1, [3])], [row(0, [17])]] {
            XCTAssertThrowsError(try decodePartial(partial(rows)))
        }
        let valid = try decodePartial(partial([row(0, [13]), row(2, [12])]))
        XCTAssertEqual(valid.rank, 2)
        XCTAssertEqual(try decodePartial(valid.checkpoint()).rank, 2)
    }
    func testMaskWordBoundariesAt64And128Columns() throws {
        for (count, length, pivot, mask) in [
            (64, 16384, 63, [UInt64(1) << 63]),
            (65, 16385, 64, [UInt64(0), 1]),
            (129, 32771, 128, [UInt64(0), 0, 1])
        ] {
            let fragment = (length + count - 1) / count
            let valid = partial([row(UInt64(pivot), mask, bytes: fragment)], count: UInt64(count), length: UInt64(length))
            XCTAssertEqual(try decodePartial(valid, expected: length).rank, 1)
            if count % 64 != 0 {
                var invalidMask = mask; invalidMask[invalidMask.count - 1] |= 2
                let bad = partial([row(UInt64(pivot), invalidMask, bytes: fragment)], count: UInt64(count), length: UInt64(length))
                XCTAssertThrowsError(try decodePartial(bad, expected: length))
            }
        }
    }
    func testRechecksPageHashAfterLoadingAnUntrustedPartialCheckpoint() throws {
        let bytes = try envelope(), sender = try OpticalSender(envelope: bytes, using: crypto)
        var original = OpticalReceiver(selecting: try frame(sender))
        try original.receive(frame(sender), using: crypto)
        var body = Data(try original.decoderCheckpoint(using: crypto).dropLast(32))
        body[body.count - 1] ^= 128
        var resumed = try OpticalReceiver(decoderCheckpoint: sealed(body), using: crypto)
        var rejected = false
        for sequence in 2...(try sender.sourcePartCount(page: 0)) {
            do { try resumed.receive(frame(sender, 0, UInt32(sequence)), using: crypto) }
            catch { rejected = true; XCTAssertEqual(error as? FTError, .checksum) }
        }
        XCTAssertTrue(rejected, "a self-computed file checksum is not page authentication")
        XCTAssertEqual(resumed.verifiedPages, 0)
        XCTAssertEqual(resumed.independentParts, 0)
        try finish(&resumed, sender)
        XCTAssertEqual(try resumed.envelope(using: crypto), bytes)
    }
    func testResetRemovesOnlyUnfinishedPageEquationsAfterResume() throws {
        let bytes = try envelope(blocks: 8), sender = try OpticalSender(envelope: bytes, using: crypto)
        var receiver = OpticalReceiver(selecting: try frame(sender))
        try receiver.receive(frame(sender), using: crypto)
        try receiver.receive(frame(sender, 1), using: crypto)
        receiver = try OpticalReceiver(decoderCheckpoint: receiver.decoderCheckpoint(using: crypto), using: crypto)
        receiver.resetIncompletePage(0); receiver.resetIncompletePage(1)
        let resumed = try OpticalReceiver(decoderCheckpoint: receiver.decoderCheckpoint(using: crypto), using: crypto)
        XCTAssertEqual(resumed.independentParts, 0)
        XCTAssertEqual(resumed.verifiedPages, 1)
        XCTAssertEqual(resumed.verifiedBytes, 74)
    }
    func testResumedReceiverRefusesUnrelatedSourceAndChangedPageDigest() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        var receiver = OpticalReceiver(selecting: try frame(sender))
        try receiver.receive(frame(sender), using: crypto)
        receiver = try OpticalReceiver(decoderCheckpoint: receiver.decoderCheckpoint(using: crypto), using: crypto)
        let unrelated = try OpticalSender(envelope: envelope(tag: 24), using: crypto)
        XCTAssertThrowsError(try receiver.receive(frame(unrelated), using: crypto))
        var fields = try sender.frame(page: 0, sequence: 2, using: crypto).split(separator: "/", maxSplits: 6).map(String.init)
        fields[5] = String(repeating: "A", count: 64)
        XCTAssertThrowsError(try receiver.receive(OpticalFrame(text: fields.joined(separator: "/")), using: crypto))
        XCTAssertEqual(receiver.receivedParts, 1)
    }
    func testRecipientEnvelopeBytesRemainOpaque() throws {
        let bytes = try envelope(mode: .recipient), sender = try OpticalSender(envelope: bytes, using: crypto)
        var receiver = OpticalReceiver(selecting: try frame(sender))
        try receiver.receive(frame(sender), using: crypto)
        receiver = try OpticalReceiver(decoderCheckpoint: receiver.decoderCheckpoint(using: crypto), using: crypto)
        try finish(&receiver, sender)
        XCTAssertEqual(try receiver.envelope(using: crypto), bytes)
        XCTAssertEqual(try FTAGHeader(envelope: receiver.envelope(using: crypto)).mode, .recipient)
    }
    func testMaximum128PagePartialStateIsBoundedAndRestorable() throws {
        let identity = OpticalFrame.Identity(transferID: Data(repeating: 1, count: 16),
            envelopeDigest: Data(repeating: 2, count: 32), envelopeLength: FTLimit.envelope)
        var entries: [Data] = []
        for page in 0..<128 {
            let count = 129, length = FTLimit.page + 3, fragment = (length + count - 1) / count
            let rows = (0..<128).map { pivot -> Data in
                var mask = [UInt64](repeating: 0, count: 3)
                mask[pivot / 64] = UInt64(1) << (pivot % 64)
                return row(UInt64(pivot), mask, bytes: fragment)
            }
            entries.append(entry(index: UInt64(page), digest: Data(repeating: 3, count: 32), kind: 1,
                                 payload: partial(rows, count: UInt64(count), length: UInt64(length))))
        }
        let bytes = wrapper(identity: identity, entries: entries)
        XCTAssertLessThan(bytes.count, OpticalReceiver.decoderCheckpointByteLimit)
        let resumed = try OpticalReceiver(decoderCheckpoint: bytes, using: crypto)
        XCTAssertEqual(resumed.independentParts, 128 * 128)
        XCTAssertEqual(resumed.verifiedPages, 0)
        XCTAssertEqual(resumed.incompletePageIndices.count, 128)
        XCTAssertEqual(try resumed.decoderCheckpoint(using: crypto), bytes)
    }
}
