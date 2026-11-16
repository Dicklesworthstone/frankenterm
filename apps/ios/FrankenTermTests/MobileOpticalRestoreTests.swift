import XCTest
import UIKit
@testable import FrankenTermKit
@testable import FrankenTerm

/// App-owned scanner lifecycle, without camera permissions or a live camera.
/// Physical QR capture remains part of device qualification.
@MainActor
final class MobileOpticalRestoreTests: XCTestCase {
    private func partialScan() async throws
        -> (OfflineRestore<AppleCryptography>, OfflineRestoreStatus, StoreTestPersistence, WorkspaceNetworkActivity) {
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
        return (restore, state, persistence, network)
    }

    func testCameraFailureOffersRetryWithoutDiscardingCiphertextOrOfflineExclusion() async throws {
        let (restore, progress, persistence, network) = try await partialScan()
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
