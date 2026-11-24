import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class TerminalFleetHistorySearchTests: XCTestCase {
    private final class Lifetime { var current = true }
    private struct Fixture {
        let source: TerminalFleetHistorySearch.Source
        let control: TerminalMuxControl
        let lifetime: Lifetime
        let sessionID: UUID
    }

    private let query = TerminalMuxControl.SearchQuery(text: "needle", caseSensitive: false, regex: false)

    private func machine(_ id: String, panes: [Int] = [1], name: String? = nil,
                         title: String = "Build") throws -> Fixture {
        let control = TerminalMuxControl(), lifetime = Lifetime(), sessionID = UUID()
        try control.inputRouting.replaceInventory(panes)
        if let first = panes.first { control.inputRouting.didApplyOutput(try control.inputRouting.select(pane: first)) }
        control.publish(panes: panes.map {
            .init(paneId: $0, tabId: $0, windowId: 0, tabTitle: "", title: title, workspace: "default",
                  workingDir: nil, size: .init(rows: 24, cols: 80), isActive: $0 == panes.first, isZoomed: false)
        }, shown: panes.first)
        let source = TerminalFleetHistorySearch.Source(machineID: id, machineName: name ?? id,
            sessionID: sessionID, control: control, isCurrent: { lifetime.current })
        return Fixture(source: source, control: control, lifetime: lifetime, sessionID: sessionID)
    }

    private func answer(_ fixture: Fixture, pane: Int = 1, count: Int = 1, text: String = "a needle",
                        rows: Int = 100, error: String? = nil, request: UUID? = nil) throws {
        let request = try request ?? XCTUnwrap(fixture.control.current.search?.id)
        fixture.control.deliver(search: request, pane: pane, results: .init(search: 1, matches: (0..<count).map {
            .init(row: Int64($0 + 5), startColumn: 2, endRow: Int64($0 + 5), endColumn: 8, text: text)
        }, searchedRows: rows, error: error))
    }

    func testAtMostThreeMachinesRunAndCompletionAdmitsTheNextWithoutChangingFocus() async throws {
        let now = ContinuousClock.now
        let machines = try (0..<5).map { try machine("m\($0)") }
        let before = machines.map { $0.control.inputRouting.current }
        let owner = try TerminalFleetHistorySearch(query: query, sources: machines.map(\.source), at: now)
        XCTAssertEqual(machines.map { $0.control.current.search != nil }, [true, true, true, false, false])
        XCTAssertEqual(owner.coverage.map(\.status), [.searching, .searching, .searching, .queued, .queued])
        try answer(machines[0])
        owner.advance(at: now.advanced(by: .milliseconds(1)))
        XCTAssertEqual(machines.map { $0.control.current.search != nil }, [false, true, true, true, false])
        XCTAssertEqual(owner.coverage.first?.status, .complete)
        XCTAssertEqual(owner.hits.count, 1)
        for fixture in machines[1...3] { try answer(fixture) }
        owner.advance(at: now.advanced(by: .milliseconds(2)))
        XCTAssertEqual(machines.map { $0.control.current.search != nil }, [false, false, false, false, true])
        XCTAssertEqual(machines.map { $0.control.inputRouting.current }, before)
        XCTAssertTrue(machines.allSatisfy { $0.control.current.shown == 1 })
        owner.retire()
        XCTAssertTrue(machines.allSatisfy { $0.control.current.search == nil })
    }

    func testDisconnectedBusyContextAndEmptySourcesAreExplicitlyNotSearched() async throws {
        let busy = try machine("busy"), context = try machine("context")
        let empty = try machine("empty", panes: [])
        let other = busy.control.search(query, panes: [1])
        context.control.readContext(pane: 1, row: 5)
        let contextID = context.control.current.context?.id
        let owner = try TerminalFleetHistorySearch(query: query, sources: [
            .init(machineID: "offline", machineName: "Offline", unavailable: "Disconnected at query start."),
            busy.source, context.source, empty.source
        ])
        XCTAssertFalse(owner.isRunning)
        XCTAssertEqual(owner.coverage.map(\.status), [.unavailable, .notSearched, .notSearched, .notSearched])
        XCTAssertTrue(owner.coverage.allSatisfy { $0.issue != nil })
        XCTAssertEqual(busy.control.current.search?.id, other)
        XCTAssertEqual(context.control.current.context?.id, contextID)
        XCTAssertNil(empty.control.current.search)
        owner.retire()
        XCTAssertEqual(busy.control.takeSearch()?.id, other)
        XCTAssertEqual(context.control.current.context?.id, contextID)
    }

    func testLargeMachineSearchesAllBatchesWithoutIncludingNewOrReusedPanes() async throws {
        let now = ContinuousClock.now
        let fixture = try machine("large", panes: Array(1...200))
        let owner = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
        let first = try XCTUnwrap(fixture.control.current.search)
        XCTAssertEqual(first.panes, Array(1...128))
        for id in first.panes { try answer(fixture, pane: id, count: 0, rows: 1) }

        // Pane 129 closed and its numeric ID was reused before its turn.
        let inventory = fixture.control.current.panes
        let newPane = try XCTUnwrap(try machine("new", panes: [201]).control.current.panes.first)
        fixture.control.remove(pane: 129)
        fixture.control.inputRouting.remove(pane: 129)
        try fixture.control.inputRouting.replaceInventory(Array(1...201))
        fixture.control.publish(panes: inventory + [newPane], shown: 1)
        owner.advance(at: now.advanced(by: .seconds(1)))
        let second = try XCTUnwrap(fixture.control.current.search)
        XCTAssertNotEqual(first.id, second.id)
        XCTAssertEqual(second.panes, Array(130...200))
        for id in second.panes { try answer(fixture, pane: id, count: id == 200 ? 1 : 0, rows: 1) }
        owner.advance(at: now.advanced(by: .seconds(2)))
        let coverage = try XCTUnwrap(owner.coverage.first)
        XCTAssertEqual(coverage.totalPanes, 200)
        XCTAssertEqual(coverage.searchedPanes, 199)
        XCTAssertEqual(coverage.searchedRows, 199)
        XCTAssertEqual(coverage.failedPanes, 1)
        XCTAssertEqual(coverage.status, .partial)
        XCTAssertEqual(owner.hits.map(\.paneID), [200])
        XCTAssertFalse(owner.isRunning)
        XCTAssertNil(fixture.control.current.search)
    }

    func testLargeMachineFinishesEveryBatchAndReleasesTheFinalRequest() async throws {
        let now = ContinuousClock.now
        let fixture = try machine("large", panes: Array(1...257))
        let owner = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
        for (index, expected) in [Array(1...128), Array(129...256), [257]].enumerated() {
            let batch = try XCTUnwrap(fixture.control.current.search)
            XCTAssertEqual(batch.panes, expected)
            for id in batch.panes { try answer(fixture, pane: id, count: 0, rows: 1) }
            owner.advance(at: now.advanced(by: .seconds(index + 1)))
        }
        let coverage = try XCTUnwrap(owner.coverage.first)
        XCTAssertEqual(coverage.status, .complete)
        XCTAssertEqual(coverage.searchedPanes, 257)
        XCTAssertEqual(coverage.searchedRows, 257)
        XCTAssertNil(fixture.control.current.search)
        XCTAssertFalse(owner.isRunning)
    }

    func testSynchronousBatchHandoffObserverCannotStartAFourthMachine() async throws {
        let now = ContinuousClock.now
        let large = try machine("large", panes: Array(1...129))
        let others = try ["second", "third", "fourth"].map { try machine($0) }
        let owner = try TerminalFleetHistorySearch(query: query, sources: ([large] + others).map(\.source), at: now)
        for id in 1...128 { try answer(large, pane: id, count: 0) }
        large.control.observe { snapshot in
            if snapshot.search == nil {
                MainActor.assumeIsolated { owner.advance(at: now.advanced(by: .seconds(1))) }
            }
        }
        owner.advance(at: now.advanced(by: .seconds(1)))
        XCTAssertEqual(large.control.current.search?.panes, [129])
        XCTAssertEqual(owner.coverage.map(\.status), [.searching, .searching, .searching, .queued])
        XCTAssertNil(others.last?.control.current.search)
        owner.retire()
        XCTAssertNil(others.last?.control.takeSearch())
    }

    func testLaterBatchSharesDeadlineAndPreservesIndependentContext() async throws {
        for contextOpened in [false, true] {
            let now = ContinuousClock.now
            let fixture = try machine("large", panes: Array(1...129))
            let owner = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
            let first = try XCTUnwrap(fixture.control.current.search)
            for id in first.panes { try answer(fixture, pane: id, count: 0) }
            if contextOpened { fixture.control.readContext(pane: 1, row: 8) }
            let context = fixture.control.current.context
            owner.advance(at: now.advanced(by: .seconds(31)))
            if contextOpened {
                XCTAssertNil(fixture.control.current.search)
                XCTAssertEqual(fixture.control.current.context, context)
                XCTAssertNotNil(fixture.control.takeContext())
            } else {
                XCTAssertEqual(fixture.control.current.search?.panes, [129])
                owner.advance(at: now.advanced(by: .seconds(32)))
            }
            XCTAssertEqual(owner.coverage.first?.status, .partial)
            XCTAssertEqual(owner.coverage.first?.searchedPanes, 128)
            XCTAssertNil(fixture.control.current.search)
        }
    }

    func testInvalidQueriesAndInventoriesDoNotPublishAnyWork() async throws {
        let fixture = try machine("one")
        for text in ["", String(repeating: "é", count: 2049)] {
            XCTAssertThrowsError(try TerminalFleetHistorySearch(
                query: .init(text: text, caseSensitive: false, regex: false), sources: [fixture.source])) {
                XCTAssertEqual($0 as? TerminalFleetHistorySearch.Failure, .invalidQuery)
            }
        }
        XCTAssertThrowsError(try TerminalFleetHistorySearch(query: query, sources: [fixture.source, fixture.source]))
        let unavailable = (1...128).map {
            TerminalFleetHistorySearch.Source(machineID: "m\($0)", machineName: "M", unavailable: "Offline")
        }
        XCTAssertThrowsError(try TerminalFleetHistorySearch(query: query, sources: [fixture.source] + unavailable))
        for source in [
            TerminalFleetHistorySearch.Source(machineID: String(repeating: "x", count: 129), machineName: "M", unavailable: "Offline"),
            TerminalFleetHistorySearch.Source(machineID: "m", machineName: String(repeating: "é", count: 513), unavailable: "Offline")
        ] {
            XCTAssertThrowsError(try TerminalFleetHistorySearch(query: query, sources: [fixture.source, source]))
        }
        XCTAssertNil(fixture.control.current.search); XCTAssertNil(fixture.control.takeSearch())
    }

    func testCompleteEmptySearchReportsRowsAndReleasesPerMachinePayloadWithoutLaterTimeout() async throws {
        let now = ContinuousClock.now, fixture = try machine("one", panes: [1, 2])
        let owner = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
        try answer(fixture, pane: 1, count: 0, rows: 120)
        try answer(fixture, pane: 2, count: 0, rows: 35)
        owner.advance(at: now.advanced(by: .seconds(1)))
        XCTAssertFalse(owner.isRunning); XCTAssertNil(owner.issue); XCTAssertTrue(owner.hits.isEmpty)
        let coverage = try XCTUnwrap(owner.coverage.first)
        XCTAssertEqual(coverage.status, .complete); XCTAssertEqual(coverage.searchedPanes, 2)
        XCTAssertEqual(coverage.searchedRows, 155)
        XCTAssertNil(fixture.control.current.search); XCTAssertNil(fixture.control.takeSearch())
        let revision = owner.revision
        owner.advance(at: now.advanced(by: .seconds(100)))
        XCTAssertEqual(owner.coverage, [coverage]); XCTAssertNil(owner.issue)
        XCTAssertEqual(owner.revision, revision, "monitoring a finished snapshot must not invent a timeout")
    }

    func testSameNumericPaneOnDifferentMachinesKeepsItsOwnCapturedConnectionAndSession() async throws {
        let now = ContinuousClock.now, first = try machine("alpha"), second = try machine("beta")
        let owner = try TerminalFleetHistorySearch(query: query, sources: [first.source, second.source], at: now)
        try answer(first, text: "alpha needle"); try answer(second, text: "beta needle")
        owner.advance(at: now.advanced(by: .seconds(1)))
        XCTAssertEqual(owner.hits.map(\.machineID), ["alpha", "beta"])
        XCTAssertEqual(Set(owner.hits.map(\.id)).count, 2)
        let target = try owner.navigationTarget(XCTUnwrap(owner.hits.first?.id))
        XCTAssertEqual(target.machineID, "alpha"); XCTAssertEqual(target.sessionID, first.sessionID)
        XCTAssertEqual(target.plan.paneID, 1)
        XCTAssertNoThrow(try first.control.inputRouting.validateNavigation(target.plan))
        XCTAssertThrowsError(try second.control.inputRouting.validateNavigation(target.plan))
        XCTAssertNil(first.control.current.search); XCTAssertNil(second.control.current.search)
    }

    func testPaneFailureIsPartialAndSuccessfulRowsRemainReadable() async throws {
        let now = ContinuousClock.now, fixture = try machine("one", panes: [1, 2])
        let owner = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
        try answer(fixture, pane: 1)
        try answer(fixture, pane: 2, error: "History unavailable")
        owner.advance(at: now.advanced(by: .seconds(1)))
        let coverage = try XCTUnwrap(owner.coverage.first)
        XCTAssertEqual(coverage.status, .partial); XCTAssertEqual(coverage.searchedPanes, 1)
        XCTAssertEqual(coverage.failedPanes, 1); XCTAssertEqual(coverage.totalPanes, 2)
        XCTAssertEqual(coverage.searchedRows, 100); XCTAssertEqual(coverage.issue, "History unavailable")
        XCTAssertEqual(owner.hits.count, 1); XCTAssertFalse(owner.isRunning)
        XCTAssertNoThrow(try owner.navigationTarget(XCTUnwrap(owner.hits.first?.id)))
    }

    func testMuxDeadlineKeepsAcceptedResultsButCannotClaimAllPanesWereSearched() async throws {
        let now = ContinuousClock.now, fixture = try machine("one", panes: [1, 2])
        let owner = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
        try answer(fixture, pane: 1)
        fixture.control.finishSearch(try XCTUnwrap(fixture.control.current.search?.id), reason: "Remote search timed out.")
        owner.advance(at: now.advanced(by: .seconds(1)))
        XCTAssertEqual(owner.hits.count, 1)
        XCTAssertEqual(owner.coverage.first?.status, .partial)
        XCTAssertEqual(owner.coverage.first?.searchedPanes, 1)
        XCTAssertEqual(owner.coverage.first?.issue, "Remote search timed out.")
        XCTAssertNil(fixture.control.current.search)
    }

    func testReplacementSearchAndContextSurviveOldOwnerReleaseAndLateAnswers() async throws {
        let now = ContinuousClock.now, fixture = try machine("one")
        let owner = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
        let old = try XCTUnwrap(fixture.control.current.search?.id)
        let replacement = fixture.control.search(.init(text: "replacement", caseSensitive: true, regex: false), panes: [1])
        fixture.control.readContext(pane: 1, row: 5)
        let context = fixture.control.current.context?.id
        owner.advance(at: now.advanced(by: .seconds(1)))
        XCTAssertEqual(owner.coverage.first?.status, .partial)
        owner.stop(); owner.retire()
        try answer(fixture, request: old)
        XCTAssertEqual(fixture.control.current.search?.id, replacement)
        XCTAssertEqual(fixture.control.current.context?.id, context)
        XCTAssertEqual(fixture.control.takeSearch()?.id, replacement)
        XCTAssertTrue(owner.hits.isEmpty)
    }

    func testLaterIndependentContextSurvivesFleetCompletionAndDirectRetirement() async throws {
        for completeBeforeRetirement in [false, true] {
            let now = ContinuousClock.now, fixture = try machine("one")
            let owner = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
            let searchID = try XCTUnwrap(fixture.control.current.search?.id)
            fixture.control.readContext(pane: 1, row: 42, before: 2, after: 3)
            let context = try XCTUnwrap(fixture.control.current.context)
            XCTAssertEqual(fixture.control.current.search?.id, searchID,
                           "the independent reader does not replace the fleet search")
            if completeBeforeRetirement {
                try answer(fixture)
                owner.advance(at: now.advanced(by: .seconds(1)))
                XCTAssertEqual(owner.coverage.first?.status, .complete)
                XCTAssertNil(fixture.control.current.search)
                XCTAssertEqual(fixture.control.current.context, context)
            }
            owner.retire()
            XCTAssertNil(fixture.control.current.search); XCTAssertNil(fixture.control.takeSearch())
            XCTAssertEqual(fixture.control.current.context, context)
            let pending = try XCTUnwrap(fixture.control.takeContext(),
                                       "fleet cleanup must preserve the independent reader's unsent request")
            XCTAssertEqual(pending.id, context.id); XCTAssertEqual(pending.pane, 1)
            XCTAssertEqual(pending.first, 40); XCTAssertEqual(pending.count, 6)
            let line = TerminalMuxContextLine(row: 42, text: "independent context")
            fixture.control.deliver(context: pending.id, lines: [line])
            XCTAssertEqual(fixture.control.current.context?.lines, [line])
            XCTAssertNil(fixture.control.current.context?.error)
        }
    }

    func testReentrantObserverReplacementCannotBeErasedByAJustAdmittedFleetOwner() async throws {
        let now = ContinuousClock.now, fixture = try machine("one")
        let control = fixture.control
        let replacementQuery = TerminalMuxControl.SearchQuery(text: "replacement", caseSensitive: true, regex: false)
        control.observe { snapshot in
            if snapshot.search?.query.text == "needle" { control.search(replacementQuery, panes: [1]) }
        }
        let owner = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
        let replacement = try XCTUnwrap(control.current.search?.id)
        owner.advance(at: now.advanced(by: .seconds(1)))
        XCTAssertEqual(owner.coverage.first?.status, .partial)
        XCTAssertEqual(control.current.search?.id, replacement)
        XCTAssertEqual(control.takeSearch()?.query, replacementQuery)
        owner.retire()
        XCTAssertEqual(control.current.search?.id, replacement)
    }

    func testAggregateMatchLimitStopsActiveAndQueuedMachinesWithoutOverretaining() async throws {
        let now = ContinuousClock.now
        let first = try machine("first", panes: [1, 2, 3]), second = try machine("second", panes: [1, 2])
        let third = try machine("third"), fourth = try machine("fourth")
        let all = [first, second, third, fourth]
        let owner = try TerminalFleetHistorySearch(query: query, sources: all.map(\.source), at: now)
        for pane in [1, 2, 3] { try answer(first, pane: pane, count: 128) }
        for pane in [1, 2] { try answer(second, pane: pane, count: 128) }
        owner.advance(at: now.advanced(by: .seconds(1)))
        XCTAssertEqual(owner.hits.count, TerminalFleetHistorySearch.maximumMatches)
        XCTAssertEqual(owner.hits.filter { $0.machineID == "first" }.count, 384)
        XCTAssertEqual(owner.hits.filter { $0.machineID == "second" }.count, 128)
        XCTAssertEqual(owner.coverage.map(\.status), [.complete, .partial, .partial, .notSearched])
        XCTAssertNotNil(owner.issue); XCTAssertFalse(owner.isRunning)
        XCTAssertTrue(all.allSatisfy { $0.control.current.search == nil })
        XCTAssertNil(fourth.control.takeSearch(), "a result cap must not start the queued machine")
    }

    func testAggregateTextBudgetCountsUTF8BytesAcrossMachines() async throws {
        let now = ContinuousClock.now, first = try machine("first"), second = try machine("second")
        let owner = try TerminalFleetHistorySearch(query: query, sources: [first.source, second.source], at: now)
        let text = String(repeating: "é", count: 8192)
        try answer(first, count: 20, text: text)
        try answer(second, count: 20, text: text)
        owner.advance(at: now.advanced(by: .seconds(1)))
        XCTAssertEqual(owner.hits.count, 32)
        XCTAssertEqual(owner.hits.reduce(0) { $0 + $1.match.text.utf8.count }, TerminalFleetHistorySearch.maximumTextBytes)
        XCTAssertEqual(owner.coverage.map(\.status), [.complete, .partial])
        XCTAssertNotNil(owner.issue); XCTAssertNil(second.control.current.search)
    }

    func testMachineAndWholeQueryDeadlinesAreFiniteAndQueuedMachinesAreNotCalledSearched() async throws {
        let now = ContinuousClock.now
        let machines = try (0..<10).map { try machine("m\($0)") }
        let owner = try TerminalFleetHistorySearch(query: query, sources: machines.map(\.source), at: now)
        owner.advance(at: now.advanced(by: .seconds(31)))
        XCTAssertEqual(owner.coverage.filter { $0.status == .searching }.count, 3)
        owner.advance(at: now.advanced(by: .seconds(32)))
        XCTAssertEqual(owner.coverage.prefix(3).map(\.status), [.partial, .partial, .partial])
        XCTAssertEqual(owner.coverage[3...5].map(\.status), [.searching, .searching, .searching])
        owner.advance(at: now.advanced(by: .seconds(64)))
        XCTAssertEqual(owner.coverage[6...8].map(\.status), [.searching, .searching, .searching])
        owner.advance(at: now.advanced(by: .seconds(90)))
        XCTAssertFalse(owner.isRunning); XCTAssertNotNil(owner.issue)
        XCTAssertEqual(owner.coverage.last?.status, .notSearched)
        XCTAssertTrue(owner.coverage.allSatisfy { $0.searchedPanes == 0 })
        XCTAssertTrue(machines.allSatisfy { $0.control.current.search == nil })
        XCTAssertNil(machines.last?.control.takeSearch())
    }

    func testStopKeepsVisibleMatchesButSourceRetirementDiscardsThemAndDoesNotResume() async throws {
        let now = ContinuousClock.now, fixture = try machine("one", panes: [1, 2])
        let owner = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
        try answer(fixture, pane: 1)
        owner.advance(at: now.advanced(by: .seconds(1)))
        let id = try XCTUnwrap(owner.hits.first?.id)
        owner.stop()
        XCTAssertEqual(owner.hits.count, 1); XCTAssertFalse(owner.isRunning)
        XCTAssertEqual(owner.coverage.first?.status, .partial)
        XCTAssertNoThrow(try owner.navigationTarget(id))
        XCTAssertNil(fixture.control.current.search)
        fixture.lifetime.current = false
        owner.advance(at: now.advanced(by: .seconds(2)))
        XCTAssertTrue(owner.hits.isEmpty); XCTAssertNil(owner.hit(id))
        XCTAssertEqual(owner.coverage.first?.status, .unavailable)
        fixture.lifetime.current = true
        owner.advance(at: now.advanced(by: .seconds(3)))
        XCTAssertNil(fixture.control.current.search, "reconnecting never joins an existing query")
        XCTAssertThrowsError(try owner.navigationTarget(id))
    }

    func testPaneRemovalAndNumericReuseInvalidateNavigationButKeepEarlierCapturedText() async throws {
        let now = ContinuousClock.now, fixture = try machine("one", panes: [1, 2])
        let owner = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
        try answer(fixture, pane: 1); try answer(fixture, pane: 2, count: 0)
        owner.advance(at: now.advanced(by: .seconds(1)))
        let id = try XCTUnwrap(owner.hits.first?.id)
        fixture.control.inputRouting.remove(pane: 1)
        try fixture.control.inputRouting.replaceInventory([1, 2])
        _ = try fixture.control.inputRouting.select(pane: 1)
        owner.advance(at: now.advanced(by: .seconds(2)))
        XCTAssertEqual(owner.hit(id)?.match.text, "a needle")
        XCTAssertEqual(owner.hit(id)?.canNavigate, false)
        XCTAssertThrowsError(try owner.navigationTarget(id))
        XCTAssertNil(fixture.control.current.search)
    }

    func testPaneRegistrationChangeBeforeHarvestDiscardsTheAmbiguousReply() async throws {
        let now = ContinuousClock.now, fixture = try machine("one")
        let owner = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
        try answer(fixture)
        fixture.control.inputRouting.remove(pane: 1)
        try fixture.control.inputRouting.replaceInventory([1])
        owner.advance(at: now.advanced(by: .seconds(1)))
        XCTAssertTrue(owner.hits.isEmpty)
        XCTAssertEqual(owner.coverage.first?.status, .partial)
        XCTAssertEqual(owner.coverage.first?.failedPanes, 1)
        XCTAssertEqual(owner.coverage.first?.searchedPanes, 0)
    }

    func testRetiredQueryCannotEraseOrReceiveResultsForANewQueryOnTheSameConnection() async throws {
        let now = ContinuousClock.now, fixture = try machine("one")
        let old = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
        let oldRequest = try XCTUnwrap(fixture.control.current.search?.id)
        old.retire()
        XCTAssertTrue(old.isRetired); XCTAssertNil(old.query); XCTAssertTrue(old.coverage.isEmpty)
        let current = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
        let currentRequest = try XCTUnwrap(fixture.control.current.search?.id)
        try answer(fixture, text: "late old text", request: oldRequest)
        XCTAssertTrue(fixture.control.current.search?.results.isEmpty == true)
        old.advance(at: now.advanced(by: .seconds(1))); old.stop(); old.retire()
        XCTAssertEqual(fixture.control.current.search?.id, currentRequest)
        try answer(fixture, text: "current text", request: currentRequest)
        current.advance(at: now.advanced(by: .seconds(1)))
        XCTAssertEqual(current.hits.map(\.match.text), ["current text"])
        XCTAssertTrue(old.hits.isEmpty)
    }

    func testSceneRetirementImmediatelyReleasesPayloadAndAllOwnedRequests() async throws {
        let now = ContinuousClock.now, scene = Lifetime(), fixture = try machine("one", panes: [1, 2])
        let owner = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now,
                                                  isCurrent: { scene.current })
        try answer(fixture, pane: 1)
        owner.advance(at: now.advanced(by: .seconds(1)))
        let id = try XCTUnwrap(owner.hits.first?.id)
        scene.current = false
        XCTAssertNil(owner.hit(id), "an action checks source lifetime before the next polling turn")
        XCTAssertThrowsError(try owner.navigationTarget(id))
        owner.advance(at: now.advanced(by: .seconds(2)))
        XCTAssertTrue(owner.isRetired); XCTAssertNil(owner.query)
        XCTAssertTrue(owner.hits.isEmpty); XCTAssertTrue(owner.coverage.isEmpty)
        XCTAssertNil(fixture.control.current.search); XCTAssertNil(fixture.control.takeSearch())
    }

    func testCapturedLabelsFitTheExactOverviewHandoffAtUTF8Boundaries() async throws {
        let now = ContinuousClock.now
        for title in [String(repeating: "é", count: 1024), String(repeating: "é", count: 1025)] {
            let fixture = try machine(String(repeating: "m", count: 128),
                name: String(repeating: "é", count: 512), title: title)
            let owner = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
            try answer(fixture)
            owner.advance(at: now.advanced(by: .seconds(1)))
            let hit = try XCTUnwrap(owner.hits.first)
            let target = try owner.navigationTarget(hit.id)
            XCTAssertLessThanOrEqual(target.paneTitle.utf8.count, 2048)
            if title.utf8.count > 2048 { XCTAssertEqual(target.paneTitle, "Pane 1") }
            let catalog = try TerminalFleetOverview(machines: [
                .init(id: target.machineID, name: target.machineName, endpoint: "", status: .live,
                      sessionID: target.sessionID, panes: [.init(plan: target.plan, title: target.paneTitle)])
            ])
            let choice = try XCTUnwrap(catalog.search(.init(scope: .panes), limit: 1).rows.first?.id)
            XCTAssertEqual(try catalog.validatePane(choice, sessionID: fixture.sessionID,
                                                   routing: fixture.control.inputRouting), target.plan)
            owner.retire()
        }
    }

    func testClockReversalStopsOnlyThisQueryAndKeepsCoverageExplicit() async throws {
        let now = ContinuousClock.now, fixture = try machine("one")
        let owner = try TerminalFleetHistorySearch(query: query, sources: [fixture.source], at: now)
        owner.advance(at: now.advanced(by: .seconds(-1)))
        XCTAssertFalse(owner.isRunning); XCTAssertNotNil(owner.issue)
        XCTAssertEqual(owner.coverage.first?.status, .partial)
        XCTAssertNil(fixture.control.current.search)
        owner.advance(at: now.advanced(by: .seconds(1)))
        XCTAssertNil(fixture.control.current.search)
    }
}
