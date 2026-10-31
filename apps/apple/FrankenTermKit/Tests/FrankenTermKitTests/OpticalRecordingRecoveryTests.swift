import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class OpticalRecordingRecoveryTests: XCTestCase {
    private let crypto = TestCryptography()

    private func envelope(size: Int = 100, marker: UInt8 = 41) throws -> Data {
        try FTAG.sealArchiveBytes(Data((0..<size).map { UInt8($0 % 251) }),
            code: TransferCode(secret: Data(repeating: 40, count: 16)),
            transferID: Data(repeating: marker, count: 16),
            salt: Data(repeating: 42, count: 16), nonce: Data(repeating: 43, count: 12),
            compress: false, using: crypto)
    }

    private func replacing(_ text: String, field: Int, with value: String) -> String {
        var fields = text.split(separator: "/", maxSplits: 6, omittingEmptySubsequences: false).map(String.init)
        fields[field] = value
        return fields.joined(separator: "/")
    }

    func testPartialRecordingsRecoverExactCiphertextAndGenerateFreshTargetedRepair() throws {
        let bytes = try envelope(size: 40_000)
        let sender = try OpticalSender(envelope: bytes, using: crypto)
        XCTAssertEqual(sender.identity.pageCount, 2)
        var recovery = OpticalRecordingRecovery()
        let first = try recovery.beginRecording()
        // The first recording contains a complete second page and only a few
        // independent parts of the first, with a repeated video hold frame.
        for sequence in 1...(try sender.sourcePartCount(page: 1)) {
            try recovery.observe([sender.frame(page: 1, sequence: UInt32(sequence), using: crypto)], recording: first, using: crypto)
        }
        for sequence in 1...7 {
            let text = try sender.frame(page: 0, sequence: UInt32(sequence), using: crypto)
            try recovery.observe([text, text], recording: first, using: crypto)
        }
        XCTAssertEqual(recovery.progress.verifiedPages, 1)
        XCTAssertEqual(recovery.progress.receivedParts, try sender.sourcePartCount(page: 1) + 7)
        recovery.endRecording(first)
        XCTAssertThrowsError(try recovery.envelope(using: crypto))
        let partial = recovery.progress
        let second = try recovery.beginRecording()
        XCTAssertEqual(recovery.progress, partial)
        for sequence in (8...(try sender.sourcePartCount(page: 0))).reversed() {
            try recovery.observe([sender.frame(page: 0, sequence: UInt32(sequence), using: crypto)], recording: second, using: crypto)
        }
        XCTAssertTrue(recovery.progress.isComplete)
        XCTAssertEqual(recovery.progress.verifiedBytes, bytes.count)
        XCTAssertEqual(recovery.progress.receivedParts, sender.identity.sourcePartCount)
        XCTAssertThrowsError(try recovery.envelope(using: crypto), "The native reader must relinquish its lease before publication.")
        recovery.endRecording(second)
        let recovered = try recovery.envelope(using: crypto)
        XCTAssertEqual(recovered, bytes)

        var replay = try OpticalExportSession(envelope: recovered, using: crypto)
        XCTAssertFalse(replay.hasTransferCode)
        XCTAssertThrowsError(try replay.revealTransferCode(using: crypto))
        var phone: OpticalReceiver?
        for page in [1, 0] {
            let playback = try replay.beginPlayback(symbolsPerSecond: 3, repairPage: page)
            let count = try sender.sourcePartCount(page: page)
            for index in 0...count {
                let frame = try OpticalFrame(text: replay.next(playback, using: crypto).text)
                XCTAssertEqual(frame.pageIndex, page)
                XCTAssertEqual(frame.identity, sender.identity)
                if phone == nil { phone = OpticalReceiver(selecting: frame) }
                try phone?.receive(frame, using: crypto)
                if index == count {
                    let components = frame.ur.split(separator: "/")
                    let part = try URPart(cbor: Bytewords.decode(components[2]))
                    XCTAssertGreaterThan(part.sequence, UInt32(count), "Reopened ciphertext must generate fresh repair beyond a finite recording.")
                }
            }
            replay.pause(playback)
        }
        XCTAssertEqual(try phone?.envelope(using: crypto), bytes)
        XCTAssertEqual(replay.envelope, bytes)
        XCTAssertEqual(try recovery.envelope(using: crypto), bytes)
    }

    func testSameFrameCompetingSourcesRejectBeforeSelectingEitherTransfer() throws {
        let a = try OpticalSender(envelope: envelope(marker: 51), using: crypto)
        let b = try OpticalSender(envelope: envelope(marker: 52), using: crypto)
        var recovery = OpticalRecordingRecovery()
        let recording = try recovery.beginRecording()
        XCTAssertThrowsError(try recovery.observe([
            a.frame(page: 0, sequence: 1, using: crypto), b.frame(page: 0, sequence: 1, using: crypto)
        ], recording: recording, using: crypto)) {
            XCTAssertEqual($0 as? FTError, .malformed("recording.ambiguous_sources"))
        }
        XCTAssertNil(recovery.progress.identity)
        XCTAssertEqual(recovery.progress.receivedParts, 0)
        try recovery.observe([a.frame(page: 0, sequence: 1, using: crypto)], recording: recording, using: crypto)
        XCTAssertEqual(recovery.progress.identity, a.identity)
    }

    func testLaterForeignFramesNeverSwitchOrPartiallyEnterTheSelectedTransfer() throws {
        let a = try OpticalSender(envelope: envelope(size: 100, marker: 53), using: crypto)
        // Reusing the same transfer ID does not weaken the full identity check.
        let b = try OpticalSender(envelope: envelope(size: 150, marker: 53), using: crypto)
        XCTAssertEqual(a.identity.transferID, b.identity.transferID)
        XCTAssertNotEqual(a.identity.envelopeDigest, b.identity.envelopeDigest)
        var recovery = OpticalRecordingRecovery()
        let first = try recovery.beginRecording()
        try recovery.observe([a.frame(page: 0, sequence: 1, using: crypto)], recording: first, using: crypto)
        recovery.endRecording(first)
        let second = try recovery.beginRecording()
        let before = recovery.progress
        XCTAssertThrowsError(try recovery.observe([
            a.frame(page: 0, sequence: 2, using: crypto), b.frame(page: 0, sequence: 1, using: crypto)
        ], recording: second, using: crypto))
        XCTAssertEqual(recovery.progress, before)
        try recovery.observe([a.frame(page: 0, sequence: 2, using: crypto)], recording: second, using: crypto)
        XCTAssertEqual(recovery.progress.receivedParts, before.receivedParts + 1)
        XCTAssertEqual(recovery.progress.identity, a.identity)
    }

    func testUnrelatedCodesMalformedWrappersAndOversizedTextDoNotSelectOrUnlock() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        let valid = try sender.frame(page: 0, sequence: 1, using: crypto)
        var recovery = OpticalRecordingRecovery()
        let recording = try recovery.beginRecording()
        let unrelated = ["", "https://example.invalid/transfer", "FTCODE1:0123456789ABCDEF:secret-code",
                         "FTRQ1:recipient-request", "FTQR1/not-a-frame", valid.lowercased(), String(repeating: "X", count: FTLimit.qrText + 1)]
        try recovery.observe(unrelated, recording: recording, using: crypto)
        XCTAssertNil(recovery.progress.identity)
        XCTAssertThrowsError(try recovery.observe(Array(repeating: valid, count: 9), recording: recording, using: crypto)) {
            XCTAssertEqual($0 as? FTError, .limit("recording.qr_count"))
        }
        XCTAssertNil(recovery.progress.identity)
        try recovery.observe(unrelated + [valid], recording: recording, using: crypto)
        XCTAssertEqual(recovery.progress.identity, sender.identity)
        XCTAssertEqual(recovery.progress.receivedParts, 1)
    }

    func testCancelledAndDiscardedReaderLeasesCannotContaminateResumedRecovery() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        let other = try OpticalSender(envelope: envelope(marker: 54), using: crypto)
        var recovery = OpticalRecordingRecovery()
        let old = try recovery.beginRecording()
        try recovery.observe([sender.frame(page: 0, sequence: 1, using: crypto)], recording: old, using: crypto)
        recovery.endRecording(old)
        let next = try recovery.beginRecording()
        recovery.endRecording(old)
        XCTAssertThrowsError(try recovery.beginRecording())
        XCTAssertThrowsError(try recovery.observe([sender.frame(page: 0, sequence: 2, using: crypto)], recording: old, using: crypto)) {
            XCTAssertEqual($0 as? FTError, .stale)
        }
        try recovery.observe([sender.frame(page: 0, sequence: 2, using: crypto)], recording: next, using: crypto)
        XCTAssertEqual(recovery.progress.receivedParts, 2)
        recovery.discard()
        let replacement = try recovery.beginRecording()
        recovery.endRecording(next)
        XCTAssertThrowsError(try recovery.observe([sender.frame(page: 0, sequence: 3, using: crypto)], recording: next, using: crypto))
        try recovery.observe([other.frame(page: 0, sequence: 1, using: crypto)], recording: replacement, using: crypto)
        XCTAssertEqual(recovery.progress.identity, other.identity)
        XCTAssertEqual(recovery.progress.receivedParts, 1)
    }

    func testTaskCancellationBeforeAdmissionPreservesPartialRecovery() async throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        var recovery = OpticalRecordingRecovery()
        let recording = try recovery.beginRecording()
        try recovery.observe([sender.frame(page: 0, sequence: 1, using: crypto)], recording: recording, using: crypto)
        let text = try sender.frame(page: 0, sequence: 2, using: crypto)
        let before = recovery.progress
        let task = Task { [recovery] in
            var candidate = recovery
            withUnsafeCurrentTask { $0?.cancel() }
            do {
                try candidate.observe([text], recording: recording, using: TestCryptography())
                return (false, candidate)
            } catch {
                return (error is CancellationError, candidate)
            }
        }
        let result = await task.value
        XCTAssertTrue(result.0)
        XCTAssertEqual(result.1.progress, before)
        var resumed = result.1
        resumed.endRecording(recording)
        let next = try resumed.beginRecording()
        try resumed.observe([text], recording: next, using: crypto)
        XCTAssertEqual(resumed.progress.receivedParts, before.receivedParts + 1)
    }

    func testIncorrectPageDigestNeverPublishesAndGoodRecordingCanRepairPage() throws {
        let bytes = try envelope()
        let sender = try OpticalSender(envelope: bytes, using: crypto)
        let count = try sender.sourcePartCount(page: 0)
        var recovery = OpticalRecordingRecovery()
        let first = try recovery.beginRecording()
        for sequence in 1...count {
            let text = replacing(try sender.frame(page: 0, sequence: UInt32(sequence), using: crypto),
                                 field: 5, with: String(repeating: "A", count: 64))
            if sequence == count {
                XCTAssertThrowsError(try recovery.observe([text], recording: first, using: crypto)) {
                    XCTAssertEqual($0 as? FTError, .checksum)
                }
            } else { try recovery.observe([text], recording: first, using: crypto) }
        }
        XCTAssertFalse(recovery.progress.isComplete)
        XCTAssertEqual(recovery.progress.verifiedBytes, 0)
        XCTAssertEqual(recovery.progress.receivedParts, 0)
        recovery.endRecording(first)
        XCTAssertThrowsError(try recovery.envelope(using: crypto))
        let second = try recovery.beginRecording()
        for sequence in 1...count {
            try recovery.observe([sender.frame(page: 0, sequence: UInt32(sequence), using: crypto)], recording: second, using: crypto)
        }
        recovery.endRecording(second)
        XCTAssertEqual(try recovery.envelope(using: crypto), bytes)
    }

    func testChangedPageDigestDoesNotKeepPoisonedEquations() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        var recovery = OpticalRecordingRecovery()
        let recording = try recovery.beginRecording()
        try recovery.observe([sender.frame(page: 0, sequence: 1, using: crypto)], recording: recording, using: crypto)
        let changed = replacing(try sender.frame(page: 0, sequence: 2, using: crypto),
                                field: 5, with: String(repeating: "B", count: 64))
        XCTAssertThrowsError(try recovery.observe([changed], recording: recording, using: crypto)) {
            XCTAssertEqual($0 as? FTError, .malformed("qr.changed_page_digest"))
        }
        XCTAssertEqual(recovery.progress.receivedParts, 0)
        XCTAssertEqual(recovery.progress.identity, sender.identity)
    }

    func testWrongWholeDigestOrHeaderTransferIDCannotBecomeComplete() throws {
        let sender = try OpticalSender(envelope: envelope(), using: crypto)
        let count = try sender.sourcePartCount(page: 0)
        for (field, length) in [(4, 64), (1, 32)] {
            var recovery = OpticalRecordingRecovery()
            let recording = try recovery.beginRecording()
            for sequence in 1...count {
                let text = replacing(try sender.frame(page: 0, sequence: UInt32(sequence), using: crypto),
                                     field: field, with: String(repeating: "C", count: length))
                if sequence == count {
                    XCTAssertThrowsError(try recovery.observe([text], recording: recording, using: crypto)) {
                        XCTAssertEqual($0 as? FTError, .checksum)
                    }
                } else { try recovery.observe([text], recording: recording, using: crypto) }
            }
            XCTAssertNil(recovery.progress.identity)
            XCTAssertFalse(recovery.progress.isComplete)
            recovery.endRecording(recording)
            XCTAssertThrowsError(try recovery.envelope(using: crypto))
        }
    }

    func testRecipientBoundRecordingReopensWithoutReceivingARecipientSecret() throws {
        let recipient = try RecipientSession(using: crypto)
        let bytes = try FTAG.sealArchiveBytes(Data("synthetic opaque recipient archive".utf8),
            recipient: recipient.request, transferID: Data(repeating: 71, count: 16), compress: false, using: crypto)
        let sender = try OpticalSender(envelope: bytes, using: crypto)
        var recovery = OpticalRecordingRecovery()
        let recording = try recovery.beginRecording()
        for page in 0..<sender.identity.pageCount {
            for sequence in 1...(try sender.sourcePartCount(page: page)) {
                try recovery.observe([sender.frame(page: page, sequence: UInt32(sequence), using: crypto)], recording: recording, using: crypto)
            }
        }
        recovery.endRecording(recording)
        let recovered = try recovery.envelope(using: crypto)
        XCTAssertEqual(recovered, bytes)
        var replay = try OpticalExportSession(envelope: recovered, using: crypto)
        XCTAssertEqual(replay.mode, .recipient)
        XCTAssertFalse(replay.hasTransferCode)
        XCTAssertThrowsError(try replay.revealTransferCode(using: crypto))
        XCTAssertEqual(replay.identity, sender.identity)
    }
}
