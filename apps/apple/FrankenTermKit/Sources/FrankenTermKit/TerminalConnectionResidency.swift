import Foundation

/// Admission planning for a retained fleet, not connection authority. Only an
/// automatically prepared sign-in which has never opened a terminal may yield
/// its slot. The caller rechecks the exact owner and seals it synchronously,
/// then joins real cleanup before admitting its replacement. In-flight and
/// failed cleanup still occupy a slot; no counter here pretends they finished.
public struct TerminalConnectionResidency: Sendable {
    public enum Failure: Error, Equatable, Sendable { case invalidInventory, invalidLimit, invalidRelease }
    public enum State: Equatable, Sendable {
        case preparing, automaticPreparation, protectedPreparation, terminal, settling
    }
    public struct Owner: Equatable, Sendable {
        public let machineID: String
        public let id: UUID
        public let state: State
        public init(machineID: String, id: UUID, state: State) {
            self.machineID = machineID; self.id = id; self.state = state
        }
    }
    public enum Decision: Equatable, Sendable {
        case available, alreadyOwned, blocked
        case release(Owner)
    }

    public let limit: Int
    /// Exact retired preparers, not a user disconnect or failed remote session.
    /// Keeping this marker prevents background warming from immediately
    /// recreating the connection just released to make room for another one.
    private var released: [String: UUID] = [:]

    public init(limit: Int = 16) throws {
        guard (1...16).contains(limit) else { throw Failure.invalidLimit }
        self.limit = limit
    }

    /// priority is the complete known inventory, with the current group's
    /// selected machine and adjacent machines first. Reclaim the lowest-priority
    /// eligible preparer, never the selected/requested machine or a live shell.
    /// This function does no I/O and does not reserve or free any resource.
    public func decision(for machineID: String, priority: [String], owners: [Owner],
                         protecting protected: Set<String> = []) throws -> Decision {
        guard priority.count <= 128, owners.count <= 128,
              !priority.contains(where: { $0.isEmpty || $0.utf8.count > 128 }),
              Set(priority).count == priority.count,
              Set(owners.map(\.machineID)).count == owners.count,
              Set(owners.map(\.id)).count == owners.count else { throw Failure.invalidInventory }
        let known = Set(priority)
        guard known.contains(machineID), protected.isSubset(of: known),
              owners.allSatisfy({ known.contains($0.machineID) }) else {
            throw Failure.invalidInventory
        }
        if owners.contains(where: { $0.machineID == machineID }) { return .alreadyOwned }
        if owners.count < limit { return .available }
        let byMachine = Dictionary(uniqueKeysWithValues: owners.map { ($0.machineID, $0) })
        for id in priority.reversed() where id != machineID && !protected.contains(id) {
            if let candidate = byMachine[id], candidate.state == .automaticPreparation,
               released[id] != candidate.id { return .release(candidate) }
        }
        return .blocked
    }

    /// Record BEFORE sealing the owner: synchronous phase callbacks must not
    /// mistake this unopened sign-in for a terminal ending. Failure to seal may
    /// roll back this exact marker; cancellation after sealing may not.
    public mutating func willRelease(_ owner: Owner) throws {
        guard owner.state == .automaticPreparation, !owner.machineID.isEmpty,
              owner.machineID.utf8.count <= 128,
              released.count < 128 || released[owner.machineID] != nil else { throw Failure.invalidRelease }
        released[owner.machineID] = owner.id
    }

    public mutating func releaseRefused(_ owner: Owner) {
        if released[owner.machineID] == owner.id { released.removeValue(forKey: owner.machineID) }
    }

    public func wasReleased(machineID: String, ownerID: UUID) -> Bool {
        released[machineID] == ownerID
    }

    /// Selection or an explicit action may reprepare a released sign-in, but
    /// only if the independent connection-intent owner still authorizes it.
    /// An inactive group's preferences alone cannot cause release/reconnect
    /// thrashing. A manual route remains manual after reauthentication.
    public func permitsPreparation(_ machineID: String, selected: String?, explicitlyRequested: Bool) -> Bool {
        released[machineID] == nil || selected == machineID || explicitlyRequested
    }

    /// Called after the successor's real owner is reserved, never optimistically
    /// while teardown is running. An old completion cannot unmark a successor.
    public mutating func started(_ machineID: String) { released.removeValue(forKey: machineID) }
}
