"""Imported workspace -> real production signer -> libssh2 -> independent libssh.

Run with a built native driver and XCTest bundle, without rebuilding the package:
python Tests/Interop/test_workspace.py /path/to/libftssh.so /path/to/FrankenTermSSHPackageTests.xctest
Append /path/to/WorkspaceSSHProbe to also exercise all four key families,
protected-worker cancellation and durable managed-session creation/retirement.
Synthetic test keys only. No user Keychain, external destination or actual shell.
"""
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ed25519, ec

NATIVE_LIBRARY, TEST_BUNDLE = sys.argv[1:3]
PROBE = sys.argv[3] if len(sys.argv) > 3 else None
sys.argv = [sys.argv[0], NATIVE_LIBRARY]
spec = importlib.util.spec_from_file_location("native_fixture", Path(__file__).resolve().parents[1] / "Native" / "test_native.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


LIVE_TEST = "FrankenTermSSHTests.WorkspaceAuthenticationTests/testLiveImportedWorkspaceAuthenticationAndPTY"


def xctest_command():
    """SwiftPM emits an executable test product on Linux and a loadable .xctest
    bundle directory on Apple platforms, which runs through `xcrun xctest`."""
    if os.path.isdir(TEST_BUNDLE):
        return ["xcrun", "xctest", "-XCTest", LIVE_TEST, TEST_BUNDLE]
    return [TEST_BUNDLE, LIVE_TEST]


class WorkspaceInteropTests(unittest.TestCase):
    def test_imported_workspace_through_real_transport(self):
        for mode in ["success", "wrong-host", "revoked", "revoke-before-shell",
                     "owner-success", "owner-revoked", "owner-marker-failure", "owner-close-before-run"]:
            with self.subTest(mode=mode):
                key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
                server = fixture.LoopbackServer(key)
                try:
                    environment = dict(os.environ, FT_WORKSPACE_SSH_TEST_PORT=str(server.address[1]),
                                       FT_WORKSPACE_SSH_TEST_HOSTKEY=base64.b64encode(server.host_blob).decode(),
                                       FT_WORKSPACE_SSH_TEST_MODE=mode)
                    result = subprocess.run(xctest_command(), env=environment, capture_output=True, text=True, timeout=20)
                    # macOS xctest reports its summary on stderr; Linux on stdout.
                    output = result.stdout + result.stderr
                    self.assertEqual(result.returncode, 0, result.stdout[-12000:] + result.stderr[-12000:])
                    self.assertIn("Executed 1 test", output)
                    self.assertNotIn("skipped", output.lower())
                    authentication = [event[1] for event in server.events if event[0] == "authentication"]
                    self.assertEqual(authentication, [] if mode in ["wrong-host", "revoked"] else [0, 1])
                    self.assertEqual(any(event[0] == "shell" for event in server.events),
                                     mode in ["success", "owner-success", "owner-revoked", "owner-close-before-run"])
                    if mode == "success":
                        self.assertEqual(server.received, "Imported key → real SSH.\rEND-SESSION\r".encode())
                    elif mode == "owner-success":
                        self.assertEqual(server.received, "Imported key → owned terminal.\rEND-SESSION\r".encode())
                        self.assertIn(("resize", 132, 42), server.events)
                    else:
                        self.assertEqual(server.received, b"")
                finally:
                    server.close()


class ObservedServerEvents(list):
    """Observe the real server boundary, independently of the Swift actor."""
    def __init__(self, journal):
        super().__init__()
        self.journal = journal
        self.creation_observations = 0

    def append(self, event):
        if event[0] in ["channel", "pty", "shell"]:
            catalog = json.loads(self.journal.read_bytes())
            if catalog["format"] != "frankenterm-terminal-session-history" or len(catalog["records"]) != 1:
                raise AssertionError("missing durable creation marker before real SSH side effect")
            record = catalog["records"][0]
            if record["state"] != "creationStarted" or record["machine"] != "machine" or record["route"] != "route":
                raise AssertionError("wrong durable source identity before real SSH side effect")
            self.creation_observations += 1
        super().append(event)


@unittest.skipUnless(PROBE, "provide WorkspaceSSHProbe for protected-worker and journal scenarios")
class ProtectedWorkspaceInteropTests(unittest.TestCase):
    def run_probe(self, mode, key=None):
        key = key or ed25519.Ed25519PrivateKey.generate()
        server = fixture.LoopbackServer(key)
        host = fixture.public_blob(ec.generate_private_key(ec.SECP256R1())) if mode == "wrongPin" else server.host_blob
        pem = key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.OpenSSH, serialization.NoEncryption())
        request = {"pem": pem.decode()}
        if mode in ["managed", "managedClose"]:
            # Keep the tiny public journal artifact for failed-run diagnosis.
            directory = Path(tempfile.mkdtemp(prefix="frankenterm-workspace-ssh-journal-"))
            request["journalDirectory"] = str(directory)
            server.events = ObservedServerEvents(directory / "catalog.json")
        try:
            process = subprocess.run([PROBE, str(server.address[1]), base64.b64encode(host).decode(), mode],
                                     input=json.dumps(request).encode(), capture_output=True, timeout=15)
            self.assertEqual(process.returncode, 0, process.stderr.decode(errors="replace")[:8192])
            self.assertLess(len(process.stdout), 2048)
            result = json.loads(process.stdout)
            self.assertTrue(result["closed"])
            public = fixture.public_blob(key)
            algorithm, _ = fixture.take_string(public)
            self.assertEqual(result["algorithm"], algorithm.decode())
            self.assertEqual(result["fingerprint"], "SHA256:" + base64.b64encode(hashlib.sha256(public).digest()).decode().rstrip("="))
            self.assertTrue(server.done.wait(2), "SSH server retained a closed connection")
            offered = [event[1] for event in server.events if event[0] == "authentication"]
            side_effects = [event[0] for event in server.events if event[0] in ["channel", "pty", "shell"]]
            if mode in ["io", "foreignCancel", "managed"]:
                self.assertEqual(offered, [0, 1], "libssh must validate the production signature")
                self.assertEqual(side_effects, ["channel", "pty", "shell"])
                self.assertEqual(server.received, "protected workspace → actual SSH 🦀\r\nEND-SESSION\r".encode())
                self.assertEqual(result["bytes"], len(server.received))
                self.assertEqual(result["signs"], 1)
                if mode == "managed":
                    self.assertEqual(server.events.creation_observations, 3)
                    self.assertIn(("resize", 132, 42), server.events)
                    self.assertNotIn(("resize", 160, 50), server.events)
                    journal = (directory / "catalog.json").read_bytes()
                    records = json.loads(journal)["records"]
                    self.assertEqual([record["state"] for record in records], ["acknowledged"])
                    for forbidden in [b"localhost", b"operator", b"PRIVATE KEY", b"END-SESSION", public]:
                        self.assertNotIn(forbidden, journal)
            elif mode in ["beforeShellRevocation", "managedClose"]:
                self.assertEqual(offered, [0, 1])
                self.assertEqual(side_effects, [], "a revoked or retired connection must not create a channel or PTY")
                self.assertEqual(result["signs"], 1)
                if mode == "managedClose":
                    self.assertEqual(server.events.creation_observations, 0)
                    records = json.loads((directory / "catalog.json").read_bytes())["records"]
                    self.assertEqual([record["state"] for record in records], ["interrupted"])
            elif mode in ["revokeKeyDuring", "revokeHostDuring", "reimportDuring", "cancelDuring", "suspendDuring"]:
                self.assertEqual(offered, [0], "a stale or cancelled signature must never reach SSH")
                self.assertEqual(side_effects, [])
                self.assertEqual(result["signs"], 1)
                if mode in ["cancelDuring", "suspendDuring"]:
                    self.assertGreater(result["cancellations"], 0)
            else:
                self.assertEqual(offered, [], "untrusted or stale identity must not even be offered")
                self.assertEqual(side_effects, [])
                self.assertEqual(result["signs"], 0)
            if mode not in ["io", "foreignCancel", "managed"]:
                self.assertEqual(result["bytes"], 0)
                self.assertEqual(server.received, b"")
        finally:
            server.close()

    def test_imported_protected_keys_authenticate_and_open_real_pty_for_all_families(self):
        keys = [ed25519.Ed25519PrivateKey.generate()] + [ec.generate_private_key(curve) for curve in
                                                       [ec.SECP256R1(), ec.SECP384R1(), ec.SECP521R1()]]
        for key in keys:
            with self.subTest(curve=getattr(key, "curve", None)):
                self.run_probe("io", key)

    def test_wrong_peer_revoked_identity_and_stale_revision_never_offer_a_key(self):
        for mode in ["wrongPin", "revokeKeyBefore", "revokeHostBefore", "staleRevision"]:
            with self.subTest(mode=mode):
                self.run_probe(mode)

    def test_independently_serialized_full_padding_block_reaches_real_authentication(self):
        # A 32-byte positive scalar with a clear high bit gives pyca an aligned
        # private body; its serializer appends a complete 01...08 block.
        self.run_probe("io", ec.derive_private_key(1 << 254, ec.SECP256R1()))

    def test_mutation_while_protected_worker_is_active_discards_completed_signature(self):
        for mode in ["revokeKeyDuring", "revokeHostDuring", "reimportDuring"]:
            with self.subTest(mode=mode):
                self.run_probe(mode)

    def test_cancel_or_background_retains_worker_until_callback_settles(self):
        for mode in ["cancelDuring", "suspendDuring"]:
            with self.subTest(mode=mode):
                self.run_probe(mode)

    def test_revocation_after_authentication_prevents_channel_and_shell_creation(self):
        self.run_probe("beforeShellRevocation")

    def test_cancellation_for_another_transport_cannot_kill_this_attempt(self):
        self.run_probe("foreignCancel")

    def test_managed_connection_persists_creation_before_remote_shell_and_blocks_replay(self):
        self.run_probe("managed")

    def test_suspension_joins_parked_creation_without_reviving_old_grants_or_overriding_resume(self):
        self.run_probe("managedClose")


if __name__ == "__main__":
    unittest.main(verbosity=2)
