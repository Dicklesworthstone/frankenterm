import Foundation

public enum TerminalPresentationVisibilityError: Error, Equatable, Sendable {
    case staleSource, retired
}

/// One latest-value owner for a terminal's model visibility. Navigation can
/// change its mind while a model transaction is suspended, without creating a
/// Task per gesture or dropping the final hide. This controls presentation only:
/// hiding never closes a channel, stops parsing, changes focus or resizes a PTY.
/// The native owner fences its view immediately, then submits the same intent
/// here. At most one apply call and one pending Boolean are retained.
@MainActor
public final class TerminalPresentationVisibility<Source: Equatable & Sendable> {
    public let source: Source
    public private(set) var requestedVisible: Bool
    public private(set) var appliedVisible: Bool?
    public private(set) var isRetired = false
    public private(set) var lastFailure: (any Error)?
    public var onFailure: (@MainActor (any Error) -> Void)?

    private let apply: @Sendable (Bool, Source) async throws -> Void
    private var generation = UUID()
    private var worker: Task<Void, Never>?

    /// initiallyVisible must match the model's initial presentation policy.
    public init(source: Source, initiallyVisible: Bool,
                apply: @escaping @Sendable (Bool, Source) async throws -> Void) {
        self.source = source; requestedVisible = initiallyVisible
        appliedVisible = initiallyVisible; self.apply = apply
    }

    public var isSettled: Bool { worker == nil }

    public func request(_ visible: Bool, source: Source) throws {
        guard source == self.source else { throw TerminalPresentationVisibilityError.staleSource }
        guard !isRetired else { throw TerminalPresentationVisibilityError.retired }
        guard requestedVisible != visible else { return }
        requestedVisible = visible; generation = UUID(); lastFailure = nil
        startWorker()
    }

    /// Errors do not create an automatic retry loop. Only a new intent or this
    /// explicit retry can repeat a failed, idempotent model-visibility operation.
    public func retry() {
        guard lastFailure != nil, worker == nil else { return }
        generation = UUID(); lastFailure = nil; startWorker()
    }

    /// Retirement is irreversible and requests a final hide. Do not cancel the
    /// in-flight apply: cancellation alone cannot prove whether it took effect.
    /// Its real completion is followed by hide, and all callers may join it.
    public func retire() {
        guard !isRetired else { return }
        isRetired = true; requestedVisible = false
        generation = UUID(); lastFailure = nil; startWorker()
    }

    public func waitUntilSettled() async {
        while let worker { await worker.value }
    }

    private func startWorker() {
        guard worker == nil, appliedVisible != requestedVisible else { return }
        worker = Task { await self.drive() }
    }

    private func drive() async {
        defer { worker = nil }
        while appliedVisible != requestedVisible {
            let visible = requestedVisible, request = generation
            do {
                try await apply(visible, source)
                appliedVisible = visible
                lastFailure = nil
            } catch {
                // A failed call may have changed model state before throwing.
                // Force the newest state through the boundary rather than
                // treating an obsolete failure as proof that nothing happened.
                appliedVisible = nil
                if generation != request { continue }
                lastFailure = error
                onFailure?(error)
                // A synchronous failure observer may retire or replace intent.
                // Honor that new state; otherwise stop, with no timer or retry.
                if generation == request { return }
            }
        }
    }
}

/// One latest-value owner for a terminal's selected history viewport. A pan
/// asks many times per second; at most one model call runs and only the
/// newest request waits behind it. Every request is applied even when it
/// equals an earlier one: the model may have moved the viewport since (input,
/// eviction), so a repeated value is not known to be in effect.
@MainActor
public final class TerminalHistorySelection<Source: Equatable & Sendable> {
    public let source: Source
    public private(set) var isRetired = false
    public var onFailure: (@MainActor (any Error) -> Void)?

    private let apply: @Sendable (Int64?, Source) async throws -> Void
    private var pending: Int64??
    private var worker: Task<Void, Never>?

    public init(source: Source, apply: @escaping @Sendable (Int64?, Source) async throws -> Void) {
        self.source = source; self.apply = apply
    }

    public var isSettled: Bool { worker == nil }

    /// `top` is the first stable row to show; nil is the live screen.
    public func request(top: Int64?, source: Source) throws {
        guard source == self.source else { throw TerminalPresentationVisibilityError.staleSource }
        guard !isRetired else { throw TerminalPresentationVisibilityError.retired }
        pending = .some(top)
        guard worker == nil else { return }
        worker = Task { await self.drive() }
    }

    /// Irreversible. Drops a waiting request; an in-flight call completes and
    /// callers may join it before releasing the model.
    public func retire() {
        isRetired = true; pending = nil
    }

    public func waitUntilSettled() async {
        while let worker { await worker.value }
    }

    private func drive() async {
        defer { worker = nil }
        while !isRetired, let next = pending {
            pending = nil
            do { try await apply(next, source) } catch {
                // No automatic retry: a later request is a new user intent.
                pending = nil
                if !isRetired { onFailure?(error) }
                return
            }
        }
    }
}
