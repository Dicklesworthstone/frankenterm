import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class TerminalRenderResourceBudgetTests: XCTestCase {
    func testLowPowerCapsPresentationAndStopsIdleBlinkingAtEveryTemperature() {
        for state in TerminalRenderPowerPolicy.ThermalState.allCases {
            let ordinary = TerminalRenderPowerPolicy(thermalState: state, lowPower: false)
            let conserving = TerminalRenderPowerPolicy(thermalState: state, lowPower: true)
            XCTAssertLessThanOrEqual(conserving.maximumFramesPerSecond, 30)
            XCTAssertLessThanOrEqual(conserving.maximumFramesPerSecond, ordinary.maximumFramesPerSecond)
            XCTAssertLessThanOrEqual(conserving.preferredFramesPerSecond, ordinary.preferredFramesPerSecond)
            XCTAssertFalse(conserving.animatesBlink)
        }
    }

    func testIncreasingHeatNeverIncreasesWorkAndCriticalHeatStillPresentsOutput() {
        for lowPower in [false, true] {
            let policies = TerminalRenderPowerPolicy.ThermalState.allCases.map {
                TerminalRenderPowerPolicy(thermalState: $0, lowPower: lowPower)
            }
            for (cooler, warmer) in zip(policies, policies.dropFirst()) {
                XCTAssertLessThanOrEqual(warmer.maximumFramesPerSecond, cooler.maximumFramesPerSecond)
                XCTAssertLessThanOrEqual(warmer.preferredFramesPerSecond, cooler.preferredFramesPerSecond)
                XCTAssertFalse(warmer.animatesBlink && !cooler.animatesBlink)
            }
            for policy in policies {
                XCTAssertGreaterThan(policy.minimumFramesPerSecond, 0)
                XCTAssertLessThanOrEqual(policy.minimumFramesPerSecond, policy.preferredFramesPerSecond)
                XCTAssertLessThanOrEqual(policy.preferredFramesPerSecond, policy.maximumFramesPerSecond)
            }
            let critical = policies.last!
            XCTAssertEqual(critical.maximumFramesPerSecond, 15)
            XCTAssertFalse(critical.animatesBlink)
        }
    }

    func testBatteryAndThermalConstraintsMustBothClearBeforeBlinkingResumes() {
        let normal = TerminalRenderPowerPolicy(thermalState: .nominal, lowPower: false)
        XCTAssertEqual(normal.preferredFramesPerSecond, 60)
        XCTAssertTrue(normal.animatesBlink)
        let stillHot = TerminalRenderPowerPolicy(thermalState: .serious, lowPower: false)
        XCTAssertEqual(stillHot.maximumFramesPerSecond, 30)
        XCTAssertFalse(stillHot.animatesBlink)
        let stillConserving = TerminalRenderPowerPolicy(thermalState: .nominal, lowPower: true)
        XCTAssertEqual(stillConserving.maximumFramesPerSecond, 30)
        XCTAssertFalse(stillConserving.animatesBlink)
    }

    func testExactCapacityNeverGrantsAnExtraResource() {
        let budget = TerminalRenderResourceBudget(maximumResources: 6) {}
        var leases = (0..<6).compactMap { _ in budget.acquire() }
        XCTAssertEqual(leases.count, 6); XCTAssertEqual(budget.retainedResources, 6)
        XCTAssertFalse(budget.hasCapacity); XCTAssertNil(budget.acquire())
        leases.removeLast()
        XCTAssertEqual(budget.retainedResources, 5); XCTAssertTrue(budget.hasCapacity)
        leases.append(budget.acquire()!)
        XCTAssertEqual(budget.retainedResources, 6)
        leases.removeAll(); XCTAssertEqual(budget.retainedResources, 0)
    }

    func testCacheEvictionDoesNotReleaseCommandOwnedLease() {
        let budget = TerminalRenderResourceBudget(maximumResources: 1) {}
        var cache = [0: budget.acquire()!]
        var submitted = cache[0]
        cache.removeAll()
        XCTAssertEqual(budget.retainedResources, 1)
        XCTAssertNil(budget.acquire())
        XCTAssertNotNil(submitted)
        submitted = nil
        XCTAssertEqual(budget.retainedResources, 0)
        XCTAssertNotNil(budget.acquire())
    }

    func testMemoryPressureReclaimsIdleSlotsButPreservesPreparedAndSubmittedOwners() {
        let budget = TerminalRenderResourceBudget(maximumResources: 3) {}
        var cache = Dictionary(uniqueKeysWithValues: (0..<3).map { ($0, budget.acquire()!) })
        var prepared = cache[0], submitted = cache[1]
        cache.removeAll()
        XCTAssertEqual(budget.retainedResources, 2, "Only the idle third slot is reclaimable")
        let anotherScene = budget.acquire()
        XCTAssertNotNil(anotherScene)
        for _ in 0..<100 { cache.removeAll() }
        XCTAssertEqual(budget.retainedResources, 3)
        XCTAssertNil(budget.acquire(), "Repeated warnings cannot release another owner's reservation")
        XCTAssertNotNil(prepared); XCTAssertNotNil(submitted)
        prepared = nil
        XCTAssertEqual(budget.retainedResources, 2)
        submitted = nil
        XCTAssertEqual(budget.retainedResources, 1)
        withExtendedLifetime(anotherScene) {}
    }

    func testManyReleasesCoalesceIntoOneAvailabilityWake() async {
        var calls = 0
        let awakened = expectation(description: "availability")
        let budget = TerminalRenderResourceBudget(maximumResources: 6) {
            calls += 1; awakened.fulfill()
        }
        for _ in 0..<10_000 {
            var lease = budget.acquire()
            XCTAssertNotNil(lease)
            lease = nil
        }
        XCTAssertEqual(calls, 0)
        XCTAssertEqual(budget.retainedResources, 0)
        let result = await XCTWaiter.fulfillment(of: [awakened], timeout: 2)
        XCTAssertEqual(result, .completed)
        XCTAssertEqual(calls, 1)
    }

    func testAvailabilityCallbackCanReenterAdmissionWithoutDeadlock() async {
        var replacement: TerminalRenderResourceBudget.Lease?
        var budget: TerminalRenderResourceBudget?
        let awakened = expectation(description: "readmit")
        var handled = false
        budget = TerminalRenderResourceBudget(maximumResources: 1) {
            guard !handled else { return }
            handled = true
            replacement = budget?.acquire()
            awakened.fulfill()
        }
        var original = budget?.acquire()
        XCTAssertNotNil(original)
        original = nil
        let result = await XCTWaiter.fulfillment(of: [awakened], timeout: 2)
        XCTAssertEqual(result, .completed)
        XCTAssertNotNil(replacement); XCTAssertEqual(budget?.retainedResources, 1)
        replacement = nil
        XCTAssertEqual(budget?.retainedResources, 0)
        budget = nil
    }

    func testReleaseBeforeWaiterInstallationIsVisibleInCapacityCheck() {
        let budget = TerminalRenderResourceBudget(maximumResources: 1) {}
        var owner = budget.acquire()
        XCTAssertNotNil(owner); XCTAssertNil(budget.acquire())
        owner = nil
        // Native preparation may report its refusal after this release. The
        // view's post-refusal check observes room even if the wake was earlier.
        XCTAssertTrue(budget.hasCapacity)
        let resumed = budget.acquire()
        XCTAssertNotNil(resumed)
        withExtendedLifetime(resumed) { XCTAssertFalse(budget.hasCapacity) }
    }

    func testRepeatedFleetEvictionReusesBudgetWithoutRaisingItsLimit() {
        let budget = TerminalRenderResourceBudget(maximumResources: 6) {}
        var submitted: [TerminalRenderResourceBudget.Lease] = []
        for _ in 0..<200 {
            var selected = (0..<3).compactMap { _ in budget.acquire() }
            XCTAssertEqual(selected.count, 3)
            submitted.append(selected[0])
            selected.removeAll()
            XCTAssertEqual(budget.retainedResources, submitted.count)
            if submitted.count == 3 { submitted.removeFirst() }
        }
        submitted.removeAll()
        XCTAssertEqual(budget.maximumResources, 6)
        XCTAssertEqual(budget.retainedResources, 0)
    }

    func testConcurrentAdmissionAndRetirementRespectGlobalCapacity() async {
        let budget = TerminalRenderResourceBudget(maximumResources: 6) {}
        let peak = await withTaskGroup(of: Int.self, returning: Int.self) { group in
            for _ in 0..<16 {
                group.addTask {
                    var peak = 0
                    for _ in 0..<500 {
                        if let lease = budget.acquire() {
                            peak = max(peak, budget.retainedResources)
                            await Task.yield()
                            withExtendedLifetime(lease) {}
                        } else { await Task.yield() }
                    }
                    return peak
                }
            }
            var peak = 0
            for await value in group { peak = max(peak, value) }
            return peak
        }
        XCTAssertGreaterThan(peak, 0); XCTAssertLessThanOrEqual(peak, 6)
        XCTAssertEqual(budget.retainedResources, 0)
    }

    func testDrawableRecoveryCoalescesRequestsAndStopsAfterABoundedBurst() {
        var recovery = TerminalDrawableRetry()
        let start = ContinuousClock.now
        var now = start, previousDelay: TimeInterval = 0
        var scheduled = 0
        for _ in 0..<32 {
            switch recovery.unavailable(at: now) {
            case let .schedule(id, delay):
                scheduled += 1
                XCTAssertGreaterThan(delay, 0)
                XCTAssertGreaterThanOrEqual(delay, previousDelay)
                previousDelay = delay
                XCTAssertTrue(recovery.isWaiting)
                for _ in 0..<100 {
                    XCTAssertEqual(recovery.unavailable(at: now), .pending,
                        "new output cannot add a wake or replenish a waiting episode")
                }
                now = now.advanced(by: .seconds(delay))
                XCTAssertEqual(recovery.wake(id, at: now), .ready)
                XCTAssertFalse(recovery.isWaiting)
            case .exhausted: break
            case .pending: XCTFail("each scheduled wake was consumed")
            }
            if recovery.isExhausted { break }
        }
        XCTAssertGreaterThan(scheduled, 1, "a transient refusal must receive automatic recovery")
        XCTAssertLessThan(scheduled, 32, "a permanently missing drawable must stop requesting frames")
        XCTAssertTrue(recovery.isExhausted)
        XCTAssertFalse(recovery.isWaiting)
        XCTAssertLessThanOrEqual(start.duration(to: now), .seconds(2))
        for _ in 0..<100 {
            XCTAssertEqual(recovery.unavailable(at: now), .exhausted,
                "an output flood cannot silently restart the exhausted burst")
        }
    }

    func testLateDrawableWakeExhaustsInsteadOfRenderingAfterItsDeadline() {
        var recovery = TerminalDrawableRetry()
        let now = ContinuousClock.now
        guard case let .schedule(id, _) = recovery.unavailable(at: now) else {
            return XCTFail("first refusal should schedule recovery")
        }
        XCTAssertEqual(recovery.wake(id, at: now.advanced(by: .seconds(3))), .exhausted)
        XCTAssertTrue(recovery.isExhausted)
        XCTAssertFalse(recovery.isWaiting)
        XCTAssertEqual(recovery.wake(id, at: now.advanced(by: .seconds(4))), .stale)
        XCTAssertEqual(recovery.unavailable(at: now.advanced(by: .seconds(4))), .exhausted)
    }

    func testDrawableLifecycleResetCannotReviveAnEarlierTimerOrConsumeANewerOne() {
        var recovery = TerminalDrawableRetry()
        let now = ContinuousClock.now
        guard case let .schedule(retired, _) = recovery.unavailable(at: now) else {
            return XCTFail("first refusal should schedule recovery")
        }
        recovery.reset()
        XCTAssertFalse(recovery.isWaiting); XCTAssertFalse(recovery.isExhausted)
        guard case let .schedule(current, delay) = recovery.unavailable(at: now) else {
            return XCTFail("a new presentation owns a new recovery episode")
        }
        XCTAssertNotEqual(retired, current)
        XCTAssertEqual(recovery.wake(retired, at: now), .stale)
        XCTAssertTrue(recovery.isWaiting, "the old callback cannot consume the new timer")
        XCTAssertEqual(recovery.wake(current, at: now.advanced(by: .seconds(delay))), .ready)
        recovery.reset() // successful acquisition has the same cancellation boundary
        XCTAssertEqual(recovery.wake(current, at: now), .stale)
        XCTAssertFalse(recovery.isWaiting); XCTAssertFalse(recovery.isExhausted)
    }

    func testTimeSpentWithoutADrawableCountsAgainstTheWholeRecoveryEpisode() {
        var recovery = TerminalDrawableRetry()
        let now = ContinuousClock.now
        guard case let .schedule(id, delay) = recovery.unavailable(at: now) else {
            return XCTFail("first refusal should schedule recovery")
        }
        XCTAssertEqual(recovery.wake(id, at: now.advanced(by: .seconds(delay))), .ready)
        // Acquiring a drawable can itself take time. A later failure does not
        // earn another full deadline, even though it was only the second try.
        XCTAssertEqual(recovery.unavailable(at: now.advanced(by: .seconds(3))), .exhausted)
        recovery.reset() // a deliberate Retry display starts over
        guard case .schedule = recovery.unavailable(at: now.advanced(by: .seconds(4))) else {
            return XCTFail("explicit recovery should admit a fresh bounded episode")
        }
    }
}
