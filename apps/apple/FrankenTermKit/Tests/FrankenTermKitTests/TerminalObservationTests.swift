import Foundation
import XCTest
@testable import FrankenTermKit

final class TerminalGridFitTests: XCTestCase {
    func testFittingUsesWholePhysicalCellsWithoutUpscalingOrChangingTheGrid() throws {
        let full = try TerminalGridFit(columns: 80, rows: 24, cellWidth: 10, cellHeight: 20,
            pixelWidth: 1000, pixelHeight: 600)
        XCTAssertEqual(full.scale, 1)
        XCTAssertTrue(full.contains(cellWidth: 10, cellHeight: 20))
        let wide = try TerminalGridFit(columns: 240, rows: 80, cellWidth: 11.1, cellHeight: 23.1,
            pixelWidth: 1179, pixelHeight: 600)
        XCTAssertEqual(wide.maximumCellWidth, 4)
        XCTAssertEqual(wide.maximumCellHeight, 7)
        XCTAssertEqual(wide.scale, 7.0 / 24.0)
        XCTAssertTrue(wide.contains(cellWidth: 4, cellHeight: 7))
        XCTAssertFalse(wide.contains(cellWidth: 4.01, cellHeight: 7))
        XCTAssertFalse(wide.contains(cellWidth: 4, cellHeight: 7.01))
    }

    func testRotationRefitsTheSameRemoteGridAndMaximumSupportedGridStaysBounded() throws {
        let portrait = try TerminalGridFit(columns: 256, rows: 128, cellWidth: 12, cellHeight: 24,
            pixelWidth: 1179, pixelHeight: 1800)
        let landscape = try TerminalGridFit(columns: 256, rows: 128, cellWidth: 12, cellHeight: 24,
            pixelWidth: 2400, pixelHeight: 900)
        XCTAssertEqual(portrait.scale, 4.0 / 12.0)
        XCTAssertEqual(landscape.scale, 7.0 / 24.0)
        for fit in [portrait, landscape] {
            XCTAssertGreaterThan(fit.scale, 0)
            XCTAssertLessThanOrEqual(fit.scale, 1)
        }
        XCTAssertLessThanOrEqual(256 * portrait.maximumCellWidth, 1179)
        XCTAssertLessThanOrEqual(128 * portrait.maximumCellHeight, 1800)
        XCTAssertLessThanOrEqual(256 * landscape.maximumCellWidth, 2400)
        XCTAssertLessThanOrEqual(128 * landscape.maximumCellHeight, 900)
    }

    func testInvalidAndUnrepresentableGeometryFailsWithoutOverflowOrNonfiniteScale() {
        for (columns, rows) in [(0, 24), (-1, 24), (Int.max, 24), (80, Int.min), (512, 512)] {
            XCTAssertThrowsError(try TerminalGridFit(columns: columns, rows: rows, cellWidth: 10, cellHeight: 20,
                pixelWidth: 1179, pixelHeight: 1800))
        }
        for value in [Double.nan, .infinity, -.infinity, 0, -1, 513] {
            XCTAssertThrowsError(try TerminalGridFit(columns: 80, rows: 24, cellWidth: value, cellHeight: 20,
                pixelWidth: 1179, pixelHeight: 1800))
            XCTAssertThrowsError(try TerminalGridFit(columns: 80, rows: 24, cellWidth: 10, cellHeight: value,
                pixelWidth: 1179, pixelHeight: 1800))
        }
        for (width, height) in [(0, 500), (500, 0), (Int.max, 500), (500, Int.min), (79, 24), (80, 23)] {
            XCTAssertThrowsError(try TerminalGridFit(columns: 80, rows: 24, cellWidth: 10, cellHeight: 20,
                pixelWidth: width, pixelHeight: height))
        }
    }
}

/// Distinguishes cleanup admission from ordinary current-focus admission. It
/// allows tests to deny one without conflating it with the other's lifetime.
private final class CleanupAdmissionTransport: TerminalByteTransport, @unchecked Sendable {
    let identity: TerminalConnectionIdentity
    let allowsCleanup: Bool
    init(allowsCleanup: Bool) throws {
        self.allowsCleanup = allowsCleanup
        identity = try .init(workspaceID: "observation", machineID: "machine", routeID: "route", routeRevision: 1,
            transportID: UUID(), channelID: UUID())
    }
    func validateInputAdmission() throws { throw TerminalInputError.stale }
    func validateCleanupAdmission() throws { if !allowsCleanup { throw TerminalInputError.unavailable } }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite { .accepted(bytes.count) }
    func close() async {}
}

final class TerminalObservationTests: XCTestCase, @unchecked Sendable {
    func testViewOnlyCanDisplayNavigateAndSearchButNeverAcquireInputPermission() throws {
        let control = TerminalMuxControl(accessMode: .viewOnly), routing = control.inputRouting
        let panes = (1...2).map {
            TerminalMuxPane(paneId: $0, tabId: $0, windowId: 1, tabTitle: "tab", title: "pane", workspace: "default",
                workingDir: nil, size: .init(rows: 80, cols: 240), isActive: $0 == 1, isZoomed: false)
        }
        try routing.replaceInventory([1, 2])
        let first = try routing.select(pane: 1)
        control.publish(panes: panes, shown: 1); control.setVisible(true); control.setMouseGrabbed(true)
        routing.didApplyOutput(first)
        XCTAssertTrue(routing.current.ready)
        XCTAssertEqual(try routing.capture(), first)
        XCTAssertThrowsError(try routing.validateAdmission(first))
        XCTAssertThrowsError(try routing.validateDelivery(first))
        XCTAssertThrowsError(try control.click(column: 2, row: 2, destination: first))
        XCTAssertThrowsError(try control.scroll(steps: -1, column: 2, row: 2, destination: first))
        XCTAssertFalse(routing.hasPendingPointers); XCTAssertNil(routing.takePointer())
        let plan = try XCTUnwrap(routing.navigation(forward: true))
        try control.request(plan)
        XCTAssertEqual(control.takeRequest(), 2)
        let second = try routing.select(pane: 2)
        control.publish(shown: 2); routing.didApplyOutput(second)
        XCTAssertTrue(routing.current.ready)
        XCTAssertThrowsError(try routing.validateAdmission(second))
        let query = TerminalMuxControl.SearchQuery(text: "needle", caseSensitive: false, regex: false)
        let search = control.search(query, panes: [1, 2])
        XCTAssertEqual(control.takeSearch()?.panes, [1, 2])
        control.deliver(search: search, pane: 1, results: .init(search: 1, matches: [], searchedRows: 100, error: nil))
        XCTAssertEqual(control.current.search?.pending, [2])
        control.readContext(pane: 2, row: 10)
        XCTAssertEqual(control.takeContext()?.pane, 2)
    }

    func testInteractiveDefaultPreservesOldVisitDeliveryWithoutReopeningNewAdmission() throws {
        let control = TerminalMuxControl(), routing = control.inputRouting
        XCTAssertEqual(control.accessMode, .interactive)
        XCTAssertTrue(control.accessMode.controlsSize)
        try routing.replaceInventory([1, 2])
        let old = try routing.select(pane: 1)
        routing.didApplyOutput(old)
        _ = try routing.select(pane: 2)
        XCTAssertThrowsError(try routing.validateAdmission(old))
        XCTAssertNoThrow(try routing.validateDelivery(old))
    }

    func testCleanupRestrictionsApplyBeforeAnyQueueOrOutcomeReservation() async throws {
        let transport = try CleanupAdmissionTransport(allowsCleanup: false), coordinator = try TerminalInputCoordinator()
        let source = try await coordinator.attach(transport)
        let stream = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(stream, to: source)
        let cleanup = Data([0x1b, 0x5b, 0x4f])
        do { _ = try await coordinator.cleanup(focus, encodedBytes: cleanup); XCTFail("Disallowed cleanup queued") }
        catch { XCTAssertEqual(error as? TerminalInputError, .unavailable) }
        do { _ = try await coordinator.changeFocus(focus, to: nil, cleanup: cleanup); XCTFail("Focus handoff queued disallowed cleanup") }
        catch { XCTAssertEqual(error as? TerminalInputError, .unavailable) }
        let queued = try await coordinator.suspend(focus, cleanup: cleanup)
        XCTAssertFalse(queued)
        let accounting = await coordinator.accounting()
        XCTAssertEqual(accounting.cleanupRecords, 0); XCTAssertEqual(accounting.cleanupReservationBytes, 0)
        XCTAssertEqual(accounting.retainedBytes, 0); XCTAssertEqual(accounting.retainedRecords, 0)
        let outcomes = try await coordinator.pendingOutcomes(source)
        XCTAssertTrue(outcomes.outcomes.isEmpty)
        try await coordinator.disconnect(source)
        try await coordinator.retire(source)
    }

    func testOldInteractiveCleanupDoesNotNeedCurrentInputAdmission() async throws {
        let transport = try CleanupAdmissionTransport(allowsCleanup: true), coordinator = try TerminalInputCoordinator()
        let source = try await coordinator.attach(transport)
        let stream = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(stream, to: source)
        do { _ = try await coordinator.submit(focus, encodedBytes: Data([1])); XCTFail("Stale current admission opened") }
        catch { XCTAssertEqual(error as? TerminalInputError, .stale) }
        let next = try await coordinator.changeFocus(focus, to: nil, cleanup: Data([2]))
        let drained = try await coordinator.drainOne(source)
        XCTAssertEqual(drained, .progressed(1))
        let outcomes = try await coordinator.pendingOutcomes(source)
        XCTAssertEqual(outcomes.outcomes.count, 1)
        try await coordinator.acknowledgeOutcomes(outcomes)
        _ = try await coordinator.suspend(next)
        try await coordinator.disconnect(source)
        try await coordinator.retire(source)
    }
}
