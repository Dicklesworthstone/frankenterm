import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
private final class BatchProbe {
    struct Resource: Sendable, Equatable { let id: Int, token: UUID }
    enum Failure: Error { case connect, dispose }
    var started: [Int] = [], disposed: [Resource] = []
    var waiting: [Int: CheckedContinuation<Resource, any Error>] = [:]
    var cleanup: [UUID: CheckedContinuation<Void, any Error>] = [:]
    var blockCleanup = false, failCleanup = false
    var active = 0, maximumActive = 0, cleanupWasCancelled = false
    func connect(_ id: Int) async throws -> Resource {
        started.append(id); active += 1; maximumActive = max(maximumActive, active)
        defer { active -= 1 }
        return try await withCheckedThrowingContinuation { waiting[id] = $0 }
    }
    func succeed(_ id: Int) -> Resource {
        let value = Resource(id: id, token: UUID())
        waiting.removeValue(forKey: id)?.resume(returning: value)
        return value
    }
    func fail(_ id: Int) { waiting.removeValue(forKey: id)?.resume(throwing: Failure.connect) }
    func dispose(_ value: Resource) async throws {
        cleanupWasCancelled = cleanupWasCancelled || Task.isCancelled
        disposed.append(value)
        if failCleanup { throw Failure.dispose }
        if blockCleanup { try await withCheckedThrowingContinuation { cleanup[value.token] = $0 } }
    }
    func finishCleanup(_ token: UUID) { cleanup.removeValue(forKey: token)?.resume() }
    func releaseAll() {
        let pending = waiting; waiting.removeAll()
        for (id, continuation) in pending { continuation.resume(returning: Resource(id: id, token: UUID())) }
        let closing = cleanup; cleanup.removeAll()
        for continuation in closing.values { continuation.resume() }
    }
}

@MainActor
final class TerminalConnectionBatchTests: XCTestCase {
    private typealias Batch = TerminalConnectionBatch<Int, BatchProbe.Resource>
    private func batch(_ probe: BatchProbe, concurrency: Int = 3, maximum: Int = 128) throws -> Batch {
        try Batch(maximumConcurrent: concurrency, maximumRequests: maximum,
                  connect: { try await probe.connect($0) }, dispose: { try await probe.dispose($0) })
    }
    private func eventually(_ predicate: () -> Bool, file: StaticString = #filePath, line: UInt = #line) async {
        let limit = ContinuousClock.now + .seconds(3)
        while !predicate(), ContinuousClock.now < limit { await Task.yield() }
        XCTAssertTrue(predicate(), file: file, line: line)
    }

    func testConstructingAndPromotingDoNotAuthorizeConnections() async throws {
        let probe = BatchProbe(), batch = try batch(probe)
        batch.promote(7)
        await Task.yield()
        XCTAssertTrue(probe.started.isEmpty)
        XCTAssertTrue(batch.states.isEmpty)
        XCTAssertNil(batch.take(7))
        try await batch.close()
    }

    func testThreeWorkersPreserveApprovedOrderingAndProgressPastFailure() async throws {
        let probe = BatchProbe(), batch = try batch(probe)
        defer { probe.releaseAll() }
        var failures: [Int] = []
        batch.onFailure = { id, _ in failures.append(id) }
        try batch.enqueue([4, 2, 9, 1, 7])
        await eventually { probe.started.count == 3 }
        XCTAssertEqual(Set(probe.started), Set([4, 2, 9]))
        XCTAssertEqual(batch.queuedCount, 2)
        probe.fail(2)
        await eventually { probe.started.contains(1) }
        _ = probe.succeed(4)
        await eventually { probe.started.contains(7) }
        for id in [9, 1, 7] { _ = probe.succeed(id) }
        await batch.waitUntilSettled()
        XCTAssertEqual(batch.readyCount, 4)
        XCTAssertEqual(batch.states[2], .failed)
        XCTAssertEqual(failures, [2])
        XCTAssertEqual(probe.maximumActive, 3)
        try await batch.close()
        XCTAssertEqual(Set(probe.disposed.map(\.id)), Set([4, 9, 1, 7]))
        XCTAssertEqual(batch.retainedResourceCount, 0)
    }

    func testNavigationOnlyPromotesQueuedRequests() async throws {
        let probe = BatchProbe(), batch = try batch(probe, concurrency: 1)
        defer { probe.releaseAll() }
        try batch.enqueue([1, 2, 3, 4])
        await eventually { probe.started == [1] }
        for _ in 0..<10_000 { batch.promote(4); batch.promote(99) }
        _ = probe.succeed(1)
        await eventually { probe.started == [1, 4] }
        _ = probe.succeed(4)
        await eventually { probe.started == [1, 4, 2] }
        _ = probe.succeed(2)
        await eventually { probe.started == [1, 4, 2, 3] }
        _ = probe.succeed(3)
        await batch.waitUntilSettled()
        XCTAssertEqual(batch.states.count, 4)
        try await batch.close()
    }

    func testRepeatedApprovalsNeverDuplicateActiveReadyOrClaimedValues() async throws {
        let probe = BatchProbe(), batch = try batch(probe)
        defer { probe.releaseAll() }
        try batch.enqueue([1, 2])
        for _ in 0..<100 { try batch.enqueue([1, 2]) }
        await eventually { probe.started.count == 2 }
        let first = probe.succeed(1)
        _ = probe.succeed(2)
        await batch.waitUntilSettled()
        XCTAssertEqual(batch.take(1), first)
        XCTAssertNil(batch.take(1))
        for _ in 0..<100 { try batch.enqueue([1, 2]) }
        XCTAssertEqual(probe.started.count, 2)
        try await batch.close()
        XCTAssertEqual(probe.disposed.map(\.id), [2])
        XCTAssertEqual(batch.states[1], .claimed)
    }

    func testFailuresRequireAnotherExplicitApprovalToRetry() async throws {
        let probe = BatchProbe(), batch = try batch(probe, concurrency: 1)
        defer { probe.releaseAll() }
        try batch.enqueue([1])
        await eventually { probe.waiting[1] != nil }
        probe.fail(1)
        await batch.waitUntilSettled()
        for _ in 0..<100 { batch.promote(1) }
        XCTAssertEqual(probe.started, [1])
        try batch.enqueue([1])
        await eventually { probe.started.count == 2 }
        _ = probe.succeed(1)
        await batch.waitUntilSettled()
        XCTAssertEqual(batch.states[1], .ready)
        try await batch.close()
    }

    func testCancellationBeforeAWorkerStartsNeverCallsConnect() async throws {
        let probe = BatchProbe(), batch = try batch(probe)
        try batch.enqueue([1, 2, 3, 4, 5])
        batch.cancelPending()
        await batch.waitUntilSettled()
        XCTAssertTrue(probe.started.isEmpty)
        XCTAssertTrue(batch.states.values.allSatisfy { $0 == .cancelled })
        try await batch.close()
    }

    func testCancelledLateSuccessIsDisposedOnAnUncancelledWorker() async throws {
        let probe = BatchProbe(), batch = try batch(probe, concurrency: 1)
        defer { probe.releaseAll() }
        try batch.enqueue([1, 2])
        await eventually { probe.started == [1] }
        batch.cancelPending()
        XCTAssertEqual(batch.activeCount, 1, "A cancelled native call still owns its slot")
        XCTAssertEqual(batch.states[2], .cancelled)
        let late = probe.succeed(1)
        await batch.waitUntilSettled()
        XCTAssertNil(batch.take(1))
        XCTAssertEqual(probe.disposed, [late])
        XCTAssertFalse(probe.cleanupWasCancelled)
        XCTAssertEqual(probe.started, [1])
        try await batch.close()
    }

    func testCancelPendingPreservesReadyConnections() async throws {
        let probe = BatchProbe(), batch = try batch(probe, concurrency: 1)
        defer { probe.releaseAll() }
        try batch.enqueue([1, 2])
        await eventually { probe.waiting[1] != nil }
        let first = probe.succeed(1)
        await eventually { probe.waiting[2] != nil }
        batch.cancelPending()
        _ = probe.succeed(2)
        await batch.waitUntilSettled()
        XCTAssertEqual(batch.take(1), first)
        XCTAssertEqual(probe.disposed.map(\.id), [2])
        try await batch.close()
    }

    func testCancelledNativeCallCannotBeReplacedBeforeItSettles() async throws {
        let probe = BatchProbe(), batch = try batch(probe, concurrency: 1)
        defer { probe.releaseAll() }
        try batch.enqueue([1])
        await eventually { probe.waiting[1] != nil }
        batch.cancelPending()
        try batch.enqueue([1, 2])
        XCTAssertEqual(batch.activeCount, 1)
        XCTAssertEqual(probe.started, [1])
        _ = probe.succeed(1)
        await eventually { probe.waiting[2] != nil }
        _ = probe.succeed(2)
        await batch.waitUntilSettled()
        XCTAssertEqual(probe.started, [1, 2])
        XCTAssertEqual(probe.disposed.map(\.id), [1])
        try await batch.close()
    }

    func testClosingRetainsResourceAndConcurrentCallersJoinTheSameCleanup() async throws {
        let probe = BatchProbe(), batch = try batch(probe)
        defer { probe.releaseAll() }
        try batch.enqueue([1])
        await eventually { probe.waiting[1] != nil }
        let resource = probe.succeed(1)
        await batch.waitUntilSettled()
        probe.blockCleanup = true
        var completed = 0
        let first = Task { try await batch.close(); completed += 1 }
        let second = Task { try await batch.close(); completed += 1 }
        await eventually { probe.cleanup[resource.token] != nil }
        XCTAssertEqual(batch.retainedResourceCount, 1)
        XCTAssertEqual(completed, 0)
        XCTAssertNil(batch.take(1))
        probe.finishCleanup(resource.token)
        try await first.value; try await second.value
        XCTAssertEqual(completed, 2)
        XCTAssertEqual(probe.disposed, [resource])
    }

    func testCleanupFailureRetainsExactResourceForAnExplicitRetry() async throws {
        let probe = BatchProbe(), batch = try batch(probe)
        defer { probe.releaseAll() }
        try batch.enqueue([1])
        await eventually { probe.waiting[1] != nil }
        let resource = probe.succeed(1)
        await batch.waitUntilSettled()
        probe.failCleanup = true
        do { try await batch.close(); XCTFail("Lost failed-cleanup ownership") }
        catch { XCTAssertEqual(error as? TerminalConnectionBatchError, .cleanupIncomplete) }
        XCTAssertEqual(batch.retainedResourceCount, 1)
        XCTAssertEqual(batch.states[1], .cleanupFailed)
        XCTAssertNil(batch.take(1))
        probe.failCleanup = false
        try await batch.close()
        XCTAssertEqual(probe.disposed, [resource, resource])
        XCTAssertEqual(batch.retainedResourceCount, 0)
    }

    func testRetirementKeepsTheWorkerBoundWhileDrainingManyReadyValues() async throws {
        let probe = BatchProbe(), batch = try batch(probe)
        defer { probe.releaseAll() }
        try batch.enqueue(Array(0..<12))
        for id in 0..<12 {
            await eventually { probe.waiting[id] != nil }
            _ = probe.succeed(id)
        }
        await batch.waitUntilSettled()
        probe.blockCleanup = true
        batch.retire()
        await eventually { probe.cleanup.count == 3 }
        XCTAssertEqual(batch.activeCount, 3)
        XCTAssertEqual(probe.disposed.count, 3)
        for _ in 0..<4 {
            let tokens = Array(probe.cleanup.keys)
            tokens.forEach(probe.finishCleanup)
            if probe.disposed.count < 12 { await eventually { probe.cleanup.count == 3 } }
        }
        probe.blockCleanup = false; probe.releaseAll()
        try await batch.close()
        XCTAssertEqual(Set(probe.disposed.map(\.id)), Set(0..<12))
        XCTAssertEqual(probe.disposed.count, 12)
    }

    func testCancelledCloseCallerStillJoinsCleanup() async throws {
        let probe = BatchProbe(), batch = try batch(probe)
        defer { probe.releaseAll() }
        try batch.enqueue([1])
        await eventually { probe.waiting[1] != nil }
        let value = probe.succeed(1)
        await batch.waitUntilSettled()
        probe.blockCleanup = true
        let close = Task { try await batch.close() }
        await eventually { probe.cleanup[value.token] != nil }
        close.cancel()
        XCTAssertEqual(batch.retainedResourceCount, 1)
        probe.finishCleanup(value.token)
        try await close.value
        XCTAssertFalse(probe.cleanupWasCancelled)
        XCTAssertEqual(batch.retainedResourceCount, 0)
    }

    func testInvalidAdmissionIsAtomicAndLifetimeInventoryIsBounded() async throws {
        let probe = BatchProbe(), batch = try batch(probe, concurrency: 1, maximum: 2)
        defer { probe.releaseAll() }
        XCTAssertThrowsError(try batch.enqueue([1, 1]))
        XCTAssertTrue(batch.states.isEmpty)
        XCTAssertThrowsError(try batch.enqueue([1, 2, 3]))
        XCTAssertTrue(batch.states.isEmpty)
        try batch.enqueue([1, 2])
        XCTAssertThrowsError(try batch.enqueue([3]))
        XCTAssertEqual(Set(batch.states.keys), Set([1, 2]))
        batch.retire()
        XCTAssertThrowsError(try batch.enqueue([1]))
        try await batch.close()
    }

    func testInvalidLimitsAreRefused() async throws {
        let probe = BatchProbe()
        for (concurrency, count) in [(0, 128), (4, 128), (3, 2), (1, 0), (1, 129)] {
            XCTAssertThrowsError(try batch(probe, concurrency: concurrency, maximum: count))
        }
    }

    func testTwentyMachineFleetSettlesWithBoundedResourcesAndOneShotClaims() async throws {
        let probe = BatchProbe(), batch = try batch(probe)
        defer { probe.releaseAll() }
        let expected = [10, 11, 9] + (0..<20).filter { ![10, 11, 9].contains($0) }
        try batch.enqueue(expected)
        for id in expected {
            await eventually { probe.waiting[id] != nil }
            _ = probe.succeed(id)
        }
        await batch.waitUntilSettled()
        XCTAssertEqual(batch.readyCount, 20)
        XCTAssertLessThanOrEqual(probe.maximumActive, 3)
        let retained = expected.prefix(4).compactMap { batch.take($0) }
        XCTAssertEqual(retained.count, 4)
        try await batch.close()
        XCTAssertEqual(probe.disposed.count, 16)
        XCTAssertEqual(Set(probe.disposed.map(\.id)), Set(expected.dropFirst(4)))
    }

    func testCompletionCallbacksMayClaimReadyOwnershipSynchronously() async throws {
        let probe = BatchProbe(), batch = try batch(probe, concurrency: 1)
        defer { probe.releaseAll() }
        var claimed: [BatchProbe.Resource] = []
        batch.onChange = { [weak batch] in
            guard let batch, let value = batch.take(1) else { return }
            claimed.append(value)
        }
        try batch.enqueue([1])
        await eventually { probe.waiting[1] != nil }
        let value = probe.succeed(1)
        await batch.waitUntilSettled()
        XCTAssertEqual(claimed, [value])
        try await batch.close()
        XCTAssertTrue(probe.disposed.isEmpty)
    }

    func testPriorityUsesTheFullRingEvenWhenSelectedMachineIsAlreadyConnected() async throws {
        let inventory = ["one", "two", "three", "four", "five"]
        XCTAssertEqual(try TerminalConnectionOrder.prioritize(inventory, selected: "three",
            including: Set(inventory)), ["three", "four", "two", "one", "five"])
        XCTAssertEqual(try TerminalConnectionOrder.prioritize(inventory, selected: "three",
            including: ["one", "two", "four", "five"]), ["four", "two", "one", "five"])
        XCTAssertEqual(try TerminalConnectionOrder.prioritize(inventory, selected: "one",
            including: Set(inventory)), ["one", "two", "five", "three", "four"])
    }

    func testPriorityCannotAddUnapprovedHostsAndHandlesEmptyAndSingleRings() async throws {
        XCTAssertEqual(try TerminalConnectionOrder.prioritize([Int](), selected: nil, including: []), [])
        XCTAssertEqual(try TerminalConnectionOrder.prioritize([1], selected: 1, including: [1]), [1])
        XCTAssertEqual(try TerminalConnectionOrder.prioritize([1, 2], selected: 1, including: [1, 2]), [1, 2])
        XCTAssertEqual(try TerminalConnectionOrder.prioritize([1, 2, 3], selected: 99, including: [2]), [2])
        XCTAssertThrowsError(try TerminalConnectionOrder.prioritize([1, 2], selected: 1, including: [3]))
        XCTAssertThrowsError(try TerminalConnectionOrder.prioritize([1, 1], selected: 1, including: [1]))
        XCTAssertThrowsError(try TerminalConnectionOrder.prioritize(Array(0..<129), selected: 1, including: [1]))
    }

    func testMaximumInventoryFailureStormCannotLeakWorkersOrRestartRequests() async throws {
        let probe = BatchProbe(), batch = try batch(probe)
        defer { probe.releaseAll() }
        var failed = Set<Int>()
        batch.onFailure = { id, _ in failed.insert(id) }
        try batch.enqueue(Array(0..<128))
        for id in 0..<128 {
            await eventually { probe.waiting[id] != nil }
            probe.fail(id)
        }
        await batch.waitUntilSettled()
        XCTAssertEqual(failed, Set(0..<128))
        XCTAssertEqual(batch.activeCount, 0)
        XCTAssertEqual(batch.retainedResourceCount, 0)
        XCTAssertEqual(probe.started.count, 128)
        XCTAssertLessThanOrEqual(probe.maximumActive, 3)
        try await batch.close()
    }
}
