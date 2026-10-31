"""Explicit workspace route/password/MFA choices through real SSH peers.

python Tests/Interop/test_explicit_selection.py /path/to/libftssh.so /path/to/FrankenTermSSHPackageTests.xctest

Every configured route has a usable imported key first. The independent libssh
servers prove that the reviewed methods alone reach the wire, that three mixed
hops never create shells, and that stale choices cannot start network work.
"""
import base64
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest

from cryptography.hazmat.primitives.asymmetric import ed25519

NATIVE_LIBRARY, TEST_BUNDLE = sys.argv[1:3]
NATIVE_FIXTURE = (Path(sys.argv[3]) if len(sys.argv) > 3
                  else Path(__file__).resolve().parents[1] / "Native" / "test_native.py")
sys.argv = [sys.argv[0], NATIVE_LIBRARY]
spec = importlib.util.spec_from_file_location("native_explicit_selection_fixture", NATIVE_FIXTURE)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

LIVE_TEST = "FrankenTermSSHTests.WorkspaceExplicitSelectionTests/testLiveExplicitRouteAuthentication"
PASSWORD = b"selected-password"
TERMINAL_INPUT = "Explicit route → owned terminal.\rEND-SESSION\r".encode()


def interactive_rounds():
    return [dict(name=b"Verification", instruction=b"Enter the one-time code.",
                 prompts=[(b"Verification code:", False)], responses=[b"654321"])]


def xctest_command():
    if os.path.isdir(TEST_BUNDLE):
        return ["xcrun", "xctest", "-XCTest", LIVE_TEST, TEST_BUNDLE]
    return [TEST_BUNDLE, LIVE_TEST]


class ExplicitSelectionInteropTests(unittest.TestCase):
    def assert_authentication(self, server, method, *, refused=False, terminal=False):
        self.assertEqual(server.accepted_connections, 1)
        self.assertTrue(server.done.wait(2), "a closed connection retained its SSH worker")
        self.assertEqual(server.errors, [])
        self.assertEqual([event for event in server.events if event[0] == "authentication"], [],
                         "the explicit prompt choice offered an imported key")
        passwords = [event for event in server.events if event[0] == "password-authentication"]
        requests = [event for event in server.events if event[0] == "interactive-request"]
        responses = [event for event in server.events if event[0] == "interactive-response"]
        if method == "password":
            size = len(b"wrong-password") if refused else len(PASSWORD)
            self.assertEqual(passwords, [("password-authentication", b"operator", size, not refused)])
            self.assertEqual(requests, [])
            self.assertEqual(responses, [])
        else:
            self.assertEqual(passwords, [])
            self.assertEqual(requests, [("interactive-request", b"operator")])
            self.assertEqual(responses, [("interactive-response", 1, 1, True)])
        effects = [event[0] for event in server.events if event[0] in ["channel", "pty", "shell"]]
        self.assertEqual(effects, ["channel", "pty", "shell"] if terminal else [])
        self.assertEqual(server.received, TERMINAL_INPUT if terminal else b"")

    def test_reviewed_route_and_every_hop_use_only_the_selected_method(self):
        modes = ["password", "interactive", "jump-mixed", "jump-destination-refused",
                 "stale", "hop-mismatch", "missing-prompter"]
        for mode in modes:
            with self.subTest(mode=mode):
                key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
                destination_method = "keyboard-interactive" if mode in ["interactive", "jump-mixed"] else "password"
                destination = fixture.LoopbackServer(key,
                    password=PASSWORD if destination_method == "password" else None,
                    interactive_rounds=interactive_rounds() if destination_method == "keyboard-interactive" else None)
                methods = (["password", "keyboard-interactive", "password"] if mode == "jump-mixed"
                           else ["keyboard-interactive"] if mode == "missing-prompter"
                           else ["password"] if mode in ["hop-mismatch", "jump-destination-refused"] else [])
                hops = []
                try:
                    next_peer = destination
                    for method in reversed(methods):
                        peer = fixture.LoopbackServer(key, forward_to=next_peer.address, forward_host="127.0.0.1",
                            password=PASSWORD if method == "password" else None,
                            interactive_rounds=interactive_rounds() if method == "keyboard-interactive" else None)
                        hops.insert(0, peer)
                        next_peer = peer
                    descriptors = [dict(port=peer.address[1],
                        hostKey=base64.b64encode(peer.host_blob).decode(), authentication=method)
                        for peer, method in zip(hops, methods)]
                    environment = dict(os.environ,
                        FT_SELECTION_TEST_PORT=str(destination.address[1]),
                        FT_SELECTION_TEST_HOSTKEY=base64.b64encode(destination.host_blob).decode(),
                        FT_SELECTION_TEST_MODE=mode,
                        FT_SELECTION_TEST_HOPS=json.dumps(descriptors),
                        PYTHONDONTWRITEBYTECODE="1")
                    result = subprocess.run(xctest_command(), env=environment,
                        capture_output=True, text=True, timeout=30)
                    output = result.stdout + result.stderr
                    self.assertEqual(result.returncode, 0, output[-18000:])
                    self.assertIn("Executed 1 test", output)
                    self.assertNotIn("skipped", output.lower())
                    if mode in ["stale", "hop-mismatch", "missing-prompter"]:
                        for peer in [*hops, destination]:
                            self.assertEqual(peer.accepted_connections, 0)
                            self.assertEqual(peer.events, [])
                            self.assertEqual(peer.received, b"")
                    else:
                        refused = mode == "jump-destination-refused"
                        self.assert_authentication(destination, destination_method, refused=refused, terminal=not refused)
                        self.assertEqual([event for event in destination.events if event[0] == "forward"], [])
                        for index, (peer, method) in enumerate(zip(hops, methods)):
                            self.assert_authentication(peer, method)
                            next_peer = hops[index + 1] if index + 1 < len(hops) else destination
                            self.assertEqual([event for event in peer.events if event[0] == "forward"],
                                [("forward", b"127.0.0.1", next_peer.address[1])])
                    print(f"explicit selection scenario passed: {mode}", flush=True)
                finally:
                    for peer in hops:
                        peer.close()
                    destination.close()


if __name__ == "__main__":
    unittest.main()
