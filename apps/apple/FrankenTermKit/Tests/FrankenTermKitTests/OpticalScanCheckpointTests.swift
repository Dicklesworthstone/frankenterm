import Foundation
import XCTest
@testable import FrankenTermKit
#if canImport(CryptoKit)
import CryptoKit
#else
import CTestOpenSSL
#endif

/// Hash-only oracle. These tests exercise the actual optical codec, checkpoint
/// framing and resumption, not FTAG encryption or a second fountain decoder.
struct ScanCheckpointCryptography: AirGapCryptography {
    func sha256(_ bytes: Data) -> Data {
        #if canImport(CryptoKit)
        return Data(SHA256.hash(data: bytes))
        #else
        var result = Data(count: 32)
        result.withUnsafeMutableBytes { output in
            bytes.withUnsafeBytes { input in
                _ = SHA256(input.bindMemory(to: UInt8.self).baseAddress, bytes.count,
                           output.bindMemory(to: UInt8.self).baseAddress)
            }
        }
        return result
        #endif
    }
    func hmacSHA1(key: Data, message: Data) throws -> Data { throw FTError.unsupported("test.hash_only") }
    func randomBytes(count: Int) throws -> Data { throw FTError.unsupported("test.hash_only") }
    func seal(_ plaintext: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data { throw FTError.unsupported("test.hash_only") }
    func open(_ ciphertextAndTag: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data { throw FTError.unsupported("test.hash_only") }
    func ed25519PublicKey(seed: Data) throws -> Data { throw FTError.unsupported("test.hash_only") }
    func ecdsaPublicKey(scalar: Data, curve: SSHECDSACurve) throws -> Data { throw FTError.unsupported("test.hash_only") }
    func bcryptPBKDF(passphrase: Data, salt: Data, rounds: UInt32, outputByteCount: Int) throws -> Data { throw FTError.unsupported("test.hash_only") }
    func decryptAESCTR(_ ciphertext: Data, key: Data, iv: Data) throws -> Data { throw FTError.unsupported("test.hash_only") }
    func decryptAESCBC(_ ciphertext: Data, key: Data, iv: Data) throws -> Data { throw FTError.unsupported("test.hash_only") }
}

final class OpticalScanCheckpointTests: XCTestCase {
    let crypto = ScanCheckpointCryptography()

    // A structurally valid FTAG header and deterministic opaque payload. No test
    // claims this fixture has an authentication tag or contains a real key.
    func envelope(blocks: Int = 17, seed: UInt8 = 7) -> Data {
        var bytes = Data("FTAG".utf8) + Data([1, 1, 0, 0]) + Data(repeating: seed, count: 16)
        bytes.appendBE(UInt64(blocks * 4096 + 16), bytes: 4); bytes.appendBE(28, bytes: 2)
        bytes += Data(repeating: 3, count: 28)
        bytes += Data((0..<(blocks * 4096 + 16)).map { UInt8(truncatingIfNeeded: $0 &* 31 &+ Int(seed)) })
        return bytes
    }
    func start(_ sender: OpticalSender) throws -> OpticalReceiver {
        OpticalReceiver(selecting: try OpticalFrame(text: sender.frame(page: 0, sequence: 1, using: crypto)))
    }
    func receivePage(_ page: Int, sender: OpticalSender, receiver: inout OpticalReceiver) throws {
        for sequence in 1...(try sender.sourcePartCount(page: page)) {
            try receiver.receive(OpticalFrame(text: sender.frame(page: page, sequence: UInt32(sequence), using: crypto)), using: crypto)
        }
    }
    func snapshot(_ receiver: OpticalReceiver) throws -> Data {
        try XCTUnwrap(receiver.checkpoint(using: crypto)).encoded(using: crypto)
    }
    func rehash(_ data: Data) -> Data {
        let body = Data(data.dropLast(32))
        return body + crypto.sha256(Data("FrankenTerm/FTSC/v1/checkpoint\0".utf8) + body)
    }

    func testNoCheckpointForUnverifiedEquationsOrCodeFrame() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        var receiver = try start(sender)
        XCTAssertNil(try receiver.checkpoint(using: crypto))
        for sequence in 1...64 {
            try receiver.receive(OpticalFrame(text: sender.frame(page: 0, sequence: UInt32(sequence), using: crypto)), using: crypto)
        }
        XCTAssertEqual(receiver.independentParts, 64)
        XCTAssertNil(try receiver.checkpoint(using: crypto))
        XCTAssertThrowsError(try OpticalFrame(text: TransferCodeFrame.text(envelopeDigest: sender.identity.envelopeDigest,
                                                                         code: "NEVER-SAVE-THIS-CODE")))
    }

    func testRestartKeepsVerifiedPagesDiscardsEquationsAndFinishesSameEnvelope() throws {
        let original = envelope(), sender = try OpticalSender(envelope: original, using: crypto)
        var receiver = try start(sender)
        try receivePage(1, sender: sender, receiver: &receiver)
        for sequence in 1...31 {
            try receiver.receive(OpticalFrame(text: sender.frame(page: 0, sequence: UInt32(sequence), using: crypto)), using: crypto)
        }
        let bytes = try snapshot(receiver)
        XCTAssertEqual(bytes.count, 64 + 40 + FTLimit.page + 32)
        let saved = try OpticalScanCheckpoint(data: bytes, using: crypto)
        XCTAssertEqual(saved.identity, sender.identity)
        XCTAssertEqual(saved.verifiedPages, 1); XCTAssertEqual(saved.verifiedBytes, FTLimit.page)
        XCTAssertEqual(saved.incompletePageIndices, [0, 2])
        var restarted = try OpticalReceiver(resuming: saved, using: crypto)
        XCTAssertEqual(restarted.independentParts, 0)
        XCTAssertEqual(restarted.receivedParts, sender.identity.sourcePartCount(page: 1))
        XCTAssertThrowsError(try restarted.envelope(using: crypto))
        // Replayed verified-page symbols must not increase progress or allocate
        // equations. Page-local repair after relaunch leaves that page intact.
        let progress = restarted.receivedParts
        try receivePage(1, sender: sender, receiver: &restarted)
        restarted.resetIncompletePage(1)
        XCTAssertEqual(restarted.receivedParts, progress)
        try receivePage(2, sender: sender, receiver: &restarted)
        try receivePage(0, sender: sender, receiver: &restarted)
        XCTAssertEqual(try restarted.envelope(using: crypto), original)
    }

    func testCompleteCheckpointStillVerifiesEnvelopeAndTransferIdentity() throws {
        let sender = try OpticalSender(envelope: envelope(blocks: 1), using: crypto)
        var receiver = try start(sender)
        try receivePage(0, sender: sender, receiver: &receiver)
        let encoded = try snapshot(receiver)
        let resumed = try OpticalReceiver(resuming: OpticalScanCheckpoint(data: encoded, using: crypto), using: crypto)
        XCTAssertTrue(resumed.isComplete)
        XCTAssertEqual(try resumed.envelope(using: crypto), envelope(blocks: 1))
        for offset in [12, 28] {
            var changed = encoded; changed[offset] ^= 1
            let syntacticallyValid = try OpticalScanCheckpoint(data: rehash(changed), using: crypto)
            XCTAssertThrowsError(try OpticalReceiver(resuming: syntacticallyValid, using: crypto))
        }
    }

    func testEverySingleByteCorruptionAndTruncationFails() throws {
        let sender = try OpticalSender(envelope: envelope(blocks: 1), using: crypto)
        var receiver = try start(sender)
        try receivePage(0, sender: sender, receiver: &receiver)
        let encoded = try snapshot(receiver)
        for index in encoded.indices {
            var damaged = encoded; damaged[index] ^= 1
            XCTAssertThrowsError(try OpticalScanCheckpoint(data: damaged, using: crypto), "byte \(index)")
        }
        for size in [0, 1, 63, 64, 95, 104, 136, encoded.count - 1] {
            XCTAssertThrowsError(try OpticalScanCheckpoint(data: Data(encoded.prefix(size)), using: crypto))
        }
        XCTAssertThrowsError(try OpticalScanCheckpoint(data: encoded + Data([0]), using: crypto))
        XCTAssertThrowsError(try OpticalScanCheckpoint(data: Data(count: OpticalScanCheckpoint.maximumBytes + 1), using: crypto))
    }

    func testForgedChecksumsCannotBypassFramingOrPageChecks() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        var receiver = try start(sender)
        try receivePage(0, sender: sender, receiver: &receiver)
        let encoded = try snapshot(receiver)
        for offset in [0, 5, 7, 8, 60, 61, 63, 64, 67, 68, 71, 72, 104] {
            var damaged = encoded; damaged[offset] ^= 1
            XCTAssertThrowsError(try OpticalScanCheckpoint(data: rehash(damaged), using: crypto), "field byte \(offset)")
        }
        var trailing = Data(encoded.dropLast(32)); trailing.append(0); trailing += Data(count: 32)
        XCTAssertThrowsError(try OpticalScanCheckpoint(data: rehash(trailing), using: crypto))
    }

    func testRehashedPageRelocationStillCannotCompleteTheOriginalTransfer() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        var receiver = try start(sender)
        try receivePage(0, sender: sender, receiver: &receiver)
        var relocated = try snapshot(receiver)
        relocated[65] = 1
        // A checksum is not authentication: a structurally plausible index
        // substitution can be parsed, but cannot satisfy the selected envelope.
        receiver = try OpticalReceiver(resuming: OpticalScanCheckpoint(data: rehash(relocated), using: crypto), using: crypto)
        XCTAssertThrowsError(try receiver.receive(OpticalFrame(text: sender.frame(page: 1, sequence: 1, using: crypto)), using: crypto))
        try receivePage(0, sender: sender, receiver: &receiver)
        try receivePage(2, sender: sender, receiver: &receiver)
        XCTAssertThrowsError(try receiver.envelope(using: crypto))
    }

    func testResumedReceiverRecoversRemainingPageFromRepairSymbolsOnly() throws {
        let sender = try OpticalSender(envelope: envelope(blocks: 8), using: crypto)
        var receiver = try start(sender)
        try receivePage(1, sender: sender, receiver: &receiver)
        receiver = try OpticalReceiver(resuming: OpticalScanCheckpoint(data: snapshot(receiver), using: crypto), using: crypto)
        for sequence in 130...1000 {
            try receiver.receive(OpticalFrame(text: sender.frame(page: 0, sequence: UInt32(sequence), using: crypto)), using: crypto)
            if receiver.isComplete { break }
        }
        XCTAssertTrue(receiver.isComplete)
        XCTAssertEqual(try receiver.envelope(using: crypto), envelope(blocks: 8))
    }

    func testDuplicateAndOutOfOrderPagesAreRejected() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        var receiver = try start(sender)
        try receivePage(0, sender: sender, receiver: &receiver)
        try receivePage(1, sender: sender, receiver: &receiver)
        let encoded = try snapshot(receiver)
        let second = 64 + 40 + FTLimit.page
        var duplicate = encoded; duplicate[second + 1] = 0
        XCTAssertThrowsError(try OpticalScanCheckpoint(data: rehash(duplicate), using: crypto))
        var reversed = encoded
        let firstRecord = encoded.subdata(in: 64..<second)
        let secondRecord = encoded.subdata(in: second..<(second + 40 + FTLimit.page))
        reversed.replaceSubrange(64..<second, with: secondRecord)
        reversed.replaceSubrange(second..<(second + 40 + FTLimit.page), with: firstRecord)
        XCTAssertThrowsError(try OpticalScanCheckpoint(data: rehash(reversed), using: crypto))
    }

    func testResumeNeverAdmitsAnotherTransferOrChangedPageDigest() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        let other = try OpticalSender(envelope: envelope(seed: 8), using: crypto)
        var receiver = try start(sender)
        try receivePage(0, sender: sender, receiver: &receiver)
        receiver = try OpticalReceiver(resuming: XCTUnwrap(receiver.checkpoint(using: crypto)), using: crypto)
        let before = try snapshot(receiver)
        XCTAssertThrowsError(try receiver.receive(OpticalFrame(text: other.frame(page: 1, sequence: 1, using: crypto)), using: crypto))
        var fields = try sender.frame(page: 0, sequence: 1, using: crypto).split(separator: "/", maxSplits: 6).map(String.init)
        fields[5] = String(repeating: "0", count: 64)
        XCTAssertThrowsError(try receiver.receive(OpticalFrame(text: fields.joined(separator: "/")), using: crypto))
        XCTAssertEqual(try snapshot(receiver), before)
    }

    func testPartialFinalPageAndDataSliceResumeExactly() throws {
        let sender = try OpticalSender(envelope: envelope(blocks: 8), using: crypto)
        var receiver = try start(sender)
        try receivePage(1, sender: sender, receiver: &receiver)
        let bytes = try snapshot(receiver)
        let prefixed = Data([1, 2, 3]) + bytes
        let checkpoint = try OpticalScanCheckpoint(data: prefixed.dropFirst(3), using: crypto)
        XCTAssertEqual(checkpoint.verifiedBytes, 74)
        receiver = try OpticalReceiver(resuming: checkpoint, using: crypto)
        try receivePage(0, sender: sender, receiver: &receiver)
        XCTAssertEqual(try receiver.envelope(using: crypto), envelope(blocks: 8))
    }

    func testCheckpointOrderingIsDeterministicAndMaximumBoundsAreExact() throws {
        let envelopeLength = FTLimit.envelope
        let identity = OpticalFrame.Identity(transferID: Data(count: 16), envelopeDigest: Data(count: 32), envelopeLength: envelopeLength)
        var pages: [Int: Data] = [:], digests: [Int: Data] = [:]
        for index in (0..<128).reversed() {
            let page = Data(repeating: UInt8(index), count: FTLimit.page)
            pages[index] = page; digests[index] = crypto.sha256(page)
        }
        let saved = try OpticalScanCheckpoint(identity: identity, pages: pages, pageDigests: digests, using: crypto)
        let encoded = try saved.encoded(using: crypto)
        XCTAssertEqual(encoded.count, OpticalScanCheckpoint.maximumBytes)
        let decoded = try OpticalScanCheckpoint(data: encoded, using: crypto)
        XCTAssertEqual(try decoded.encoded(using: crypto), encoded)
        XCTAssertEqual(decoded.verifiedPages, 128); XCTAssertTrue(decoded.incompletePageIndices.isEmpty)
        // A hash-consistent cache is not enough to make these fabricated bytes
        // a valid FTAG envelope, let alone authenticated private material.
        XCTAssertThrowsError(try OpticalReceiver(resuming: decoded, using: crypto))
    }
}
