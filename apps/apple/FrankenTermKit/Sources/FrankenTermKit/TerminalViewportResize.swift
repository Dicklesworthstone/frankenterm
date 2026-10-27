import Foundation

/// Queue admission is not a remote acknowledgement. Completion arrives through
/// the source's ordered TerminalSessionUpdate.resized path after model.resize.
public enum TerminalResizeAdmission: Sendable { case queued, busy }
public enum TerminalViewportResizeError: Error, Equatable, Sendable { case staleSource, retired }

/// One scene-owned, latest-value resize driver. Source can be the coordinator's
/// TerminalSource; keeping it generic also allows exact actor-independent tests.
/// No timers, per-layout task queue, optimistic model resize, or SSH retry exist.
/// A changed layout during a queued request replaces ONE pending proposal.
@MainActor
public final class TerminalViewportResize<Source: Equatable & Sendable> {
    public let source: Source
    public private(set) var applied: TerminalSessionGeometry
    public private(set) var isRetired = false
    public var onFailure: (@MainActor (Error) -> Void)?
    public var isPending: Bool { flight != nil || desired != nil }

    private struct Flight {
        let id: UUID, geometry: TerminalSessionGeometry, progress: UInt64
        var admitted = false, completed = false
    }
    private let request: @Sendable (TerminalSessionGeometry) async throws -> TerminalResizeAdmission
    private var desired: TerminalSessionGeometry?
    private var flight: Flight?
    private var worker: Task<Void, Never>?
    private var waitingForProgress = false
    private var progress: UInt64 = 0

    public init(source: Source, initial: TerminalSessionGeometry,
                request: @escaping @Sendable (TerminalSessionGeometry) async throws -> TerminalResizeAdmission) {
        self.source = source; applied = initial; self.request = request
    }

    /// Synchronous UI entry: a layout storm changes one value, not task count.
    public func propose(_ geometry: TerminalSessionGeometry, source: Source) throws {
        try validate(source)
        desired = geometry == (flight?.geometry ?? applied) ? nil : geometry
        drive()
    }

    /// Call only after the canonical model has consumed the corresponding
    /// source-bound update. It may race the request's asynchronous return.
    /// A mux pane can take someone else's size before, or instead of, the one
    /// asked for. That settles the request too: waiting for the asked size
    /// would hold every later layout behind one that may never arrive.
    public func didApply(_ geometry: TerminalSessionGeometry, source: Source) throws {
        try validate(source)
        applied = geometry
        if flight != nil { flight?.completed = true }
        if flight?.admitted == true, flight?.completed == true { flight = nil }
        if desired == applied, flight == nil { desired = nil }
        signalProgress()
    }

    /// Allows one retry after a real service turn reports completion/progress.
    /// Never call this from a timer or a layout callback to defeat backpressure.
    public func serviceProgress(source: Source) throws {
        try validate(source)
        signalProgress()
    }

    private func signalProgress() {
        // Only equality with a flight's captured value matters. Wrapping cannot
        // alias in practice without 2^64 real service events during one request.
        progress &+= 1
        waitingForProgress = false
        drive()
    }

    private func validate(_ source: Source) throws {
        guard source == self.source else { throw TerminalViewportResizeError.staleSource }
        guard !isRetired else { throw TerminalViewportResizeError.retired }
    }

    private func drive() {
        guard !isRetired, worker == nil, flight == nil, !waitingForProgress,
              let geometry = desired else { return }
        if geometry == applied { desired = nil; return }
        desired = nil
        let id = UUID(), capturedProgress = progress, request = self.request
        flight = Flight(id: id, geometry: geometry, progress: capturedProgress)
        worker = Task { [weak self] in
            let result: Result<TerminalResizeAdmission, Error>
            do { try Task.checkCancellation(); result = .success(try await request(geometry)) }
            catch { result = .failure(error) }
            self?.settle(id: id, result: result)
        }
    }

    private func settle(id: UUID, result: Result<TerminalResizeAdmission, Error>) {
        worker = nil
        guard !isRetired, let owned = flight, owned.id == id else { return }
        switch result {
        case .success(.queued):
            flight?.admitted = true
            if owned.completed { flight = nil }
        case .success(.busy):
            flight = nil
            // A newer proposal supersedes the refused one. Keep the old value
            // only if no newer layout arrived, including a return to applied.
            if desired == nil { desired = owned.geometry }
            waitingForProgress = progress == owned.progress
        case .failure(let error):
            isRetired = true; flight = nil; desired = nil
            onFailure?(error)
            return
        }
        drive()
    }

    /// Retire synchronously before a scene/connection can change. A queued SSH
    /// resize is not undone; its owning connection still has to settle/close.
    public func retire() {
        guard !isRetired else { return }
        isRetired = true; desired = nil; flight = nil; onFailure = nil
        worker?.cancel()
    }

    /// Joins the one native-admission call, not the remote terminal's geometry.
    public func waitUntilSettled() async { if let worker { await worker.value } }
}
