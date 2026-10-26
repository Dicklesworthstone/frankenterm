import Foundation
import XCTest
@testable import FrankenTermKit

// The case has no mutable state; each test owns its independent index/routing.
final class TerminalFleetOverviewTests: XCTestCase, @unchecked Sendable {
    private func machine(_ id: String = "alpha", name: String = "Alpha Production",
                         ids: [Int] = [10, 20, 30]) throws
        -> (TerminalFleetOverview.Machine, TerminalMuxInputRouting, UUID) {
        let routing = TerminalMuxInputRouting(), session = UUID()
        try routing.replaceInventory(ids)
        if let first = ids.first { routing.didApplyOutput(try routing.select(pane: first)) }
        let panes = try ids.enumerated().map { offset, id in
            TerminalFleetOverview.Pane(plan: try routing.navigation(pane: id), title: "Build \(offset)",
                tabTitle: "Agents", workingDirectory: "/srv/projects/project-\(offset)")
        }
        return (.init(id: id, name: name, endpoint: "\(id).example:22", status: .live,
                      sessionID: session, panes: panes, selectedPaneID: ids.first), routing, session)
    }
    private func paneChoice(_ catalog: TerminalFleetOverview, _ id: Int) throws -> TerminalFleetOverview.Choice {
        try XCTUnwrap(catalog.search(.init(scope: .panes)).rows.first { $0.paneID == id }?.id)
    }

    func testEmptyInventoryAndEmptyQuery() throws {
        let catalog = try TerminalFleetOverview(machines: [])
        let result = try catalog.search()
        XCTAssertEqual(catalog.machineCount, 0); XCTAssertEqual(catalog.paneCount, 0)
        XCTAssertEqual(result.matchingCount, 0); XCTAssertTrue(result.rows.isEmpty); XCTAssertFalse(result.limited)
    }

    func testOrderRemainsImportedMachineThenRemotePaneOrder() throws {
        let a = try machine("zeta", ids: [9, 3, 12]), b = try machine("beta", ids: [40, 2])
        let catalog = try TerminalFleetOverview(machines: [a.0, b.0], selectedMachineID: "beta")
        let rows = try catalog.search().rows
        XCTAssertEqual(rows.map(\.machineID), ["zeta", "zeta", "zeta", "zeta", "beta", "beta", "beta"])
        XCTAssertEqual(rows.map(\.paneID), [nil, 9, 3, 12, nil, 40, 2])
        XCTAssertEqual(rows.filter(\.selected).map(\.paneID), [nil, 40])
        XCTAssertEqual(catalog.machineCount, 2); XCTAssertEqual(catalog.paneCount, 5)
    }

    func testTermsCanSpanMachineHostPaneTabAndWorkingDirectory() throws {
        let (value, _, _) = try machine()
        let catalog = try TerminalFleetOverview(machines: [value])
        let result = try catalog.search(.init(text: "PRODUCTION alpha.example agents project-1"))
        XCTAssertEqual(result.rows.map(\.paneID), [20])
        XCTAssertEqual(try catalog.search(.init(text: "project-1 missing")).matchingCount, 0)
        XCTAssertEqual(try catalog.search(.init(text: "  \tAlpha\n ")).matchingCount, 4)
    }

    func testUnicodeSearchIsCaseAndDiacriticInsensitiveWithoutChangingLabels() throws {
        let (value, _, _) = try machine("unicode", name: "Café ΔΟΚΙΜΉ 東京")
        let catalog = try TerminalFleetOverview(machines: [value])
        let result = try catalog.search(.init(text: "CAFE δοκιμη 東京", scope: .machines))
        XCTAssertEqual(result.rows.count, 1)
        XCTAssertEqual(result.rows.first?.title, "Café ΔΟΚΙΜΉ 東京")
    }

    func testSearchTreatsRegularExpressionAndMarkupCharactersLiterally() throws {
        let value = TerminalFleetOverview.Machine(id: "literal", name: "prod[1] .* <tag>", endpoint: "host", status: .disconnected)
        let catalog = try TerminalFleetOverview(machines: [value])
        XCTAssertEqual(try catalog.search(.init(text: ".*")).matchingCount, 1)
        XCTAssertEqual(try catalog.search(.init(text: "^prod")).matchingCount, 0)
        XCTAssertEqual(try catalog.search(.init(text: "<tag>")).rows.first?.title, value.name)
    }

    func testScopeAndStatusFiltersDoNotSilentlySkipDisconnectedMachines() throws {
        let live = try machine().0
        let rest = [TerminalFleetOverview.Status.ready, .connecting, .attention, .disconnected, .unavailable].map {
            TerminalFleetOverview.Machine(id: $0.rawValue, name: $0.label, endpoint: "host", status: $0)
        }
        let catalog = try TerminalFleetOverview(machines: [live] + rest)
        XCTAssertEqual(try catalog.search(.init(scope: .machines)).matchingCount, 6)
        XCTAssertEqual(try catalog.search(.init(scope: .panes)).matchingCount, 3)
        XCTAssertEqual(try catalog.search(.init(scope: .machines, filter: .connected)).rows.map(\.machineID), ["alpha", "ready"])
        XCTAssertEqual(try catalog.search(.init(filter: .attention)).rows.map(\.machineID), ["attention"])
        XCTAssertEqual(try catalog.search(.init(scope: .panes, filter: .attention)).matchingCount, 0)
    }

    func testNewOutputFilterKeepsTheMarkedMachinesAndPanesInOrder() throws {
        let routing = TerminalMuxInputRouting(), session = UUID()
        try routing.replaceInventory([10, 20, 30])
        routing.didApplyOutput(try routing.select(pane: 10))
        let panes = try [10, 20, 30].map { id in
            TerminalFleetOverview.Pane(plan: try routing.navigation(pane: id), title: "Pane \(id)", newOutput: id != 20)
        }
        let mux = TerminalFleetOverview.Machine(id: "mux", name: "Mux", endpoint: "mux:22", status: .live,
                                                sessionID: session, panes: panes, selectedPaneID: 10, newOutput: true)
        let direct = TerminalFleetOverview.Machine(id: "direct", name: "Direct", endpoint: "direct:22", status: .live,
                                                   newOutput: true)
        let quiet = TerminalFleetOverview.Machine(id: "quiet", name: "Quiet", endpoint: "quiet:22", status: .live)
        let catalog = try TerminalFleetOverview(machines: [quiet, mux, direct])
        let marked = try catalog.search(.init(filter: .newOutput)).rows
        XCTAssertEqual(marked.map(\.machineID), ["mux", "mux", "mux", "direct"])
        XCTAssertEqual(marked.map(\.paneID), [nil, 10, 30, nil])
        XCTAssertTrue(marked.allSatisfy(\.newOutput))
        XCTAssertEqual(try catalog.search(.init(scope: .panes, filter: .newOutput)).rows.map(\.paneID), [10, 30])
        XCTAssertEqual(try catalog.search(.init(text: "direct", filter: .newOutput)).rows.map(\.machineID), ["direct"])
        let all = try catalog.search().rows
        XCTAssertEqual(all.filter(\.newOutput).count, 4, "every row carries its own mark")
        XCTAssertFalse(try XCTUnwrap(all.first { $0.machineID == "quiet" }).newOutput)
    }

    func testResultsLimitReportsTheFullMatchCountAndKeepsStablePrefix() throws {
        let catalog = try TerminalFleetOverview(machines: [try machine().0])
        let result = try catalog.search(limit: 2)
        XCTAssertEqual(result.matchingCount, 4); XCTAssertTrue(result.limited)
        XCTAssertEqual(result.rows.map(\.paneID), [nil, 10])
        XCTAssertFalse(try catalog.search(limit: 4).limited)
        for limit in [Int.min, -1, 0, 513, Int.max] { XCTAssertThrowsError(try catalog.search(limit: limit)) }
    }

    func testQueryLimitsRejectBeforeSearching() throws {
        let catalog = try TerminalFleetOverview(machines: [try machine().0])
        for query in [String(repeating: "x", count: 513), String(repeating: "🙂", count: 129),
                      Array(repeating: "a", count: 17).joined(separator: " ")] {
            XCTAssertThrowsError(try catalog.search(.init(text: query))) {
                XCTAssertEqual($0 as? TerminalFleetOverview.Failure, .invalidQuery)
            }
        }
        XCTAssertNoThrow(try catalog.search(.init(text: String(repeating: "x", count: 512))))
    }

    func testChoiceCannotBeResolvedByAReplacementSnapshot() throws {
        let value = try machine().0
        let original = try TerminalFleetOverview(machines: [value]), replacement = try TerminalFleetOverview(machines: [value])
        let choice = try XCTUnwrap(original.search().rows.first?.id)
        XCTAssertEqual(try original.resolve(choice), .machine("alpha"))
        XCTAssertThrowsError(try replacement.resolve(choice)) { XCTAssertEqual($0 as? TerminalFleetOverview.Failure, .stale) }
    }

    func testPaneChoiceRequiresExactSessionAndRoutingOwner() throws {
        let (value, routing, session) = try machine()
        let catalog = try TerminalFleetOverview(machines: [value]), choice = try paneChoice(catalog, 20)
        XCTAssertEqual(try catalog.validatePane(choice, sessionID: session, routing: routing).paneID, 20)
        XCTAssertThrowsError(try catalog.validatePane(choice, sessionID: UUID(), routing: routing))
        let other = try machine()
        XCTAssertThrowsError(try catalog.validatePane(choice, sessionID: session, routing: other.1))
        let machineChoice = try XCTUnwrap(catalog.search(.init(scope: .machines)).rows.first?.id)
        XCTAssertThrowsError(try catalog.validatePane(machineChoice, sessionID: session, routing: routing))
    }

    func testRemovalAndNumericReuseInvalidateFrozenPaneChoice() throws {
        let (value, routing, session) = try machine()
        let catalog = try TerminalFleetOverview(machines: [value]), choice = try paneChoice(catalog, 20)
        routing.remove(pane: 20); try routing.replaceInventory([10, 20, 30])
        XCTAssertThrowsError(try catalog.validatePane(choice, sessionID: session, routing: routing))
    }

    func testSelectionRoundTripOrReorderingCannotReviveAChoice() throws {
        let (value, routing, session) = try machine()
        let catalog = try TerminalFleetOverview(machines: [value]), choice = try paneChoice(catalog, 20)
        try routing.select(pane: 30); try routing.select(pane: 10)
        XCTAssertThrowsError(try catalog.validatePane(choice, sessionID: session, routing: routing))
        let reordered = try machine()
        let other = try TerminalFleetOverview(machines: [reordered.0]), second = try paneChoice(other, 20)
        try reordered.1.replaceInventory([30, 20, 10])
        XCTAssertThrowsError(try other.validatePane(second, sessionID: reordered.2, routing: reordered.1))
    }

    func testUnchangedInventoryAndReadinessDoNotInvalidateChoice() throws {
        let (value, routing, session) = try machine()
        let catalog = try TerminalFleetOverview(machines: [value]), choice = try paneChoice(catalog, 20)
        try routing.replaceInventory([10, 20, 30])
        routing.didApplyOutput(try routing.capture())
        XCTAssertNoThrow(try catalog.validatePane(choice, sessionID: session, routing: routing))
        routing.retire()
        XCTAssertThrowsError(try catalog.validatePane(choice, sessionID: session, routing: routing))
    }

    func testInvalidInventoryIsRejectedRatherThanAliasingRows() throws {
        let value = try machine().0
        XCTAssertThrowsError(try TerminalFleetOverview(machines: [value, value]))
        XCTAssertThrowsError(try TerminalFleetOverview(machines: [.init(id: "", name: "", endpoint: "", status: .ready)]))
        for status in [TerminalFleetOverview.Status.ready, .disconnected, .unavailable] {
            XCTAssertThrowsError(try TerminalFleetOverview(machines: [.init(id: "bad", name: "Bad", endpoint: "host",
                status: status, sessionID: UUID(), panes: value.panes)]))
        }
        XCTAssertThrowsError(try TerminalFleetOverview(machines: [.init(id: "bad", name: "Bad", endpoint: "host",
            status: .live, panes: value.panes)]))
        XCTAssertThrowsError(try TerminalFleetOverview(machines: [.init(id: "bad", name: "Bad", endpoint: "host",
            status: .live, sessionID: UUID(), panes: [value.panes[0], value.panes[0]])]))
    }

    func testMetadataAndTotalBudgetsAreEnforced() throws {
        let many = (0..<129).map { TerminalFleetOverview.Machine(id: "m\($0)", name: "Host", endpoint: "host", status: .ready) }
        XCTAssertThrowsError(try TerminalFleetOverview(machines: many))
        XCTAssertThrowsError(try TerminalFleetOverview(machines: [.init(id: "m", name: String(repeating: "a", count: 1025),
            endpoint: "host", status: .ready)]))
        let (value, routing, session) = try machine(ids: Array(0..<600))
        let oversized = try (0..<600).map { id in TerminalFleetOverview.Pane(plan: try routing.navigation(pane: id),
            title: "Pane", workingDirectory: String(repeating: "p", count: 4096)) }
        XCTAssertThrowsError(try TerminalFleetOverview(machines: [.init(id: value.id, name: value.name, endpoint: value.endpoint,
            status: .live, sessionID: session, panes: oversized)]))
    }

    func testTwentyMachineTwoHundredPaneJourneyFindsEveryExactDestination() throws {
        var values: [TerminalFleetOverview.Machine] = []
        var owners: [TerminalMuxInputRouting] = [], sessions: [UUID] = []
        for host in 0..<20 {
            let (value, owner, session) = try machine("machine-\(host)", name: "Machine \(host)", ids: Array(100..<110))
            values.append(value); owners.append(owner); sessions.append(session)
        }
        let catalog = try TerminalFleetOverview(machines: values)
        XCTAssertEqual(catalog.machineCount, 20); XCTAssertEqual(catalog.paneCount, 200)
        XCTAssertEqual(try catalog.search().matchingCount, 220)
        for host in 0..<20 {
            for pane in 0..<10 {
                let result = try catalog.search(.init(text: "machine-\(host).example project-\(pane)", scope: .panes))
                XCTAssertEqual(result.matchingCount, 1)
                let row = try XCTUnwrap(result.rows.first)
                XCTAssertEqual(row.machineID, "machine-\(host)")
                XCTAssertEqual(try catalog.validatePane(row.id, sessionID: sessions[host], routing: owners[host]).paneID, 100 + pane)
            }
        }
    }

    func testMaximumPaneInventoryHasBoundedResultsAndSearchesPastVisiblePrefix() throws {
        let value = try machine(ids: Array(0..<4096)).0
        let catalog = try TerminalFleetOverview(machines: [value])
        let all = try catalog.search(limit: 512)
        XCTAssertEqual(all.matchingCount, 4097); XCTAssertEqual(all.rows.count, 512); XCTAssertTrue(all.limited)
        let last = try catalog.search(.init(text: "project-4095", scope: .panes))
        XCTAssertEqual(last.rows.map(\.paneID), [4095])
        XCTAssertThrowsError(try TerminalFleetOverview(machines: [value, try machine("overflow", ids: [5000]).0]))
    }

    func testParallelSearchDoesNotMutateSnapshotOrChoices() async throws {
        let catalog = try TerminalFleetOverview(machines: [try machine().0])
        let choice = try paneChoice(catalog, 20)
        try await withThrowingTaskGroup(of: Void.self) { group in
            for _ in 0..<200 {
                group.addTask {
                    let result = try catalog.search(.init(text: "project-1", scope: .panes))
                    XCTAssertEqual(result.rows.map(\.id), [choice])
                    XCTAssertEqual(result.matchingCount, 1)
                }
            }
            try await group.waitForAll()
        }
    }

    func testCancelledSearchRefusesToPublishResults() async throws {
        let catalog = try TerminalFleetOverview(machines: [try machine().0])
        let cancelled = await Task.detached {
            withUnsafeCurrentTask { $0?.cancel() }
            do { _ = try catalog.search(); return false }
            catch is CancellationError { return true }
            catch { return false }
        }.value
        XCTAssertTrue(cancelled)
    }
}
