"""Real signed-public-key to keyboard-interactive continuation and ownership.

python Tests/Interop/test_multifactor.py /path/to/libftssh.so /path/to/XCTest-runner

The independent libssh fixture requires a valid signed first factor, emits a
partial-success reply, and checks the second factor on the same connection.
Assertions cover native actors and the production workspace connection factory.
Only synthetic fixture responses are used; event records never include them.
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
    "native_multifactor_fixture",
    Path(__file__).resolve().parents[1] / "Native" / "test_native.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

NATIVE_TEST = "FrankenTermSSHTests.NativeMultifactorAuthenticationTests/testLivePublicKeyInteractiveContinuation"
WORKSPACE_TEST = "FrankenTermSSHTests.WorkspaceMultifactorAuthenticationTests/testLiveWorkspacePublicKeyInteractiveFactory"

MODES = [
    "success", "key-only", "wrong-host", "wrong-key", "rejected-signature",
    "unsigned-success", "unsigned-partial", "unsupported-method", "wrong-account",
    "wrong-response", "interactive-partial", "key-only-api", "cancel",
    "close-prompt", "combined-deadline", "revoke-key-prompt",
    "revoke-interactive-prompt", "revoke-key-live", "revoke-interactive-live",
    "hop-success", "hop-key-revoked", "hop-interactive-revoked",
    "workspace-success", "workspace-key-only", "workspace-disallowed",
    "workspace-no-prompter", "workspace-jump", "workspace-key-revoked-prompt",
    "workspace-key-revoked-live", "workspace-hop-key-revoked",
    "workspace-jump-destination-refused",
]
NO_INTERACTIVE = {
    "key-only", "wrong-host", "wrong-key", "rejected-signature",
    "unsigned-success", "unsigned-partial", "unsupported-method", "wrong-account",
    "key-only-api", "workspace-key-only", "workspace-disallowed",
    "workspace-no-prompter", "workspace-jump-destination-refused",
}
PENDING_PROMPT = {
    "cancel", "close-prompt", "combined-deadline", "revoke-key-prompt",
    "revoke-interactive-prompt", "workspace-key-revoked-prompt",
}
SHELL_MODES = {
    "success", "key-only", "hop-success", "workspace-success",
    "workspace-key-only", "workspace-jump",
}


def rounds():
    return [
        dict(name=b"Verification", instruction=b"Enter the one-time code.",
             prompts=[(b"Verification code:", False)], responses=[b"654321"]),
        dict(name=b"Recovery", instruction=b"Enter the recovery token.",
             prompts=[(b"Recovery token:", False)], responses=[b"approved"]),
    ]


def xctest_command(mode):
    selected = WORKSPACE_TEST if mode.startswith("workspace-") else NATIVE_TEST
    if os.path.isdir(TEST_BUNDLE):
        return ["xcrun", "xctest", "-XCTest", selected, TEST_BUNDLE]
    return [TEST_BUNDLE, selected]


class MultifactorInteropTests(unittest.TestCase):
    def assert_retired(self, peer, label):
        self.assertEqual(peer.accepted_connections, 1, label)
        self.assertTrue(peer.done.wait(2), "Closed owner retained " + label)
        self.assertEqual(peer.errors, [], label)

    def assert_authentication(self, peer, states, mode, wrong_key=False):
        events = peer.events
        offers = [event for event in events if event[0] == "authentication"]
        self.assertEqual([event[1] for event in offers], states, mode)
        self.assertTrue(all(event[2] == b"operator" for event in offers), mode)
        if wrong_key:
            self.assertTrue(all(event[3] != peer.public for event in offers), mode)
        else:
            self.assertTrue(all(event[3] == peer.public for event in offers), mode)
        self.assertEqual([event for event in events if event[0] in
                          ("password-authentication", "unexpected-method-probe", "invalid-signature")], [], mode)
        probes = [event for event in events if event[0] == "authentication-methods"]
        self.assertTrue(all(event[1] == b"operator" for event in probes), mode)
        signed = [index for index, event in enumerate(events)
                  if event[0] == "authentication" and event[1] == 1]
        if signed:
            # Continuing must consume the proven partial result directly. A
            # fresh none/method query can destroy server-side MFA state.
            self.assertEqual([event for event in events[signed[0] + 1:]
                              if event[0] == "authentication-methods"], [], mode)
            for index, event in enumerate(events):
                if event[0] == "interactive-request":
                    self.assertGreater(index, signed[0], mode)

    def assert_interactive(self, peer, expected_requests, expected_responses, mode):
        self.assertEqual([event for event in peer.events if event[0] == "interactive-request"],
                         expected_requests, mode)
        self.assertEqual([event for event in peer.events if event[0] == "interactive-response"],
                         expected_responses, mode)

    def test_public_key_interactive_authentication_and_authority_lifetime(self):
        completed = 0
        for mode in MODES:
            with self.subTest(mode=mode):
                key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
                auth_mode = {
                    "key-only": "normal", "workspace-key-only": "normal",
                    "rejected-signature": "rejectSignature",
                    "unsigned-success": "unsignedSuccess", "unsigned-partial": "unsignedPartial",
                    "unsupported-method": "publickeyInteractiveNoKI",
                    "interactive-partial": "publickeyInteractivePartial",
                }.get(mode, "publickeyInteractive")
                # Normal mode advertises KI when given challenges. Complete
                # key-only success must advertise publickey and never start KI.
                challenges = None if mode in {"key-only", "workspace-key-only"} else rounds()
                destination = fixture.LoopbackServer(key, interactive_rounds=challenges,
                    auth_mode=auth_mode, reject=mode == "workspace-jump-destination-refused")
                hop = None
                try:
                    environment = dict(os.environ,
                        FT_MFA_TEST_PORT=str(destination.address[1]),
                        FT_MFA_TEST_HOSTKEY=base64.b64encode(destination.host_blob).decode(),
                        FT_MFA_TEST_MODE=mode)
                    jumped = mode.startswith("hop-") or mode in {
                        "workspace-jump", "workspace-hop-key-revoked", "workspace-jump-destination-refused"}
                    forward_host = "target.internal.invalid" if mode.startswith("workspace-") else "127.0.0.1"
                    if jumped:
                        hop_key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes([41]) * 32)
                        hop = fixture.LoopbackServer(hop_key, interactive_rounds=rounds(),
                            auth_mode="publickeyInteractive", forward_to=destination.address,
                            forward_host=forward_host)
                        environment.update(
                            FT_MFA_TEST_HOP_PORT=str(hop.address[1]),
                            FT_MFA_TEST_HOP_HOSTKEY=base64.b64encode(hop.host_blob).decode())
                    result = subprocess.run(xctest_command(mode), env=environment,
                        capture_output=True, text=True, timeout=30)
                    output = result.stdout + result.stderr
                    self.assertEqual(result.returncode, 0, output[-18000:])
                    self.assertIn("Executed 1 test", output)
                    self.assertNotIn("skipped", output.lower())
                    self.assert_retired(destination, "destination in " + mode)

                    if mode == "wrong-host":
                        states = []
                    elif mode in {"wrong-key", "unsigned-success", "unsigned-partial", "workspace-jump-destination-refused"}:
                        states = [0]
                    else:
                        states = [0, 1]
                    self.assert_authentication(destination, states, mode, wrong_key=mode == "wrong-key")
                    if mode in NO_INTERACTIVE:
                        requests, responses = [], []
                    else:
                        requests = [("interactive-request", b"operator")]
                        if mode in PENDING_PROMPT:
                            responses = []
                        elif mode == "wrong-response":
                            responses = [("interactive-response", 1, 1, False)]
                        else:
                            responses = [("interactive-response", 1, 1, True),
                                         ("interactive-response", 2, 1, True)]
                    self.assert_interactive(destination, requests, responses, mode)

                    effects = [event[0] for event in destination.events
                               if event[0] in ("channel", "pty", "shell", "exec", "forward")]
                    creates_shell = mode in SHELL_MODES
                    self.assertEqual(effects, ["channel", "pty", "shell"] if creates_shell else [], mode)
                    payload = ("Workspace MFA → owned terminal.\rEND-SESSION\r" if mode.startswith("workspace-")
                               else "Native MFA → owned terminal.\rEND-SESSION\r").encode()
                    self.assertEqual(destination.received, payload if creates_shell else b"", mode)
                    if creates_shell and responses:
                        last_response = max(index for index, event in enumerate(destination.events)
                                            if event[0] == "interactive-response")
                        first_channel = next(index for index, event in enumerate(destination.events)
                                             if event[0] == "channel")
                        self.assertGreater(first_channel, last_response, mode)

                    if hop is not None:
                        self.assert_retired(hop, "jump host in " + mode)
                        self.assert_authentication(hop, [0, 1], mode)
                        self.assert_interactive(hop, [("interactive-request", b"operator")],
                            [("interactive-response", 1, 1, True), ("interactive-response", 2, 1, True)], mode)
                        self.assertEqual([event for event in hop.events
                                          if event[0] in ("channel", "pty", "shell", "exec")], [], mode)
                        self.assertEqual([event for event in hop.events if event[0] == "forward"],
                            [("forward", forward_host.encode(), destination.address[1])], mode)
                        last_response = max(index for index, event in enumerate(hop.events)
                                            if event[0] == "interactive-response")
                        forwarding = next(index for index, event in enumerate(hop.events) if event[0] == "forward")
                        self.assertGreater(forwarding, last_response, mode)
                    completed += 1
                finally:
                    if hop is not None:
                        hop.close()
                    destination.close()
        if completed == len(MODES):
            print(f"Validated {completed} real MFA authentication scenarios.")


if __name__ == "__main__":
    unittest.main()
