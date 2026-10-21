import Foundation

public enum TerminalConnectionBatchError: Error, Equatable, Sendable {
    case invalidLimits, capacity, duplicateRequest, retired, cleanupIncomplete
}

/// A bounded, foreground-owned batch of explicit connection requests. A ready
/// value is owned here until take transfers it to a terminal owner, or close
/// joins its disposal. This type never opens a shell, retries an attempt, changes
/// focus, or derives permission from imported preferences.
@MainActor
public final class TerminalConnectionBatch<ID: Hashable & Sendable, Resource: Sendable> {
    public enum State: Equatable, Sendable {
        case queued, connecting, ready, claimed, failed, cancelled, closing, cleanupFailed
    }
    public let id = UUID()
    public private(set) var states: [ID: State] = [:]
    public private(set) var isRetired = false
    public var onChange: (@MainActor () -> Void)?
    public var onFailure: (@MainActor (ID, any Error) -> Void)?

    private enum Kind { case connect, cleanup }
    private struct Job {
        let token: UUID, kind: Kind
        let task: Task<Void, Never>
    }
    private let maximumConcurrent: Int, maximumRequests: Int
    private let connect: @MainActor @Sendable (ID) async throws -> Resource
    private let dispose: @MainActor @Sendable (Resource) async throws -> Void
    private var queue: [ID] = []
    private var cleanupQueue: [ID] = []
    private var jobs: [ID: Job] = [:]
    private var resources: [ID: Resource] = [:]

    public init(maximumConcurrent: Int = 3, maximumRequests: Int = 128,
                connect: @escaping @MainActor @Sendable (ID) async throws -> Resource,
                dispose: @escaping @MainActor @Sendable (Resource) async throws -> Void) throws {
        guard (1...3).contains(maximumConcurrent), (1...128).contains(maximumRequests),
              maximumConcurrent <= maximumRequests else { throw TerminalConnectionBatchError.invalidLimits }
        self.maximumConcurrent = maximumConcurrent; self.maximumRequests = maximumRequests
        self.connect = connect; self.dispose = dispose
    }

    public var isBusy: Bool { !jobs.isEmpty || !queue.isEmpty || !cleanupQueue.isEmpty }
    public var activeCount: Int { jobs.count }
    public var queuedCount: Int { queue.count }
    public var readyCount: Int { states.values.filter { $0 == .ready }.count }
    public var retainedResourceCount: Int { resources.count }

    /// A new, deliberate user approval supplies this ordered inventory. Duplicate
    /// IDs or an excessive inventory fail atomically. Existing live requests,
    /// ready values and claimed values are not attempted twice. Failed/cancelled
    /// IDs may be requested again only by this explicit call after their old job
    /// has settled. A new consent never changes an in-flight request's closure.
    public func enqueue(_ ids: [ID]) throws {
        guard !isRetired else { throw TerminalConnectionBatchError.retired }
        guard ids.count <= maximumRequests else { throw TerminalConnectionBatchError.capacity }
        let unique = Set(ids)
        guard unique.count == ids.count else { throw TerminalConnectionBatchError.duplicateRequest }
        guard unique.filter({ states[$0] == nil }).count <= maximumRequests - states.count else {
            throw TerminalConnectionBatchError.capacity
        }
        for id in ids where jobs[id] == nil {
            switch states[id] {
            case nil, .failed, .cancelled:
                states[id] = .queued; queue.append(id)
            default: break
            }
        }
        schedule(); onChange?()
    }

    /// Selection promotes only already-approved queued work. Swiping cannot
    /// manufacture a request, restart a failure or duplicate a handshake.
    public func promote(_ id: ID) {
        guard !isRetired, let index = queue.firstIndex(of: id), index != 0 else { return }
        queue.remove(at: index); queue.insert(id, at: 0)
        onChange?()
    }

    /// Synchronous, one-shot ownership transfer. Cancellation/retirement cannot
    /// later dispose a value that a terminal owner has claimed.
    public func take(_ id: ID) -> Resource? {
        guard !isRetired, states[id] == .ready, let resource = resources.removeValue(forKey: id) else { return nil }
        states[id] = .claimed
        onChange?()
        return resource
    }

    /// Stop unsent and in-flight attempts, but preserve already-authenticated
    /// values for explicit use. Cancellation retains each worker reservation
    /// until it really settles. A success racing cancellation goes to cleanup.
    public func cancelPending() {
        for id in queue { states[id] = .cancelled }
        queue.removeAll()
        for job in jobs.values where job.kind == .connect { job.task.cancel() }
        onChange?()
    }

    /// Synchronous admission barrier. Disposal uses fresh, retained tasks, not
    /// the cancelled connection task: cleanup itself must not start cancelled.
    public func retire() {
        guard !isRetired else { return }
        isRetired = true
        for id in queue { states[id] = .cancelled }
        queue.removeAll()
        for job in jobs.values where job.kind == .connect { job.task.cancel() }
        for id in Array(resources.keys) where jobs[id] == nil && states[id] == .ready {
            states[id] = .closing; cleanupQueue.append(id)
        }
        schedule(); onChange?()
    }

    /// Joins all accepted work, including disposal of late successes. It never
    /// cancels the cleanup merely because its own caller is cancelled. Failed
    /// disposal retains the exact value and is retried by another close call.
    public func close() async throws {
        retire()
        for id in Array(resources.keys) where states[id] == .cleanupFailed && jobs[id] == nil {
            states[id] = .closing; cleanupQueue.append(id)
        }
        schedule()
        await waitUntilSettled()
        guard resources.isEmpty else { throw TerminalConnectionBatchError.cleanupIncomplete }
    }

    public func waitUntilSettled() async {
        while let job = jobs.values.first { await job.task.value }
    }

    private func schedule() {
        while jobs.count < maximumConcurrent {
            if !cleanupQueue.isEmpty {
                startCleanup(cleanupQueue.removeFirst())
            } else if !isRetired, !queue.isEmpty {
                startConnection(queue.removeFirst())
            } else { return }
        }
    }

    private func startConnection(_ id: ID) {
        let token = UUID(), connect = self.connect
        states[id] = .connecting
        let task = Task { [self] in
            let result: Result<Resource, any Error>
            do {
                try Task.checkCancellation()
                result = .success(try await connect(id))
            } catch { result = .failure(error) }
            finishConnection(id, token: token, result: result, cancelled: Task.isCancelled)
        }
        jobs[id] = Job(token: token, kind: .connect, task: task)
    }

    private func finishConnection(_ id: ID, token: UUID, result: Result<Resource, any Error>, cancelled: Bool) {
        // No retry is admitted while this exact job owns its slot.
        guard jobs[id]?.token == token else { return }
        jobs[id] = nil
        switch result {
        case let .success(resource):
            resources[id] = resource
            if isRetired || cancelled {
                states[id] = .closing; cleanupQueue.append(id)
            } else { states[id] = .ready }
        case let .failure(error):
            states[id] = isRetired || cancelled || error is CancellationError ? .cancelled : .failed
            if states[id] == .failed { onFailure?(id, error) }
        }
        schedule(); onChange?()
    }

    private func startCleanup(_ id: ID) {
        guard let resource = resources[id] else { return }
        let token = UUID(), dispose = self.dispose
        states[id] = .closing
        let task = Task { [self] in
            let failure: (any Error)?
            do { try await dispose(resource); failure = nil }
            catch { failure = error }
            guard jobs[id]?.token == token else { return }
            jobs[id] = nil
            if let failure {
                states[id] = .cleanupFailed
                onFailure?(id, failure)
            } else {
                resources[id] = nil; states[id] = .cancelled
            }
            schedule(); onChange?()
        }
        jobs[id] = Job(token: token, kind: .cleanup, task: task)
    }
}
