#if os(macOS) || os(Linux)
import Foundation

public enum WorkspaceKeyEnrollmentSSHError: Error, LocalizedError, Sendable {
    case route, policy, certificate, trust, hostAlias
    public var errorDescription: String? {
        switch self {
        case .route: "Enrollment currently supports direct SSH accounts without jump hosts or Via Mac."
        case .policy: "This route requires authentication or capabilities that this enrollment flow cannot preserve."
        case .certificate: "This account requires a certificate. Use its approved certificate issuer to enroll the phone."
        case .trust: "This source contains a host revocation or required trust policy. Resolve it before enrollment."
        case .hostAlias: "Enrollment cannot substitute an SSH host-key alias for the reviewed host."
        }
    }
}

/// An explicit Mac operation using its existing OpenSSH authentication and host
/// trust. Imported private paths, proxies and commands never become local argv.
/// A successful result proves the account's ordinary authorized_keys entry was
/// read back; it does not prove that this server's authentication policy uses
/// that file, or that the phone has successfully authenticated.
public enum WorkspaceKeyEnrollmentSSH {
    public static func target(for requestedRoute: WorkspaceDocument.Route,
                              in document: WorkspaceDocument) throws -> WorkspaceKeyEnrollmentTarget {
        guard let route = document.routes.first(where: { $0.id == requestedRoute.id }),
              route.host == requestedRoute.host, route.port == requestedRoute.port,
              route.user == requestedRoute.user,
              ["direct_ssh", "frankenterm_mux"].contains(route.mode), route.jumpRoutes.isEmpty else {
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
        guard route.hostKeyAlias == nil else { throw WorkspaceKeyEnrollmentSSHError.hostAlias }
        guard let selected = document.hostTrust.first(where: { $0.id == route.trustID }),
              selected.host == route.host, selected.port == route.port, selected.hostKeyAlias == nil else {
            throw WorkspaceKeyEnrollmentSSHError.trust
        }
        let related = document.hostTrust.filter {
            $0.canonicalTrustScope == selected.canonicalTrustScope || ($0.host == route.host && $0.port == route.port)
        }
        guard related.allSatisfy({ $0.revokedKeys.isEmpty && $0.requiredPolicyRequirements.isEmpty }) else {
            throw WorkspaceKeyEnrollmentSSHError.trust
        }
        return try WorkspaceKeyEnrollmentTarget(host: route.host, port: route.port, user: route.user)
    }

    /// Call on a worker, after persisting the journal's uncertain install entry.
    /// Every install starts with readback. Inspection alone never creates files,
    /// and a lost install response is never converted into a successful retry.
    public static func perform(request: WorkspaceKeyEnrollmentRequest, target: WorkspaceKeyEnrollmentTarget,
                               action: WorkspaceKeyEnrollmentAction,
                               isCancelled: () -> Bool = { Task<Never, Never>.isCancelled }) throws -> WorkspaceKeyEnrollmentResult {
        try perform(request: request, target: target, action: action,
                    sshConfiguration: nil, isCancelled: isCancelled)
    }

    // The explicit configuration seam is internal: fixtures can supply isolated
    // identities and known_hosts without ever consulting the operator's files.
    static func perform(request: WorkspaceKeyEnrollmentRequest, target: WorkspaceKeyEnrollmentTarget,
                        action: WorkspaceKeyEnrollmentAction, sshConfiguration: URL?,
                        isCancelled: () -> Bool = { Task<Never, Never>.isCancelled }) throws -> WorkspaceKeyEnrollmentResult {
        func result(_ state: WorkspaceKeyEnrollmentState, _ detail: String) throws -> WorkspaceKeyEnrollmentResult {
            try .init(operationID: request.operationID(for: target), target: target, state: state, detail: detail)
        }
        func invoke(_ operation: WorkspaceKeyEnrollmentAction) throws -> WorkspaceKeyEnrollmentResult {
            let call = try invocation(request: request, target: target, action: operation,
                                      sshConfiguration: sshConfiguration)
            let data = try LocalHelperProcess.run(executable: call.executable, arguments: call.arguments,
                maximumOutputBytes: 4096, maximumDiagnosticBytes: 4096, timeout: 20, isCancelled: isCancelled)
            return try response(data, request: request, target: target, action: operation)
        }
        do {
            if isCancelled() { throw CancellationError() }
            let observed = try invoke(.inspect)
            guard action == .install else { return observed }
            if observed.state == .blocked || observed.state == .uncertain {
                return try result(.uncertain, observed.detail + " Reconcile before retrying an earlier operation.")
            }
            guard observed.state == .absent else { return observed }
            if isCancelled() { throw CancellationError() }
            let applied = try invoke(.install)
            if applied.state == .blocked {
                return try result(.uncertain, applied.detail + " Reconcile before retrying an earlier operation.")
            }
            return applied
        } catch {
            // Do not display remote stderr: it can contain arbitrary terminal
            // controls, host-provided text, local paths, or authentication data.
            return try result(action == .install ? .uncertain : .blocked,
                isCancelled() ? "The operation was cancelled. Its remote result has not been confirmed."
                    : "SSH could not confirm the result. Check this Mac's existing host trust, public-key authentication, and Python 3 support, then reconcile.")
        }
    }

    static func invocation(request: WorkspaceKeyEnrollmentRequest, target: WorkspaceKeyEnrollmentTarget,
                           action: WorkspaceKeyEnrollmentAction, sshConfiguration: URL? = nil)
        throws -> (executable: URL, arguments: [String]) {
        var arguments = ["-T"]
        if let sshConfiguration {
            guard sshConfiguration.isFileURL, !sshConfiguration.path.utf8.contains(0) else {
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
            "ProxyCommand=none", "ProxyJump=none", "CanonicalizeHostname=no", "SessionType=default",
            "PasswordAuthentication=no", "KbdInteractiveAuthentication=no", "PreferredAuthentications=publickey",
            "PubkeyAuthentication=yes", "NumberOfPasswordPrompts=0", "ConnectionAttempts=1", "ConnectTimeout=8",
            "ServerAliveInterval=5", "ServerAliveCountMax=1", "ForkAfterAuthentication=no", "LogLevel=ERROR",
            "HostName=" + target.host,
            "HostKeyAlias=" + (target.port == 22 ? target.host : "[\(target.host)]:\(target.port)")
        ] { arguments += ["-o", option] }
        arguments += ["-p", String(target.port), "-l", target.user, "--", target.host,
                      try remoteCommand(request: request, target: target, action: action)]
        return (URL(fileURLWithPath: "/usr/bin/ssh"), arguments)
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
        case (.absent, "not_present") where action == .inspect:
            detail = "This public key is absent from the account's standard authorized_keys file."
        case (.installed, "added") where action == .install:
            detail = "The exact phone-key entry was installed and read back. Verify a connection from the phone before relying on it."
        case (.alreadyPresent, "owned_entry"):
            detail = "The entry for this operation was read back. No duplicate was added. Verify a connection from the phone."
        case (.alreadyPresent, "existing_entry"):
            detail = "This public key already has an unrestricted entry. Existing content was preserved; no duplicate was added."
        case (.restricted, "restricted_entry"):
            detail = "This public key already has a restricted entry. Its restrictions were preserved; no unrestricted copy was added."
        case (.blocked, "account_mismatch"): detail = "The remote account does not match the reviewed account."
        case (.blocked, "unsafe_home"), (.blocked, "unsafe_directory"), (.blocked, "unsafe_file"):
            detail = "The remote home, SSH directory, or key file has unsupported ownership, permissions, links, or type."
        case (.blocked, "file_limit"): detail = "The existing authorized_keys file exceeds the supported one-MiB limit."
        case (.blocked, "key_conflict"): detail = "The existing key file contains a conflicting or unrecognized entry for this phone identity."
        case (.blocked, "lock_busy"): detail = "Another operation is changing the key file. Reconcile after it finishes."
        case (.blocked, "changed_file"): detail = "The key file changed during inspection. Reconcile its current contents."
        case (.blocked, "unsupported_platform"): detail = "The server lacks the required Python 3 and POSIX file capabilities."
        case (.blocked, "io"): detail = "The server could not safely inspect or open the standard key file."
        case (.uncertain, "write_uncertain") where action == .install:
            detail = "The server could not confirm the complete write and readback. Reconcile before another installation attempt."
        default: throw FTError.malformed("enrollment.response_state")
        }
        if action == .inspect && (state == .installed || state == .uncertain) {
            throw FTError.malformed("enrollment.inspect_effect")
        }
        return try .init(operationID: request.operationID(for: target), target: target, state: state, detail: detail)
    }

    // This bounded remote program uses the account's standard authorized_keys
    // path only. It never replaces, chmods, removes, or rewrites existing data.
    // O_APPEND, no-follow directory-relative opens, a cooperative file lock,
    // stable-inode checks and exact readback preserve unrelated entries. A
    // noncooperating writer or lost response remains an uncertain operation.
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
    return bytes(data)

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
    seen = False
    restricted = False
    owned = False
    marker = ('frankenterm-phone:' + key_id + ':').encode('ascii')
    for line in data.split(b'\n'):
        line = line.rstrip(b'\r')
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
                if marker in line or base64.b64encode(blob) in line:
                    refuse('key_conflict')
        if match:
            seen = True
            restricted = restricted or options
            owned = owned or line[end:].strip() == marker + operation.encode('ascii')
        elif marker in line or base64.b64encode(blob) in line:
            refuse('key_conflict')
    if restricted:
        return ('restricted', 'restricted_entry')
    if seen:
        return ('alreadyPresent', 'owned_entry' if owned else 'existing_entry')
    return ('absent', 'not_present')

def main():
    global payload, fingerprint, started_write
    if sys.version_info < (3, 8) or os.name != 'posix' or not hasattr(os, 'O_NOFOLLOW') or not hasattr(os, 'O_DIRECTORY'):
        raise RuntimeError('unsupported_platform')
    signal.alarm(12)
    if len(sys.argv) != 2 or len(sys.argv[1]) > 32768:
        refuse('bad_request')
    payload = json.loads(base64.b64decode(sys.argv[1], validate=True), object_pairs_hook=exact)
    if set(payload) != {'version', 'action', 'operation_id', 'target_id', 'user', 'request'} or payload['version'] != 1 or payload['action'] not in ('inspect', 'install'):
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
                emit('absent', 'not_present')
                return
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
            flags = os.O_NOFOLLOW | os.O_NONBLOCK | (os.O_RDWR | os.O_APPEND if install else os.O_RDONLY)
            created = False
            try:
                fd = os.open('authorized_keys', flags, dir_fd=directory)
            except FileNotFoundError:
                if not install:
                    same_directory(home, None, home_fd)
                    same_directory('.ssh', home_fd, directory)
                    still_absent('authorized_keys', directory)
                    emit('absent', 'not_present')
                    return
                try:
                    fd = os.open('authorized_keys', flags | os.O_CREAT | os.O_EXCL, 0o600, dir_fd=directory)
                    created = True
                except FileExistsError:
                    fd = os.open('authorized_keys', flags, dir_fd=directory)
            try:
                checked_file(fd)
                lock = fcntl.LOCK_EX if install else fcntl.LOCK_SH
                until = time.monotonic() + 5
                while True:
                    try:
                        fcntl.flock(fd, lock | fcntl.LOCK_NB)
                        break
                    except BlockingIOError:
                        if time.monotonic() >= until:
                            refuse('lock_busy')
                        time.sleep(0.025)
                same_path(directory, fd)
                before = read_all(fd)
                outcome = existing(before, blob, key['id'], payload['operation_id'])
                if outcome[0] != 'absent' or not install:
                    same_directory(home, None, home_fd)
                    same_directory('.ssh', home_fd, directory)
                    same_path(directory, fd)
                    emit(*outcome)
                    return
                addition = (b'\n' if before and not before.endswith(b'\n') else b'') + b'ssh-ed25519 ' + key['public_blob'].encode('ascii') + b' frankenterm-phone:' + key['id'].encode('ascii') + b':' + payload['operation_id'].encode('ascii') + b'\n'
                if len(before) + len(addition) > LIMIT:
                    refuse('file_limit')
                if read_all(fd) != before:
                    refuse('changed_file')
                same_directory(home, None, home_fd)
                same_directory('.ssh', home_fd, directory)
                same_path(directory, fd)
                metadata = checked_file(fd)
                started_write = True
                if os.write(fd, addition) != len(addition):
                    refuse('write_uncertain')
                os.fsync(fd)
                if created:
                    os.fsync(directory)
                same_directory(home, None, home_fd)
                same_directory('.ssh', home_fd, directory)
                same_path(directory, fd)
                after = read_all(fd)
                final = checked_file(fd)
                if after != before + addition or (final.st_mode, final.st_uid, final.st_gid) != (metadata.st_mode, metadata.st_uid, metadata.st_gid):
                    refuse('write_uncertain')
                same_directory(home, None, home_fd)
                same_directory('.ssh', home_fd, directory)
                same_path(directory, fd)
                emit('installed', 'added')
            finally:
                os.close(fd)
        finally:
            os.close(directory)
    finally:
        os.close(home_fd)

try:
    main()
except Exception as error:
    if payload is not None and fingerprint is not None:
        code = str(error) if isinstance(error, Refusal) else 'io'
        emit('uncertain' if started_write else 'blocked', 'write_uncertain' if started_write else code)
    else:
        sys.exit(1)
"""#
}
#endif
