import Foundation
import XCTest
@testable import FrankenTermKit

final class TerminalConnectionResidencyTests: XCTestCase {
    private typealias Policy = TerminalConnectionResidency
    private func owner(_ id: String, _ state: Policy.State = .automaticPreparation) -> Policy.Owner {
        .init(machineID: id, id: UUID(), state: state)
    }

    func testLimitAndInventoryAreValidatedBeforePlanning() throws {
        for limit in [Int.min, -1, 0, 17, Int.max] { XCTAssertThrowsError(try Policy(limit: limit)) }
        let policy = try Policy()
        XCTAssertThrowsError(try policy.decision(for: "a", priority: ["a", "a"], owners: []))
        XCTAssertThrowsError(try policy.decision(for: "a", priority: ["a", ""], owners: []))
        XCTAssertThrowsError(try policy.decision(for: "missing", priority: ["a"], owners: []))
        XCTAssertThrowsError(try policy.decision(for: "a", priority: ["a"], owners: [owner("b")]))
        XCTAssertThrowsError(try policy.decision(for: "a", priority: ["a"], owners: [], protecting: ["b"]))
        let id = UUID()
        XCTAssertThrowsError(try policy.decision(for: "a", priority: ["a", "b"], owners: [
            .init(machineID: "a", id: id, state: .terminal), .init(machineID: "b", id: id, state: .terminal)]))
        XCTAssertThrowsError(try policy.decision(for: "a", priority: ["a", "b"], owners: [owner("b"), owner("b")]))
        XCTAssertThrowsError(try policy.decision(for: "a", priority: ["a", String(repeating: "x", count: 129)], owners: []))
        XCTAssertThrowsError(try policy.decision(for: "m0", priority: (0..<129).map { "m\($0)" }, owners: []))
    }

    func testAvailableSlotDoesNotRequestUnnecessaryCleanup() throws {
        let policy = try Policy(limit: 3)
        XCTAssertEqual(try policy.decision(for: "a", priority: ["a", "b"], owners: []), .available)
        XCTAssertEqual(try policy.decision(for: "a", priority: ["a", "b"], owners: [owner("b")]), .available)
    }

    func testExistingOwnerWinsOverCapacityAndIsNeverReplaced() throws {
        let policy = try Policy(limit: 1)
        for state: Policy.State in [.preparing, .automaticPreparation, .protectedPreparation, .terminal, .settling] {
            XCTAssertEqual(try policy.decision(for: "a", priority: ["a", "b"], owners: [owner("a", state)]), .alreadyOwned)
        }
    }

    func testLeastImportantAutomaticPreparationYieldsBeforeNeighbours() throws {
        let policy = try Policy(limit: 3)
        let near = owner("near"), previous = owner("previous"), outside = owner("outside")
        XCTAssertEqual(try policy.decision(for: "wanted", priority: ["wanted", "near", "previous", "outside"],
                                          owners: [previous, outside, near]), .release(outside))
        XCTAssertEqual(try policy.decision(for: "wanted", priority: ["wanted", "outside", "previous", "near"],
                                          owners: [previous, outside, near]), .release(near))
    }

    func testSelectionAndExplicitConnectionsAreProtected() throws {
        let policy = try Policy(limit: 3)
        let selected = owner("selected"), manual = owner("manual", .protectedPreparation), idle = owner("idle")
        XCTAssertEqual(try policy.decision(for: "wanted", priority: ["wanted", "idle", "manual", "selected"],
            owners: [selected, manual, idle], protecting: ["selected"]), .release(idle))
    }

    func testLiveOpeningClosingAndExplicitOwnersNeverYield() throws {
        let policy = try Policy(limit: 4)
        let owners = [owner("a", .terminal), owner("b", .preparing), owner("c", .settling), owner("d", .protectedPreparation)]
        XCTAssertEqual(try policy.decision(for: "wanted", priority: ["wanted", "a", "b", "c", "d"], owners: owners), .blocked)
    }

    func testPlanningDoesNotReleaseOrReserveResources() throws {
        var policy = try Policy(limit: 1)
        let old = owner("old"), order = ["wanted", "old"]
        for _ in 0..<1000 { XCTAssertEqual(try policy.decision(for: "wanted", priority: order, owners: [old]), .release(old)) }
        try policy.willRelease(old)
        XCTAssertEqual(try policy.decision(for: "wanted", priority: order, owners: [old]), .blocked)
        XCTAssertEqual(try policy.decision(for: "wanted", priority: order, owners: [
            .init(machineID: "old", id: old.id, state: .settling)]), .blocked)
        XCTAssertEqual(try policy.decision(for: "wanted", priority: order, owners: []), .available)
    }

    func testReleasedPreparationDoesNotImmediatelyWarmAgain() throws {
        var policy = try Policy()
        let old = owner("old")
        try policy.willRelease(old)
        XCTAssertTrue(policy.wasReleased(machineID: "old", ownerID: old.id))
        XCTAssertFalse(policy.permitsPreparation("old", selected: "new", explicitlyRequested: false))
        XCTAssertTrue(policy.permitsPreparation("old", selected: "old", explicitlyRequested: false))
        XCTAssertTrue(policy.permitsPreparation("old", selected: "new", explicitlyRequested: true))
        XCTAssertTrue(policy.permitsPreparation("unreleased", selected: "new", explicitlyRequested: false))
    }

    func testStaleFailureCannotClearSuccessorsMarker() throws {
        var policy = try Policy()
        let old = owner("a"), successor = owner("a")
        try policy.willRelease(old); try policy.willRelease(successor)
        policy.releaseRefused(old)
        XCTAssertTrue(policy.wasReleased(machineID: "a", ownerID: successor.id))
        XCTAssertFalse(policy.wasReleased(machineID: "a", ownerID: old.id))
        policy.releaseRefused(successor)
        XCTAssertFalse(policy.wasReleased(machineID: "a", ownerID: successor.id))
    }

    func testSuccessorAdmissionClearsOnlyItsMachine() throws {
        var policy = try Policy()
        let a = owner("a"), b = owner("b")
        try policy.willRelease(a); try policy.willRelease(b)
        policy.started("a")
        XCTAssertFalse(policy.wasReleased(machineID: "a", ownerID: a.id))
        XCTAssertTrue(policy.wasReleased(machineID: "b", ownerID: b.id))
    }

    func testReleaseMarkersAreBoundedAndRequireAnEligibleOwner() throws {
        var policy = try Policy()
        for state: Policy.State in [.preparing, .protectedPreparation, .terminal, .settling] {
            XCTAssertThrowsError(try policy.willRelease(owner("a", state)))
        }
        XCTAssertThrowsError(try policy.willRelease(owner("")))
        XCTAssertThrowsError(try policy.willRelease(owner(String(repeating: "a", count: 129))))
        for i in 0..<128 { try policy.willRelease(owner("m\(i)")) }
        XCTAssertThrowsError(try policy.willRelease(owner("overflow")))
        for _ in 0..<1000 { try policy.willRelease(owner("m0")) }
        policy.started("m0")
        XCTAssertNoThrow(try policy.willRelease(owner("replacement")))
    }

    func testSelectionNeverOverridesIndependentConnectionIntent() throws {
        var policy = try Policy()
        var intent = try TerminalConnectionIntent(machines: [.init(id: "a", autoConnect: true)],
            selectedMachineID: "a", group: .init(id: "g", machineIDs: ["a"], autoConnect: true))
        try policy.willRelease(owner("a")); try intent.disconnect("a")
        XCTAssertTrue(policy.permitsPreparation("a", selected: "a", explicitlyRequested: false))
        XCTAssertFalse(intent.shouldPrepare("a"), "residency is not networking authorization")
        XCTAssertEqual(intent.opening("a", creation: .manual), .none)
        try intent.approveConnection("a")
        XCTAssertTrue(intent.shouldPrepare("a"))
        XCTAssertEqual(intent.opening("a", creation: .manual), .none, "re-signing in must not open a manual terminal")
    }

    func testTwentyMachineFleetCanVisitEveryMachineWithoutReleasingLiveTerminals() throws {
        var policy = try Policy(limit: 4)
        let machines = (0..<20).map { "m\($0)" }
        let terminal = owner("m0", .terminal), explicit = owner("m1", .protectedPreparation)
        var owners = [terminal, explicit, owner("m2"), owner("m3")]
        var releases = 0, reused = 0
        var visited: Set<String> = []
        for index in 0..<4000 {
            let target = machines[2 + index % 18]
            let priority = [target] + machines.filter { $0 != target }
            visited.insert(target)
            if owners.contains(where: { $0.machineID == target }) { reused += 1; continue }
            guard case let .release(victim) = try policy.decision(for: target, priority: priority, owners: owners) else {
                return XCTFail("unused preparations should make room for \(target)")
            }
            XCTAssertNotEqual(victim.id, terminal.id); XCTAssertNotEqual(victim.id, explicit.id)
            try policy.willRelease(victim)
            // Still full until cleanup is known to finish. No optimistic free.
            let closing = owners.map { $0.id == victim.id ? Policy.Owner(machineID: $0.machineID, id: $0.id, state: .settling) : $0 }
            XCTAssertEqual(closing.count, 4)
            owners.removeAll { $0.id == victim.id }
            XCTAssertEqual(try policy.decision(for: target, priority: priority, owners: owners), .available)
            policy.started(target); owners.append(owner(target)); releases += 1
            XCTAssertEqual(owners.count, 4)
            XCTAssertTrue(owners.contains(terminal)); XCTAssertTrue(owners.contains(explicit))
            XCTAssertFalse(policy.permitsPreparation(victim.machineID, selected: target, explicitlyRequested: false))
        }
        XCTAssertEqual(releases + reused, 4000)
        XCTAssertEqual(visited, Set(machines.dropFirst(2)))
        XCTAssertGreaterThan(releases, 0)
        XCTAssertGreaterThan(reused, 0, "cached targets should not cause unnecessary replacement")
    }
}
