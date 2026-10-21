import Foundation
import XCTest
@testable import FrankenTermKit

private actor JournalStartGate {
    private var opened = false
    private var waiter: CheckedContinuation<Void, Never>?
    func wait() async {
        if opened { return }
        await withCheckedContinuation { waiter = $0 }
    }
    func open() { opened = true; waiter?.resume(); waiter = nil }
}

@MainActor
final class TerminalSessionJournalTests: XCTestCase {
    private func scope(workspace: UUID = UUID(), revision: UUID = UUID(),
                       machine: String = "machine-1", route: String = "route-1") throws -> TerminalSessionScope {
        try .init(workspaceID: workspace, workspaceRevision: revision, machineID: machine,
                  routeID: route, transportID: UUID(), channelID: UUID())
    }

    private func expect(_ expected: TerminalSessionJournalError,
                        _ operation: () async throws -> Void,
                        file: StaticString = #filePath, line: UInt = #line) async {
        do { try await operation(); XCTFail("Expected a journal refusal", file: file, line: line) }
        catch { XCTAssertEqual(error as? TerminalSessionJournalError, expected, file: file, line: line) }
    }

    func testScopeAndCapacityConfigurationAreBounded() throws {
        for machine in ["", "has spaces", "command\ninput", String(repeating: "x", count: 129)] {
            XCTAssertThrowsError(try scope(machine: machine)) {
                XCTAssertEqual($0 as? TerminalSessionJournalError, .invalidScope)
            }
        }
        XCTAssertThrowsError(try scope(route: "../bad/path"))
        for limits in [(0, 1), (257, 1), (1, 0), (1, 129)] {
            XCTAssertThrowsError(try TerminalSessionJournal(persistence: StoreTestPersistence(),
                maximumUnresolved: limits.0, maximumResolved: limits.1)) {
                XCTAssertEqual($0 as? TerminalSessionJournalError, .invalidLimits)
            }
        }
    }

    func testCreationReturnsOnlyAfterContentFreeMarkerIsPublished() async throws {
        let disk = StoreTestPersistence(), journal = try TerminalSessionJournal(persistence: disk)
        let connection = try scope()
        let epoch = try await journal.beginCreation(scope: connection)
        let bytes = try XCTUnwrap(disk.bytes(.catalog))
        let root = try StrictJSON.decode(bytes).object()
        let row = try XCTUnwrap(root["records"]).array().first?.object()
        XCTAssertEqual(try row?.text("id"), epoch.id.uuidString)
        XCTAssertEqual(try row?.text("state"), "creationStarted")
        XCTAssertEqual(try row?.text("transport"), connection.transportID.uuidString)
        XCTAssertEqual(try row?.text("channel"), connection.channelID.uuidString)
        XCTAssertEqual(Set(try XCTUnwrap(row).keys), ["id", "revision", "workspace", "workspace_revision",
            "machine", "route", "transport", "channel", "process", "owner", "state", "input_evidence"])
        XCTAssertNil(disk.bytes(.journal))
        let records = try await journal.records()
        XCTAssertEqual(records.map(\.scope), [connection])
        XCTAssertTrue(records[0].requiresReview)
        XCTAssertFalse(records[0].canResolve)
    }

    func testWriteFailureBeforePublicationReturnsNoCreationPermission() async throws {
        let disk = StoreTestPersistence(), journal = try TerminalSessionJournal(persistence: disk)
        let connection = try scope()
        disk.configure(.before(.catalog))
        var shellRequests = 0
        await expect(.recoveryRequired) {
            _ = try await journal.beginCreation(scope: connection)
            shellRequests += 1
        }
        XCTAssertEqual(shellRequests, 0)
        XCTAssertNil(disk.bytes(.catalog))
        let records = try await journal.records()
        XCTAssertTrue(records.isEmpty)
        _ = try await journal.beginCreation(scope: connection)
    }

    func testAmbiguousCreationWriteNeverMintsPermissionOrRetriesAutomatically() async throws {
        let disk = StoreTestPersistence(), journal = try TerminalSessionJournal(persistence: disk)
        let connection = try scope()
        disk.configure(.after(.catalog))
        var shellRequests = 0
        await expect(.recoveryRequired) {
            _ = try await journal.beginCreation(scope: connection)
            shellRequests += 1
        }
        XCTAssertEqual(shellRequests, 0)
        let published = try await journal.records()
        XCTAssertEqual(published.count, 1)
        XCTAssertTrue(published[0].canResolve) // This owner knows no permission escaped.
        await expect(.reviewRequired) { _ = try await journal.beginCreation(scope: connection) }
        let other = try TerminalSessionJournal(persistence: disk)
        let conservative = try await other.records()
        XCTAssertFalse(conservative[0].canResolve)
        await expect(.activeSession) { try await other.resolve(conservative[0]) }
        try await journal.resolve(published[0])
        _ = try await journal.beginCreation(scope: connection)
        XCTAssertEqual(shellRequests, 0)
    }

    func testUncertaintyBlocksAcrossRouteAndWorkspaceRevisionChanges() async throws {
        let journal = try TerminalSessionJournal(persistence: StoreTestPersistence())
        let workspace = UUID(), first = try scope(workspace: workspace)
        _ = try await journal.beginCreation(scope: first)
        let changed = try scope(workspace: workspace, route: "other-route")
        XCTAssertNotEqual(first.workspaceRevision, changed.workspaceRevision)
        await expect(.reviewRequired) { _ = try await journal.beginCreation(scope: changed) }
        _ = try await journal.beginCreation(scope: scope(workspace: workspace, machine: "machine-2"))
        _ = try await journal.beginCreation(scope: scope(machine: "machine-1"))
        let records = try await journal.records()
        XCTAssertEqual(records.count, 3)
    }

    func testActiveEpochCannotResolveAndInterruptionIsIdempotent() async throws {
        let journal = try TerminalSessionJournal(persistence: StoreTestPersistence())
        let connection = try scope(), epoch = try await journal.beginCreation(scope: connection)
        let active = try await journal.records()
        await expect(.activeSession) { try await journal.resolve(active[0]) }
        try await journal.markInterrupted(epoch: epoch)
        try await journal.markInterrupted(epoch: epoch)
        let interrupted = try await journal.records()
        XCTAssertEqual(interrupted[0].state, .interrupted)
        XCTAssertTrue(interrupted[0].canResolve)
        await expect(.reviewRequired) { _ = try await journal.beginCreation(scope: connection) }
        try await journal.resolve(interrupted[0])
        try await journal.markInterrupted(epoch: epoch)
        let acknowledged = try await journal.records()
        XCTAssertEqual(acknowledged[0].state, .acknowledged)
        XCTAssertFalse(acknowledged[0].requiresReview)
        XCTAssertFalse(acknowledged[0].canResolve)
        let next = try await journal.beginCreation(scope: connection)
        XCTAssertNotEqual(next.id, epoch.id)
    }

    func testAnotherOwnerCannotRetireEpochOrResolveLiveRecord() async throws {
        let disk = StoreTestPersistence()
        let first = try TerminalSessionJournal(persistence: disk)
        let second = try TerminalSessionJournal(persistence: disk)
        let epoch = try await first.beginCreation(scope: scope())
        await expect(.invalidEpoch) { try await second.markInterrupted(epoch: epoch) }
        let ownReview = try await second.records(), foreignReview = try await first.records()
        XCTAssertFalse(ownReview[0].canResolve)
        await expect(.activeSession) { try await second.resolve(ownReview[0]) }
        await expect(.staleReview) { try await second.resolve(foreignReview[0]) }
        try await first.markInterrupted(epoch: epoch)
        let updated = try await second.records()
        try await second.resolve(updated[0])
    }

    func testRelaunchKeepsUncertaintyUntilExplicitNewSessionReview() async throws {
        let disk = StoreTestPersistence(), connection = try scope()
        let previous = try TerminalSessionJournal(persistence: disk, processID: UUID())
        let oldEpoch = try await previous.beginCreation(scope: connection)
        let relaunched = try TerminalSessionJournal(persistence: disk, processID: UUID())
        let recovery = try await relaunched.records()
        XCTAssertEqual(recovery[0].id, oldEpoch.id)
        XCTAssertEqual(recovery[0].state, .creationStarted)
        XCTAssertTrue(recovery[0].canResolve)
        await expect(.reviewRequired) { _ = try await relaunched.beginCreation(scope: connection) }
        await expect(.invalidEpoch) { try await relaunched.markInterrupted(epoch: oldEpoch) }
        try await relaunched.resolve(recovery[0])
        let next = try await relaunched.beginCreation(scope: connection)
        XCTAssertNotEqual(next.id, oldEpoch.id)
        let records = try await relaunched.records()
        XCTAssertEqual(records.map(\.state), [.acknowledged, .creationStarted])
    }

    func testInterruptionWriteFaultPreservesMarkerAndCanBeRetried() async throws {
        for fault in [StoreTestPersistence.Fault.before(.catalog), .after(.catalog)] {
            let disk = StoreTestPersistence(), journal = try TerminalSessionJournal(persistence: disk)
            let connection = try scope(), epoch = try await journal.beginCreation(scope: connection)
            disk.configure(fault)
            await expect(.recoveryRequired) { try await journal.markInterrupted(epoch: epoch) }
            let uncertain = try await journal.records()
            XCTAssertEqual(uncertain.count, 1)
            XCTAssertTrue(uncertain[0].requiresReview)
            await expect(.reviewRequired) { _ = try await journal.beginCreation(scope: connection) }
            try await journal.markInterrupted(epoch: epoch)
            let retired = try await journal.records()
            XCTAssertEqual(retired[0].state, .interrupted)
            XCTAssertTrue(retired[0].canResolve)
            try await journal.resolve(retired[0])
        }
    }

    func testFailedResolutionDoesNotReportDurabilityOrEraseHistory() async throws {
        for fault in [StoreTestPersistence.Fault.before(.catalog), .after(.catalog)] {
            let disk = StoreTestPersistence(), journal = try TerminalSessionJournal(persistence: disk)
            let connection = try scope(), epoch = try await journal.beginCreation(scope: connection)
            try await journal.markInterrupted(epoch: epoch)
            let review = try await journal.records()
            disk.configure(fault)
            await expect(.recoveryRequired) { try await journal.resolve(review[0]) }
            let observed = try await journal.records()
            XCTAssertEqual(observed.count, 1)
            XCTAssertEqual(observed[0].scope, connection)
            if observed[0].requiresReview {
                await expect(.reviewRequired) { _ = try await journal.beginCreation(scope: connection) }
                try await journal.resolve(observed[0])
            }
            // A new permission always requires another successful durable write,
            // even when the failed resolution's candidate is already readable.
            disk.configure(.before(.catalog))
            await expect(.recoveryRequired) { _ = try await journal.beginCreation(scope: connection) }
        }
    }

    func testStaleReviewCannotResolveChangedRecord() async throws {
        let journal = try TerminalSessionJournal(persistence: StoreTestPersistence())
        let epoch = try await journal.beginCreation(scope: scope())
        let beforeClose = try await journal.records()
        try await journal.markInterrupted(epoch: epoch)
        await expect(.staleReview) { try await journal.resolve(beforeClose[0]) }
        let current = try await journal.records()
        try await journal.resolve(current[0])
        await expect(.staleReview) { try await journal.resolve(current[0]) }
    }

    func testUnresolvedCapacityNeverEvictsAnotherMachine() async throws {
        let journal = try TerminalSessionJournal(persistence: StoreTestPersistence(), maximumUnresolved: 2)
        let first = try await journal.beginCreation(scope: scope())
        let second = try await journal.beginCreation(scope: scope())
        await expect(.capacity) { _ = try await journal.beginCreation(scope: scope()) }
        let full = try await journal.records()
        XCTAssertEqual(full.map(\.id), [first.id, second.id])
        try await journal.markInterrupted(epoch: first)
        let retired = try await journal.records()
        try await journal.resolve(retired[0])
        _ = try await journal.beginCreation(scope: scope())
        let reopened = try await journal.records()
        XCTAssertTrue(reopened.contains { $0.id == second.id && $0.requiresReview })
    }

    func testOnlyResolvedHistoryIsPrunedAndLateDuplicateCloseIsHarmless() async throws {
        let journal = try TerminalSessionJournal(persistence: StoreTestPersistence(),
                                                maximumUnresolved: 2, maximumResolved: 1)
        let retained = try await journal.beginCreation(scope: scope())
        var oldest: TerminalSessionEpoch?
        for _ in 0..<4 {
            let epoch = try await journal.beginCreation(scope: scope())
            if oldest == nil { oldest = epoch }
            try await journal.markInterrupted(epoch: epoch)
            let records = try await journal.records()
            try await journal.resolve(try XCTUnwrap(records.first { $0.id == epoch.id }))
        }
        let compacted = try await journal.records()
        XCTAssertEqual(compacted.count, 2)
        XCTAssertEqual(compacted[0].id, retained.id)
        XCTAssertTrue(compacted[0].requiresReview)
        XCTAssertEqual(compacted[1].state, .acknowledged)
        try await journal.markInterrupted(epoch: XCTUnwrap(oldest))
    }

    func testWorkspaceCatalogOrImportJournalNamespaceIsNeverOverwritten() async throws {
        for slot in [WorkspaceStorageSlot.catalog, .journal] {
            let disk = StoreTestPersistence()
            let workspaceBytes = Data("{\"version\":4,\"workspaces\":[]}".utf8)
            disk.seed(workspaceBytes, slot: slot)
            let journal = try TerminalSessionJournal(persistence: disk)
            await expect(.wrongNamespace) { _ = try await journal.beginCreation(scope: scope()) }
            XCTAssertEqual(disk.bytes(slot), workspaceBytes)
            if slot == .journal { XCTAssertNil(disk.bytes(.catalog)) }
        }
    }

    func testMalformedDuplicateAndUnsupportedCatalogsFailClosed() async throws {
        let original = StoreTestPersistence(), owner = try TerminalSessionJournal(persistence: original)
        _ = try await owner.beginCreation(scope: scope())
        let bytes = try XCTUnwrap(original.bytes(.catalog))
        let text = try XCTUnwrap(String(data: bytes, encoding: .utf8))
        var root = try StrictJSON.decode(bytes).object()
        root["version"] = .number("3")
        let future = try StrictJSON.encode(.object(root))
        root["version"] = .number("2")
        let rows = try XCTUnwrap(root["records"]).array()
        root["records"] = .array(rows + rows)
        let repeated = try StrictJSON.encode(.object(root))
        let duplicateKey = Data(text.replacingOccurrences(of: "\"version\":2",
                                                          with: "\"version\":2,\"version\":2").utf8)
        for corrupt in [Data("{".utf8), future, repeated, duplicateKey] {
            let disk = StoreTestPersistence(); disk.seed(corrupt, slot: .catalog)
            let journal = try TerminalSessionJournal(persistence: disk)
            await expect(.corruptStore) { _ = try await journal.beginCreation(scope: scope()) }
            XCTAssertEqual(disk.bytes(.catalog), corrupt)
        }
    }

    func testReadFailureAndCatalogReplacementCannotAuthorizeShell() async throws {
        let disk = StoreTestPersistence(), journal = try TerminalSessionJournal(persistence: disk)
        disk.configure(nil, failedRead: true)
        await expect(.recoveryRequired) { _ = try await journal.beginCreation(scope: scope()) }
        XCTAssertNil(disk.bytes(.catalog))
        disk.configure(nil)
        _ = try await journal.beginCreation(scope: scope())
        var root = try StrictJSON.decode(XCTUnwrap(disk.bytes(.catalog))).object()
        root["id"] = .string(UUID().uuidString)
        disk.seed(try StrictJSON.encode(.object(root)), slot: .catalog)
        await expect(.corruptStore) { _ = try await journal.beginCreation(scope: scope()) }
    }

    func testConcurrentOwnersSerializeCreationWithoutLosingMarkers() async throws {
        let disk = StoreTestPersistence(), connection = try scope()
        let first = try TerminalSessionJournal(persistence: disk), second = try TerminalSessionJournal(persistence: disk)
        async let left: Result<TerminalSessionEpoch, Error> = Self.attempt(first, connection)
        async let right: Result<TerminalSessionEpoch, Error> = Self.attempt(second, connection)
        let results = await [left, right]
        XCTAssertEqual(results.filter { if case .success = $0 { return true }; return false }.count, 1)
        for result in results {
            if case .failure(let error) = result { XCTAssertEqual(error as? TerminalSessionJournalError, .reviewRequired) }
        }
        _ = try await second.beginCreation(scope: scope(workspace: connection.workspaceID, machine: "different"))
        let records = try await first.records()
        XCTAssertEqual(records.count, 2)
    }

    private nonisolated static func attempt(_ journal: TerminalSessionJournal, _ connection: TerminalSessionScope)
        async -> Result<TerminalSessionEpoch, Error> {
        do { return .success(try await journal.beginCreation(scope: connection)) }
        catch { return .failure(error) }
    }

    func testCancellationBeforeReadinessCreatesNoMarker() async throws {
        let disk = StoreTestPersistence(), journal = try TerminalSessionJournal(persistence: disk)
        let gate = JournalStartGate(), connection = try scope()
        let task = Task {
            await gate.wait()
            return try await journal.beginCreation(scope: connection)
        }
        task.cancel(); await gate.open()
        do { _ = try await task.value; XCTFail("Cancelled creation returned permission") }
        catch { XCTAssertTrue(error is CancellationError) }
        XCTAssertNil(disk.bytes(.catalog))
    }

    func testProtectedFileMarkerSurvivesReopeningAndRequiresReview() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("ft-session-history-test-" + UUID().uuidString)
        let disk = try FileWorkspacePersistence(directory: directory)
        let first = try TerminalSessionJournal(persistence: disk, processID: UUID())
        let connection = try scope(), epoch = try await first.beginCreation(scope: connection)
        let reopenedDisk = try FileWorkspacePersistence(directory: directory)
        let reopened = try TerminalSessionJournal(persistence: reopenedDisk, processID: UUID())
        let recovery = try await reopened.records()
        XCTAssertEqual(recovery.map(\.id), [epoch.id])
        XCTAssertEqual(recovery[0].scope, connection)
        XCTAssertTrue(recovery[0].canResolve)
        await expect(.reviewRequired) { _ = try await reopened.beginCreation(scope: connection) }
        try await reopened.resolve(recovery[0])
        let third = try TerminalSessionJournal(persistence: reopenedDisk, processID: UUID())
        let resolved = try await third.records()
        XCTAssertEqual(resolved[0].state, .acknowledged)
        _ = try await third.beginCreation(scope: connection)
    }
}
