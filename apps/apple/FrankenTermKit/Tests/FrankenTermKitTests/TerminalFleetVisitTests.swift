import Foundation
import XCTest
@testable import FrankenTermKit

final class TerminalFleetVisitTests: XCTestCase, @unchecked Sendable {
    private func routing() throws -> TerminalMuxInputRouting {
        let value = TerminalMuxInputRouting(permitsInput: false)
        try value.replaceInventory([4, 9, 2])
        _ = try value.select(pane: 4)
        return value
    }

    func testKnownAndNotYetConnectedMachinesKeepTheirOriginalSessionWitness() throws {
        let session = UUID()
        let known = try TerminalFleetVisit(machineID: "alpha", sessionID: session)
        XCTAssertNoThrow(try known.validate(sessionID: session))
        XCTAssertThrowsError(try known.validate(sessionID: UUID()))
        XCTAssertThrowsError(try known.validate(sessionID: nil))
        let unavailable = try TerminalFleetVisit(machineID: "beta", sessionID: nil)
        XCTAssertNoThrow(try unavailable.validate(sessionID: nil))
        XCTAssertThrowsError(try unavailable.validate(sessionID: session))
    }

    func testPaneShortcutValidatesTheOriginalRouteAndGrantsNoInputReadiness() throws {
        let routing = try routing(), session = UUID()
        let target = try TerminalFleetVisit(machineID: "alpha", sessionID: session,
                                            pane: routing.navigation(pane: 9))
        let before = routing.current
        XCTAssertNoThrow(try target.validate(sessionID: session, routing: routing))
        XCTAssertEqual(routing.current, before)
        XCTAssertFalse(routing.current.ready)
        XCTAssertFalse(routing.permitsInput)
        XCTAssertThrowsError(try target.validate(sessionID: UUID(), routing: routing))
        XCTAssertThrowsError(try target.validate(sessionID: session))
        XCTAssertThrowsError(try target.validate(sessionID: session, routing: self.routing()),
                             "a new connection with the same pane numbers is not the captured target")
    }

    func testRemovedAndRecreatedPaneNumberCannotReviveTheCapturedVisit() throws {
        let routing = try routing(), session = UUID()
        let target = try TerminalFleetVisit(machineID: "alpha", sessionID: session,
                                            pane: routing.navigation(pane: 9))
        try routing.replaceInventory([4, 2])
        try routing.replaceInventory([4, 9, 2])
        XCTAssertThrowsError(try target.validate(sessionID: session, routing: routing))
        let fresh = try TerminalFleetVisit(machineID: "alpha", sessionID: session,
                                           pane: routing.navigation(pane: 9))
        XCTAssertNoThrow(try fresh.validate(sessionID: session, routing: routing))
    }

    func testSourceVisitABAAndReorderedInventoryRetireAnEarlierShortcut() throws {
        for reorder in [false, true] {
            let routing = try routing(), session = UUID()
            let target = try TerminalFleetVisit(machineID: "alpha", sessionID: session,
                                                pane: routing.navigation(pane: 9))
            if reorder { try routing.replaceInventory([4, 2, 9]) }
            else { _ = try routing.select(pane: 2); _ = try routing.select(pane: 4) }
            XCTAssertThrowsError(try target.validate(sessionID: session, routing: routing))
        }
    }

    func testRetiredConnectionAndClosedPaneDoNotResolveToItsNeighbour() throws {
        let routing = try routing(), session = UUID()
        let target = try TerminalFleetVisit(machineID: "alpha", sessionID: session,
                                            pane: routing.navigation(pane: 9))
        try routing.replaceInventory([4, 2])
        XCTAssertThrowsError(try target.validate(sessionID: session, routing: routing))
        XCTAssertEqual(routing.current.destination?.paneID, 4)
        routing.retire()
        XCTAssertThrowsError(try target.validate(sessionID: session, routing: routing))
    }

    func testInvalidShortcutMetadataIsRefusedBeforeRetention() throws {
        let pane = try routing().navigation(pane: 9)
        XCTAssertThrowsError(try TerminalFleetVisit(machineID: "alpha", sessionID: nil, pane: pane))
        XCTAssertThrowsError(try TerminalFleetVisit(machineID: "", sessionID: nil))
        XCTAssertThrowsError(try TerminalFleetVisit(machineID: String(repeating: "é", count: 65), sessionID: nil))
        XCTAssertNoThrow(try TerminalFleetVisit(machineID: String(repeating: "é", count: 64), sessionID: nil))
    }
}
