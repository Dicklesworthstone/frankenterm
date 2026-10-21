import Foundation
import XCTest
@testable import FrankenTermKit

private actor NavigationTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    private var written = Data(), closed = false
    init(machine: String, workspace: String = "workspace", transport: UUID = UUID(), channel: UUID = UUID()) throws {
        identity = try .init(workspaceID: workspace, machineID: machine, routeID: "route", routeRevision: 1,
                             transportID: transport, channelID: channel)
    }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        guard !closed else { return .failed }
        let count = min(3, bytes.count)
        written += bytes.prefix(count)
        return .accepted(count)
    }
    func close() async { closed = true }
    func bytes() -> Data { written }
}

/// A deliberately synthetic encoder. The real keyboard, input coordinator,
/// navigation owner and handoff run unchanged; these bytes are not ANSI claims.
private actor NavigationEncoder: TerminalKeyboardEncoding {
    let source: TerminalSource
    private var pauseNext = false
    private var parked: CheckedContinuation<Void, Never>?
    private var reached: CheckedContinuation<Void, Never>?
    init(_ source: TerminalSource) { self.source = source }
    func pause() { pauseNext = true }
    func waitForPause() async {
        if parked != nil { return }
        await withCheckedContinuation { reached = $0 }
    }
    func release() { let value = parked; parked = nil; value?.resume() }
    func encode(_ intent: TerminalKeyboardIntent, source: TerminalSource, maximumBytes: Int) async throws -> Data {
        guard source == self.source else { throw TerminalInputError.stale }
        if pauseNext {
            pauseNext = false
            await withCheckedContinuation { continuation in
                parked = continuation
                let waiter = reached; reached = nil; waiter?.resume()
            }
        }
        let text: String
        switch intent {
        case .text(let value): text = value
        case .key(let key, let phase):
            text = "<\(phase == .up ? "up" : "down"):\(key.usage)>"
        case .releaseAll(let keys): text = keys.map { "<up:\($0.usage)>" }.joined()
        }
        let bytes = Data(text.utf8)
        guard bytes.count <= maximumBytes else { throw TerminalKeyboardError.capacity }
        return bytes
    }
}

@MainActor
final class TerminalKeyboardNavigationTests: XCTestCase {
    private struct Fixture {
        let input: TerminalInputCoordinator
        let owner: TerminalKeyboardNavigationOwner
        let navigation: TerminalNavigationCoordinator
        let transports: [NavigationTransport]
        let sources: [TerminalSource]
        let encoders: [NavigationEncoder]
        let bindings: [TerminalKeyboardNavigationBinding]
    }
    private func fixture(limits: TerminalInputLimits = .init()) async throws -> Fixture {
        let input = try TerminalInputCoordinator(limits: limits)
        var transports: [NavigationTransport] = [], sources: [TerminalSource] = []
        var encoders: [NavigationEncoder] = [], bindings: [TerminalKeyboardNavigationBinding] = []
        for machine in ["A", "A", "B"] {
            let transport = try NavigationTransport(machine: machine)
            let source = try await input.attach(transport), encoder = NavigationEncoder(source)
            transports.append(transport); sources.append(source); encoders.append(encoder)
            bindings.append(.init(source: source, encoder: encoder))
        }
        let a = try TerminalNavigationMachine(id: "A", name: "Cobalt", panes: bindings.prefix(2).map {
            try TerminalNavigationPane(id: $0.paneID, title: "pane")
        })
        let b = try TerminalNavigationMachine(id: "B", name: "Orchid", panes: [.init(id: bindings[2].paneID, title: "pane")])
        let offline = try TerminalNavigationMachine(id: "offline", name: "Offline", panes: [])
        let owner = try TerminalKeyboardNavigationOwner(coordinator: input, sceneID: UUID(), workspaceID: "workspace", bindings: bindings)
        let navigation = TerminalNavigationCoordinator(navigation: try .init(machines: [a, b, offline]), focusOwner: owner)
        navigation.resume()
        try await navigation.activateSelection()
        return .init(input: input, owner: owner, navigation: navigation, transports: transports, sources: sources,
                     encoders: encoders, bindings: bindings)
    }
    private func move(_ f: Fixture, axis: TerminalNavigationAxis) async throws {
        try await f.navigation.perform(XCTUnwrap(f.navigation.navigation.plan(axis: axis, direction: .next)))
    }
    private func drain(_ f: Fixture) async throws {
        for source in f.sources {
            var idle = false
            for _ in 0..<10000 {
                if try await f.input.drainOne(source) == .idle { idle = true; break }
            }
            XCTAssertTrue(idle)
        }
    }
    private func finish(_ f: Fixture) async throws {
        f.navigation.suspend(); await f.navigation.joinSuspension()
        for source in f.sources {
            try await f.input.disconnect(source)
            while !(try await f.input.takeOutcomes(source, maximum: 256)).isEmpty { }
            try await f.input.retire(source)
        }
        let usage = await f.input.accounting()
        XCTAssertEqual(usage.streams, 0); XCTAssertEqual(usage.connections, 0)
    }

    func testComposedUnicodeStaysOnOriginalSourceAndOldCallbacksCannotRetarget() async throws {
        let f = try await fixture(), original = try XCTUnwrap(f.owner.keyboard)
        try original.insertText("old:")
        try original.setMarkedText("候補🦀", selectedRange: NSRange(location: 4, length: 0))
        try await move(f, axis: .machine)
        let destination = try XCTUnwrap(f.owner.keyboard)
        XCTAssertNotEqual(destination.id, original.id)
        XCTAssertEqual(destination.originalFocus.source, f.sources[2])
        XCTAssertEqual(original.phase, .retired)
        XCTAssertThrowsError(try original.insertText("late"))
        try destination.insertText("new")
        await destination.waitUntilSettled()
        try await drain(f)
        let a = await f.transports[0].bytes(), b = await f.transports[2].bytes()
        XCTAssertEqual(a, Data("old:候補🦀".utf8)); XCTAssertEqual(b, Data("new".utf8))
        XCTAssertFalse(a.contains(13)); XCTAssertFalse(a.contains(10))
        try await finish(f)
    }

    func testHeldKeyCleanupUsesOldFIFOAndOfflineMachineCreatesNoKeyboard() async throws {
        let f = try await fixture(), original = try XCTUnwrap(f.owner.keyboard)
        try original.key(.init(usage: 0x50), phase: .down)
        await original.waitUntilSettled()
        try original.insertText("queued")
        try await move(f, axis: .pane)
        let next = try XCTUnwrap(f.owner.keyboard)
        try next.insertText("pane-two")
        await next.waitUntilSettled()
        try await move(f, axis: .machine)
        try await move(f, axis: .machine)
        XCTAssertNil(f.owner.keyboard)
        XCTAssertEqual(f.owner.currentSelection, .init(machineID: "offline", paneID: nil))
        try await drain(f)
        let first = await f.transports[0].bytes(), second = await f.transports[1].bytes()
        XCTAssertEqual(first, Data("<down:80>queued<up:80>".utf8))
        XCTAssertEqual(second, Data("pane-two".utf8))
        let usage = await f.input.accounting()
        XCTAssertEqual(usage.connections, 3); XCTAssertEqual(usage.streams, 1)
        try await finish(f)
    }

    func testSuspensionDuringCompositionEncodingCannotPublishAfterResume() async throws {
        let f = try await fixture(), old = try XCTUnwrap(f.owner.keyboard)
        try old.setMarkedText("never-migrate", selectedRange: NSRange(location: 13, length: 0))
        await old.waitUntilSettled()
        await f.encoders[0].pause()
        let plan = try XCTUnwrap(f.navigation.navigation.plan(axis: .machine, direction: .next))
        let switching = Task { try await f.navigation.perform(plan) }
        await f.encoders[0].waitForPause()
        f.navigation.suspend(); f.navigation.resume()
        XCTAssertNil(f.owner.keyboard)
        await f.encoders[0].release()
        do { try await switching.value; XCTFail("retired switch published") } catch { }
        await f.navigation.joinSuspension()
        XCTAssertEqual(f.navigation.navigation.selection?.machineID, "A")
        XCTAssertNil(f.owner.currentSelection)
        let usage = await f.input.accounting()
        XCTAssertEqual(usage.streams, 0)
        try await f.navigation.activateSelection()
        XCTAssertNotEqual(f.owner.keyboard?.id, old.id)
        try await drain(f)
        let bytes = await f.transports[0].bytes()
        XCTAssertTrue(bytes.isEmpty)
        try await finish(f)
    }

    func testFailedCleanupReservationKeepsOldKeyboardAndSelection() async throws {
        var limits = TerminalInputLimits(); limits.cleanupRecordsPerConnection = 1
        let f = try await fixture(limits: limits), old = try XCTUnwrap(f.owner.keyboard)
        try old.key(.init(usage: 4), phase: .down); await old.waitUntilSettled()
        _ = try await f.input.cleanup(old.originalFocus, encodedBytes: Data("reserved".utf8))
        do { try await move(f, axis: .machine); XCTFail("cleanup capacity bypassed") }
        catch { XCTAssertEqual(error as? TerminalInputError, .capacity) }
        XCTAssertTrue(f.owner.keyboard === old)
        XCTAssertEqual(f.navigation.navigation.selection?.machineID, "A")
        XCTAssertEqual(old.accounting.heldKeys, 1)
        try await drain(f)
        _ = try await f.input.takeOutcomes(f.sources[0])
        try await move(f, axis: .machine)
        try await drain(f)
        let bytes = await f.transports[0].bytes()
        XCTAssertEqual(bytes, Data("<down:4>reserved<up:4>".utf8))
        try await finish(f)
    }

    func testAbandonedPreparedFocusReleasesStreamWithoutClosingSources() async throws {
        let f = try await fixture()
        let destination = TerminalNavigationSelection(machineID: "B", paneID: f.bindings[2].paneID)
        let prepared = try await f.owner.prepare(destination)
        XCTAssertNil(f.owner.keyboard); XCTAssertNil(f.owner.currentSelection)
        XCTAssertThrowsError(try f.owner.publish(.init(destination: destination)))
        await f.owner.abandon(prepared)
        XCTAssertThrowsError(try f.owner.publish(prepared))
        let usage = await f.input.accounting()
        XCTAssertEqual(usage.streams, 0); XCTAssertEqual(usage.connections, 3)
        try await f.navigation.activateSelection()
        try await finish(f)
    }

    func testBindingReplacementAndCrossMachineSelectionCannotReuseAuthority() async throws {
        let f = try await fixture()
        let otherTransport = try NavigationTransport(machine: "A", transport: f.sources[0].identity.transportID,
                                                     channel: f.sources[0].identity.channelID)
        let otherCoordinator = try TerminalInputCoordinator()
        let foreign = try await otherCoordinator.attach(otherTransport)
        XCTAssertThrowsError(try f.owner.replaceBindings([.init(source: foreign, encoder: NavigationEncoder(foreign))]))
        do { _ = try await f.owner.prepare(.init(machineID: "B", paneID: f.bindings[0].paneID)); XCTFail("cross-machine source") }
        catch { XCTAssertEqual(error as? TerminalNavigationError, .unavailable) }
        XCTAssertEqual(f.owner.keyboard?.originalFocus.source, f.sources[0])
        try f.owner.replaceBindings(Array(f.bindings.dropFirst()))
        XCTAssertNil(f.owner.keyboard)
        await f.owner.joinSealed()
        let usage = await f.input.accounting()
        XCTAssertEqual(usage.streams, 0)
        try await otherCoordinator.disconnect(foreign); try await otherCoordinator.retire(foreign)
        try await finish(f)
    }

    func testDisconnectedDestinationCannotStealOldFocus() async throws {
        let f = try await fixture(), old = try XCTUnwrap(f.owner.keyboard)
        try await f.input.disconnect(f.sources[2])
        do { try await move(f, axis: .machine); XCTFail("disconnected destination enabled") }
        catch { XCTAssertEqual(error as? TerminalInputError, .unavailable) }
        XCTAssertTrue(f.owner.keyboard === old)
        XCTAssertEqual(f.owner.currentSelection?.machineID, "A")
        try old.insertText("still-A"); await old.waitUntilSettled()
        for _ in 0..<10 { if try await f.input.drainOne(f.sources[0]) == .idle { break } }
        let bytes = await f.transports[0].bytes()
        XCTAssertEqual(bytes, Data("still-A".utf8))
        try await finish(f)
    }

    func testRepeatedMachineAndPaneHandoffsDoNotLeakInputStreams() async throws {
        let f = try await fixture()
        var expected = Array(repeating: Data(), count: 3)
        for step in 0..<120 {
            let index = step % 3
            let plan = try f.navigation.navigation.plan(paneID: f.bindings[index].paneID)
            if let plan { try await f.navigation.perform(plan) }
            let keyboard = try XCTUnwrap(f.owner.keyboard)
            let text = "[\(step):界]"
            try keyboard.insertText(text); await keyboard.waitUntilSettled()
            expected[index] += Data(text.utf8)
            let usage = await f.input.accounting()
            XCTAssertEqual(usage.streams, 1)
            if step % 11 == 0 {
                try await drain(f)
                for source in f.sources { _ = try await f.input.takeOutcomes(source) }
            }
        }
        try await drain(f)
        for index in 0..<3 {
            let bytes = await f.transports[index].bytes()
            XCTAssertEqual(bytes, expected[index])
        }
        try await finish(f)
    }
}
