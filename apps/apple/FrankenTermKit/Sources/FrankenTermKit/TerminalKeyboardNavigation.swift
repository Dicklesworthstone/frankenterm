import Foundation

/// Bind only an already-authenticated source to its canonical encoder. For a
/// direct SSH terminal, transport/channel UUIDs identify its exact incarnation.
/// This binding never creates a session or grants remote control authority.
public struct TerminalKeyboardNavigationBinding: Sendable {
    public let source: TerminalSource
    public let encoder: any TerminalKeyboardEncoding
    public var paneID: TerminalNavigationPaneID {
        .init(id: source.identity.channelID, incarnation: source.identity.transportID)
    }
    public init(source: TerminalSource, encoder: any TerminalKeyboardEncoding) {
        self.source = source; self.encoder = encoder
    }
}

/// The production navigation handoff uses the existing composition/held-key
/// owner and coordinator barrier. It does not encode ANSI, flush a socket, change
/// PTY geometry, close a connection, or select a source by a mutable title.
///
/// After coordinator.onChange, the scene installs keyboard in a matching native
/// terminal surface. The renderer's existing caret gate keeps that input closed
/// until a current frame supplies exact source-bound geometry.
@MainActor
public final class TerminalKeyboardNavigationOwner: TerminalNavigationFocusOwner {
    public private(set) var currentSelection: TerminalNavigationSelection?
    public private(set) var keyboard: TerminalKeyboardSession?
    private let coordinator: TerminalInputCoordinator
    private let sceneID: UUID, workspaceID: String
    private var bindings: [TerminalNavigationPaneID: TerminalKeyboardNavigationBinding]
    private var focus: TerminalFocus?
    private struct Pending {
        let preparation: TerminalNavigationPreparation
        let focus: TerminalFocus
        let binding: TerminalKeyboardNavigationBinding?
    }
    private var pending: Pending?
    private var generation = UUID(), preparing = false
    private var cleanup: (id: UUID, task: Task<Void, Never>)?

    public init(coordinator: TerminalInputCoordinator, sceneID: UUID, workspaceID: String,
                bindings: [TerminalKeyboardNavigationBinding]) throws {
        self.coordinator = coordinator; self.sceneID = sceneID; self.workspaceID = workspaceID
        self.bindings = try Self.checked(bindings, workspaceID: workspaceID)
    }
    private static func checked(_ values: [TerminalKeyboardNavigationBinding], workspaceID: String)
        throws -> [TerminalNavigationPaneID: TerminalKeyboardNavigationBinding] {
        guard !workspaceID.isEmpty, workspaceID.utf8.count <= 128, values.count <= 128 else {
            throw TerminalNavigationError.capacity
        }
        var result: [TerminalNavigationPaneID: TerminalKeyboardNavigationBinding] = [:]
        for value in values {
            guard value.source.identity.workspaceID == workspaceID, result[value.paneID] == nil else {
                throw TerminalNavigationError.invalidInventory
            }
            result[value.paneID] = value
        }
        return result
    }
    /// New bindings must be installed before inventory offers them. Removal of
    /// the visible source seals its input before publishing the changed list.
    /// Other sources' admitted input and transport ownership remain untouched.
    public func replaceBindings(_ values: [TerminalKeyboardNavigationBinding]) throws {
        let replacement = try Self.checked(values, workspaceID: workspaceID)
        guard !preparing, pending == nil else { throw TerminalNavigationError.busy }
        for (id, binding) in replacement {
            if let old = bindings[id], old.source != binding.source { throw TerminalNavigationError.stale }
        }
        if let id = currentSelection?.paneID, replacement[id] == nil { seal() }
        bindings = replacement
    }

    public func prepare(_ destination: TerminalNavigationSelection) async throws -> TerminalNavigationPreparation {
        guard !preparing, pending == nil, cleanup == nil else { throw TerminalNavigationError.busy }
        let binding: TerminalKeyboardNavigationBinding?
        if let id = destination.paneID {
            guard let value = bindings[id], value.source.identity.machineID == destination.machineID else {
                throw TerminalNavigationError.unavailable
            }
            binding = value
        } else { binding = nil }
        try Task.checkCancellation()
        preparing = true
        defer { preparing = false }
        let epoch = generation
        var newlyOwned: TerminalFocus?
        do {
            let next: TerminalFocus
            if let original = keyboard {
                next = try await original.handoff(to: binding?.source)
                // After success the old keyboard has relinquished its focus.
                // Do not install the destination responder until publish().
                if keyboard === original { keyboard = nil }
            } else {
                let initial: TerminalFocus
                if let focus { initial = focus }
                else {
                    initial = try await coordinator.makeStream(sceneID: sceneID)
                    newlyOwned = initial
                }
                next = try await coordinator.changeFocus(initial, to: binding?.source)
            }
            newlyOwned = next
            focus = nil; currentSelection = nil
            try Task.checkCancellation()
            guard epoch == generation else { throw TerminalNavigationError.stale }
            let preparation = TerminalNavigationPreparation(destination: destination)
            pending = .init(preparation: preparation, focus: next, binding: binding)
            newlyOwned = nil
            return preparation
        } catch {
            // A failed old-keyboard admission still owns its original focus and
            // draft. Only newly returned/unpublished handles belong to this path.
            if let newlyOwned { _ = try? await coordinator.suspend(newlyOwned) }
            if keyboard?.phase == .retired {
                keyboard = nil; focus = nil; currentSelection = nil
            }
            throw error
        }
    }

    public func publish(_ preparation: TerminalNavigationPreparation) throws {
        guard let value = pending, value.preparation == preparation, cleanup == nil else {
            throw TerminalNavigationError.stale
        }
        let newKeyboard = try value.binding.map {
            try TerminalKeyboardSession(focus: value.focus, coordinator: coordinator, encoder: $0.encoder)
        }
        // No client callback or suspension can interleave focus and identity.
        keyboard = newKeyboard; focus = value.focus
        currentSelection = preparation.destination; pending = nil
    }
    public func abandon(_ preparation: TerminalNavigationPreparation) async {
        guard let value = pending, value.preparation == preparation else { return }
        pending = nil
        retire(keyboard: nil, focus: value.focus)
        await joinSealed()
    }
    public func seal() {
        generation = UUID()
        let old = keyboard, unpublished = pending?.focus
        let orphan = old == nil ? focus : nil
        old?.suspend()
        keyboard = nil; focus = nil; pending = nil; currentSelection = nil
        retire(keyboard: old, focus: unpublished ?? orphan)
    }
    private func retire(keyboard: TerminalKeyboardSession?, focus: TerminalFocus?) {
        guard keyboard != nil || focus != nil else { return }
        let prior = cleanup?.task, id = UUID(), coordinator = self.coordinator
        let task = Task { [weak self] in
            if let prior { await prior.value }
            if let keyboard { await keyboard.waitUntilSuspended() }
            if let focus { _ = try? await coordinator.suspend(focus) }
            if self?.cleanup?.id == id { self?.cleanup = nil }
        }
        cleanup = (id, task)
    }
    public func joinSealed() async {
        while let current = cleanup { await current.task.value }
    }
}
