"""Enforce imported host-signature restrictions against independent SSH peers.

Run with the actual adapter and built XCTest product:
  python Tests/Interop/test_host_algorithms.py libftssh.so NativeTests.xctest
For the C boundary alone, replace the XCTest product with --native-only.

Only generated fixture keys, loopback sockets and an isolated inetd sshd are
used. The existing account's authorized keys and SSH configuration are never
changed. sshd cannot create a session channel, shell, PTY or forwarding channel.
Temporary public fixtures and server logs are retained for diagnosis.
"""
import base64
import ctypes as C
import importlib.util
import json
import os
from pathlib import Path
import pwd
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ed25519, rsa


NATIVE_LIBRARY, TEST_BUNDLE = sys.argv[1:3]
NATIVE_ONLY = TEST_BUNDLE == "--native-only"
NATIVE_FIXTURE = Path(__file__).resolve().parents[1] / "Native" / "test_native.py"
sys.argv = [sys.argv[0], NATIVE_LIBRARY]
spec = importlib.util.spec_from_file_location("native_host_algorithm_fixture", NATIVE_FIXTURE)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
DRIVER, P, I, Z = fixture.DRIVER, C.c_void_p, C.c_int, C.c_size_t
for name, result, arguments in [
    ("ftssh_supports_host_algorithm_policy", I, []),
    ("ftssh_set_host_key_algorithms", I, [P, P, Z]),
    ("ftssh_set_host_key_policy", I, [P, I]),
]:
    function = getattr(DRIVER, name)
    function.restype, function.argtypes = result, arguments

AUTH_EVENTS = {"authentication-methods", "authentication", "password-authentication",
               "interactive-request", "interactive-response"}
LIVE_TEST = "FrankenTermSSHTests.WorkspaceHostAlgorithmLiveTests/testLiveMandatoryAlgorithmsBeforeCredentials"


class OpenSSHPeer:
    """One real OpenSSH connection; no listening SSH daemon survives the test."""
    def __init__(self, directory, name, key, certificate, algorithms, authorized_keys=None):
        self.directory, self.log_path = directory, directory / (name + ".log")
        self.config = directory / (name + ".conf")
        lines = ["HostKey " + str(key), "HostKeyAlgorithms " + algorithms,
                 "UsePAM no", "PasswordAuthentication no", "KbdInteractiveAuthentication no",
                 "PubkeyAuthentication " + ("yes" if authorized_keys else "no"),
                 "PermitRootLogin prohibit-password", "StrictModes no", "LogLevel DEBUG3",
                 "LoginGraceTime 15", "MaxSessions 0", "PermitTTY no", "AllowTcpForwarding no",
                 "AllowAgentForwarding no", "X11Forwarding no", "PermitUserEnvironment no"]
        if certificate is not None:
            lines.append("HostCertificate " + str(certificate))
        if authorized_keys is not None:
            lines += ["AuthorizedKeysFile " + str(authorized_keys), "AuthenticationMethods publickey"]
        self.config.write_text("\n".join(lines) + "\n", encoding="utf-8")
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0)); self.listener.listen(1)
        self.listener.settimeout(0.1)
        self.address = self.listener.getsockname()
        self.stop, self.done = threading.Event(), threading.Event()
        self.accepted_connections, self.process, self.errors = 0, None, []
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        try:
            deadline = time.monotonic() + 15
            while not self.stop.is_set():
                try:
                    peer, _ = self.listener.accept()
                    break
                except socket.timeout:
                    if time.monotonic() >= deadline:
                        raise TimeoutError("The isolated OpenSSH peer was never contacted")
            else:
                return
            self.accepted_connections += 1
            with peer, self.log_path.open("wb") as log:
                self.process = subprocess.Popen([shutil.which("sshd") or "/usr/sbin/sshd",
                    "-i", "-e", "-f", str(self.config)], stdin=peer, stdout=peer,
                    stderr=log, close_fds=True)
                self.process.wait(timeout=20)
        except Exception as error:
            if not self.stop.is_set():
                self.errors.append(repr(error))
        finally:
            self.done.set()

    def close(self):
        self.stop.set()
        self.listener.close()
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
        self.thread.join(5)
        if self.thread.is_alive():
            raise AssertionError("The isolated OpenSSH peer did not stop")
        if self.process is not None:
            self.process.wait(timeout=5)
        if self.errors:
            raise AssertionError(self.errors)


class AlgorithmFixtures:
    def __init__(self):
        self.directory = Path(tempfile.mkdtemp(prefix="frankenterm-host-algorithms-"))
        self.keygen = shutil.which("ssh-keygen")
        if not self.keygen or not Path(shutil.which("sshd") or "/usr/sbin/sshd").is_file():
            raise RuntimeError("These interoperability tests require OpenSSH sshd and ssh-keygen")
        self.client = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
        self.rsa = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        self.rsa_path = self.directory / "rsa-host"
        self.rsa_path.write_bytes(self.rsa.private_bytes(serialization.Encoding.PEM,
            serialization.PrivateFormat.OpenSSH, serialization.NoEncryption()))
        self.rsa_path.chmod(0o600)
        self.rsa_path.with_suffix(".pub").write_bytes(self.rsa.public_key().public_bytes(
            serialization.Encoding.OpenSSH, serialization.PublicFormat.OpenSSH) + b"\n")
        self.authorized_keys = self.directory / "authorized_keys"
        self.authorized_keys.write_bytes(self.client.public_key().public_bytes(
            serialization.Encoding.OpenSSH, serialization.PublicFormat.OpenSSH) + b"\n")
        self.authorized_keys.chmod(0o600)
        self.ca = self.directory / "rsa-ca"
        subprocess.run([self.keygen, "-q", "-t", "rsa", "-b", "2048", "-N", "", "-f", str(self.ca)],
                       check=True, capture_output=True)
        self.certificates = {}
        for bits in [256, 512]:
            subject = self.directory / ("ed-host-" + str(bits))
            subprocess.run([self.keygen, "-q", "-t", "ed25519", "-N", "", "-f", str(subject)],
                           check=True, capture_output=True)
            subprocess.run([self.keygen, "-q", "-s", str(self.ca), "-I", "fixture-" + str(bits),
                "-h", "-n", "127.0.0.1", "-V", "20200101000000:20500101000000",
                "-t", "rsa-sha2-" + str(bits), str(subject) + ".pub"], check=True, capture_output=True)
            self.certificates[bits] = (subject, Path(str(subject) + "-cert.pub"))
        subprocess.run([self.keygen, "-q", "-s", str(self.ca), "-I", "rsa-fixture", "-h", "-n", "127.0.0.1",
            "-V", "20200101000000:20500101000000", str(self.rsa_path) + ".pub"], check=True, capture_output=True)

    @staticmethod
    def blob(path):
        return base64.b64decode(path.read_text().split()[1], validate=True)


MATERIAL = None


def material():
    global MATERIAL
    if MATERIAL is None:
        MATERIAL = AlgorithmFixtures()
    return MATERIAL


class NativeHostAlgorithmPolicyTests(unittest.TestCase):
    def exchange(self, preferred, offered, *, bad_lists=(), expected=None, certificate=False, success=True):
        self.assertEqual(DRIVER.ftssh_supports_host_algorithm_policy(), 1)
        keys = material()
        name = "native-" + str(time.monotonic_ns())
        peer = OpenSSHPeer(keys.directory, name, keys.rsa_path,
            Path(str(keys.rsa_path) + "-cert.pub") if certificate else None, offered)
        client = socket.create_connection(peer.address, timeout=5)
        handle = P()
        try:
            self.assertEqual(DRIVER.ftssh_create(client.fileno(), 4 * 1024 * 1024, C.byref(handle)), 0)
            client.detach()
            encoded = preferred.encode("ascii")
            self.assertEqual(DRIVER.ftssh_set_host_key_algorithms(handle, encoded, len(encoded)), 0)
            for invalid in bad_lists:
                with self.subTest(invalid=repr(invalid)):
                    self.assertNotEqual(DRIVER.ftssh_set_host_key_algorithms(handle, invalid, len(invalid)), 0)
            deadline = time.monotonic() + 10
            result = DRIVER.ftssh_handshake(handle)
            # An EAGAIN handshake still freezes both policy entry points.
            self.assertNotEqual(DRIVER.ftssh_set_host_key_algorithms(handle, b"rsa-sha2-512", 12), 0)
            self.assertNotEqual(DRIVER.ftssh_set_host_key_policy(handle, 0), 0)
            while result == 1:
                self.assertLess(time.monotonic(), deadline, "OpenSSH KEX did not complete")
                fd, directions = DRIVER.ftssh_descriptor(handle), DRIVER.ftssh_directions(handle)
                select.select([fd] if directions & 1 else [], [fd] if directions & 2 else [], [], 0.02)
                result = DRIVER.ftssh_handshake(handle)
            self.assertEqual(result, 0 if success else -1004)
            if success:
                method = C.create_string_buffer(128)
                self.assertEqual(DRIVER.ftssh_copy_method(handle, 1, method, len(method)), 0)
                self.assertEqual(method.value.decode("ascii"), expected)
                buffer, length = C.create_string_buffer(65536), Z()
                self.assertEqual(DRIVER.ftssh_copy_host_key(handle, buffer, len(buffer), C.byref(length)), 0)
                cert = expected.endswith("-cert-v01@openssh.com")
                public = Path(str(keys.rsa_path) + ("-cert.pub" if cert else ".pub"))
                self.assertEqual(buffer.raw[:length.value], keys.blob(public))
        finally:
            client.close()
            if handle:
                self.assertEqual(DRIVER.ftssh_destroy(C.byref(handle)), 0)
            peer.close()
        self.assertNotIn("userauth-request", peer.log_path.read_text(errors="replace"))

    def test_rsa_signature_order_is_distinct_from_raw_blob_type(self):
        for first, second in [(256, 512), (512, 256)]:
            with self.subTest(preferred=first):
                self.exchange(f"rsa-sha2-{first},rsa-sha2-{second}", "rsa-sha2-512,rsa-sha2-256",
                              expected=f"rsa-sha2-{first}")

    def test_invalid_lists_are_atomic_and_never_silently_widened(self):
        invalid = [b"", b"rsa-sha2-256,", b",rsa-sha2-256", b"rsa-sha2-256,,rsa-sha2-512",
            b"rsa-sha2-256,rsa-sha2-256", b"rsa-sha2-256,unknown", b"rsa-sha2-256,ssh-rsa",
            b"rsa-sha2-*", b"+rsa-sha2-256", b"^rsa-sha2-256", b"-rsa-sha2-256",
            b"rsa-sha2-256\0,rsa-sha2-512", b"rsa-sha2-256, rsa-sha2-512", b"x" * 4096]
        self.exchange("rsa-sha2-256", "rsa-sha2-512,rsa-sha2-256", bad_lists=invalid,
                      expected="rsa-sha2-256")

    def test_explicit_mixed_order_preserves_raw_or_certificate_preference(self):
        raw, certificate = "rsa-sha2-256", "rsa-sha2-256-cert-v01@openssh.com"
        for names in [(raw, certificate), (certificate, raw)]:
            with self.subTest(preferred=names[0]):
                self.exchange(",".join(names), certificate + "," + raw, certificate=True, expected=names[0])

    def test_rsa_allowlist_cannot_fall_back_to_another_digest(self):
        self.exchange("rsa-sha2-512", "rsa-sha2-256", success=False)


class WorkspaceHostAlgorithmInteropTests(unittest.TestCase):
    def invoke(self, mode, port, host_blob, *, hop=None, certificate=None):
        environment = dict(os.environ, FT_HOST_ALGORITHM_MODE=mode, FT_HOST_ALGORITHM_PORT=str(port),
            FT_HOST_ALGORITHM_HOSTKEY=base64.b64encode(host_blob).decode(),
            FT_HOST_ALGORITHM_HOP=json.dumps(hop), PYTHONDONTWRITEBYTECODE="1")
        if certificate is not None:
            environment["FT_HOST_ALGORITHM_CERTIFICATE"] = base64.b64encode(certificate).decode()
            environment["FT_HOST_ALGORITHM_USER"] = pwd.getpwuid(os.geteuid()).pw_name
        command = (["xcrun", "xctest", "-XCTest", LIVE_TEST, TEST_BUNDLE] if os.path.isdir(TEST_BUNDLE)
                   else [TEST_BUNDLE, LIVE_TEST])
        result = subprocess.run(command, env=environment, capture_output=True, text=True, timeout=25)
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, 0, output[-20000:])
        self.assertIn("Executed 1 test", output)
        self.assertNotIn("skipped", output.lower())

    def assert_no_authentication(self, peer):
        self.assertEqual([event for event in peer.events if event[0] in AUTH_EVENTS], [])
        self.assertEqual([event for event in peer.events if event[0] in {"channel", "pty", "shell", "forward"}], [])

    def assert_signed_login(self, peer):
        offers = [event for event in peer.events if event[0] == "authentication"]
        self.assertEqual([event[1] for event in offers], [0, 1])
        self.assertTrue(all(event[2] == b"operator" and event[3] == fixture.public_blob(material().client)
                            for event in offers))
        self.assertEqual([event for event in peer.events if event[0] in {"channel", "pty", "shell"}], [])

    def test_raw_and_jump_host_policies_precede_every_credential_method(self):
        keys = material()
        modes = ["raw-256", "raw-512", "deny-256", "deny-512", "deny-password", "deny-interactive",
                 "jump-allowed", "jump-denied", "probe-allowed", "probe-denied"]
        for mode in modes:
            with self.subTest(mode=mode):
                server_signature = "rsa-sha2-256" if mode in {
                    "raw-256", "deny-512", "deny-password", "deny-interactive", "probe-denied"} else "rsa-sha2-512"
                destination = fixture.LoopbackServer(keys.client, host_key=keys.rsa, host_signature=server_signature)
                hop = None
                try:
                    descriptor = None
                    if mode.startswith("jump-"):
                        hop = fixture.LoopbackServer(keys.client, host_key=keys.rsa, host_signature="rsa-sha2-256",
                            forward_to=destination.address, forward_host="127.0.0.1")
                        descriptor = dict(port=hop.address[1], hostKey=base64.b64encode(hop.host_blob).decode())
                    self.invoke(mode, destination.address[1], destination.host_blob, hop=descriptor)
                    if mode == "jump-denied":
                        self.assertEqual(destination.accepted_connections, 0)
                        self.assertEqual(destination.events, [])
                    else:
                        self.assertTrue(destination.done.wait(2))
                        self.assertEqual(destination.accepted_connections, 1)
                        self.assertEqual(destination.errors, [])
                        if mode.startswith(("deny-", "probe-")):
                            self.assert_no_authentication(destination)
                        else:
                            self.assert_signed_login(destination)
                    if hop is not None:
                        self.assertTrue(hop.done.wait(2))
                        self.assertEqual(hop.accepted_connections, 1)
                        self.assertEqual(hop.errors, [])
                        if mode == "jump-denied":
                            self.assert_no_authentication(hop)
                        else:
                            self.assert_signed_login(hop)
                            self.assertEqual([e for e in hop.events if e[0] == "forward"],
                                [("forward", b"127.0.0.1", destination.address[1])])
                    print("host algorithm scenario passed: " + mode, flush=True)
                finally:
                    if hop is not None:
                        hop.close()
                    destination.close()

    def test_rsa_ca_signature_digest_is_independent_from_host_subject_and_ca_key_type(self):
        keys = material()
        for bits in [512, 256]:
            mode = "ca-" + str(bits)
            subject, certificate = keys.certificates[bits]
            peer = OpenSSHPeer(keys.directory, mode, subject, certificate,
                "ssh-ed25519-cert-v01@openssh.com", keys.authorized_keys)
            try:
                self.invoke(mode, peer.address[1], keys.blob(Path(str(subject) + ".pub")),
                            certificate=keys.blob(certificate))
                self.assertTrue(peer.done.wait(3))
            finally:
                peer.close()
            log = peer.log_path.read_text(errors="replace")
            self.assertEqual(peer.accepted_connections, 1)
            if bits == 512:
                self.assertIn("Accepted publickey", log)
            else:
                self.assertNotIn("userauth-request", log,
                    "A prohibited CA signature reached a username or authentication-method probe")
                self.assertNotIn("Accepted publickey", log)
            self.assertNotIn("server_input_channel_open:", log)
            print("host algorithm scenario passed: " + mode, flush=True)


if __name__ == "__main__":
    suite = unittest.TestSuite()
    suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(NativeHostAlgorithmPolicyTests))
    if not NATIVE_ONLY:
        suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(WorkspaceHostAlgorithmInteropTests))
    outcome = unittest.TextTestRunner(verbosity=2).run(suite)
    sys.exit(0 if outcome.wasSuccessful() else 1)
