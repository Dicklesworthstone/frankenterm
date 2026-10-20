import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class OfflineRepairTests: XCTestCase {
    private let crypto = TestCryptography()

    private func sender(size: Int) throws -> (OpticalSender, Data) {
        let payload = Data((0..<size).map { UInt8(truncatingIfNeeded: $0) })
        let envelope = try FTAG.sealArchiveBytes(payload, code: TransferCode(secret: Data(repeating: 7, count: 16)),
            transferID: Data(repeating: 8, count: 16), salt: Data(repeating: 9, count: 16), nonce: Data(repeating: 10, count: 12),
            compress: false, using: crypto)
        return (try OpticalSender(envelope: envelope, using: crypto), envelope)
    }

    private func environment() -> (OfflineRestore<TestCryptography>, StoreTestPersistence, StoreTestVault) {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        return (OfflineRestore(using: crypto, store: WorkspaceStore(persistence: persistence, vault: vault)), persistence, vault)
    }

    private func select(_ sender: OpticalSender, page: Int, using restore: OfflineRestore<TestCryptography>) async throws -> OfflineRestoreStatus {
        let initial = await restore.status()
        let candidate = try await restore.observe(sender.frame(page: page, sequence: 1, using: crypto), generation: initial.generation)
        XCTAssertTrue(candidate.incompletePageIndices.isEmpty, "Candidate display has not allocated a selected receive session.")
        return try await restore.selectCandidate(generation: candidate.generation, digest: sender.identity.envelopeDigest)
    }

    private func receive(_ sender: OpticalSender, page: Int, sequences: ClosedRange<Int>,
                         state: OfflineRestoreStatus, using restore: OfflineRestore<TestCryptography>) async throws -> OfflineRestoreStatus {
        var current = state
        for sequence in sequences {
            current = try await restore.observe(sender.frame(page: page, sequence: UInt32(sequence), using: crypto), generation: current.generation)
        }
        return current
    }

    func testUnfinishedPageIndicesTrackVerifiedPagesSuspensionAndCompleteCiphertext() async throws {
        let (sender, envelope) = try sender(size: 40000), (restore, persistence, vault) = environment()
        let initial = await restore.status()
        XCTAssertTrue(initial.incompletePageIndices.isEmpty)
        var state = try await select(sender, page: 1, using: restore)
        XCTAssertEqual(state.incompletePageIndices, [0, 1])
        state = try await receive(sender, page: 1, sequences: 2...(try sender.sourcePartCount(page: 1)), state: state, using: restore)
        XCTAssertEqual(state.phase, .collecting)
        XCTAssertEqual(state.incompletePageIndices, [0])
        XCTAssertEqual(state.verifiedPages, 1)
        XCTAssertEqual(state.verifiedBytes, envelope.count - FTLimit.page)
        let before = state
        let suspended = await restore.suspend()
        XCTAssertEqual(suspended.incompletePageIndices, [0])
        state = await restore.resume()
        XCTAssertEqual(state.identity, before.identity)
        XCTAssertEqual(state.incompletePageIndices, [0])
        XCTAssertEqual(state.verifiedBytes, before.verifiedBytes)
        XCTAssertNotEqual(state.generation, before.generation)
        do {
            _ = try await restore.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: before.generation)
            XCTFail("A callback from before suspension reached the resumed scan.")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        state = try await receive(sender, page: 0, sequences: 1...(try sender.sourcePartCount(page: 0)), state: state, using: restore)
        XCTAssertEqual(state.phase, .locked)
        XCTAssertEqual(state.verifiedPages, sender.identity.pageCount)
        XCTAssertEqual(state.verifiedBytes, envelope.count)
        XCTAssertTrue(state.incompletePageIndices.isEmpty)
        XCTAssertNil(state.review, "Complete ciphertext still requires decryption and review.")
        XCTAssertNil(persistence.bytes(.catalog)); XCTAssertEqual(vault.creates, 0)
    }

    func testRestartDiscardsOnlySelectedUnfinishedPageAndFencesQueuedObservations() async throws {
        let (sender, envelope) = try sender(size: 66000), (restore, persistence, vault) = environment()
        var state = try await select(sender, page: 2, using: restore)
        state = try await receive(sender, page: 2, sequences: 2...(try sender.sourcePartCount(page: 2)), state: state, using: restore)
        state = try await receive(sender, page: 0, sequences: 1...7, state: state, using: restore)
        state = try await receive(sender, page: 1, sequences: 1...5, state: state, using: restore)
        XCTAssertEqual(state.independentParts, 12)
        XCTAssertEqual(state.incompletePageIndices, [0, 1])
        let before = state
        state = try await restore.restartPage(0, generation: before.generation)
        XCTAssertEqual(state.phase, .collecting)
        XCTAssertEqual(state.identity, before.identity)
        XCTAssertEqual(state.verifiedPages, 1)
        XCTAssertEqual(state.verifiedBytes, before.verifiedBytes)
        XCTAssertEqual(state.independentParts, 5)
        XCTAssertEqual(state.incompletePageIndices, [0, 1])
        XCTAssertNotEqual(state.generation, before.generation)
        do {
            _ = try await restore.observe(sender.frame(page: 0, sequence: 8, using: crypto), generation: before.generation)
            XCTFail("A queued observation repopulated the restarted page.")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        do {
            _ = try await restore.restartPage(1, generation: before.generation)
            XCTFail("An old page-restart action reached the new receive generation.")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        let after = await restore.status()
        XCTAssertEqual(after.independentParts, 5)
        for page in [-1, 2, sender.identity.pageCount] {
            do { _ = try await restore.restartPage(page, generation: state.generation); XCTFail("Invalid or verified page was restarted.") }
            catch { XCTAssertEqual(error as? FTError, .stale) }
        }
        state = try await receive(sender, page: 0, sequences: 1...(try sender.sourcePartCount(page: 0)), state: state, using: restore)
        XCTAssertEqual(state.incompletePageIndices, [1])
        state = try await receive(sender, page: 1, sequences: 6...(try sender.sourcePartCount(page: 1)), state: state, using: restore)
        XCTAssertEqual(state.phase, .locked)
        XCTAssertEqual(state.verifiedBytes, envelope.count)
        XCTAssertTrue(state.incompletePageIndices.isEmpty)
        XCTAssertNil(persistence.bytes(.journal)); XCTAssertEqual(vault.creates, 0)
    }

    func testExplicitPageRestartRecoversFromAConflictingUnauthenticatedPageDigest() async throws {
        let (sender, envelope) = try sender(size: 40000), (restore, _, vault) = environment()
        var state = try await select(sender, page: 1, using: restore)
        state = try await receive(sender, page: 1, sequences: 2...(try sender.sourcePartCount(page: 1)), state: state, using: restore)
        var fields = try sender.frame(page: 0, sequence: 1, using: crypto).split(separator: "/", maxSplits: 6).map(String.init)
        fields[5] = String(repeating: "A", count: 64)
        state = try await restore.observe(fields.joined(separator: "/"), generation: state.generation)
        XCTAssertEqual(state.independentParts, 1)
        do {
            _ = try await restore.observe(sender.frame(page: 0, sequence: 2, using: crypto), generation: state.generation)
            XCTFail("Conflicting page digests were mixed in one decoder.")
        } catch { XCTAssertEqual(error as? FTError, .malformed("qr.changed_page_digest")) }
        let before = await restore.status()
        XCTAssertEqual(before.verifiedPages, 1)
        state = try await restore.restartPage(0, generation: before.generation)
        XCTAssertEqual(state.identity, before.identity)
        XCTAssertEqual(state.verifiedBytes, before.verifiedBytes)
        XCTAssertEqual(state.independentParts, 0)
        state = try await receive(sender, page: 0, sequences: 1...(try sender.sourcePartCount(page: 0)), state: state, using: restore)
        XCTAssertEqual(state.phase, .locked)
        XCTAssertEqual(state.verifiedBytes, envelope.count)
        XCTAssertTrue(state.incompletePageIndices.isEmpty)
        XCTAssertEqual(vault.creates, 0)
    }

    func testPageRestartRequiresAnActiveSelectedIncompleteScan() async throws {
        let (sender, envelope) = try sender(size: 40000), (restore, _, _) = environment()
        var state = await restore.status()
        do { _ = try await restore.restartPage(0, generation: state.generation); XCTFail("Restarted an idle receive session.") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        state = try await restore.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        do { _ = try await restore.restartPage(0, generation: state.generation); XCTFail("Restarted an unselected candidate.") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        state = try await restore.selectCandidate(generation: state.generation, digest: sender.identity.envelopeDigest)
        state = await restore.suspend()
        do { _ = try await restore.restartPage(0, generation: state.generation); XCTFail("Restarted a suspended scan.") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        _ = await restore.resume()
        state = try await restore.reset()
        XCTAssertTrue(state.incompletePageIndices.isEmpty)
        XCTAssertNil(state.identity)
        state = try await restore.acceptEncryptedFile(envelope, generation: state.generation)
        XCTAssertEqual(state.phase, .locked)
        XCTAssertTrue(state.incompletePageIndices.isEmpty)
        XCTAssertEqual(state.verifiedPages, sender.identity.pageCount)
        do { _ = try await restore.restartPage(0, generation: state.generation); XCTFail("Restarted a complete encrypted file.") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
    }
}
