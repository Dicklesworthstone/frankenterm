"""Mandatory transport policy against independent bounded libssh peers.

  python Tests/Interop/test_transport_algorithms.py libftssh.so --native-only
  python Tests/Interop/test_transport_algorithms.py libftssh.so NativeTests.xctest

Generated synthetic identities and loopback sockets only. The peer's five
algorithm lists are configured independently, including asymmetric directions.
No shell, PTY, external endpoint or user's SSH configuration is used.
"""
import base64
import ctypes as C
import importlib.util
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import time
import unittest


NATIVE_LIBRARY, TEST_BUNDLE = sys.argv[1:3]
NATIVE_ONLY = TEST_BUNDLE == "--native-only"
WORKSPACE_ONLY = "--workspace-only" in sys.argv[3:]
sys.argv = [sys.argv[0], NATIVE_LIBRARY]
spec = importlib.util.spec_from_file_location("native_transport_algorithm_fixture",
    Path(__file__).resolve().parents[1] / "Native/test_native.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
sys.argv = [sys.argv[0]]
DRIVER, P, I, Z = fixture.DRIVER, C.c_void_p, C.c_int, C.c_size_t
fixture.bind(DRIVER, "ftssh_supports_transport_algorithm_policy", I)
fixture.bind(DRIVER, "ftssh_set_transport_algorithms", I, P, P, Z, P, Z, P, Z)

EXCHANGES = ["curve25519-sha256", "curve25519-sha256@libssh.org", "ecdh-sha2-nistp256",
             "ecdh-sha2-nistp384", "ecdh-sha2-nistp521"]
CIPHERS = ["aes256-ctr", "aes128-ctr"]
MACS = ["hmac-sha2-256-etm@openssh.com", "hmac-sha2-512-etm@openssh.com"]
AUTH_EVENTS = {"authentication-methods", "authentication", "password-authentication",
               "interactive-request", "interactive-response"}
CLIENT = fixture.ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
LIVE_TEST = "FrankenTermSSHTests.WorkspaceTransportAlgorithmLiveTests/testLiveTransportPolicyBeforeCredentials"


def profile(exchanges=EXCHANGES, ciphers=CIPHERS, macs=MACS):
    return dict(kex=",".join(exchanges), cipher_c_s=",".join(ciphers), cipher_s_c=",".join(ciphers),
                mac_c_s=",".join(macs), mac_s_c=",".join(macs))


def set_policy(handle, values):
    encoded = [value.encode("ascii") if isinstance(value, str) else value for value in values]
    return DRIVER.ftssh_set_transport_algorithms(handle,
        encoded[0], len(encoded[0]), encoded[1], len(encoded[1]), encoded[2], len(encoded[2]))


class NativeTransportAlgorithmPolicyTests(unittest.TestCase):
    def exchange(self, selected, offered, *, expected=None, invalid=(), success=True):
        self.assertEqual(DRIVER.ftssh_supports_transport_algorithm_policy(), 1)
        peer = fixture.LoopbackServer(CLIENT, transport_algorithms=offered)
        client = socket.create_connection(peer.address, timeout=5)
        handle = P()
        try:
            self.assertEqual(DRIVER.ftssh_create(client.fileno(), 4 * 1024 * 1024, C.byref(handle)), 0)
            client.detach()
            self.assertEqual(set_policy(handle, selected), 0)
            before = fixture.Stats()
            self.assertEqual(DRIVER.ftssh_get_stats(handle, C.byref(before)), 0)
            for candidate in invalid:
                with self.subTest(invalid=repr(candidate)):
                    self.assertIn(set_policy(handle, candidate), (-1000, -1010))
            after = fixture.Stats()
            self.assertEqual(DRIVER.ftssh_get_stats(handle, C.byref(after)), 0)
            self.assertEqual(after.allocated, before.allocated, "Refused policy must not retain validation allocations")
            deadline = time.monotonic() + 8
            result = DRIVER.ftssh_handshake(handle)
            self.assertEqual(set_policy(handle, selected), -1001, "The first handshake freezes every list, even on EAGAIN")
            while result == 1:
                self.assertLess(time.monotonic(), deadline, "Independent peer KEX did not settle")
                fd, directions = DRIVER.ftssh_descriptor(handle), DRIVER.ftssh_directions(handle)
                select.select([fd] if directions & 1 else [], [fd] if directions & 2 else [], [], 0.02)
                result = DRIVER.ftssh_handshake(handle)
            self.assertEqual(result, 0 if success else -1004)
            if success:
                negotiated = []
                for method in [0, 2, 3, 4, 5]:
                    value = C.create_string_buffer(128)
                    self.assertEqual(DRIVER.ftssh_copy_method(handle, method, value, len(value)), 0)
                    negotiated.append(value.value.decode("ascii"))
                self.assertEqual(negotiated, expected)
        finally:
            client.close()
            if handle:
                self.assertEqual(DRIVER.ftssh_destroy(C.byref(handle)), 0)
            peer.close()
        self.assertEqual([e for e in peer.events if e[0] in AUTH_EVENTS | {"channel", "pty", "shell", "forward"}], [])
        self.assertEqual(peer.accepted_connections, 1)

    def test_each_qualified_exchange_can_be_selected_without_profile_expansion(self):
        for exchange in EXCHANGES:
            with self.subTest(exchange=exchange):
                self.exchange([exchange, CIPHERS[1], MACS[1]], profile([exchange], CIPHERS, MACS),
                    expected=[exchange, CIPHERS[1], CIPHERS[1], MACS[1], MACS[1]])

    def test_client_order_is_preserved_for_all_five_negotiated_names(self):
        for reverse in [False, True]:
            exchanges = list(reversed(EXCHANGES)) if reverse else EXCHANGES
            ciphers = list(reversed(CIPHERS)) if reverse else CIPHERS
            macs = list(reversed(MACS)) if reverse else MACS
            with self.subTest(reverse=reverse):
                self.exchange([",".join(exchanges), ",".join(ciphers), ",".join(macs)], profile(),
                    expected=[exchanges[0], ciphers[0], ciphers[0], macs[0], macs[0]])

    def test_both_directions_keep_independent_negotiated_evidence(self):
        offered = profile()
        offered.update(cipher_c_s=CIPHERS[1], cipher_s_c=CIPHERS[0], mac_c_s=MACS[0], mac_s_c=MACS[1])
        self.exchange([",".join(EXCHANGES), ",".join(CIPHERS), ",".join(MACS)], offered,
            expected=[EXCHANGES[0], CIPHERS[1], CIPHERS[0], MACS[0], MACS[1]])

    def test_all_lists_validate_before_any_preference_changes(self):
        original = [EXCHANGES[4], CIPHERS[1], MACS[1]]
        replacement = [EXCHANGES[0], CIPHERS[0], MACS[0]]
        invalid = []
        for index, valid in enumerate(replacement):
            for bad in [b"", b"," + valid.encode(), valid.encode() + b",", (valid + "," + valid).encode(),
                        (valid + ",unknown").encode(), ("+" + valid).encode(), ("^" + valid).encode(),
                        ("-" + valid).encode(), (valid + "*").encode(), valid.encode() + b"\0",
                        (" " + valid).encode(), b"x" * 1024]:
                candidate = replacement[:]; candidate[index] = bad; invalid.append(candidate)
        invalid += [["diffie-hellman-group14-sha1", CIPHERS[0], MACS[0]],
                    [EXCHANGES[0], "aes256-gcm@openssh.com", MACS[0]],
                    [EXCHANGES[0], CIPHERS[0], "hmac-sha2-256"]]
        self.exchange(original, profile(), invalid=invalid,
            expected=[original[0], original[1], original[1], original[2], original[2]])

    def test_no_intersection_in_any_direction_ends_before_username_or_authentication(self):
        selected = [EXCHANGES[0], CIPHERS[0], MACS[0]]
        for name, unavailable in [("kex", EXCHANGES[2]), ("cipher_c_s", CIPHERS[1]),
                                  ("cipher_s_c", CIPHERS[1]), ("mac_c_s", MACS[1]), ("mac_s_c", MACS[1])]:
            offered = profile(); offered[name] = unavailable
            with self.subTest(direction=name):
                self.exchange(selected, offered, success=False)


class WorkspaceTransportAlgorithmInteropTests(unittest.TestCase):
    def invoke(self, mode, peer, expected, hop=None):
        descriptor = None if hop is None else dict(port=hop.address[1], hostKey=base64.b64encode(hop.host_blob).decode())
        environment = dict(os.environ, FT_TRANSPORT_POLICY_CASE=mode, FT_TRANSPORT_POLICY_PORT=str(peer.address[1]),
            FT_TRANSPORT_POLICY_HOSTKEY=base64.b64encode(peer.host_blob).decode(),
            FT_TRANSPORT_POLICY_HOP=json.dumps(descriptor), FT_TRANSPORT_POLICY_EXPECTED=json.dumps(expected),
            PYTHONDONTWRITEBYTECODE="1")
        command = (["xcrun", "xctest", "-XCTest", LIVE_TEST, TEST_BUNDLE] if os.path.isdir(TEST_BUNDLE)
                   else [TEST_BUNDLE, LIVE_TEST])
        result = subprocess.run(command, env=environment, capture_output=True, text=True, timeout=25)
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, 0, mode + "\n" + output[-20000:])
        self.assertIn("Executed 1 test", output)
        self.assertNotIn("skipped", output.lower())

    def assert_no_authentication(self, peer):
        self.assertEqual([event for event in peer.events if event[0] in AUTH_EVENTS], [])
        self.assertEqual([event for event in peer.events if event[0] in {"channel", "pty", "shell", "exec", "forward"}], [])

    def assert_login(self, peer, mode):
        key = not mode.endswith(("-password", "-interactive"))
        self.assertEqual([event[1] for event in peer.events if event[0] == "authentication"], [0, 1] if key else [])
        self.assertEqual(len([event for event in peer.events if event[0] == "password-authentication"]),
                         1 if mode.endswith("-password") else 0)
        responses = [event for event in peer.events if event[0] == "interactive-response"]
        self.assertEqual(responses, [("interactive-response", 1, 1, True)]
                         if mode.endswith(("-interactive", "-multifactor")) else [])
        self.assertEqual([event for event in peer.events if event[0] in {"channel", "pty", "shell", "exec"}], [])

    def test_managed_policy_precedes_all_authentication_and_preserves_each_hop_and_probe(self):
        modes = ["allow-publickey", "allow-password", "allow-interactive", "allow-multifactor",
                 "deny-publickey", "deny-password", "deny-interactive", "deny-multifactor", "asymmetric",
                 "jump-allowed", "jump-denied", "jump-destination-denied", "probe-allowed", "probe-denied",
                 "empty", "unsupported"]
        completed = 0
        for mode in modes:
            with self.subTest(mode=mode):
                offered = profile()
                if mode.startswith("deny-") or mode in {"probe-denied", "jump-destination-denied"}:
                    offered["kex"] = "ecdh-sha2-nistp256"
                expected = ["ecdh-sha2-nistp384", "aes128-ctr", "aes128-ctr", MACS[1], MACS[1]]
                if mode == "asymmetric":
                    offered.update(cipher_c_s=CIPHERS[0], cipher_s_c=CIPHERS[1], mac_c_s=MACS[0], mac_s_c=MACS[1])
                    expected = ["ecdh-sha2-nistp384", CIPHERS[0], CIPHERS[1], MACS[0], MACS[1]]
                challenge = [dict(name=b"Transport verification", instruction=b"Enter the one-time code.",
                    prompts=[(b"Transport code:", False)], responses=[b"654321"])]
                peer = fixture.LoopbackServer(CLIENT, transport_algorithms=offered,
                    password=b"synthetic transport password" if mode.endswith("-password") else None,
                    interactive_rounds=challenge if mode.endswith(("-interactive", "-multifactor")) else None,
                    auth_mode="publickeyInteractive" if mode.endswith("-multifactor") else "normal")
                hop = None
                try:
                    if mode.startswith("jump-"):
                        hop = fixture.LoopbackServer(CLIENT, forward_to=peer.address, forward_host="127.0.0.1",
                            transport_algorithms=profile(["ecdh-sha2-nistp521" if mode == "jump-denied"
                                                          else "ecdh-sha2-nistp256"], [CIPHERS[0]], [MACS[0]]))
                    self.invoke(mode, peer, expected, hop)
                    if mode in {"empty", "unsupported", "jump-denied"}:
                        self.assertEqual(peer.accepted_connections, 0)
                        self.assertEqual(peer.events, [])
                    else:
                        self.assertTrue(peer.done.wait(2), mode)
                        self.assertEqual(peer.accepted_connections, 1)
                        self.assertEqual(peer.errors, [])
                        if mode.startswith(("deny-", "probe-")) or mode == "jump-destination-denied":
                            self.assert_no_authentication(peer)
                        else:
                            self.assert_login(peer, mode)
                    if hop is not None:
                        self.assertTrue(hop.done.wait(2), mode)
                        self.assertEqual(hop.accepted_connections, 1)
                        self.assertEqual(hop.errors, [])
                        if mode == "jump-denied":
                            self.assert_no_authentication(hop)
                        else:
                            self.assert_login(hop, "allow-publickey")
                            self.assertEqual([e for e in hop.events if e[0] == "forward"],
                                             [("forward", b"127.0.0.1", peer.address[1])])
                    completed += 1
                    print("Qualified transport policy case: " + mode, flush=True)
                finally:
                    if hop is not None:
                        hop.close()
                    peer.close()
        self.assertEqual(completed, len(modes))
        print("Qualified managed transport policy scenarios: " + str(completed), flush=True)


if __name__ == "__main__":
    suite = unittest.TestSuite()
    if not WORKSPACE_ONLY:
        suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(NativeTransportAlgorithmPolicyTests))
    if not NATIVE_ONLY:
        suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(WorkspaceTransportAlgorithmInteropTests))
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(0 if result.wasSuccessful() else 1)
