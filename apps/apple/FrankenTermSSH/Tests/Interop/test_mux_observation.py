"""Production Swift mux effect boundaries over an independent libssh channel.

The Swift protocol oracle checks every resize/input sink and read-only viewing
progress. The server independently verifies public-key authentication, one exec
channel, no PTY/shell, no raw input, and complete connection retirement.
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
spec = importlib.util.spec_from_file_location(
    "native_observation_fixture", Path(__file__).resolve().parents[1] / "Native" / "test_native.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
SELECTOR = "FrankenTermSSHTests.MuxObservationTests/testLiveMuxViewOnlyAndInteractiveEffectBoundary"


class MuxObservationInteropTests(unittest.TestCase):
    def test_viewing_and_interactive_effect_boundaries(self):
        for mode in ["observe", "interactive", "oversize", "invalid-grid", "no-panes"]:
            with self.subTest(mode=mode):
                key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
                server = fixture.LoopbackServer(key, allow_exec=True)
                try:
                    env = dict(os.environ, FT_MUX_OBSERVATION_PORT=str(server.address[1]),
                        FT_MUX_OBSERVATION_HOSTKEY=base64.b64encode(server.host_blob).decode(),
                        FT_MUX_OBSERVATION_MODE=mode)
                    command = ["xcrun", "xctest", "-XCTest", SELECTOR, TEST_BUNDLE] if os.path.isdir(TEST_BUNDLE) else [TEST_BUNDLE, SELECTOR]
                    result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=25)
                    output = result.stdout + result.stderr
                    self.assertEqual(result.returncode, 0, output[-18000:])
                    self.assertIn("Executed 1 test", output)
                    self.assertNotIn("skipped", output.lower())
                    self.assertEqual(server.accepted_connections, 1)
                    self.assertTrue(server.done.wait(2), "closed observer retained native channel work")
                    self.assertEqual(server.errors, [])
                    self.assertEqual([event[1] for event in server.events if event[0] == "authentication"], [0, 1])
                    self.assertEqual([event for event in server.events if event[0] in ["channel", "pty", "shell", "exec", "resize"]],
                        [("channel",), ("exec", b"fixture-mux-observe")])
                    self.assertEqual(server.received, b"", "Protocol effects bypassed the mux oracle into the SSH byte stream")
                finally:
                    server.close()


if __name__ == "__main__":
    unittest.main()
