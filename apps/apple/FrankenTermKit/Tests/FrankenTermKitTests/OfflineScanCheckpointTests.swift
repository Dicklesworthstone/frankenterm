import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class OfflineScanCheckpointTests: XCTestCase {
    private let crypto = TestCryptography()

    private func fixture(activity: WorkspaceNetworkActivity = WorkspaceNetworkActivity()) -> OfflineRestore<TestCryptography> {
        OfflineRestore(using: crypto, store: WorkspaceStore(persistence: StoreTestPersistence(), vault: StoreTestVault()),
                       networkActivity: activity)
    }
    /// Opaque transport data with a valid outer header, not a decrypted archive.
    private func sender(mode: FTAGMode = .code) throws -> OpticalSender {
        var bytes = Data("FTAG".utf8) + Data([1, mode.rawValue, 0, 0]) + Data(repeating: 19, count: 16)
        bytes.appendBE(4112, bytes: 4); bytes.appendBE(UInt64(mode.modeHeaderLength), bytes: 2)
        bytes += Data(repeating: 27, count: mode.modeHeaderLength)
        bytes += Data((0..<4112).map { UInt8(truncatingIfNeeded: $0 * 13) })
        return try OpticalSender(envelope: bytes, using: crypto)
    }
    private func checkpoint(_ sender: OpticalSender, completed: Bool = false) throws -> Data {
        let first = try OpticalFrame(text: sender.frame(page: 0, sequence: 1, using: crypto))
        var receiver = OpticalReceiver(selecting: first)
        let count = completed ? try sender.sourcePartCount(page: 0) : 4
        for sequence in 1...count {
            try receiver.receive(OpticalFrame(text: sender.frame(page: 0, sequence: UInt32(sequence), using: crypto)), using: crypto)
        }
        return try receiver.decoderCheckpoint(using: crypto)
    }

    func testExportFromOneActorResumesInAnotherWithoutDecrypting() async throws {
        let sender = try sender(), original = fixture()
        var state = await original.status()
        state = try await original.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        state = try await original.selectCandidate(generation: state.generation, digest: sender.identity.envelopeDigest)
        for sequence in 2...4 {
            state = try await original.observe(sender.frame(page: 0, sequence: UInt32(sequence), using: crypto), generation: state.generation)
        }
        let saved = try await original.exportDecoderCheckpoint(generation: state.generation)
        _ = await original.suspend(discardRecipientRequest: true)
        let resumed = fixture()
        let empty = await resumed.status()
        state = try await resumed.acceptDecoderCheckpoint(saved, generation: empty.generation)
        XCTAssertNotEqual(state.generation, empty.generation)
        XCTAssertEqual(state.phase, .collecting)
        XCTAssertEqual(state.source, .optical)
        XCTAssertEqual(state.identity, sender.identity)
        XCTAssertEqual(state.receivedParts, 4)
        XCTAssertNil(state.review); XCTAssertNil(state.receipt)
        for sequence in 5...(try sender.sourcePartCount(page: 0)) {
            state = try await resumed.observe(sender.frame(page: 0, sequence: UInt32(sequence), using: crypto), generation: state.generation)
        }
        XCTAssertEqual(state.phase, .locked)
        XCTAssertEqual(state.encryptionMode, .code)
        XCTAssertEqual(state.verifiedBytes, sender.identity.envelopeLength)
        XCTAssertNil(state.review); XCTAssertNil(state.receipt); XCTAssertNil(state.arrival)
    }
    func testResumeRetiresPreImportCameraAndExportTokens() async throws {
        let sender = try sender(), restore = fixture()
        let before = await restore.status()
        let after = try await restore.acceptDecoderCheckpoint(checkpoint(sender), generation: before.generation)
        do {
            _ = try await restore.observe(sender.frame(page: 0, sequence: 5, using: crypto), generation: before.generation)
            XCTFail("old callback reached a newly selected scan")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        do {
            _ = try await restore.exportDecoderCheckpoint(generation: before.generation)
            XCTFail("old export token accepted")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        let current = await restore.status()
        XCTAssertEqual(current.generation, after.generation)
        XCTAssertEqual(current.receivedParts, 4)
    }
    func testBadCheckpointDoesNotChangeIdleGenerationOrSelectASource() async throws {
        let sender = try sender(), restore = fixture()
        let before = await restore.status()
        do {
            _ = try await restore.acceptDecoderCheckpoint(Data(checkpoint(sender).dropLast()), generation: before.generation)
            XCTFail("truncated checkpoint accepted")
        } catch {}
        let current = await restore.status()
        XCTAssertEqual(current.generation, before.generation)
        XCTAssertEqual(current.phase, .idle)
        XCTAssertNil(current.identity); XCTAssertNil(current.review)
        let valid = try await restore.acceptDecoderCheckpoint(checkpoint(sender), generation: before.generation)
        XCTAssertEqual(valid.receivedParts, 4)
    }
    func testSelectedScanCannotBeReplacedByCheckpointFile() async throws {
        let sender = try sender(), restore = fixture()
        let empty = await restore.status()
        let collecting = try await restore.acceptDecoderCheckpoint(checkpoint(sender), generation: empty.generation)
        do {
            _ = try await restore.acceptDecoderCheckpoint(checkpoint(sender, completed: true), generation: collecting.generation)
            XCTFail("file replaced an active scan")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .busy) }
        let state = await restore.status()
        XCTAssertEqual(state.phase, .collecting)
        XCTAssertEqual(state.receivedParts, 4)
    }
    func testExportRequiresActiveCollectionAndDoesNotReviveSuspension() async throws {
        let sender = try sender(), restore = fixture()
        let empty = await restore.status()
        do { _ = try await restore.exportDecoderCheckpoint(generation: empty.generation); XCTFail("exported idle") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        let collecting = try await restore.acceptDecoderCheckpoint(checkpoint(sender), generation: empty.generation)
        let suspended = await restore.suspend()
        do { _ = try await restore.exportDecoderCheckpoint(generation: suspended.generation); XCTFail("exported suspended") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        let resumed = await restore.resume()
        XCTAssertNotEqual(resumed.generation, collecting.generation)
        let bytes = try await restore.exportDecoderCheckpoint(generation: resumed.generation)
        XCTAssertEqual(try OpticalReceiver(decoderCheckpoint: bytes, using: crypto).receivedParts, 4)
    }
    func testCompletedCheckpointIsLockedNotApprovedForInstallation() async throws {
        let sender = try sender(), restore = fixture()
        let empty = await restore.status()
        let locked = try await restore.acceptDecoderCheckpoint(checkpoint(sender, completed: true), generation: empty.generation)
        XCTAssertEqual(locked.phase, .locked)
        XCTAssertNil(locked.review); XCTAssertNil(locked.receipt)
        do {
            _ = try await restore.commit(reviewToken: UUID(), generation: locked.generation, sourceAndContentsConfirmed: true)
            XCTFail("checkpoint granted installation authority")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        do { _ = try await restore.exportDecoderCheckpoint(generation: locked.generation); XCTFail("exported outside collection") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
    }
    func testRecipientCheckpointDoesNotRestoreASecretRequestKey() async throws {
        let sender = try sender(mode: .recipient), restore = fixture()
        let empty = await restore.status()
        let locked = try await restore.acceptDecoderCheckpoint(checkpoint(sender, completed: true), generation: empty.generation)
        XCTAssertEqual(locked.phase, .locked)
        XCTAssertEqual(locked.encryptionMode, .recipient)
        XCTAssertFalse(locked.canUnlockRecipient)
        XCTAssertNil(locked.recipientRequest)
        XCTAssertNil(locked.review); XCTAssertNil(locked.receipt)
    }
    func testExistingConnectionExcludesResumeAndRestoredScanExcludesNetworking() async throws {
        let sender = try sender(), activity = WorkspaceNetworkActivity(), restore = fixture(activity: activity)
        let before = await restore.status()
        var lease: WorkspaceNetworkLease? = try activity.beginConnection()
        do { _ = try await restore.acceptDecoderCheckpoint(checkpoint(sender), generation: before.generation); XCTFail("resumed online") }
        catch { XCTAssertEqual(error as? WorkspaceNetworkError, .connectionsActive) }
        let rejected = await restore.status()
        XCTAssertEqual(rejected.phase, .idle); XCTAssertEqual(rejected.generation, before.generation)
        withExtendedLifetime(lease) {}; lease = nil
        _ = try await restore.acceptDecoderCheckpoint(checkpoint(sender), generation: before.generation)
        XCTAssertEqual(activity.accounting().offlineRestores, 1)
        XCTAssertThrowsError(try activity.beginConnection()) {
            XCTAssertEqual($0 as? WorkspaceNetworkError, .offlineRestoreActive)
        }
        _ = await restore.suspend(discardRecipientRequest: true)
        XCTAssertEqual(activity.accounting().offlineRestores, 0)
    }
    func testCancelledResumePublishesNothing() async throws {
        let sender = try sender(), restore = fixture()
        let before = await restore.status(), bytes = try checkpoint(sender)
        let task = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await restore.acceptDecoderCheckpoint(bytes, generation: before.generation)
        }
        do { _ = try await task.value; XCTFail("cancelled resume published state") }
        catch { XCTAssertTrue(error is CancellationError) }
        let after = await restore.status()
        XCTAssertEqual(after.phase, .idle); XCTAssertEqual(after.generation, before.generation)
    }
}
