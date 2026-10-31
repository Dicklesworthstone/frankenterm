import Foundation
#if canImport(CryptoKit)
import CryptoKit
#elseif canImport(Crypto)
import Crypto
#endif

public enum WorkspaceKeyEnrollmentReportError: Error, Equatable, Sendable, LocalizedError {
    case phoneKeyMismatch, noSelection, unavailableSelection

    public var errorDescription: String? {
        switch self {
        case .phoneKeyMismatch:
            "These results do not name the exact phone key selected in this workspace."
        case .noSelection:
            "Select the routes that should use this phone key."
        case .unavailableSelection:
            "A selected route is not covered by usable results for every account path it affects. Review the current results and route policy."
        }
    }
}

/// A store-issued, immutable review of an explicitly opened public report.
/// Reports are unsigned observations of an authorized_keys file, not proof of
/// server identity, successful phone authentication, or permission to connect.
/// Nothing is selected by this value. The person must choose exact route IDs.
public struct WorkspaceKeyEnrollmentReportReview: Identifiable, Sendable {
    public enum Issue: String, Equatable, Sendable {
        case unsupportedPolicy, uncoveredPath, unconfirmedObservation, removalRequested

        public var message: String {
            switch self {
            case .unsupportedPolicy:
                "This route or a path that uses it has a required policy that this phone-key enrollment review cannot preserve."
            case .uncoveredPath:
                "The report does not cover every ordered account path where this route's key choice would apply."
            case .unconfirmedObservation:
                "At least one affected account lacks a confirmed unrestricted entry in this report."
            case .removalRequested:
                "The last requested change for an affected account was removal. These results cannot select that key for sign-in."
            }
        }
    }

    public struct Observation: Equatable, Identifiable, Sendable {
        public let target: WorkspaceKeyEnrollmentTarget
        public let state: WorkspaceKeyEnrollmentState?
        public let action: WorkspaceKeyEnrollmentAction?
        public let lastMutation: WorkspaceKeyEnrollmentAction?
        /// Bounded text from the selected file. Display it as reported detail,
        /// never as a trusted instruction, route, command, or success decision.
        public let detail: String?
        public var id: String { target.id }

        var confirmsUnrestrictedEntry: Bool {
            (state == .installed || state == .alreadyPresent) && lastMutation != .remove
        }
    }

    public struct Path: Equatable, Identifiable, Sendable {
        /// Reconstructed solely from the current saved workspace. No endpoint,
        /// alias, or hop supplied by the report can become a connection route.
        public let plan: WorkspaceKeyEnrollmentSSHPlan
        public let observation: Observation?
        public var id: String { plan.target.id }
    }

    public struct Route: Equatable, Identifiable, Sendable {
        public let id: String
        public let account: WorkspaceKeyEnrollmentTarget
        /// All machines affected by this route, including its transit uses.
        public let machineNames: [String]
        public let paths: [Path]
        public let issues: [Issue]
        public let assignedCredentialID: String?
        public var canAssign: Bool { !paths.isEmpty && issues.isEmpty }
    }

    public let id: UUID
    public let workspaceID: UUID
    public let expectedRevision: UUID
    public let workspaceName: String
    public let key: WorkspaceDeviceKey
    public let requestID: String
    public let reportDigest: String
    public let routes: [Route]
    public let unmatchedObservations: [Observation]
    let owner: UUID
    let reportData: Data

    /// Route assignments are global to a route ID, including when that route
    /// is a transit hop. Enumerate actual machine destinations and each expanded
    /// prefix: a definition used only as a jump does not invent a direct use.
    /// Conversely, a shared jump behind two bastions needs both observations.
    init(workspace: InstalledWorkspace, credentialID: String, data: Data,
         owner: UUID, blockedRouteIDs: Set<String>) throws {
        let journal = try WorkspaceKeyEnrollmentJournal(data: data)
        guard let key = workspace.deviceKeys.first(where: { $0.id == credentialID }),
              journal.request.key == key else {
            throw WorkspaceKeyEnrollmentReportError.phoneKeyMismatch
        }
        let document = try workspace.importedDocument()
        let indexed = Dictionary(uniqueKeysWithValues: document.routes.map { ($0.id, $0) })
        let results = Dictionary(uniqueKeysWithValues: journal.results.map { ($0.target.id, $0) })
        let observations = journal.targets.map { target in
            Observation(target: target, state: results[target.id]?.state,
                action: journal.lastAction(for: target.id), lastMutation: journal.lastMutation(for: target.id),
                detail: results[target.id]?.detail)
        }
        let observed = Dictionary(uniqueKeysWithValues: observations.map { ($0.id, $0) })
        struct Uses {
            var paths: [String: WorkspaceKeyEnrollmentSSHPlan] = [:]
            var machineIDs = Set<String>()
            var unsupported = false
        }
        var uses: [String: Uses] = [:]
        var destinations: [String] = []
        var destinationMachines: [String: Set<String>] = [:]
        for machine in document.machines {
            for routeID in machine.routeIDs {
                if destinationMachines[routeID] == nil { destinations.append(routeID) }
                destinationMachines[routeID, default: []].insert(machine.id)
            }
        }
        var matchingTargets = Set<String>()
        var pathCount = 0
        for destinationID in destinations {
            guard let destination = indexed[destinationID],
                  destination.mode == "direct_ssh" || destination.mode == "frankenterm_mux" else { continue }
            let chain = try document.orderedJumpRoutes(forRouteID: destinationID) + [destination]
            let supported = (try? WorkspaceKeyEnrollmentSSH.plan(for: destination, in: document)) != nil
                && chain.allSatisfy { !blockedRouteIDs.contains($0.id) }
            var prefix: [WorkspaceKeyEnrollmentSSHHost] = []
            var invalidPrefix = false
            for route in chain {
                var value = uses[route.id] ?? Uses()
                value.machineIDs.formUnion(destinationMachines[destinationID] ?? [])
                value.unsupported = value.unsupported || !supported || !WorkspaceDeviceKeyOverlay.canAssign(route, in: document)
                do {
                    guard !invalidPrefix else { throw WorkspaceKeyEnrollmentSSHError.route }
                    let account = try WorkspaceKeyEnrollmentTarget(host: route.host, port: route.port, user: route.user)
                    let plan = try WorkspaceKeyEnrollmentSSHPlan(target: account, hostKeyAlias: route.hostKeyAlias,
                                                                 jumpHosts: prefix)
                    if value.paths.updateValue(plan, forKey: plan.target.id) == nil {
                        pathCount += 1
                    }
                    if observed[plan.target.id]?.target == plan.target { matchingTargets.insert(plan.target.id) }
                    prefix.append(plan.destination)
                } catch {
                    // A malformed/unrepresentable prefix cannot disappear and
                    // make a later private account appear to be a direct host.
                    invalidPrefix = true
                    value.unsupported = true
                }
                guard pathCount <= 1024 else { throw FTError.limit("enrollment.report_paths") }
                uses[route.id] = value
            }
        }
        var rows: [Route] = []
        for route in document.routes {
            guard let value = uses[route.id],
                  let account = try? WorkspaceKeyEnrollmentTarget(host: route.host, port: route.port, user: route.user) else {
                continue
            }
            let paths = value.paths.values.sorted { $0.target.id < $1.target.id }.map { plan in
                let observation = observed[plan.target.id].flatMap { $0.target == plan.target ? $0 : nil }
                return Path(plan: plan, observation: observation)
            }
            var issues: [Issue] = []
            if value.unsupported { issues.append(.unsupportedPolicy) }
            if paths.isEmpty || paths.contains(where: { $0.observation == nil }) { issues.append(.uncoveredPath) }
            if paths.contains(where: { $0.observation?.lastMutation == .remove }) { issues.append(.removalRequested) }
            if paths.contains(where: { $0.observation != nil && $0.observation?.confirmsUnrestrictedEntry != true }) {
                issues.append(.unconfirmedObservation)
            }
            rows.append(.init(id: route.id, account: account,
                machineNames: document.machines.filter { value.machineIDs.contains($0.id) }.map(\.name),
                paths: paths, issues: issues,
                assignedCredentialID: workspace.deviceKeyAssignments.first { $0.routeID == route.id }?.credentialID))
        }
        id = UUID()
        workspaceID = workspace.id
        expectedRevision = workspace.revision
        workspaceName = document.name
        self.key = key
        requestID = journal.request.id
        reportDigest = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
        routes = rows
        unmatchedObservations = observations.filter { !matchingTargets.contains($0.id) }
        self.owner = owner
        reportData = data
    }
}
