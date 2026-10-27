"""Real native password transport, prompt retirement, and jump ownership.

Run against the built native driver and Swift XCTest product:
python Tests/Interop/test_password.py /path/to/libftssh.so /path/to/FrankenTermSSHPackageTests.xctest

The independent libssh server accepts only a fixed synthetic password and
records request counts, lengths, and matching booleans, never secret contents.
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
    "native_password_fixture", Path(__file__).resolve().parents[1] / "Native" / "test_native.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

LIVE_TEST = "FrankenTermSSHTests.NativePasswordAuthenticationTests/testLivePasswordPromptAndTerminalOwnership"
PASSWORD = "pw-é🔐".encode()


def xctest_command():
    if os.path.isdir(TEST_BUNDLE):
        return ["xcrun", "xctest", "-XCTest", LIVE_TEST, TEST_BUNDLE]
    return [TEST_BUNDLE, LIVE_TEST]


class PasswordInteropTests(unittest.TestCase):
    def test_password_authentication_and_prompt_lifetime(self):
        modes = ["success", "wrong-host", "wrong-password", "unsupported", "none",
                 "partial", "change", "cancel", "timeout", "revoke-prompt",
                 "revoke-live", "forward-success", "forward-revoked"]
        for mode in modes:
            with self.subTest(mode=mode):
                key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
                auth_mode = {"unsupported": "passwordDisabled", "none": "noneSucceeds",
                             "partial": "passwordPartial", "change": "passwordChange"}.get(mode, "normal")
                destination = fixture.LoopbackServer(key, password=PASSWORD, auth_mode=auth_mode)
                hop = None
                try:
                    environment = dict(os.environ,
                        FT_PASSWORD_TEST_PORT=str(destination.address[1]),
                        FT_PASSWORD_TEST_HOSTKEY=base64.b64encode(destination.host_blob).decode(),
                        FT_PASSWORD_TEST_MODE=mode)
                    if mode.startswith("forward-"):
                        hop = fixture.LoopbackServer(key, password=PASSWORD,
                            forward_to=destination.address, forward_host="127.0.0.1")
                        environment.update(
                            FT_PASSWORD_TEST_HOP_PORT=str(hop.address[1]),
                            FT_PASSWORD_TEST_HOP_HOSTKEY=base64.b64encode(hop.host_blob).decode())
                    result = subprocess.run(xctest_command(), env=environment,
                        capture_output=True, text=True, timeout=20)
                    output = result.stdout + result.stderr
                    self.assertEqual(result.returncode, 0, output[-18000:])
                    self.assertIn("Executed 1 test", output)
                    self.assertNotIn("skipped", output.lower())
                    self.assertEqual(destination.accepted_connections, 1)
                    self.assertTrue(destination.done.wait(2), "closed actor retained destination work")
                    self.assertEqual(destination.errors, [])

                    offers = [event for event in destination.events if event[0] == "password-authentication"]
                    expected_offer = mode not in ["wrong-host", "unsupported", "none", "cancel", "timeout", "revoke-prompt"]
                    if expected_offer:
                        supplied = b"wrong-password" if mode == "wrong-password" else PASSWORD
                        self.assertEqual(offers, [("password-authentication", b"operator",
                                                  len(supplied), mode != "wrong-password")])
                    else:
                        self.assertEqual(offers, [])
                    self.assertEqual([event for event in destination.events if event[0] == "authentication"], [])
                    effects = [event[0] for event in destination.events if event[0] in ["channel", "pty", "shell"]]
                    creates_shell = mode in ["success", "forward-success"]
                    self.assertEqual(effects, ["channel", "pty", "shell"] if creates_shell else [])
                    self.assertEqual(destination.received,
                        "Password → owned terminal.\rEND-SESSION\r".encode() if creates_shell else b"")

                    if hop is not None:
                        self.assertEqual(hop.accepted_connections, 1)
                        self.assertTrue(hop.done.wait(2), "closed child retained hop work")
                        self.assertEqual(hop.errors, [])
                        self.assertEqual([event for event in hop.events if event[0] == "password-authentication"],
                            [("password-authentication", b"operator", len(PASSWORD), True)])
                        self.assertEqual([event for event in hop.events if event[0] in
                            ["authentication", "channel", "pty", "shell"]], [])
                        self.assertEqual([event for event in hop.events if event[0] == "forward"],
                            [("forward", b"127.0.0.1", destination.address[1])])
                finally:
                    if hop is not None:
                        hop.close()
                    destination.close()


if __name__ == "__main__":
    unittest.main()
