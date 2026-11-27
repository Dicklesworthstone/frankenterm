import XCTest
import UIKit
@testable import FrankenTermKit
@testable import FrankenTerm

/// App-owned scanner lifecycle, without camera permissions or a live camera.
/// Physical QR capture remains part of device qualification.
@MainActor
final class MobileOpticalRestoreTests: XCTestCase {
    func testFilesOpensMacRecordingProgressAndContinuesTheSameOfflineTransfer() async throws {
        let crypto = AppleCryptography(), persistence = StoreTestPersistence(), network = WorkspaceNetworkActivity()
        let restore = OfflineRestore(using: crypto,
            store: WorkspaceStore(persistence: persistence, vault: AppleWorkspaceVault()), networkActivity: network)
        let idle = await restore.status()
        let ready = try await restore.createRecipientRequest(generation: idle.generation)
        let envelope = try FTAG.sealArchiveBytes(Data(repeating: 7, count: 100),
            recipient: XCTUnwrap(ready.recipientRequest), transferID: Data(repeating: 2, count: 16),
            compress: false, using: crypto)
        let sender = try OpticalSender(envelope: envelope, using: crypto)
        var mac = OpticalRecordingRecovery()
        let recording = try mac.beginRecording()
        let first = try sender.frame(page: 0, sequence: 1, using: crypto)
        try mac.observe([first], recording: recording, using: crypto)
        mac.endRecording(recording)
        let bytes = try mac.decoderCheckpoint(using: crypto)
        let file = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            .appendingPathExtension("ftqrprogress")
        defer { try? FileManager.default.removeItem(at: file) }

        let model = MobileWorkspaceModel(restore: restore)
        let priorBookmark = UserDefaults.standard.data(forKey: "live-screen")
        defer {
            model.setRestoreVisible(false); model.active = false
            if let priorBookmark { UserDefaults.standard.set(priorBookmark, forKey: "live-screen") }
        }
        model.active = true; model.state = ready
        func openSelectedFile() async throws {
            model.importFile(file)
            await model.restoreSettled()
            for _ in 0..<200 where model.busy || model.hasPendingImport {
                try await Task.sleep(for: .milliseconds(5))
            }
            XCTAssertFalse(model.busy); XCTAssertFalse(model.hasPendingImport)
        }

        // Damaged checkpoints cannot select a source or consume its recipient key.
        var damaged = bytes
        damaged[damaged.count - 1] ^= 1
        try damaged.write(to: file)
        try await openSelectedFile()
        XCTAssertNotNil(model.message)
        XCTAssertEqual(model.state?.phase, .idle); XCTAssertEqual(model.state?.recipientRequest, ready.recipientRequest)

        try bytes.write(to: file, options: .atomic)
        try await openSelectedFile()
        let partial = try XCTUnwrap(model.state)
        XCTAssertEqual(partial.phase, .collecting); XCTAssertEqual(partial.source, .optical)
        XCTAssertEqual(partial.identity, sender.identity); XCTAssertEqual(partial.receivedParts, 1)
        XCTAssertEqual(partial.recipientRequest, ready.recipientRequest)
        XCTAssertFalse(model.cameraRequested); XCTAssertFalse(model.isSavingScanProgress)
        XCTAssertNil(partial.review); XCTAssertNil(partial.receipt)

        model.importFile(file)
        XCTAssertFalse(model.hasPendingImport); XCTAssertEqual(model.state?.generation, partial.generation)
        var current = partial
        for sequence in 2...(try sender.sourcePartCount(page: 0)) {
            let text = try sender.frame(page: 0, sequence: UInt32(sequence), using: crypto)
            current = try await restore.observeRecordingFrame([text], generation: current.generation)
        }
        XCTAssertEqual(current.phase, .locked); XCTAssertTrue(current.canUnlockRecipient)
        XCTAssertEqual(current.verifiedBytes, envelope.count); XCTAssertNil(current.review)
        XCTAssertThrowsError(try network.beginConnection())
        XCTAssertNil(persistence.bytes(.catalog)); XCTAssertNil(persistence.bytes(.journal))
    }

    private func partialScan() async throws
        -> (OfflineRestore<AppleCryptography>, OfflineRestoreStatus, StoreTestPersistence, WorkspaceNetworkActivity, OpticalSender) {
        let crypto = AppleCryptography(), persistence = StoreTestPersistence(), network = WorkspaceNetworkActivity()
        let restore = OfflineRestore(using: crypto,
            store: WorkspaceStore(persistence: persistence, vault: AppleWorkspaceVault()), networkActivity: network)
        var state = await restore.status()
        state = try await restore.createRecipientRequest(generation: state.generation)
        // The receiver treats these synthetic encrypted bytes as opaque. This
        // test never decrypts an archive or touches Keychain/publication.
        let envelope = try FTAG.sealArchiveBytes(Data(repeating: 7, count: 40_000),
            recipient: XCTUnwrap(state.recipientRequest), transferID: Data(repeating: 2, count: 16),
            compress: false, using: crypto)
        let sender = try OpticalSender(envelope: envelope, using: crypto)
        state = try await restore.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        state = try await restore.selectCandidate(generation: state.generation, digest: sender.identity.envelopeDigest)
        for sequence in 2...(try sender.sourcePartCount(page: 0)) {
            state = try await restore.observe(sender.frame(page: 0, sequence: UInt32(sequence), using: crypto),
                                              generation: state.generation)
        }
        state = try await restore.observe(sender.frame(page: 1, sequence: 1, using: crypto), generation: state.generation)
        XCTAssertEqual(state.verifiedPages, 1); XCTAssertEqual(state.independentParts, 1)
        return (restore, state, persistence, network, sender)
    }

    func testCameraFailureOffersRetryWithoutDiscardingCiphertextOrOfflineExclusion() async throws {
        let (restore, progress, persistence, network, _) = try await partialScan()
        let model = MobileWorkspaceModel(restore: restore)
        let priorBookmark = UserDefaults.standard.data(forKey: "live-screen")
        defer {
            model.cameraRequested = false; model.active = false; model.showRestore = false
            if let priorBookmark { UserDefaults.standard.set(priorBookmark, forKey: "live-screen") }
        }
        model.active = true; model.showRestore = true; model.state = progress
        model.cameraRequested = true
        let stoppedCamera = model.cameraGeneration
        model.cameraFailed(FTError.unsupported("camera.unavailable"), generation: stoppedCamera)

        XCTAssertFalse(model.cameraRequested, "The restore screen must expose Resume Camera again.")
        XCTAssertNotEqual(model.cameraGeneration, stoppedCamera)
        XCTAssertEqual(model.state?.identity, progress.identity)
        XCTAssertEqual(model.state?.generation, progress.generation)
        XCTAssertEqual(model.state?.verifiedBytes, progress.verifiedBytes)
        XCTAssertEqual(model.state?.independentParts, 1)
        XCTAssertNotNil(progress.recipientRequest)
        XCTAssertEqual(model.state?.recipientRequest, progress.recipientRequest)
        XCTAssertTrue(model.message?.contains("progress is kept") == true)
        XCTAssertEqual(network.accounting().offlineRestores, 1)
        XCTAssertThrowsError(try network.beginConnection())
        XCTAssertNil(persistence.bytes(.catalog)); XCTAssertNil(persistence.bytes(.journal))
        let retained = await restore.status()
        XCTAssertEqual(retained.identity, progress.identity)
        XCTAssertEqual(retained.receivedParts, progress.receivedParts)
        XCTAssertEqual(retained.recipientRequest, progress.recipientRequest)

        // A successful explicit retry requests a fresh scanner. An old
        // instance's delayed failure must not hide that replacement camera.
        model.cameraRequested = true
        let replacement = model.cameraGeneration
        model.message = nil
        model.cameraFailed(FTError.unsupported("camera.unavailable"), generation: stoppedCamera)
        XCTAssertTrue(model.cameraRequested); XCTAssertEqual(model.cameraGeneration, replacement)
        XCTAssertNil(model.message); XCTAssertEqual(model.state?.receivedParts, progress.receivedParts)
    }

    func testScanGuidanceRetiresWithTheCameraAndRestartsAfterPageRepair() async throws {
        let (restore, progress, persistence, network, _) = try await partialScan()
        var now = ContinuousClock.now
        let model = MobileWorkspaceModel(restore: restore, opticalScanNow: { now })
        let priorBookmark = UserDefaults.standard.data(forKey: "live-screen")
        defer {
            model.cameraRequested = false; model.active = false; model.showRestore = false
            if let priorBookmark { UserDefaults.standard.set(priorBookmark, forKey: "live-screen") }
        }
        model.active = true; model.showRestore = true; model.state = progress; model.cameraRequested = true
        let originalCamera = model.cameraGeneration
        now = now.advanced(by: .seconds(8)); model.state = progress
        XCTAssertEqual(model.opticalScanGuidance, .noRecentFrames)
        XCTAssertEqual(model.state?.receivedParts, progress.receivedParts)
        XCTAssertEqual(network.accounting().offlineRestores, 1)
        XCTAssertNil(persistence.bytes(.catalog)); XCTAssertNil(persistence.bytes(.journal))

        model.cameraRequested = false
        XCTAssertNil(model.opticalScanGuidance)
        now = now.advanced(by: .seconds(600))
        model.cameraRequested = true
        XCTAssertNotEqual(model.cameraGeneration, originalCamera)
        XCTAssertNil(model.opticalScanGuidance, "Paused time is not an active-camera stall.")
        model.cameraFailed(FTError.storage, generation: originalCamera)
        XCTAssertTrue(model.cameraRequested)
        now = now.advanced(by: .seconds(8)); model.state = progress
        XCTAssertEqual(model.opticalScanGuidance, .noRecentFrames)

        let repaired = try await restore.resetIncompletePage(1, generation: progress.generation)
        model.state = repaired
        XCTAssertNotEqual(repaired.generation, progress.generation)
        XCTAssertNil(model.opticalScanGuidance, "A deliberate repair gets a new monitoring generation.")
        XCTAssertEqual(repaired.verifiedBytes, progress.verifiedBytes)
        XCTAssertEqual(repaired.independentParts, 0)
        now = now.advanced(by: .seconds(8)); model.state = repaired
        XCTAssertEqual(model.opticalScanGuidance, .noRecentFrames)
        model.state = await restore.suspend()
        XCTAssertNil(model.opticalScanGuidance)
        model.active = false
        XCTAssertNil(model.opticalScanGuidance)
        XCTAssertEqual(network.accounting().offlineRestores, 1, "Advice and camera teardown do not release offline exclusion.")
    }

    func testRestartedCameraCannotConsumeThePreviousCamerasQueuedFrames() async throws {
        let (restore, progress, _, _, sender) = try await partialScan()
        let model = MobileWorkspaceModel(restore: restore)
        let priorBookmark = UserDefaults.standard.data(forKey: "live-screen")
        defer {
            model.setRestoreVisible(false); model.active = false
            if let priorBookmark { UserDefaults.standard.set(priorBookmark, forKey: "live-screen") }
        }
        model.active = true; model.showRestore = true; model.state = progress
        model.retryOfflineMode()
        await model.restoreSettled()
        model.cameraRequested = true
        let originalCamera = model.cameraGeneration, crypto = AppleCryptography()
        let obsolete = try sender.frame(page: 1, sequence: 2, using: crypto)
        let current = try sender.frame(page: 1, sequence: 3, using: crypto)
        let originalCheckpoint = try await restore.exportDecoderCheckpoint(generation: progress.generation)
        var expected = try OpticalReceiver(decoderCheckpoint: originalCheckpoint, using: crypto)
        try expected.receive(OpticalFrame(text: current), using: crypto)

        // No suspension occurs between queueing and replacement: the consumer
        // cannot admit this old frame before the camera generation is retired.
        model.observed(obsolete, cameraGeneration: originalCamera)
        model.cameraRequested = false; model.cameraRequested = true
        let replacementCamera = model.cameraGeneration
        model.observed(current, cameraGeneration: replacementCamera)
        let expectedParts = progress.receivedParts + 1
        for _ in 0..<200 {
            if (model.state?.receivedParts ?? 0) >= expectedParts { break }
            try await Task.sleep(for: .milliseconds(5))
        }
        let retained = await restore.status()
        XCTAssertEqual(model.state?.receivedParts, expectedParts)
        XCTAssertEqual(retained.receivedParts, expectedParts,
                       "Only the replacement camera's new independent piece may be admitted.")
        XCTAssertEqual(retained.verifiedBytes, progress.verifiedBytes)
        let checkpoint = try await restore.exportDecoderCheckpoint(generation: retained.generation)
        XCTAssertEqual(checkpoint, try expected.decoderCheckpoint(using: crypto),
                       "The retained equation must come from the replacement camera, not a delayed old symbol.")
        XCTAssertEqual(model.cameraGeneration, replacementCamera)
        model.setRestoreVisible(false)
        await model.restoreSettled()
    }

    func testCameraCoordinatorDefersAndDeliversOnlyOneFailure() async {
        let delivered = expectation(description: "camera failure delivered outside the view update")
        var failures = 0
        let coordinator = OpticalCamera.Coordinator(onCode: { _ in XCTFail("unexpected camera observation") }, onError: { error in
            failures += 1
            XCTAssertEqual(error as? FTError, .unsupported("camera.unavailable"))
            delivered.fulfill()
        })
        XCTAssertTrue(coordinator.permitsStart)
        coordinator.failed(FTError.storage)
        coordinator.failed(FTError.storage)
        XCTAssertFalse(coordinator.permitsStart, "A failed scanner waits for an explicit replacement.")
        XCTAssertEqual(failures, 0, "UIViewRepresentable updates cannot synchronously mutate the model.")
        await fulfillment(of: [delivered], timeout: 2)
        XCTAssertEqual(failures, 1)
        coordinator.retire()
    }

    func testRetiredCameraCannotDeliverItsQueuedOrSubsequentFailure() async {
        let obsolete = expectation(description: "retired camera callback")
        obsolete.isInverted = true
        let coordinator = OpticalCamera.Coordinator(onCode: { _ in obsolete.fulfill() }, onError: { _ in obsolete.fulfill() })
        coordinator.failed(FTError.storage)
        coordinator.retire()
        coordinator.failed(FTError.storage)
        XCTAssertFalse(coordinator.permitsStart)
        await fulfillment(of: [obsolete], timeout: 0.1)
    }
}
