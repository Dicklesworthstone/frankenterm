import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class OpticalRecordingTests: XCTestCase {
    private let crypto = TestCryptography()

    private func environment() -> (OfflineRestore<TestCryptography>, WorkspaceStore, StoreTestVault, WorkspaceNetworkActivity) {
        let vault = StoreTestVault(), network = WorkspaceNetworkActivity()
        let store = WorkspaceStore(persistence: StoreTestPersistence(), vault: vault)
        return (OfflineRestore(using: crypto, store: store, networkActivity: network), store, vault, network)
    }

    private func export() throws -> FTAG.Export {
        let (key, fingerprint) = try sampleKey(using: crypto)
        return try FTAG.seal(AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint)),
                                           keys: [key], using: crypto), using: crypto)
    }

    private func sender(transfer: UInt8 = 3) throws -> OpticalSender {
        let code = try TransferCode(secret: Data(repeating: 2, count: 16))
        let envelope = try FTAG.sealArchiveBytes(Data(repeating: 7, count: 40_000), code: code,
            transferID: Data(repeating: transfer, count: 16), salt: Data(repeating: 4, count: 16),
            nonce: Data(repeating: 5, count: 12), compress: false, using: crypto)
        return try OpticalSender(envelope: envelope, using: crypto)
    }

    func testRecordingAdmitsOnlyBoundedLocalFormatsAndDecodedDimensions() throws {
        for ext in ["mp4", "MP4", "mov", "m4v", "png", "apng"] {
            XCTAssertTrue(OpticalRecordingLimits.accepts(URL(fileURLWithPath: "/tmp/transfer.\(ext)")))
        }
        for path in ["https://example.invalid/transfer.mp4", "file:///tmp/transfer.json", "file:///tmp/transfer.gif"] {
            XCTAssertFalse(OpticalRecordingLimits.accepts(try XCTUnwrap(URL(string: path))))
        }
        try OpticalRecordingLimits.checkDimensions(width: 3840, height: 2160)
        for (width, height) in [(0, 1), (1, -1), (Int.max, 1), (4097, 1), (4096, 4096)] {
            XCTAssertThrowsError(try OpticalRecordingLimits.checkDimensions(width: width, height: height))
        }
    }

    private func pngCanvasHeader(width: UInt32, height: UInt32) -> Data {
        var header = Data([137, 80, 78, 71, 13, 10, 26, 10,
                           0, 0, 0, 13, 73, 72, 68, 82])
        for value in [width, height] {
            header.append(contentsOf: [UInt8(truncatingIfNeeded: value >> 24), UInt8(truncatingIfNeeded: value >> 16),
                                       UInt8(truncatingIfNeeded: value >> 8), UInt8(truncatingIfNeeded: value)])
        }
        return header
    }

    func testPNGCanvasPreflightAcceptsCanonicalBoundedHeaderWithoutImageMetadata() throws {
        let dimensions: [(UInt32, UInt32)] = [(1, 1), (1024, 1024), (3840, 2160), (4096, 2048), (2048, 4096)]
        for (width, height) in dimensions {
            let header = pngCanvasHeader(width: width, height: height)
            XCTAssertEqual(header.count, 24)
            // No frame properties or image decoding are needed to bound the
            // full canvas; PNG and APNG share this exact header layout.
            try OpticalRecordingLimits.checkPNGCanvasHeader(header)
        }
    }

    func testPNGCanvasPreflightRejectsTruncationSignatureAndFirstChunkMismatch() throws {
        let valid = pngCanvasHeader(width: 1024, height: 1024)
        for count in 0..<valid.count {
            XCTAssertThrowsError(try OpticalRecordingLimits.checkPNGCanvasHeader(Data(valid.prefix(count)))) { error in
                XCTAssertEqual(error as? FTError, .malformed("recording.png"))
            }
        }
        for offset in 0..<16 {
            var changed = valid
            changed[offset] ^= 1
            XCTAssertThrowsError(try OpticalRecordingLimits.checkPNGCanvasHeader(changed)) { error in
                XCTAssertEqual(error as? FTError, .malformed("recording.png"))
            }
        }
        // The helper consumes an exact fixed-size read, not arbitrary input.
        XCTAssertThrowsError(try OpticalRecordingLimits.checkPNGCanvasHeader(valid + Data([0])))
    }

    func testPNGCanvasPreflightRejectsZeroOversizedAndHugeUnsignedDimensions() throws {
        let dimensions: [(UInt32, UInt32)] = [
            (0, 1), (1, 0), (4097, 1), (1, 4097), (4096, 2049), (4096, 4096),
            (0x80000000, 1), (1, 0x80000000), (.max, .max)
        ]
        for (width, height) in dimensions {
            XCTAssertThrowsError(try OpticalRecordingLimits.checkPNGCanvasHeader(pngCanvasHeader(width: width, height: height))) { error in
                XCTAssertEqual(error as? FTError, .limit("recording.dimensions"))
            }
        }
    }

    func testAmbiguousFirstImageDoesNotChooseEitherTransferOrAllocateProgress() async throws {
        let (restore, _, vault, _) = environment()
        let first = try sender(), other = try sender(transfer: 9)
        let before = await restore.status()
        do {
            _ = try await restore.observeRecordingFrame([
                first.frame(page: 0, sequence: 1, using: crypto),
                other.frame(page: 0, sequence: 1, using: crypto)
            ], generation: before.generation)
            XCTFail("ambiguous recording selected a source")
        } catch { XCTAssertEqual(error as? FTError, .malformed("recording.ambiguous_sources")) }
        let after = await restore.status()
        XCTAssertEqual(after.phase, .idle); XCTAssertNil(after.identity)
        XCTAssertEqual(after.generation, before.generation); XCTAssertEqual(after.receivedParts, 0)
        XCTAssertEqual(vault.creates, 0)
    }

    func testPartialRecordingCanContinueWithCameraAndForeignFramesCannotSwitchSources() async throws {
        let (restore, _, _, _) = environment(), source = try sender(), foreign = try sender(transfer: 8)
        var state = await restore.status()
        let first = try source.frame(page: 0, sequence: 1, using: crypto)
        state = try await restore.observeRecordingFrame([first, first], generation: state.generation)
        XCTAssertEqual(state.phase, .collecting); XCTAssertEqual(state.receivedParts, 1)
        let generation = state.generation
        state = try await restore.observeRecordingFrame([
            foreign.frame(page: 0, sequence: 1, using: crypto),
            source.frame(page: 0, sequence: 2, using: crypto)
        ], generation: generation)
        XCTAssertEqual(state.identity, source.identity); XCTAssertEqual(state.receivedParts, 2)
        XCTAssertEqual(state.verifiedPages, 0)
        // Ending a finite recording performs no finish/reset operation. The
        // ordinary camera can add the remaining pieces to this exact receiver.
        for page in 0..<source.identity.pageCount {
            for sequence in 1...(try source.sourcePartCount(page: page)) {
                state = try await restore.observe(source.frame(page: page, sequence: UInt32(sequence), using: crypto),
                                                  generation: generation)
            }
        }
        XCTAssertEqual(state.phase, .locked); XCTAssertEqual(state.identity, source.identity)
        XCTAssertEqual(state.generation, generation); XCTAssertEqual(state.verifiedPages, source.identity.pageCount)
        XCTAssertNil(state.receipt); XCTAssertNil(state.review)
    }

    func testRecordingNeverConsumesCodeOrInstallsEvenAfterCiphertextCompletionAndUnlock() async throws {
        let (restore, store, vault, network) = environment(), exported = try export()
        let source = try OpticalSender(envelope: exported.envelope, using: crypto)
        try await restore.enterOfflineMode()
        var state = await restore.status()
        let code = exported.code.reveal(using: crypto)
        let codeFrame = "FTCODE1:\(TransferCodeFrame.digestPrefix(source.identity.envelopeDigest)):\(code)"
        state = try await restore.observeRecordingFrame([codeFrame, "https://example.invalid", "FTQR1/invalid"], generation: state.generation)
        XCTAssertEqual(state.phase, .idle)
        for page in 0..<source.identity.pageCount {
            for sequence in 1...(try source.sourcePartCount(page: page)) {
                state = try await restore.observeRecordingFrame([
                    codeFrame, source.frame(page: page, sequence: UInt32(sequence), using: crypto)
                ], generation: state.generation)
            }
        }
        XCTAssertEqual(state.phase, .locked); XCTAssertNil(state.review); XCTAssertNil(state.receipt)
        XCTAssertEqual(vault.creates, 0)
        let beforeUnlock = try await store.load()
        XCTAssertTrue(beforeUnlock.isEmpty)
        XCTAssertThrowsError(try network.beginConnection())
        XCTAssertEqual(network.accounting().connections, 0)
        state = try await restore.unlock(enteredCode: code, generation: state.generation)
        XCTAssertEqual(state.phase, .review); XCTAssertNotNil(state.review); XCTAssertNil(state.receipt)
        XCTAssertEqual(vault.creates, 0)
        let beforeApproval = try await store.load()
        XCTAssertTrue(beforeApproval.isEmpty)
        XCTAssertEqual(network.accounting().offlineRestores, 1)
        XCTAssertThrowsError(try network.beginConnection())
    }

    func testSuspensionAndResetRetireRecordingCallbacksWithoutLosingVerifiedPieces() async throws {
        let (restore, _, _, _) = environment(), source = try sender()
        var state = await restore.status()
        for sequence in 1...(try source.sourcePartCount(page: 0)) {
            state = try await restore.observeRecordingFrame([source.frame(page: 0, sequence: UInt32(sequence), using: crypto)],
                                                            generation: state.generation)
        }
        XCTAssertEqual(state.verifiedPages, 1)
        let old = state.generation, next = try source.frame(page: 1, sequence: 1, using: crypto)
        _ = await restore.suspend()
        do {
            _ = try await restore.observeRecordingFrame([next], generation: old)
            XCTFail("suspended recording callback was accepted")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        state = await restore.resume()
        XCTAssertEqual(state.verifiedPages, 1); XCTAssertNotEqual(state.generation, old)
        state = try await restore.observeRecordingFrame([next], generation: state.generation)
        XCTAssertEqual(state.independentParts, 1)
        let retired = state.generation
        state = try await restore.reset()
        do {
            _ = try await restore.observeRecordingFrame([next], generation: retired)
            XCTFail("old recording selected a source after reset")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        let after = await restore.status()
        XCTAssertEqual(after.phase, .idle); XCTAssertNil(after.identity)
        XCTAssertEqual(after.generation, state.generation)
    }

    func testCancelledOrOversizedFrameAdmissionCannotMutateReceiver() async throws {
        let (restore, _, _, _) = environment(), source = try sender()
        let state = await restore.status(), text = try source.frame(page: 0, sequence: 1, using: crypto)
        do {
            _ = try await restore.observeRecordingFrame(Array(repeating: text, count: OpticalRecordingLimits.maximumCodesPerFrame + 1),
                                                        generation: state.generation)
            XCTFail("accepted too many codes from one image")
        } catch { XCTAssertEqual(error as? FTError, .limit("recording.qr_count")) }
        let cancelled = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await restore.observeRecordingFrame([text], generation: state.generation)
        }
        do { _ = try await cancelled.value; XCTFail("cancelled reader mutated restore") }
        catch { XCTAssertTrue(error is CancellationError) }
        let after = await restore.status()
        XCTAssertEqual(after.phase, .idle); XCTAssertNil(after.identity); XCTAssertEqual(after.receivedParts, 0)
    }

    #if canImport(AVFoundation) && canImport(ImageIO) && canImport(Vision) && canImport(CryptoKit) && canImport(Darwin)
    func testNativeMP4AndAPNGRoundTripIntoTheActualOfflineRestore() async throws {
        try skipIfSimulatorLacksVisionBarcodeInference()
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("ft-recording-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let exported = try export()
        for ext in ["apng", "mp4"] {
            let url = directory.appendingPathComponent("transfer.\(ext)")
            if ext == "apng" {
                _ = try await NativeOpticalPresentation().saveAnimation(envelope: exported.envelope, to: url)
            } else {
                _ = try await NativeOpticalVideo().save(source: OpticalVideoSource(envelope: exported.envelope), to: url)
            }
            let (restore, store, vault, _) = environment()
            try await restore.enterOfflineMode()
            let generation = await restore.status().generation
            let read = try await NativeOpticalRecording().read(url) { texts, _ in
                let status = try await restore.observeRecordingFrame(texts, generation: generation)
                return status.phase != .locked
            }
            let status = await restore.status()
            XCTAssertGreaterThan(read.framesRead, 0); XCTAssertFalse(read.reachedEnd)
            XCTAssertEqual(status.phase, .locked)
            XCTAssertEqual(status.identity?.envelopeDigest, crypto.sha256(exported.envelope))
            XCTAssertEqual(vault.creates, 0)
            let saved = try await store.load(); XCTAssertTrue(saved.isEmpty)
        }
    }
    #endif
}
