import XCTest
@testable import FrankenTermKit

final class TerminalConnectionIntentTests: XCTestCase {
    private let machines: [TerminalConnectionIntent.Machine] = [
        .init(id: "build", autoConnect: true), .init(id: "gpu", autoConnect: true),
        .init(id: "manual", autoConnect: false), .init(id: "archive", autoConnect: true)
    ]
    private var available: Set<String> { Set(machines.map(\.id)) }
    private var work: TerminalConnectionIntent.Group {
        .init(id: "work", machineIDs: ["gpu", "build", "manual"], autoConnect: true)
    }

    func testWarmupUsesOnlyTheActiveGroupAndBothOptIns() throws {
        var intent = try TerminalConnectionIntent(machines: machines, selectedMachineID: "archive", group: work)
        XCTAssertEqual(Set(try intent.connectionOrder(available: available)), ["gpu", "build"])
        XCTAssertFalse(intent.shouldPrepare("manual"))
        XCTAssertFalse(intent.shouldPrepare("archive"))
        XCTAssertEqual(intent.opening("archive", creation: .onSelect), .none)

        try intent.setGroup(.init(id: "work", machineIDs: work.machineIDs, autoConnect: false))
        XCTAssertEqual(try intent.connectionOrder(available: available), [])
        try intent.approveConnection("archive")
        XCTAssertEqual(try intent.connectionOrder(available: available), ["archive"])
        try intent.setGroup(nil)
        XCTAssertEqual(try intent.connectionOrder(available: available), ["archive"])
    }

    func testConnectPreparesManualRouteButOnlyOpenTerminalAuthorizesItsChannel() throws {
        var intent = try TerminalConnectionIntent(machines: machines, selectedMachineID: "manual")
        try intent.approveConnection("manual")
        try intent.connectionStarted("manual")
        XCTAssertEqual(try intent.connectionOrder(available: available), ["manual"])
        // The actual route is checked after authentication: an on-select
        // preferred route cannot confer permission on a manual fallback.
        XCTAssertEqual(intent.opening("manual", creation: .onSelect), .onSelect)
        XCTAssertEqual(intent.opening("manual", creation: .manual), .none)
        try intent.requestTerminal("manual")
        XCTAssertEqual(intent.opening("manual", creation: .manual), .explicit)
        try intent.terminalOpened("manual")
        XCTAssertEqual(intent.opening("manual", creation: .manual), .none)
    }

    func testNavigationPromotesApprovedMachinesWithoutGrantingNewConnections() throws {
        var intent = try TerminalConnectionIntent(machines: machines, group: work)
        XCTAssertEqual(try intent.connectionOrder(available: available), ["gpu", "build"])
        try intent.select("build")
        XCTAssertEqual(try intent.connectionOrder(available: available), ["build", "gpu"])
        XCTAssertEqual(intent.opening("build", creation: .onSelect), .onSelect)
        try intent.select("manual")
        XCTAssertFalse(intent.shouldPrepare("manual"))
        XCTAssertEqual(intent.opening("manual", creation: .onSelect), .none)
        try intent.select("archive")
        XCTAssertEqual(intent.opening("archive", creation: .existingOnly), .none)
        XCTAssertThrowsError(try intent.connectionStarted("archive")) {
            XCTAssertEqual($0 as? TerminalConnectionIntentError, .notApproved)
        }
    }

    func testChangingGroupsChangesUnsentWorkAndKeepsActualOwnersReachable() throws {
        var intent = try TerminalConnectionIntent(machines: machines, selectedMachineID: "build", group: work)
        try intent.connectionStarted("build")
        try intent.approveConnection("manual")
        try intent.setGroup(.init(id: "archive", machineIDs: ["archive"], autoConnect: true))
        XCTAssertEqual(Set(try intent.connectionOrder(available: available)), ["archive", "manual"])
        XCTAssertFalse(intent.shouldPrepare("gpu"))
        XCTAssertTrue(intent.permitsReconnect("build"))
        XCTAssertFalse(intent.permitsReconnect("gpu"))
        // An already authenticated owner remains useful across group changes.
        XCTAssertEqual(intent.opening("build", creation: .onSelect), .onSelect)
    }

    func testUserDisconnectSurvivesGroupChangesAndNavigationUntilConnect() throws {
        var intent = try TerminalConnectionIntent(machines: machines, selectedMachineID: "build", group: work)
        try intent.connectionStarted("build")
        try intent.disconnect("build")
        try intent.select("gpu"); try intent.select("build")
        try intent.setGroup(nil); try intent.setGroup(work)
        XCTAssertEqual(try intent.connectionOrder(available: available), ["gpu"])
        XCTAssertEqual(intent.opening("build", creation: .onSelect), .none)
        XCTAssertFalse(intent.permitsReconnect("build"))
        try intent.approveConnection("build")
        XCTAssertTrue(intent.shouldPrepare("build"))
        XCTAssertEqual(intent.opening("build", creation: .manual), .none)
    }

    func testNormalExitAndLossRequireNewSessionEvenAfterReauthentication() throws {
        for creation in [TerminalSessionCreation.onSelect, .manual, .existingOnly] {
            var intent = try TerminalConnectionIntent(machines: machines, selectedMachineID: "build", group: work)
            try intent.connectionStarted("build")
            try intent.terminalOpened("build")
            try intent.terminalEnded("build")
            XCTAssertTrue(intent.permitsReconnect("build"))
            try intent.connectionStarted("build")
            try intent.select("gpu"); try intent.select("build")
            try intent.approveConnection("build")
            XCTAssertEqual(intent.opening("build", creation: creation), .none)
            XCTAssertTrue(intent.requiresNewSession.contains("build"))
            try intent.requestTerminal("build")
            XCTAssertEqual(intent.opening("build", creation: creation), .explicit)
            try intent.terminalOpened("build")
            XCTAssertFalse(intent.requiresNewSession.contains("build"))
        }
    }

    func testExplicitTerminalActionKeepsItsTargetAndDisconnectRetiresIt() throws {
        var intent = try TerminalConnectionIntent(machines: machines, selectedMachineID: "manual", group: work)
        try intent.requestTerminal("manual")
        try intent.select("gpu")
        try intent.setGroup(nil)
        XCTAssertEqual(intent.opening("manual", creation: .manual), .explicit)
        XCTAssertEqual(intent.opening("gpu", creation: .manual), .none)
        try intent.disconnect("manual")
        XCTAssertEqual(intent.opening("manual", creation: .manual), .none)
        XCTAssertEqual(try intent.connectionOrder(available: available), [])
    }

    func testInvalidReferencesFailAtomicallyAndUnavailableRoutesAreNotScheduled() throws {
        XCTAssertThrowsError(try TerminalConnectionIntent(machines: machines + [machines[0]]))
        XCTAssertThrowsError(try TerminalConnectionIntent(machines: machines, selectedMachineID: "absent"))
        XCTAssertThrowsError(try TerminalConnectionIntent(machines:
            (0..<129).map { .init(id: "machine-\($0)", autoConnect: true) }))
        var intent = try TerminalConnectionIntent(machines: machines, selectedMachineID: "build", group: work)
        for group in [TerminalConnectionIntent.Group(id: "foreign", machineIDs: ["absent"], autoConnect: true),
                      .init(id: "duplicate", machineIDs: ["build", "build"], autoConnect: true)] {
            XCTAssertThrowsError(try intent.setGroup(group))
            XCTAssertEqual(intent.group, work)
        }
        XCTAssertThrowsError(try intent.select("absent"))
        XCTAssertEqual(intent.selectedMachineID, "build")
        XCTAssertThrowsError(try intent.approveConnection("absent"))
        XCTAssertThrowsError(try intent.connectionOrder(available: ["absent"]))
        XCTAssertEqual(try intent.connectionOrder(available: ["gpu"]), ["gpu"])
    }
}
