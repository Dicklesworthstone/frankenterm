"""Real Swift keyboard-interactive prompts, cancellation and jump ownership.

python Tests/Interop/test_interactive.py /path/to/libftssh.so /path/to/FrankenTermSSHPackageTests.xctest

The independent libssh fixture checks response equality and ordered remote
effects. Neither credential responses nor arbitrary server text enter reports.
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
    "native_interactive_fixture", Path(__file__).resolve().parents[1] / "Native" / "test_native.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

LIVE_TEST = "FrankenTermSSHTests.NativeInteractiveAuthenticationTests/testLiveInteractivePromptsAndTerminalOwnership"
WORKSPACE_TEST = "FrankenTermSSHTests.NativeInteractiveAuthenticationTests/testLiveWorkspaceInteractiveFactory"


def rounds():
    return [dict(name=b"Sign in", instruction=b"Enter the account password.",
                 prompts=[(b"Account:", True), (b"Password:", False)],
                 responses=[b"operator", "pw-é🔐".encode()]),
            dict(name=b"Verification", instruction=b"Enter the one-time code.",
                 prompts=[(b"Verification code:", False)], responses=[b"654321"])]


def xctest_command(mode):
    selected = WORKSPACE_TEST if mode.startswith("workspace-") else LIVE_TEST
    if os.path.isdir(TEST_BUNDLE):
        return ["xcrun", "xctest", "-XCTest", selected, TEST_BUNDLE]
    return [TEST_BUNDLE, selected]


class InteractiveInteropTests(unittest.TestCase):
    def test_interactive_authentication_and_prompt_lifetime(self):
        modes = ["success", "zero-prompts", "wrong-host", "wrong-response", "unsupported", "none",
                 "immediate", "partial", "extra-response", "oversize-response", "cancel", "close-prompt",
                 "timeout", "revoke-prompt", "revoke-live", "forward-success", "forward-revoked",
                 "workspace-success", "workspace-jump", "workspace-revoked", "workspace-jump-revoked"]
        for mode in modes:
            with self.subTest(mode=mode):
                key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
                auth_mode = {"unsupported": "interactiveDisabled", "none": "noneSucceeds",
                             "immediate": "interactiveImmediateSuccess", "partial": "interactivePartial"}.get(mode, "normal")
                challenges = [dict(name=b"Notice", instruction=b"Continue.", prompts=[], responses=[])] if mode == "zero-prompts" else rounds()
                destination = fixture.LoopbackServer(key, interactive_rounds=challenges, auth_mode=auth_mode)
                hop = None
                try:
                    environment = dict(os.environ,
                        FT_INTERACTIVE_TEST_PORT=str(destination.address[1]),
                        FT_INTERACTIVE_TEST_HOSTKEY=base64.b64encode(destination.host_blob).decode(),
                        FT_INTERACTIVE_TEST_MODE=mode)
                    forward_host = "target.internal.invalid" if mode.startswith("workspace-") else "127.0.0.1"
                    if mode.startswith("forward-") or mode.startswith("workspace-jump"):
                        hop = fixture.LoopbackServer(key, interactive_rounds=rounds(),
                            forward_to=destination.address, forward_host=forward_host)
                        environment.update(
                            FT_INTERACTIVE_TEST_HOP_PORT=str(hop.address[1]),
                            FT_INTERACTIVE_TEST_HOP_HOSTKEY=base64.b64encode(hop.host_blob).decode())
                    result = subprocess.run(xctest_command(mode), env=environment,
                        capture_output=True, text=True, timeout=20)
                    output = result.stdout + result.stderr
                    self.assertEqual(result.returncode, 0, output[-18000:])
                    self.assertIn("Executed 1 test", output)
                    self.assertNotIn("skipped", output.lower())
                    self.assertEqual(destination.accepted_connections, 1)
                    self.assertTrue(destination.done.wait(2), "closed actor retained destination work")
                    self.assertEqual(destination.errors, [])

                    requests = [event for event in destination.events if event[0] == "interactive-request"]
                    self.assertEqual(requests, [] if mode in ["wrong-host", "unsupported", "none"]
                                     else [("interactive-request", b"operator")])
                    responses = [event for event in destination.events if event[0] == "interactive-response"]
                    if mode == "zero-prompts":
                        expected_responses = [("interactive-response", 1, 0, True)]
                    elif mode == "wrong-response":
                        expected_responses = [("interactive-response", 1, 2, False)]
                    elif mode in ["success", "partial", "revoke-live", "forward-success", "forward-revoked"] or mode.startswith("workspace-"):
                        expected_responses = [("interactive-response", 1, 2, True), ("interactive-response", 2, 1, True)]
                    else:
                        expected_responses = []
                    self.assertEqual(responses, expected_responses)
                    self.assertEqual([event for event in destination.events if event[0] in
                                      ["authentication", "password-authentication"]], [])
                    effects = [event[0] for event in destination.events if event[0] in ["channel", "pty", "shell"]]
                    creates_shell = mode in ["success", "zero-prompts", "forward-success", "workspace-success", "workspace-jump"]
                    self.assertEqual(effects, ["channel", "pty", "shell"] if creates_shell else [])
                    terminal_input = "Workspace interactive → owned terminal.\rEND-SESSION\r" if mode.startswith("workspace-") else "Interactive → owned terminal.\rEND-SESSION\r"
                    self.assertEqual(destination.received, terminal_input.encode() if creates_shell else b"")
                    if hop is not None:
                        self.assertEqual(hop.accepted_connections, 1)
                        self.assertTrue(hop.done.wait(2), "closed child retained hop work")
                        self.assertEqual(hop.errors, [])
                        self.assertEqual([event for event in hop.events if event[0] == "interactive-request"],
                            [("interactive-request", b"operator")])
                        self.assertEqual([event for event in hop.events if event[0] == "interactive-response"],
                            [("interactive-response", 1, 2, True), ("interactive-response", 2, 1, True)])
                        self.assertEqual([event for event in hop.events if event[0] in
                            ["authentication", "password-authentication", "channel", "pty", "shell"]], [])
                        self.assertEqual([event for event in hop.events if event[0] == "forward"],
                            [("forward", forward_host.encode(), destination.address[1])])
                finally:
                    if hop is not None:
                        hop.close()
                    destination.close()


if __name__ == "__main__":
    unittest.main()
