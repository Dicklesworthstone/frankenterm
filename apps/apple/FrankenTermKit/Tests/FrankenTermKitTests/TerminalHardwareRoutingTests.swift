import Foundation
import XCTest
@testable import FrankenTermKit

final class TerminalHardwareRoutingTests: XCTestCase {
    func testPrintablePhysicalKeysHaveOnlyTheNativeTextPath() {
        for usage in UInt16(0x04)...0x27 {
            XCTAssertEqual(TerminalHardwareRouting.destination(usage: usage, modifiers: [], hasMarkedText: false), .textSystem)
            XCTAssertEqual(TerminalHardwareRouting.destination(usage: usage, modifiers: [.shift], hasMarkedText: false), .textSystem)
        }
        XCTAssertEqual(TerminalHardwareRouting.destination(usage: 0x2C, modifiers: [], hasMarkedText: false), .textSystem)
    }
    func testControlNavigationAndFunctionKeysUseTheSharedEncoder() {
        for usage: UInt16 in [0x28, 0x29, 0x2A, 0x2B, 0x3A, 0x45, 0x49, 0x4A, 0x4F, 0x50, 0x52, 0x58, 0x68, 0x73] {
            XCTAssertEqual(TerminalHardwareRouting.destination(usage: usage, modifiers: [], hasMarkedText: false), .terminal)
        }
        XCTAssertEqual(TerminalHardwareRouting.destination(usage: 0x06, modifiers: [.control], hasMarkedText: false), .terminal)
        XCTAssertEqual(TerminalHardwareRouting.destination(usage: 0x05, modifiers: [.alt], hasMarkedText: false), .terminal)
    }
    func testIMEAndSystemShortcutsKeepOwnershipEvenForControlKeys() {
        for usage: UInt16 in [0x04, 0x28, 0x29, 0x2A, 0x4F, 0xE0] {
            XCTAssertEqual(TerminalHardwareRouting.destination(usage: usage, modifiers: [.control], hasMarkedText: true), .textSystem)
            XCTAssertEqual(TerminalHardwareRouting.destination(usage: usage, modifiers: [.command, .control], hasMarkedText: false), .textSystem)
        }
        for usage: UInt16 in [0, 0xE0, 0xE3, 0xE7, UInt16.max] {
            XCTAssertEqual(TerminalHardwareRouting.destination(usage: usage, modifiers: [.alt], hasMarkedText: false), .textSystem)
        }
        XCTAssertEqual(TerminalHardwareRouting.destination(usage: 0x2C, modifiers: [.control], hasMarkedText: false), .textSystem)
    }
    func testOptionCharacterModeLeavesDeadKeysAndNativeNavigationToTheTextSystem() throws {
        let preferences = TerminalHardwarePreferences(modifierMapping: .optionForCharacters)
        for usage: UInt16 in [0x08, 0x2A, 0x2C, 0x4F] {
            XCTAssertEqual(TerminalHardwareRouting.destination(usage: usage, modifiers: [.alt],
                hasMarkedText: false, preferences: preferences), .textSystem)
            XCTAssertThrowsError(try TerminalHardwareRouting.key(usage: usage, modifiers: [.alt],
                characters: "e", preferences: preferences))
        }
        XCTAssertEqual(TerminalHardwareRouting.destination(usage: 0x06, modifiers: [.control],
            hasMarkedText: false, preferences: preferences), .terminal)
        let chord = try TerminalHardwareRouting.key(usage: 0x06, modifiers: [.control, .alt],
            characters: "c", preferences: preferences)
        XCTAssertEqual(chord.modifiers, [.control, .alt])
    }
    func testSwappedModifiersPreserveTheLayoutAndDoNotSwapLabelledHelperKeys() throws {
        let preferences = TerminalHardwarePreferences(modifierMapping: .swapControlAndOption)
        let control = try TerminalHardwareRouting.key(usage: 0x06, modifiers: [.alt, .shift, .capsLock],
            characters: "C", preferences: preferences)
        XCTAssertEqual(control.usage, 0x06); XCTAssertEqual(control.characters, "C")
        XCTAssertEqual(control.modifiers, [.control, .shift, .capsLock])
        let meta = try TerminalHardwareRouting.key(usage: 0x05, modifiers: [.control],
            characters: "b", preferences: preferences)
        XCTAssertEqual(meta.modifiers, [.alt])
        let both = try TerminalHardwareRouting.key(usage: 0x05, modifiers: [.control, .alt],
            characters: "b", preferences: preferences)
        XCTAssertEqual(both.modifiers, [.control, .alt])
        let helper = try TerminalHardwareRouting.key(usage: 0x4F, modifiers: [], characters: "",
            preferences: preferences, accessoryModifiers: [.control])
        XCTAssertEqual(helper.modifiers, [.control], "the visible Ctrl helper still means Control")
    }
    func testBackspacePreferenceUsesCanonicalControlBackspaceWithTheSameHeldKeyIdentity() throws {
        let preferences = TerminalHardwarePreferences(backspace: .controlBackspace)
        let normal = try TerminalHardwareRouting.key(usage: 0x2A, modifiers: [], characters: "",
            preferences: .init())
        let mapped = try TerminalHardwareRouting.key(usage: 0x2A, modifiers: [.alt], characters: "",
            preferences: preferences)
        XCTAssertEqual(normal.usage, 0x2A); XCTAssertTrue(normal.modifiers.isEmpty)
        XCTAssertEqual(mapped.usage, normal.usage)
        XCTAssertEqual(mapped.modifiers, [.control, .alt])
        let forwardDelete = try TerminalHardwareRouting.key(usage: 0x4C, modifiers: [], characters: "",
            preferences: preferences)
        XCTAssertTrue(forwardDelete.modifiers.isEmpty, "the separate forward Delete key is unchanged")
    }
    func testEveryMappingPreservesPhysicalSystemShortcutsAndIMEBeforeRemapping() {
        for mapping in TerminalHardwarePreferences.ModifierMapping.allCases {
            let preferences = TerminalHardwarePreferences(modifierMapping: mapping, backspace: .controlBackspace)
            for usage: UInt16 in [0x06, 0x2A, 0x2C, 0x50] {
                XCTAssertEqual(TerminalHardwareRouting.destination(usage: usage, modifiers: [.command, .alt],
                    hasMarkedText: false, preferences: preferences), .textSystem)
                XCTAssertEqual(TerminalHardwareRouting.destination(usage: usage, modifiers: [.control, .alt],
                    hasMarkedText: true, preferences: preferences), .textSystem)
            }
            XCTAssertEqual(TerminalHardwareRouting.destination(usage: 0x2C, modifiers: [.control],
                hasMarkedText: false, preferences: preferences), .textSystem)
            XCTAssertThrowsError(try TerminalHardwareRouting.key(usage: 0x2C, modifiers: [.control],
                characters: " ", preferences: preferences))
            XCTAssertThrowsError(try TerminalHardwareRouting.key(usage: 0x06, modifiers: [.command],
                characters: "c", preferences: preferences))
        }
    }
    func testHardwarePreferencesPersistTogetherAndCanReturnToCurrentDefaults() throws {
        let name = "TerminalHardwarePreferencesTests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: name))
        defer { defaults.removePersistentDomain(forName: name) }
        let store = TerminalHardwarePreferencesStore(defaults: defaults)
        XCTAssertEqual(store.current, .init())
        let selected = TerminalHardwarePreferences(modifierMapping: .swapControlAndOption, backspace: .controlBackspace)
        try store.save(selected)
        XCTAssertEqual(store.current, selected)
        XCTAssertEqual(TerminalHardwarePreferencesStore(defaults: defaults).current, selected)
        try store.save(.init())
        XCTAssertEqual(TerminalHardwarePreferencesStore(defaults: defaults).current, .init())
    }
    func testInvalidOrFutureStoredPreferencesCannotSelectAnUnexpectedMapping() throws {
        let name = "TerminalHardwarePreferencesTests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: name))
        defer { defaults.removePersistentDomain(forName: name) }
        for bytes in [
            Data(#"{"version":2,"preferences":{"modifierMapping":"swapControlAndOption","backspace":"controlBackspace"}}"#.utf8),
            Data(#"{"version":1,"preferences":{"modifierMapping":"unknown","backspace":"delete"}}"#.utf8),
            Data(#"{"version":1,"preferences":{"backspace":"controlBackspace"}}"#.utf8),
            Data(repeating: 0x20, count: 1025)
        ] {
            defaults.set(bytes, forKey: TerminalHardwarePreferencesStore.storageKey)
            XCTAssertEqual(TerminalHardwarePreferencesStore(defaults: defaults).current, .init())
        }
    }
    func testSoftwareHelperChordsKeepLogicalCharactersAndModifiers() throws {
        let controlC = try TerminalHardwareRouting.accessoryKey(text: "c", modifiers: [.control])
        XCTAssertEqual(controlC.usage, 0x06); XCTAssertEqual(controlC.characters, "c")
        XCTAssertEqual(controlC.modifiers, [.control])
        let controlQuestion = try TerminalHardwareRouting.accessoryKey(text: "?", modifiers: [.control])
        XCTAssertEqual(controlQuestion.usage, 0x38); XCTAssertEqual(controlQuestion.modifiers, [.control, .shift])
        let altCapital = try TerminalHardwareRouting.accessoryKey(text: "B", modifiers: [.alt])
        XCTAssertEqual(altCapital.usage, 0x05); XCTAssertEqual(altCapital.modifiers, [.alt, .shift])
        XCTAssertEqual(try TerminalHardwareRouting.accessoryKey(text: "\n", modifiers: [.alt]).usage, 0x28)
        XCTAssertEqual(try TerminalHardwareRouting.accessoryKey(text: " ", modifiers: [.control]).usage, 0x2C)
    }
    func testUnsupportedHelperTextDoesNotFallBackToUnmodifiedTyping() {
        for text in ["", "hello", "界", "🦀", "e\u{301}", "\u{1B}", "\u{7F}"] {
            XCTAssertThrowsError(try TerminalHardwareRouting.accessoryKey(text: text, modifiers: [.control]))
        }
        XCTAssertThrowsError(try TerminalHardwareRouting.accessoryKey(text: "c", modifiers: [.command]))
    }
    func testOneShotAndLockedModifiersHaveDistinctLifetime() {
        var modifiers = TerminalAccessoryModifiers()
        modifiers.cycleControl(); XCTAssertEqual(modifiers.control, .oneShot)
        modifiers.cycleAlt(); modifiers.cycleAlt(); XCTAssertEqual(modifiers.alt, .locked)
        XCTAssertEqual(modifiers.modifiers, [.control, .alt])
        modifiers.consume(); XCTAssertEqual(modifiers.control, .off); XCTAssertEqual(modifiers.alt, .locked)
        modifiers.cycleAlt(); XCTAssertTrue(modifiers.modifiers.isEmpty)
        modifiers.cycleControl(); modifiers.cycleControl(); modifiers.clear()
        XCTAssertEqual(modifiers, TerminalAccessoryModifiers())
    }
}

private actor NativeInputTestTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    private var output = Data()
    init() throws {
        identity = try TerminalConnectionIdentity(workspaceID: "workspace", machineID: "machine", routeID: "route",
                                                  routeRevision: 1, transportID: UUID(), channelID: UUID())
    }
    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        let count = min(bytes.count, 2); output += bytes.prefix(count); return .accepted(count)
    }
    func close() async {}
    func bytes() -> Data { output }
}

/// Synthetic encoder makes mode selection and FIFO order independently visible.
/// Production is intentionally unavailable unless its canonical core qualifies it.
private actor NativeInputTestEncoder: TerminalKeyboardEncoding {
    nonisolated let supportsPaste: Bool
    private var bracketed = true
    private var preparations = 0
    init(supportsPaste: Bool = true) { self.supportsPaste = supportsPaste }
    func setBracketed(_ enabled: Bool) { bracketed = enabled }
    func preparationCount() -> Int { preparations }
    func encode(_ intent: TerminalKeyboardIntent, source: TerminalSource, maximumBytes: Int) async throws -> Data {
        let value: String
        switch intent {
        case .text(let text): value = text
        case .key(let key, _): value = "<key:\(key.usage)>"
        case .releaseAll(let keys): value = keys.map { "<release:\($0.usage)>" }.joined()
        }
        guard value.utf8.count <= maximumBytes else { throw TerminalKeyboardError.capacity }
        return Data(value.utf8)
    }
    func preparePaste(_ text: String, source: TerminalSource, maximumBytes: Int) async throws -> TerminalPreparedPaste {
        preparations += 1
        guard supportsPaste, text.utf8.count <= maximumBytes else { throw TerminalKeyboardError.unavailable }
        // The UI never chooses this setting. This controlled core can change it
        // before admission, exactly as a remote application's mode change can.
        return TerminalPreparedPaste(text: text.replacingOccurrences(of: "\r\n", with: "\n"), bracketed: bracketed)
    }
}

@MainActor
final class TerminalNativeInputAdmissionTests: XCTestCase {
    private final class ObserverToken { var value: UUID? }
    private func setup(supportsPaste: Bool = true, limits: TerminalInputLimits = TerminalInputLimits()) async throws
        -> (TerminalInputCoordinator, NativeInputTestTransport, TerminalSource, NativeInputTestEncoder, TerminalKeyboardSession) {
        let coordinator = try TerminalInputCoordinator(limits: limits)
        let transport = try NativeInputTestTransport()
        let source = try await coordinator.attach(transport)
        let stream = try await coordinator.makeStream(sceneID: UUID())
        let focus = try await coordinator.changeFocus(stream, to: source)
        let encoder = NativeInputTestEncoder(supportsPaste: supportsPaste)
        let session = try TerminalKeyboardSession(focus: focus, coordinator: coordinator, encoder: encoder)
        return (coordinator, transport, source, encoder, session)
    }
    private func drain(_ coordinator: TerminalInputCoordinator, _ source: TerminalSource) async throws {
        for _ in 0..<10_000 {
            if try await coordinator.drainOne(source) == .idle { return }
        }
        XCTFail("synthetic transport failed to drain within its explicit bound")
    }
    func testReviewRefusesControlInjectionAndExcessBytesBeforeItIsQueued() async throws {
        let (_, _, _, _, session) = try await setup()
        for text in ["", "\u{1B}[201~whoami", "\u{0000}", "\u{0085}", "\u{7F}", String(repeating: "a", count: 32_769)] {
            XCTAssertThrowsError(try TerminalPasteReview(text: text, keyboardID: session.id, focus: session.originalFocus))
        }
        let review = try TerminalPasteReview(text: "界\tline\n", keyboardID: session.id, focus: session.originalFocus)
        XCTAssertTrue(review.containsLineBreaks); XCTAssertEqual(review.utf8Bytes, 9)
        XCTAssertFalse(String(reflecting: review).contains("line"))
        XCTAssertEqual(session.accounting.pendingEvents, 0)
        session.suspend(); await session.waitUntilSuspended()
    }
    func testReviewedPasteSharesFIFOAndUsesCoreModeAndNewlinePolicy() async throws {
        let (coordinator, transport, source, encoder, session) = try await setup()
        try session.insertText("before")
        let review = try TerminalPasteReview(text: "a\r\nb", keyboardID: session.id, focus: session.originalFocus)
        try session.paste(review)
        try session.insertText("after")
        await session.waitUntilSettled(); try await drain(coordinator, source)
        let actual = await transport.bytes()
        XCTAssertEqual(actual, Data("before\u{1B}[200~a\nb\u{1B}[201~after".utf8))
        let preparations = await encoder.preparationCount(); XCTAssertEqual(preparations, 1)
        session.suspend(); await session.waitUntilSuspended()
    }
    func testUIHasNoBracketFlagAndUnqualifiedEncoderCannotAdmitPaste() async throws {
        let (coordinator, transport, source, encoder, session) = try await setup(supportsPaste: false)
        XCTAssertFalse(session.acceptsPaste)
        let review = try TerminalPasteReview(text: "echo", keyboardID: session.id, focus: session.originalFocus)
        XCTAssertThrowsError(try session.paste(review))
        try session.insertText("ordinary")
        await session.waitUntilSettled(); try await drain(coordinator, source)
        let actual = await transport.bytes(); XCTAssertEqual(actual, Data("ordinary".utf8))
        let preparations = await encoder.preparationCount(); XCTAssertEqual(preparations, 0)
        session.suspend(); await session.waitUntilSuspended()
    }
    func testCoreCanSelectUnbracketedPasteWithoutChangingReviewedDestination() async throws {
        let (coordinator, transport, source, encoder, session) = try await setup()
        await encoder.setBracketed(false)
        let review = try TerminalPasteReview(text: "literal", keyboardID: session.id, focus: session.originalFocus)
        try session.paste(review); await session.waitUntilSettled(); try await drain(coordinator, source)
        let actual = await transport.bytes(); XCTAssertEqual(actual, Data("literal".utf8))
        session.suspend(); await session.waitUntilSuspended()
    }
    func testReviewCannotMoveToAnotherResponderOrCrossComposition() async throws {
        let (_, _, _, _, source) = try await setup()
        let (_, _, _, _, target) = try await setup()
        let review = try TerminalPasteReview(text: "sensitive", keyboardID: source.id, focus: source.originalFocus)
        XCTAssertThrowsError(try target.paste(review))
        try source.setMarkedText("候補", selectedRange: NSRange(location: 2, length: 0))
        XCTAssertFalse(source.acceptsPaste); XCTAssertThrowsError(try source.paste(review))
        source.suspend(); target.suspend()
        await source.waitUntilSuspended(); await target.waitUntilSuspended()
        XCTAssertThrowsError(try source.paste(review))
    }
    func testObserversAreBoundedAndRemovalDuringNotificationIsSafe() async throws {
        let (_, _, _, _, session) = try await setup()
        var calls = 0
        let token = ObserverToken()
        token.value = try session.observeState {
            calls += 1
            if let value = token.value { session.removeStateObserver(value) }
        }
        var others: [UUID] = []
        for _ in 0..<7 { others.append(try session.observeState {}) }
        XCTAssertThrowsError(try session.observeState {})
        try session.insertText("a"); await session.waitUntilSettled()
        XCTAssertEqual(calls, 1)
        try session.insertText("b"); await session.waitUntilSettled()
        XCTAssertEqual(calls, 1)
        for token in others { session.removeStateObserver(token) }
        var retired = false
        let final = try session.observeState { if session.phase == .retired { retired = true } }
        session.suspend(); await session.waitUntilSuspended()
        XCTAssertTrue(retired); session.removeStateObserver(final)
    }
    func testMarkedTextReleasesHeldKeysBeforeCompositionAndIgnoresLateKeyUp() async throws {
        let (coordinator, transport, source, _, session) = try await setup()
        let key = try TerminalHardwareKey(usage: 0x06, modifiers: [.control], characters: "c")
        try session.key(key, phase: .down)
        try session.setMarkedText("你", selectedRange: NSRange(location: 1, length: 0))
        try session.key(key, phase: .up)
        try session.unmarkText()
        await session.waitUntilSettled(); try await drain(coordinator, source)
        let actual = await transport.bytes()
        XCTAssertEqual(actual, Data("<key:6><release:6>你".utf8))
        XCTAssertEqual(session.phase, .active); XCTAssertEqual(session.accounting.heldKeys, 0)
        let outcomes = try await coordinator.takeOutcomes(source)
        XCTAssertEqual(outcomes.filter { $0.kind == .cleanup }.count, 1)
        session.suspend(); await session.waitUntilSuspended()
    }
    func testFailedCompositionCleanupKeepsHeldKeyOwnershipUntilExplicitRetry() async throws {
        var limits = TerminalInputLimits()
        limits.cleanupBytesPerConnection = 16; limits.maximumCleanupBytes = 16
        let (coordinator, transport, source, _, session) = try await setup(limits: limits)
        _ = try await coordinator.cleanup(session.originalFocus, encodedBytes: Data(repeating: 0x58, count: 16))
        let key = try TerminalHardwareKey(usage: 0x06)
        try session.key(key, phase: .down); await session.waitUntilSettled()
        try session.setMarkedText("你", selectedRange: NSRange(location: 1, length: 0))
        await session.waitUntilSettled()
        XCTAssertEqual(session.phase, .blocked); XCTAssertEqual(session.accounting.heldKeys, 1)
        try await drain(coordinator, source)
        try session.retryLocalAdmission(); await session.waitUntilSettled()
        XCTAssertEqual(session.phase, .active); XCTAssertEqual(session.accounting.heldKeys, 0)
        try session.unmarkText(); await session.waitUntilSettled(); try await drain(coordinator, source)
        let actual = await transport.bytes()
        XCTAssertEqual(actual, Data((String(repeating: "X", count: 16) + "<key:6><release:6>你").utf8))
        session.suspend(); await session.waitUntilSuspended()
    }
    func testCleanupLaneRejectsStaleFocusAndDoesNotFinishComposition() async throws {
        let (coordinator, _, source, _, session) = try await setup()
        let composition = try await coordinator.beginComposition(session.originalFocus)
        _ = try await coordinator.cleanup(session.originalFocus, encodedBytes: Data("release".utf8))
        do {
            _ = try await coordinator.submit(session.originalFocus, encodedBytes: Data("must stay blocked".utf8))
            XCTFail("cleanup incorrectly settled the composition")
        } catch { XCTAssertEqual(error as? TerminalInputError, .compositionPending) }
        try await coordinator.cancelComposition(composition)
        let next = try await coordinator.changeFocus(session.originalFocus, to: source)
        do {
            _ = try await coordinator.cleanup(session.originalFocus, encodedBytes: Data("stale".utf8))
            XCTFail("cleanup accepted a retired focus generation")
        } catch { XCTAssertEqual(error as? TerminalInputError, .stale) }
        _ = try await coordinator.suspend(next)
        session.suspend(); await session.waitUntilSuspended()
    }
}
