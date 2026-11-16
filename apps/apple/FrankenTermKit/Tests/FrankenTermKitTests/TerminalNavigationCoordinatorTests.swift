import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
private final class ControlledNavigationFocus: TerminalNavigationFocusOwner {
    var currentSelection: TerminalNavigationSelection?
    var block = false, failBeforeBarrier = false, failPublication = false, wrongDestination = false
    var published: [TerminalNavigationPreparation] = [], abandoned: [TerminalNavigationPreparation] = []
    var seals = 0, joins = 0, preparing = false
    private var pending: TerminalNavigationPreparation?
    private var waiter: CheckedContinuation<Void, Never>?
    private var entered: CheckedContinuation<Void, Never>?
    func prepare(_ destination: TerminalNavigationSelection) async throws -> TerminalNavigationPreparation {
        if failBeforeBarrier { throw TerminalNavigationError.unavailable }
        currentSelection = nil
        let result = TerminalNavigationPreparation(destination: wrongDestination
            ? .init(machineID: "wrong", paneID: nil) : destination)
        pending = result; preparing = true
        entered?.resume(); entered = nil
        if block { await withCheckedContinuation { waiter = $0 } }
        preparing = false
        return result // Deliberately returns after cancellation to test the caller's fence.
    }
    func waitForPreparation() async {
        if preparing { return }
        await withCheckedContinuation { entered = $0 }
    }
    func release() { let waiter = waiter; self.waiter = nil; waiter?.resume() }
    func publish(_ preparation: TerminalNavigationPreparation) throws {
        guard !failPublication, preparation == pending else { throw TerminalNavigationError.stale }
        published.append(preparation); currentSelection = preparation.destination; pending = nil
    }
    func abandon(_ preparation: TerminalNavigationPreparation) async {
        abandoned.append(preparation)
        if pending == preparation { pending = nil }
    }
    func seal() { seals += 1; currentSelection = nil }
    func joinSealed() async { joins += 1 }
}

@MainActor
final class TerminalNavigationCoordinatorTests: XCTestCase {
    private func setup() throws -> (TerminalNavigationCoordinator, ControlledNavigationFocus) {
        let machines = try ["A", "B", "C"].map {
            try TerminalNavigationMachine(id: $0, name: $0, panes: [
                TerminalNavigationPane(id: .init(id: UUID(), incarnation: UUID()), title: "pane")
            ])
        }
        let owner = ControlledNavigationFocus()
        let coordinator = TerminalNavigationCoordinator(navigation: try .init(machines: machines), focusOwner: owner)
        return (coordinator, owner)
    }

    func testExplicitActivationAndSinglePublication() async throws {
        let (coordinator, owner) = try setup()
        XCTAssertEqual(coordinator.phase, .suspended)
        do { try await coordinator.activateSelection(); XCTFail("suspended activation") } catch { }
        coordinator.resume()
        XCTAssertEqual(coordinator.phase, .inactive)
        try await coordinator.activateSelection()
        XCTAssertEqual(coordinator.phase, .ready)
        XCTAssertEqual(owner.currentSelection, coordinator.navigation.selection)
        let plan = try XCTUnwrap(coordinator.navigation.plan(machineID: "B"))
        try await coordinator.perform(plan)
        XCTAssertEqual(owner.published.count, 2)
        XCTAssertEqual(coordinator.navigation.selection?.machineID, "B")
        do { try await coordinator.perform(plan); XCTFail("replayed plan") } catch { }
        XCTAssertEqual(owner.published.count, 2)
    }

    func testOutstandingHandoffIsBoundedAndDoesNotPublishPreview() async throws {
        let (coordinator, owner) = try setup(); coordinator.resume()
        try await coordinator.activateSelection()
        owner.block = true
        let plan = try XCTUnwrap(coordinator.navigation.plan(machineID: "B"))
        let job = Task { try await coordinator.perform(plan) }
        await owner.waitForPreparation()
        XCTAssertEqual(coordinator.phase, .switching)
        XCTAssertEqual(coordinator.navigation.selection?.machineID, "A")
        do { try await coordinator.perform(plan); XCTFail("concurrent handoff") }
        catch { XCTAssertEqual(error as? TerminalNavigationError, .busy) }
        owner.release(); try await job.value
        XCTAssertEqual(coordinator.phase, .ready)
        XCTAssertEqual(coordinator.navigation.selection?.machineID, "B")
    }

    func testInventoryChangeAbandonsLateFocusInsteadOfRetargeting() async throws {
        let (coordinator, owner) = try setup(); coordinator.resume()
        try await coordinator.activateSelection(); owner.block = true
        let plan = try XCTUnwrap(coordinator.navigation.plan(machineID: "B"))
        let job = Task { try await coordinator.perform(plan) }
        await owner.waitForPreparation()
        let old = coordinator.navigation.machines
        try coordinator.replaceInventory([old[0], old[2], old[1]])
        XCTAssertNil(owner.currentSelection)
        owner.release()
        do { try await job.value; XCTFail("stale topology published") } catch { }
        await coordinator.joinSuspension()
        XCTAssertEqual(coordinator.navigation.selection?.machineID, "A")
        XCTAssertEqual(owner.published.count, 1)
        XCTAssertEqual(owner.abandoned.count, 1)
        XCTAssertEqual(owner.abandoned.first?.destination.machineID, "B")
        XCTAssertEqual(coordinator.phase, .inactive)
    }

    func testSuspendResumeCannotReviveDelayedPreparation() async throws {
        let (coordinator, owner) = try setup(); coordinator.resume()
        try await coordinator.activateSelection(); owner.block = true
        let plan = try XCTUnwrap(coordinator.navigation.plan(machineID: "B"))
        let job = Task { try await coordinator.perform(plan) }
        await owner.waitForPreparation()
        coordinator.suspend(); coordinator.resume()
        owner.release()
        do { try await job.value; XCTFail("old foreground revived") } catch { }
        await coordinator.joinSuspension()
        XCTAssertEqual(owner.published.count, 1)
        XCTAssertEqual(coordinator.phase, .inactive)
        owner.block = false
        try await coordinator.activateSelection()
        XCTAssertEqual(coordinator.phase, .ready)
        XCTAssertEqual(owner.currentSelection?.machineID, "A")
    }

    func testCallerCancellationOwnsLatePreparedFocus() async throws {
        let (coordinator, owner) = try setup(); coordinator.resume()
        try await coordinator.activateSelection(); owner.block = true
        let plan = try XCTUnwrap(coordinator.navigation.plan(machineID: "B"))
        let job = Task { try await coordinator.perform(plan) }
        await owner.waitForPreparation()
        job.cancel(); owner.release()
        do { try await job.value; XCTFail("cancelled focus published") } catch { }
        XCTAssertEqual(owner.published.count, 1)
        XCTAssertEqual(owner.abandoned.count, 1)
        XCTAssertNil(owner.currentSelection)
        XCTAssertEqual(coordinator.phase, .inactive)
    }

    func testPreBarrierFailurePreservesOriginalKeyboardAndSelection() async throws {
        let (coordinator, owner) = try setup(); coordinator.resume()
        try await coordinator.activateSelection(); owner.failBeforeBarrier = true
        let old = coordinator.navigation.selection
        do { try await coordinator.perform(XCTUnwrap(coordinator.navigation.plan(machineID: "B"))); XCTFail() } catch { }
        XCTAssertEqual(coordinator.navigation.selection, old)
        XCTAssertEqual(owner.currentSelection, old)
        XCTAssertEqual(owner.seals, 0)
        XCTAssertEqual(coordinator.phase, .ready)
    }

    func testPublicationFailureAbandonsExactPreparedFocus() async throws {
        let (coordinator, owner) = try setup(); coordinator.resume()
        try await coordinator.activateSelection(); owner.failPublication = true
        do { try await coordinator.perform(XCTUnwrap(coordinator.navigation.plan(machineID: "B"))); XCTFail() } catch { }
        XCTAssertEqual(coordinator.navigation.selection?.machineID, "A")
        XCTAssertEqual(owner.abandoned.count, 1)
        XCTAssertEqual(owner.abandoned[0].destination.machineID, "B")
        XCTAssertEqual(coordinator.phase, .inactive)
    }

    func testWrongPreparedDestinationIsNeverPublished() async throws {
        let (coordinator, owner) = try setup(); coordinator.resume()
        owner.wrongDestination = true
        do { try await coordinator.activateSelection(); XCTFail() } catch { }
        XCTAssertTrue(owner.published.isEmpty)
        XCTAssertEqual(owner.abandoned.count, 1)
    }

    func testRemovalSealsInputAndRequiresExplicitSurvivingSelection() async throws {
        let (coordinator, owner) = try setup(); coordinator.resume()
        try await coordinator.activateSelection()
        try coordinator.replaceInventory(Array(coordinator.navigation.machines.dropFirst()))
        XCTAssertNil(owner.currentSelection)
        XCTAssertNil(coordinator.navigation.selection)
        await coordinator.joinSuspension()
        do { try await coordinator.activateSelection(); XCTFail() } catch { }
        try await coordinator.perform(XCTUnwrap(coordinator.navigation.plan(machineID: "B")))
        XCTAssertEqual(coordinator.navigation.selection?.machineID, "B")
    }

    func testInvalidInventoryDoesNotCancelValidHandoff() async throws {
        let (coordinator, owner) = try setup(); coordinator.resume()
        try await coordinator.activateSelection(); owner.block = true
        let job = Task { try await coordinator.perform(XCTUnwrap(coordinator.navigation.plan(machineID: "B"))) }
        await owner.waitForPreparation()
        let first = coordinator.navigation.machines[0]
        XCTAssertThrowsError(try coordinator.replaceInventory([first, first]))
        XCTAssertEqual(owner.seals, 0)
        owner.release(); try await job.value
        XCTAssertEqual(coordinator.navigation.selection?.machineID, "B")
    }
}
