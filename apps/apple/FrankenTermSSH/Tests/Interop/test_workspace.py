"""Imported workspace -> real production signer -> libssh2 -> independent libssh.

Run with a built native driver and XCTest bundle, without rebuilding the package:
python Tests/Interop/test_workspace.py /path/to/libftssh.so /path/to/FrankenTermSSHPackageTests.xctest
Synthetic test keys only. No user Keychain, external destination or actual shell.
"""
import base64
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import unittest
from cryptography.hazmat.primitives.asymmetric import ed25519

NATIVE_LIBRARY, TEST_BUNDLE = sys.argv[1:3]
sys.argv = [sys.argv[0], NATIVE_LIBRARY]
spec = importlib.util.spec_from_file_location("native_fixture", Path(__file__).resolve().parents[1] / "Native" / "test_native.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


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
                    result = subprocess.run([TEST_BUNDLE,
                        "FrankenTermSSHTests.WorkspaceAuthenticationTests/testLiveImportedWorkspaceAuthenticationAndPTY"],
                        env=environment, capture_output=True, text=True, timeout=20)
                    self.assertEqual(result.returncode, 0, result.stdout[-12000:] + result.stderr[-12000:])
                    self.assertIn("Executed 1 test", result.stdout)
                    self.assertNotIn("skipped", result.stdout.lower())
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


if __name__ == "__main__":
    unittest.main(verbosity=2)
