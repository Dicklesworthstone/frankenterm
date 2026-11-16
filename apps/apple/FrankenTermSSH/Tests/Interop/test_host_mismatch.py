"""Inspect exact SSH host mismatches without offering credentials to that peer.

python Tests/Interop/test_host_mismatch.py /path/to/libftssh.so /path/to/FrankenTermSSHPackageTests.xctest

The independent libssh peers record connection, authentication and forwarding
events. All keys and passwords belong to this local synthetic fixture.
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
spec = importlib.util.spec_from_file_location("native_host_mismatch_fixture", NATIVE_FIXTURE)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

LIVE_TEST = "FrankenTermSSHTests.WorkspaceHostKeyMismatchLiveTests/testLiveMismatchRetainsExactPeerWithoutAuthentication"
PASSWORD = b"verified-hop-password"
AUTH_EVENTS = {"authentication-methods", "authentication", "password-authentication",
               "interactive-request", "interactive-response"}
TERMINAL_EVENTS = {"channel", "pty", "shell"}


def interactive_rounds():
    return [dict(name=b"Verified hop", instruction=b"Enter the one-time code.",
                 prompts=[(b"One-time code:", False)], responses=[b"654321"])]


def xctest_command():
    if os.path.isdir(TEST_BUNDLE):
        return ["xcrun", "xctest", "-XCTest", LIVE_TEST, TEST_BUNDLE]
    return [TEST_BUNDLE, LIVE_TEST]


class HostMismatchInteropTests(unittest.TestCase):
    def assert_closed(self, peer):
        self.assertTrue(peer.done.wait(2), "a rejected host retained SSH work")
        self.assertEqual(peer.errors, [])
        self.assertEqual(peer.received, b"")
        self.assertEqual([event for event in peer.events if event[0] in TERMINAL_EVENTS], [])

    def assert_no_authentication(self, events):
        self.assertEqual([event for event in events if event[0] in AUTH_EVENTS], [],
                         "the mismatching peer received an account or authentication offer")

    def test_direct_and_nested_host_identity_failures(self):
        modes = ["direct-key", "direct-password", "direct-interactive",
                 "destination-after-hops", "second-hop", "revoked-presented", "reviewed-update"]
        for mode in modes:
            with self.subTest(mode=mode):
                key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
                destination_method = ("password" if mode == "direct-password"
                    else "keyboard-interactive" if mode == "direct-interactive" else "publickey")
                destination = fixture.LoopbackServer(key, connections=2 if mode == "reviewed-update" else 1,
                    password=PASSWORD if destination_method == "password" else None,
                    interactive_rounds=interactive_rounds() if destination_method == "keyboard-interactive" else None)
                methods = (["password", "keyboard-interactive"] if mode == "destination-after-hops"
                    else ["password", "keyboard-interactive", "password"] if mode == "second-hop" else [])
                hops = []
                try:
                    next_peer = destination
                    for method in reversed(methods):
                        peer = fixture.LoopbackServer(key, forward_to=next_peer.address, forward_host="127.0.0.1",
                            password=PASSWORD if method == "password" else None,
                            interactive_rounds=interactive_rounds() if method == "keyboard-interactive" else None)
                        hops.insert(0, peer)
                        next_peer = peer
                    descriptors = [dict(port=peer.address[1], hostKey=base64.b64encode(peer.host_blob).decode(),
                        authentication=method) for peer, method in zip(hops, methods)]
                    environment = dict(os.environ,
                        FT_HOST_MISMATCH_PORT=str(destination.address[1]),
                        FT_HOST_MISMATCH_HOSTKEY=base64.b64encode(destination.host_blob).decode(),
                        FT_HOST_MISMATCH_MODE=mode,
                        FT_HOST_MISMATCH_HOPS=json.dumps(descriptors),
                        PYTHONDONTWRITEBYTECODE="1")
                    result = subprocess.run(xctest_command(), env=environment,
                        capture_output=True, text=True, timeout=25)
                    output = result.stdout + result.stderr
                    self.assertEqual(result.returncode, 0, output[-18000:])
                    self.assertIn("Executed 1 test", output)
                    self.assertNotIn("skipped", output.lower())

                    if mode == "second-hop":
                        self.assertEqual(destination.accepted_connections, 0)
                        self.assertEqual(destination.events, [])
                        self.assertEqual(hops[2].accepted_connections, 0)
                        self.assertEqual(hops[2].events, [])
                        self.assertEqual(hops[1].accepted_connections, 1)
                        self.assert_closed(hops[1])
                        self.assert_no_authentication(hops[1].events)
                        self.assertEqual([event for event in hops[1].events if event[0] == "forward"], [])
                    else:
                        self.assertEqual(destination.accepted_connections, 2 if mode == "reviewed-update" else 1)
                        self.assert_closed(destination)
                        self.assertEqual([event for event in destination.events if event[0] == "forward"], [])
                        if mode == "reviewed-update":
                            boundary = destination.events.index(("connection", 2))
                            self.assert_no_authentication(destination.events[:boundary])
                            offers = [event for event in destination.events[boundary:] if event[0] == "authentication"]
                            self.assertEqual([event[1] for event in offers], [0, 1])
                            self.assertTrue(all(event[2] == b"operator" and event[3] == fixture.public_blob(key)
                                                for event in offers))
                        else:
                            self.assert_no_authentication(destination.events)

                    trusted_hops = 1 if mode == "second-hop" else len(hops)
                    for index in range(trusted_hops):
                        peer, method = hops[index], methods[index]
                        self.assertEqual(peer.accepted_connections, 1)
                        self.assert_closed(peer)
                        self.assertEqual([event for event in peer.events if event[0] == "authentication"], [])
                        if method == "password":
                            self.assertEqual([event for event in peer.events if event[0] == "password-authentication"],
                                [("password-authentication", b"operator", len(PASSWORD), True)])
                            self.assertEqual([event for event in peer.events if event[0] == "interactive-request"], [])
                        else:
                            self.assertEqual([event for event in peer.events if event[0] == "password-authentication"], [])
                            self.assertEqual([event for event in peer.events if event[0] == "interactive-request"],
                                [("interactive-request", b"operator")])
                            self.assertEqual([event for event in peer.events if event[0] == "interactive-response"],
                                [("interactive-response", 1, 1, True)])
                        next_peer = hops[index + 1] if index + 1 < len(hops) else destination
                        self.assertEqual([event for event in peer.events if event[0] == "forward"],
                            [("forward", b"127.0.0.1", next_peer.address[1])])
                    print(f"host mismatch scenario passed: {mode}", flush=True)
                finally:
                    for peer in hops:
                        peer.close()
                    destination.close()


if __name__ == "__main__":
    unittest.main()
