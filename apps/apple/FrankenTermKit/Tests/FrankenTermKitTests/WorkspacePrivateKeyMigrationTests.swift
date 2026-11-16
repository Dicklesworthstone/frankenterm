import Foundation
import XCTest
@testable import FrankenTermKit

final class WorkspacePrivateKeyMigrationTests: XCTestCase {
    // Opaque synthetic bytes exercise publication and recovery, not SSH parsing
    // or native Keychain behavior. The production adapter validates both.
    private let accepted = Data("synthetic accepted private container".utf8)

    private enum Injected: Error, Equatable {
        case legacyLookup, beforePublication, afterPublication, verification, policy
        case beforeRetirement, afterRetirement
    }

    private final class Items {
        var legacy: Data?
        var current: Data?
        var events: [String] = []
        init(legacy: Data?, current: Data? = nil) { self.legacy = legacy; self.current = current }
    }

    /// Fresh adapters share only persisted items, so every failure test can
    /// simulate reopening without retaining the failed operation's state.
    private struct Storage: WorkspacePrivateKeyMigrationStorage {
        let items: Items
        var fault: Injected?
        func legacyExists() throws -> Bool {
            items.events.append("legacy lookup")
            if fault == .legacyLookup { throw Injected.legacyLookup }
            return items.legacy != nil
        }
        func publishCurrentIfAbsent(_ container: Data) throws {
            items.events.append("publish")
            if fault == .beforePublication { throw Injected.beforePublication }
            if items.current == nil { items.current = container }
            if fault == .afterPublication { throw Injected.afterPublication }
        }
        func readCurrent() throws -> Data? {
            items.events.append("verify")
            if fault == .verification { throw Injected.verification }
            if fault == .policy { throw Injected.policy }
            return items.current
        }
        func retireLegacy() throws {
            items.events.append("retire")
            if fault == .beforeRetirement { throw Injected.beforeRetirement }
            items.legacy = nil
            if fault == .afterRetirement { throw Injected.afterRetirement }
        }
    }

    private func migrate(_ items: Items, fault: Injected? = nil) throws {
        try WorkspacePrivateKeyMigration.migrate(accepted, maximumBytes: 64 * 1024,
            storage: Storage(items: items, fault: fault))
    }

    private func reopened(_ items: Items) -> (generation: WorkspacePrivateKeyGeneration, value: Data)? {
        WorkspacePrivateKeyGeneration.select { generation in
            generation == .current ? items.current : items.legacy
        }
    }

    func testPublishesAndVerifiesBeforeRetiringAcceptedKey() throws {
        let items = Items(legacy: accepted)
        try migrate(items)
        XCTAssertEqual(items.events, ["legacy lookup", "publish", "verify", "retire"])
        XCTAssertNil(items.legacy)
        XCTAssertEqual(items.current, accepted)
        XCTAssertEqual(reopened(items)?.generation, .current)
        XCTAssertEqual(reopened(items)?.value, accepted)
    }

    func testFailedLookupCannotPublishOrRetireAnything() {
        let items = Items(legacy: accepted)
        XCTAssertThrowsError(try migrate(items, fault: .legacyLookup)) {
            XCTAssertEqual($0 as? Injected, .legacyLookup)
        }
        XCTAssertEqual(items.events, ["legacy lookup"])
        XCTAssertEqual(items.legacy, accepted)
        XCTAssertNil(items.current)
    }

    func testFailedPublicationLeavesLegacyUsableAfterReopen() throws {
        let items = Items(legacy: accepted)
        XCTAssertThrowsError(try migrate(items, fault: .beforePublication))
        XCTAssertEqual(items.events, ["legacy lookup", "publish"])
        XCTAssertEqual(reopened(items)?.generation, .legacy)
        XCTAssertEqual(reopened(items)?.value, accepted)
        try migrate(items)
        XCTAssertNil(items.legacy)
        XCTAssertEqual(items.current, accepted)
    }

    func testPublicationThenErrorKeepsBothCopiesAndRetryDoesNotOverwrite() throws {
        let items = Items(legacy: accepted)
        XCTAssertThrowsError(try migrate(items, fault: .afterPublication)) {
            XCTAssertEqual($0 as? Injected, .afterPublication)
        }
        XCTAssertEqual(items.events, ["legacy lookup", "publish"])
        XCTAssertEqual(items.legacy, accepted)
        XCTAssertEqual(items.current, accepted)
        XCTAssertEqual(reopened(items)?.generation, .current)
        XCTAssertEqual(reopened(items)?.value, accepted)
        try migrate(items)
        XCTAssertNil(items.legacy)
        XCTAssertEqual(items.current, accepted)
    }

    func testReadbackOrAccessibilityFailureNeverRetiresLegacy() throws {
        for fault in [Injected.verification, .policy] {
            let items = Items(legacy: accepted)
            XCTAssertThrowsError(try migrate(items, fault: fault)) {
                XCTAssertEqual($0 as? Injected, fault)
            }
            XCTAssertEqual(items.events, ["legacy lookup", "publish", "verify"])
            XCTAssertEqual(items.legacy, accepted)
            XCTAssertEqual(items.current, accepted)
            try migrate(items)
            XCTAssertNil(items.legacy)
            XCTAssertEqual(items.current, accepted)
        }
    }

    func testExistingDifferentReplacementIsNeitherOverwrittenNorSelectedAsProof() {
        let different = Data("another private container".utf8)
        let items = Items(legacy: accepted, current: different)
        XCTAssertThrowsError(try migrate(items)) {
            XCTAssertEqual($0 as? WorkspacePrivateKeyMigrationError, .replacementMismatch)
        }
        XCTAssertEqual(items.events, ["legacy lookup", "publish", "verify"])
        XCTAssertEqual(items.legacy, accepted)
        XCTAssertEqual(items.current, different)
    }

    func testExistingIdenticalReplacementCanFinishInterruptedMigration() throws {
        let items = Items(legacy: accepted, current: accepted)
        try migrate(items)
        XCTAssertNil(items.legacy)
        XCTAssertEqual(items.current, accepted)
        // A second call after cleanup performs no add or delete.
        items.events = []
        try migrate(items)
        XCTAssertEqual(items.events, ["legacy lookup", "verify"])
        XCTAssertEqual(items.current, accepted)
    }

    func testCleanupFailureBeforeOrAfterEffectLeavesVerifiedCurrentKeyUsable() throws {
        for fault in [Injected.beforeRetirement, .afterRetirement] {
            let items = Items(legacy: accepted)
            XCTAssertThrowsError(try migrate(items, fault: fault)) {
                XCTAssertEqual($0 as? Injected, fault)
            }
            XCTAssertEqual(items.events, ["legacy lookup", "publish", "verify", "retire"])
            XCTAssertEqual(reopened(items)?.generation, .current)
            XCTAssertEqual(reopened(items)?.value, accepted)
            XCTAssertEqual(items.legacy, fault == .beforeRetirement ? accepted : nil)
            try migrate(items)
            XCTAssertNil(items.legacy)
            XCTAssertEqual(items.current, accepted)
        }
    }

    func testRemovedAccountCannotBeResurrectedFromPreviouslyUnlockedBytes() {
        let items = Items(legacy: nil)
        XCTAssertThrowsError(try migrate(items)) {
            XCTAssertEqual($0 as? WorkspacePrivateKeyMigrationError, .missingSource)
        }
        XCTAssertEqual(items.events, ["legacy lookup", "verify"])
        XCTAssertNil(items.legacy)
        XCTAssertNil(items.current)
        XCTAssertNil(reopened(items))
    }

    func testMissingLegacyDoesNotAuthorizeADifferentCurrentItem() {
        let other = Data("a different current item".utf8)
        let items = Items(legacy: nil, current: other)
        XCTAssertThrowsError(try migrate(items)) {
            XCTAssertEqual($0 as? WorkspacePrivateKeyMigrationError, .replacementMismatch)
        }
        XCTAssertEqual(items.events, ["legacy lookup", "verify"])
        XCTAssertEqual(items.current, other)
    }

    func testCurrentReadErrorCannotFallBackToLegacy() {
        var queried: [WorkspacePrivateKeyGeneration] = []
        XCTAssertThrowsError(try WorkspacePrivateKeyGeneration.select { generation -> Data? in
            queried.append(generation)
            if generation == .current { throw Injected.verification }
            return accepted
        }) {
            XCTAssertEqual($0 as? Injected, .verification)
        }
        XCTAssertEqual(queried, [.current])
    }

    func testCurrentAuthenticationRefusalRemainsBoundToCurrentGeneration() {
        // The signer carries native status as the selected value. A refusal is
        // present evidence, not the nil that means errSecItemNotFound.
        var queried: [WorkspacePrivateKeyGeneration] = []
        let chosen = WorkspacePrivateKeyGeneration.select { generation -> Int? in
            queried.append(generation)
            return generation == .current ? -25308 : 0
        }
        XCTAssertEqual(chosen?.generation, .current)
        XCTAssertEqual(chosen?.value, -25308)
        XCTAssertEqual(queried, [.current])
    }

    func testOnlyExplicitAbsenceSelectsLegacyAndServicesStayDistinct() {
        var queried: [WorkspacePrivateKeyGeneration] = []
        let chosen = WorkspacePrivateKeyGeneration.select { generation -> Data? in
            queried.append(generation)
            return generation == .current ? nil : accepted
        }
        XCTAssertEqual(queried, [.current, .legacy])
        XCTAssertEqual(chosen?.generation, .legacy)
        XCTAssertEqual(chosen?.value, accepted)
        XCTAssertNotEqual(WorkspacePrivateKeyGeneration.current.service, WorkspacePrivateKeyGeneration.legacy.service)
        XCTAssertEqual(WorkspacePrivateKeyGeneration.legacy.service, "org.frankenterm.mobile.workspace-keys.v1")
    }

    func testContainerBoundsRejectBeforeAnyStorageOperation() {
        for (container, limit) in [(Data(), 64), (accepted, 0), (accepted, accepted.count - 1)] {
            let items = Items(legacy: accepted)
            XCTAssertThrowsError(try WorkspacePrivateKeyMigration.migrate(container, maximumBytes: limit,
                storage: Storage(items: items))) {
                XCTAssertEqual($0 as? WorkspacePrivateKeyMigrationError, .invalidContainer)
            }
            XCTAssertTrue(items.events.isEmpty)
            XCTAssertEqual(items.legacy, accepted)
            XCTAssertNil(items.current)
        }
    }
}
