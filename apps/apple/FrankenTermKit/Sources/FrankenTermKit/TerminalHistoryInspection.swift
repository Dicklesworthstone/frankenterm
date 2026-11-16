import Foundation
#if canImport(SwiftUI)
import Observation
#endif

/// One bounded, read-only inspection of an exact live source. The UI retires
/// the terminal's responder before creating this owner. Search cannot send
/// input, open a connection or substitute another machine's history.
@MainActor
#if canImport(SwiftUI)
@Observable
#endif
public final class TerminalHistoryInspection: Identifiable {
    public let id = UUID()
    public let source: TerminalSource
    public let destination: String
    public var query = ""
    public private(set) var result: TerminalHistorySearchResult<TerminalSource>?
    public private(set) var preview: TerminalCopyDocument?
    public private(set) var previewID: UUID?
    public private(set) var selectedMatchID: UUID?
    public private(set) var failure: String?
    public private(set) var isBusy = false
    public private(set) var isClosing = false
    public private(set) var isPresented = true
    public private(set) var isUnavailable = false
    private let runtime: TerminalModelRuntime
    private let prepare: @MainActor () async -> Void
    // Observation serves the Apple UI only. The same owner state and lifecycle
    // run on other platforms without a dependency on a UI observation runtime.
    // These private fields are tracked like the rest: an @ObservationIgnored
    // inside `#if` is invisible to @Observable, whose accessors then collide
    // with its own (a Release build error), and no view reads them anyway.
    private var onClose: (@MainActor (UUID, TerminalHistoryBookmark?, String?) -> Void)?
    private var bookmark: TerminalHistoryBookmark?
    private var didReveal = false
    private var generation = UUID()
    private var work: Task<Void, Never>?
    private var closing: Task<Void, Never>?
    private var closed = false

    public init(runtime: TerminalModelRuntime, source: TerminalSource, destination: String,
                prepare: @escaping @MainActor () async -> Void,
                onClose: @escaping @MainActor (UUID, TerminalHistoryBookmark?, String?) -> Void) {
        self.runtime = runtime; self.source = source; self.destination = destination
        self.prepare = prepare; self.onClose = onClose
    }

    /// Explicit submission, never one detached search per keystroke. A cancelled
    /// operation retains the only worker slot until its canonical read settles.
    public func search() {
        guard work == nil, !isClosing, !isUnavailable, !closed else { return }
        let token = generation, query = query
        failure = nil; result = nil; preview = nil; previewID = nil; selectedMatchID = nil; isBusy = true
        work = Task { [weak self] in
            guard let self else { return }
            defer { self.work = nil; self.isBusy = false }
            do {
                await prepare()
                try Task.checkCancellation()
                if bookmark == nil { bookmark = try await runtime.captureHistoryBookmark(source: source) }
                let runtime = runtime, source = source
                let found = try await TerminalHistorySearch.search(query: query, source: source) { top in
                    try await runtime.captureSearchPage(top: top, source: source)
                }
                try Task.checkCancellation()
                guard generation == token, !isUnavailable else { return }
                result = found
            } catch is CancellationError {
                // Closing or retirement owns the next visible state.
            } catch {
                if generation == token, !isUnavailable { failure = Self.describe(error) }
            }
        }
    }

    public func reveal(_ match: TerminalHistoryMatch<TerminalSource>) {
        guard work == nil, !isClosing, !isUnavailable, !closed, bookmark != nil,
              result?.matches.contains(where: { $0.id == match.id }) == true else { return }
        let token = generation
        failure = nil; preview = nil; previewID = nil; selectedMatchID = nil; isBusy = true
        work = Task { [weak self] in
            guard let self else { return }
            defer { self.work = nil; self.isBusy = false }
            do {
                // Publication can fail or be cancelled after the canonical
                // viewport changes, so Done must restore even in that case.
                didReveal = true
                try await runtime.revealSearchMatch(match, source: source)
                let document = try await runtime.captureOutputDocument(source: source, liveViewport: false)
                try Task.checkCancellation()
                guard generation == token, !isUnavailable else { return }
                preview = document; previewID = UUID(); selectedMatchID = match.id
            } catch is CancellationError {
            } catch {
                if generation == token, !isUnavailable { failure = Self.describe(error) }
            }
        }
    }

    public func cancelSearch() { work?.cancel() }

    /// Retire synchronously on navigation, backgrounding or session end. Even
    /// an already closing owner is cancelled and joined before model disposal.
    public func retire() {
        guard !isUnavailable else { return }
        isUnavailable = true; isPresented = false; generation = UUID()
        work?.cancel(); closing?.cancel()
        query = ""; result = nil; preview = nil; previewID = nil; selectedMatchID = nil; failure = nil
        _ = beginClose(restoreViewport: false)
    }

    public func close(restoreViewport: Bool = true) async {
        if let job = beginClose(restoreViewport: restoreViewport) { await job.value }
    }

    public func waitUntilSettled() async {
        if let closing { await closing.value }
        else if let work { await work.value }
    }

    private func beginClose(restoreViewport: Bool) -> Task<Void, Never>? {
        if let closing { return closing }
        guard !closed else { return nil }
        isClosing = true; generation = UUID(); work?.cancel()
        let job = Task {
            if let work { await work.value }
            var problem: String?
            if !isUnavailable, restoreViewport, didReveal, let bookmark {
                do { try await runtime.restoreHistoryBookmark(bookmark, source: source) }
                catch { if !isUnavailable { problem = Self.describe(error) } }
            }
            let returning = !isUnavailable && !restoreViewport && didReveal ? bookmark : nil
            query = ""; result = nil; preview = nil; previewID = nil; selectedMatchID = nil; bookmark = nil
            closed = true; isPresented = false
            onClose?(id, returning, problem); onClose = nil
            closing = nil
        }
        closing = job
        return job
    }

    private static func describe(_ error: any Error) -> String {
        (error as? LocalizedError)?.errorDescription ?? String(describing: error)
    }
}
