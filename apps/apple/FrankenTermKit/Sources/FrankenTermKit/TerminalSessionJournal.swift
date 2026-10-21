import Foundation

/// Opaque local references only. These identify the attempted connection; they
/// never restore an authenticated transport, shell, input lease, or remote PTY.
public struct TerminalSessionScope: Hashable, Sendable {
    public let workspaceID: UUID
    public let workspaceRevision: UUID
    public let machineID: String
    public let routeID: String
    public let transportID: UUID
    public let channelID: UUID

    public init(workspaceID: UUID, workspaceRevision: UUID, machineID: String,
                routeID: String, transportID: UUID, channelID: UUID) throws {
        do {
            _ = try WorkspaceDocument.identifier(machineID)
            _ = try WorkspaceDocument.identifier(routeID)
        } catch { throw TerminalSessionJournalError.invalidScope }
        self.workspaceID = workspaceID; self.workspaceRevision = workspaceRevision
        self.machineID = machineID; self.routeID = routeID
        self.transportID = transportID; self.channelID = channelID
    }
}

/// Issued only after the creation marker's atomic, durable write succeeds.
/// Deliberately not Codable and not constructible by a caller. Possession does
/// not authorize replay or another shell request; the connection owner must
/// consume it for at most one explicit creation attempt.
public struct TerminalSessionEpoch: Hashable, Sendable {
    public let id: UUID
    public let scope: TerminalSessionScope
    fileprivate let issuer: UUID
}

public enum TerminalSessionRecordState: String, Sendable {
    case creationStarted
    case interrupted
    /// An explicit acknowledgement of possible unknown remote effects. This
    /// does not claim that input was delivered or that the shell has exited.
    case acknowledged
}

/// A revision-bound review snapshot, not a durable permission or live handle.
/// Request a fresh snapshot after a failed write or a stale-review result.
public struct TerminalSessionRecord: Identifiable, Hashable, Sendable {
    public let id: UUID
    public let scope: TerminalSessionScope
    public let state: TerminalSessionRecordState
    public let canResolve: Bool
    public var requiresReview: Bool { state != .acknowledged }
    fileprivate let revision: UUID
    fileprivate let reviewer: UUID
}

public enum TerminalSessionJournalError: Error, Equatable, Sendable, LocalizedError {
    case invalidScope, invalidLimits, wrongNamespace, corruptStore, busy
    case capacity, reviewRequired, activeSession, staleReview, invalidEpoch
    case recoveryRequired

    public var errorDescription: String? {
        switch self {
        case .invalidScope: return "The session's local identity is invalid."
        case .invalidLimits: return "The session history limits are invalid."
        case .wrongNamespace: return "Session history needs its own protected storage directory."
        case .corruptStore: return "Session history could not be validated. No new shell was authorized."
        case .busy: return "Another session history transaction is active."
        case .capacity: return "Review unresolved session history before creating more sessions."
        case .reviewRequired: return "An earlier session for this machine may have unknown effects. Review it before creating a new session."
        case .activeSession: return "Close the current local session before resolving its history."
        case .staleReview: return "Session history changed. Review the current record again."
        case .invalidEpoch: return "This session marker belongs to another owner."
        case .recoveryRequired: return "Session history storage needs recovery. Do not assume the operation was rolled back."
        }
    }
}

private enum TerminalSessionProcess {
    /// Shared by every journal instance in this process. Another instance must
    /// not mistake a currently owned creation marker for a prior-launch record.
    static let id = UUID()
}

/// App-owned gate for explicit session creation, shared by all scenes.
///
/// Use a DEDICATED WorkspacePersistence namespace, never a WorkspaceStore's
/// directory. FileWorkspacePersistence supplies protected atomic writes and
/// directory fsync. The journal validates its format and rejects any import
/// journal before writing, including an accidentally supplied workspace store.
///
/// After authentication, call beginCreation immediately before the first remote
/// shell/exec request. Failure returns no epoch and forbids that request. Once
/// returned, an epoch must remain owned until admission is sealed and native
/// connection teardown has joined; then call markInterrupted, including on
/// cancellation and ordinary close. The scene separately retains and joins its
/// pump consumer and outcome delivery. There is no implicit "clean" close: SSH
/// delivery and local close cannot prove which remote effects occurred.
///
/// Unresolved records block another creation for the same installed workspace
/// and machine, including route/revision changes and relaunch. Only explicit
/// review resolution permits a new attempt. This actor neither connects nor
/// replays anything, and it never stores input, output, keys, hostnames, error
/// strings, or payload hashes. A new live frame cannot clear earlier uncertainty.
public actor TerminalSessionJournal {
    private struct Entry {
        let id: UUID
        var revision: UUID
        let scope: TerminalSessionScope
        let process: UUID
        let owner: UUID
        var state: TerminalSessionRecordState
    }
    private struct Catalog {
        let id: UUID
        var entries: [Entry]
    }
    private struct Machine: Hashable {
        let workspace: UUID
        let machine: String
        init(_ scope: TerminalSessionScope) {
            workspace = scope.workspaceID; machine = scope.machineID
        }
    }

    private static let format = "frankenterm-terminal-session-history"
    private static let maximumBytes = 512 * 1024
    private static let hardUnresolvedLimit = 256
    private static let hardResolvedLimit = 128
    private let persistence: any WorkspacePersistence
    private let maximumUnresolved: Int
    private let maximumResolved: Int
    private let process: UUID
    private let issuer = UUID()
    private var catalogID: UUID?
    private var active: Set<UUID> = []
    /// A failed write can publish a marker without ever issuing permission.
    /// Only its originating owner knows it is safe to resolve in this process.
    private var unissued: Set<UUID> = []

    public init(persistence: any WorkspacePersistence, maximumUnresolved: Int = 64,
                maximumResolved: Int = 32) throws {
        try Self.validateLimits(maximumUnresolved, maximumResolved)
        self.persistence = persistence; self.maximumUnresolved = maximumUnresolved
        self.maximumResolved = maximumResolved; self.process = TerminalSessionProcess.id
    }

    /// The directory is exclusively for session history. Do not use the
    /// workspace catalog directory; both stores intentionally reject each other.
    public init(directory: URL, maximumUnresolved: Int = 64,
                maximumResolved: Int = 32) throws {
        try Self.validateLimits(maximumUnresolved, maximumResolved)
        self.persistence = try FileWorkspacePersistence(directory: directory)
        self.maximumUnresolved = maximumUnresolved; self.maximumResolved = maximumResolved
        self.process = TerminalSessionProcess.id
    }

    /// Deterministic process-lifetime injection for crash/relaunch fault tests.
    /// Production callers cannot replace the shared process identity.
    init(persistence: any WorkspacePersistence, maximumUnresolved: Int = 64,
         maximumResolved: Int = 32, processID: UUID) throws {
        try Self.validateLimits(maximumUnresolved, maximumResolved)
        self.persistence = persistence; self.maximumUnresolved = maximumUnresolved
        self.maximumResolved = maximumResolved; self.process = processID
    }

    /// The durable write is the readiness boundary. Cancellation is checked
    /// before publication, never after a successful write and before ownership
    /// can be returned. A caller cancelled at that boundary must close and retire
    /// the returned epoch, without automatically retrying shell creation.
    public func beginCreation(scope: TerminalSessionScope) throws -> TerminalSessionEpoch {
        try Task.checkCancellation()
        return try transaction {
            var catalog = try load()
            let machine = Machine(scope)
            guard !catalog.entries.contains(where: {
                $0.state != .acknowledged && Machine($0.scope) == machine
            }) else { throw TerminalSessionJournalError.reviewRequired }
            guard catalog.entries.filter({ $0.state != .acknowledged }).count < maximumUnresolved
            else { throw TerminalSessionJournalError.capacity }
            try Task.checkCancellation()
            let entry = Entry(id: UUID(), revision: UUID(), scope: scope,
                              process: process, owner: issuer, state: .creationStarted)
            catalog.entries.append(entry)
            trimResolved(&catalog)
            do { try save(catalog) }
            catch {
                unissued.insert(entry.id)
                throw error
            }
            active.insert(entry.id)
            return TerminalSessionEpoch(id: entry.id, scope: scope, issuer: issuer)
        }
    }

    /// Call only after closing local admission and joining the owned connection
    /// teardown. This retires in-memory authority and preserves durable unknown
    /// outcome metadata. Repeated calls for this owner's epoch are idempotent,
    /// including after explicit resolution and bounded resolved-history pruning.
    /// Cancellation never prevents this cleanup attempt; failed writes can be
    /// retried with the same epoch and never authorize another shell.
    public func markInterrupted(epoch: TerminalSessionEpoch) throws {
        guard epoch.issuer == issuer else { throw TerminalSessionJournalError.invalidEpoch }
        // Retirement is local and irrevocable, even if protected storage is
        // unavailable. A creationStarted marker remains conservatively blocked
        // until this write can succeed or a later process explicitly reviews it.
        active.remove(epoch.id)
        try transaction {
            var catalog = try load()
            guard let index = catalog.entries.firstIndex(where: { $0.id == epoch.id }) else {
                return // A previously acknowledged record may have been pruned.
            }
            let entry = catalog.entries[index]
            guard entry.owner == issuer, entry.process == process, entry.scope == epoch.scope
            else { throw TerminalSessionJournalError.invalidEpoch }
            guard entry.state == .creationStarted else { return }
            catalog.entries[index].state = .interrupted
            catalog.entries[index].revision = UUID()
            try save(catalog)
        }
    }

    /// Bounded, oldest-first metadata snapshots. Previous-process creation
    /// records are reviewable uncertainty, never restored live authority.
    public func records() throws -> [TerminalSessionRecord] {
        try transaction {
            try load().entries.map {
                TerminalSessionRecord(id: $0.id, scope: $0.scope, state: $0.state,
                                      canResolve: canResolve($0), revision: $0.revision,
                                      reviewer: issuer)
            }
        }
    }

    /// Separately records the user's explicit acknowledgement of possible
    /// unknown remote effects. It does not kill or resume a remote process and
    /// never opens a new session. The exact current review is required, and an
    /// active creation marker cannot be cleared by this or another journal.
    public func resolve(_ reviewed: TerminalSessionRecord) throws {
        guard reviewed.reviewer == issuer else { throw TerminalSessionJournalError.staleReview }
        try transaction {
            var catalog = try load()
            guard let index = catalog.entries.firstIndex(where: { $0.id == reviewed.id }),
                  catalog.entries[index].revision == reviewed.revision,
                  catalog.entries[index].scope == reviewed.scope,
                  catalog.entries[index].state == reviewed.state,
                  reviewed.requiresReview else { throw TerminalSessionJournalError.staleReview }
            guard canResolve(catalog.entries[index]) else { throw TerminalSessionJournalError.activeSession }
            catalog.entries[index].state = .acknowledged
            catalog.entries[index].revision = UUID()
            trimResolved(&catalog)
            try save(catalog)
            unissued.remove(reviewed.id)
        }
    }

    private func canResolve(_ entry: Entry) -> Bool {
        guard entry.state != .acknowledged, !active.contains(entry.id) else { return false }
        return entry.state == .interrupted || entry.process != process
            || (entry.owner == issuer && unissued.contains(entry.id))
    }

    private static func validateLimits(_ unresolved: Int, _ resolved: Int) throws {
        guard (1...hardUnresolvedLimit).contains(unresolved),
              (1...hardResolvedLimit).contains(resolved)
        else { throw TerminalSessionJournalError.invalidLimits }
    }

    private func transaction<T>(_ body: () throws -> T) throws -> T {
        do { return try persistence.withExclusiveAccess(body) }
        catch let error as TerminalSessionJournalError { throw error }
        catch is CancellationError { throw CancellationError() }
        catch WorkspaceStoreError.busy { throw TerminalSessionJournalError.busy }
        catch { throw TerminalSessionJournalError.recoveryRequired }
    }

    private func trimResolved(_ catalog: inout Catalog) {
        var excess = catalog.entries.filter { $0.state == .acknowledged }.count - maximumResolved
        guard excess > 0 else { return }
        catalog.entries.removeAll {
            if $0.state == .acknowledged && excess > 0 { excess -= 1; return true }
            return false
        }
    }

    /// All validation runs under the same exclusive lease as the subsequent
    /// mutation. No actor caches a writable copy across another owner's commit.
    private func load() throws -> Catalog {
        guard try persistence.read(.journal, limit: 128 * 1024) == nil
        else { throw TerminalSessionJournalError.wrongNamespace }
        guard let data = try persistence.read(.catalog, limit: Self.maximumBytes) else {
            guard catalogID == nil else { throw TerminalSessionJournalError.corruptStore }
            unissued.removeAll(keepingCapacity: true)
            return Catalog(id: UUID(), entries: [])
        }
        let catalog: Catalog
        do {
            let object = try StrictJSON.decode(data).object()
            guard object["format"] == .string(Self.format) else {
                throw TerminalSessionJournalError.wrongNamespace
            }
            guard Set(object.keys) == ["format", "version", "id", "records"],
                  try object.required("version").uint() == 1 else {
                throw TerminalSessionJournalError.corruptStore
            }
            let id = try Self.uuid(object, "id")
            if let catalogID, catalogID != id { throw TerminalSessionJournalError.corruptStore }
            let values = try object.required("records").array()
            guard values.count <= Self.hardUnresolvedLimit + Self.hardResolvedLimit
            else { throw TerminalSessionJournalError.corruptStore }
            var ids: Set<UUID> = [], machines: Set<Machine> = []
            var unresolved = 0, resolved = 0
            var entries: [Entry] = []
            for value in values {
                let fields = try value.object()
                guard Set(fields.keys) == ["id", "revision", "workspace", "workspace_revision",
                    "machine", "route", "transport", "channel", "process", "owner", "state"],
                      let state = TerminalSessionRecordState(rawValue: try fields.text("state"))
                else { throw TerminalSessionJournalError.corruptStore }
                let scope = try TerminalSessionScope(workspaceID: Self.uuid(fields, "workspace"),
                    workspaceRevision: Self.uuid(fields, "workspace_revision"),
                    machineID: fields.text("machine"), routeID: fields.text("route"),
                    transportID: Self.uuid(fields, "transport"), channelID: Self.uuid(fields, "channel"))
                let entry = try Entry(id: Self.uuid(fields, "id"), revision: Self.uuid(fields, "revision"),
                                      scope: scope, process: Self.uuid(fields, "process"),
                                      owner: Self.uuid(fields, "owner"), state: state)
                guard ids.insert(entry.id).inserted else { throw TerminalSessionJournalError.corruptStore }
                if state == .acknowledged { resolved += 1 }
                else {
                    unresolved += 1
                    guard machines.insert(Machine(scope)).inserted
                    else { throw TerminalSessionJournalError.corruptStore }
                }
                entries.append(entry)
            }
            guard unresolved <= Self.hardUnresolvedLimit, resolved <= Self.hardResolvedLimit,
                  active.isSubset(of: ids) else { throw TerminalSessionJournalError.corruptStore }
            catalog = Catalog(id: id, entries: entries)
        } catch let error as TerminalSessionJournalError { throw error }
        catch { throw TerminalSessionJournalError.corruptStore }
        catalogID = catalog.id
        unissued.formIntersection(catalog.entries.filter { $0.state != .acknowledged }.map(\.id))
        return catalog
    }

    private static func uuid(_ object: [String: JSONValue], _ key: String) throws -> UUID {
        let value = try object.text(key)
        guard let uuid = UUID(uuidString: value), uuid.uuidString == value
        else { throw TerminalSessionJournalError.corruptStore }
        return uuid
    }

    private func save(_ catalog: Catalog) throws {
        let records = catalog.entries.map { entry -> JSONValue in
            .object([
                "id": .string(entry.id.uuidString), "revision": .string(entry.revision.uuidString),
                "workspace": .string(entry.scope.workspaceID.uuidString),
                "workspace_revision": .string(entry.scope.workspaceRevision.uuidString),
                "machine": .string(entry.scope.machineID), "route": .string(entry.scope.routeID),
                "transport": .string(entry.scope.transportID.uuidString),
                "channel": .string(entry.scope.channelID.uuidString),
                "process": .string(entry.process.uuidString), "owner": .string(entry.owner.uuidString),
                "state": .string(entry.state.rawValue)
            ])
        }
        let bytes = try StrictJSON.encode(.object([
            "format": .string(Self.format), "version": .number("1"),
            "id": .string(catalog.id.uuidString), "records": .array(records)
        ]))
        guard bytes.count <= Self.maximumBytes else { throw TerminalSessionJournalError.capacity }
        // A thrown write can have published. Never mint an epoch or report
        // successful resolution merely because those bytes can be read back.
        try persistence.write(bytes, to: .catalog)
        catalogID = catalog.id
    }
}
