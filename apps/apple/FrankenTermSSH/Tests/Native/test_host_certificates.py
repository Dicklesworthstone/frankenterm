"""Real host-certificate KEX against the system OpenSSH server.

Run: python Tests/Native/test_host_certificates.py /path/to/libftssh.so
Requires ssh-keygen, sshd and its platform privilege-separation directory. Uses
only synthetic keys and loopback sockets; every server stops before userauth.
Retains the small temporary fixture directory and logs for failed-run diagnosis.
CA/principal/time/revocation policy is separately tested in FrankenTermKit and
the workspace authentication integration: C proves only subject-key possession.
"""
import base64
import ctypes as C
from pathlib import Path
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest


DRIVER = C.CDLL(sys.argv.pop(1))
P, I, Z = C.c_void_p, C.c_int, C.c_size_t
for name, result, arguments in [
    ("ftssh_supports_host_certificates", I, []),
    ("ftssh_set_host_key_policy", I, [P, I]),
    ("ftssh_create", I, [I, Z, C.POINTER(P)]),
    ("ftssh_handshake", I, [P]),
    ("ftssh_copy_host_key", I, [P, P, Z, C.POINTER(Z)]),
    ("ftssh_copy_method", I, [P, I, P, Z]),
    ("ftssh_begin_password_authentication", I, [P, P, Z]),
    ("ftssh_approve_host_key", I, [P, P, Z]),
    ("ftssh_directions", I, [P]),
    ("ftssh_descriptor", I, [P]),
    ("ftssh_destroy", I, [C.POINTER(P)]),
]:
    function = getattr(DRIVER, name)
    function.restype, function.argtypes = result, arguments


class HostCertificateExchange(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.sshd = shutil.which("sshd") or "/usr/sbin/sshd"
        cls.keygen = shutil.which("ssh-keygen")
        if not Path(cls.sshd).is_file() or not cls.keygen:
            raise RuntimeError("OpenSSH server and ssh-keygen are required")
        if DRIVER.ftssh_supports_host_certificates() != 1:
            raise RuntimeError("This suite requires the versioned host-certificate backend")
        cls.directory = Path(tempfile.mkdtemp(prefix="frankenterm-host-certificate-test-"))
        cls.counter = 0
        cls.generate("ca", "ed25519")
        for name, kind, bits in [("ed25519", "ed25519", None), ("p256", "ecdsa", 256),
                                  ("p384", "ecdsa", 384), ("p521", "ecdsa", 521),
                                  ("rsa", "rsa", 2048)]:
            cls.generate(name, kind, bits)
            subprocess.run([cls.keygen, "-q", "-s", str(cls.directory / "ca"), "-I", name,
                            "-h", "-n", "localhost", "-V", "-1m:+10m",
                            str(cls.directory / (name + ".pub"))], check=True, capture_output=True)

    @classmethod
    def generate(cls, name, kind, bits=None):
        arguments = [cls.keygen, "-q", "-t", kind, "-N", "", "-f", str(cls.directory / name)]
        if bits is not None:
            arguments += ["-b", str(bits)]
        subprocess.run(arguments, check=True, capture_output=True)

    def exchange(self, name, algorithms, policy, expect_certificate=True, expect_success=True):
        type(self).counter += 1
        stem = self.directory / ("exchange-" + str(self.counter))
        certificate = self.directory / (name + "-cert.pub")
        config = stem.with_suffix(".conf")
        config.write_text("\n".join([
            "HostKey " + str(self.directory / name), "HostCertificate " + str(certificate),
            "HostKeyAlgorithms " + algorithms,
            "PasswordAuthentication no", "KbdInteractiveAuthentication no", "PubkeyAuthentication no",
            "PermitRootLogin no", "UsePAM no", "LogLevel VERBOSE", "LoginGraceTime 10",
        ]) + "\n", encoding="utf-8")
        # Inetd mode owns just this already-connected loopback socket. It never
        # starts a listening daemon, changes accounts, or creates a shell.
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0)); listener.listen(1)
        client = socket.create_connection(listener.getsockname(), timeout=5)
        server, _ = listener.accept(); listener.close()
        handle = P()
        with stem.with_suffix(".log").open("wb") as log:
            process = subprocess.Popen([self.sshd, "-i", "-e", "-f", str(config)],
                                       stdin=server, stdout=server, stderr=log, close_fds=True)
            server.close()
            try:
                self.assertEqual(DRIVER.ftssh_create(client.fileno(), 4 * 1024 * 1024, C.byref(handle)), 0)
                client.detach()
                if policy is not None:
                    self.assertEqual(DRIVER.ftssh_set_host_key_policy(handle, policy), 0)
                self.assertEqual(DRIVER.ftssh_set_host_key_policy(handle, 99), -1000)
                deadline = time.monotonic() + 10
                result = DRIVER.ftssh_handshake(handle)
                # Even EAGAIN begins negotiation; policy cannot change midway.
                self.assertNotEqual(DRIVER.ftssh_set_host_key_policy(handle, 0), 0)
                while result == 1:
                    self.assertLess(time.monotonic(), deadline, "KEX timed out; see retained server log")
                    descriptor = DRIVER.ftssh_descriptor(handle)
                    directions = DRIVER.ftssh_directions(handle)
                    select.select([descriptor] if directions & 1 else [],
                                  [descriptor] if directions & 2 else [], [], 0.02)
                    result = DRIVER.ftssh_handshake(handle)
                if not expect_success:
                    self.assertEqual(result, -1004)
                    return
                self.assertEqual(result, 0, "KEX failed; see retained server log")
                method = C.create_string_buffer(128)
                self.assertEqual(DRIVER.ftssh_copy_method(handle, 1, method, len(method)), 0)
                self.assertIn(method.value.decode("ascii"), algorithms.split(","))
                self.assertEqual(b"-cert-v01@openssh.com" in method.value, expect_certificate)
                buffer, length = C.create_string_buffer(65536), Z()
                self.assertEqual(DRIVER.ftssh_copy_host_key(handle, buffer, len(buffer), C.byref(length)), 0)
                expected = base64.b64decode((certificate if expect_certificate else
                    self.directory / (name + ".pub")).read_text().split()[1], validate=True)
                actual = buffer.raw[:length.value]
                # The full cert, including nonce, principal, issuer and issuer
                # signature, survives KEX unchanged. A raw-subject exchange hash
                # would not verify against OpenSSH's independent KEX signature.
                self.assertEqual(actual, expected)
                self.assertEqual(DRIVER.ftssh_begin_password_authentication(handle, b"nobody", 6), -1001)
                self.assertEqual(DRIVER.ftssh_approve_host_key(handle, actual, len(actual)), 0)
            finally:
                client.close()
                if handle:
                    self.assertEqual(DRIVER.ftssh_destroy(C.byref(handle)), 0)
                if process.poll() is None:
                    process.terminate()
                process.wait(timeout=5)

    def test_ed25519_subject(self):
        self.exchange("ed25519", "ssh-ed25519-cert-v01@openssh.com", 1)

    def test_ecdsa_subjects(self):
        for name, curve in [("p256", "nistp256"), ("p384", "nistp384"), ("p521", "nistp521")]:
            with self.subTest(curve=curve):
                self.exchange(name, "ecdsa-sha2-" + curve + "-cert-v01@openssh.com", 1)

    def test_rsa_subject_sha2_signatures(self):
        for digest in [256, 512]:
            with self.subTest(digest=digest):
                self.exchange("rsa", "rsa-sha2-" + str(digest) + "-cert-v01@openssh.com", 1)

    def test_mixed_policy_prefers_certificate(self):
        self.exchange("ed25519", "ssh-ed25519,ssh-ed25519-cert-v01@openssh.com", 2)

    def test_raw_default_preserves_existing_pin_routes(self):
        self.exchange("ed25519", "ssh-ed25519,ssh-ed25519-cert-v01@openssh.com", None,
                      expect_certificate=False)

    def test_ca_only_cannot_negotiate_a_raw_fallback(self):
        self.exchange("ed25519", "ssh-ed25519", 1, expect_success=False)

    def test_deprecated_rsa_sha1_certificate_is_not_negotiated(self):
        self.exchange("rsa", "ssh-rsa-cert-v01@openssh.com", 1, expect_success=False)


if __name__ == "__main__":
    unittest.main()
