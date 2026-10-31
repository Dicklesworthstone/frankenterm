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
    /// A replacement connection may proceed under its own existing consent.
    /// The earlier epoch is retained; continuing is not acknowledgement of its
    /// input outcome, and an unclean-epoch notice still needs dismissal.
    case continued
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
    public let inputEvidence: TerminalInputEvidence
    public let canResolve: Bool
    public var requiresReview: Bool { state != .acknowledged }
    /// Only an unresolved live/ended owner prevents another creation. A retained
    /// recovery notice is deliberately nonmodal and grants no connection consent.
    public var blocksNewSession: Bool { state == .creationStarted || state == .interrupted }
    /// A previous process disappeared before sealing its pre-input marker, or
    /// the transport explicitly left an input outcome unknown. This does not
    /// prove that a particular key or command arrived or executed.
    public let hasRecoveryNotice: Bool
    fileprivate let revision: UUID
    fileprivate let reviewer: UUID
}

public enum TerminalSessionJournalError: Error, Equatable, Sendable, LocalizedError {
    case invalidScope, invalidLimits, wrongNamespace, corruptStore, busy
    case capacity, reviewRequired, activeSession, staleReview, invalidEpoch
    case recoveryRequired, historyMissing

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
        case .historyMissing: return "Protected session history is missing. Recover the existing history before creating another session."
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
/// returned, bindInput associates the admitted source with the durable epoch.
/// The owner records content-free outcomes and uses a coordinator-minted seal
/// only after admission, streams, writes, delivery and native close have joined.
/// An epoch that never bound input can instead call markInterrupted after close.
/// The scene separately retains and joins its
/// pump consumer and outcome delivery. There is no implicit "clean" close: SSH
/// delivery and local close cannot prove which remote effects occurred.
///
/// Unretired records block another creation for the same installed workspace
/// and machine, including route/revision changes and relaunch. Continuing a
/// retired epoch permits an independently approved replacement connection while
/// retaining crash uncertainty until explicitly dismissed. This actor neither
/// connects nor replays anything, and it never stores input, output, keys,
/// hostnames, error strings, or payload hashes. A new live frame cannot clear
/// earlier uncertainty.
public actor TerminalSessionJournal {
    private struct Entry {
        let id: UUID
        var revision: UUID
        let scope: TerminalSessionScope
        let process: UUID
        let owner: UUID
        var state: TerminalSessionRecordState
        var inputEvidence: TerminalInputEvidence
        var unclean: Bool
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
    private static let hardRecoveryLimit = 128
    private let persistence: any WorkspacePersistence
    private let maximumUnresolved: Int
    private let maximumResolved: Int
    private let process: UUID
    private let issuer = UUID()
    private var catalogID: UUID?
    private var active: Set<UUID> = []
    private var boundSources: [UUID: TerminalSource] = [:]
    /// A genuine coordinator seal retires the owner even if its final catalog
    /// write fails. This owner can then offer explicit recovery review without
    /// forgetting the durable creation marker or needing the lost factory task.
    private var sealed: Set<UUID> = []
    /// Retain failed final publications until their owner retries. These are
    /// bounded content-free observations, never native sources or I/O tasks.
    private var pendingSeals: [UUID: TerminalInputEvidence] = [:]
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
                maximumResolved: Int = 32, identifier: String = "primary") throws {
        try Self.validateLimits(maximumUnresolved, maximumResolved)
        self.persistence = try FileTerminalSessionHistoryPersistence(directory: directory, identifier: identifier)
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
                Self.blocksNewSession($0) && Machine($0.scope) == machine
            }) else { throw TerminalSessionJournalError.reviewRequired }
            guard catalog.entries.filter(Self.blocksNewSession).count < maximumUnresolved
            else { throw TerminalSessionJournalError.capacity }
            guard boundSources.count < Self.hardUnresolvedLimit + Self.hardResolvedLimit + Self.hardRecoveryLimit
            else { throw TerminalSessionJournalError.capacity }
            try Task.checkCancellation()
            let entry = Entry(id: UUID(), revision: UUID(), scope: scope,
                              process: process, owner: issuer, state: .creationStarted, inputEvidence: [], unclean: false)
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
        guard boundSources[epoch.id] == nil else { throw TerminalSessionJournalError.activeSession }
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
            try load().entries.map(snapshot)
        }
    }

    /// Called after attaching the authenticated channel and before opening any
    /// keyboard or model-reply admission. The pre-shell marker is already
    /// durable; this local association is not restorable connection authority.
    /// Failed validation never leaves a partially bound epoch.
    public func bindInput(epoch: TerminalSessionEpoch, to source: TerminalSource) throws {
        guard epoch.issuer == issuer, active.contains(epoch.id), !sealed.contains(epoch.id),
              Self.matches(source.identity, epoch.scope),
              boundSources[epoch.id] == nil || boundSources[epoch.id] == source else {
            throw TerminalSessionJournalError.invalidEpoch
        }
        try transaction {
            let catalog = try load()
            guard let entry = catalog.entries.first(where: { $0.id == epoch.id }),
                  entry.owner == issuer, entry.process == process, entry.scope == epoch.scope,
                  entry.state == .creationStarted else { throw TerminalSessionJournalError.invalidEpoch }
        }
        boundSources[epoch.id] = source
    }

    /// Publish only a bounded union of transport observations, never payloads,
    /// operation IDs or payload hashes. A retained batch may be retried after a
    /// failed write; the union and unchanged revision make duplicates harmless.
    /// The coarse creation marker protects crash gaps between these updates.
    public func recordInput(_ outcomes: [TerminalInputOutcome], epoch: TerminalSessionEpoch) throws {
        guard epoch.issuer == issuer, active.contains(epoch.id), !sealed.contains(epoch.id),
              let source = boundSources[epoch.id] else { throw TerminalSessionJournalError.invalidEpoch }
        let evidence = try TerminalInputEvidence.observations(outcomes, source: source)
        try transaction {
            var catalog = try load()
            guard let index = catalog.entries.firstIndex(where: { $0.id == epoch.id }) else {
                throw TerminalSessionJournalError.invalidEpoch
            }
            let entry = catalog.entries[index]
            guard entry.owner == issuer, entry.process == process, entry.scope == epoch.scope,
                  entry.state == .creationStarted else { throw TerminalSessionJournalError.invalidEpoch }
            let combined = entry.inputEvidence.union(evidence)
            guard combined != entry.inputEvidence else { return }
            catalog.entries[index].inputEvidence = combined
            catalog.entries[index].revision = UUID()
            try save(catalog)
        }
    }

    /// A source-wide witness proves that admission and every producer/consumer
    /// have retired. It also conservatively retains accepted/unknown evidence
    /// if a consumer consumed its in-memory outcomes without publishing them.
    /// Local closure never proves that remote shell or input effects are absent.
    /// Repeating this operation with the same epoch/witness returns a fresh exact
    /// review; explicit acknowledgement is the only path to a resolved record.
    public func sealInput(epoch: TerminalSessionEpoch, boundary: TerminalInputSeal) throws -> TerminalSessionRecord {
        guard epoch.issuer == issuer, let source = boundSources[epoch.id], source == boundary.source,
              Self.matches(source.identity, epoch.scope) else { throw TerminalSessionJournalError.invalidEpoch }
        active.remove(epoch.id)
        sealed.insert(epoch.id)
        var observed = pendingSeals[epoch.id] ?? []
        if boundary.hasAcceptedBytes { observed.insert(.acceptedPrefix) }
        if boundary.hasUnknownTransportOutcome { observed.insert(.unknownTransportOutcome) }
        pendingSeals[epoch.id] = observed
        return try transaction {
            var catalog = try load()
            guard let index = catalog.entries.firstIndex(where: { $0.id == epoch.id }) else {
                // Only acknowledged or clean continued entries can be pruned
                // from this validated catalog. A failed final publication may outlive that pruning;
                // it must not keep an already closed owner's source alive.
                let result = TerminalSessionRecord(id: epoch.id, scope: epoch.scope, state: .acknowledged,
                    inputEvidence: observed, canResolve: false, hasRecoveryNotice: false,
                    revision: UUID(), reviewer: issuer)
                pendingSeals[epoch.id] = nil; boundSources[epoch.id] = nil; sealed.remove(epoch.id)
                return result
            }
            let entry = catalog.entries[index]
            guard entry.owner == issuer, entry.process == process, entry.scope == epoch.scope else {
                throw TerminalSessionJournalError.invalidEpoch
            }
            if entry.state == .acknowledged || entry.state == .continued {
                let result = snapshot(entry); pendingSeals[epoch.id] = nil; return result
            }
            let evidence = entry.inputEvidence.union(observed)
            pendingSeals[epoch.id] = evidence
            if entry.state != .interrupted || entry.inputEvidence != evidence {
                catalog.entries[index].state = .interrupted
                catalog.entries[index].inputEvidence = evidence
                catalog.entries[index].revision = UUID()
                try save(catalog)
            }
            let result = snapshot(catalog.entries[index])
            pendingSeals[epoch.id] = nil
            return result
        }
    }

    private static func matches(_ identity: TerminalConnectionIdentity, _ scope: TerminalSessionScope) -> Bool {
        identity.workspaceID == scope.workspaceID.uuidString && identity.machineID == scope.machineID
            && identity.routeID == scope.routeID && identity.transportID == scope.transportID
            && identity.channelID == scope.channelID
    }

    private func snapshot(_ entry: Entry) -> TerminalSessionRecord {
        TerminalSessionRecord(id: entry.id, scope: entry.scope, state: entry.state,
                              inputEvidence: entry.inputEvidence.union(pendingSeals[entry.id] ?? []),
                              canResolve: canResolve(entry),
                              hasRecoveryNotice: entry.state != .acknowledged && needsRecoveryNotice(entry),
                              revision: entry.revision, reviewer: issuer)
    }

    /// Retain the old source and its uncertainty while allowing an independently
    /// approved replacement connection. Unlike resolve, this never claims the
    /// person read or dismissed a notice. The transition and the recovery flag
    /// publish in one protected transaction, including the crash gap before any
    /// input-outcome callback could have updated a best-effort UI preference.
    @discardableResult
    public func continueAfterInterruption(_ reviewed: TerminalSessionRecord) throws -> TerminalSessionRecord {
        guard reviewed.reviewer == issuer else { throw TerminalSessionJournalError.staleReview }
        return try transaction {
            var catalog = try load()
            let index = try checkedReview(reviewed, in: catalog)
            let entry = catalog.entries[index]
            guard Self.blocksNewSession(entry) else { throw TerminalSessionJournalError.staleReview }
            let unclean = entry.unclean || (entry.state == .creationStarted && entry.process != process)
            if needsRecoveryNotice(entry) {
                guard catalog.entries.filter({ $0.state == .continued && needsRecoveryNotice($0) }).count
                        < Self.hardRecoveryLimit else { throw TerminalSessionJournalError.capacity }
            }
            catalog.entries[index].state = .continued
            catalog.entries[index].unclean = unclean
            catalog.entries[index].inputEvidence = reviewed.inputEvidence
            catalog.entries[index].revision = UUID()
            let result = snapshot(catalog.entries[index])
            trimResolved(&catalog)
            try save(catalog)
            unissued.remove(reviewed.id)
            return result
        }
    }

    /// Dismiss precisely the recovery notices the UI presented, atomically.
    /// A new epoch, another machine, or a newer revision cannot be acknowledged
    /// by an older banner. A failed write leaves the UI responsible for keeping
    /// its notice visible; it must not infer durability from a readback alone.
    /// An empty review durably republishes the current catalog without resolving
    /// any records. This supplies the explicit retry's durability receipt when
    /// an earlier acknowledgement published but failed to report success.
    public func acknowledgeRecovery(_ reviewed: [TerminalSessionRecord]) throws {
        guard reviewed.count <= Self.hardRecoveryLimit + Self.hardUnresolvedLimit,
              Set(reviewed.map(\.id)).count == reviewed.count,
              reviewed.allSatisfy({ $0.reviewer == issuer && $0.hasRecoveryNotice })
        else { throw TerminalSessionJournalError.staleReview }
        try transaction {
            var catalog = try load()
            let indices = try reviewed.map { try checkedReview($0, in: catalog) }
            for (index, record) in zip(indices, reviewed) {
                catalog.entries[index].state = .acknowledged
                catalog.entries[index].inputEvidence = record.inputEvidence
                catalog.entries[index].revision = UUID()
            }
            trimResolved(&catalog)
            try save(catalog)
        }
    }

    private func needsRecoveryNotice(_ entry: Entry) -> Bool {
        entry.unclean || entry.inputEvidence.union(pendingSeals[entry.id] ?? []).contains(.unknownTransportOutcome)
            || (entry.state == .creationStarted && entry.process != process)
    }

    private static func blocksNewSession(_ entry: Entry) -> Bool {
        entry.state == .creationStarted || entry.state == .interrupted
    }

    private func checkedReview(_ reviewed: TerminalSessionRecord, in catalog: Catalog) throws -> Int {
        guard reviewed.reviewer == issuer,
              let index = catalog.entries.firstIndex(where: { $0.id == reviewed.id }),
              catalog.entries[index].revision == reviewed.revision,
              catalog.entries[index].scope == reviewed.scope,
              catalog.entries[index].state == reviewed.state,
              catalog.entries[index].inputEvidence.union(pendingSeals[reviewed.id] ?? []) == reviewed.inputEvidence,
              snapshot(catalog.entries[index]).hasRecoveryNotice == reviewed.hasRecoveryNotice,
              reviewed.requiresReview else { throw TerminalSessionJournalError.staleReview }
        guard canResolve(catalog.entries[index]) else { throw TerminalSessionJournalError.activeSession }
        guard reviewed.canResolve else { throw TerminalSessionJournalError.staleReview }
        return index
    }

    /// Separately records the user's explicit acknowledgement of possible
    /// unknown remote effects. It does not kill or resume a remote process and
    /// never opens a new session. The exact current review is required, and an
    /// active creation marker cannot be cleared by this or another journal.
    public func resolve(_ reviewed: TerminalSessionRecord) throws {
        guard reviewed.reviewer == issuer else { throw TerminalSessionJournalError.staleReview }
        try transaction {
            var catalog = try load()
            let index = try checkedReview(reviewed, in: catalog)
            catalog.entries[index].state = .acknowledged
            catalog.entries[index].inputEvidence = reviewed.inputEvidence
            catalog.entries[index].revision = UUID()
            trimResolved(&catalog)
            try save(catalog)
            unissued.remove(reviewed.id)
        }
    }

    private func canResolve(_ entry: Entry) -> Bool {
        guard entry.state != .acknowledged, !active.contains(entry.id) else { return false }
        return entry.state == .interrupted || entry.state == .continued || entry.process != process
            || (entry.owner == issuer && (unissued.contains(entry.id) || sealed.contains(entry.id)))
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
        // A sealed continuation without crash uncertainty is ordinary bounded
        // history. An unclean continued epoch is never silently evicted.
        func disposable(_ entry: Entry) -> Bool {
            entry.state == .acknowledged || (entry.state == .continued && !needsRecoveryNotice(entry))
        }
        var excess = catalog.entries.filter(disposable).count - maximumResolved
        guard excess > 0 else { return }
        catalog.entries.removeAll {
            if disposable($0) && excess > 0 { excess -= 1; return true }
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
            let version = try object.required("version").uint()
            guard Set(object.keys) == ["format", "version", "id", "records"],
                  (1...3).contains(version) else {
                throw TerminalSessionJournalError.corruptStore
            }
            let id = try Self.uuid(object, "id")
            if let catalogID, catalogID != id { throw TerminalSessionJournalError.corruptStore }
            let values = try object.required("records").array()
            guard values.count <= Self.hardUnresolvedLimit + Self.hardResolvedLimit + Self.hardRecoveryLimit
            else { throw TerminalSessionJournalError.corruptStore }
            var ids: Set<UUID> = [], machines: Set<Machine> = []
            var unresolved = 0, resolved = 0, recovery = 0
            var entries: [Entry] = []
            for value in values {
                let fields = try value.object()
                var keys: Set<String> = ["id", "revision", "workspace", "workspace_revision",
                    "machine", "route", "transport", "channel", "process", "owner", "state"]
                if version >= 2 { keys.insert("input_evidence") }
                if version >= 3 { keys.insert("unclean") }
                guard Set(fields.keys) == keys,
                      let state = TerminalSessionRecordState(rawValue: try fields.text("state")),
                      version >= 3 || state != .continued
                else { throw TerminalSessionJournalError.corruptStore }
                var evidence: TerminalInputEvidence = []
                if version >= 2 {
                    guard let raw = UInt8(exactly: try fields.required("input_evidence").uint()) else {
                        throw TerminalSessionJournalError.corruptStore
                    }
                    evidence = TerminalInputEvidence(rawValue: raw)
                    guard evidence.isValid else { throw TerminalSessionJournalError.corruptStore }
                }
                let scope = try TerminalSessionScope(workspaceID: Self.uuid(fields, "workspace"),
                    workspaceRevision: Self.uuid(fields, "workspace_revision"),
                    machineID: fields.text("machine"), routeID: fields.text("route"),
                    transportID: Self.uuid(fields, "transport"), channelID: Self.uuid(fields, "channel"))
                let entry = try Entry(id: Self.uuid(fields, "id"), revision: Self.uuid(fields, "revision"),
                                      scope: scope, process: Self.uuid(fields, "process"),
                                      owner: Self.uuid(fields, "owner"), state: state, inputEvidence: evidence,
                                      unclean: version >= 3 ? fields.flag("unclean") : false)
                guard ids.insert(entry.id).inserted else { throw TerminalSessionJournalError.corruptStore }
                guard !entry.unclean || state == .continued || state == .acknowledged
                else { throw TerminalSessionJournalError.corruptStore }
                if Self.blocksNewSession(entry) {
                    unresolved += 1
                    guard machines.insert(Machine(scope)).inserted
                    else { throw TerminalSessionJournalError.corruptStore }
                } else if state == .continued && needsRecoveryNotice(entry) { recovery += 1 }
                else { resolved += 1 }
                entries.append(entry)
            }
            guard unresolved <= Self.hardUnresolvedLimit, resolved <= Self.hardResolvedLimit,
                  recovery <= Self.hardRecoveryLimit,
                  active.isSubset(of: ids) else { throw TerminalSessionJournalError.corruptStore }
            catalog = Catalog(id: id, entries: entries)
        } catch let error as TerminalSessionJournalError { throw error }
        catch { throw TerminalSessionJournalError.corruptStore }
        catalogID = catalog.id
        unissued.formIntersection(catalog.entries.filter(Self.blocksNewSession).map(\.id))
        let retained = Set(catalog.entries.map(\.id))
        sealed.formIntersection(retained)
        boundSources = boundSources.filter { retained.contains($0.key) || pendingSeals[$0.key] != nil }
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
                "state": .string(entry.state.rawValue),
                "input_evidence": .number(String(entry.inputEvidence.rawValue)),
                "unclean": .boolean(entry.unclean)
            ])
        }
        let bytes = try StrictJSON.encode(.object([
            "format": .string(Self.format), "version": .number("3"),
            "id": .string(catalog.id.uuidString), "records": .array(records)
        ]))
        guard bytes.count <= Self.maximumBytes else { throw TerminalSessionJournalError.capacity }
        // A thrown write can have published. Never mint an epoch or report
        // successful resolution merely because those bytes can be read back.
        try persistence.write(bytes, to: .catalog)
        catalogID = catalog.id
    }
}
