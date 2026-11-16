import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class TerminalMuxObservationTests: XCTestCase {
    private struct Seen: Equatable {
        let title: String, pane: Int?, ready: Bool, retired: Bool, issue: String?
        init(_ state: TerminalMuxControl.State, _ routing: TerminalMuxInputRouting.State) {
            title = state.panes.first?.title ?? ""
            pane = routing.destination?.paneID; ready = routing.ready; retired = routing.retired
            issue = state.failure
        }
    }
    private func pane(_ id: Int, title: String = "initial") -> TerminalMuxPane {
        .init(paneId: id, tabId: id, windowId: 0, tabTitle: "tab", title: title, workspace: "test",
              workingDir: nil, size: .init(rows: 24, cols: 80), isActive: id == 1, isZoomed: false)
    }
    private func fixture() throws -> TerminalMuxControl {
        let control = TerminalMuxControl()
        try control.inputRouting.replaceInventory([1, 2])
        let selected = try control.inputRouting.select(pane: 1)
        control.inputRouting.didApplyOutput(selected)
        control.publish(panes: [pane(1), pane(2)], shown: 1)
        control.setVisible(true)
        return control
    }
    private func eventually(_ condition: @MainActor () -> Bool, file: StaticString = #filePath, line: UInt = #line) async {
        let deadline = ContinuousClock.now + .seconds(3)
        while !condition(), ContinuousClock.now < deadline { await Task.yield() }
        XCTAssertTrue(condition(), file: file, line: line)
    }
    private func settle() async { for _ in 0..<40 { await Task.yield() } }

    func testInitialSnapshotNeedsNoSubsequentNetworkEvent() async throws {
        let control = try fixture()
        var seen: [Seen] = []
        let owner = TerminalMuxObservation(control: control) { seen.append(Seen($0, $1)) }
        await eventually { seen.count == 1 }
        XCTAssertEqual(seen.first?.title, "initial"); XCTAssertEqual(seen.first?.pane, 1)
        XCTAssertEqual(seen.first?.ready, true)
        await settle(); XCTAssertEqual(seen.count, 1)
        await owner.close()
    }

    func testOneHundredThousandUpdatesBehindBusyMainActorDeliverOnlyNewestState() async throws {
        let control = try fixture()
        var seen: [Seen] = []
        let owner = TerminalMuxObservation(control: control) { seen.append(Seen($0, $1)) }
        // No actor suspension until the producer burst ends. An implementation
        // with one MainActor task per event would retain 100,000 queued tasks.
        for index in 0..<100_000 { control.retitle(pane: 1, title: "title-\(index)") }
        await eventually { !seen.isEmpty }
        await settle()
        XCTAssertEqual(seen.count, 1)
        XCTAssertEqual(seen.first?.title, "title-99999")
        await owner.close()
    }

    func testConcurrentProducersConvergeOnAuthoritativeFinalSnapshot() async throws {
        let control = try fixture()
        var seen: [Seen] = []
        let owner = TerminalMuxObservation(control: control) { seen.append(Seen($0, $1)) }
        DispatchQueue.concurrentPerform(iterations: 10_000) { index in
            control.retitle(pane: 1, title: "concurrent-\(index)")
            control.setMouseGrabbed(index.isMultiple(of: 2))
        }
        control.retitle(pane: 1, title: "finished")
        await eventually { seen.last?.title == "finished" }
        await settle(); XCTAssertEqual(seen.count, 1)
        await owner.close()
    }

    func testInactiveObservationPublishesNothingUntilExplicitActivation() async throws {
        let control = try fixture()
        var seen: [Seen] = []
        let owner = TerminalMuxObservation(control: control, initiallyActive: false) { seen.append(Seen($0, $1)) }
        for index in 0..<20_000 { control.retitle(pane: 1, title: "hidden-\(index)") }
        await settle(); XCTAssertTrue(seen.isEmpty)
        owner.setActive(true)
        await eventually { seen.count == 1 }
        XCTAssertEqual(seen.first?.title, "hidden-19999")
        await owner.close()
    }

    func testDeactivateBeforeQueuedDeliveryPreventsItAndResumeNeedsNoNewEvent() async throws {
        let control = try fixture()
        var seen: [Seen] = []
        let owner = TerminalMuxObservation(control: control) { seen.append(Seen($0, $1)) }
        owner.setActive(false)
        await settle(); XCTAssertTrue(seen.isEmpty)
        owner.setActive(true)
        await eventually { seen.count == 1 }
        owner.setActive(false)
        owner.setActive(true)
        await eventually { seen.count == 2 }
        // Repeated visible-layout calls must not manufacture refreshes.
        for _ in 0..<10_000 { owner.setActive(true) }
        await settle(); XCTAssertEqual(seen.count, 2)
        await owner.close()
    }

    func testPauseCancelsPendingPresentationButNotAuthoritativeRouting() async throws {
        let control = try fixture()
        let original = try control.inputRouting.capture()
        var seen: [Seen] = []
        let owner = TerminalMuxObservation(control: control) { seen.append(Seen($0, $1)) }
        await eventually { seen.count == 1 }
        owner.setActive(false)
        let next = try control.inputRouting.select(pane: 2)
        control.publish(shown: 2)
        XCTAssertThrowsError(try control.inputRouting.validateAdmission(original))
        XCTAssertThrowsError(try control.inputRouting.validateAdmission(next))
        control.inputRouting.didApplyOutput(next)
        XCTAssertNoThrow(try control.inputRouting.validateAdmission(next))
        await settle(); XCTAssertEqual(seen.count, 1)
        owner.setActive(true)
        await eventually { seen.count == 2 }
        XCTAssertEqual(seen.last?.pane, 2); XCTAssertEqual(seen.last?.ready, true)
        await owner.close()
    }

    func testHiddenObservedRemovalAndReuseCannotReviveOriginalInput() async throws {
        let control = try fixture()
        let original = try control.inputRouting.capture()
        var seen: [Seen] = []
        let owner = TerminalMuxObservation(control: control, initiallyActive: false) { seen.append(Seen($0, $1)) }
        control.inputRouting.remove(pane: 1); control.remove(pane: 1)
        try control.inputRouting.replaceInventory([1, 2])
        control.publish(panes: [pane(1, title: "replacement"), pane(2)], shown: 1)
        let replacement = try control.inputRouting.select(pane: 1)
        control.inputRouting.didApplyOutput(replacement)
        XCTAssertNotEqual(original, replacement)
        XCTAssertThrowsError(try control.inputRouting.validateDelivery(original))
        owner.setActive(true)
        await eventually { seen.count == 1 }
        XCTAssertEqual(seen.first?.title, "replacement")
        await owner.close()
    }

    func testRoutingOnlyReadinessChangesAreDelivered() async throws {
        let control = try fixture()
        let next = try control.inputRouting.select(pane: 2)
        var seen: [Seen] = []
        let owner = TerminalMuxObservation(control: control) { seen.append(Seen($0, $1)) }
        await eventually { seen.count == 1 }
        XCTAssertEqual(seen.last?.ready, false)
        control.inputRouting.didApplyOutput(next)
        await eventually { seen.count == 2 }
        XCTAssertEqual(seen.last?.ready, true)
        // Duplicate acknowledgements produce neither revisions nor UI work.
        for _ in 0..<10_000 { control.inputRouting.didApplyOutput(next) }
        await settle(); XCTAssertEqual(seen.count, 2)
        await owner.close()
    }

    func testReentrantMutationIsDeliveredAsOneSuccessor() async throws {
        let control = try fixture()
        var seen: [Seen] = []
        let owner = TerminalMuxObservation(control: control) { state, routing in
            seen.append(Seen(state, routing))
            if seen.count == 1 {
                for index in 0..<1000 { control.retitle(pane: 1, title: "reentrant-\(index)") }
            }
        }
        await eventually { seen.count == 2 }
        await settle(); XCTAssertEqual(seen.count, 2)
        XCTAssertEqual(seen.last?.title, "reentrant-999")
        await owner.close()
    }

    func testRetireInsideDeliveryPreventsReentrantSuccessor() async throws {
        let control = try fixture()
        var count = 0
        var owner: TerminalMuxObservation?
        owner = TerminalMuxObservation(control: control) { _, _ in
            count += 1
            control.retitle(pane: 1, title: "queued")
            owner?.retire()
        }
        await eventually { count == 1 }
        await settle(); XCTAssertEqual(count, 1)
        await owner?.close(); owner = nil
    }

    func testCloseBeforeFirstDeliveryAndConcurrentCloseNeverPublish() async throws {
        let control = try fixture()
        var count = 0
        let owner = TerminalMuxObservation(control: control) { _, _ in count += 1 }
        owner.retire()
        async let first: Void = owner.close()
        async let second: Void = owner.close()
        _ = await (first, second)
        for index in 0..<1000 { control.retitle(pane: 1, title: "after-close-\(index)") }
        owner.setActive(true)
        await settle()
        XCTAssertTrue(owner.isRetired); XCTAssertFalse(owner.isActive); XCTAssertEqual(count, 0)
    }

    func testOldOwnerRetirementDoesNotUnsubscribeItsReplacement() async throws {
        let control = try fixture()
        var oldCount = 0, latest: String?
        let old = TerminalMuxObservation(control: control) { _, _ in oldCount += 1 }
        old.retire()
        let next = TerminalMuxObservation(control: control) { latest = $0.panes.first?.title; _ = $1 }
        await old.close()
        control.retitle(pane: 1, title: "new-owner")
        await eventually { latest == "new-owner" }
        XCTAssertEqual(oldCount, 0)
        await next.close()
    }

    func testDroppingOwnerReleasesItDespiteIdleWorkerAndProducerCallbacks() async throws {
        let control = try fixture()
        var count = 0
        var owner: TerminalMuxObservation? = TerminalMuxObservation(control: control) { _, _ in count += 1 }
        #if compiler(>=6.4)
        weak let weakOwner = owner
        #else
        weak var weakOwner = owner
        #endif
        await eventually { count == 1 }
        owner = nil
        XCTAssertNil(weakOwner)
        control.retitle(pane: 1, title: "unowned")
        await settle(); XCTAssertEqual(count, 1)
    }

    func testRetirementReleasesDeliveryCaptureBeforeJoining() async throws {
        final class Payload {}
        let control = try fixture()
        var payload: Payload? = Payload()
        #if compiler(>=6.4)
        weak let weakPayload = payload
        #else
        weak var weakPayload = payload
        #endif
        let owner = TerminalMuxObservation(control: control) { [payload] _, _ in _ = payload }
        payload = nil
        XCTAssertNotNil(weakPayload)
        owner.retire()
        XCTAssertNil(weakPayload)
        await owner.close()
    }

    func testTwentyHiddenMachinesHaveNoDeliveryUntilIndividuallySelected() async throws {
        let controls = try (0..<20).map { _ in try fixture() }
        var deliveries = [Int](repeating: 0, count: controls.count)
        var titles = [String](repeating: "", count: controls.count)
        let owners = controls.enumerated().map { index, control in
            TerminalMuxObservation(control: control, initiallyActive: false) { state, _ in
                deliveries[index] += 1; titles[index] = state.panes.first?.title ?? ""
            }
        }
        for index in controls.indices {
            for update in 0..<2000 { controls[index].retitle(pane: 1, title: "\(index)-\(update)") }
        }
        await settle(); XCTAssertEqual(deliveries.reduce(0, +), 0)
        for index in controls.indices {
            owners[index].setActive(true)
            await eventually { deliveries[index] == 1 }
            owners[index].setActive(false)
            XCTAssertEqual(titles[index], "\(index)-1999")
        }
        XCTAssertEqual(deliveries, [Int](repeating: 1, count: controls.count))
        for owner in owners { await owner.close() }
    }

    func testFailureAndRoutingRetirementConvergeWithoutChangingAuthority() async throws {
        let control = try fixture()
        var seen: [Seen] = []
        let owner = TerminalMuxObservation(control: control) { seen.append(Seen($0, $1)) }
        control.recordFailure("server stopped")
        control.inputRouting.retire()
        await eventually { seen.last?.retired == true }
        XCTAssertEqual(seen.last?.issue, "server stopped")
        XCTAssertEqual(seen.last?.ready, false); XCTAssertNil(seen.last?.pane)
        XCTAssertThrowsError(try control.inputRouting.capture())
        await owner.close()
    }
}
