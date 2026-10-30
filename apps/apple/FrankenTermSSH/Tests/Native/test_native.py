"""Real loopback SSH interoperability. libssh is an independent server, not a mock.

Run: python Tests/Native/test_native.py /path/to/libftssh.so
Requires test-only Python cryptography and the system libssh server library.
No real key files, network destinations, subprocess shells or agents are used.
"""
import base64
import array
import ctypes as C
import ctypes.util
import fcntl
from functools import lru_cache
import json
from pathlib import Path
import select
import socket
import struct
import sys
import threading
import termios
import time
import unittest
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ed25519, ec, padding, rsa
from cryptography.hazmat.primitives.asymmetric.utils import decode_dss_signature

P, I, Z = C.c_void_p, C.c_int, C.c_size_t
SERVER = C.CDLL(ctypes.util.find_library("ssh"))
DRIVER = C.CDLL(sys.argv.pop(1))

def bind(lib, name, result, *args):
    f = getattr(lib, name)
    f.restype, f.argtypes = result, args
    return f

for name, result, args in [
    ("ssh_init", I, []), ("ssh_new", P, []), ("ssh_free", None, [P]),
    ("ssh_disconnect", None, [P]), ("ssh_set_blocking", None, [P, I]),
    ("ssh_send_ignore", I, [P, C.c_char_p]),
    ("ssh_bind_new", P, []), ("ssh_bind_free", None, [P]),
    ("ssh_bind_options_set", I, [P, I, P]), ("ssh_bind_accept_fd", I, [P, P, I]),
    ("ssh_handle_key_exchange", I, [P]), ("ssh_set_auth_methods", None, [P, I]),
    ("ssh_get_error", C.c_char_p, [P]),
    ("ssh_set_log_callback", I, [P]), ("ssh_set_log_level", I, [I]),
    ("ssh_pki_import_privkey_base64", I, [C.c_char_p, C.c_char_p, P, P, C.POINTER(P)]),
    ("ssh_pki_export_pubkey_base64", I, [P, C.POINTER(P)]),
    ("ssh_key_free", None, [P]), ("ssh_string_free_char", None, [P]),
    ("ssh_message_get", P, [P]), ("ssh_message_type", I, [P]), ("ssh_message_subtype", I, [P]),
    ("ssh_message_free", None, [P]), ("ssh_message_reply_default", I, [P]),
    ("ssh_message_service_reply_success", I, [P]), ("ssh_message_auth_user", C.c_char_p, [P]),
    ("ssh_message_auth_password", C.c_char_p, [P]),
    ("ssh_message_auth_kbdint_is_response", I, [P]),
    ("ssh_message_auth_interactive_request", I, [P, C.c_char_p, C.c_char_p, C.c_uint, C.POINTER(C.c_char_p), P]),
    ("ssh_userauth_kbdint_getnanswers", I, [P]),
    ("ssh_userauth_kbdint_getanswer", C.c_char_p, [P, C.c_uint]),
    ("ssh_message_auth_pubkey", P, [P]), ("ssh_message_auth_publickey_state", I, [P]),
    ("ssh_message_auth_reply_pk_ok_simple", I, [P]), ("ssh_message_auth_reply_success", I, [P, I]),
    ("ssh_message_auth_reply_pk_ok", I, [P, P, P]),
    ("ssh_string_from_char", P, [C.c_char_p]), ("ssh_string_free", None, [P]),
    ("ssh_message_channel_request_open_reply_accept", P, [P]),
    ("ssh_message_channel_request_open_destination", C.c_char_p, [P]),
    ("ssh_message_channel_request_open_destination_port", I, [P]),
    ("ssh_message_channel_request_reply_success", I, [P]),
    ("ssh_message_channel_request_pty_term", C.c_char_p, [P]),
    ("ssh_message_channel_request_pty_width", I, [P]), ("ssh_message_channel_request_pty_height", I, [P]),
    ("ssh_message_channel_request_command", C.c_char_p, [P]),
    ("ssh_channel_read_nonblocking", I, [P, P, C.c_uint32, I]),
    ("ssh_channel_write", I, [P, P, C.c_uint32]), ("ssh_channel_write_stderr", I, [P, P, C.c_uint32]),
    ("ssh_channel_send_eof", I, [P]), ("ssh_channel_close", I, [P]), ("ssh_channel_free", None, [P]),
    ("ssh_channel_request_send_exit_status", I, [P, I]),
    ("ssh_channel_request_send_exit_signal", I, [P, C.c_char_p, I, C.c_char_p, C.c_char_p]),
]:
    bind(SERVER, name, result, *args)
SERVER.ssh_init()

class Stats(C.Structure):
    _fields_ = [("allocated", Z), ("peak", Z), ("pending", Z),
                ("accepted", C.c_uint64), ("written", C.c_uint64), ("error", I),
                ("received", C.c_uint64)]

class Completion(C.Structure):
    _fields_ = [("kind", I), ("channel_closed", I), ("exit_status", C.c_uint32),
                ("signal", C.c_char * 65)]

for name, result, args in [
    ("ftssh_create", I, [I, Z, C.POINTER(P)]), ("ftssh_handshake", I, [P]),
    ("ftssh_copy_host_key", I, [P, P, Z, C.POINTER(Z)]), ("ftssh_copy_method", I, [P, I, P, Z]),
    ("ftssh_approve_host_key", I, [P, P, Z]),
    ("ftssh_begin_authentication", I, [P, P, Z, P, Z]), ("ftssh_authentication_step", I, [P]),
    ("ftssh_supports_publickey_interactive", I, []),
    ("ftssh_begin_publickey_interactive_authentication", I, [P, P, Z, P, Z, C.c_uint32]),
    ("ftssh_continue_interactive_authentication", I, [P]),
    ("ftssh_begin_password_authentication", I, [P, P, Z]), ("ftssh_supply_password", I, [P, P, Z]),
    ("ftssh_begin_interactive_authentication", I, [P, P, Z, C.c_uint32]),
    ("ftssh_copy_interactive_challenge", I, [P, P, Z, C.POINTER(Z), C.POINTER(C.c_uint32)]),
    ("ftssh_supply_interactive_responses", I, [P, C.c_uint32, P, Z]),
    ("ftssh_cancel_interactive_authentication", I, [P]),
    ("ftssh_finish_interactive_authentication", I, [P]),
    ("ftssh_copy_challenge", I, [P, P, Z, C.POINTER(Z)]), ("ftssh_supply_signature", I, [P, P, Z]),
    ("ftssh_begin_forward", I, [P, P, Z, C.c_uint16]), ("ftssh_forward_step", I, [P]),
    ("ftssh_forward_adopt", I, [C.POINTER(P), Z, C.POINTER(P)]),
    ("ftssh_begin_shell", I, [P, C.c_uint32, C.c_uint32, C.c_uint32, C.c_uint32]), ("ftssh_shell_step", I, [P]),
    ("ftssh_admit_write", I, [P, P, Z]), ("ftssh_flush_write", I, [P]),
    ("ftssh_read", I, [P, P, Z, C.POINTER(Z)]),
    ("ftssh_completion_step", I, [P, C.POINTER(Completion)]),
    ("ftssh_resize", I, [P, C.c_uint32, C.c_uint32, C.c_uint32, C.c_uint32]), ("ftssh_keepalive", I, [P]),
    ("ftssh_directions", I, [P]), ("ftssh_descriptor", I, [P]),
    ("ftssh_get_stats", I, [P, C.POINTER(Stats)]), ("ftssh_close", I, [P]), ("ftssh_destroy", I, [C.POINTER(P)]),
]:
    bind(DRIVER, name, result, *args)


def ssh_string(value):
    return struct.pack(">I", len(value)) + value


def public_blob(key):
    return base64.b64decode(key.public_key().public_bytes(serialization.Encoding.OpenSSH, serialization.PublicFormat.OpenSSH).split()[1])


@lru_cache(maxsize=3)
def rsa_client_key(bits):
    """Independent ssh-keygen/OpenSSL fixtures, shared with production import tests."""
    path = Path(__file__).resolve().parents[5] / "fixtures" / "ios-airgap" / "rsa-source-keys.json"
    record = next(item for item in json.loads(path.read_text())["keys"] if item["bits"] == bits)
    key = serialization.load_pem_private_key(record["pkcs8PEM"].encode("ascii"), password=None)
    if not isinstance(key, rsa.RSAPrivateKey) or key.key_size != bits:
        raise ValueError("RSA test fixture does not match its named size")
    return key


def take_string(data):
    n = int.from_bytes(data[:4], "big")
    if len(data) < 4 or n > len(data) - 4:
        raise ValueError("truncated SSH string")
    return data[4:4+n], data[4+n:]


def signature(key, data):
    if isinstance(key, ed25519.Ed25519PrivateKey):
        algorithm, payload = b"ssh-ed25519", key.sign(data)
    elif isinstance(key, rsa.RSAPrivateKey):
        _, rest = take_string(data)
        if not rest or rest[0] != 50:
            raise ValueError("RSA fixture requires an SSH authentication preimage")
        rest = rest[1:]
        for _ in range(3):
            _, rest = take_string(rest)
        if not rest or rest[0] != 1:
            raise ValueError("RSA fixture requires a signed authentication request")
        algorithm, _ = take_string(rest[1:])
        if algorithm not in (b"rsa-sha2-256", b"rsa-sha2-512"):
            raise ValueError("RSA fixture never signs SHA-1 or an unknown algorithm")
        digest = hashes.SHA256() if algorithm == b"rsa-sha2-256" else hashes.SHA512()
        payload = key.sign(data, padding.PKCS1v15(), digest)
    else:
        profiles = {"secp256r1": (b"ecdsa-sha2-nistp256", hashes.SHA256()),
                    "secp384r1": (b"ecdsa-sha2-nistp384", hashes.SHA384()),
                    "secp521r1": (b"ecdsa-sha2-nistp521", hashes.SHA512())}
        algorithm, digest = profiles[key.curve.name]
        r, s = decode_dss_signature(key.sign(data, ec.ECDSA(digest)))
        def mpint(n):
            raw = n.to_bytes((n.bit_length() + 7) // 8, "big")
            return ssh_string((b"\0" if raw[0] & 128 else b"") + raw)
        payload = mpint(r) + mpint(s)
    return ssh_string(algorithm) + ssh_string(payload)


class LoopbackServer:
    def __init__(self, key, reject=False, family=socket.AF_INET, completion="unknown",
                 auth_mode="normal", connections=1, host_key=None, host_signature=None,
                 forward_to=None, forward_host="target.internal.invalid", password=None, user_signature=None,
                 interactive_rounds=None, allow_exec=False):
        if not isinstance(connections, int) or not 1 <= connections <= 8:
            raise ValueError("test connection count must be bounded")
        if host_signature not in (None, "rsa-sha2-256", "rsa-sha2-512", "ssh-rsa"):
            raise ValueError("test host signature must be an exact supported fixture value")
        if user_signature not in (None, "rsa-sha2-256", "rsa-sha2-512", "ssh-rsa"):
            raise ValueError("test user signature must be an exact supported fixture value")
        if password is not None and (not isinstance(password, bytes) or not 1 <= len(password) <= 1024):
            raise ValueError("test password must be bounded synthetic bytes")
        self.public = public_blob(key)
        self.password = password
        self.allow_exec = allow_exec
        if interactive_rounds is not None:
            if not 1 <= len(interactive_rounds) <= 8:
                raise ValueError("test interactive rounds must be bounded")
            for challenge in interactive_rounds:
                if (len(challenge["prompts"]) > 16 or len(challenge["responses"]) != len(challenge["prompts"])
                        or len(challenge["name"]) > 2048 or len(challenge["instruction"]) > 16384
                        or any(len(prompt) > 16384 for prompt, _ in challenge["prompts"])
                        or any(len(response) > 2048 for response in challenge["responses"])):
                    raise ValueError("test interactive challenge must be bounded")
        self.interactive_rounds = interactive_rounds
        self.host_key = host_key or ec.generate_private_key(ec.SECP256R1())
        self.host_blob = public_blob(self.host_key)
        self.host_signature = host_signature
        self.user_signature = user_signature
        if forward_to is not None and forward_to[0] not in ("127.0.0.1", "::1"):
            raise ValueError("test relay destination must be loopback")
        self.forward_to, self.forward_host = forward_to, forward_host
        self.reject = reject
        self.completion = completion
        self.auth_mode = auth_mode
        self.connections = connections
        self.accepted_connections = 0
        self.output_eof = threading.Event()
        self.control_flood_ready = threading.Event()
        self.release_completion = threading.Event()
        self.events, self.received, self.errors = [], bytearray(), []
        self.invalid_signature = threading.Event()
        self.stop = threading.Event()
        self.done = threading.Event()
        self.listener = socket.socket(family, socket.SOCK_STREAM)
        self.listener.bind(("127.0.0.1" if family == socket.AF_INET else "::1", 0))
        self.listener.listen(1)
        self.listener.settimeout(0.1)
        self.address = self.listener.getsockname()
        self.control = None
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        try:
            for _ in range(self.connections):
                if self.stop.is_set() or self.errors:
                    break
                self.run_connection()
        finally:
            self.done.set()

    def run_connection(self):
        session, server, key, channel = None, None, P(), None
        control = None
        verification_observer = None
        try:
            if self.auth_mode == "observeInvalidSignature":
                # libssh drops cryptographically invalid USERAUTH packets
                # before its public message queue. Observe only this fixed
                # diagnostic on its thread-local public logging callback;
                # no key, password, payload or arbitrary trace is retained.
                @C.CFUNCTYPE(None, I, C.c_char_p, C.c_char_p, P)
                def verification_observer(priority, function, message, userdata):
                    if (function == b"ssh_packet_userauth_request" and message and
                            b"Received an invalid signature from peer" in message):
                        self.invalid_signature.set()
                if SERVER.ssh_set_log_callback(verification_observer) or SERVER.ssh_set_log_level(3):
                    raise RuntimeError("test verification observer failed")
            accept_deadline = time.monotonic() + 5
            while not self.stop.is_set():
                try:
                    sock, _ = self.listener.accept()
                    break
                except socket.timeout:
                    if time.monotonic() >= accept_deadline:
                        raise TimeoutError("test connection accept exceeded deadline")
            else:
                return
            self.accepted_connections += 1
            self.events.append(("connection", self.accepted_connections))
            control = sock.dup()
            self.control = control
            host_format = (serialization.PrivateFormat.OpenSSH
                           if isinstance(self.host_key, ed25519.Ed25519PrivateKey)
                           else serialization.PrivateFormat.TraditionalOpenSSL)
            pem = self.host_key.private_bytes(serialization.Encoding.PEM, host_format, serialization.NoEncryption())
            if SERVER.ssh_pki_import_privkey_base64(pem, None, None, None, C.byref(key)) != 0:
                raise RuntimeError("test host-key import failed")
            server, session = SERVER.ssh_bind_new(), SERVER.ssh_new()
            if SERVER.ssh_bind_options_set(server, 10, key) != 0:
                raise RuntimeError("test host-key selection failed")
            key = P()  # SSH_BIND_OPTIONS_IMPORT_KEY transfers ownership to bind.
            # Disable implicit system configuration in this isolated test server.
            disabled = C.c_bool(False)
            if SERVER.ssh_bind_options_set(server, 19, C.byref(disabled)) != 0:
                raise RuntimeError("test config isolation failed")
            # SSH_BIND_OPTIONS_HOSTKEY_ALGORITHMS is 18 in libssh's public
            # server.h. Offer exactly one signature algorithm; an RSA public
            # blob remains ssh-rsa even when the exchange uses RSA/SHA-2.
            if self.host_signature is not None:
                algorithm = C.c_char_p(self.host_signature.encode("ascii"))
                if SERVER.ssh_bind_options_set(server, 18, algorithm) != 0:
                    raise RuntimeError("test host signature selection failed")
            # The independent server both advertises this exact server-sig-algs
            # list and validates user signatures against it (public option 17).
            if self.user_signature is not None:
                algorithm = C.c_char_p(self.user_signature.encode("ascii"))
                if SERVER.ssh_bind_options_set(server, 17, algorithm) != 0:
                    raise RuntimeError("test user signature selection failed")
            if SERVER.ssh_bind_accept_fd(server, session, sock.detach()) != 0:
                raise RuntimeError("test accept failed")
            if SERVER.ssh_handle_key_exchange(session) != 0:
                return  # client rejection/cancellation is an exercised path
            password_method = self.auth_mode == "publickeyDisabled" or (
                self.password is not None and self.auth_mode != "passwordDisabled")
            interactive_method = self.interactive_rounds is not None and self.auth_mode != "interactiveDisabled"
            publickey_interactive = self.auth_mode.startswith("publickeyInteractive") or (
                self.interactive_rounds is not None and self.auth_mode in (
                    "rejectSignature", "unsignedSuccess", "unsignedPartial", "disconnectSignature"))
            # KI is deliberately absent before the key factor. The client must
            # use the actual signed failure's new method list for continuation.
            SERVER.ssh_set_auth_methods(session, 4 if publickey_interactive else
                                        16 if interactive_method else 2 if password_method else 4)
            key_factor_accepted = False
            interactive_round = 0
            if verification_observer is not None:
                SERVER.ssh_set_blocking(session, 0)
            while not self.stop.is_set():
                message = SERVER.ssh_message_get(session)
                if not message:
                    if verification_observer is not None:
                        if self.invalid_signature.is_set():
                            self.events.append(("invalid-signature",))
                            control.shutdown(socket.SHUT_RDWR)
                            return
                        self.stop.wait(0.001)
                        continue
                    return
                try:
                    kind, subtype = SERVER.ssh_message_type(message), SERVER.ssh_message_subtype(message)
                    if kind == 4:
                        SERVER.ssh_message_service_reply_success(message)
                    elif kind == 1 and subtype == 1:
                        self.events.append(("authentication-methods", SERVER.ssh_message_auth_user(message)))
                        if key_factor_accepted:
                            self.events.append(("unexpected-method-probe",))
                            control.shutdown(socket.SHUT_RDWR)
                            return
                        if self.auth_mode == "noneSucceeds":
                            SERVER.ssh_message_auth_reply_success(message, 0)
                        else:
                            SERVER.ssh_message_reply_default(message)
                    elif kind == 1 and subtype == 2:
                        user = SERVER.ssh_message_auth_user(message)
                        offered = SERVER.ssh_message_auth_password(message) or b""
                        matched = self.password is not None and offered == self.password
                        # Even synthetic credentials never enter event diagnostics.
                        self.events.append(("password-authentication", user, len(offered), matched))
                        if self.reject or not matched or user != b"operator":
                            SERVER.ssh_message_reply_default(message)
                        elif self.auth_mode == "passwordPartial":
                            SERVER.ssh_set_auth_methods(session, 4)
                            SERVER.ssh_message_auth_reply_success(message, 1)
                        elif self.auth_mode == "passwordChange":
                            # RFC 4252 assigns packet 60 to both PK_OK and
                            # PASSWD_CHANGEREQ. Both contain exactly two SSH
                            # strings. libssh's explicit public packet writer
                            # emits bSS without reading public-key state, so
                            # these strings produce a real encrypted password
                            # change request, not a mocked client result.
                            prompt = SERVER.ssh_string_from_char(b"Password expired")
                            language = SERVER.ssh_string_from_char(b"en")
                            try:
                                if not prompt or not language or SERVER.ssh_message_auth_reply_pk_ok(message, prompt, language):
                                    raise RuntimeError("test password-change response failed")
                            finally:
                                SERVER.ssh_string_free(prompt)
                                SERVER.ssh_string_free(language)
                        else:
                            SERVER.ssh_message_auth_reply_success(message, 0)
                    elif kind == 1 and subtype == 16:
                        user = SERVER.ssh_message_auth_user(message)
                        if SERVER.ssh_message_auth_kbdint_is_response(message):
                            if not self.interactive_rounds or not 1 <= interactive_round <= len(self.interactive_rounds):
                                raise RuntimeError("test received an unrequested interactive response")
                            expected = self.interactive_rounds[interactive_round - 1]["responses"]
                            count = SERVER.ssh_userauth_kbdint_getnanswers(session)
                            matched = count == len(expected) and all(
                                SERVER.ssh_userauth_kbdint_getanswer(session, index) == value
                                for index, value in enumerate(expected))
                            self.events.append(("interactive-response", interactive_round, count, matched))
                            if not matched or self.reject:
                                SERVER.ssh_message_reply_default(message)
                                continue
                            if interactive_round == len(self.interactive_rounds):
                                if self.auth_mode in ("interactivePartial", "publickeyInteractivePartial"):
                                    SERVER.ssh_set_auth_methods(session, 4)
                                    SERVER.ssh_message_auth_reply_success(message, 1)
                                else:
                                    SERVER.ssh_message_auth_reply_success(message, 0)
                                continue
                        else:
                            self.events.append(("interactive-request", user))
                            if (not interactive_method or user != b"operator" or self.reject or
                                    (publickey_interactive and not key_factor_accepted)):
                                SERVER.ssh_message_reply_default(message)
                                continue
                            if self.auth_mode in ("interactiveImmediateSuccess", "publickeyInteractiveImmediateSuccess"):
                                SERVER.ssh_message_auth_reply_success(message, 0)
                                continue
                            interactive_round = 0
                        challenge = self.interactive_rounds[interactive_round]
                        prompts = (C.c_char_p * len(challenge["prompts"]))(*(text for text, _ in challenge["prompts"]))
                        echo = (C.c_char * len(challenge["prompts"]))(*(bytes([int(echo)]) for _, echo in challenge["prompts"]))
                        if SERVER.ssh_message_auth_interactive_request(message, challenge["name"], challenge["instruction"],
                                len(challenge["prompts"]), prompts, echo):
                            raise RuntimeError("test interactive challenge send failed")
                        interactive_round += 1
                    elif kind == 1 and subtype == 4:
                        pubkey = SERVER.ssh_message_auth_pubkey(message)
                        encoded = P()
                        if not pubkey or SERVER.ssh_pki_export_pubkey_base64(pubkey, C.byref(encoded)):
                            raise RuntimeError("test public-key export failed")
                        try:
                            offered = base64.b64decode(C.string_at(encoded))
                        finally:
                            SERVER.ssh_string_free_char(encoded)
                        state = SERVER.ssh_message_auth_publickey_state(message)
                        user = SERVER.ssh_message_auth_user(message)
                        self.events.append(("authentication", state, user, offered))
                        if self.auth_mode == "disconnectOffer" or (self.auth_mode == "disconnectSignature" and state == 1):
                            control.shutdown(socket.SHUT_RDWR)
                            return
                        if self.reject or offered != self.public or user != b"operator":
                            SERVER.ssh_message_reply_default(message)
                        elif state == 0:
                            if self.auth_mode == "unsignedSuccess":
                                SERVER.ssh_message_auth_reply_success(message, 0)
                            elif self.auth_mode == "unsignedPartial":
                                SERVER.ssh_set_auth_methods(session, 16)
                                SERVER.ssh_message_auth_reply_success(message, 1)
                            else:
                                SERVER.ssh_message_auth_reply_pk_ok_simple(message)
                        elif state == 1:
                            if self.auth_mode == "rejectSignature":
                                SERVER.ssh_message_reply_default(message)
                            elif publickey_interactive:
                                key_factor_accepted = True
                                methods = (2 if self.auth_mode == "publickeyInteractiveNoKI" else
                                           18 if self.auth_mode == "publickeyInteractiveAlternatives" else 16)
                                SERVER.ssh_set_auth_methods(session, methods)
                                SERVER.ssh_message_auth_reply_success(message, 1)
                            elif self.auth_mode == "partial":
                                SERVER.ssh_set_auth_methods(session, 2)
                                SERVER.ssh_message_auth_reply_success(message, 1)
                            else:
                                SERVER.ssh_message_auth_reply_success(message, 0)
                        else:
                            SERVER.ssh_message_reply_default(message)
                    elif kind == 2 and subtype == 2 and self.forward_to is not None:
                        destination = SERVER.ssh_message_channel_request_open_destination(message)
                        port = SERVER.ssh_message_channel_request_open_destination_port(message)
                        self.events.append(("forward", destination, port))
                        if destination != self.forward_host.encode("ascii") or port != self.forward_to[1]:
                            SERVER.ssh_message_reply_default(message)
                            return
                        channel = SERVER.ssh_message_channel_request_open_reply_accept(message)
                        break
                    elif kind == 2 and subtype == 1:
                        channel = SERVER.ssh_message_channel_request_open_reply_accept(message)
                        self.events.append(("channel",))
                    elif kind == 3 and subtype == 1:
                        self.events.append(("pty", SERVER.ssh_message_channel_request_pty_term(message),
                                            SERVER.ssh_message_channel_request_pty_width(message), SERVER.ssh_message_channel_request_pty_height(message)))
                        SERVER.ssh_message_channel_request_reply_success(message)
                    elif kind == 3 and subtype == 3:
                        self.events.append(("shell",))
                        SERVER.ssh_message_channel_request_reply_success(message)
                        break
                    elif kind == 3 and subtype == 2 and self.allow_exec:
                        self.events.append(("exec", SERVER.ssh_message_channel_request_command(message)))
                        SERVER.ssh_message_channel_request_reply_success(message)
                        break
                    else:
                        SERVER.ssh_message_reply_default(message)
                finally:
                    SERVER.ssh_message_free(message)
            if not channel:
                return
            if self.forward_to is not None:
                self.forward(session, channel)
                return
            SERVER.ssh_channel_write(channel, b"ready\r\n", 7)
            SERVER.ssh_channel_write_stderr(channel, b"stderr\r\n", 8)
            SERVER.ssh_set_blocking(session, 0)
            pending = b""
            buffer = C.create_string_buffer(8192)
            while not self.stop.is_set():
                message = SERVER.ssh_message_get(session)
                if message:
                    try:
                        kind, subtype = SERVER.ssh_message_type(message), SERVER.ssh_message_subtype(message)
                        if kind == 3 and subtype == 6:
                            self.events.append(("resize", SERVER.ssh_message_channel_request_pty_width(message), SERVER.ssh_message_channel_request_pty_height(message)))
                            SERVER.ssh_message_channel_request_reply_success(message)
                        else:
                            SERVER.ssh_message_reply_default(message)
                    finally:
                        SERVER.ssh_message_free(message)
                if len(pending) < 32768:
                    n = SERVER.ssh_channel_read_nonblocking(channel, buffer, len(buffer), 0)
                    if n > 0:
                        data = buffer.raw[:n]
                        self.received += data
                        pending += data
                    elif n == -1:
                        return
                if pending:
                    n = SERVER.ssh_channel_write(channel, pending, min(8192, len(pending)))
                    if n > 0:
                        pending = pending[n:]
                if bytes(self.received).endswith(b"END-SESSION\r") and not pending:
                    SERVER.ssh_set_blocking(session, 1)
                    if SERVER.ssh_channel_send_eof(channel) != 0:
                        raise RuntimeError("test EOF send failed")
                    self.output_eof.set()
                    if self.completion == "controlFlood":
                        if not self.release_completion.wait(5):
                            raise RuntimeError("test control flood was never released")
                        for _ in range(32):
                            if SERVER.ssh_send_ignore(session, b"x" * 8192) != 0:
                                raise RuntimeError("test control flood send failed")
                        self.control_flood_ready.set()
                    if self.completion in ["delayedStatus", "resetAfterEOF"] and not self.release_completion.wait(5):
                        raise RuntimeError("test delayed completion was never released")
                    if self.completion in ["status7", "delayedStatus", "statusWithoutClose", "status0", "highStatus"]:
                        code = {"status0": 0, "highStatus": -1}.get(self.completion, 7)
                        if SERVER.ssh_channel_request_send_exit_status(channel, code) != 0:
                            raise RuntimeError("test exit status send failed")
                    elif self.completion == "signalTERM":
                        if SERVER.ssh_channel_request_send_exit_signal(channel, b"TERM", 0, b"", b"") != 0:
                            raise RuntimeError("test exit signal send failed")
                    if self.completion == "resetAfterEOF":
                        self.control.shutdown(socket.SHUT_RDWR)
                    elif self.completion in ["eofOnly", "statusWithoutClose", "controlFlood"]:
                        SERVER.ssh_set_blocking(session, 0)
                        while not self.stop.is_set():
                            if SERVER.ssh_channel_read_nonblocking(channel, buffer, len(buffer), 0) == -1:
                                break
                            self.stop.wait(0.01)
                    elif SERVER.ssh_channel_close(channel) != 0:
                        raise RuntimeError("test channel close failed")
                    return
                self.stop.wait(0.001)
        except Exception as e:
            if not self.stop.is_set():
                self.errors.append(repr(e))
        finally:
            if channel:
                SERVER.ssh_channel_free(channel)
            if session:
                SERVER.ssh_disconnect(session)
                SERVER.ssh_free(session)
            if server:
                SERVER.ssh_bind_free(server)
            if key:
                SERVER.ssh_key_free(key)
            # Preserve the fixture's final TCP lifetime until close(), so the
            # client can drain SSH channel-close evidence before socket EOF.
            # Earlier attempts retire completely before the next accept.
            if control and self.accepted_connections < self.connections:
                control.close()
                if self.control is control:
                    self.control = None

    def forward(self, session, channel):
        """Independent server direct-tcpip relay, with two bounded 32 KiB slots."""
        target = socket.create_connection(self.forward_to, timeout=2)
        target.setblocking(False)
        SERVER.ssh_set_blocking(session, 0)
        to_target, to_channel = b"", b""
        buffer = C.create_string_buffer(8192)
        ended = False
        try:
            while not self.stop.is_set():
                message = SERVER.ssh_message_get(session)
                if message:
                    SERVER.ssh_message_reply_default(message)
                    SERVER.ssh_message_free(message)
                if len(to_target) < 32768:
                    n = SERVER.ssh_channel_read_nonblocking(channel, buffer, min(len(buffer), 32768 - len(to_target)), 0)
                    if n > 0:
                        to_target += buffer.raw[:n]
                    elif n == -1:
                        return
                if to_target:
                    try:
                        n = target.send(to_target)
                        to_target = to_target[n:]
                    except BlockingIOError:
                        pass
                    except (BrokenPipeError, ConnectionResetError):
                        return
                if not ended and len(to_channel) < 32768:
                    try:
                        part = target.recv(min(8192, 32768 - len(to_channel)))
                        if part:
                            to_channel += part
                        else:
                            ended = True
                    except BlockingIOError:
                        pass
                    except ConnectionResetError:
                        return
                if to_channel:
                    n = SERVER.ssh_channel_write(channel, to_channel, min(8192, len(to_channel)))
                    if n > 0:
                        to_channel = to_channel[n:]
                    elif n == -1:
                        return
                if ended and not to_channel:
                    SERVER.ssh_channel_send_eof(channel)
                    return
                self.stop.wait(0.001)
        finally:
            target.close()

    def close(self):
        self.stop.set()
        self.release_completion.set()
        control = self.control
        if control:
            try:
                control.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            control.close()
        self.listener.close()
        self.thread.join(3)
        if self.thread.is_alive():
            raise AssertionError("independent server did not settle")
        if self.errors:
            raise AssertionError(self.errors)


class NativeSSHTests(unittest.TestCase):
    def setUp(self):
        self.handles, self.servers = [], []

    def tearDown(self):
        for handle in self.handles:
            if not handle.value:
                continue
            # A failed assertion during a conversation still joins its owned
            # worker before releasing any session or adopted parent.
            if DRIVER.ftssh_close(handle) == 1:
                self.assertEqual(DRIVER.ftssh_finish_interactive_authentication(handle), -1009)
            self.assertEqual(DRIVER.ftssh_close(handle), 0)
            stats = Stats()
            self.assertEqual(DRIVER.ftssh_get_stats(handle, C.byref(stats)), 0)
            self.assertEqual(stats.allocated, 0)
            self.assertEqual(DRIVER.ftssh_destroy(C.byref(handle)), 0)
            self.assertFalse(handle.value)
        for server in self.servers:
            server.close()

    def start(self, key=None, reject=False, family=socket.AF_INET, completion="unknown", auth_mode="normal", password=None,
              user_signature=None, interactive_rounds=None):
        key = key or ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
        server = LoopbackServer(key, reject, family, completion, auth_mode=auth_mode, password=password,
                                user_signature=user_signature, interactive_rounds=interactive_rounds)
        self.servers.append(server)
        sock = socket.socket(family, socket.SOCK_STREAM)
        sock.settimeout(3)
        sock.connect(server.address)
        sock.settimeout(None)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
        handle = P()
        self.assertEqual(DRIVER.ftssh_create(sock.fileno(), 2 * 1024 * 1024, C.byref(handle)), 0)
        sock.detach()
        self.handles.append(handle)
        return handle, server, key

    def advance(self, handle, operation, terminal=(0,), timeout=3):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            result = operation(handle)
            if result != 1:
                self.assertIn(result, terminal)
                return result
            self.wait(handle)
        self.fail("SSH operation exceeded test deadline")

    def wait(self, handle):
        fd, direction = DRIVER.ftssh_descriptor(handle), DRIVER.ftssh_directions(handle)
        if direction & 4:
            return
        select.select([fd] if direction & 1 or not direction else [], [fd] if direction & 2 else [], [], 0.01)

    def data(self, handle, function, cap=65536):
        buffer, count = C.create_string_buffer(cap), Z()
        self.assertEqual(function(handle, buffer, cap, C.byref(count)), 0)
        return buffer.raw[:count.value]

    def authenticate(self, handle, server, key, terminal=(0,), interactive_timeout=None, signing_delay=0):
        self.advance(handle, DRIVER.ftssh_handshake)
        actual = self.data(handle, DRIVER.ftssh_copy_host_key)
        self.assertEqual(actual, server.host_blob)
        self.assertEqual(DRIVER.ftssh_approve_host_key(handle, server.host_blob, len(server.host_blob)), 0)
        blob = public_blob(key)
        if interactive_timeout is None:
            self.assertEqual(DRIVER.ftssh_begin_authentication(handle, b"operator", 8, blob, len(blob)), 0)
        else:
            self.assertEqual(DRIVER.ftssh_begin_publickey_interactive_authentication(
                handle, b"operator", 8, blob, len(blob), interactive_timeout), 0)
        outcome = self.advance(handle, DRIVER.ftssh_authentication_step, (2, -1006) + terminal)
        if outcome != 2:
            return outcome
        challenge = self.data(handle, DRIVER.ftssh_copy_challenge, 1536)
        sid, rest = take_string(challenge)
        self.assertIn(len(sid), (32, 48, 64))
        algorithm, _ = take_string(blob)
        if algorithm == b"ssh-rsa":
            algorithm = (server.user_signature or "rsa-sha2-512").encode("ascii")
        expected = ssh_string(sid) + bytes([50]) + ssh_string(b"operator") + ssh_string(b"ssh-connection") + ssh_string(b"publickey") + b"\x01" + ssh_string(algorithm) + ssh_string(blob)
        self.assertEqual(challenge, expected)
        for _ in range(5):
            self.assertEqual(DRIVER.ftssh_authentication_step(handle), 2)
            self.assertEqual(self.data(handle, DRIVER.ftssh_copy_challenge, 1536), challenge)
        signed = signature(key, challenge)
        if signing_delay:
            time.sleep(signing_delay)
        self.assertEqual(DRIVER.ftssh_supply_signature(handle, signed, len(signed)), 0)
        self.assertEqual(DRIVER.ftssh_supply_signature(handle, signed, len(signed)), -1001)
        return self.advance(handle, DRIVER.ftssh_authentication_step, terminal)

    def request_password(self, handle, server, terminal=(5,)):
        self.advance(handle, DRIVER.ftssh_handshake)
        self.assertEqual(self.data(handle, DRIVER.ftssh_copy_host_key), server.host_blob)
        self.assertEqual(DRIVER.ftssh_approve_host_key(handle, server.host_blob, len(server.host_blob)), 0)
        self.assertEqual(DRIVER.ftssh_begin_password_authentication(handle, b"operator", 8), 0)
        self.assertEqual(DRIVER.ftssh_supply_password(handle, b"premature", 9), -1001)
        return self.advance(handle, DRIVER.ftssh_authentication_step, terminal)

    def password_events(self, server):
        return [event for event in server.events if event[0] == "password-authentication"]

    @staticmethod
    def interactive_round(responses=(b"synthetic-password",), prompts=None, name=b"Server challenge", instruction=b"Sign in"):
        if prompts is None:
            prompts = [(b"Password: ", False)] * len(responses)
        return {"name": name, "instruction": instruction, "prompts": prompts, "responses": list(responses)}

    def begin_interactive(self, handle, server, timeout=3000):
        self.advance(handle, DRIVER.ftssh_handshake)
        self.assertEqual(self.data(handle, DRIVER.ftssh_copy_host_key), server.host_blob)
        self.assertEqual(DRIVER.ftssh_approve_host_key(handle, server.host_blob, len(server.host_blob)), 0)
        self.assertEqual(DRIVER.ftssh_begin_interactive_authentication(handle, b"operator", 8, timeout), 0)

    def interactive_challenge(self, handle, expected_round, expected):
        self.assertEqual(self.advance(handle, DRIVER.ftssh_authentication_step, (6,)), 6)
        output, length, round_id = C.create_string_buffer(8192), Z(), C.c_uint32()
        self.assertEqual(DRIVER.ftssh_copy_interactive_challenge(handle, output, 1, C.byref(length), C.byref(round_id)), -1002)
        self.assertEqual((length.value, round_id.value), (0, 0))
        self.assertEqual(DRIVER.ftssh_copy_interactive_challenge(handle, output, len(output), C.byref(length), C.byref(round_id)), 0)
        self.assertEqual(round_id.value, expected_round)
        packet = ssh_string(expected["name"]) + ssh_string(expected["instruction"]) + struct.pack(">I", len(expected["prompts"]))
        packet += b"".join(bytes([int(echo)]) + ssh_string(text) for text, echo in expected["prompts"])
        self.assertEqual(output.raw[:length.value], packet)

    def answer_interactive(self, handle, round_id, answers):
        encoded = struct.pack(">I", len(answers)) + b"".join(ssh_string(value) for value in answers)
        owned = C.create_string_buffer(encoded)
        self.assertEqual(DRIVER.ftssh_supply_interactive_responses(handle, round_id, owned, len(encoded)), 0)
        C.memset(owned, 0, len(owned))
        self.assertEqual(DRIVER.ftssh_supply_interactive_responses(handle, round_id, encoded, len(encoded)), -1001)

    def finish_interactive(self, handle, expected=0):
        self.assertEqual(self.advance(handle, DRIVER.ftssh_authentication_step, (7,), timeout=5), 7)
        self.assertEqual(DRIVER.ftssh_finish_interactive_authentication(handle), expected)

    def test_combined_authentication_requires_approval_and_matching_backend(self):
        handle, server, key = self.start(auth_mode="publickeyInteractive",
                                        interactive_rounds=[self.interactive_round()])
        blob = public_blob(key)
        self.assertEqual(DRIVER.ftssh_begin_publickey_interactive_authentication(
            handle, b"operator", 8, blob, len(blob), 3000), -1001)
        self.assertEqual(DRIVER.ftssh_continue_interactive_authentication(handle), -1001)
        self.advance(handle, DRIVER.ftssh_handshake)
        self.assertEqual(DRIVER.ftssh_approve_host_key(handle, server.host_blob, len(server.host_blob)), 0)
        for timeout in [0, 120001]:
            self.assertEqual(DRIVER.ftssh_begin_publickey_interactive_authentication(
                handle, b"operator", 8, blob, len(blob), timeout), -1000)
        expected = 0 if DRIVER.ftssh_supports_publickey_interactive() else -1010
        self.assertEqual(DRIVER.ftssh_begin_publickey_interactive_authentication(
            handle, b"operator", 8, blob, len(blob), 3000), expected)
        self.assertEqual(DRIVER.ftssh_close(handle), 0)
        self.assertTrue(server.done.wait(2))
        self.assertFalse(any(event[0] in ("authentication", "interactive-request") for event in server.events))

    @unittest.skipUnless(DRIVER.ftssh_supports_publickey_interactive(), "patched backend required")
    def test_signed_publickey_partial_success_continues_to_real_multiround_terminal(self):
        choices = [
            (ed25519.Ed25519PrivateKey.generate(), None, "publickeyInteractive"),
            (ec.generate_private_key(ec.SECP256R1()), None, "publickeyInteractive"),
            (rsa_client_key(2048), "rsa-sha2-256", "publickeyInteractive"),
            (rsa_client_key(2048), "rsa-sha2-512", "publickeyInteractive"),
            (ed25519.Ed25519PrivateKey.generate(), None, "publickeyInteractiveAlternatives"),
        ]
        for key, algorithm, mode in choices:
            with self.subTest(algorithm=algorithm or type(key).__name__, mode=mode):
                rounds = [self.interactive_round([], [], instruction=b"Confirm this sign-in"),
                          self.interactive_round([b"654321"], [(b"One-time code: ", False)])]
                handle, server, _ = self.start(key=key, user_signature=algorithm, auth_mode=mode,
                                              interactive_rounds=rounds)
                self.assertEqual(self.authenticate(handle, server, key, terminal=(8,), interactive_timeout=3000), 8)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1001)
                self.assertEqual(DRIVER.ftssh_begin_forward(handle, b"target", 6, 22), -1001)
                self.assertEqual(DRIVER.ftssh_begin_password_authentication(handle, b"operator", 8), -1001)
                self.assertEqual(DRIVER.ftssh_begin_interactive_authentication(handle, b"changed", 7, 3000), -1001)
                self.assertEqual(DRIVER.ftssh_continue_interactive_authentication(handle), 0)
                self.assertEqual(DRIVER.ftssh_continue_interactive_authentication(handle), -1001)
                for number, challenge in enumerate(rounds, 1):
                    self.interactive_challenge(handle, number, challenge)
                    self.answer_interactive(handle, number, challenge["responses"])
                self.finish_interactive(handle)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), 0)
                self.advance(handle, DRIVER.ftssh_shell_step)
                self.assertEqual(DRIVER.ftssh_close(handle), 0)
                self.assertTrue(server.done.wait(2))
                self.assertEqual([e[1] for e in server.events if e[0] == "authentication"], [0, 1])
                self.assertEqual([e[1] for e in server.events if e[0] == "authentication-methods"], [b"operator"])
                self.assertEqual([e[1] for e in server.events if e[0] == "interactive-request"], [b"operator"])
                self.assertFalse(any(e[0] in ("unexpected-method-probe", "password-authentication") for e in server.events))
                self.assertTrue(any(e[0] == "shell" for e in server.events))

    @unittest.skipUnless(DRIVER.ftssh_supports_publickey_interactive(), "patched backend required")
    def test_combined_authentication_accepts_complete_key_success_without_prompt(self):
        handle, server, key = self.start()
        self.assertEqual(self.authenticate(handle, server, key, interactive_timeout=3000), 0)
        self.assertEqual(DRIVER.ftssh_continue_interactive_authentication(handle), -1001)
        self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), 0)
        self.advance(handle, DRIVER.ftssh_shell_step)
        self.assertFalse(any(e[0] == "interactive-request" for e in server.events))

    @unittest.skipUnless(DRIVER.ftssh_supports_publickey_interactive(), "patched backend required")
    def test_only_proven_signed_partial_success_can_start_second_factor(self):
        cases = [
            ("rejectSignature", False, -1011, [0, 1]),
            ("unsignedSuccess", False, -1010, [0]),
            ("unsignedPartial", False, -1011, [0]),
            ("publickeyInteractiveNoKI", False, -1010, [0, 1]),
            ("publickeyInteractive", True, -1006, [0]),
            ("noneSucceeds", False, -1010, []),
            ("publickeyDisabled", False, -1010, []),
            ("disconnectSignature", False, -1003, [0, 1]),
        ]
        for mode, reject, expected, states in cases:
            with self.subTest(mode=mode):
                handle, server, key = self.start(auth_mode=mode, reject=reject,
                                                interactive_rounds=[self.interactive_round()])
                self.assertEqual(self.authenticate(handle, server, key, terminal=(expected,),
                                                   interactive_timeout=3000), expected)
                self.assertEqual(DRIVER.ftssh_continue_interactive_authentication(handle), expected)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), expected)
                self.assertEqual(DRIVER.ftssh_close(handle), 0)
                self.assertTrue(server.done.wait(2))
                self.assertEqual([e[1] for e in server.events if e[0] == "authentication"], states)
                self.assertFalse(any(e[0] in ("interactive-request", "channel", "pty", "shell") for e in server.events))

    def test_key_only_api_never_uses_signed_partial_to_prompt_or_retry(self):
        handle, server, key = self.start(auth_mode="publickeyInteractive",
                                        interactive_rounds=[self.interactive_round()])
        self.assertEqual(self.authenticate(handle, server, key, terminal=(-1011,)), -1011)
        self.assertEqual(DRIVER.ftssh_continue_interactive_authentication(handle), -1011)
        self.assertEqual(DRIVER.ftssh_close(handle), 0)
        self.assertTrue(server.done.wait(2))
        self.assertEqual([e[1] for e in server.events if e[0] == "authentication"], [0, 1])
        self.assertFalse(any(e[0] in ("interactive-request", "channel", "pty", "shell") for e in server.events))

    @unittest.skipUnless(DRIVER.ftssh_supports_publickey_interactive(), "patched backend required")
    def test_second_factor_failure_partial_and_challengeless_success_are_terminal(self):
        for mode, answer, expected in [
                ("publickeyInteractive", b"wrong", -1013),
                ("publickeyInteractivePartial", b"123456", -1013),
                ("publickeyInteractiveImmediateSuccess", None, -1010)]:
            with self.subTest(mode=mode):
                challenge = self.interactive_round([b"123456"], [(b"Code: ", False)])
                handle, server, key = self.start(auth_mode=mode, interactive_rounds=[challenge])
                self.assertEqual(self.authenticate(handle, server, key, terminal=(8,), interactive_timeout=3000), 8)
                self.assertEqual(DRIVER.ftssh_continue_interactive_authentication(handle), 0)
                if answer is not None:
                    self.interactive_challenge(handle, 1, challenge)
                    self.answer_interactive(handle, 1, [answer])
                self.finish_interactive(handle, expected)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), expected)
                self.assertEqual(DRIVER.ftssh_continue_interactive_authentication(handle), expected)
                self.assertEqual(DRIVER.ftssh_close(handle), 0)
                self.assertTrue(server.done.wait(2))
                self.assertEqual([e[1] for e in server.events if e[0] == "authentication"], [0, 1])
                self.assertEqual(len([e for e in server.events if e[0] == "interactive-request"]), 1)
                self.assertFalse(any(e[0] in ("channel", "pty", "shell") for e in server.events))

    @unittest.skipUnless(DRIVER.ftssh_supports_publickey_interactive(), "patched backend required")
    def test_signing_and_interactive_prompt_share_one_native_deadline(self):
        challenge = self.interactive_round()
        handle, server, key = self.start(auth_mode="publickeyInteractive", interactive_rounds=[challenge])
        started = time.monotonic()
        self.assertEqual(self.authenticate(handle, server, key, terminal=(8,), interactive_timeout=1600,
                                           signing_delay=0.9), 8)
        self.assertEqual(DRIVER.ftssh_continue_interactive_authentication(handle), 0)
        self.interactive_challenge(handle, 1, challenge)
        while True:
            status = DRIVER.ftssh_authentication_step(handle)
            if status == 7:
                break
            self.assertIn(status, (1, 6))
            self.assertLess(time.monotonic() - started, 3)
            self.wait(handle)
        self.assertEqual(DRIVER.ftssh_finish_interactive_authentication(handle), -1014)
        self.assertLess(time.monotonic() - started, 2.25)
        self.assertFalse(any(e[0] == "interactive-response" for e in server.events))

    @unittest.skipUnless(DRIVER.ftssh_supports_publickey_interactive(), "patched backend required")
    def test_expired_partial_proof_cannot_start_a_new_factor_or_deadline(self):
        handle, server, key = self.start(auth_mode="publickeyInteractive",
                                        interactive_rounds=[self.interactive_round()])
        self.assertEqual(self.authenticate(handle, server, key, terminal=(8,), interactive_timeout=200), 8)
        time.sleep(0.25)
        self.assertEqual(DRIVER.ftssh_continue_interactive_authentication(handle), -1014)
        self.assertEqual(DRIVER.ftssh_begin_interactive_authentication(handle, b"operator", 8, 3000), -1014)
        self.assertEqual(DRIVER.ftssh_close(handle), 0)
        self.assertTrue(server.done.wait(2))
        self.assertFalse(any(e[0] == "interactive-request" for e in server.events))

    @unittest.skipUnless(DRIVER.ftssh_supports_publickey_interactive(), "patched backend required")
    def test_combined_prompt_cancellation_keeps_native_ownership_until_join(self):
        challenge = self.interactive_round()
        handle, server, key = self.start(auth_mode="publickeyInteractive", interactive_rounds=[challenge])
        self.assertEqual(self.authenticate(handle, server, key, terminal=(8,), interactive_timeout=3000), 8)
        self.assertEqual(DRIVER.ftssh_continue_interactive_authentication(handle), 0)
        self.interactive_challenge(handle, 1, challenge)
        pointer = handle.value
        self.assertEqual(DRIVER.ftssh_close(handle), 1)
        self.assertEqual(DRIVER.ftssh_destroy(C.byref(handle)), 1)
        self.assertEqual(handle.value, pointer)
        self.finish_interactive(handle, -1009)
        self.assertEqual(DRIVER.ftssh_close(handle), 0)
        self.assertTrue(server.done.wait(2))
        self.assertFalse(any(e[0] in ("interactive-response", "channel", "pty", "shell") for e in server.events))

    @unittest.skipUnless(DRIVER.ftssh_supports_publickey_interactive(), "patched backend required")
    def test_combined_authentication_owns_adopted_destination_and_transit_chain(self):
        for mfa_hop in [False, True]:
            with self.subTest(mfa_hop=mfa_hop):
                keys = [ed25519.Ed25519PrivateKey.generate(), ed25519.Ed25519PrivateKey.generate()]
                challenge = self.interactive_round([b"654321"], [(b"Code: ", False)])
                destination = LoopbackServer(keys[1], auth_mode="normal" if mfa_hop else "publickeyInteractive",
                                             interactive_rounds=None if mfa_hop else [challenge])
                hop = LoopbackServer(keys[0], auth_mode="publickeyInteractive" if mfa_hop else "normal",
                                     interactive_rounds=[challenge] if mfa_hop else None, forward_to=destination.address)
                self.servers.extend([hop, destination])
                sock = socket.create_connection(hop.address, timeout=3)
                sock.settimeout(None)
                handle = P()
                self.assertEqual(DRIVER.ftssh_create(sock.fileno(), 2 * 1024 * 1024, C.byref(handle)), 0)
                sock.detach(); self.handles.append(handle)
                for index, peer in enumerate([hop, destination]):
                    if peer.interactive_rounds:
                        self.assertEqual(self.authenticate(handle, peer, keys[index], terminal=(8,),
                                                           interactive_timeout=3000), 8)
                        self.assertEqual(DRIVER.ftssh_continue_interactive_authentication(handle), 0)
                        self.interactive_challenge(handle, 1, challenge)
                        self.answer_interactive(handle, 1, challenge["responses"])
                        self.finish_interactive(handle)
                    else:
                        self.assertEqual(self.authenticate(handle, peer, keys[index]), 0)
                    if index == 0:
                        handle = self.forwarded(handle, destination)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), 0)
                self.advance(handle, DRIVER.ftssh_shell_step)
                self.assertFalse(any(e[0] in ("channel", "pty", "shell") for e in hop.events))
                self.assertFalse(any(e[0] == "unexpected-method-probe" for peer in [hop, destination] for e in peer.events))

    def test_interactive_multiple_rounds_echo_and_information_reach_a_real_terminal(self):
        rounds = [
            self.interactive_round(["pāss 界".encode(), b"operator"], [(b"Password: ", False), (b"Account: ", True)]),
            self.interactive_round([], [], instruction=b"Confirm this sign-in"),
            self.interactive_round([b"123456"], [(b"One-time code: ", False)]),
        ]
        handle, server, _ = self.start(interactive_rounds=rounds)
        self.begin_interactive(handle, server)
        for number, challenge in enumerate(rounds, 1):
            self.interactive_challenge(handle, number, challenge)
            self.assertEqual(DRIVER.ftssh_get_stats(handle, C.byref(Stats())), -1001)
            self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1001)
            self.assertEqual(DRIVER.ftssh_shell_step(handle), -1001)
            self.assertEqual(DRIVER.ftssh_handshake(handle), -1001)
            self.answer_interactive(handle, number, challenge["responses"])
        self.finish_interactive(handle)
        self.assertEqual([event for event in server.events if event[0] == "interactive-response"],
                         [("interactive-response", 1, 2, True), ("interactive-response", 2, 0, True),
                          ("interactive-response", 3, 1, True)])
        self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), 0)
        self.advance(handle, DRIVER.ftssh_shell_step)
        payload = "Interactive → terminal 🦀\rEND-SESSION\r".encode()
        self.assertEqual(DRIVER.ftssh_admit_write(handle, payload, len(payload)), 0)
        received, eof = bytearray(), False
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            self.assertIn(DRIVER.ftssh_flush_write(handle), (0, 1))
            output, count = C.create_string_buffer(256), Z()
            result = DRIVER.ftssh_read(handle, output, len(output), C.byref(count))
            self.assertIn(result, (0, 1, 3))
            received += output.raw[:count.value]
            if result == 3:
                eof = True
                break
            self.wait(handle)
        self.assertTrue(eof)
        self.assertEqual(received, b"ready\r\nstderr\r\n" + payload)
        self.assertEqual(server.received, payload)

    def test_interactive_requires_exact_host_approval_and_bounded_start(self):
        handle, server, _ = self.start(interactive_rounds=[self.interactive_round()])
        self.assertEqual(DRIVER.ftssh_begin_interactive_authentication(handle, b"operator", 8, 3000), -1001)
        self.advance(handle, DRIVER.ftssh_handshake)
        self.assertEqual(DRIVER.ftssh_begin_interactive_authentication(handle, b"operator", 8, 3000), -1001)
        self.assertEqual(DRIVER.ftssh_approve_host_key(handle, server.host_blob, len(server.host_blob)), 0)
        for user, length, timeout in [(None, 1, 3000), (b"", 0, 3000), (b"bad\x00user", 8, 3000),
                                      (b"u" * 257, 257, 3000), (b"operator", 8, 0), (b"operator", 8, 120001)]:
            self.assertEqual(DRIVER.ftssh_begin_interactive_authentication(handle, user, length, timeout), -1000)
        self.assertEqual([event for event in server.events if event[0].startswith("authentication")], [])

    def test_interactive_replies_require_current_round_shape_utf8_and_capacity(self):
        challenge = self.interactive_round([b""], [(b"Optional response: ", False)])
        handle, server, _ = self.start(interactive_rounds=[challenge])
        self.begin_interactive(handle, server)
        self.interactive_challenge(handle, 1, challenge)
        for encoded in [b"", b"\x00\x00\x00", struct.pack(">I", 9),
                        struct.pack(">I", 1) + ssh_string(b"x" * 1025),
                        struct.pack(">I", 1) + ssh_string(b"a\x00b"),
                        struct.pack(">I", 1) + ssh_string(b"\xff"),
                        struct.pack(">I", 1) + ssh_string(b"") + b"trailing"]:
            self.assertEqual(DRIVER.ftssh_supply_interactive_responses(handle, 1, encoded, len(encoded)), -1000)
        encoded = struct.pack(">I", 1) + ssh_string(b"")
        self.assertEqual(DRIVER.ftssh_supply_interactive_responses(handle, 0, encoded, len(encoded)), -1001)
        self.assertEqual(DRIVER.ftssh_supply_interactive_responses(handle, 2, encoded, len(encoded)), -1001)
        self.assertEqual(DRIVER.ftssh_supply_interactive_responses(handle, 1, struct.pack(">I", 0), 4), -1001)
        self.answer_interactive(handle, 1, [b""])
        self.finish_interactive(handle)

    def test_interactive_all_eight_capacity_sized_responses_are_owned(self):
        challenge = self.interactive_round([bytes([65 + i]) * 1024 for i in range(8)])
        handle, server, _ = self.start(interactive_rounds=[challenge])
        self.begin_interactive(handle, server)
        self.interactive_challenge(handle, 1, challenge)
        self.answer_interactive(handle, 1, challenge["responses"])
        self.finish_interactive(handle)
        self.assertIn(("interactive-response", 1, 8, True), server.events)

    def test_interactive_ssh_boolean_echo_is_normalized_before_publication(self):
        # SSH booleans treat every nonzero octet as true. libssh2's decoder
        # normalizes that wire value before it reaches the callback contract.
        challenge = self.interactive_round([b"operator"], [(b"Account: ", 2)])
        handle, server, _ = self.start(interactive_rounds=[challenge])
        self.begin_interactive(handle, server)
        normalized = {**challenge, "prompts": [(b"Account: ", True)]}
        self.interactive_challenge(handle, 1, normalized)
        self.answer_interactive(handle, 1, challenge["responses"])
        self.finish_interactive(handle)

    def test_interactive_unsupported_none_and_challengeless_success_do_not_authenticate(self):
        for mode in ["interactiveDisabled", "noneSucceeds", "interactiveImmediateSuccess"]:
            with self.subTest(mode=mode):
                handle, server, _ = self.start(interactive_rounds=[self.interactive_round()], auth_mode=mode)
                self.begin_interactive(handle, server)
                self.finish_interactive(handle, -1010)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1010)
                self.assertEqual([event for event in server.events if event[0] == "interactive-response"], [])

    def test_interactive_wrong_or_partial_response_is_terminal_without_key_retry(self):
        for mode, response in [("normal", b"wrong"), ("interactivePartial", b"synthetic-password")]:
            with self.subTest(mode=mode):
                challenge = self.interactive_round()
                handle, server, key = self.start(interactive_rounds=[challenge], auth_mode=mode)
                self.begin_interactive(handle, server)
                self.interactive_challenge(handle, 1, challenge)
                self.answer_interactive(handle, 1, [response])
                self.finish_interactive(handle, -1013)
                blob = public_blob(key)
                self.assertEqual(DRIVER.ftssh_begin_authentication(handle, b"operator", 8, blob, len(blob)), -1013)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1013)
                self.assertEqual([event for event in server.events if event[0] in ("authentication", "channel", "shell")], [])

    def test_interactive_server_bounds_and_round_budget_fail_before_unbounded_prompting(self):
        cases = [
            (self.interactive_round(name=b"x" * 257), -1002),
            (self.interactive_round(instruction=b"x" * 1025), -1002),
            (self.interactive_round([b"x"] * 9), -1002),
            (self.interactive_round([b"x"], [(b"x" * 1025, False)]), -1007),
            (self.interactive_round([b"x"] * 8, [(b"x" * 1024, False)] * 8), -1007),
            (self.interactive_round([b"x"], [(b"\xed\xa0\x80", False)]), -1007),
        ]
        for challenge, failure in cases:
            with self.subTest(failure=failure, name_bytes=len(challenge["name"]), prompts=len(challenge["prompts"])):
                handle, server, _ = self.start(interactive_rounds=[challenge])
                self.begin_interactive(handle, server)
                self.finish_interactive(handle, failure)
                self.assertEqual([event for event in server.events if event[0] == "interactive-response"], [])
        challenge = self.interactive_round()
        handle, server, _ = self.start(interactive_rounds=[challenge] * 5)
        self.begin_interactive(handle, server)
        for number in range(1, 5):
            self.interactive_challenge(handle, number, challenge)
            self.answer_interactive(handle, number, challenge["responses"])
        self.finish_interactive(handle, -1002)
        self.assertEqual(len([event for event in server.events if event[0] == "interactive-response"]), 4)

    def test_interactive_cancellation_joins_before_native_destroy(self):
        challenge = self.interactive_round()
        handle, server, _ = self.start(interactive_rounds=[challenge])
        self.begin_interactive(handle, server)
        self.interactive_challenge(handle, 1, challenge)
        pointer = handle.value
        started = time.monotonic()
        self.assertEqual(DRIVER.ftssh_close(handle), 1)
        self.assertEqual(DRIVER.ftssh_destroy(C.byref(handle)), 1)
        self.assertEqual(handle.value, pointer)
        self.finish_interactive(handle, -1009)
        self.assertLess(time.monotonic() - started, 1)
        self.assertEqual(DRIVER.ftssh_close(handle), 0)
        self.assertEqual(DRIVER.ftssh_get_stats(handle, C.byref(Stats())), 0)
        self.assertEqual([event for event in server.events if event[0] == "interactive-response"], [])

    def test_interactive_native_deadline_expires_while_awaiting_user(self):
        challenge = self.interactive_round()
        handle, server, _ = self.start(interactive_rounds=[challenge])
        self.begin_interactive(handle, server, timeout=250)
        self.interactive_challenge(handle, 1, challenge)
        deadline = time.monotonic() + 2
        while True:
            result = DRIVER.ftssh_authentication_step(handle)
            if result == 7:
                break
            self.assertIn(result, (1, 6))
            self.assertLess(time.monotonic(), deadline)
            self.wait(handle)
        self.assertEqual(DRIVER.ftssh_finish_interactive_authentication(handle), -1014)
        self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1014)
        self.assertEqual([event for event in server.events if event[0] == "interactive-response"], [])

    def test_interactive_worker_admission_remains_charged_until_join(self):
        challenge = self.interactive_round()
        active = []
        for _ in range(2):
            handle, server, _ = self.start(interactive_rounds=[challenge])
            self.begin_interactive(handle, server)
            self.interactive_challenge(handle, 1, challenge)
            active.append(handle)
        third, server, _ = self.start(interactive_rounds=[challenge])
        self.advance(third, DRIVER.ftssh_handshake)
        self.assertEqual(DRIVER.ftssh_approve_host_key(third, server.host_blob, len(server.host_blob)), 0)
        self.assertEqual(DRIVER.ftssh_begin_interactive_authentication(third, b"operator", 8, 3000), -1002)
        self.assertEqual(DRIVER.ftssh_cancel_interactive_authentication(active[0]), 0)
        self.advance(active[0], DRIVER.ftssh_authentication_step, (7,))
        self.assertEqual(DRIVER.ftssh_begin_interactive_authentication(third, b"operator", 8, 3000), -1002)
        self.assertEqual(DRIVER.ftssh_finish_interactive_authentication(active[0]), -1009)
        self.assertEqual(DRIVER.ftssh_begin_interactive_authentication(third, b"operator", 8, 3000), 0)
        self.interactive_challenge(third, 1, challenge)
        for handle in [active[1], third]:
            self.assertEqual(DRIVER.ftssh_cancel_interactive_authentication(handle), 0)
            self.finish_interactive(handle, -1009)

    def test_interactive_destination_and_transit_own_the_complete_hop_chain(self):
        for interactive_hop in [False, True]:
            with self.subTest(interactive_hop=interactive_hop):
                keys = [ed25519.Ed25519PrivateKey.generate(), ed25519.Ed25519PrivateKey.generate()]
                challenge = self.interactive_round([b"654321"], [(b"Verification code: ", False)])
                destination = LoopbackServer(keys[1], interactive_rounds=None if interactive_hop else [challenge])
                hop = LoopbackServer(keys[0], interactive_rounds=[challenge] if interactive_hop else None,
                                     forward_to=destination.address)
                self.servers.extend([hop, destination])
                sock = socket.create_connection(hop.address, timeout=3)
                sock.settimeout(None)
                handle = P()
                self.assertEqual(DRIVER.ftssh_create(sock.fileno(), 2 * 1024 * 1024, C.byref(handle)), 0)
                sock.detach(); self.handles.append(handle)
                for index, peer in enumerate([hop, destination]):
                    if peer.interactive_rounds:
                        self.begin_interactive(handle, peer)
                        self.interactive_challenge(handle, 1, challenge)
                        self.answer_interactive(handle, 1, challenge["responses"])
                        self.finish_interactive(handle)
                    else:
                        self.assertEqual(self.authenticate(handle, peer, keys[index]), 0)
                    if index == 0:
                        handle = self.forwarded(handle, destination)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), 0)
                self.advance(handle, DRIVER.ftssh_shell_step)
                self.assertEqual([event for event in hop.events if event[0] in ("channel", "pty", "shell")], [])
                self.assertIn(("shell",), destination.events)

    def rsa_challenge(self, handle, server, key):
        self.advance(handle, DRIVER.ftssh_handshake)
        self.assertEqual(self.data(handle, DRIVER.ftssh_copy_host_key), server.host_blob)
        self.assertEqual(DRIVER.ftssh_approve_host_key(handle, server.host_blob, len(server.host_blob)), 0)
        blob = public_blob(key)
        self.assertEqual(DRIVER.ftssh_begin_authentication(handle, b"operator", 8, blob, len(blob)), 0)
        self.assertEqual(self.advance(handle, DRIVER.ftssh_authentication_step, (2,)), 2)
        return self.data(handle, DRIVER.ftssh_copy_challenge, 1536)

    def test_rsa_sha2_authentication_and_real_terminal_for_2048_4096_and_8192_bits(self):
        for bits in [2048, 4096, 8192]:
            for algorithm in ["rsa-sha2-256", "rsa-sha2-512"]:
                with self.subTest(bits=bits, algorithm=algorithm):
                    handle, server, key = self.start(rsa_client_key(bits), user_signature=algorithm)
                    self.assertEqual(self.authenticate(handle, server, key), 0)
                    self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), 0)
                    self.advance(handle, DRIVER.ftssh_shell_step)
                    payload = "RSA SHA-2 → terminal 🦀\rEND-SESSION\r".encode()
                    self.assertEqual(DRIVER.ftssh_admit_write(handle, payload, len(payload)), 0)
                    received, eof = bytearray(), False
                    deadline = time.monotonic() + 3
                    while time.monotonic() < deadline:
                        self.assertIn(DRIVER.ftssh_flush_write(handle), (0, 1))
                        out, count = C.create_string_buffer(256), Z()
                        rc = DRIVER.ftssh_read(handle, out, len(out), C.byref(count))
                        self.assertIn(rc, (0, 1, 3))
                        received += out.raw[:count.value]
                        if rc == 3:
                            eof = True
                            break
                        self.wait(handle)
                    self.assertTrue(eof)
                    self.assertEqual(received, b"ready\r\nstderr\r\n" + payload)
                    self.assertEqual(server.received, payload)
                    self.assertEqual([event[1] for event in server.events if event[0] == "authentication"], [0, 1])

    def test_rsa_signature_must_match_negotiated_algorithm_and_exact_modulus_width(self):
        handle, server, key = self.start(rsa_client_key(4096), user_signature="rsa-sha2-512")
        challenge = self.rsa_challenge(handle, server, key)
        signed = signature(key, challenge)
        algorithm, rest = take_string(signed)
        payload, trailing = take_string(rest)
        self.assertEqual((algorithm, len(payload), trailing), (b"rsa-sha2-512", 512, b""))
        malformed = [ssh_string(b"ssh-rsa") + ssh_string(payload),
                     ssh_string(b"rsa-sha2-256") + ssh_string(payload),
                     ssh_string(algorithm) + ssh_string(payload[:-1]),
                     ssh_string(algorithm) + ssh_string(b"\0" + payload), signed + b"\0"]
        for value in malformed:
            self.assertEqual(DRIVER.ftssh_supply_signature(handle, value, len(value)), -1007)
            self.assertEqual(DRIVER.ftssh_authentication_step(handle), 2)
            self.assertEqual(self.data(handle, DRIVER.ftssh_copy_challenge, 1536), challenge)
        self.assertEqual(DRIVER.ftssh_supply_signature(handle, signed, len(signed)), 0)
        self.assertEqual(self.advance(handle, DRIVER.ftssh_authentication_step), 0)
        self.assertEqual(DRIVER.ftssh_supply_signature(handle, signed, len(signed)), -1001)

    def test_rsa_wrong_hash_is_rejected_by_independent_server_without_retry(self):
        handle, server, key = self.start(rsa_client_key(2048), user_signature="rsa-sha2-512",
                                         auth_mode="observeInvalidSignature")
        challenge = self.rsa_challenge(handle, server, key)
        # Correct key and length, deliberately wrong digest under the claimed
        # algorithm. Only the independent SSH server verifies this signature.
        payload = key.sign(challenge, padding.PKCS1v15(), hashes.SHA256())
        signed = ssh_string(b"rsa-sha2-512") + ssh_string(payload)
        self.assertEqual(DRIVER.ftssh_supply_signature(handle, signed, len(signed)), 0)
        self.assertEqual(self.advance(handle, DRIVER.ftssh_authentication_step, (-1003,)), -1003)
        self.assertTrue(server.invalid_signature.is_set())
        self.assertIn(("invalid-signature",), server.events)
        self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1003)
        self.assertEqual([event[1] for event in server.events if event[0] == "authentication"], [0])
        self.assertEqual([event for event in server.events if event[0] in ("channel", "pty", "shell")], [])

    def test_rsa_public_admission_rejects_noncanonical_sizes_exponents_and_certificates(self):
        handle, server, _ = self.start(rsa_client_key(8192), user_signature="rsa-sha2-512")
        self.advance(handle, DRIVER.ftssh_handshake)
        self.assertEqual(DRIVER.ftssh_approve_host_key(handle, server.host_blob, len(server.host_blob)), 0)
        _, rest = take_string(server.public)
        exponent, rest = take_string(rest)
        modulus, trailing = take_string(rest)
        self.assertEqual(trailing, b"")
        def blob(e, n, kind=b"ssh-rsa"):
            return ssh_string(kind) + ssh_string(e) + ssh_string(n)
        cases = [(blob(b"\0", modulus), -1000), (blob(b"\1", modulus), -1000),
                 (blob(b"\2", modulus), -1000), (blob(b"\x80", modulus), -1000),
                 (blob(b"\0" + exponent, modulus), -1000),
                 (blob(b"\1" + b"\0" * 7 + b"\1", modulus), -1000),
                 (blob(exponent, modulus[1:]), -1000), (blob(exponent, b"\0" + modulus), -1000),
                 (blob(exponent, b"\0"), -1000), (blob(exponent, modulus[:-1] + b"\2"), -1000),
                 (blob(exponent, b"\x40" + b"\0" * 254 + b"\1"), -1010),
                 (server.public + b"\0", -1000),
                 # A user certificate is admitted as one: without its nonce
                 # and certificate fields, this one is malformed.
                 (blob(exponent, b"\0\x80" + b"\0" * 254 + b"\1",
                       b"ssh-rsa-cert-v01@openssh.com"), -1000)]
        for value, expected in cases:
            self.assertEqual(DRIVER.ftssh_begin_authentication(handle, b"operator", 8, value, len(value)), expected)
        # Largest canonical public encoding: a 64-bit positive exponent needs
        # its sign byte, as does the 8192-bit modulus. Admission never offers it.
        largest = blob(b"\0\x80" + b"\0" * 6 + b"\1", modulus)
        self.assertEqual(len(largest), 1053)
        self.assertEqual(DRIVER.ftssh_begin_authentication(handle, b"operator", 8, largest, len(largest)), 0)
        self.assertEqual(DRIVER.ftssh_close(handle), 0)
        self.assertTrue(server.done.wait(2))
        self.assertEqual([event for event in server.events if event[0].startswith("authentication")], [])

    def test_rsa_sha1_only_offer_never_requests_a_signature_or_opens_a_terminal(self):
        handle, server, key = self.start(rsa_client_key(2048), user_signature="ssh-rsa")
        self.assertEqual(self.authenticate(handle, server, key, (-1010,)), -1010)
        challenge, count = C.create_string_buffer(1536), Z()
        self.assertEqual(DRIVER.ftssh_copy_challenge(handle, challenge, len(challenge), C.byref(count)), -1010)
        self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1010)
        self.assertEqual([event for event in server.events if event[0] == "authentication"], [])
        self.assertEqual([event for event in server.events if event[0] in ("channel", "pty", "shell")], [])

    def test_rsa_unsigned_refusal_is_terminal_without_unproven_algorithm_retry(self):
        for algorithm in ["rsa-sha2-256", "rsa-sha2-512"]:
            with self.subTest(algorithm=algorithm):
                handle, server, key = self.start(rsa_client_key(2048), reject=True, user_signature=algorithm)
                self.assertEqual(self.authenticate(handle, server, key, (-1011,)), -1011)
                self.assertEqual(DRIVER.ftssh_authentication_step(handle), -1011)
                self.assertEqual([event[1] for event in server.events if event[0] == "authentication"], [0])
                self.assertEqual([event for event in server.events if event[0] in ("channel", "pty", "shell")], [])

    def test_rsa_destination_uses_independent_authentication_through_a_key_hop(self):
        self.exercise_forwarding_chain(1, destination_key=rsa_client_key(4096))

    def test_password_cannot_probe_or_submit_before_host_key_approval(self):
        handle, server, _ = self.start(password=b"test-password")
        self.assertEqual(DRIVER.ftssh_begin_password_authentication(handle, b"operator", 8), -1001)
        self.assertEqual(DRIVER.ftssh_supply_password(handle, b"test-password", 13), -1001)
        self.advance(handle, DRIVER.ftssh_handshake)
        self.assertEqual(DRIVER.ftssh_begin_password_authentication(handle, b"operator", 8), -1001)
        wrong = bytearray(server.host_blob)
        wrong[-1] ^= 1
        self.assertEqual(DRIVER.ftssh_approve_host_key(handle, bytes(wrong), len(wrong)), -1005)
        self.assertEqual(DRIVER.ftssh_begin_password_authentication(handle, b"operator", 8), -1005)
        self.assertEqual(DRIVER.ftssh_supply_password(handle, b"test-password", 13), -1005)
        self.assertEqual(DRIVER.ftssh_close(handle), 0)
        self.assertTrue(server.done.wait(2))
        self.assertEqual([event for event in server.events if event[0] != "connection"], [])

    def test_password_prompt_waits_for_supported_method_and_copies_unicode_once(self):
        password = "pāssphrase 界 🦀".encode("utf-8")
        handle, server, _ = self.start(password=password)
        self.assertEqual(self.request_password(handle, server), 5)
        self.assertEqual([event for event in server.events if event[0] == "authentication-methods"],
                         [("authentication-methods", b"operator")])
        for _ in range(5):
            self.assertEqual(DRIVER.ftssh_authentication_step(handle), 5)
        self.assertEqual(self.password_events(server), [])
        self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1001)
        challenge, length = C.create_string_buffer(1024), Z()
        self.assertEqual(DRIVER.ftssh_copy_challenge(handle, challenge, len(challenge), C.byref(length)), -1001)
        self.assertEqual(DRIVER.ftssh_supply_signature(handle, b"not-a-signature", 15), -1001)
        for value, count in [(None, 1), (b"", 0), (b"x" * 1025, 1025)]:
            self.assertEqual(DRIVER.ftssh_supply_password(handle, value, count), -1000)
        supplied = C.create_string_buffer(password)
        self.assertEqual(DRIVER.ftssh_supply_password(handle, supplied, len(password)), 0)
        C.memset(supplied, ord("x"), len(supplied))
        self.assertEqual(DRIVER.ftssh_supply_password(handle, b"second-attempt", 14), -1001)
        self.assertEqual(self.advance(handle, DRIVER.ftssh_authentication_step), 0)
        self.assertEqual(self.password_events(server), [("password-authentication", b"operator", len(password), True)])
        self.assertEqual([event for event in server.events if event[0] == "authentication"], [])
        self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), 0)
        self.advance(handle, DRIVER.ftssh_shell_step)
        self.assertIn(("pty", b"xterm-256color", 80, 24), server.events)
        self.assertIn(("shell",), server.events)

    def test_password_accepts_capacity_boundary_and_rejects_invalid_user_before_probe(self):
        password = b"p" * 1024
        handle, server, _ = self.start(password=password)
        self.advance(handle, DRIVER.ftssh_handshake)
        self.assertEqual(DRIVER.ftssh_approve_host_key(handle, server.host_blob, len(server.host_blob)), 0)
        for user, length in [(None, 1), (b"", 0), (b"u" * 257, 257), (b"bad\x00user", 8), (b"bad\nuser", 8)]:
            self.assertEqual(DRIVER.ftssh_begin_password_authentication(handle, user, length), -1000)
        self.assertEqual(DRIVER.ftssh_begin_password_authentication(handle, b"operator", 8), 0)
        self.assertEqual(self.advance(handle, DRIVER.ftssh_authentication_step, (5,)), 5)
        self.assertEqual(DRIVER.ftssh_supply_password(handle, password, len(password)), 0)
        self.assertEqual(self.advance(handle, DRIVER.ftssh_authentication_step), 0)
        self.assertEqual(self.password_events(server), [("password-authentication", b"operator", 1024, True)])

    def test_password_unsupported_and_none_success_never_request_a_secret(self):
        for mode in ["passwordDisabled", "noneSucceeds"]:
            with self.subTest(mode=mode):
                handle, server, _ = self.start(password=b"test-password", auth_mode=mode)
                self.assertEqual(self.request_password(handle, server, (-1010,)), -1010)
                self.assertEqual(DRIVER.ftssh_supply_password(handle, b"test-password", 13), -1010)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1010)
                self.assertEqual(self.password_events(server), [])
                self.assertEqual([event for event in server.events if event[0] in ("channel", "pty", "shell")], [])

    def test_password_wrong_partial_and_change_are_terminal_without_key_retry_evidence(self):
        for mode, supplied in [("normal", b"incorrect"), ("passwordPartial", b"test-password"),
                               ("passwordChange", b"test-password")]:
            with self.subTest(mode=mode):
                handle, server, key = self.start(password=b"test-password", auth_mode=mode)
                self.assertEqual(self.request_password(handle, server), 5)
                self.assertEqual(DRIVER.ftssh_supply_password(handle, supplied, len(supplied)), 0)
                self.assertEqual(self.advance(handle, DRIVER.ftssh_authentication_step, (-1012,)), -1012)
                self.assertEqual(DRIVER.ftssh_authentication_step(handle), -1012)
                self.assertEqual(DRIVER.ftssh_supply_password(handle, b"test-password", 13), -1012)
                blob = public_blob(key)
                self.assertEqual(DRIVER.ftssh_begin_authentication(handle, b"operator", 8, blob, len(blob)), -1012)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1012)
                self.assertEqual(self.password_events(server),
                                 [("password-authentication", b"operator", len(supplied), mode != "normal")])
                self.assertEqual([event for event in server.events if event[0] in ("authentication", "channel", "pty", "shell")], [])

    def test_password_prompt_cancellation_closes_without_secret_or_live_allocation(self):
        handle, server, _ = self.start(password=b"test-password")
        self.assertEqual(self.request_password(handle, server), 5)
        self.assertEqual(DRIVER.ftssh_close(handle), 0)
        self.assertEqual(DRIVER.ftssh_close(handle), 0)
        self.assertEqual(DRIVER.ftssh_descriptor(handle), -1)
        self.assertEqual(DRIVER.ftssh_supply_password(handle, b"test-password", 13), -1009)
        self.assertEqual(DRIVER.ftssh_authentication_step(handle), -1009)
        stats = Stats()
        self.assertEqual(DRIVER.ftssh_get_stats(handle, C.byref(stats)), 0)
        self.assertEqual((stats.allocated, stats.pending), (0, 0))
        self.assertTrue(server.done.wait(2))
        self.assertEqual(self.password_events(server), [])

    def test_password_and_key_can_authenticate_separate_forwarding_peers(self):
        for password_hop in [True, False]:
            with self.subTest(password_hop=password_hop):
                keys = [ed25519.Ed25519PrivateKey.generate(), ed25519.Ed25519PrivateKey.generate()]
                destination = LoopbackServer(keys[1], password=None if password_hop else b"destination-password")
                hop = LoopbackServer(keys[0], password=b"hop-password" if password_hop else None,
                                     forward_to=destination.address)
                self.servers.extend([hop, destination])
                sock = socket.create_connection(hop.address, timeout=3)
                sock.settimeout(None)
                handle = P()
                self.assertEqual(DRIVER.ftssh_create(sock.fileno(), 2 * 1024 * 1024, C.byref(handle)), 0)
                sock.detach(); self.handles.append(handle)
                for index, peer in enumerate([hop, destination]):
                    if peer.password is not None:
                        self.assertEqual(self.request_password(handle, peer), 5)
                        self.assertEqual(DRIVER.ftssh_supply_password(handle, peer.password, len(peer.password)), 0)
                        self.assertEqual(self.advance(handle, DRIVER.ftssh_authentication_step), 0)
                        self.assertEqual(self.password_events(peer),
                                         [("password-authentication", b"operator", len(peer.password), True)])
                        self.assertEqual([event for event in peer.events if event[0] == "authentication"], [])
                    else:
                        self.assertEqual(self.authenticate(handle, peer, keys[index]), 0)
                        self.assertEqual(self.password_events(peer), [])
                    if index == 0:
                        handle = self.forwarded(handle, destination)
                self.assertNotEqual(hop.host_blob, destination.host_blob)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), 0)
                self.advance(handle, DRIVER.ftssh_shell_step)
                self.assertEqual([event for event in hop.events if event[0] in ("channel", "pty", "shell")], [])
                self.assertIn(("shell",), destination.events)
                self.assertEqual(DRIVER.ftssh_close(handle), 0)
                stats = Stats()
                self.assertEqual(DRIVER.ftssh_get_stats(handle, C.byref(stats)), 0)
                self.assertEqual(stats.allocated, 0)
                self.assertTrue(hop.done.wait(2))
                self.assertTrue(destination.done.wait(2))

    def test_one_hop_owns_independent_exchange_and_transit_never_opens_a_shell(self):
        self.exercise_forwarding_chain(1)

    def test_two_hops_own_independent_exchanges_and_only_the_destination_terminal(self):
        self.exercise_forwarding_chain(2)

    def test_three_hops_preserve_backpressure_accounting_and_refuse_a_fourth_hop(self):
        self.exercise_forwarding_chain(3)

    def forwarding_chain(self, hop_count, destination_key=None):
        destination_key = destination_key or ed25519.Ed25519PrivateKey.generate()
        destination = LoopbackServer(destination_key)
        peers, keys = [destination], [destination_key]
        for _ in range(hop_count):
            key = ec.generate_private_key(ec.SECP256R1())
            peers.insert(0, LoopbackServer(key, forward_to=peers[0].address))
            keys.insert(0, key)
        self.servers.extend(peers)
        sock = socket.create_connection(peers[0].address, timeout=3)
        sock.settimeout(None)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
        handle = P()
        self.assertEqual(DRIVER.ftssh_create(sock.fileno(), 2 * 1024 * 1024, C.byref(handle)), 0)
        sock.detach(); self.handles.append(handle)
        return handle, peers, keys

    def forwarded(self, handle, destination):
        host = b"target.internal.invalid"
        self.assertEqual(DRIVER.ftssh_begin_forward(handle, host, len(host), destination.address[1]), 0)
        self.advance(handle, DRIVER.ftssh_forward_step)
        child = P()
        owned = handle.value
        self.assertEqual(DRIVER.ftssh_forward_adopt(C.byref(handle), 1, C.byref(child)), -1000)
        self.assertEqual(handle.value, owned)
        self.assertFalse(child.value)
        self.assertEqual(DRIVER.ftssh_forward_adopt(C.byref(handle), 2 * 1024 * 1024, C.byref(child)), 0)
        self.assertFalse(handle.value)
        self.handles.append(child)
        return child

    def exercise_forwarding_chain(self, hop_count, destination_key=None):
        child, peers, keys = self.forwarding_chain(hop_count, destination_key=destination_key)
        destination = peers[-1]
        for index, (peer, key) in enumerate(zip(peers, keys)):
            self.assertEqual(self.authenticate(child, peer, key), 0)
            if index < hop_count:
                child = self.forwarded(child, peers[index + 1])
        if hop_count == 3:
            host = b"target.internal.invalid"
            self.assertEqual(DRIVER.ftssh_begin_forward(child, host, len(host), destination.address[1]), -1010)
        self.assertEqual(DRIVER.ftssh_begin_shell(child, 80, 24, 0, 0), 0)
        self.advance(child, DRIVER.ftssh_shell_step)
        self.advance(child, lambda h: DRIVER.ftssh_resize(h, 132, 42, 1000, 700))
        # Cross the outer receive window several times in both directions.
        # The inner write may need a parent WINDOW_ADJUST read, and every
        # retried packet must retain its original immutable contents.
        payload = (("Nested SSH → independent keys.\r\n" * 100).encode()
                   + bytes(range(256)) * 2048 + b"END-SESSION\r")
        admitted, received, eof = 0, bytearray(), False
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if admitted < len(payload):
                chunk = payload[admitted:admitted+16384]
                rc = DRIVER.ftssh_admit_write(child, chunk, len(chunk))
                self.assertIn(rc, (0, 1))
                if rc == 0:
                    admitted += len(chunk)
                    self.assertEqual(DRIVER.ftssh_admit_write(child, b"NOT-ADMITTED", 12), 1)
            self.assertIn(DRIVER.ftssh_flush_write(child), (0, 1))
            out, count = C.create_string_buffer(8192), Z()
            rc = DRIVER.ftssh_read(child, out, len(out), C.byref(count))
            self.assertIn(rc, (0, 1, 3))
            received += out.raw[:count.value]
            stats = Stats()
            self.assertEqual(DRIVER.ftssh_get_stats(child, C.byref(stats)), 0)
            self.assertLessEqual(stats.pending, 16384)
            self.assertLessEqual(stats.peak, (hop_count + 1) * 2 * 1024 * 1024)
            if rc == 3:
                eof = True
                break
            self.wait(child)
        self.assertTrue(eof, {"hops": hop_count, "admitted": admitted, "written": stats.written,
                              "pending": stats.pending, "received": len(received), "server_received": len(destination.received),
                              "directions": DRIVER.ftssh_directions(child), "server_errors": [peer.errors for peer in peers]})
        self.assertEqual(admitted, len(payload))
        self.assertEqual(stats.accepted, len(payload))
        self.assertEqual(stats.written, len(payload))
        self.assertEqual(received, b"ready\r\nstderr\r\n" + payload)
        self.assertEqual(destination.received, payload)
        self.assertIn(("resize", 132, 42), destination.events)
        self.assertEqual(len({peer.host_blob for peer in peers}), hop_count + 1)
        self.assertEqual(len({public_blob(key) for key in keys}), hop_count + 1)
        for index, peer in enumerate(peers):
            authentication = [e for e in peer.events if e[0] == "authentication"]
            self.assertEqual([e[1] for e in authentication], [0, 1])
            self.assertTrue(all(e[3] == public_blob(keys[index]) for e in authentication))
            if index < hop_count:
                self.assertEqual([e for e in peer.events if e[0] in ["channel", "pty", "shell", "resize"]], [])
                self.assertEqual([e for e in peer.events if e[0] == "forward"],
                                 [("forward", b"target.internal.invalid", peers[index + 1].address[1])])
        self.assertEqual(DRIVER.ftssh_close(child), 0)
        stats = Stats()
        self.assertEqual(DRIVER.ftssh_get_stats(child, C.byref(stats)), 0)
        self.assertEqual(stats.allocated, 0)
        self.assertLessEqual(stats.peak, (hop_count + 1) * 2 * 1024 * 1024)
        for peer in peers:
            self.assertTrue(peer.done.wait(2), "closing the destination retained an ancestor transport")

    def test_deep_host_pin_rejection_closes_every_ancestor_without_offering_a_key(self):
        child, peers, keys = self.forwarding_chain(3)
        for index in range(2):
            self.assertEqual(self.authenticate(child, peers[index], keys[index]), 0)
            child = self.forwarded(child, peers[index + 1])
        self.advance(child, DRIVER.ftssh_handshake)
        self.assertEqual(self.data(child, DRIVER.ftssh_copy_host_key), peers[2].host_blob)
        wrong = peers[0].host_blob
        self.assertEqual(DRIVER.ftssh_approve_host_key(child, wrong, len(wrong)), -1005)
        self.assertEqual([e for e in peers[2].events if e[0] == "authentication"], [])
        self.assertEqual(peers[3].accepted_connections, 0)
        self.assertEqual(DRIVER.ftssh_close(child), 0)
        stats = Stats()
        self.assertEqual(DRIVER.ftssh_get_stats(child, C.byref(stats)), 0)
        self.assertEqual(stats.allocated, 0)
        for peer in peers[:3]:
            self.assertTrue(peer.done.wait(2), "deep host rejection retained an ancestor transport")
        for peer in peers:
            self.assertEqual([e for e in peer.events if e[0] in ["channel", "pty", "shell"]], [])

    def test_three_hop_close_with_inflight_output_releases_all_owned_buffers(self):
        child, peers, keys = self.forwarding_chain(3)
        for index, (peer, key) in enumerate(zip(peers, keys)):
            self.assertEqual(self.authenticate(child, peer, key), 0)
            if index < 3:
                child = self.forwarded(child, peers[index + 1])
        self.assertEqual(DRIVER.ftssh_begin_shell(child, 80, 24, 0, 0), 0)
        self.advance(child, DRIVER.ftssh_shell_step)
        payload = b"inflight-chain-close\r\n" * 700
        stats = Stats()
        for _ in range(128):
            self.assertIn(DRIVER.ftssh_admit_write(child, payload, len(payload)), (0, 1))
            self.assertIn(DRIVER.ftssh_flush_write(child), (0, 1))
            self.assertEqual(DRIVER.ftssh_get_stats(child, C.byref(stats)), 0)
            if stats.pending:
                break
        self.assertGreater(stats.pending, 0, "fixture did not retain a pending physical write receipt")
        self.assertLess(stats.written, stats.accepted)
        self.assertEqual(DRIVER.ftssh_close(child), 0)
        self.assertEqual(DRIVER.ftssh_get_stats(child, C.byref(stats)), 0)
        self.assertEqual(stats.allocated, 0)
        self.assertEqual(stats.pending, 0)
        for peer in peers:
            self.assertTrue(peer.done.wait(2), "closing in-flight output retained an ancestor transport")

    def test_real_authentication_pty_unicode_data_and_resize_for_four_key_families(self):
        keys = [ed25519.Ed25519PrivateKey.generate()] + [ec.generate_private_key(c) for c in [ec.SECP256R1(), ec.SECP384R1(), ec.SECP521R1()]]
        for key in keys:
            with self.subTest(key=type(key).__name__, curve=getattr(key, "curve", None)):
                handle, server, key = self.start(key)
                self.assertEqual(self.authenticate(handle, server, key), 0)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 640, 480), 0)
                self.advance(handle, DRIVER.ftssh_shell_step)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1001)
                self.advance(handle, lambda h: DRIVER.ftssh_resize(h, 132, 42, 1000, 700))
                payload = ("echo naïve 界 🦀\r\n" * 100).encode() + b"END-SESSION\r"
                self.assertEqual(DRIVER.ftssh_admit_write(handle, payload, len(payload)), 0)
                self.assertEqual(DRIVER.ftssh_admit_write(handle, b"duplicate", 9), 1)
                received = bytearray()
                end = time.monotonic() + 5
                eof = False
                while time.monotonic() < end:
                    self.assertIn(DRIVER.ftssh_flush_write(handle), (0, 1))
                    out, count = C.create_string_buffer(257), Z()
                    rc = DRIVER.ftssh_read(handle, out, len(out), C.byref(count))
                    self.assertIn(rc, (0, 1, 3))
                    received += out.raw[:count.value]
                    if rc == 3:
                        eof = True
                        break
                    self.wait(handle)
                self.assertTrue(eof)
                self.assertEqual(received, b"ready\r\nstderr\r\n" + payload)
                self.assertEqual(server.received, payload)
                self.assertIn(("pty", b"xterm-256color", 80, 24), server.events)
                self.assertIn(("resize", 132, 42), server.events)
                self.assertEqual([e[1] for e in server.events if e[0] == "authentication"], [0, 1])
                stats = Stats()
                DRIVER.ftssh_get_stats(handle, C.byref(stats))
                self.assertEqual(stats.pending, 0)
                self.assertEqual(stats.accepted, len(payload))
                self.assertEqual(stats.written, len(payload))
                self.assertLessEqual(stats.peak, 2 * 1024 * 1024)

    def test_remote_completion_preserves_nonzero_signal_and_unknown_zero(self):
        for mode, kind, code, signal in [("status7", 1, 7, b""), ("highStatus", 1, 0xffffffff, b""),
                                         ("signalTERM", 2, 0, b"TERM"), ("status0", 0, 0, b""),
                                         ("unknown", 0, 0, b""), ("delayedStatus", 1, 7, b"")]:
            with self.subTest(mode=mode):
                handle, server, key = self.start(completion=mode)
                self.assertEqual(self.authenticate(handle, server, key), 0)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), 0)
                self.advance(handle, DRIVER.ftssh_shell_step)
                evidence = Completion()
                self.assertEqual(DRIVER.ftssh_completion_step(handle, C.byref(evidence)), -1001)
                payload = b"final output before completion\r\nEND-SESSION\r"
                self.assertEqual(DRIVER.ftssh_admit_write(handle, payload, len(payload)), 0)
                received, eof = bytearray(), False
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    self.assertIn(DRIVER.ftssh_flush_write(handle), (0, 1))
                    out, count = C.create_string_buffer(19), Z()
                    result = DRIVER.ftssh_read(handle, out, len(out), C.byref(count))
                    self.assertIn(result, (0, 1, 3))
                    received += out.raw[:count.value]
                    if result == 3:
                        eof = True
                        break
                    self.wait(handle)
                self.assertTrue(eof)
                self.assertEqual(received, b"ready\r\nstderr\r\n" + payload)
                if mode == "delayedStatus":
                    self.assertTrue(server.output_eof.is_set())
                    self.assertEqual(DRIVER.ftssh_completion_step(handle, C.byref(evidence)), 1)
                    self.assertEqual((evidence.kind, evidence.channel_closed), (0, 0))
                    server.release_completion.set()
                closed = False
                while time.monotonic() < deadline:
                    result = DRIVER.ftssh_completion_step(handle, C.byref(evidence))
                    self.assertIn(result, (0, 1, 4))
                    if result == 0:
                        closed = True
                        break
                    if result == 1:
                        self.wait(handle)
                self.assertTrue(closed)
                self.assertEqual((evidence.kind, evidence.exit_status, evidence.signal, evidence.channel_closed),
                                 (kind, code, signal, 1))

    def test_stats_abi_keeps_guard_bytes_and_reports_received_traffic(self):
        class GuardedStats(C.Structure):
            _fields_ = [("before", C.c_uint64), ("stats", Stats), ("after", C.c_uint64)]
        handle, server, key = self.start()
        self.assertEqual(self.authenticate(handle, server, key), 0)
        guarded = GuardedStats()
        guarded.before = 0x123456789abcdef0
        guarded.after = 0xfedcba9876543210
        pointer = C.cast(C.byref(guarded, GuardedStats.stats.offset), C.POINTER(Stats))
        self.assertEqual(DRIVER.ftssh_get_stats(handle, pointer), 0)
        self.assertEqual(guarded.before, 0x123456789abcdef0)
        self.assertEqual(guarded.after, 0xfedcba9876543210)
        self.assertGreater(guarded.stats.received, 0)

    def test_a_keepalive_libssh2_holds_back_is_reported_as_not_sent(self):
        handle, server, key = self.start()
        self.assertEqual(self.authenticate(handle, server, key), 0)
        self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), 0)
        self.advance(handle, DRIVER.ftssh_shell_step)
        self.assertEqual(DRIVER.ftssh_keepalive(handle), 0)
        # Once the socket buffer is empty again (the 4 KiB one start() sets
        # defers any probe while bytes are queued), libssh2 is asked, and it
        # sends at most one per 2 s, reporting a skipped one as sent: no
        # reply is due for it.
        time.sleep(0.3)
        self.assertEqual(DRIVER.ftssh_keepalive(handle), 1)
        time.sleep(3)
        self.assertEqual(DRIVER.ftssh_keepalive(handle), 0)

    def handshake_against(self, serve):
        """The handshake's result against a raw server running serve(peer, finished)."""
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        finished, errors = threading.Event(), []
        def run():
            peer, _ = listener.accept()
            peer.settimeout(3)
            try:
                serve(peer, finished)
            except Exception as error:
                errors.append(error)
            finally:
                peer.close()
        thread = threading.Thread(target=run)
        thread.start()
        sock = socket.create_connection(listener.getsockname(), timeout=3)
        sock.settimeout(None)
        handle = P()
        self.assertEqual(DRIVER.ftssh_create(sock.fileno(), 2 * 1024 * 1024, C.byref(handle)), 0)
        sock.detach()
        self.handles.append(handle)
        try:
            return self.advance(handle, DRIVER.ftssh_handshake, (-1003, -1004))
        finally:
            finished.set()
            thread.join(3)
            listener.close()
            self.assertFalse(errors)

    def test_a_transport_lost_mid_exchange_is_not_a_negotiation_answer(self):
        def read_client_kexinit(peer):
            seen = b""
            while b"\r\n" not in seen or len(seen.split(b"\r\n", 1)[1]) < 6:
                part = peer.recv(4096)
                self.assertTrue(part, "the client sent its key exchange")
                seen += part
        for reset in [False, True]:
            with self.subTest(reset=reset):
                def lost(peer, finished):
                    peer.sendall(b"SSH-2.0-FrankenTermTest\r\n")
                    read_client_kexinit(peer)
                    if reset:
                        peer.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
                self.assertEqual(self.handshake_against(lost), -1003)
        # A server that answers with no cipher this client accepts is an answer.
        payload = bytes([20]) + bytes(16) + b"".join(ssh_string(name) for name in [
            b"curve25519-sha256", b"ssh-ed25519", b"3des-cbc", b"3des-cbc", b"hmac-sha2-256-etm@openssh.com",
            b"hmac-sha2-256-etm@openssh.com", b"none", b"none", b"", b""]) + b"\x00" + bytes(4)
        padding = 8 - (5 + len(payload)) % 8
        padding += 8 if padding < 4 else 0
        kexinit = struct.pack(">IB", 1 + len(payload) + padding, padding) + payload + bytes(padding)
        def mismatched(peer, finished):
            peer.sendall(b"SSH-2.0-FrankenTermTest\r\n" + kexinit)
            finished.wait(5)
        self.assertEqual(self.handshake_against(mismatched), -1004)

    def test_completion_control_flood_yields_after_bounded_receive_quantum(self):
        handle, server, key = self.start(completion="controlFlood")
        self.assertEqual(self.authenticate(handle, server, key), 0)
        self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), 0)
        self.advance(handle, DRIVER.ftssh_shell_step)
        payload = b"END-SESSION\r"
        self.assertEqual(DRIVER.ftssh_admit_write(handle, payload, len(payload)), 0)
        deadline, eof = time.monotonic() + 5, False
        while time.monotonic() < deadline:
            self.assertIn(DRIVER.ftssh_flush_write(handle), (0, 1))
            out, count = C.create_string_buffer(256), Z()
            result = DRIVER.ftssh_read(handle, out, len(out), C.byref(count))
            self.assertIn(result, (0, 1, 3))
            if result == 3:
                eof = True
                break
            self.wait(handle)
        self.assertTrue(eof)
        server.release_completion.set()
        self.assertTrue(server.control_flood_ready.wait(2))
        # Prove the receive quantum is already available before measuring it;
        # a successful server send alone does not prove network arrival.
        queued = array.array("i", [0])
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            fcntl.ioctl(DRIVER.ftssh_descriptor(handle), termios.FIONREAD, queued, True)
            if queued[0] >= 65536:
                break
            server.stop.wait(0.001)
        self.assertGreaterEqual(queued[0], 65536)
        before, after, evidence = Stats(), Stats(), Completion()
        self.assertEqual(DRIVER.ftssh_get_stats(handle, C.byref(before)), 0)
        started = time.monotonic()
        self.assertEqual(DRIVER.ftssh_completion_step(handle, C.byref(evidence)), 4)
        self.assertLess(time.monotonic() - started, 1)
        self.assertEqual(DRIVER.ftssh_get_stats(handle, C.byref(after)), 0)
        self.assertLessEqual(after.received - before.received, 32768)
        self.assertEqual((evidence.kind, evidence.channel_closed), (0, 0))

    def test_host_mismatch_stops_before_any_authentication(self):
        handle, server, key = self.start()
        self.advance(handle, DRIVER.ftssh_handshake)
        blob = public_blob(key)
        self.assertEqual(DRIVER.ftssh_begin_authentication(handle, b"operator", 8, blob, len(blob)), -1001)
        wrong = bytearray(server.host_blob); wrong[-1] ^= 1
        self.assertEqual(DRIVER.ftssh_approve_host_key(handle, bytes(wrong), len(wrong)), -1005)
        self.assertEqual(DRIVER.ftssh_authentication_step(handle), -1005)
        self.assertEqual(DRIVER.ftssh_close(handle), 0)
        self.assertTrue(server.done.wait(2))
        self.assertFalse(any(e[0] == "authentication" for e in server.events))

    def test_server_rejection_never_requests_a_signature_or_a_shell(self):
        handle, server, key = self.start(reject=True)
        expected = -1006 if DRIVER.ftssh_supports_publickey_interactive() else -1011
        self.assertEqual(self.authenticate(handle, server, key, terminal=(expected,)), expected)
        self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), expected)
        self.assertEqual(DRIVER.ftssh_close(handle), 0)
        self.assertTrue(server.done.wait(2))
        self.assertEqual([e[1] for e in server.events if e[0] == "authentication"], [0])
        self.assertEqual([e[1] for e in server.events if e[0] == "authentication-methods"], [b"operator"])
        self.assertFalse(any(e[0] == "shell" for e in server.events))

    def test_disabled_publickey_and_none_success_cannot_offer_a_key_or_open_a_channel(self):
        for mode in ["publickeyDisabled", "noneSucceeds"]:
            with self.subTest(mode=mode):
                handle, server, key = self.start(auth_mode=mode)
                self.assertEqual(self.authenticate(handle, server, key, terminal=(-1010,)), -1010)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1010)
                self.assertEqual(DRIVER.ftssh_close(handle), 0)
                self.assertTrue(server.done.wait(2))
                self.assertEqual([e[1] for e in server.events if e[0] == "authentication-methods"], [b"operator"])
                self.assertFalse(any(e[0] in ("authentication", "channel", "pty", "shell") for e in server.events))

    def test_unsigned_publickey_success_is_not_key_authentication(self):
        handle, server, key = self.start(auth_mode="unsignedSuccess")
        self.assertEqual(self.authenticate(handle, server, key, terminal=(-1010,)), -1010)
        self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1010)
        self.assertEqual(DRIVER.ftssh_close(handle), 0)
        self.assertTrue(server.done.wait(2))
        self.assertEqual([e[1] for e in server.events if e[0] == "authentication"], [0])
        self.assertFalse(any(e[0] in ("channel", "pty", "shell") for e in server.events))

    def test_signed_rejection_and_partial_authentication_are_terminal_not_refusal(self):
        for mode in ["rejectSignature", "partial"]:
            with self.subTest(mode=mode):
                handle, server, key = self.start(auth_mode=mode)
                self.assertEqual(self.authenticate(handle, server, key, terminal=(-1011,)), -1011)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1011)
                self.assertEqual(DRIVER.ftssh_close(handle), 0)
                self.assertTrue(server.done.wait(2))
                self.assertEqual([e[1] for e in server.events if e[0] == "authentication"], [0, 1])
                self.assertFalse(any(e[0] in ("channel", "pty", "shell") for e in server.events))

    def test_lost_socket_before_or_after_signature_is_never_key_refusal(self):
        for mode, states in [("disconnectOffer", [0]), ("disconnectSignature", [0, 1])]:
            with self.subTest(mode=mode):
                handle, server, key = self.start(auth_mode=mode)
                self.assertEqual(self.authenticate(handle, server, key, terminal=(-1003,)), -1003)
                self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1003)
                self.assertEqual(DRIVER.ftssh_close(handle), 0)
                self.assertTrue(server.done.wait(2))
                self.assertEqual([e[1] for e in server.events if e[0] == "authentication"], states)
                self.assertFalse(any(e[0] in ("channel", "pty", "shell") for e in server.events))

    def test_ipv6_socket_and_idempotent_close(self):
        handle, server, key = self.start(family=socket.AF_INET6)
        self.assertEqual(self.authenticate(handle, server, key), 0)
        self.assertEqual(DRIVER.ftssh_close(handle), 0)
        self.assertEqual(DRIVER.ftssh_close(handle), 0)
        self.assertEqual(DRIVER.ftssh_handshake(handle), -1009)
        self.assertEqual(DRIVER.ftssh_descriptor(handle), -1)

    def test_invalid_descriptors_limits_and_phase_do_not_consume_resources(self):
        handle = P()
        self.assertEqual(DRIVER.ftssh_create(-1, 1024 * 1024, C.byref(handle)), -1000)
        a, b = socket.socketpair()
        try:
            for cap in [0, 65535, 4 * 1024 * 1024 + 1]:
                self.assertEqual(DRIVER.ftssh_create(a.fileno(), cap, C.byref(handle)), -1000)
                self.assertFalse(handle.value)
            self.assertEqual(DRIVER.ftssh_create(a.fileno(), 2 * 1024 * 1024, C.byref(handle)), 0)
            a.detach(); self.handles.append(handle)
            self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), -1001)
            self.assertEqual(DRIVER.ftssh_admit_write(handle, b"x", 1), -1001)
            self.assertEqual(DRIVER.ftssh_handshake(handle), 1)
            start = time.monotonic()
            self.assertEqual(DRIVER.ftssh_close(handle), 0)
            self.assertLess(time.monotonic() - start, 0.1)
        finally:
            a.close(); b.close()

    def test_cancellation_during_deferred_signature_closes_without_signing(self):
        handle, server, key = self.start()
        self.advance(handle, DRIVER.ftssh_handshake)
        self.assertEqual(DRIVER.ftssh_approve_host_key(handle, server.host_blob, len(server.host_blob)), 0)
        blob = public_blob(key)
        self.assertEqual(DRIVER.ftssh_begin_authentication(handle, b"operator", 8, blob, len(blob)), 0)
        self.advance(handle, DRIVER.ftssh_authentication_step, (2,))
        challenge = self.data(handle, DRIVER.ftssh_copy_challenge, 1024)
        signed = signature(key, challenge)
        self.assertEqual(DRIVER.ftssh_close(handle), 0)
        self.assertEqual(DRIVER.ftssh_supply_signature(handle, signed, len(signed)), -1009)
        self.assertTrue(server.done.wait(2))
        self.assertEqual([e[1] for e in server.events if e[0] == "authentication"], [0])

    def test_stable_write_slot_backpressure_preserves_half_megabyte_exactly(self):
        handle, server, key = self.start()
        self.assertEqual(self.authenticate(handle, server, key), 0)
        self.assertFalse(any(e[0] in ("channel", "shell") for e in server.events))
        self.assertEqual(DRIVER.ftssh_begin_shell(handle, 80, 24, 0, 0), 0)
        self.advance(handle, DRIVER.ftssh_shell_step)
        payload = bytes(range(256)) * 2000 + b"END-SESSION\r"
        admitted, received, eof = 0, bytearray(), False
        end = time.monotonic() + 10
        while time.monotonic() < end:
            if admitted < len(payload):
                chunk = payload[admitted:admitted+16384]
                rc = DRIVER.ftssh_admit_write(handle, chunk, len(chunk))
                self.assertIn(rc, (0, 1))
                if rc == 0:
                    admitted += len(chunk)
                    self.assertEqual(DRIVER.ftssh_admit_write(handle, b"NOT-ADMITTED", 12), 1)
            self.assertIn(DRIVER.ftssh_flush_write(handle), (0, 1))
            out, count = C.create_string_buffer(16384), Z()
            rc = DRIVER.ftssh_read(handle, out, len(out), C.byref(count))
            self.assertIn(rc, (0, 1, 3))
            received += out.raw[:count.value]
            stats = Stats(); DRIVER.ftssh_get_stats(handle, C.byref(stats))
            self.assertLessEqual(stats.pending, 16384)
            self.assertLessEqual(stats.peak, 2 * 1024 * 1024)
            if rc == 3:
                eof = True
                break
            self.wait(handle)
        self.assertTrue(eof)
        self.assertEqual(admitted, len(payload))
        self.assertEqual(received, b"ready\r\nstderr\r\n" + payload)
        self.assertEqual(server.received, payload)

    def test_small_allocator_budget_fails_without_losing_socket_ownership(self):
        a, b = socket.socketpair()
        try:
            handle = P()
            rc = DRIVER.ftssh_create(a.fileno(), 65536, C.byref(handle))
            if rc == 0:
                a.detach(); self.handles.append(handle)
                stats = Stats(); DRIVER.ftssh_get_stats(handle, C.byref(stats))
                self.assertLessEqual(stats.peak, 65536)
            else:
                self.assertEqual(rc, -1002)
                self.assertFalse(handle.value)
                self.assertEqual(a.send(b"still-owned"), 11)
                self.assertEqual(b.recv(11), b"still-owned")
        finally:
            a.close(); b.close()

    def test_global_connection_limit_and_released_capacity(self):
        peers = []
        try:
            for _ in range(32):
                a, b = socket.socketpair(); peers.append(b)
                handle = P()
                self.assertEqual(DRIVER.ftssh_create(a.fileno(), 2 * 1024 * 1024, C.byref(handle)), 0)
                a.detach(); self.handles.append(handle)
            a, b = socket.socketpair(); peers.extend([a, b])
            refused = P()
            self.assertEqual(DRIVER.ftssh_create(a.fileno(), 2 * 1024 * 1024, C.byref(refused)), -1002)
            self.assertFalse(refused.value)
            self.assertEqual(DRIVER.ftssh_destroy(C.byref(self.handles[-1])), 0)
            self.assertEqual(DRIVER.ftssh_create(a.fileno(), 2 * 1024 * 1024, C.byref(refused)), 0)
            a.detach(); self.handles.append(refused)
        finally:
            for peer in peers:
                peer.close()

if __name__ == "__main__":
    unittest.main(verbosity=2)
