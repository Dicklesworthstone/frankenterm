import Foundation
import XCTest
@testable import FrankenTermKit

private actor InputJournalTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    private var nextResult: TerminalTransportWrite?
    private var writeCount = 0
    private var closeHeld = false, closing = false
    private var closeWaiter: CheckedContinuation<Void, Never>?
    private var closeObservers: [CheckedContinuation<Void, Never>] = []

    init(scope: TerminalSessionScope) throws {
        identity = try TerminalConnectionIdentity(workspaceID: scope.workspaceID.uuidString,
            machineID: scope.machineID, routeID: scope.routeID, routeRevision: 1,
            transportID: scope.transportID, channelID: scope.channelID)
    }
    func script(_ result: TerminalTransportWrite) { nextResult = result }
    func calls() -> Int { writeCount }
    func writeSome(_ bytes: Data) -> TerminalTransportWrite {
        writeCount += 1
        let result = nextResult ?? .accepted(bytes.count)
        nextResult = nil
        return result
    }
    func holdClose() { closeHeld = true }
    func waitUntilClosing() async {
        if closing { return }
        await withCheckedContinuation { closeObservers.append($0) }
    }
    func releaseClose() { closeHeld = false; closeWaiter?.resume(); closeWaiter = nil }
    func close() async {
        closing = true
        for observer in closeObservers { observer.resume() }
        closeObservers.removeAll()
        if closeHeld { await withCheckedContinuation { closeWaiter = $0 } }
    }
}

@MainActor
final class TerminalInputJournalTests: XCTestCase {
    private struct Fixture {
        let disk: StoreTestPersistence
        let journal: TerminalSessionJournal
        let epoch: TerminalSessionEpoch
        let coordinator: TerminalInputCoordinator
        let transport: InputJournalTransport
        let source: TerminalSource
        let focus: TerminalFocus
    }

    private func setup(maximumResolved: Int = 32) async throws -> Fixture {
        let disk = StoreTestPersistence()
        let journal = try TerminalSessionJournal(persistence: disk, maximumResolved: maximumResolved)
        let scope = try TerminalSessionScope(workspaceID: UUID(), workspaceRevision: UUID(),
            machineID: "machine", routeID: "route", transportID: UUID(), channelID: UUID())
        let epoch = try await journal.beginCreation(scope: scope)
        let coordinator = try TerminalInputCoordinator(), transport = try InputJournalTransport(scope: scope)
        let source = try await coordinator.attach(transport)
        try await journal.bindInput(epoch: epoch, to: source)
        let initial = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(initial, to: source)
        return Fixture(disk: disk, journal: journal, epoch: epoch, coordinator: coordinator,
                       transport: transport, source: source, focus: focus)
    }

    private func deliver(_ fixture: Fixture) async throws {
        let batch = try await fixture.coordinator.pendingOutcomes(fixture.source)
        guard !batch.outcomes.isEmpty else { return }
        try await fixture.journal.recordInput(batch.outcomes, epoch: fixture.epoch)
        try await fixture.coordinator.acknowledgeOutcomes(batch)
    }

    private func close(_ fixture: Fixture) async throws -> TerminalInputSeal {
        try await fixture.coordinator.stopAdmission(fixture.source)
        try await fixture.coordinator.disconnect(fixture.source)
        try await fixture.coordinator.suspend(fixture.focus)
        try await deliver(fixture)
        return try await fixture.coordinator.mintSeal(for: fixture.source)
    }

    private func expect(_ expected: TerminalSessionJournalError,
                        _ operation: () async throws -> Void,
                        file: StaticString = #filePath, line: UInt = #line) async {
        do { try await operation(); XCTFail("Expected a journal refusal", file: file, line: line) }
        catch { XCTAssertEqual(error as? TerminalSessionJournalError, expected, file: file, line: line) }
    }

    private func expectUnsealed(_ fixture: Fixture,
                                file: StaticString = #filePath, line: UInt = #line) async {
        do {
            _ = try await fixture.coordinator.mintSeal(for: fixture.source)
            XCTFail("Source was sealed before all owned work settled", file: file, line: line)
        } catch { XCTAssertEqual(error as? TerminalInputError, .unavailable, file: file, line: line) }
    }

    func testTransportEvidenceIsContentFreeDurableAndIdempotent() async throws {
        let f = try await setup(), secret = "synthetic-secret-never-in-effect-history\r"
        _ = try await f.coordinator.submit(f.focus, encodedBytes: Data(secret.utf8))
        _ = try await f.coordinator.drainOne(f.source)
        let batch = try await f.coordinator.pendingOutcomes(f.source)
        try await f.journal.recordInput(batch.outcomes, epoch: f.epoch)
        let first = try XCTUnwrap(f.disk.bytes(.catalog))
        try await f.journal.recordInput(batch.outcomes, epoch: f.epoch)
        XCTAssertEqual(f.disk.bytes(.catalog), first)
        let text = try XCTUnwrap(String(data: first, encoding: .utf8))
        XCTAssertFalse(text.contains(secret))
        XCTAssertFalse(text.contains(Data(secret.utf8).base64EncodedString()))
        let rows = try await f.journal.records()
        XCTAssertEqual(rows[0].inputEvidence, [.transportAccepted, .acceptedPrefix])
        XCTAssertTrue(rows[0].inputEvidence.mayHaveRemoteEffect)
        XCTAssertFalse(rows[0].canResolve)
        try await f.coordinator.acknowledgeOutcomes(batch)
        let seal = try await close(f)
        let review = try await f.journal.sealInput(epoch: f.epoch, boundary: seal)
        XCTAssertTrue(review.requiresReview)
        XCTAssertTrue(review.canResolve)
        XCTAssertEqual(review.state, .interrupted)
        let repeated = try await f.journal.sealInput(epoch: f.epoch, boundary: seal)
        XCTAssertEqual(repeated, review)
        try await f.coordinator.retire(f.source)
    }

    func testAnotherStreamPendingOutcomesAndNativeClosePreventSeal() async throws {
        let f = try await setup()
        let second = try await f.coordinator.makeStream(sceneID: UUID())
        let otherFocus = try await f.coordinator.changeFocus(second, to: f.source)
        _ = try await f.coordinator.submit(otherFocus, encodedBytes: Data("queued".utf8))
        await expectUnsealed(f)
        try await f.coordinator.stopAdmission(f.source)
        try await f.coordinator.suspend(f.focus)
        await f.transport.holdClose()
        let closing = Task { try await f.coordinator.disconnect(f.source) }
        await f.transport.waitUntilClosing()
        await expectUnsealed(f)
        await f.transport.releaseClose()
        try await closing.value
        await expectUnsealed(f) // The other stream still owns the source.
        try await f.coordinator.suspend(otherFocus)
        await expectUnsealed(f) // Its known-unsent outcome is still retained.
        try await deliver(f)
        let seal = try await f.coordinator.mintSeal(for: f.source)
        XCTAssertFalse(seal.hasAcceptedBytes)
        let review = try await f.journal.sealInput(epoch: f.epoch, boundary: seal)
        XCTAssertEqual(review.inputEvidence, .knownUnsent)
        XCTAssertTrue(review.requiresReview) // Shell creation may itself have effects.
        XCTAssertTrue(review.canResolve)
    }

    func testSealRetainsAcceptedEvidenceEvenIfConsumerDiscardedItsOutcome() async throws {
        let f = try await setup()
        _ = try await f.coordinator.submit(f.focus, encodedBytes: Data("effect".utf8))
        _ = try await f.coordinator.drainOne(f.source)
        _ = try await f.coordinator.takeOutcomes(f.source)
        let seal = try await close(f)
        XCTAssertTrue(seal.hasAcceptedBytes)
        let review = try await f.journal.sealInput(epoch: f.epoch, boundary: seal)
        XCTAssertTrue(review.inputEvidence.contains(.acceptedPrefix))
        XCTAssertTrue(review.inputEvidence.mayHaveRemoteEffect)
    }

    func testLostLeasedTailSurvivesRestartWithoutInventingReplayAuthority() async throws {
        let f = try await setup()
        _ = try await f.coordinator.submit(f.focus, encodedBytes: Data("ambiguous".utf8))
        await f.transport.script(.failed)
        let result = try await f.coordinator.drainOne(f.source)
        XCTAssertEqual(result, .connectionClosed)
        try await deliver(f)
        let restarted = try TerminalSessionJournal(persistence: f.disk, processID: UUID())
        let recovery = try await restarted.records()
        XCTAssertEqual(recovery[0].inputEvidence, .unknownTransportOutcome)
        XCTAssertTrue(recovery[0].canResolve)
        let changedRoute = try TerminalSessionScope(workspaceID: f.epoch.scope.workspaceID,
            workspaceRevision: UUID(), machineID: f.epoch.scope.machineID,
            routeID: "new-route", transportID: UUID(), channelID: UUID())
        await expect(.reviewRequired) { _ = try await restarted.beginCreation(scope: changedRoute) }
        try await restarted.resolve(recovery[0])
        let next = try await restarted.beginCreation(scope: changedRoute)
        XCTAssertNotEqual(next.id, f.epoch.id)
        let records = try await restarted.records()
        XCTAssertEqual(records[0].state, .acknowledged)
        XCTAssertEqual(records[0].inputEvidence, .unknownTransportOutcome)
        XCTAssertEqual(records[1].inputEvidence, [])
    }

    func testCrashBeforeOutcomeCheckpointRetainsOnlyConservativeEpochUncertainty() async throws {
        let f = try await setup()
        let beforeInput = f.disk.bytes(.catalog)
        _ = try await f.coordinator.submit(f.focus, encodedBytes: Data("uncheckpointed".utf8))
        _ = try await f.coordinator.drainOne(f.source)
        XCTAssertEqual(f.disk.bytes(.catalog), beforeInput)
        let relaunched = try TerminalSessionJournal(persistence: f.disk, processID: UUID())
        let recovery = try await relaunched.records()
        XCTAssertTrue(recovery[0].requiresReview)
        XCTAssertTrue(recovery[0].canResolve)
        XCTAssertEqual(recovery[0].state, .creationStarted)
        XCTAssertEqual(recovery[0].inputEvidence, []) // No invented per-operation observation.
        XCTAssertTrue(recovery[0].hasRecoveryNotice, "the pre-input marker covers the missing callback")
        await expect(.reviewRequired) { _ = try await relaunched.beginCreation(scope: f.epoch.scope) }
        let continued = try await relaunched.continueAfterInterruption(recovery[0])
        XCTAssertTrue(continued.hasRecoveryNotice)
        XCTAssertFalse(continued.blocksNewSession)
        let next = try await relaunched.beginCreation(scope: f.epoch.scope)
        try await relaunched.markInterrupted(epoch: next)

        // Another connection and another process still find the original
        // uncertainty, even though no input-outcome preference was ever set.
        let later = try TerminalSessionJournal(persistence: f.disk, processID: UUID())
        let retained = try await later.records()
        XCTAssertEqual(retained.filter(\.hasRecoveryNotice).map(\.id), [f.epoch.id])
        var delivery = TerminalInputDelivery()
        delivery.setRecoveryNotice(true)
        XCTAssertFalse(delivery.unanswered, "a coarse marker does not assert that a key definitely left")
        XCTAssertEqual(TerminalAttentionReason.observed(input: delivery), [.inputUncertain])
        let message = try XCTUnwrap(delivery.message(machine: "Alpha"))
        XCTAssertTrue(message.contains("Input may have been in flight"))
        XCTAssertFalse(message.contains("Your last input left this phone"))
        try await later.acknowledgeRecovery(retained.filter(\.hasRecoveryNotice))
        let acknowledged = try await later.records()
        XCTAssertFalse(acknowledged.contains(where: \.hasRecoveryNotice))
        XCTAssertTrue(acknowledged.contains { $0.id == next.id && $0.blocksNewSession },
                      "dismissing the old notice does not retire another epoch")
        let writes = await f.transport.calls()
        XCTAssertEqual(writes, 1) // Review/new readiness never replays the prior payload.
    }

    func testSealedUnknownTransportOutcomeSurvivesRelaunchAndContinuation() async throws {
        let f = try await setup(maximumResolved: 1)
        _ = try await f.coordinator.submit(f.focus, encodedBytes: Data("unknown-tail".utf8))
        await f.transport.script(.failed)
        _ = try await f.coordinator.drainOne(f.source)
        let boundary = try await close(f)
        let ended = try await f.journal.sealInput(epoch: f.epoch, boundary: boundary)
        XCTAssertEqual(ended.state, .interrupted)
        XCTAssertTrue(ended.inputEvidence.contains(.unknownTransportOutcome))
        XCTAssertTrue(ended.hasRecoveryNotice)
        let later = try TerminalSessionJournal(persistence: f.disk, maximumResolved: 1, processID: UUID())
        let old = try await later.records()
        let retained = try await later.continueAfterInterruption(old[0])
        XCTAssertTrue(retained.hasRecoveryNotice, "sealing local teardown cannot prove remote input delivery")
        for _ in 0..<3 {
            let epoch = try await later.beginCreation(scope: f.epoch.scope)
            try await later.markInterrupted(epoch: epoch)
            let current = try await later.records()
            _ = try await later.continueAfterInterruption(try XCTUnwrap(current.first { $0.id == epoch.id }))
        }
        let history = try await later.records()
        XCTAssertEqual(history.filter(\.hasRecoveryNotice).map(\.id), [f.epoch.id])
        XCTAssertEqual(history.count, 2, "normal history compacts without dropping unknown input")
    }

    func testSealedAcceptedInputDoesNotInventACrashOrUnknownOutcome() async throws {
        let f = try await setup()
        _ = try await f.coordinator.submit(f.focus, encodedBytes: Data("accepted".utf8))
        _ = try await f.coordinator.drainOne(f.source)
        let boundary = try await close(f)
        let ended = try await f.journal.sealInput(epoch: f.epoch, boundary: boundary)
        XCTAssertTrue(ended.inputEvidence.contains(.transportAccepted))
        XCTAssertTrue(ended.inputEvidence.contains(.acceptedPrefix))
        XCTAssertFalse(ended.hasRecoveryNotice, "cumulative acceptance is not evidence of a final failed input")
        let later = try TerminalSessionJournal(persistence: f.disk, processID: UUID())
        let records = try await later.records()
        let continued = try await later.continueAfterInterruption(records[0])
        XCTAssertFalse(continued.hasRecoveryNotice)
        XCTAssertEqual(continued.state, .continued)
    }

    func testRecoveryAcknowledgementPersistsEvidenceFromAFailedFinalSeal() async throws {
        let f = try await setup()
        _ = try await f.coordinator.submit(f.focus, encodedBytes: Data("unpublished-tail".utf8))
        await f.transport.script(.failed)
        _ = try await f.coordinator.drainOne(f.source)
        _ = try await f.coordinator.takeOutcomes(f.source)
        let boundary = try await close(f)
        XCTAssertTrue(boundary.hasUnknownTransportOutcome)
        f.disk.configure(.before(.catalog))
        await expect(.recoveryRequired) { _ = try await f.journal.sealInput(epoch: f.epoch, boundary: boundary) }
        let shown = try await f.journal.records()
        XCTAssertEqual(shown[0].state, .creationStarted)
        XCTAssertTrue(shown[0].hasRecoveryNotice)
        XCTAssertTrue(shown[0].inputEvidence.contains(.unknownTransportOutcome))
        try await f.journal.acknowledgeRecovery(shown)

        // A late cleanup retry clears the owner's in-memory pending seal. The
        // acknowledged history must still retain the exact evidence reviewed.
        _ = try await f.journal.sealInput(epoch: f.epoch, boundary: boundary)
        let relaunched = try TerminalSessionJournal(persistence: f.disk, processID: UUID())
        let persisted = try await relaunched.records()
        XCTAssertEqual(persisted[0].state, .acknowledged)
        XCTAssertEqual(persisted[0].inputEvidence, shown[0].inputEvidence)
        XCTAssertTrue(persisted[0].inputEvidence.contains(.unknownTransportOutcome))
        XCTAssertFalse(persisted[0].hasRecoveryNotice)
    }

    func testBoundEpochRequiresCoordinatorProofAndRejectsLookalikeSource() async throws {
        let f = try await setup()
        await expect(.activeSession) { try await f.journal.markInterrupted(epoch: f.epoch) }
        let other = try TerminalInputCoordinator(), transport = try InputJournalTransport(scope: f.epoch.scope)
        let lookalike = try await other.attach(transport)
        await expect(.invalidEpoch) { try await f.journal.bindInput(epoch: f.epoch, to: lookalike) }
        try await other.disconnect(lookalike)
        let wrongSeal = try await other.mintSeal(for: lookalike)
        await expect(.invalidEpoch) { _ = try await f.journal.sealInput(epoch: f.epoch, boundary: wrongSeal) }
        let active = try await f.journal.records()
        XCTAssertFalse(active[0].canResolve)
        let seal = try await close(f)
        let interrupted = try await f.journal.sealInput(epoch: f.epoch, boundary: seal)
        XCTAssertTrue(interrupted.requiresReview)
        XCTAssertEqual(interrupted.inputEvidence, [])
    }

    func testFailedBindingLeavesUnattachedFactoryCleanupAvailable() async throws {
        let f = try await setup()
        let journal = try TerminalSessionJournal(persistence: StoreTestPersistence())
        let wrong = try TerminalSessionScope(workspaceID: f.epoch.scope.workspaceID,
            workspaceRevision: f.epoch.scope.workspaceRevision, machineID: f.epoch.scope.machineID,
            routeID: f.epoch.scope.routeID, transportID: f.epoch.scope.transportID, channelID: UUID())
        let epoch = try await journal.beginCreation(scope: wrong)
        await expect(.invalidEpoch) { try await journal.bindInput(epoch: epoch, to: f.source) }
        try await journal.markInterrupted(epoch: epoch)
        let review = try await journal.records()
        XCTAssertTrue(review[0].canResolve)
        XCTAssertEqual(review[0].state, .interrupted)
    }

    func testWrongSourceOutcomesDoNotChangeHistoryOrConsumeTheRealBatch() async throws {
        let first = try await setup(), other = try await setup()
        _ = try await other.coordinator.submit(other.focus, encodedBytes: Data([65]))
        _ = try await other.coordinator.drainOne(other.source)
        let batch = try await other.coordinator.pendingOutcomes(other.source)
        let before = first.disk.bytes(.catalog)
        await expect(.invalidEpoch) { try await first.journal.recordInput(batch.outcomes, epoch: first.epoch) }
        XCTAssertEqual(first.disk.bytes(.catalog), before)
        let stillPending = try await other.coordinator.pendingOutcomes(other.source)
        XCTAssertEqual(stillPending.outcomes, batch.outcomes)
    }

    func testRecordFailureKeepsBatchAndRetryCannotDuplicateEvidence() async throws {
        for fault in [StoreTestPersistence.Fault.before(.catalog), .after(.catalog)] {
            let f = try await setup()
            _ = try await f.coordinator.submit(f.focus, encodedBytes: Data([65]))
            _ = try await f.coordinator.drainOne(f.source)
            let batch = try await f.coordinator.pendingOutcomes(f.source)
            f.disk.configure(fault)
            await expect(.recoveryRequired) { try await f.journal.recordInput(batch.outcomes, epoch: f.epoch) }
            let pending = try await f.coordinator.pendingOutcomes(f.source)
            XCTAssertEqual(pending.outcomes, batch.outcomes)
            let uncertain = try await f.journal.records()
            XCTAssertTrue(uncertain[0].requiresReview)
            XCTAssertFalse(uncertain[0].canResolve)
            try await f.journal.recordInput(batch.outcomes, epoch: f.epoch)
            let durable = f.disk.bytes(.catalog)
            try await f.journal.recordInput(batch.outcomes, epoch: f.epoch)
            XCTAssertEqual(f.disk.bytes(.catalog), durable)
            try await f.coordinator.acknowledgeOutcomes(batch)
        }
    }

    func testSealFailureRetiresProvenClosedOwnerAndAllowsExplicitReview() async throws {
        for fault in [StoreTestPersistence.Fault.before(.catalog), .after(.catalog)] {
            let f = try await setup(), seal = try await close(f)
            f.disk.configure(fault)
            await expect(.recoveryRequired) { _ = try await f.journal.sealInput(epoch: f.epoch, boundary: seal) }
            let recovery = try await f.journal.records()
            XCTAssertTrue(recovery[0].requiresReview)
            XCTAssertTrue(recovery[0].canResolve)
            await expect(.reviewRequired) { _ = try await f.journal.beginCreation(scope: f.epoch.scope) }
            let retried = try await f.journal.sealInput(epoch: f.epoch, boundary: seal)
            XCTAssertEqual(retried.state, .interrupted)
            try await f.journal.resolve(retried)
            let closed = try await f.journal.sealInput(epoch: f.epoch, boundary: seal)
            XCTAssertEqual(closed.state, .acknowledged)
        }
    }

    func testFailedSealCanFinishAfterExplicitAcknowledgementAndHistoryPruning() async throws {
        let f = try await setup(maximumResolved: 1)
        _ = try await f.coordinator.submit(f.focus, encodedBytes: Data([65]))
        _ = try await f.coordinator.drainOne(f.source)
        let seal = try await close(f)
        try await f.coordinator.retire(f.source)
        f.disk.configure(.before(.catalog))
        await expect(.recoveryRequired) { _ = try await f.journal.sealInput(epoch: f.epoch, boundary: seal) }
        let recovery = try await f.journal.records()
        try await f.journal.resolve(recovery[0])
        let next = try await f.journal.beginCreation(scope: f.epoch.scope)
        try await f.journal.markInterrupted(epoch: next)
        let nextReview = try await f.journal.records()
        try await f.journal.resolve(XCTUnwrap(nextReview.first { $0.id == next.id }))
        let pruned = try await f.journal.records()
        XCTAssertFalse(pruned.contains { $0.id == f.epoch.id })
        let completed = try await f.journal.sealInput(epoch: f.epoch, boundary: seal)
        XCTAssertEqual(completed.id, f.epoch.id)
        XCTAssertEqual(completed.state, .acknowledged)
        XCTAssertFalse(completed.canResolve)
        XCTAssertEqual(completed.inputEvidence, [.transportAccepted, .acceptedPrefix])
        let accounting = await f.coordinator.accounting()
        XCTAssertEqual(accounting.connections, 0)
    }

    func testPrecloseReviewCannotAcknowledgeASealWhosePublicationFailed() async throws {
        let f = try await setup(), before = try await f.journal.records()
        _ = try await f.coordinator.submit(f.focus, encodedBytes: Data([65]))
        _ = try await f.coordinator.drainOne(f.source)
        _ = try await f.coordinator.takeOutcomes(f.source)
        let seal = try await close(f)
        f.disk.configure(.before(.catalog))
        await expect(.recoveryRequired) { _ = try await f.journal.sealInput(epoch: f.epoch, boundary: seal) }
        await expect(.staleReview) { try await f.journal.resolve(before[0]) }
        let current = try await f.journal.records()
        XCTAssertTrue(current[0].inputEvidence.contains(.acceptedPrefix))
        try await f.journal.resolve(current[0])
        let persisted = try await f.journal.records()
        XCTAssertTrue(persisted[0].inputEvidence.contains(.acceptedPrefix))
        XCTAssertEqual(persisted[0].state, .acknowledged)
    }

    func testEarlierReviewCannotAcknowledgeNewInputEvidenceOrClosure() async throws {
        let f = try await setup(), initial = try await f.journal.records()
        _ = try await f.coordinator.submit(f.focus, encodedBytes: Data([65]))
        _ = try await f.coordinator.drainOne(f.source)
        try await deliver(f)
        await expect(.staleReview) { try await f.journal.resolve(initial[0]) }
        let beforeClosure = try await f.journal.records(), seal = try await close(f)
        let final = try await f.journal.sealInput(epoch: f.epoch, boundary: seal)
        await expect(.staleReview) { try await f.journal.resolve(beforeClosure[0]) }
        try await f.journal.resolve(final)
    }

    func testLegacyHistoryMigratesWithoutDroppingUnknownShellEffects() async throws {
        let f = try await setup()
        var root = try StrictJSON.decode(XCTUnwrap(f.disk.bytes(.catalog))).object()
        root["version"] = .number("1")
        root["records"] = .array(try root.required("records").array().map {
            var row = try $0.object(); row["input_evidence"] = nil; row["unclean"] = nil
            return .object(row)
        })
        f.disk.seed(try StrictJSON.encode(.object(root)), slot: .catalog)
        let relaunched = try TerminalSessionJournal(persistence: f.disk, processID: UUID())
        let legacy = try await relaunched.records()
        XCTAssertEqual(legacy[0].id, f.epoch.id)
        XCTAssertTrue(legacy[0].requiresReview)
        XCTAssertEqual(legacy[0].inputEvidence, [])
        try await relaunched.resolve(legacy[0])
        let migrated = try StrictJSON.decode(XCTUnwrap(f.disk.bytes(.catalog))).object()
        XCTAssertEqual(migrated["version"], .number("3"))
        let row = try XCTUnwrap(migrated["records"]).array()[0].object()
        XCTAssertEqual(row["input_evidence"], .number("0"))
    }

    func testUnknownEvidenceAndOverlargeOutcomeBatchFailClosed() async throws {
        let f = try await setup()
        _ = try await f.coordinator.submit(f.focus, encodedBytes: Data([65]))
        _ = try await f.coordinator.drainOne(f.source)
        let batch = try await f.coordinator.pendingOutcomes(f.source)
        await expect(.capacity) {
            try await f.journal.recordInput(Array(repeating: batch.outcomes[0], count: 257), epoch: f.epoch)
        }
        var root = try StrictJSON.decode(XCTUnwrap(f.disk.bytes(.catalog))).object()
        var rows = try root.required("records").array(), row = try rows[0].object()
        row["input_evidence"] = .number("16"); rows[0] = .object(row); root["records"] = .array(rows)
        let invalid = try StrictJSON.encode(.object(root)); f.disk.seed(invalid, slot: .catalog)
        await expect(.corruptStore) { _ = try await f.journal.records() }
        XCTAssertEqual(f.disk.bytes(.catalog), invalid)
    }
}
