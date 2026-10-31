import Foundation
#if canImport(CryptoKit)
import CryptoKit
#elseif canImport(Crypto)
import Crypto
#endif

public enum WorkspaceKeyEnrollmentSSHError: Error, LocalizedError, Sendable {
    case route, policy, certificate, trust, hostAlias
    public var errorDescription: String? {
        switch self {
        case .route: "Enrollment requires a supported SSH route with at most three reviewed jump hosts. Via Mac is a different connection mode."
        case .policy: "This route requires authentication or capabilities that this enrollment flow cannot preserve."
        case .certificate: "This account requires a certificate. Use its approved certificate issuer to enroll the phone."
        case .trust: "This source contains a host revocation or required trust policy. Resolve it before enrollment."
        case .hostAlias: "This host-key alias must be retained in the reviewed connection plan."
        }
    }
}

/// One explicitly reviewed SSH peer. This value names an endpoint and its
/// known-host lookup; it does not prove reachability, host trust or login.
public struct WorkspaceKeyEnrollmentSSHHost: Equatable, Hashable, Sendable, Identifiable {
    public let target: WorkspaceKeyEnrollmentTarget
    public let hostKeyAlias: String?
    public var id: String {
        enrollmentRouteDigest("frankenterm-phone-key-enrollment-hop-v1\0" + target.id + "\0" +
            (hostKeyAlias ?? ""))
    }

    public init(target: WorkspaceKeyEnrollmentTarget, hostKeyAlias: String? = nil) throws {
        // A digest cannot reconstruct an account's transport path. Only this
        // plan may attach a fresh scope after reviewing all concrete peers.
        guard target.routeScope == nil else { throw WorkspaceKeyEnrollmentSSHError.route }
        if let hostKeyAlias {
            guard !hostKeyAlias.isEmpty, hostKeyAlias.utf8.allSatisfy({ $0 < 128 }),
                  hostKeyAlias.first != "-" else { throw WorkspaceKeyEnrollmentSSHError.hostAlias }
            self.hostKeyAlias = try WorkspaceDocument.host(hostKeyAlias)
        } else { self.hostKeyAlias = nil }
        self.target = target
    }
}

/// A bounded, immutable connection path. A private endpoint reached through a
/// different bastion chain is a different durable enrollment scope; no fallback
/// or route lookup is performed after this value has been approved.
public struct WorkspaceKeyEnrollmentSSHPlan: Equatable, Hashable, Sendable, Identifiable {
    public let destination: WorkspaceKeyEnrollmentSSHHost
    public let jumpHosts: [WorkspaceKeyEnrollmentSSHHost]
    public let target: WorkspaceKeyEnrollmentTarget
    public let id: String

    public init(target: WorkspaceKeyEnrollmentTarget, hostKeyAlias: String? = nil,
                jumpHosts: [WorkspaceKeyEnrollmentSSHHost] = []) throws {
        guard jumpHosts.count <= 3 else { throw WorkspaceKeyEnrollmentSSHError.route }
        let destination = try WorkspaceKeyEnrollmentSSHHost(target: target, hostKeyAlias: hostKeyAlias)
        let hosts = jumpHosts + [destination]
        // A repeated private endpoint can name a different account in each
        // forwarding namespace. Preserve every position; the three-hop bound
        // and the document's route-ID graph check prevent unbounded expansion.
        let witness = "frankenterm-phone-key-enrollment-route-v1\0" + String(hosts.count) + "\0" +
            hosts.map { $0.target.id + "\0" + ($0.hostKeyAlias ?? "") + "\0" }.joined()
        let scope = jumpHosts.isEmpty && destination.hostKeyAlias == nil ? nil : enrollmentRouteDigest(witness)
        self.destination = destination; self.jumpHosts = jumpHosts
        self.target = try WorkspaceKeyEnrollmentTarget(host: target.host, port: target.port, user: target.user,
                                                       routeScope: scope)
        self.id = enrollmentRouteDigest("frankenterm-phone-key-enrollment-plan-v1\0" + witness)
    }

    /// Each transit account is a separate optional remote edit, using only its
    /// earlier prefix. Merely traversing a hop never enrolls the key there.
    public var transitPlans: [WorkspaceKeyEnrollmentSSHPlan] {
        get throws {
            try jumpHosts.enumerated().map { index, host in
                try WorkspaceKeyEnrollmentSSHPlan(target: host.target, hostKeyAlias: host.hostKeyAlias,
                                                    jumpHosts: Array(jumpHosts.prefix(index)))
            }
        }
    }
}

private func enrollmentRouteDigest(_ value: String) -> String {
    SHA256.hash(data: Data(value.utf8)).map { String(format: "%02x", $0) }.joined()
}

/// An explicit Mac operation using its existing OpenSSH authentication and host
/// trust. Imported private paths, proxies and commands never become local argv.
/// A successful result proves the account's ordinary authorized_keys entry was
/// read back; it does not prove that this server's authentication policy uses
/// that file, or that the phone has successfully authenticated.
public enum WorkspaceKeyEnrollmentSSH {
    /// Retains the original direct-account API without discarding a route's
    /// transit path or explicit host-key alias. Use `plan` for those routes.
    public static func target(for requestedRoute: WorkspaceDocument.Route,
                              in document: WorkspaceDocument) throws -> WorkspaceKeyEnrollmentTarget {
        let planned = try plan(for: requestedRoute, in: document)
        guard planned.jumpHosts.isEmpty else { throw WorkspaceKeyEnrollmentSSHError.route }
        guard planned.destination.hostKeyAlias == nil else { throw WorkspaceKeyEnrollmentSSHError.hostAlias }
        return planned.target
    }

    public static func plan(for requestedRoute: WorkspaceDocument.Route,
                            in document: WorkspaceDocument) throws -> WorkspaceKeyEnrollmentSSHPlan {
        guard let route = document.routes.first(where: { $0.id == requestedRoute.id }),
              route.host == requestedRoute.host, route.port == requestedRoute.port,
              route.user == requestedRoute.user, route.mode == requestedRoute.mode,
              route.hostKeyAlias == requestedRoute.hostKeyAlias, route.jumpRoutes == requestedRoute.jumpRoutes,
              route.credentialIDs == requestedRoute.credentialIDs, route.trustID == requestedRoute.trustID,
              route.authenticationMethods == requestedRoute.authenticationMethods,
              route.requiredCapabilities == requestedRoute.requiredCapabilities,
              route.requiredExtensions == requestedRoute.requiredExtensions else {
            throw WorkspaceKeyEnrollmentSSHError.route
        }
        let destination = try reviewedHost(route, in: document)
        let hops = try document.orderedJumpRoutes(forRouteID: route.id).map { try reviewedHost($0, in: document) }
        return try .init(target: destination.target, hostKeyAlias: destination.hostKeyAlias, jumpHosts: hops)
    }

    private static func reviewedHost(_ route: WorkspaceDocument.Route,
                                     in document: WorkspaceDocument) throws -> WorkspaceKeyEnrollmentSSHHost {
        guard ["direct_ssh", "frankenterm_mux"].contains(route.mode) else {
            throw WorkspaceKeyEnrollmentSSHError.route
        }
        guard route.authenticationMethods.contains(.publicKey), route.requiredCapabilities.isEmpty,
              route.requiredExtensions.isEmpty else { throw WorkspaceKeyEnrollmentSSHError.policy }
        let credentials = document.credentials.filter { route.credentialIDs.contains($0.id) }
        guard credentials.allSatisfy({ $0.userCertificate == nil }) else {
            throw WorkspaceKeyEnrollmentSSHError.certificate
        }
        guard credentials.allSatisfy({ $0.requiredCapabilities.isEmpty && $0.requiredExtensions.isEmpty }) else {
            throw WorkspaceKeyEnrollmentSSHError.policy
        }
        guard let selected = document.hostTrust.first(where: { $0.id == route.trustID }),
              selected.host == route.host, selected.port == route.port, selected.hostKeyAlias == route.hostKeyAlias else {
            throw WorkspaceKeyEnrollmentSSHError.trust
        }
        let related = document.hostTrust.filter {
            $0.canonicalTrustScope == selected.canonicalTrustScope || ($0.host == route.host && $0.port == route.port)
        }
        guard related.allSatisfy({ $0.revokedKeys.isEmpty && $0.requiredPolicyRequirements.isEmpty }) else {
            throw WorkspaceKeyEnrollmentSSHError.trust
        }
        return try .init(target: WorkspaceKeyEnrollmentTarget(host: route.host, port: route.port, user: route.user),
                         hostKeyAlias: route.hostKeyAlias)
    }

    #if os(macOS) || os(Linux)
    /// Call on a worker, after persisting the journal's uncertain mutation entry.
    /// Every mutation starts with readback. Inspection never creates files or
    /// disables entries, and a lost response cannot become a successful retry.
    public static func perform(request: WorkspaceKeyEnrollmentRequest, target: WorkspaceKeyEnrollmentTarget,
                               action: WorkspaceKeyEnrollmentAction,
                               isCancelled: () -> Bool = { Task<Never, Never>.isCancelled }) throws -> WorkspaceKeyEnrollmentResult {
        try perform(request: request, target: target, action: action,
                    sshConfiguration: nil, isCancelled: isCancelled)
    }

    public static func perform(request: WorkspaceKeyEnrollmentRequest, plan: WorkspaceKeyEnrollmentSSHPlan,
                               action: WorkspaceKeyEnrollmentAction,
                               isCancelled: () -> Bool = { Task<Never, Never>.isCancelled }) throws -> WorkspaceKeyEnrollmentResult {
        try perform(request: request, plan: plan, action: action,
                    sshConfiguration: nil, isCancelled: isCancelled)
    }

    // The explicit configuration seam is internal: fixtures can supply isolated
    // identities and known_hosts without ever consulting the operator's files.
    static func perform(request: WorkspaceKeyEnrollmentRequest, target: WorkspaceKeyEnrollmentTarget,
                        action: WorkspaceKeyEnrollmentAction, sshConfiguration: URL?,
                        isCancelled: () -> Bool = { Task<Never, Never>.isCancelled }) throws -> WorkspaceKeyEnrollmentResult {
        try perform(request: request, plan: WorkspaceKeyEnrollmentSSHPlan(target: target), action: action,
                    sshConfiguration: sshConfiguration, isCancelled: isCancelled)
    }

    static func perform(request: WorkspaceKeyEnrollmentRequest, plan: WorkspaceKeyEnrollmentSSHPlan,
                        action: WorkspaceKeyEnrollmentAction, sshConfiguration: URL?,
                        isCancelled: () -> Bool = { Task<Never, Never>.isCancelled }) throws -> WorkspaceKeyEnrollmentResult {
        let target = plan.target
        func result(_ state: WorkspaceKeyEnrollmentState, _ detail: String) throws -> WorkspaceKeyEnrollmentResult {
            try .init(operationID: request.operationID(for: target), target: target, state: state, detail: detail)
        }
        func invoke(_ operation: WorkspaceKeyEnrollmentAction) throws -> WorkspaceKeyEnrollmentResult {
            let call = try invocation(request: request, plan: plan, action: operation,
                                      sshConfiguration: sshConfiguration)
            let data = try LocalHelperProcess.run(executable: call.executable, arguments: call.arguments,
                maximumOutputBytes: 4096, maximumDiagnosticBytes: 4096,
                timeout: 20 + 8 * Double(plan.jumpHosts.count),
                ownedProcessGroup: !plan.jumpHosts.isEmpty,
                environmentOverrides: plan.jumpHosts.isEmpty ? [:] : ["SHELL": "/bin/sh"],
                isCancelled: isCancelled)
            return try response(data, request: request, target: target, action: operation)
        }
        do {
            if isCancelled() { throw CancellationError() }
            let observed = try invoke(.inspect)
            guard action != .inspect else { return observed }
            if observed.state == .blocked || observed.state == .uncertain {
                return try result(.uncertain, observed.detail + " Reconcile before retrying an earlier operation.")
            }
            switch action {
            case .install:
                // An independent entry must be rechecked by the install path,
                // which preserves its existing restrictions without appending.
                guard observed.state == .absent || observed.state == .stillAuthorized else { return observed }
            case .remove:
                if observed.state == .absent || observed.state == .stillAuthorized { return observed }
                guard observed.state == .alreadyPresent else {
                    return try result(.uncertain, "The account's owned enrollment entry could not be established. Reconcile before removal.")
                }
            case .inspect: return observed
            }
            if isCancelled() { throw CancellationError() }
            let applied = try invoke(action)
            if applied.state == .blocked {
                return try result(.uncertain, applied.detail + " Reconcile before retrying an earlier operation.")
            }
            return applied
        } catch {
            // Do not display remote stderr: it can contain arbitrary terminal
            // controls, host-provided text, local paths, or authentication data.
            return try result(action == .inspect ? .blocked : .uncertain,
                isCancelled() ? "The operation was cancelled. Its remote result has not been confirmed."
                    : "SSH could not confirm the result. Check this Mac's existing host trust, public-key authentication, and Python 3 support, then reconcile.")
        }
    }

    static func invocation(request: WorkspaceKeyEnrollmentRequest, target: WorkspaceKeyEnrollmentTarget,
                           action: WorkspaceKeyEnrollmentAction, sshConfiguration: URL? = nil)
        throws -> (executable: URL, arguments: [String]) {
        try invocation(request: request, plan: WorkspaceKeyEnrollmentSSHPlan(target: target),
                       action: action, sshConfiguration: sshConfiguration)
    }

    static func invocation(request: WorkspaceKeyEnrollmentRequest, plan: WorkspaceKeyEnrollmentSSHPlan,
                           action: WorkspaceKeyEnrollmentAction, sshConfiguration: URL? = nil)
        throws -> (executable: URL, arguments: [String]) {
        let hosts = plan.jumpHosts + [plan.destination]
        var proxy: String?
        for index in plan.jumpHosts.indices {
            let hop = hosts[index], next = hosts[index + 1].target
            var hopArguments = try sshArguments(host: hop, proxy: proxy, sshConfiguration: sshConfiguration)
            hopArguments += ["-W", "[\(next.host)]:\(next.port)", "--", hop.target.host]
            // OpenSSH percent-expands ProxyCommand even inside shell quotes.
            // Protect every literal percent once per nesting layer, including
            // a previously quoted child and the private fixture config path.
            proxy = (["/usr/bin/ssh"] + hopArguments).map(shellArgument).joined(separator: " ")
                .replacingOccurrences(of: "%", with: "%%")
        }
        var arguments = try sshArguments(host: plan.destination, proxy: proxy, sshConfiguration: sshConfiguration)
        arguments += ["--", plan.destination.target.host,
                      try remoteCommand(request: request, target: plan.target, action: action)]
        return (URL(fileURLWithPath: "/usr/bin/ssh"), arguments)
    }

    private static func shellArgument(_ value: String) -> String {
        "'" + value.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    private static func sshArguments(host: WorkspaceKeyEnrollmentSSHHost, proxy: String?, sshConfiguration: URL?) throws -> [String] {
        let target = host.target
        var arguments = ["-T"]
        if let sshConfiguration {
            guard sshConfiguration.isFileURL, sshConfiguration.path.utf8.count <= 4096,
                  !sshConfiguration.path.utf8.contains(0) else {
                throw LocalHelperProcessError.invalidInvocation
            }
            arguments += ["-F", sshConfiguration.path]
        }
        // Command-line values precede the Mac's existing configuration. Local
        // identities/agent and known_hosts remain that configuration's choices;
        // host/account, proxying, commands, prompts and forwarding do not.
        for option in [
            "BatchMode=yes", "StrictHostKeyChecking=yes", "CheckHostIP=yes", "VerifyHostKeyDNS=no",
            "UpdateHostKeys=no", "KnownHostsCommand=none", "ClearAllForwardings=yes", "ForwardAgent=no",
            "ForwardX11=no", "ForwardX11Trusted=no", "Tunnel=no", "RequestTTY=no", "RemoteCommand=none",
            "PermitLocalCommand=no", "ControlMaster=no", "ControlPath=none", "ControlPersist=no",
            "ProxyCommand=" + (proxy ?? "none"), "ProxyJump=none", "ProxyUseFdpass=no",
            "CanonicalizeHostname=no", "SessionType=default", "StdinNull=no", "ExitOnForwardFailure=yes",
            "PasswordAuthentication=no", "KbdInteractiveAuthentication=no", "PreferredAuthentications=publickey",
            "PubkeyAuthentication=yes", "NumberOfPasswordPrompts=0", "ConnectionAttempts=1", "ConnectTimeout=8",
            "ServerAliveInterval=5", "ServerAliveCountMax=1", "ForkAfterAuthentication=no", "LogLevel=ERROR",
            "HostName=" + target.host,
            "HostKeyAlias=" + (host.hostKeyAlias ?? (target.port == 22 ? target.host : "[\(target.host)]:\(target.port)"))
        ] { arguments += ["-o", option] }
        arguments += ["-p", String(target.port), "-l", target.user]
        return arguments
    }

    static func remoteCommand(request: WorkspaceKeyEnrollmentRequest, target: WorkspaceKeyEnrollmentTarget,
                              action: WorkspaceKeyEnrollmentAction) throws -> String {
        let payload = try StrictJSON.encode(.object([
            "version": .number("1"), "action": .string(action.rawValue),
            "operation_id": .string(request.operationID(for: target)), "target_id": .string(target.id),
            "user": .string(target.user), "request": try StrictJSON.decode(request.encoded())
        ])).base64EncodedString()
        // Both substitutions use the base64 alphabet, never shell syntax. The
        // isolated interpreter ignores PYTHONPATH and user/site startup code.
        let program = Data(remoteProgram.utf8).base64EncodedString()
        return "python3 -I -S -c 'import base64;exec(base64.b64decode(\"" + program + "\"))' '" + payload + "'"
    }

    static func response(_ data: Data, request: WorkspaceKeyEnrollmentRequest,
                         target: WorkspaceKeyEnrollmentTarget, action: WorkspaceKeyEnrollmentAction) throws -> WorkspaceKeyEnrollmentResult {
        guard !data.isEmpty, data.count <= 4096 else { throw FTError.limit("enrollment.response") }
        let root = try StrictJSON.decode(data).object()
        guard Set(root.keys) == Set(["version", "operation_id", "target_id", "fingerprint", "state", "detail_code"]),
              try root.required("version").uint() == 1,
              try root.required("operation_id").string() == request.operationID(for: target),
              try root.required("target_id").string() == target.id,
              try root.required("fingerprint").string() == request.key.fingerprint,
              let state = WorkspaceKeyEnrollmentState(rawValue: try root.required("state").string()) else {
            throw FTError.malformed("enrollment.response_binding")
        }
        let code = try root.required("detail_code").string()
        let detail: String
        switch (state, code) {
        case (.absent, "not_present") where action != .install:
            detail = "This public key is absent from the account's standard authorized_keys file. Existing SSH sessions and other authorization sources were not changed."
        case (.installed, "added") where action == .install:
            detail = "The exact phone-key entry was installed and read back. Verify a connection from the phone before relying on it."
        case (.alreadyPresent, "owned_entry"):
            detail = "The entry for this operation was read back. No duplicate was added. Verify a connection from the phone."
        case (.alreadyPresent, "owned_and_other"):
            detail = "This operation's entry and another live entry for the same public key were read back. Removing this enrollment alone will leave the other entry intact."
        case (.alreadyPresent, "existing_entry"):
            detail = "This public key already has an unrestricted entry. Existing content was preserved; no duplicate was added."
        case (.restricted, "restricted_entry"):
            detail = "This public key already has a restricted entry. Its restrictions were preserved; no unrestricted copy was added."
        case (.removed, "owned_entry_disabled") where action == .remove:
            detail = "This operation's entry was disabled and read back. No live entry for this public key remains in the standard authorized_keys file. Existing SSH sessions are unaffected; other server authorization sources may still grant access."
        case (.stillAuthorized, "independent_entry") where action != .install:
            detail = "This operation has no unchanged owned entry, but another live entry for the public key remains. Independent, modified, and restricted entries were preserved."
        case (.stillAuthorized, "owned_removed_other_remains") where action == .remove:
            detail = "This operation's entry was disabled and read back, but another live entry for the public key remains. Its authorization and existing SSH sessions were not revoked."
        case (.blocked, "account_mismatch"): detail = "The remote account does not match the reviewed account."
        case (.blocked, "unsafe_home"), (.blocked, "unsafe_directory"), (.blocked, "unsafe_file"):
            detail = "The remote home, SSH directory, or key file has unsupported ownership, permissions, links, or type."
        case (.blocked, "file_limit"): detail = "The existing authorized_keys file exceeds the supported one-MiB limit."
        case (.blocked, "key_conflict"): detail = "The existing key file contains a conflicting or unrecognized entry for this phone identity."
        case (.blocked, "lock_busy"): detail = "Another operation is changing the key file. Reconcile after it finishes."
        case (.blocked, "lease_unavailable") where action == .remove:
            detail = "Safe removal requires a Linux server and a filesystem that supports kernel write leases. This account could not provide that exclusion, so no removal was attempted."
        case (.blocked, "lease_busy") where action == .remove:
            detail = "Another process has the key file open. Safe removal could not acquire exclusive writer protection; retry after that process finishes, then reconcile."
        case (.blocked, "lease_changed") where action == .remove:
            detail = "Another process requested the key file during removal preparation. No removal write was attempted. Reconcile before another change."
        case (.blocked, "changed_file"): detail = "The key file changed during inspection. Reconcile its current contents."
        case (.blocked, "unsupported_platform"): detail = "The server lacks the required Python 3 and POSIX file capabilities."
        case (.blocked, "io"): detail = "The server could not safely inspect or open the standard key file."
        case (.uncertain, "write_uncertain") where action != .inspect:
            detail = "The server could not confirm the complete write and readback. Reconcile before another installation or removal attempt."
        default: throw FTError.malformed("enrollment.response_state")
        }
        switch action {
        case .inspect:
            if state == .installed || state == .removed || state == .uncertain {
                throw FTError.malformed("enrollment.inspect_effect")
            }
        case .install:
            if state == .absent || state == .removed || state == .stillAuthorized {
                throw FTError.malformed("enrollment.install_effect")
            }
        case .remove:
            if state == .installed || state == .alreadyPresent || state == .restricted {
                throw FTError.malformed("enrollment.remove_effect")
            }
        }
        return try .init(operationID: request.operationID(for: target), target: target, state: state, detail: detail)
    }

    // This bounded remote program uses the account's standard authorized_keys
    // path only. Installation appends; removal comments the first byte of one
    // exact canonical owned line without replacing or truncating the file.
    // Removal additionally requires a Linux kernel write lease, excluding
    // other open descriptors and refusing a requested lease break. No-follow
    // directory-relative opens, stable-inode checks and exact readback preserve
    // unrelated entries. Lost authority or a lost response remains uncertain;
    // no advisory-only removal or blind rollback is attempted.
    static let remoteProgram = #"""
import base64, errno, fcntl, hashlib, json, os, pwd, signal, stat, struct, sys, time, uuid

LIMIT = 1048576
started_write = False
payload = None
fingerprint = None

class Refusal(Exception):
    pass

def refuse(code):
    raise Refusal(code)

def lease_changed(signum, frame):
    refuse('lease_changed')

def require_write_lease(fd):
    try:
        current = fcntl.fcntl(fd, fcntl.F_GETLEASE)
    except OSError:
        refuse('lease_changed')
    # A pending break already reports its target type, even before the kernel
    # lets the competing open/truncate proceed. Never treat it as authority.
    if current != fcntl.F_WRLCK:
        refuse('lease_changed')

def qualify_write_lease_timeout():
    # A forced break must not overtake this helper's twelve-second process
    # alarm. Read only this exact kernel setting, with a fixed byte budget.
    try:
        descriptor = os.open('/proc/sys/fs/lease-break-time', os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        try:
            raw = bytearray()
            while len(raw) < 32:
                part = os.read(descriptor, 32 - len(raw))
                if not part:
                    break
                raw.extend(part)
        finally:
            os.close(descriptor)
    except OSError:
        refuse('lease_unavailable')
    # A full budget leaves no room to establish EOF, so reject it as well as
    # longer inputs. The kernel's decimal setting is much shorter than this.
    if not raw or len(raw) == 32:
        refuse('lease_unavailable')
    if raw[-1] == 10:
        raw = raw[:-1]
    if not raw or any(byte < 48 or byte > 57 for byte in raw) or int(raw) < 15:
        refuse('lease_unavailable')

def acquire_write_lease(fd):
    if sys.platform != 'linux' or not hasattr(os, 'pwrite') or not hasattr(signal, 'SIGIO') or not all(
            hasattr(fcntl, name) for name in ('F_SETLEASE', 'F_GETLEASE', 'F_SETOWN', 'F_WRLCK')):
        refuse('lease_unavailable')
    qualify_write_lease_timeout()
    signal.signal(signal.SIGIO, lease_changed)
    try:
        fcntl.fcntl(fd, fcntl.F_SETOWN, os.getpid())
        fcntl.fcntl(fd, fcntl.F_SETLEASE, fcntl.F_WRLCK)
    except OSError as error:
        if error.errno in (errno.EAGAIN, errno.EWOULDBLOCK):
            refuse('lease_busy')
        refuse('lease_unavailable')
    require_write_lease(fd)

def exact(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            refuse('bad_request')
        result[key] = value
    return result

def emit(state, code):
    print(json.dumps({'version': 1, 'operation_id': payload['operation_id'],
        'target_id': payload['target_id'], 'fingerprint': fingerprint,
        'state': state, 'detail_code': code}, separators=(',', ':')), flush=True)

def checked_dir(fd, code):
    info = os.fstat(fd)
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o022:
        refuse(code)
    return info

def checked_file(fd):
    info = os.fstat(fd)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_nlink != 1 or info.st_mode & 0o022:
        refuse('unsafe_file')
    if info.st_size > LIMIT:
        refuse('file_limit')
    return info

def stamp(info):
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns, info.st_mode, info.st_uid, info.st_gid, info.st_nlink)

def same_snapshot(fd, observed):
    if stamp(checked_file(fd)) != observed:
        refuse('changed_file')

def same_path(directory, fd):
    current = os.stat('authorized_keys', dir_fd=directory, follow_symlinks=False)
    opened = checked_file(fd)
    if stamp(current) != stamp(opened):
        refuse('changed_file')

def same_directory(path, parent, fd):
    current = os.stat(path, dir_fd=parent, follow_symlinks=False)
    opened = checked_dir(fd, 'unsafe_directory')
    if (current.st_dev, current.st_ino, current.st_mode, current.st_uid) != (opened.st_dev, opened.st_ino, opened.st_mode, opened.st_uid):
        refuse('changed_file')

def safe_file_path(directory):
    try:
        info = os.stat('authorized_keys', dir_fd=directory, follow_symlinks=False)
    except FileNotFoundError:
        return
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_nlink != 1 or info.st_mode & 0o022:
        refuse('unsafe_file')

def still_absent(path, directory):
    try:
        os.stat(path, dir_fd=directory, follow_symlinks=False)
    except FileNotFoundError:
        return
    refuse('changed_file')

def read_all(fd):
    before = stamp(checked_file(fd))
    os.lseek(fd, 0, os.SEEK_SET)
    data = bytearray()
    while True:
        chunk = os.read(fd, min(65536, LIMIT + 1 - len(data)))
        if not chunk:
            break
        data.extend(chunk)
        if len(data) > LIMIT:
            refuse('file_limit')
    if before != stamp(checked_file(fd)):
        refuse('changed_file')
    if b'\x00' in data:
        refuse('key_conflict')
    return bytes(data), before

def field(line, start):
    while start < len(line) and line[start] in b' \t':
        start += 1
    begin = start
    quoted = False
    escaped = False
    while start < len(line):
        char = line[start]
        if escaped:
            escaped = False
        elif char == 92 and quoted:
            escaped = True
        elif char == 34:
            quoted = not quoted
        elif char in b' \t' and not quoted:
            break
        start += 1
    if quoted or escaped:
        refuse('key_conflict')
    return line[begin:start], start

def existing(data, blob, key_id, operation):
    unrestricted = False
    restricted = False
    owned = None
    marker = ('frankenterm-phone:' + key_id + ':').encode('ascii')
    encoded_blob = base64.b64encode(blob)
    canonical = b'ssh-ed25519 ' + encoded_blob + b' ' + marker + operation.encode('ascii')
    offset = 0
    for raw in data.split(b'\n'):
        line_offset = offset
        offset += len(raw) + 1
        line = raw.rstrip(b'\r')
        stripped = line.lstrip(b' \t')
        if not stripped or stripped.startswith(b'#'):
            continue
        if len(line) > 16384:
            refuse('key_conflict')
        first, end = field(line, 0)
        second, end = field(line, end)
        options = first != b'ssh-ed25519'
        if not options:
            algorithm, encoded = first, second
        else:
            algorithm = second
            encoded, end = field(line, end)
        match = False
        if algorithm == b'ssh-ed25519':
            try:
                match = base64.b64decode(encoded, validate=True) == blob
            except Exception:
                if marker in line or encoded_blob in line:
                    refuse('key_conflict')
        if match:
            # Only our exact unrestricted line grants removal authority. A
            # changed comment, indentation, option, encoding or line ending
            # belongs to the account owner and remains untouched.
            if not options and raw == canonical and offset <= len(data):
                if owned is not None:
                    refuse('key_conflict')
                owned = line_offset
            else:
                restricted = restricted or options
                unrestricted = unrestricted or not options
        elif marker in line or encoded_blob in line:
            refuse('key_conflict')
    return owned, unrestricted, restricted

def entry_state(entries, action):
    owned, unrestricted, restricted = entries
    if action == 'install' and restricted:
        return ('restricted', 'restricted_entry')
    if owned is not None:
        return ('alreadyPresent', 'owned_and_other' if unrestricted or restricted else 'owned_entry')
    if unrestricted or restricted:
        if action == 'install':
            return ('alreadyPresent', 'existing_entry')
        return ('stillAuthorized', 'independent_entry')
    return ('absent', 'not_present')

def main():
    global payload, fingerprint, started_write
    if sys.version_info < (3, 8) or os.name != 'posix' or not hasattr(os, 'O_NOFOLLOW') or not hasattr(os, 'O_DIRECTORY'):
        raise RuntimeError('unsupported_platform')
    signal.alarm(12)
    if len(sys.argv) != 2 or len(sys.argv[1]) > 32768:
        refuse('bad_request')
    payload = json.loads(base64.b64decode(sys.argv[1], validate=True), object_pairs_hook=exact)
    if set(payload) != {'version', 'action', 'operation_id', 'target_id', 'user', 'request'} or payload['version'] != 1 or payload['action'] not in ('inspect', 'install', 'remove'):
        refuse('bad_request')
    for name in ('operation_id', 'target_id'):
        value = payload[name]
        if not isinstance(value, str) or len(value) != 64 or any(c not in '0123456789abcdef' for c in value):
            refuse('bad_request')
    request = payload['request']
    if set(request) != {'format', 'version', 'request_id', 'key'} or request['format'] != 'ft-phone-key-enrollment' or request['version'] != 1:
        refuse('bad_request')
    if str(uuid.UUID(request['request_id'])) != request['request_id']:
        refuse('bad_request')
    key = request['key']
    if set(key) != {'id', 'algorithm', 'public_blob', 'public_fingerprint'} or key['algorithm'] != 'ssh-ed25519':
        refuse('bad_request')
    if not key['id'].startswith('phone-key-') or str(uuid.UUID(key['id'][10:])) != key['id'][10:]:
        refuse('bad_request')
    blob = base64.b64decode(key['public_blob'], validate=True)
    if len(blob) != 51 or blob[:19] != struct.pack('>I', 11) + b'ssh-ed25519' + struct.pack('>I', 32):
        refuse('bad_request')
    fingerprint = 'SHA256:' + base64.b64encode(hashlib.sha256(blob).digest()).decode('ascii').rstrip('=')
    if fingerprint != key['public_fingerprint'] or base64.b64encode(blob).decode('ascii') != key['public_blob']:
        refuse('bad_request')
    account = pwd.getpwuid(os.geteuid())
    if account.pw_name != payload['user']:
        refuse('account_mismatch')
    install = payload['action'] == 'install'
    remove = payload['action'] == 'remove'
    # sshd's standard %h directory comes from the account database, not a
    # shell startup file or client/server environment variable named HOME.
    home = account.pw_dir
    if not os.path.isabs(home):
        refuse('unsafe_home')
    os.umask(0o077)
    home_fd = os.open(home, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        checked_dir(home_fd, 'unsafe_home')
        same_directory(home, None, home_fd)
        try:
            directory = os.open('.ssh', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=home_fd)
        except FileNotFoundError:
            if not install:
                same_directory(home, None, home_fd)
                still_absent('.ssh', home_fd)
                return ('absent', 'not_present')
            try:
                os.mkdir('.ssh', 0o700, dir_fd=home_fd)
                os.fsync(home_fd)
            except FileExistsError:
                pass
            directory = os.open('.ssh', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=home_fd)
        try:
            checked_dir(directory, 'unsafe_directory')
            same_directory(home, None, home_fd)
            same_directory('.ssh', home_fd, directory)
            safe_file_path(directory)
            # pwrite must never inherit O_APPEND: Linux otherwise appends even
            # when pwrite supplies an offset. Removal preserves the file size.
            flags = os.O_NOFOLLOW | os.O_NONBLOCK | (os.O_RDWR if install or remove else os.O_RDONLY)
            if install:
                flags |= os.O_APPEND
            created = False
            try:
                fd = os.open('authorized_keys', flags, dir_fd=directory)
            except FileNotFoundError:
                if not install:
                    same_directory(home, None, home_fd)
                    same_directory('.ssh', home_fd, directory)
                    still_absent('authorized_keys', directory)
                    return ('absent', 'not_present')
                try:
                    fd = os.open('authorized_keys', flags | os.O_CREAT | os.O_EXCL, 0o600, dir_fd=directory)
                    created = True
                except FileExistsError:
                    fd = os.open('authorized_keys', flags, dir_fd=directory)
            try:
                checked_file(fd)
                lock = fcntl.LOCK_EX if install or remove else fcntl.LOCK_SH
                until = time.monotonic() + 5
                while True:
                    try:
                        fcntl.flock(fd, lock | fcntl.LOCK_NB)
                        break
                    except BlockingIOError:
                        if time.monotonic() >= until:
                            refuse('lock_busy')
                        time.sleep(0.025)
                if remove:
                    acquire_write_lease(fd)
                same_path(directory, fd)
                before, observed_stamp = read_all(fd)
                entries = existing(before, blob, key['id'], payload['operation_id'])
                outcome = entry_state(entries, payload['action'])
                should_install = install and outcome[0] == 'absent'
                should_remove = remove and entries[0] is not None
                if not should_install and not should_remove:
                    same_directory(home, None, home_fd)
                    same_directory('.ssh', home_fd, directory)
                    same_path(directory, fd)
                    same_snapshot(fd, observed_stamp)
                    if remove:
                        require_write_lease(fd)
                    return outcome
                if should_remove:
                    # A one-byte comment marker disables exactly our owned
                    # entry. Retaining the remainder avoids replacing/truncating
                    # unrelated keys and preserves recoverable public evidence.
                    write_offset = entries[0]
                    addition = b'#'
                    expected = before[:write_offset] + addition + before[write_offset + 1:]
                else:
                    addition = (b'\n' if before and not before.endswith(b'\n') else b'') + b'ssh-ed25519 ' + key['public_blob'].encode('ascii') + b' frankenterm-phone:' + key['id'].encode('ascii') + b':' + payload['operation_id'].encode('ascii') + b'\n'
                    expected = before + addition
                if len(expected) > LIMIT:
                    refuse('file_limit')
                repeated, repeated_stamp = read_all(fd)
                if repeated != before or repeated_stamp != observed_stamp:
                    refuse('changed_file')
                same_directory(home, None, home_fd)
                same_directory('.ssh', home_fd, directory)
                same_path(directory, fd)
                metadata = checked_file(fd)
                if stamp(metadata) != observed_stamp:
                    refuse('changed_file')
                if should_remove:
                    require_write_lease(fd)
                started_write = True
                written = os.pwrite(fd, addition, write_offset) if should_remove else os.write(fd, addition)
                if written != len(addition):
                    refuse('write_uncertain')
                os.fsync(fd)
                if created:
                    os.fsync(directory)
                same_directory(home, None, home_fd)
                same_directory('.ssh', home_fd, directory)
                same_path(directory, fd)
                after, final_stamp = read_all(fd)
                final = checked_file(fd)
                if after != expected or stamp(final) != final_stamp or (final.st_mode, final.st_uid, final.st_gid) != (metadata.st_mode, metadata.st_uid, metadata.st_gid):
                    refuse('write_uncertain')
                same_directory(home, None, home_fd)
                same_directory('.ssh', home_fd, directory)
                same_path(directory, fd)
                same_snapshot(fd, final_stamp)
                if should_remove:
                    require_write_lease(fd)
                    if entries[1] or entries[2]:
                        return ('stillAuthorized', 'owned_removed_other_remains')
                    return ('removed', 'owned_entry_disabled')
                return ('installed', 'added')
            finally:
                os.close(fd)
        finally:
            os.close(directory)
    finally:
        os.close(home_fd)

try:
    # main's finally blocks close the file and release its lease before any
    # reply I/O. The lease covers only bounded local filesystem operations.
    outcome = main()
    emit(*outcome)
except Exception as error:
    if payload is not None and fingerprint is not None:
        code = str(error) if isinstance(error, Refusal) else 'io'
        emit('uncertain' if started_write else 'blocked', 'write_uncertain' if started_write else code)
    else:
        sys.exit(1)
"""#
    #endif
}
