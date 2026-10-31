"""Production OpenSSH enrollment against an independent loopback libssh server.

Run: python test_device_key_enrollment.py <XCTest executable or bundle>
Requires Python cryptography and libssh. Fixture homes and generated identities
are preserved. No ambient SSH configuration, identities or hosts are accessed.
"""
import base64
import ctypes as C
import ctypes.util
import hashlib
import json
import os
from pathlib import Path
import pwd
import select
import shlex
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ec, ed25519

TEST_BUNDLE = sys.argv.pop(1)
SELECTOR = "FrankenTermEnrollmentTests.WorkspaceKeyEnrollmentSSHTests/testLiveEnrollmentUsesTrustedSSHAndAccountsForRemoteEffects"
P, I = C.c_void_p, C.c_int
SERVER = C.CDLL(ctypes.util.find_library("ssh"))
for name, result, args in [
    ("ssh_init", I, []), ("ssh_new", P, []), ("ssh_free", None, [P]),
    ("ssh_disconnect", None, [P]), ("ssh_bind_new", P, []), ("ssh_bind_free", None, [P]),
    ("ssh_bind_options_set", I, [P, I, P]), ("ssh_bind_accept_fd", I, [P, P, I]),
    ("ssh_handle_key_exchange", I, [P]), ("ssh_set_auth_methods", None, [P, I]),
    ("ssh_pki_import_privkey_base64", I, [C.c_char_p, C.c_char_p, P, P, C.POINTER(P)]),
    ("ssh_pki_export_pubkey_base64", I, [P, C.POINTER(P)]),
    ("ssh_key_free", None, [P]), ("ssh_string_free_char", None, [P]),
    ("ssh_message_get", P, [P]), ("ssh_message_type", I, [P]), ("ssh_message_subtype", I, [P]),
    ("ssh_message_free", None, [P]), ("ssh_message_reply_default", I, [P]),
    ("ssh_message_service_reply_success", I, [P]), ("ssh_message_auth_user", C.c_char_p, [P]),
    ("ssh_message_auth_pubkey", P, [P]), ("ssh_message_auth_publickey_state", I, [P]),
    ("ssh_message_auth_reply_pk_ok_simple", I, [P]), ("ssh_message_auth_reply_success", I, [P, I]),
    ("ssh_message_channel_request_open_reply_accept", P, [P]),
    ("ssh_message_channel_request_open_destination", C.c_char_p, [P]),
    ("ssh_message_channel_request_open_destination_port", I, [P]),
    ("ssh_message_channel_request_reply_success", I, [P]),
    ("ssh_message_channel_request_command", C.c_char_p, [P]),
    ("ssh_channel_write", I, [P, P, C.c_uint32]),
    ("ssh_channel_write_stderr", I, [P, P, C.c_uint32]),
    ("ssh_channel_read_nonblocking", I, [P, P, C.c_uint32, I]),
    ("ssh_channel_is_eof", I, [P]), ("ssh_channel_is_closed", I, [P]),
    ("ssh_is_connected", I, [P]), ("ssh_set_blocking", None, [P, I]),
    ("ssh_channel_send_eof", I, [P]), ("ssh_channel_close", I, [P]),
    ("ssh_channel_free", None, [P]), ("ssh_channel_request_send_exit_status", I, [P, I]),
    ("ssh_blocking_flush", I, [P, I]),
]:
    function = getattr(SERVER, name)
    function.restype, function.argtypes = result, args
SERVER.ssh_init()

# Only the account-database home is synthetic. The production command/program
# executes unmodified under the real UID and performs real POSIX file effects.
# A separate audit guard makes any accidental path outside that home fail,
# except the removal contract's one bounded, read-only kernel lease setting.
ACCOUNT_DATABASE_WRAPPER = r'''
import base64, errno, fcntl, hashlib, json, os, pwd, signal, stat, struct, sys, time, uuid
home, original_code, payload, concurrent_append, interference_json = sys.argv[1:]
home = os.path.realpath(home)
original = pwd.getpwuid
record = original(os.geteuid())
fixture_record = pwd.struct_passwd((record.pw_name, record.pw_passwd, record.pw_uid,
    record.pw_gid, record.pw_gecos, home, record.pw_shell))
pwd.getpwuid = lambda uid: fixture_record if uid == os.geteuid() else original(uid)
assert pwd.getpwuid(os.geteuid()).pw_dir == home
def audit(event, arguments):
    if event in ('open', 'os.mkdir', 'os.chdir', 'os.listdir'):
        path = arguments[0]
        if isinstance(path, (str, bytes)):
            path = os.fsdecode(path)
            if event == 'open' and path == '/proc/sys/fs/lease-break-time':
                flags = arguments[2]
                if flags & (os.O_WRONLY | os.O_RDWR | os.O_APPEND | os.O_CREAT | os.O_TRUNC):
                    raise PermissionError('fixture kernel policy is read-only')
                return
            if os.path.isabs(path):
                path = os.path.realpath(path)
                if path != home and not path.startswith(home + os.sep):
                    raise PermissionError('fixture path boundary')
            elif path not in ('.ssh', 'authorized_keys', '.'):
                raise PermissionError('fixture relative path boundary')
    if event in ('os.remove', 'os.rmdir', 'os.rename', 'os.link', 'os.symlink',
                 'os.chmod', 'os.chown', 'subprocess.Popen', 'os.exec'):
        raise PermissionError('fixture forbids unrelated effects')
sys.addaudithook(audit)
interference = json.loads(interference_json)
if interference.get('mode') == 'short_lease':
    original_open, original_read = os.open, os.read
    policy_fds = set()
    def open_with_short_policy(path, *arguments, **keywords):
        fd = original_open(path, *arguments, **keywords)
        if path == '/proc/sys/fs/lease-break-time':
            policy_fds.add(fd)
        return fd
    def read_with_short_policy(fd, count):
        actual = original_read(fd, count)
        if fd in policy_fds:
            policy_fds.remove(fd)
            return b'1\n'
        return actual
    os.open, os.read = open_with_short_policy, read_with_short_policy
elif interference.get('mode') == 'unsupported_lease':
    original_fcntl = fcntl.fcntl
    def unsupported_lease(fd, command, *arguments):
        if command == fcntl.F_SETLEASE:
            raise OSError(errno.EOPNOTSUPP, 'fixture filesystem has no leases')
        return original_fcntl(fd, command, *arguments)
    fcntl.fcntl = unsupported_lease
elif interference.get('mode') in ('break_lease', 'replace_directory'):
    original_pwrite = os.pwrite
    def pwrite_after_contender(fd, data, offset):
        os.write(interference['trigger_fd'], b'R')
        if os.read(interference['reply_fd'], 1) != b'D':
            raise OSError('fixture contender did not finish')
        return original_pwrite(fd, data, offset)
    os.pwrite = pwrite_after_contender
# A deterministic interference point performs a real append after the one-byte
# removal write. The production helper remains unchanged and must detect the
# extra writer's bytes without truncating or rolling them back.
if concurrent_append:
    append_bytes = base64.b64decode(concurrent_append, validate=True)
    original_pwrite = os.pwrite
    def pwrite_with_concurrent_append(fd, data, offset):
        count = original_pwrite(fd, data, offset)
        os.lseek(fd, 0, os.SEEK_END)
        os.write(fd, append_bytes)
        os.fsync(fd)
        return count
    os.pwrite = pwrite_with_concurrent_append
sys.argv = ['-c', payload]
exec(compile(base64.b64decode(original_code), '<production enrollment command>', 'exec'), {})
'''


def public_line(key):
    return key.public_key().public_bytes(serialization.Encoding.OpenSSH, serialization.PublicFormat.OpenSSH)


def ssh_config_path(path):
    return '"' + str(path).replace('\\', '\\\\').replace('"', '\\"') + '"'


class EnrollmentServer:
    def __init__(self, directory, host="127.0.0.1", forward_to=None):
        self.directory, self.home = directory, directory / "remote-home"
        self.home.mkdir(mode=0o700)
        self.decoy_home = directory / "untrusted-environment-home"
        self.decoy_home.mkdir(mode=0o700)
        self.account = pwd.getpwuid(os.geteuid()).pw_name
        client = ed25519.Ed25519PrivateKey.generate()
        self.client_blob = base64.b64decode(public_line(client).split()[1])
        self.host_key = ec.generate_private_key(ec.SECP256R1())
        self.host, self.forward_to = host, forward_to
        self.host_key_alias = None
        self.listener = socket.socket()
        self.listener.bind((host, 0))
        self.listener.listen(4)
        self.listener.settimeout(0.1)
        self.port = self.listener.getsockname()[1]
        self.stop = threading.Event()
        self.events, self.errors, self.commands = [], [], []
        self.forward_requests, self.forwards = [], []
        self.relay_outcomes = []
        self.accepted_connections, self.completed_connections = 0, 0
        self.stall_forward = False
        self.forward_stall_marker = None
        self.forward_stalled = threading.Event()
        self.control = None
        self.drop_next_acknowledgement = False
        self.drop_next_removal_acknowledgement = False
        self.concurrent_removal_append = b""
        self.removal_interference = ""
        self.interference_results = []
        self.replacement_contents = b""
        self.identity = directory / "client-identity"
        self.identity.write_bytes(client.private_bytes(serialization.Encoding.PEM,
            serialization.PrivateFormat.OpenSSH, serialization.NoEncryption()))
        self.identity.chmod(0o600)
        self.known_hosts = directory / "known_hosts"
        self.known_hosts.write_bytes(self.host_record(public_line(self.host_key)))
        self.configuration = directory / "ssh_config"
        self.configuration.write_text("Host *\n  IdentityFile " + ssh_config_path(self.identity) +
            "\n  UserKnownHostsFile " + ssh_config_path(self.known_hosts) +
            "\n  GlobalKnownHostsFile /dev/null\n  IdentityAgent none\n  IdentitiesOnly yes\n"
            "  PreferredAuthentications publickey\n  PasswordAuthentication no\n"
            "  KbdInteractiveAuthentication no\n  LogLevel ERROR\n")
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def host_record(self, key):
        return ("[" + self.host + "]:" + str(self.port) + " ").encode() + key + b"\n"

    def run(self):
        try:
            while not self.stop.is_set():
                try:
                    connection, _ = self.listener.accept()
                except socket.timeout:
                    continue
                except OSError:
                    if self.stop.is_set(): break
                    raise
                if self.accepted_connections >= 32:
                    connection.close()
                    raise RuntimeError("fixture connection budget exceeded")
                self.accepted_connections += 1
                self.connection(connection)
        except Exception as error:
            self.errors.append(repr(error))

    def connection(self, connection):
        binding, session, channel, upstream, key = None, None, None, None, P()
        control = connection.dup()
        self.control = control
        authenticated = False
        try:
            pem = self.host_key.private_bytes(serialization.Encoding.PEM,
                serialization.PrivateFormat.TraditionalOpenSSL, serialization.NoEncryption())
            if SERVER.ssh_pki_import_privkey_base64(pem, None, None, None, C.byref(key)):
                raise RuntimeError("fixture host-key import failed")
            binding, session = SERVER.ssh_bind_new(), SERVER.ssh_new()
            if SERVER.ssh_bind_options_set(binding, 10, key):
                raise RuntimeError("fixture host-key selection failed")
            key = P()
            disabled = C.c_bool(False)
            if SERVER.ssh_bind_options_set(binding, 19, C.byref(disabled)):
                raise RuntimeError("fixture system configuration isolation failed")
            if SERVER.ssh_bind_accept_fd(binding, session, connection.detach()):
                raise RuntimeError("fixture socket acceptance failed")
            if SERVER.ssh_handle_key_exchange(session): return
            SERVER.ssh_set_auth_methods(session, 4)
            command = None
            while not self.stop.is_set():
                message = SERVER.ssh_message_get(session)
                if not message: return
                try:
                    kind, subtype = SERVER.ssh_message_type(message), SERVER.ssh_message_subtype(message)
                    if kind == 4:
                        SERVER.ssh_message_service_reply_success(message)
                    elif kind == 1 and subtype == 4:
                        encoded = P()
                        offered = SERVER.ssh_message_auth_pubkey(message)
                        if not offered or SERVER.ssh_pki_export_pubkey_base64(offered, C.byref(encoded)):
                            raise RuntimeError("fixture public-key export failed")
                        try:
                            blob = base64.b64decode(C.string_at(encoded), validate=True)
                        finally:
                            SERVER.ssh_string_free_char(encoded)
                        state = SERVER.ssh_message_auth_publickey_state(message)
                        user = SERVER.ssh_message_auth_user(message)
                        if blob != self.client_blob or user != self.account.encode():
                            self.events.append("authentication-rejected")
                            SERVER.ssh_message_reply_default(message)
                        elif state == 0:
                            SERVER.ssh_message_auth_reply_pk_ok_simple(message)
                        elif state == 1:
                            authenticated = True
                            self.events.append("authenticated")
                            SERVER.ssh_message_auth_reply_success(message, 0)
                        else:
                            SERVER.ssh_message_reply_default(message)
                    elif kind == 2 and subtype == 1 and authenticated:
                        channel = SERVER.ssh_message_channel_request_open_reply_accept(message)
                        self.events.append("channel")
                    elif kind == 2 and subtype == 2 and authenticated:
                        destination = SERVER.ssh_message_channel_request_open_destination(message)
                        destination_port = SERVER.ssh_message_channel_request_open_destination_port(message)
                        endpoint = (destination.decode("ascii"), destination_port) if destination else None
                        self.forward_requests.append(endpoint)
                        if endpoint != self.forward_to or endpoint is None:
                            SERVER.ssh_message_reply_default(message)
                            continue
                        if self.stall_forward:
                            self.forward_stalled.set()
                            if self.forward_stall_marker:
                                self.forward_stall_marker.write_bytes(b"authenticated direct-tcpip request\n")
                            self.wait_for_cancelled_forward(control)
                            return
                        upstream = socket.create_connection(endpoint, timeout=2)
                        channel = SERVER.ssh_message_channel_request_open_reply_accept(message)
                        if not channel:
                            raise RuntimeError("fixture direct-tcpip acceptance failed")
                        self.forwards.append(endpoint)
                        self.events.append("forward")
                        break
                    elif kind == 3 and subtype == 2 and channel and authenticated:
                        command = SERVER.ssh_message_channel_request_command(message)
                        if not command or len(command) > 65_536:
                            raise RuntimeError("fixture exec command exceeded bound")
                        self.events.append("exec")
                        SERVER.ssh_message_channel_request_reply_success(message)
                        break
                    else:
                        if kind == 3 and subtype in (1, 3): self.events.append("forbidden-pty-or-shell")
                        SERVER.ssh_message_reply_default(message)
                finally:
                    SERVER.ssh_message_free(message)
            if upstream:
                self.relay(session, channel, control, upstream)
                return
            if not command: return
            words = shlex.split(command.decode("utf-8"))
            if len(words) != 6 or words[:4] != ["python3", "-I", "-S", "-c"]:
                raise RuntimeError("fixture requires the exact bounded Python invocation")
            # HOME deliberately disagrees with the account database. The audit
            # guard denies any access outside the fixture account's real files.
            environment = dict(os.environ, HOME=str(self.decoy_home), USER=self.account,
                               LOGNAME=self.account, PYTHONDONTWRITEBYTECODE="1")
            action = json.loads(base64.b64decode(words[5], validate=True))["action"]
            interference = {"mode": self.removal_interference if action == "remove" else ""}
            descriptors, inherited, contender = [], (), None
            if interference["mode"] in ("break_lease", "replace_directory"):
                trigger_read, trigger_write = os.pipe()
                reply_read, reply_write = os.pipe()
                descriptors = [trigger_read, trigger_write, reply_read, reply_write]
                inherited = (trigger_write, reply_read)
                interference.update(trigger_fd=trigger_write, reply_fd=reply_read)
                def interfere():
                    try:
                        if os.read(trigger_read, 1) != b'R':
                            return
                        if interference["mode"] == "break_lease":
                            try:
                                writer = os.open(self.home / '.ssh/authorized_keys', os.O_WRONLY | os.O_NONBLOCK)
                            except BlockingIOError:
                                self.interference_results.append('writer_excluded')
                            else:
                                os.close(writer)
                                self.interference_results.append('writer_was_not_excluded')
                        else:
                            (self.home / '.ssh').rename(self.home / 'preserved-ssh-directory')
                            (self.home / '.ssh').mkdir(mode=0o700)
                            replacement = self.home / '.ssh/authorized_keys'
                            replacement.write_bytes(self.replacement_contents)
                            replacement.chmod(0o600)
                            self.interference_results.append('directory_replaced')
                        try:
                            os.write(reply_write, b'D')
                        except BrokenPipeError:
                            pass
                    except Exception as error:
                        self.errors.append('contender: ' + repr(error))
                contender = threading.Thread(target=interfere, daemon=True)
                contender.start()
            try:
                completed = subprocess.run([sys.executable, "-I", "-S", "-c", ACCOUNT_DATABASE_WRAPPER,
                    str(self.home), base64.b64encode(words[4].encode()).decode(), words[5],
                    base64.b64encode(self.concurrent_removal_append).decode(), json.dumps(interference)],
                    stdin=subprocess.DEVNULL, capture_output=True, timeout=12, pass_fds=inherited,
                    env=environment, cwd=self.home)
            finally:
                for descriptor in inherited:
                    os.close(descriptor)
                if contender:
                    contender.join(3)
                    if contender.is_alive():
                        raise RuntimeError('fixture contender did not retire')
                for descriptor in descriptors:
                    if descriptor not in inherited:
                        os.close(descriptor)
            if len(completed.stdout) > 65_536 or len(completed.stderr) > 65_536:
                raise RuntimeError("fixture remote output exceeded bound")
            self.commands.append((completed.returncode, completed.stdout, completed.stderr))
            (self.directory / ("remote-operation-" + str(len(self.commands)) + ".json")).write_text(json.dumps({
                "action": action, "exit": completed.returncode,
                "stdout": completed.stdout.decode("utf-8", errors="replace"),
                "stderr": completed.stderr.decode("utf-8", errors="replace")}, indent=2))
            # Drop only after the mutation has completed, never its preceding
            # read-only reconciliation. The file oracle below proves that cut.
            outcome = json.loads(completed.stdout)
            if self.drop_next_acknowledgement and outcome.get("state") == "installed":
                self.drop_next_acknowledgement = False
                control.shutdown(socket.SHUT_RDWR)
                return
            if self.drop_next_removal_acknowledgement and outcome.get("state") in ("removed", "stillAuthorized") and outcome.get("detail_code") in ("owned_entry_disabled", "owned_removed_other_remains"):
                self.drop_next_removal_acknowledgement = False
                control.shutdown(socket.SHUT_RDWR)
                return
            for payload, writer in ((completed.stdout, SERVER.ssh_channel_write),
                                    (completed.stderr, SERVER.ssh_channel_write_stderr)):
                offset = 0
                while offset < len(payload):
                    count = writer(channel, payload[offset:], len(payload) - offset)
                    if count <= 0: raise RuntimeError("fixture output delivery failed")
                    offset += count
            if SERVER.ssh_channel_request_send_exit_status(channel, completed.returncode):
                raise RuntimeError("fixture exit status delivery failed")
            if SERVER.ssh_channel_send_eof(channel) or SERVER.ssh_channel_close(channel):
                raise RuntimeError("fixture channel completion failed")
            if SERVER.ssh_blocking_flush(session, 1000):
                raise RuntimeError("fixture response flush failed")
            # SSH exit-status/EOF/CLOSE completes the channel. Keep the TCP
            # transport alive until the peer has processed that completion
            # and closed, while bounding its final protocol bytes and time.
            self.drain_peer_close(control, "exec")
        finally:
            if upstream: upstream.close()
            if channel: SERVER.ssh_channel_free(channel)
            if session:
                SERVER.ssh_disconnect(session)
                SERVER.ssh_free(session)
            if binding: SERVER.ssh_bind_free(binding)
            if key: SERVER.ssh_key_free(key)
            control.close()
            self.control = None
            self.completed_connections += 1

    def wait_for_cancelled_forward(self, control):
        # This cut is after real authentication and an actual direct-tcpip
        # request, before a connection to the next hop. Only process-group
        # cancellation can make the real SSH peer close this stalled socket.
        deadline, received = time.monotonic() + 10, 0
        control.settimeout(0.05)
        while not self.stop.is_set() and time.monotonic() < deadline:
            try:
                tail = control.recv(4096)
            except socket.timeout:
                continue
            except ConnectionResetError:
                return
            if not tail:
                return
            received += len(tail)
            if received > 16_384:
                raise RuntimeError("fixture stalled-hop input exceeded bound")
        if not self.stop.is_set():
            raise RuntimeError("cancelled SSH proxy left its real connection open")

    def relay(self, session, channel, control, upstream):
        # Independent libssh terminates the SSH hop and relays actual channel
        # bytes to the next TCP endpoint. No production transport or shell
        # forwarding implementation is reused by this oracle.
        SERVER.ssh_set_blocking(session, 0)
        upstream.setblocking(False)
        buffer = C.create_string_buffer(32_768)
        to_upstream, to_client = bytearray(), bytearray()
        client_eof, upstream_eof, upstream_write_closed = False, False, False
        client_closed, forwarded_eof = False, False
        upstream_write_failure = None
        deadline, transferred = time.monotonic() + 25, 0
        def finished(reason, code=None):
            self.relay_outcomes.append({"connection": self.accepted_connections, "reason": reason,
                "code": code, "to_upstream_bytes": len(to_upstream), "to_client_bytes": len(to_client),
                "transferred_bytes": transferred, "client_eof": client_eof,
                "upstream_eof": upstream_eof, "client_closed": client_closed,
                "forwarded_eof": forwarded_eof, "upstream_write_failure": upstream_write_failure})
        while not self.stop.is_set():
            if time.monotonic() >= deadline:
                finished("deadline")
                raise RuntimeError("fixture direct-tcpip forwarding exceeded deadline")
            if len(to_upstream) < 131_072 and not client_eof and not client_closed:
                count = SERVER.ssh_channel_read_nonblocking(channel, buffer,
                    min(len(buffer), 131_072 - len(to_upstream)), 0)
                if count > 0:
                    if not upstream_write_closed:
                        to_upstream.extend(buffer.raw[:count])
                    transferred += count
                elif count == -1:
                    client_closed = True
                elif count == -127 or SERVER.ssh_channel_is_eof(channel):
                    client_eof = True
            client_closed = client_closed or bool(SERVER.ssh_channel_is_closed(channel)) or not SERVER.ssh_is_connected(session)
            if to_upstream:
                try:
                    count = upstream.send(to_upstream)
                    del to_upstream[:count]
                except BlockingIOError:
                    pass
                except (BrokenPipeError, ConnectionResetError) as error:
                    # Failure in this direction cannot discard a response
                    # already queued or still readable in the reverse one.
                    # Continue draining that response through its EOF under
                    # the same byte, queue and time bounds.
                    upstream_write_failure = {"code": error.errno,
                        "pending_upstream_bytes": len(to_upstream), "queued_client_bytes": len(to_client)}
                    upstream_write_closed = True
                    to_upstream.clear()
            if client_closed and not to_upstream:
                finished("client_closed")
                return
            if client_eof and not to_upstream and not upstream_write_closed:
                try:
                    upstream.shutdown(socket.SHUT_WR)
                except OSError:
                    pass
                upstream_write_closed = True
            if to_client and not client_closed:
                data = bytes(to_client[:32_768])
                count = SERVER.ssh_channel_write(channel, data, len(data))
                if count > 0:
                    del to_client[:count]
                elif count not in (0, -2):
                    finished("client_write_failed", count)
                    return
            if upstream_eof and not to_client and not forwarded_eof and not client_closed:
                # A TCP read EOF closes one direction only. Keep the SSH
                # channel/session alive until the peer finishes its reverse
                # direction; closing the entire transport here can interrupt
                # a nested client's delivery of its final exit-status packet.
                code = SERVER.ssh_channel_send_eof(channel)
                if code not in (0, -2):
                    finished("forward_eof_failed", code)
                    raise RuntimeError("fixture forwarding EOF failed")
                code = SERVER.ssh_blocking_flush(session, 1000)
                if code:
                    finished("forward_eof_flush_failed", code)
                    raise RuntimeError("fixture forwarding EOF flush failed")
                forwarded_eof = True
            if client_eof and upstream_eof and not to_client and not to_upstream:
                code = SERVER.ssh_channel_close(channel)
                if code not in (0, -2):
                    finished("forward_close_failed", code)
                    raise RuntimeError("fixture forwarding channel close failed")
                code = SERVER.ssh_blocking_flush(session, 1000)
                if code:
                    finished("forward_close_flush_failed", code)
                    raise RuntimeError("fixture forwarding close flush failed")
                self.drain_peer_close(control, "forward")
                finished("bidirectional_eof")
                return
            if transferred > 4 * 1024 * 1024:
                finished("byte_limit")
                raise RuntimeError("fixture direct-tcpip byte budget exceeded")
            readers = [] if client_closed else [control]
            if not upstream_eof and not client_closed and len(to_client) < 131_072:
                readers.append(upstream)
            readable, _, _ = select.select(readers, [upstream] if to_upstream else [], [], 0.01)
            if upstream in readable:
                try:
                    data = upstream.recv(min(32_768, 131_072 - len(to_client)))
                except BlockingIOError:
                    continue
                except ConnectionResetError:
                    data = b""
                if data:
                    to_client.extend(data)
                    transferred += len(data)
                else:
                    upstream_eof = True
        finished("fixture_stopped")

    def drain_peer_close(self, control, context):
        deadline = time.monotonic() + 2
        received = 0
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError("fixture " + context + " peer did not close after SSH completion")
            control.settimeout(remaining)
            try:
                tail = control.recv(4096)
            except ConnectionResetError:
                self.events.append(context + "-peer-reset")
                return
            except socket.timeout:
                raise RuntimeError("fixture " + context + " peer did not close after SSH completion") from None
            if not tail:
                self.events.append(context + "-peer-eof")
                return
            received += len(tail)
            if received > 16_384:
                raise RuntimeError("fixture " + context + " shutdown input exceeded bound")

    def close(self):
        self.stop.set()
        self.listener.close()
        if self.control:
            try: self.control.shutdown(socket.SHUT_RDWR)
            except OSError: pass
        self.thread.join(5)
        # Public transport metadata only: no identities, packet contents or
        # remote programs. Retain the exact delivery boundary if a test fails.
        (self.directory / "transport-events.json").write_text(json.dumps({
            "host": self.host, "accepted_connections": self.accepted_connections,
            "completed_connections": self.completed_connections, "events": self.events,
            "forward_requests": self.forward_requests, "forwards": self.forwards,
            "relay_outcomes": self.relay_outcomes}, indent=2))
        if self.thread.is_alive(): raise RuntimeError("fixture server did not retire")


class DeviceKeyEnrollmentInteropTests(unittest.TestCase):
    def setUp(self):
        self.directory = Path(tempfile.mkdtemp(prefix="ft-enrollment-", dir=os.environ.get("FT_ENROLLMENT_FIXTURE_ROOT")))
        self.server = EnrollmentServer(self.directory)
        phone = ed25519.Ed25519PrivateKey.generate()
        line = public_line(phone)
        blob = base64.b64decode(line.split()[1])
        self.phone_key = line + b" phone-key-11111111-1111-1111-1111-111111111111"
        self.request = self.directory / "request.ftkeyrequest"
        self.request.write_text(json.dumps({"format": "ft-phone-key-enrollment", "version": 1,
            "request_id": "22222222-2222-2222-2222-222222222222", "key": {
                "id": "phone-key-11111111-1111-1111-1111-111111111111", "algorithm": "ssh-ed25519",
                "public_blob": base64.b64encode(blob).decode(),
                "public_fingerprint": "SHA256:" + base64.b64encode(hashlib.sha256(blob).digest()).decode().rstrip("=")}}))
        self.authorized_keys = self.server.home / ".ssh/authorized_keys"

    def tearDown(self):
        self.server.close()
        self.assertEqual(self.server.errors, [])
        self.assertNotIn("forbidden-pty-or-shell", self.server.events)
        for event in ("authenticated", "channel", "exec"):
            self.assertEqual(self.server.events.count(event), len(self.server.commands))
        self.assertEqual(list(self.server.decoy_home.iterdir()), [], "remote HOME must not select the account key file")

    def invoke(self, action, expected):
        before = len(self.server.commands)
        environment = {key: value for key, value in os.environ.items() if not key.startswith("FT_ENROLLMENT_")}
        environment.update(FT_ENROLLMENT_CONFIG=str(self.server.configuration),
            FT_ENROLLMENT_REQUEST=str(self.request), FT_ENROLLMENT_PORT=str(self.server.port),
            FT_ENROLLMENT_ACCOUNT=self.server.account, FT_ENROLLMENT_ACTION=action,
            FT_ENROLLMENT_EXPECTED=expected)
        command = (["xcrun", "xctest", "-XCTest", SELECTOR, TEST_BUNDLE]
                   if os.path.isdir(TEST_BUNDLE) else [TEST_BUNDLE, SELECTOR])
        result = subprocess.run(command, capture_output=True, text=True, timeout=35, env=environment)
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, 0, output[-16000:])
        self.assertIn("Executed 1 test", output)
        self.assertNotIn("skipped", output.lower())
        self.assertGreater(len(self.server.commands), before, "the expected result requires an actual authenticated remote exec")

    def existing(self, contents, mode=0o640):
        self.authorized_keys.parent.mkdir(mode=0o750)
        self.authorized_keys.write_bytes(contents)
        self.authorized_keys.chmod(mode)

    def test_install_preserves_existing_bytes_and_permissions_and_retries_idempotently(self):
        other = public_line(ed25519.Ed25519PrivateKey.generate())
        original = b'# retained comment\nfrom="192.0.2.7",command="echo preserved" ' + other + b" other device\n"
        self.existing(original)
        before = self.authorized_keys.stat()
        self.invoke("inspect", "absent")
        self.assertEqual(self.authorized_keys.read_bytes(), original)
        self.invoke("install", "installed")
        installed = self.authorized_keys.read_bytes()
        self.assertTrue(installed.startswith(original))
        self.assertEqual(installed.count(self.phone_key.split()[1]), 1)
        after = self.authorized_keys.stat()
        self.assertEqual((after.st_uid, after.st_gid, stat.S_IMODE(after.st_mode)),
                         (before.st_uid, before.st_gid, stat.S_IMODE(before.st_mode)))
        self.invoke("install", "alreadyPresent")
        self.assertEqual(self.authorized_keys.read_bytes(), installed)

    def test_fresh_home_inspection_does_not_create_files_and_install_uses_private_modes(self):
        self.invoke("inspect", "absent")
        self.assertFalse(self.authorized_keys.parent.exists())
        self.invoke("install", "installed")
        self.assertEqual(stat.S_IMODE(self.authorized_keys.parent.stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE(self.authorized_keys.stat().st_mode), 0o600)
        self.assertEqual(self.authorized_keys.read_bytes().count(self.phone_key.split()[1]), 1)

    def test_restricted_existing_phone_key_cannot_gain_an_unrestricted_duplicate(self):
        original = b'from="192.0.2.8",command="echo restricted",no-pty ' + self.phone_key + b"\n"
        self.existing(original)
        self.invoke("install", "restricted")
        self.assertEqual(self.authorized_keys.read_bytes(), original)

    def test_lost_acknowledgement_requires_readback_and_never_duplicates_the_key(self):
        self.server.drop_next_acknowledgement = True
        self.invoke("install", "uncertain")
        self.assertTrue(self.authorized_keys.exists(), "lost reply must follow an actual remote edit")
        installed = self.authorized_keys.read_bytes()
        self.assertEqual(installed.count(self.phone_key.split()[1]), 1)
        self.invoke("inspect", "alreadyPresent")
        self.assertEqual(self.authorized_keys.read_bytes(), installed)
        self.invoke("install", "alreadyPresent")
        self.assertEqual(self.authorized_keys.read_bytes(), installed)

    def test_symlink_authorized_keys_cannot_redirect_an_edit(self):
        target = self.directory / "unrelated-file"
        target.write_bytes(b"preserve unrelated data\n")
        self.authorized_keys.parent.mkdir(mode=0o700)
        self.authorized_keys.symlink_to(target)
        self.invoke("inspect", "blocked")
        self.invoke("install", "uncertain")
        self.invoke("remove", "uncertain")
        self.assertEqual(target.read_bytes(), b"preserve unrelated data\n")

    def test_insecure_permissions_are_reported_without_rewriting_permissions_or_content(self):
        original = b"# preserve insecure file for explicit owner repair\n"
        self.existing(original, mode=0o666)
        self.invoke("inspect", "blocked")
        self.invoke("install", "uncertain")
        self.invoke("remove", "uncertain")
        self.assertEqual(self.authorized_keys.read_bytes(), original)
        self.assertEqual(stat.S_IMODE(self.authorized_keys.stat().st_mode), 0o666)

    def test_hard_linked_authorized_keys_cannot_mutate_another_name(self):
        original = b"# preserve shared inode\n"
        self.existing(original)
        alias = self.directory / "other-name"
        os.link(self.authorized_keys, alias)
        self.invoke("inspect", "blocked")
        self.invoke("install", "uncertain")
        self.invoke("remove", "uncertain")
        self.assertEqual(alias.read_bytes(), original)
        self.assertEqual(self.authorized_keys.read_bytes(), original)

    def test_existing_final_line_without_newline_is_preserved_before_append(self):
        original = b"# final comment without newline"
        self.existing(original)
        self.invoke("install", "installed")
        self.assertTrue(self.authorized_keys.read_bytes().startswith(original + b"\n"))

    def test_fifo_key_path_is_refused_without_waiting_for_a_writer(self):
        self.authorized_keys.parent.mkdir(mode=0o700)
        os.mkfifo(self.authorized_keys, mode=0o600)
        self.invoke("inspect", "blocked")
        self.invoke("install", "uncertain")
        self.invoke("remove", "uncertain")
        self.assertTrue(stat.S_ISFIFO(self.authorized_keys.lstat().st_mode))

    def test_remove_owned_entry_preserves_unrelated_bytes_inode_and_permissions(self):
        other = public_line(ed25519.Ed25519PrivateKey.generate())
        original = b'# retained comment\nfrom="192.0.2.7",command="echo retained" ' + other + b" other device\r\n"
        self.existing(original)
        self.invoke("install", "installed")
        installed, metadata = self.authorized_keys.read_bytes(), self.authorized_keys.stat()
        self.invoke("remove", "removed")
        removed, final = self.authorized_keys.read_bytes(), self.authorized_keys.stat()
        self.assertEqual(removed[:len(original)], original)
        self.assertEqual(len(removed), len(installed))
        self.assertEqual([(i, before, after) for i, (before, after) in enumerate(zip(installed, removed)) if before != after],
                         [(len(original), ord("s"), ord("#"))])
        self.assertEqual((final.st_dev, final.st_ino, final.st_uid, final.st_gid, final.st_mode),
                         (metadata.st_dev, metadata.st_ino, metadata.st_uid, metadata.st_gid, metadata.st_mode))
        # OpenSSH's independent key-file parser must no longer see the phone
        # fingerprint, while it still recognizes the unrelated restricted key.
        listing = subprocess.run(["/usr/bin/ssh-keygen", "-lf", str(self.authorized_keys)],
                                 capture_output=True, text=True, timeout=5)
        self.assertEqual(listing.returncode, 0, listing.stderr)
        phone_fingerprint = json.loads(self.request.read_text())["key"]["public_fingerprint"]
        self.assertNotIn(phone_fingerprint, listing.stdout)
        other_blob = base64.b64decode(other.split()[1])
        other_fingerprint = "SHA256:" + base64.b64encode(hashlib.sha256(other_blob).digest()).decode().rstrip("=")
        self.assertEqual(listing.stdout.count(other_fingerprint), 1, listing.stdout)
        self.assertEqual(listing.stdout.count("SHA256:"), 1, listing.stdout)
        self.invoke("inspect", "absent")
        self.invoke("remove", "absent")
        self.assertEqual(self.authorized_keys.read_bytes(), removed)
        self.assertEqual(self.authorized_keys.stat().st_mtime_ns, final.st_mtime_ns)

    def test_remove_absent_key_never_creates_authorization_files(self):
        self.invoke("remove", "absent")
        self.assertFalse(self.authorized_keys.parent.exists())
        self.authorized_keys.parent.mkdir(mode=0o750)
        self.invoke("remove", "absent")
        self.assertFalse(self.authorized_keys.exists())
        self.assertEqual(stat.S_IMODE(self.authorized_keys.parent.stat().st_mode), 0o750)

    def test_independently_authorized_same_key_is_preserved_and_reported(self):
        original = b"# independently managed key\n" + self.phone_key + b"\n"
        self.existing(original)
        metadata = self.authorized_keys.stat()
        self.invoke("inspect", "stillAuthorized")
        self.invoke("remove", "stillAuthorized")
        self.assertEqual(self.authorized_keys.read_bytes(), original)
        self.assertEqual(self.authorized_keys.stat().st_mtime_ns, metadata.st_mtime_ns)
        self.invoke("install", "alreadyPresent")
        self.assertEqual(self.authorized_keys.read_bytes(), original)

    def test_removal_preserves_restricted_matching_key_even_with_our_marker(self):
        self.invoke("install", "installed")
        original = b'restrict,from="192.0.2.18" ' + self.authorized_keys.read_bytes()
        self.authorized_keys.write_bytes(original)
        self.invoke("remove", "stillAuthorized")
        self.invoke("inspect", "stillAuthorized")
        self.assertEqual(self.authorized_keys.read_bytes(), original)
        self.invoke("install", "restricted")
        self.assertEqual(self.authorized_keys.read_bytes(), original)

    def test_owned_removal_reports_same_key_still_authorized_by_another_entry(self):
        self.invoke("install", "installed")
        owned = self.authorized_keys.read_bytes()
        independent = b'from="192.0.2.19",command="echo independent" ' + self.phone_key + b"\n"
        self.authorized_keys.write_bytes(owned + independent)
        self.invoke("remove", "stillAuthorized")
        removed = self.authorized_keys.read_bytes()
        self.assertEqual(removed, b"#" + owned[1:] + independent)
        self.invoke("inspect", "stillAuthorized")
        self.invoke("remove", "stillAuthorized")
        self.assertEqual(self.authorized_keys.read_bytes(), removed)

    def test_another_enrollment_request_cannot_remove_the_previous_requests_entry(self):
        self.invoke("install", "installed")
        original = self.authorized_keys.read_bytes()
        request = json.loads(self.request.read_text())
        request["request_id"] = "33333333-3333-3333-3333-333333333333"
        self.request.write_text(json.dumps(request))
        self.invoke("remove", "stillAuthorized")
        self.assertEqual(self.authorized_keys.read_bytes(), original)

    def test_duplicate_owned_entries_require_review_and_are_not_partially_removed(self):
        self.invoke("install", "installed")
        original = self.authorized_keys.read_bytes() * 2
        self.authorized_keys.write_bytes(original)
        self.invoke("remove", "uncertain")
        self.assertEqual(self.authorized_keys.read_bytes(), original)

    def test_lost_removal_reply_is_reconciled_without_another_edit(self):
        self.invoke("install", "installed")
        installed = self.authorized_keys.read_bytes()
        self.server.drop_next_removal_acknowledgement = True
        self.invoke("remove", "uncertain")
        removed = self.authorized_keys.read_bytes()
        self.assertEqual(removed, b"#" + installed[1:])
        self.assertFalse(self.server.drop_next_removal_acknowledgement,
                         "the lost reply must follow a completed removal")
        metadata = self.authorized_keys.stat()
        self.invoke("inspect", "absent")
        self.invoke("remove", "absent")
        self.assertEqual(self.authorized_keys.read_bytes(), removed)
        self.assertEqual(self.authorized_keys.stat().st_mtime_ns, metadata.st_mtime_ns)

    def test_concurrent_append_is_preserved_and_prevents_a_false_removal_receipt(self):
        self.invoke("install", "installed")
        installed = self.authorized_keys.read_bytes()
        other = public_line(ed25519.Ed25519PrivateKey.generate()) + b" concurrent administrator\n"
        self.server.concurrent_removal_append = other
        self.invoke("remove", "uncertain")
        self.assertEqual(self.authorized_keys.read_bytes(), b"#" + installed[1:] + other)
        self.server.concurrent_removal_append = b""
        self.invoke("inspect", "absent")
        self.assertTrue(self.authorized_keys.read_bytes().endswith(other))

    def test_only_an_explicit_install_can_reauthorize_a_removed_enrollment(self):
        self.invoke("install", "installed")
        self.invoke("remove", "removed")
        tombstone = self.authorized_keys.read_bytes()
        self.invoke("inspect", "absent")
        self.invoke("remove", "absent")
        self.assertEqual(self.authorized_keys.read_bytes(), tombstone)
        self.invoke("install", "installed")
        reinstalled = self.authorized_keys.read_bytes()
        self.assertTrue(reinstalled.startswith(tombstone))
        live_lines = [line for line in reinstalled.splitlines() if line.strip() and not line.lstrip().startswith(b"#")]
        self.assertEqual(len(live_lines), 1)

    def test_modified_owned_line_without_final_newline_is_not_removed(self):
        self.invoke("install", "installed")
        original = self.authorized_keys.read_bytes().removesuffix(b"\n")
        self.authorized_keys.write_bytes(original)
        self.invoke("inspect", "stillAuthorized")
        self.invoke("remove", "stillAuthorized")
        self.assertEqual(self.authorized_keys.read_bytes(), original)

    def test_existing_independent_writer_prevents_removal_before_any_write(self):
        self.invoke("install", "installed")
        original = self.authorized_keys.read_bytes()
        writer = os.open(self.authorized_keys, os.O_RDWR)
        try:
            self.invoke("remove", "uncertain")
            self.assertEqual(self.authorized_keys.read_bytes(), original)
        finally:
            os.close(writer)
        self.invoke("inspect", "alreadyPresent")

    def test_new_writer_between_final_check_and_write_is_excluded(self):
        self.invoke("install", "installed")
        original = self.authorized_keys.read_bytes()
        self.server.removal_interference = "break_lease"
        self.invoke("remove", "uncertain")
        self.assertEqual(self.server.interference_results, ["writer_excluded"])
        self.assertEqual(self.authorized_keys.read_bytes(), original)
        self.server.removal_interference = ""
        self.invoke("inspect", "alreadyPresent")

    def test_filesystem_without_exclusive_leases_cannot_fall_back_to_unsafe_removal(self):
        self.invoke("install", "installed")
        original = self.authorized_keys.read_bytes()
        self.server.removal_interference = "unsupported_lease"
        self.invoke("remove", "uncertain")
        self.assertEqual(self.authorized_keys.read_bytes(), original)
        self.server.removal_interference = ""
        self.invoke("inspect", "alreadyPresent")

    def test_short_kernel_lease_break_policy_refuses_removal_before_any_write(self):
        self.invoke("install", "installed")
        original = self.authorized_keys.read_bytes()
        self.server.removal_interference = "short_lease"
        self.invoke("remove", "uncertain")
        self.assertEqual(self.authorized_keys.read_bytes(), original)
        self.server.removal_interference = ""
        self.invoke("inspect", "alreadyPresent")

    def test_replaced_ssh_directory_cannot_redirect_removal_to_another_key_file(self):
        self.invoke("install", "installed")
        original = self.authorized_keys.read_bytes()
        replacement = public_line(ed25519.Ed25519PrivateKey.generate()) + b" replacement account file\n"
        self.server.replacement_contents = replacement
        self.server.removal_interference = "replace_directory"
        self.invoke("remove", "uncertain")
        self.assertEqual(self.server.interference_results, ["directory_replaced"])
        self.assertEqual(self.authorized_keys.read_bytes(), replacement)
        preserved = self.server.home / "preserved-ssh-directory/authorized_keys"
        self.assertEqual(preserved.read_bytes(), b"#" + original[1:])


class DeviceKeyEnrollmentBastionInteropTests(unittest.TestCase):
    def setUp(self):
        self.directory = Path(tempfile.mkdtemp(prefix="ft-enrollment-bastion-",
            dir=os.environ.get("FT_ENROLLMENT_FIXTURE_ROOT")))
        for name in ("final", "first-hop", "second-hop"):
            (self.directory / name).mkdir(mode=0o700)
        # Separate loopback addresses select separate OpenSSH Host stanzas.
        # Each independent server generates its own host AND client identity.
        self.final = EnrollmentServer(self.directory / "final")
        self.second = EnrollmentServer(self.directory / "second-hop", "127.0.0.3",
            (self.final.host, self.final.port))
        self.first = EnrollmentServer(self.directory / "first-hop", "127.0.0.2",
            (self.second.host, self.second.port))
        self.servers = [self.first, self.second, self.final]
        self.assertEqual(len({server.client_blob for server in self.servers}), 3)
        self.assertEqual(len({public_line(server.host_key) for server in self.servers}), 3)
        # The nested ProxyCommand must preserve this literal path through both
        # POSIX quoting and OpenSSH's recursive percent-token expansion.
        self.configuration = self.directory / "ssh config 'quoted'%h"
        config = ""
        for server in self.servers:
            config += ("Host " + server.host + "\n  IdentityFile " + ssh_config_path(server.identity) +
                "\n  UserKnownHostsFile " + ssh_config_path(server.known_hosts) + "\n")
        self.configuration.write_text(config + "Host *\n  GlobalKnownHostsFile /dev/null\n"
            "  IdentityAgent none\n  IdentitiesOnly yes\n  PreferredAuthentications publickey\n"
            "  PasswordAuthentication no\n  KbdInteractiveAuthentication no\n  LogLevel ERROR\n")
        line = public_line(ed25519.Ed25519PrivateKey.generate())
        blob = base64.b64decode(line.split()[1])
        self.phone_key = line + b" phone-key-11111111-1111-1111-1111-111111111111"
        self.request = self.directory / "request.ftkeyrequest"
        self.request.write_text(json.dumps({"format": "ft-phone-key-enrollment", "version": 1,
            "request_id": "22222222-2222-2222-2222-222222222222", "key": {
                "id": "phone-key-11111111-1111-1111-1111-111111111111", "algorithm": "ssh-ed25519",
                "public_blob": base64.b64encode(blob).decode(),
                "public_fingerprint": "SHA256:" + base64.b64encode(hashlib.sha256(blob).digest()).decode().rstrip("=")}}))
        self.authorized_keys = self.final.home / ".ssh/authorized_keys"

    def tearDown(self):
        for server in self.servers:
            server.close()
        for server in self.servers:
            self.assertEqual(server.errors, [], server.host)
            self.assertEqual(server.accepted_connections, server.completed_connections, server.host)
            self.assertNotIn("forbidden-pty-or-shell", server.events)
            self.assertEqual(server.events.count("exec"), len(server.commands), server.host)
            self.assertEqual(list(server.decoy_home.iterdir()), [], server.host)

    def invoke(self, action, expected, jumps=None, target=None, cancellation_file=None):
        target = self.final if target is None else target
        jumps = [self.first, self.second] if jumps is None else jumps
        environment = {key: value for key, value in os.environ.items() if not key.startswith("FT_ENROLLMENT_")}
        environment.update(FT_ENROLLMENT_CONFIG=str(self.configuration),
            FT_ENROLLMENT_REQUEST=str(self.request), FT_ENROLLMENT_HOST=target.host,
            FT_ENROLLMENT_PORT=str(target.port), FT_ENROLLMENT_ACCOUNT=target.account,
            FT_ENROLLMENT_ACTION=action, FT_ENROLLMENT_EXPECTED=expected,
            FT_ENROLLMENT_JUMPS=json.dumps([{"host": hop.host, "port": hop.port, "user": hop.account,
                "hostKeyAlias": hop.host_key_alias}
                for hop in jumps]))
        if target.host_key_alias:
            environment["FT_ENROLLMENT_HOST_ALIAS"] = target.host_key_alias
        if cancellation_file:
            environment["FT_ENROLLMENT_CANCEL_FILE"] = str(cancellation_file)
        command = (["xcrun", "xctest", "-XCTest", SELECTOR, TEST_BUNDLE]
                   if os.path.isdir(TEST_BUNDLE) else [TEST_BUNDLE, SELECTOR])
        started = time.monotonic()
        result = subprocess.run(command, capture_output=True, text=True, timeout=35, env=environment)
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, 0, output[-16000:])
        self.assertIn("Executed 1 test", output)
        self.assertNotIn("skipped", output.lower())
        # This observation precedes tearDown, so server shutdown cannot conceal
        # a nested proxy which survived cancellation or ordinary completion.
        deadline = time.monotonic() + 3
        while any(server.accepted_connections != server.completed_connections for server in self.servers):
            if time.monotonic() >= deadline:
                self.fail("SSH invocation left a real bastion connection open")
            time.sleep(0.01)
        return time.monotonic() - started

    def assert_transit_untouched(self):
        for server in (self.first, self.second):
            self.assertEqual(server.commands, [], server.host)
            self.assertEqual(list(server.home.iterdir()), [], server.host)
            self.assertEqual(server.events.count("channel"), 0, server.host)
            self.assertEqual(server.forwards, [server.forward_to] * len(server.forwards), server.host)

    def assert_no_remote_effects(self):
        for server in self.servers:
            self.assertEqual(server.commands, [], server.host)
            self.assertEqual(list(server.home.iterdir()), [], server.host)

    def test_one_bastion_installs_reconciles_and_removes_only_on_the_destination(self):
        self.first.forward_to = (self.final.host, self.final.port)
        self.invoke("inspect", "absent", jumps=[self.first])
        self.assertFalse(self.authorized_keys.parent.exists())
        self.invoke("install", "installed", jumps=[self.first])
        installed = self.authorized_keys.read_bytes()
        self.assertEqual(installed.count(self.phone_key.split()[1]), 1)
        self.invoke("install", "alreadyPresent", jumps=[self.first])
        self.assertEqual(self.authorized_keys.read_bytes(), installed)
        self.invoke("remove", "removed", jumps=[self.first])
        self.assertEqual(self.authorized_keys.read_bytes(), b"#" + installed[1:])
        self.invoke("inspect", "absent", jumps=[self.first])
        self.assertEqual(len(self.first.forwards), 7)
        self.assertEqual(self.second.accepted_connections, 0)
        self.assertEqual(len(self.final.commands), 7)
        self.assert_transit_untouched()

    def test_two_independently_authenticated_bastions_install_and_remove_only_at_destination(self):
        self.invoke("inspect", "absent")
        self.invoke("install", "installed")
        installed = self.authorized_keys.read_bytes()
        self.assertEqual(installed.count(self.phone_key.split()[1]), 1)
        self.invoke("remove", "removed")
        self.assertEqual(self.authorized_keys.read_bytes(), b"#" + installed[1:])
        for server in self.servers:
            self.assertEqual(server.events.count("authenticated"), 5, server.host)
        self.assertEqual(len(self.first.forwards), 5)
        self.assertEqual(len(self.second.forwards), 5)
        self.assertEqual(len(self.final.commands), 5)
        self.assert_transit_untouched()

    def test_maximum_three_bastions_preserve_each_identity_and_mutate_only_the_destination(self):
        directory = self.directory / "third-hop"
        directory.mkdir(mode=0o700)
        third = EnrollmentServer(directory, "127.0.0.4", (self.final.host, self.final.port))
        self.servers.insert(2, third)
        self.second.forward_to = (third.host, third.port)
        self.assertEqual(len({server.client_blob for server in self.servers}), 4)
        self.assertEqual(len({public_line(server.host_key) for server in self.servers}), 4)
        for index, server in enumerate(self.servers):
            server.host_key_alias = "enrollment-peer-" + str(index) + ".fixture.invalid"
            self.assertNotEqual(server.port, 22)
            # No address or [alias]:port entry is present. Each real SSH peer
            # must use the explicit reviewed alias verbatim for host trust.
            server.known_hosts.write_bytes(server.host_key_alias.encode() + b" " +
                public_line(server.host_key) + b"\n")
        with self.configuration.open("a") as config:
            config.write("Host " + third.host + "\n  IdentityFile " + ssh_config_path(third.identity) +
                "\n  UserKnownHostsFile " + ssh_config_path(third.known_hosts) + "\n")
        hops = [self.first, self.second, third]
        self.invoke("install", "installed", jumps=hops)
        installed = self.authorized_keys.read_bytes()
        self.assertEqual(installed.count(self.phone_key.split()[1]), 1)
        self.invoke("inspect", "alreadyPresent", jumps=hops)
        self.assertEqual(self.authorized_keys.read_bytes(), installed)
        self.invoke("remove", "removed", jumps=hops)
        self.assertEqual(self.authorized_keys.read_bytes(), b"#" + installed[1:])
        for server in hops:
            self.assertEqual(server.commands, [], server.host)
            self.assertEqual(list(server.home.iterdir()), [], server.host)
            self.assertEqual(server.events.count("channel"), 0, server.host)
            self.assertEqual(server.forwards, [server.forward_to] * 5, server.host)
            self.assertEqual(server.accepted_connections, server.completed_connections, server.host)
        self.assertEqual(len(self.final.commands), 5)

    def test_a_bastion_receives_an_enrollment_only_when_explicitly_selected_as_target(self):
        self.invoke("install", "installed", target=self.first, jumps=[])
        first_keys = self.first.home / ".ssh/authorized_keys"
        first_installed = first_keys.read_bytes()
        self.assertEqual(first_installed.count(self.phone_key.split()[1]), 1)
        self.assertEqual(len(self.first.commands), 2)
        self.assertEqual(self.first.forwards, [])
        self.invoke("install", "installed", target=self.second, jumps=[self.first])
        self.assertEqual(first_keys.read_bytes(), first_installed)
        self.assertEqual((self.second.home / ".ssh/authorized_keys").read_bytes().count(self.phone_key.split()[1]), 1)
        self.assertEqual(len(self.first.commands), 2)
        self.assertEqual(len(self.second.commands), 2)
        self.assertEqual(self.second.forwards, [])
        self.assertEqual(self.final.accepted_connections, 0)

    def assert_identity_failure_stops_at(self, server, host_trust):
        position = self.servers.index(server)
        if host_trust:
            wrong_host = public_line(ec.generate_private_key(ec.SECP256R1()))
            server.known_hosts.write_bytes(server.host_record(wrong_host))
        else:
            wrong_client = ed25519.Ed25519PrivateKey.generate()
            server.identity.write_bytes(wrong_client.private_bytes(serialization.Encoding.PEM,
                serialization.PrivateFormat.OpenSSH, serialization.NoEncryption()))
        self.invoke("install", "uncertain")
        self.assertEqual(server.accepted_connections, 1)
        self.assertEqual(server.events.count("authenticated"), 0)
        if not host_trust:
            self.assertGreater(server.events.count("authentication-rejected"), 0)
        self.assertEqual(server.forwards, [])
        for upstream in self.servers[:position]:
            self.assertEqual(upstream.events.count("authenticated"), 1)
            self.assertEqual(len(upstream.forwards), 1)
        for downstream in self.servers[position + 1:]:
            self.assertEqual(downstream.accepted_connections, 0,
                "a failed hop must never fall back to the directly reachable destination")
        self.assert_no_remote_effects()

    def test_first_bastion_wrong_host_key_prevents_all_forwarding_and_direct_fallback(self):
        self.assert_identity_failure_stops_at(self.first, host_trust=True)

    def test_second_bastion_wrong_host_key_prevents_final_connection(self):
        self.assert_identity_failure_stops_at(self.second, host_trust=True)

    def test_final_host_wrong_key_prevents_authentication_and_remote_exec(self):
        self.assert_identity_failure_stops_at(self.final, host_trust=True)

    def test_first_bastion_wrong_client_key_cannot_borrow_another_hops_identity(self):
        self.assert_identity_failure_stops_at(self.first, host_trust=False)

    def test_second_bastion_wrong_client_key_cannot_borrow_another_hops_identity(self):
        self.assert_identity_failure_stops_at(self.second, host_trust=False)

    def test_final_host_wrong_client_key_prevents_remote_exec(self):
        self.assert_identity_failure_stops_at(self.final, host_trust=False)

    def test_denied_direct_tcpip_request_cannot_fall_back_to_remote_exec_or_direct_connection(self):
        self.first.forward_to = (self.final.host, self.final.port)
        self.invoke("install", "uncertain")
        self.assertEqual(self.first.forward_requests, [(self.second.host, self.second.port)])
        self.assertEqual(self.first.forwards, [])
        self.assertEqual(self.second.accepted_connections, 0)
        self.assertEqual(self.final.accepted_connections, 0)
        self.assert_no_remote_effects()

    def assert_stalled_hop_cancellation(self, server, action):
        marker = self.directory / "cancel-after-real-hop-request"
        server.stall_forward = True
        server.forward_stall_marker = marker
        elapsed = self.invoke(action, "blocked" if action == "inspect" else "uncertain",
            cancellation_file=marker)
        self.assertTrue(server.forward_stalled.is_set())
        self.assertTrue(marker.exists())
        self.assertLess(elapsed, 5, "cancellation must retire proxy processes promptly")
        self.assertEqual(server.accepted_connections, server.completed_connections)
        self.assertEqual(server.forwards, [])
        self.assertEqual(self.final.accepted_connections, 0)
        self.assert_no_remote_effects()

    def test_cancelled_readback_closes_real_first_hop_without_reaching_next_hop(self):
        self.assert_stalled_hop_cancellation(self.first, "inspect")
        self.assertEqual(self.second.accepted_connections, 0)

    def test_cancelled_install_closes_both_real_hops_without_reaching_destination(self):
        self.assert_stalled_hop_cancellation(self.second, "install")
        self.assertEqual(len(self.first.forwards), 1)

    def test_lost_install_reply_through_two_hops_reconciles_without_duplicate_or_transit_edit(self):
        self.final.drop_next_acknowledgement = True
        self.invoke("install", "uncertain")
        self.assertFalse(self.final.drop_next_acknowledgement)
        installed = self.authorized_keys.read_bytes()
        self.assertEqual(installed.count(self.phone_key.split()[1]), 1)
        self.invoke("inspect", "alreadyPresent")
        self.invoke("install", "alreadyPresent")
        self.assertEqual(self.authorized_keys.read_bytes(), installed)
        self.assertEqual(len(self.final.commands), 4)
        self.assert_transit_untouched()

    def test_lost_removal_reply_through_two_hops_reconciles_without_another_edit(self):
        self.invoke("install", "installed")
        installed = self.authorized_keys.read_bytes()
        self.final.drop_next_removal_acknowledgement = True
        self.invoke("remove", "uncertain")
        self.assertFalse(self.final.drop_next_removal_acknowledgement)
        removed, modified = self.authorized_keys.read_bytes(), self.authorized_keys.stat().st_mtime_ns
        self.assertEqual(removed, b"#" + installed[1:])
        self.invoke("inspect", "absent")
        self.invoke("remove", "absent")
        self.assertEqual(self.authorized_keys.read_bytes(), removed)
        self.assertEqual(self.authorized_keys.stat().st_mtime_ns, modified)
        self.assert_transit_untouched()

    def test_another_bastion_path_cannot_remove_an_entry_owned_by_the_original_route(self):
        self.invoke("install", "installed")
        installed = self.authorized_keys.read_bytes()
        self.first.forward_to = (self.final.host, self.final.port)
        self.invoke("remove", "stillAuthorized", jumps=[self.first])
        self.assertEqual(self.authorized_keys.read_bytes(), installed)
        self.first.forward_to = (self.second.host, self.second.port)
        self.invoke("remove", "removed")
        self.assertEqual(self.authorized_keys.read_bytes(), b"#" + installed[1:])
        self.assertEqual(self.first.commands, [])
        self.assertEqual(self.second.commands, [])


if __name__ == "__main__":
    unittest.main()
