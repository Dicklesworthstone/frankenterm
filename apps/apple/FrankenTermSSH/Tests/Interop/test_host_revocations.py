"""Exercise real workspace revocation before credential activity.

  python Tests/Interop/test_host_revocations.py libftssh.so NativeTests.xctest

Reuses the independent libssh and isolated OpenSSH peers from the algorithm
suite. All generated keys, KRLs, configs and logs remain in owned temporary
directories. No peer can open a shell, PTY or session channel.
"""
import base64
import importlib.util
import json
import os
from pathlib import Path
import pwd
import subprocess
import sys
import time
import unittest

LIBRARY, TEST_BUNDLE = sys.argv[1:3]
sys.argv = [sys.argv[0], LIBRARY, "--native-only"]
spec = importlib.util.spec_from_file_location("host_revocation_peer_helpers",
    Path(__file__).with_name("test_host_algorithms.py"))
peers = importlib.util.module_from_spec(spec)
spec.loader.exec_module(peers)
sys.argv = [sys.argv[0]]
LIVE_TEST = "FrankenTermSSHTests.WorkspaceHostRevocationLiveTests/testLiveRevocationPrecedesEveryCredentialAndSurvivesReimport"
MATERIAL = None


def material():
    global MATERIAL
    if MATERIAL is None:
        MATERIAL = peers.AlgorithmFixtures()
    return MATERIAL


def source(directive, public, *, authority=None):
    keys = material()
    name = "revocation-" + str(time.monotonic_ns())
    specification, output = keys.directory / (name + ".spec"), keys.directory / (name + ".krl")
    specification.write_bytes(directive.encode() + b": " + public)
    command = [keys.keygen, "-q", "-k", "-f", str(output), "-z", "1"]
    if authority is not None:
        command += ["-s", str(authority)]
    subprocess.run(command + [str(specification)], capture_output=True, check=True, timeout=10)
    return output.read_bytes()


class WorkspaceHostRevocationInteropTests(unittest.TestCase):
    def invoke(self, mode, port, host_blob, revocations, *, hop=None, certificate=None):
        environment = dict(os.environ, FT_HOST_REVOCATION_MODE=mode, FT_HOST_REVOCATION_PORT=str(port),
            FT_HOST_REVOCATION_HOSTKEY=base64.b64encode(host_blob).decode(),
            FT_HOST_REVOCATION_SOURCE=base64.b64encode(revocations).decode(),
            FT_HOST_REVOCATION_HOP=json.dumps(hop), PYTHONDONTWRITEBYTECODE="1")
        if certificate is not None:
            environment["FT_HOST_REVOCATION_CERTIFICATE"] = base64.b64encode(certificate).decode()
            environment["FT_HOST_REVOCATION_USER"] = pwd.getpwuid(os.geteuid()).pw_name
        command = (["xcrun", "xctest", "-XCTest", LIVE_TEST, TEST_BUNDLE] if os.path.isdir(TEST_BUNDLE)
                   else [TEST_BUNDLE, LIVE_TEST])
        result = subprocess.run(command, env=environment, capture_output=True, text=True, timeout=25)
        self.assertEqual(result.returncode, 0, (result.stdout + result.stderr)[-20000:])
        self.assertIn("Executed 1 test", result.stdout + result.stderr)
        self.assertNotIn("skipped", (result.stdout + result.stderr).lower())

    def assert_no_authentication(self, peer):
        self.assertEqual([event for event in peer.events if event[0] in peers.AUTH_EVENTS], [])
        self.assertEqual([event for event in peer.events if event[0] in {"channel", "pty", "shell", "forward"}], [])

    def assert_signed_login(self, peer):
        offers = [event for event in peer.events if event[0] == "authentication"]
        self.assertEqual([event[1] for event in offers], [0, 1])
        self.assertTrue(all(event[2] == b"operator" and event[3] == peers.fixture.public_blob(material().client)
                            for event in offers))
        self.assertEqual([event for event in peer.events if event[0] in {"channel", "pty", "shell"}], [])

    def test_raw_revocation_checks_all_credentials_and_hop_scopes_before_userauth(self):
        keys = material()
        revoked = source("sha1", keys.rsa_path.with_suffix(".pub").read_bytes())
        unrelated = source("sha1", keys.ca.with_suffix(".pub").read_bytes())
        for mode in ["raw-allowed", "raw-denied", "password-denied", "interactive-denied",
                     "jump-denied", "destination-denied", "probe-history"]:
            with self.subTest(mode=mode):
                destination = peers.fixture.LoopbackServer(keys.client, host_key=keys.rsa, host_signature="rsa-sha2-512")
                hop = None
                try:
                    descriptor = None
                    if mode in {"jump-denied", "destination-denied"}:
                        hop = peers.fixture.LoopbackServer(keys.client, host_key=keys.rsa, host_signature="rsa-sha2-512",
                            forward_to=destination.address, forward_host="127.0.0.1")
                        descriptor = dict(port=hop.address[1], hostKey=base64.b64encode(hop.host_blob).decode())
                    self.invoke(mode, destination.address[1], destination.host_blob,
                                unrelated if mode == "raw-allowed" else revoked, hop=descriptor)
                    if mode == "jump-denied":
                        self.assertEqual(destination.accepted_connections, 0)
                        self.assertEqual(destination.events, [])
                    else:
                        self.assertTrue(destination.done.wait(2))
                        self.assertEqual(destination.accepted_connections, 1)
                        self.assertEqual(destination.errors, [])
                        if mode == "raw-allowed":
                            self.assert_signed_login(destination)
                        else:
                            self.assert_no_authentication(destination)
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
                    print("host revocation scenario passed: " + mode, flush=True)
                finally:
                    if hop is not None:
                        hop.close()
                    destination.close()

    def test_openssh_certificate_serial_subject_and_issuer_revocation_precede_username(self):
        keys = material()
        subject, _ = keys.certificates[512]
        public = keys.directory / "revocation-certified.pub"
        public.write_bytes(Path(str(subject) + ".pub").read_bytes())
        subprocess.run([keys.keygen, "-q", "-s", str(keys.ca), "-I", "revocation-live", "-h",
            "-n", "127.0.0.1", "-z", "700", "-V", "20200101000000:20500101000000", str(public)],
            capture_output=True, check=True, timeout=10)
        certificate = public.with_name("revocation-certified-cert.pub")
        sources = {
            "certificate-allowed": source("sha1", keys.rsa_path.with_suffix(".pub").read_bytes()),
            "certificate-serial-denied": source("serial", b"700\n", authority=keys.ca.with_suffix(".pub")),
            "certificate-issuer-denied": source("sha1", keys.ca.with_suffix(".pub").read_bytes()),
            "certificate-subject-denied": public.read_bytes(),
        }
        for mode, revocations in sources.items():
            with self.subTest(mode=mode):
                peer = peers.OpenSSHPeer(keys.directory, mode, subject, certificate,
                    "ssh-ed25519-cert-v01@openssh.com", keys.authorized_keys)
                try:
                    self.invoke(mode, peer.address[1], keys.blob(public), revocations,
                                certificate=keys.blob(certificate))
                    self.assertTrue(peer.done.wait(3))
                finally:
                    peer.close()
                self.assertEqual(peer.accepted_connections, 1)
                log = peer.log_path.read_text(errors="replace")
                if mode == "certificate-allowed":
                    self.assertIn("Accepted publickey", log)
                else:
                    self.assertNotIn("userauth-request", log, "A revoked host reached username or method negotiation")
                    self.assertNotIn("Accepted publickey", log)
                self.assertNotIn("server_input_channel_open:", log)
                print("host revocation scenario passed: " + mode, flush=True)


if __name__ == "__main__":
    unittest.main(verbosity=2)
