import Foundation
import XCTest
@testable import FrankenTermKit

final class TerminalNavigationGroupTests: XCTestCase {
    private func machines(_ count: Int = 4) throws -> [TerminalNavigationMachine] {
        try (0..<count).map { machine in
            try TerminalNavigationMachine(id: "m\(machine)", name: "Machine \(machine)", panes: (0..<2).map { pane in
                try TerminalNavigationPane(id: .init(id: UUID(), incarnation: UUID()), title: "pane \(pane)")
            })
        }
    }
    private func group(_ id: String, _ members: [String]) throws -> TerminalNavigationGroup {
        try .init(id: id, machineIDs: members)
    }

    func testInitialRingUsesGroupOrderNotWorkspaceOrder() throws {
        let all = try machines()
        var navigation = try TerminalNavigation(machines: all, group: group("work", ["m3", "m1", "m0"]))
        XCTAssertEqual(navigation.machines.map(\.id), ["m3", "m1", "m0"])
        XCTAssertEqual(navigation.selection?.machineID, "m3")
        let previous = try XCTUnwrap(navigation.plan(axis: .machine, direction: .previous))
        XCTAssertTrue(previous.wraps); XCTAssertEqual(previous.destination.machineID, "m0")
        try navigation.commit(previous)
        let next = try XCTUnwrap(navigation.plan(axis: .machine, direction: .next))
        XCTAssertTrue(next.wraps); XCTAssertEqual(next.destination.machineID, "m3")
    }

    func testInvalidGroupsAndOutOfGroupInitialSelectionAreRefused() throws {
        let all = try machines()
        XCTAssertThrowsError(try group("", ["m0"]))
        XCTAssertThrowsError(try group("g", ["m0", "m0"]))
        XCTAssertThrowsError(try group("g", [""]))
        XCTAssertThrowsError(try group("g", (0..<129).map { "m\($0)" }))
        XCTAssertThrowsError(try TerminalNavigation(machines: all, group: group("g", ["unknown"])))
        XCTAssertThrowsError(try TerminalNavigation(machines: all, initialMachineID: "m1", group: group("g", ["m0"])))
    }

    func testEmptyAndSingleMemberGroupsNeverEscapeIntoWorkspace() throws {
        let all = try machines()
        let empty = try TerminalNavigation(machines: all, group: group("empty", []))
        XCTAssertNil(empty.selection); XCTAssertTrue(empty.machines.isEmpty)
        XCTAssertNil(try empty.plan(axis: .machine, direction: .next))
        let one = try TerminalNavigation(machines: all, group: group("one", ["m2"]))
        XCTAssertNil(try one.plan(axis: .machine, direction: .previous))
        XCTAssertThrowsError(try one.plan(machineID: "m0"))
        XCTAssertThrowsError(try one.plan(paneID: all[0].panes[0].id))
        XCTAssertEqual(try one.plan(axis: .pane, direction: .next)?.destination.paneID, all[2].panes[1].id)
    }

    func testOfflineMembersRemainInTheirImportedPosition() throws {
        var all = try machines()
        all[1] = try .init(id: "m1", name: "offline", panes: [])
        var navigation = try TerminalNavigation(machines: all, group: group("g", ["m0", "m1", "m3"]))
        try navigation.commit(XCTUnwrap(navigation.plan(axis: .machine, direction: .next)))
        XCTAssertEqual(navigation.selection, .init(machineID: "m1", paneID: nil))
        XCTAssertEqual(try navigation.plan(axis: .machine, direction: .next)?.destination.machineID, "m3")
    }

    func testPreparingAGroupPlanDoesNotPublishMembershipOrSelection() throws {
        var navigation = try TerminalNavigation(machines: machines(), group: group("a", ["m0", "m1"]))
        let plan = try XCTUnwrap(navigation.plan(group: group("b", ["m3", "m2"]), selecting: "m2"))
        XCTAssertEqual(navigation.group?.id, "a"); XCTAssertEqual(navigation.selection?.machineID, "m0")
        XCTAssertEqual(navigation.machines.map(\.id), ["m0", "m1"])
        try navigation.commit(plan)
        XCTAssertEqual(navigation.group?.id, "b"); XCTAssertEqual(navigation.selection?.machineID, "m2")
        XCTAssertEqual(navigation.machines.map(\.id), ["m3", "m2"])
    }

    func testReturningToWholeWorkspaceIsAlsoAGroupTransaction() throws {
        var navigation = try TerminalNavigation(machines: machines(), group: group("g", ["m2", "m0"]))
        let plan = try XCTUnwrap(navigation.plan(group: nil, selecting: "m1"))
        try navigation.commit(plan)
        XCTAssertNil(navigation.group)
        XCTAssertEqual(navigation.machines.map(\.id), ["m0", "m1", "m2", "m3"])
    }

    func testOverlappingGroupSwitchInvalidatesOldGesturesAndViewportCallbacks() throws {
        var navigation = try TerminalNavigation(machines: machines(), group: group("a", ["m0", "m1"]))
        let oldSwipe = try XCTUnwrap(navigation.plan(axis: .machine, direction: .next))
        let oldView = try XCTUnwrap(navigation.viewWitness)
        let plan = try XCTUnwrap(navigation.plan(group: group("b", ["m0", "m2"]), selecting: "m0"))
        XCTAssertEqual(plan.destination, navigation.selection)
        try navigation.commit(plan)
        XCTAssertThrowsError(try navigation.commit(oldSwipe))
        XCTAssertThrowsError(try navigation.remember(.init(topRow: 7), for: oldView))
        XCTAssertEqual(try navigation.plan(axis: .machine, direction: .next)?.destination.machineID, "m2")
    }

    func testPaneAndViewportMemorySurviveGroupRoundTrips() throws {
        let all = try machines()
        let a = try group("a", ["m0", "m1"]), b = try group("b", ["m2", "m3"])
        var navigation = try TerminalNavigation(machines: all, group: a)
        try navigation.commit(XCTUnwrap(navigation.plan(paneID: all[0].panes[1].id)))
        let viewport = try TerminalNavigationViewport(topRow: 42, fontPointSize: 21)
        try navigation.remember(viewport, for: XCTUnwrap(navigation.viewWitness))
        try navigation.commit(XCTUnwrap(navigation.plan(group: b, selecting: "m3")))
        try navigation.commit(XCTUnwrap(navigation.plan(paneID: all[3].panes[1].id)))
        try navigation.commit(XCTUnwrap(navigation.plan(group: a, selecting: "m0")))
        XCTAssertEqual(navigation.selection?.paneID, all[0].panes[1].id)
        XCTAssertEqual(navigation.viewport(for: all[0].panes[1].id), viewport)
        try navigation.commit(XCTUnwrap(navigation.plan(group: b, selecting: "m3")))
        XCTAssertEqual(navigation.selection?.paneID, all[3].panes[1].id)
    }

    func testInactiveMachineRemovalExpiresItsMemoryWithoutChangingActiveRing() throws {
        var all = try machines()
        let oldPane = all[0].panes[1].id
        var navigation = try TerminalNavigation(machines: all)
        try navigation.commit(XCTUnwrap(navigation.plan(paneID: oldPane)))
        try navigation.remember(.init(topRow: 17), for: XCTUnwrap(navigation.viewWitness))
        try navigation.commit(XCTUnwrap(navigation.plan(group: group("b", ["m2", "m3"]), selecting: "m2")))
        all[0] = try .init(id: "m0", name: "replacement", panes: [])
        XCTAssertFalse(try navigation.replaceInventory(all))
        XCTAssertNil(navigation.viewport(for: oldPane))
        try navigation.commit(XCTUnwrap(navigation.plan(group: nil, selecting: "m0")))
        XCTAssertNil(navigation.selection?.paneID)
    }

    func testGroupChangeDoesNotResurrectADisappearedSelectedPane() throws {
        var all = try machines()
        var navigation = try TerminalNavigation(machines: all, group: group("a", ["m0", "m1"]))
        all[0] = try .init(id: "m0", name: "m0", panes: [all[0].panes[1]])
        XCTAssertTrue(try navigation.replaceInventory(all))
        XCTAssertNil(navigation.selection?.paneID)
        try navigation.commit(XCTUnwrap(navigation.plan(group: group("b", ["m0", "m2"]), selecting: "m0")))
        XCTAssertNil(navigation.selection?.paneID)
        XCTAssertNil(try navigation.plan(axis: .pane, direction: .next))
    }

    func testInvalidGroupPlanLeavesAllStateUnchanged() throws {
        let navigation = try TerminalNavigation(machines: machines(), group: group("a", ["m0", "m1"]))
        let oldSelection = navigation.selection, oldRing = navigation.machines
        XCTAssertThrowsError(try navigation.plan(group: group("b", ["m2"]), selecting: "m0"))
        XCTAssertThrowsError(try navigation.plan(group: group("b", ["missing"]), selecting: "missing"))
        XCTAssertEqual(navigation.selection, oldSelection); XCTAssertEqual(navigation.machines, oldRing)
    }

    func testInventoryRefreshInvalidatesPendingGroupChoiceEvenOutsideActiveRing() throws {
        var all = try machines()
        var navigation = try TerminalNavigation(machines: all, group: group("a", ["m0", "m1"]))
        let plan = try XCTUnwrap(navigation.plan(group: group("b", ["m2", "m3"]), selecting: "m2"))
        all[2] = try .init(id: "m2", name: "changed", panes: all[2].panes)
        try navigation.replaceInventory(all)
        XCTAssertThrowsError(try navigation.commit(plan))
        XCTAssertEqual(navigation.group?.id, "a")
    }

    func testTwentyThousandStepsRespectTenOverlappingGroupsAndPaneMemory() throws {
        let all = try machines(20)
        var navigation = try TerminalNavigation(machines: all)
        var expected: [String: TerminalNavigationPaneID] = [:]
        for iteration in 0..<2000 {
            let start = iteration % 20
            let ids = (0..<5).map { "m\((start + $0 * 3) % 20)" }
            let group = try self.group("g\(iteration % 10)", ids)
            if let plan = try navigation.plan(group: group, selecting: ids[0]) { try navigation.commit(plan) }
            for offset in 0..<5 {
                let machineID = ids[offset]
                XCTAssertEqual(navigation.selection?.machineID, machineID)
                let original = try XCTUnwrap(all.first { $0.id == machineID })
                XCTAssertEqual(navigation.selection?.paneID, expected[machineID] ?? original.panes[0].id)
                let pane = try XCTUnwrap(navigation.plan(axis: .pane, direction: .next))
                expected[machineID] = pane.destination.paneID
                try navigation.commit(pane)
                let next = try XCTUnwrap(navigation.plan(axis: .machine, direction: .next))
                XCTAssertEqual(next.destination.machineID, ids[(offset + 1) % ids.count])
                try navigation.commit(next)
            }
        }
    }

    @MainActor
    private final class Owner: TerminalNavigationFocusOwner {
        var currentSelection: TerminalNavigationSelection?
        var gate: CheckedContinuation<Void, Never>?
        var prepares = 0, publications = 0, abandonments = 0, seals = 0
        var delay = false, failPrepare = false, failPublish = false
        func prepare(_ destination: TerminalNavigationSelection) async throws -> TerminalNavigationPreparation {
            prepares += 1
            if delay { await withCheckedContinuation { gate = $0 } }
            if failPrepare { throw TerminalNavigationError.busy }
            return .init(destination: destination)
        }
        func publish(_ preparation: TerminalNavigationPreparation) throws {
            if failPublish { throw TerminalNavigationError.unavailable }
            currentSelection = preparation.destination; publications += 1
        }
        func abandon(_ preparation: TerminalNavigationPreparation) async { abandonments += 1 }
        func seal() { currentSelection = nil; seals += 1 }
        func joinSealed() async {}
        func release() { let old = gate; gate = nil; old?.resume() }
    }

    @MainActor
    private func waitForPreparation(_ owner: Owner) async throws {
        for _ in 0..<1000 { if owner.gate != nil { return }; await Task.yield() }
        XCTFail("the bounded preparation did not start")
        throw TerminalNavigationError.unavailable
    }

    @MainActor
    func testCoordinatorPublishesGroupAndFocusTogetherOnlyAfterHandoff() async throws {
        let owner = Owner(); owner.delay = true
        let coordinator = TerminalNavigationCoordinator(navigation: try .init(machines: machines(), group: group("a", ["m0", "m1"])), focusOwner: owner)
        coordinator.resume()
        let plan = try XCTUnwrap(coordinator.navigation.plan(group: group("b", ["m3", "m2"]), selecting: "m2"))
        let job = Task { try await coordinator.perform(plan) }
        try await waitForPreparation(owner)
        XCTAssertEqual(coordinator.navigation.group?.id, "a"); XCTAssertEqual(owner.publications, 0)
        owner.release(); try await job.value
        XCTAssertEqual(coordinator.navigation.group?.id, "b")
        XCTAssertEqual(owner.currentSelection, coordinator.navigation.selection)
        XCTAssertEqual(owner.publications, 1)
    }

    @MainActor
    func testPrepareAndPublishFailuresDoNotCommitTheNewGroup() async throws {
        for failure in 0..<2 {
            let owner = Owner(); owner.failPrepare = failure == 0; owner.failPublish = failure == 1
            let coordinator = TerminalNavigationCoordinator(navigation: try .init(machines: machines(), group: group("a", ["m0"])), focusOwner: owner)
            coordinator.resume()
            let plan = try XCTUnwrap(coordinator.navigation.plan(group: group("b", ["m2"]), selecting: "m2"))
            do { try await coordinator.perform(plan); XCTFail("expected refusal") } catch {}
            XCTAssertEqual(coordinator.navigation.group?.id, "a")
            XCTAssertEqual(coordinator.navigation.selection?.machineID, "m0")
            XCTAssertEqual(owner.publications, 0)
            XCTAssertEqual(owner.abandonments, failure)
        }
    }

    @MainActor
    func testSuspensionWhilePreparingCannotCommitAnotherGroupOnResume() async throws {
        let owner = Owner(); owner.delay = true
        let coordinator = TerminalNavigationCoordinator(navigation: try .init(machines: machines(), group: group("a", ["m0"])), focusOwner: owner)
        coordinator.resume()
        let plan = try XCTUnwrap(coordinator.navigation.plan(group: group("b", ["m2"]), selecting: "m2"))
        let job = Task { try await coordinator.perform(plan) }
        try await waitForPreparation(owner); coordinator.suspend(); coordinator.resume(); owner.release()
        do { try await job.value; XCTFail("expected cancellation") } catch {}
        await coordinator.joinSuspension()
        XCTAssertEqual(coordinator.navigation.group?.id, "a"); XCTAssertEqual(owner.publications, 0)
    }

    @MainActor
    func testOutsideGroupInventoryChangeCancelsAnInFlightGroupHandoff() async throws {
        var all = try machines()
        let owner = Owner(); owner.delay = true
        let coordinator = TerminalNavigationCoordinator(navigation: try .init(machines: all, group: group("a", ["m0"])), focusOwner: owner)
        coordinator.resume()
        let plan = try XCTUnwrap(coordinator.navigation.plan(group: group("b", ["m2"]), selecting: "m2"))
        let job = Task { try await coordinator.perform(plan) }
        try await waitForPreparation(owner)
        all[2] = try .init(id: "m2", name: "new registration", panes: [])
        try coordinator.replaceInventory(all)
        XCTAssertGreaterThan(owner.seals, 0)
        owner.release()
        do { try await job.value; XCTFail("expected invalidation") } catch {}
        await coordinator.joinSuspension()
        XCTAssertEqual(coordinator.navigation.group?.id, "a"); XCTAssertEqual(owner.publications, 0)
    }

    @MainActor
    func testOverlappingGroupRequestsAreRefusedWithoutReplacingOwnedWork() async throws {
        let owner = Owner(); owner.delay = true
        let coordinator = TerminalNavigationCoordinator(navigation: try .init(machines: machines()), focusOwner: owner)
        coordinator.resume()
        let first = try XCTUnwrap(coordinator.navigation.plan(group: group("b", ["m2"]), selecting: "m2"))
        let second = try XCTUnwrap(coordinator.navigation.plan(group: group("c", ["m3"]), selecting: "m3"))
        let job = Task { try await coordinator.perform(first) }
        try await waitForPreparation(owner)
        do { try await coordinator.perform(second); XCTFail("expected busy") }
        catch { XCTAssertEqual(error as? TerminalNavigationError, .busy) }
        owner.release(); try await job.value
        XCTAssertEqual(coordinator.navigation.group?.id, "b"); XCTAssertEqual(owner.prepares, 1)
    }
}
