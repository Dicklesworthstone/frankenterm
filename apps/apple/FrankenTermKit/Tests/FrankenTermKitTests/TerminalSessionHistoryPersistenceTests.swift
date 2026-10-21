import Foundation
import XCTest
#if canImport(Security)
import Security
#endif
@testable import FrankenTermKit

/// Fail immediately before or after one real atomic, synchronized file write.
/// The anchor and all other file operations retain the production path.
private final class SessionHistoryWriteFault: WorkspacePersistence, @unchecked Sendable {
    private let files: FileWorkspacePersistence
    private let target: Int
    private let afterPublication: Bool
    private let lock = NSLock()
    private var writes = 0

    init(files: FileWorkspacePersistence, target: Int, afterPublication: Bool) {
        self.files = files; self.target = target; self.afterPublication = afterPublication
    }
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T {
        try files.withExclusiveAccess(body)
    }
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? {
        try files.read(slot, limit: limit)
    }
    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws {
        lock.lock(); writes += 1; let fail = writes == target; lock.unlock()
        if fail && !afterPublication { throw FTError.storage }
        try files.write(data, to: slot)
        if fail && afterPublication { throw FTError.storage }
    }
    func removeJournal() throws { throw TerminalSessionJournalError.wrongNamespace }
}

@MainActor
final class TerminalSessionHistoryPersistenceTests: XCTestCase {
    /// On Apple the continuity anchor is the device-only data-protection
    /// Keychain, which needs a keychain-entitled process. An unhosted package
    /// test process has none (errSecMissingEntitlement), so it skips here; the
    /// app-hosted FrankenTermTests compiles this same file and runs it with one.
    override func setUp() async throws {
        try await super.setUp()
        #if canImport(Security)
        let probe: [String: Any] = [kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: "org.frankenterm.tests.keychain-probe",
            kSecAttrAccount as String: UUID().uuidString, kSecUseDataProtectionKeychain as String: true,
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
            kSecValueData as String: Data([1])]
        let status = SecItemAdd(probe as CFDictionary, nil)
        if status == errSecMissingEntitlement {
            throw XCTSkip("The session-history anchor needs the data-protection Keychain, which this unhosted process cannot use; FrankenTermTests runs these tests in the entitled app")
        }
        var removal = probe; removal[kSecValueData as String] = nil; removal[kSecAttrAccessible as String] = nil
        SecItemDelete(removal as CFDictionary)
        #endif
    }

    private func directory() -> URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("ft-session-continuity-" + UUID().uuidString)
    }
    private func scope() throws -> TerminalSessionScope {
        try .init(workspaceID: UUID(), workspaceRevision: UUID(), machineID: "machine", routeID: "route",
                  transportID: UUID(), channelID: UUID())
    }
    private func expect(_ expected: TerminalSessionJournalError,
                        _ operation: () async throws -> Void,
                        file: StaticString = #filePath, line: UInt = #line) async {
        do { try await operation(); XCTFail("Expected a continuity refusal", file: file, line: line) }
        catch { XCTAssertEqual(error as? TerminalSessionJournalError, expected, file: file, line: line) }
    }

    func testFirstPublicationFailuresRecoverWithoutAuthorizingOrForgettingAShell() async throws {
        for target in 1...2 {
            for afterPublication in [false, true] {
                let root = directory(), identifier = "test-" + UUID().uuidString
                let files = try FileWorkspacePersistence(directory: root)
                let fault = SessionHistoryWriteFault(files: files, target: target,
                                                    afterPublication: afterPublication)
                let storage = try FileTerminalSessionHistoryPersistence(files: fault,
                    directory: root, identifier: identifier)
                let first = try TerminalSessionJournal(persistence: storage), connection = try scope()
                var shellRequests = 0
                await expect(.recoveryRequired) {
                    _ = try await first.beginCreation(scope: connection)
                    shellRequests += 1
                }
                XCTAssertEqual(shellRequests, 0)
                let markerPublished = target == 2 && afterPublication
                let bytes = try files.read(.catalog, limit: 512 * 1024)
                if target == 1 && !afterPublication {
                    XCTAssertNil(bytes)
                } else {
                    let object = try StrictJSON.decode(XCTUnwrap(bytes)).object()
                    XCTAssertEqual(object["version"], .number(markerPublished ? "2" : "1"))
                    XCTAssertEqual(try object.required("records").array().count, markerPublished ? 1 : 0)
                }

                // This owner knows no permission escaped. A completely written
                // candidate remains explicit review; a bootstrap is truly empty.
                let observed = try await first.records()
                XCTAssertEqual(observed.count, markerPublished ? 1 : 0)
                if markerPublished {
                    XCTAssertTrue(observed[0].canResolve)
                    await expect(.reviewRequired) { _ = try await first.beginCreation(scope: connection) }
                }

                let reopenedStorage = try FileTerminalSessionHistoryPersistence(directory: root, identifier: identifier)
                let reopened = try TerminalSessionJournal(persistence: reopenedStorage, processID: UUID())
                let recovery = try await reopened.records()
                XCTAssertEqual(recovery.map(\.id), observed.map(\.id))
                if markerPublished {
                    XCTAssertTrue(recovery[0].requiresReview)
                    await expect(.reviewRequired) { _ = try await reopened.beginCreation(scope: connection) }
                    try await reopened.resolve(recovery[0])
                }
                let next = try await reopened.beginCreation(scope: connection)
                XCTAssertNotEqual(next.id, observed.first?.id)
                let current = try await reopened.records()
                XCTAssertEqual(current.filter(\.requiresReview).map(\.id), [next.id])
                XCTAssertEqual(shellRequests, 0)
            }
        }
    }

    func testProtectedJournalReopensWithoutTreatingLiveOwnerAsInterrupted() async throws {
        let root = directory(), identifier = "test-" + UUID().uuidString
        let first = try TerminalSessionJournal(directory: root, identifier: identifier)
        let epoch = try await first.beginCreation(scope: scope())
        let reopened = try TerminalSessionJournal(directory: root, identifier: identifier)
        let records = try await reopened.records()
        XCTAssertEqual(records.map(\.id), [epoch.id])
        XCTAssertFalse(records[0].canResolve)
        await expect(.activeSession) { try await reopened.resolve(records[0]) }
        try await first.markInterrupted(epoch: epoch)
        let review = try await reopened.records()
        XCTAssertTrue(review[0].canResolve)
        try await reopened.resolve(review[0])
        let bytes = try Data(contentsOf: root.appendingPathComponent("catalog.json"))
        let attributes = try FileManager.default.attributesOfItem(atPath: root.appendingPathComponent("catalog.json").path)
        XCTAssertEqual((attributes[.posixPermissions] as? NSNumber)?.intValue, 0o600)
        XCTAssertEqual(try StrictJSON.decode(bytes).object()["version"], .number("2"))
    }

    func testMissingCatalogAfterRelaunchCannotAuthorizeAnEmptyHistory() async throws {
        let root = directory(), identifier = "test-" + UUID().uuidString
        let first = try TerminalSessionJournal(directory: root, identifier: identifier)
        _ = try await first.beginCreation(scope: scope())
        let catalog = root.appendingPathComponent("catalog.json")
        let preserved = root.appendingPathComponent("catalog-preserved.json")
        try FileManager.default.moveItem(at: catalog, to: preserved)
        let relaunched = try TerminalSessionJournal(directory: root, identifier: identifier)
        await expect(.historyMissing) { _ = try await relaunched.records() }
        await expect(.historyMissing) { _ = try await relaunched.beginCreation(scope: scope()) }
        XCTAssertFalse(FileManager.default.fileExists(atPath: catalog.path))
        XCTAssertTrue(FileManager.default.fileExists(atPath: preserved.path))
    }

    func testMissingWholeHistoryDirectoryIsDetectedByIndependentAnchor() async throws {
        let root = directory(), identifier = "test-" + UUID().uuidString
        let first = try TerminalSessionJournal(directory: root, identifier: identifier)
        _ = try await first.beginCreation(scope: scope())
        let preserved = root.appendingPathExtension("preserved")
        try FileManager.default.moveItem(at: root, to: preserved)
        let reopened = try TerminalSessionJournal(directory: root, identifier: identifier)
        await expect(.historyMissing) { _ = try await reopened.beginCreation(scope: scope()) }
        XCTAssertTrue(FileManager.default.fileExists(atPath: preserved.appendingPathComponent("catalog.json").path))
    }

    func testForeignOrCorruptHistoryIsPreservedAndCannotResetContinuity() async throws {
        for candidate in [Data("{".utf8), Data("{\"version\":4,\"workspaces\":[]}".utf8)] {
            let root = directory(), identifier = "test-" + UUID().uuidString
            let first = try TerminalSessionJournal(directory: root, identifier: identifier)
            _ = try await first.beginCreation(scope: scope())
            let catalog = root.appendingPathComponent("catalog.json")
            try FileManager.default.moveItem(at: catalog, to: root.appendingPathComponent("catalog-preserved.json"))
            try candidate.write(to: catalog, options: .atomic)
            let reopened = try TerminalSessionJournal(directory: root, identifier: identifier)
            await expect(candidate.count == 1 ? .corruptStore : .wrongNamespace) {
                _ = try await reopened.beginCreation(scope: scope())
            }
            XCTAssertEqual(try Data(contentsOf: catalog), candidate)
        }
    }

    func testReplacementCatalogWithDifferentIdentityCannotBorrowAnAnchor() async throws {
        let root = directory(), identifier = "test-" + UUID().uuidString
        let first = try TerminalSessionJournal(directory: root, identifier: identifier)
        _ = try await first.beginCreation(scope: scope())
        let catalog = root.appendingPathComponent("catalog.json")
        let old = try Data(contentsOf: catalog)
        var object = try StrictJSON.decode(old).object()
        object["id"] = .string(UUID().uuidString)
        let replacement = try StrictJSON.encode(.object(object))
        try FileManager.default.moveItem(at: catalog, to: root.appendingPathComponent("catalog-preserved.json"))
        try replacement.write(to: catalog, options: .atomic)
        let relaunched = try TerminalSessionJournal(directory: root, identifier: identifier)
        await expect(.corruptStore) { _ = try await relaunched.records() }
        XCTAssertEqual(try Data(contentsOf: catalog), replacement)
    }

    func testLegacyCatalogGainsContinuityWithoutDroppingItsUnresolvedRecord() async throws {
        let root = directory(), identifier = "test-" + UUID().uuidString
        let disk = StoreTestPersistence(), prototype = try TerminalSessionJournal(persistence: disk, processID: UUID())
        let epoch = try await prototype.beginCreation(scope: scope())
        var object = try StrictJSON.decode(XCTUnwrap(disk.bytes(.catalog))).object()
        object["version"] = .number("1")
        object["records"] = .array(try object.required("records").array().map {
            var fields = try $0.object(); fields["input_evidence"] = nil; return .object(fields)
        })
        let legacy = try StrictJSON.encode(.object(object)), files = try FileWorkspacePersistence(directory: root)
        try files.write(legacy, to: .catalog)
        let migrated = try TerminalSessionJournal(directory: root, identifier: identifier)
        let recovery = try await migrated.records()
        XCTAssertEqual(recovery.map(\.id), [epoch.id])
        XCTAssertTrue(recovery[0].requiresReview)
        XCTAssertTrue(recovery[0].canResolve)
        XCTAssertEqual(recovery[0].inputEvidence, [])
        try await migrated.resolve(recovery[0])
        let reopened = try TerminalSessionJournal(directory: root, identifier: identifier)
        let resolved = try await reopened.records()
        XCTAssertEqual(resolved[0].state, .acknowledged)
        let catalog = root.appendingPathComponent("catalog.json")
        try FileManager.default.moveItem(at: catalog, to: root.appendingPathComponent("catalog-preserved.json"))
        let missing = try TerminalSessionJournal(directory: root, identifier: identifier)
        await expect(.historyMissing) { _ = try await missing.records() }
    }

    #if canImport(Security)
    func testSameDeviceAnchorCannotCreateIndependentHistoryInAnotherDirectory() async throws {
        let firstRoot = directory(), secondRoot = directory(), identifier = "test-" + UUID().uuidString
        let first = try TerminalSessionJournal(directory: firstRoot, identifier: identifier)
        _ = try await first.beginCreation(scope: scope())
        let second = try TerminalSessionJournal(directory: secondRoot, identifier: identifier)
        await expect(.historyMissing) { _ = try await second.beginCreation(scope: scope()) }
        XCTAssertFalse(FileManager.default.fileExists(atPath: secondRoot.appendingPathComponent("catalog.json").path))
        let original = try await first.records()
        XCTAssertEqual(original.count, 1)
        XCTAssertTrue(original[0].requiresReview)
    }
    #else
    func testMissingOrCorruptPortableAnchorFailsClosedWithoutReplacingCatalog() async throws {
        for corruptAnchor in [Data?.none, Data("not-a-continuity-id".utf8)] {
            let root = directory(), identifier = "test-" + UUID().uuidString
            let journal = try TerminalSessionJournal(directory: root, identifier: identifier)
            _ = try await journal.beginCreation(scope: scope())
            let catalog = try Data(contentsOf: root.appendingPathComponent("catalog.json"))
            let anchor = root.deletingLastPathComponent().appendingPathComponent(root.lastPathComponent + ".anchor")
            try FileManager.default.moveItem(at: anchor, to: anchor.appendingPathExtension("preserved"))
            if let corruptAnchor { try corruptAnchor.write(to: anchor, options: .atomic) }
            let reopened = try TerminalSessionJournal(directory: root, identifier: identifier)
            await expect(corruptAnchor == nil ? .historyMissing : .corruptStore) {
                _ = try await reopened.beginCreation(scope: scope())
            }
            XCTAssertEqual(try Data(contentsOf: root.appendingPathComponent("catalog.json")), catalog)
        }
    }
    #endif
}
