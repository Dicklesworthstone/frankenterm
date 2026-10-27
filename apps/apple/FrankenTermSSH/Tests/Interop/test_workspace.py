"""Imported workspace -> real production signer -> libssh2 -> independent libssh.

Run with a built native driver and XCTest bundle, without rebuilding the package:
python Tests/Interop/test_workspace.py /path/to/libftssh.so /path/to/FrankenTermSSHPackageTests.xctest
Append /path/to/WorkspaceSSHProbe to also exercise every supported key family,
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
from cryptography.hazmat.primitives.asymmetric import ed25519, ec, rsa

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
                     "owner-success", "owner-revoked", "owner-marker-failure", "owner-close-before-run", "owner-connect-only"]:
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

    def run_host_mode(self, mode, host_key=None, host_signature=None):
        key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
        server = fixture.LoopbackServer(key, host_key=host_key, host_signature=host_signature)
        try:
            environment = dict(os.environ, FT_WORKSPACE_SSH_TEST_PORT=str(server.address[1]),
                               FT_WORKSPACE_SSH_TEST_HOSTKEY=base64.b64encode(server.host_blob).decode(),
                               FT_WORKSPACE_SSH_TEST_MODE=mode)
            if host_signature is None:
                environment.pop("FT_WORKSPACE_SSH_TEST_HOST_SIGNATURE", None)
            else:
                environment["FT_WORKSPACE_SSH_TEST_HOST_SIGNATURE"] = host_signature
            result = subprocess.run(xctest_command(), env=environment, capture_output=True, text=True, timeout=20)
            output = result.stdout + result.stderr
            self.assertEqual(result.returncode, 0, output[-16000:])
            self.assertIn("Executed 1 test", output)
            self.assertNotIn("skipped", output.lower())
            authentication = [event for event in server.events if event[0] == "authentication"]
            side_effects = [event[0] for event in server.events if event[0] in ["channel", "pty", "shell"]]
            expected_authentication = [0, 1] if mode in ["rsa-success", "mux-owner-auth"] else []
            self.assertEqual([event[1] for event in authentication], expected_authentication)
            self.assertEqual([event[3] for event in authentication],
                             [fixture.public_blob(key)] * len(expected_authentication))
            self.assertEqual(side_effects, ["channel", "pty", "shell"] if mode == "rsa-success" else [])
            self.assertEqual(server.received,
                             "Imported key → real SSH.\rEND-SESSION\r".encode() if mode == "rsa-success" else b"")
            self.assertEqual(server.accepted_connections, 0 if mode == "mux-revoked" else 1)
        finally:
            server.close()

    def test_rsa_hosts_use_each_sha2_signature_and_open_a_real_terminal(self):
        for bits, algorithm in [(2048, "rsa-sha2-256"), (4096, "rsa-sha2-512")]:
            with self.subTest(bits=bits, algorithm=algorithm):
                host = rsa.generate_private_key(public_exponent=65537, key_size=bits)
                self.run_host_mode("rsa-success", host, algorithm)

    def test_wrong_or_revoked_rsa_host_pin_prevents_every_key_offer(self):
        host = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        for mode in ["rsa-wrong-host", "rsa-revoked"]:
            with self.subTest(mode=mode):
                self.run_host_mode(mode, host, "rsa-sha2-512")

    def test_rsa_sha1_only_server_cannot_trigger_authentication_or_fallback(self):
        host = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        self.run_host_mode("rsa-sha1-only", host, "ssh-rsa")

    def test_rsa_route_diagnosis_verifies_sha2_host_without_offering_a_key(self):
        host = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        for algorithm in ["rsa-sha2-256", "rsa-sha2-512"]:
            with self.subTest(algorithm=algorithm):
                self.run_host_mode("rsa-probe", host, algorithm)

    def test_explicit_mux_route_authenticates_without_opening_a_terminal(self):
        self.run_host_mode("mux-owner-auth")

    def test_revoked_explicit_mux_route_is_denied_before_networking(self):
        self.run_host_mode("mux-revoked")

    def test_one_hop_workspace_uses_independent_keys_and_only_destination_pty(self):
        for mode in ["jump-success", "jump-auth-only", "jump-wrong-hop", "jump-wrong-destination",
                     "jump-revoked", "jump-revoke-hop-after-auth", "jump-probe", "jump-cancel-signature",
                     "jump-revoke-during-signature", "jump-hop-key-fallback", "jump-hop-refused"]:
            with self.subTest(mode=mode):
                destination_key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
                hop_key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32, 64)))
                destination = fixture.LoopbackServer(destination_key)
                repeated_hop = mode in ["jump-hop-key-fallback", "jump-hop-refused"]
                hop = fixture.LoopbackServer(hop_key, forward_to=destination.address,
                    connections=2 if repeated_hop else 1, reject=mode == "jump-hop-refused")
                try:
                    environment = dict(os.environ, FT_WORKSPACE_SSH_TEST_PORT=str(destination.address[1]),
                        FT_WORKSPACE_SSH_TEST_HOSTKEY=base64.b64encode(destination.host_blob).decode(),
                        FT_WORKSPACE_SSH_JUMP_PORT=str(hop.address[1]),
                        FT_WORKSPACE_SSH_JUMP_HOSTKEY=base64.b64encode(hop.host_blob).decode(),
                        FT_WORKSPACE_SSH_TEST_MODE=mode)
                    result = subprocess.run(xctest_command(), env=environment, capture_output=True, text=True, timeout=20)
                    output = result.stdout + result.stderr
                    self.assertEqual(result.returncode, 0, output[-18000:])
                    self.assertIn("Executed 1 test", output)
                    self.assertNotIn("skipped", output.lower())
                    no_hop = mode in ["jump-revoked", "jump-probe"]
                    no_target = no_hop or mode in ["jump-wrong-hop", "jump-hop-refused"]
                    self.assertEqual(hop.accepted_connections, 0 if no_hop else (2 if repeated_hop else 1))
                    self.assertEqual(destination.accepted_connections, 0 if no_target else 1)
                    hop_offers = [e for e in hop.events if e[0] == "authentication"]
                    target_offers = [e for e in destination.events if e[0] == "authentication"]
                    expected_hop = [0, 0, 1] if mode == "jump-hop-key-fallback" else (
                        [0, 0] if mode == "jump-hop-refused" else ([] if no_target else [0, 1]))
                    self.assertEqual([e[1] for e in hop_offers], expected_hop)
                    expected_target = [] if no_target or mode == "jump-wrong-destination" else (
                        [0] if mode in ["jump-cancel-signature", "jump-revoke-during-signature"] else [0, 1])
                    self.assertEqual([e[1] for e in target_offers], expected_target)
                    if repeated_hop:
                        self.assertEqual(hop_offers[0][3], fixture.public_blob(destination_key))
                        self.assertTrue(all(e[3] == fixture.public_blob(hop_key) for e in hop_offers[1:]))
                    else:
                        self.assertTrue(all(e[3] == fixture.public_blob(hop_key) for e in hop_offers))
                    self.assertTrue(all(e[3] == fixture.public_blob(destination_key) for e in target_offers))
                    self.assertEqual([e for e in hop.events if e[0] in ["channel", "pty", "shell"]], [])
                    target_shell = any(e[0] == "shell" for e in destination.events)
                    self.assertEqual(target_shell, mode == "jump-success")
                    self.assertEqual(destination.received,
                        "Imported key → owned terminal.\rEND-SESSION\r".encode() if mode == "jump-success" else b"")
                finally:
                    hop.close()
                    destination.close()

    def test_two_and_three_hop_workspaces_preserve_every_peer_authorization(self):
        cases = [(2, "jump-chain-success"), (2, "jump-chain-wrong-hop")]
        cases += [(3, mode) for mode in [
            "jump-chain-success", "jump-chain-auth-only", "jump-chain-wrong-hop", "jump-chain-wrong-destination",
            "jump-chain-revoked", "jump-chain-revoke-first-hop", "jump-chain-revoke-middle-hop", "jump-chain-revoke-last-hop",
            "jump-chain-cancel-signature", "jump-chain-revoke-during-signature", "jump-chain-hop-key-fallback", "jump-chain-hop-refused"]]
        for hop_count, mode in cases:
            with self.subTest(hops=hop_count, mode=mode):
                destination_key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
                keys = [ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32 * (index + 1), 32 * (index + 2))))
                        for index in range(hop_count)]
                destination = fixture.LoopbackServer(destination_key)
                repeats = mode in ["jump-chain-hop-key-fallback", "jump-chain-hop-refused"]
                peers = []
                for index in reversed(range(hop_count)):
                    target = peers[0] if peers else destination
                    peers.insert(0, fixture.LoopbackServer(keys[index], forward_to=target.address,
                        connections=2 if repeats else 1,
                        reject=mode == "jump-chain-hop-refused" and index == hop_count - 1))
                try:
                    chain = [{"port": peer.address[1], "hostKey": base64.b64encode(peer.host_blob).decode()} for peer in peers]
                    environment = dict(os.environ, FT_WORKSPACE_SSH_TEST_PORT=str(destination.address[1]),
                        FT_WORKSPACE_SSH_TEST_HOSTKEY=base64.b64encode(destination.host_blob).decode(),
                        FT_WORKSPACE_SSH_JUMP_CHAIN=json.dumps(chain), FT_WORKSPACE_SSH_TEST_MODE=mode)
                    result = subprocess.run(xctest_command(), env=environment, capture_output=True, text=True, timeout=25)
                    output = result.stdout + result.stderr
                    self.assertEqual(result.returncode, 0, output[-18000:])
                    self.assertIn("Executed 1 test", output)
                    self.assertNotIn("skipped", output.lower())
                    no_hops = mode == "jump-chain-revoked"
                    no_target = no_hops or mode in ["jump-chain-wrong-hop", "jump-chain-hop-refused"]
                    for index, peer in enumerate(peers):
                        self.assertEqual(peer.accepted_connections, 0 if no_hops else (2 if repeats else 1))
                        offers = [e for e in peer.events if e[0] == "authentication"]
                        if no_hops or (mode == "jump-chain-wrong-hop" and index == hop_count - 1):
                            expected = []
                        elif repeats and index == hop_count - 1:
                            expected = [0, 0, 1] if mode == "jump-chain-hop-key-fallback" else [0, 0]
                        else:
                            expected = [0, 1, 0, 1] if repeats else [0, 1]
                        self.assertEqual([e[1] for e in offers], expected)
                        if repeats and index == hop_count - 1:
                            self.assertEqual(offers[0][3], fixture.public_blob(destination_key))
                            self.assertTrue(all(e[3] == fixture.public_blob(keys[index]) for e in offers[1:]))
                        else:
                            self.assertTrue(all(e[3] == fixture.public_blob(keys[index]) for e in offers))
                        self.assertEqual([e for e in peer.events if e[0] in ["channel", "pty", "shell", "resize"]], [])
                        if peer.accepted_connections:
                            self.assertTrue(peer.done.wait(2), "retired destination retained a hop's live server work")
                    self.assertEqual(destination.accepted_connections, 0 if no_target else 1)
                    offers = [e for e in destination.events if e[0] == "authentication"]
                    expected = [] if no_target or mode == "jump-chain-wrong-destination" else (
                        [0] if mode in ["jump-chain-cancel-signature", "jump-chain-revoke-during-signature"] else [0, 1])
                    self.assertEqual([e[1] for e in offers], expected)
                    self.assertTrue(all(e[3] == fixture.public_blob(destination_key) for e in offers))
                    self.assertEqual(any(e[0] == "shell" for e in destination.events), mode == "jump-chain-success")
                    self.assertEqual(destination.received,
                        "Imported key → owned terminal.\rEND-SESSION\r".encode() if mode == "jump-chain-success" else b"")
                    if mode == "jump-chain-success":
                        self.assertIn(("resize", 132, 42), destination.events)
                finally:
                    for peer in peers:
                        peer.close()
                    destination.close()


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


@unittest.skipUnless(PROBE, "provide WorkspaceSSHProbe for ordered credential scenarios")
class OrderedWorkspaceInteropTests(unittest.TestCase):
    def run_ordered(self, mode):
        refused = ed25519.Ed25519PrivateKey.generate()
        accepted = ec.generate_private_key(ec.SECP256R1())
        server = fixture.LoopbackServer(accepted, connections=2)
        directory = Path(tempfile.mkdtemp(prefix="frankenterm-ordered-ssh-journal-"))
        server.events = ObservedServerEvents(directory / "catalog.json")
        def pem(key):
            return key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.OpenSSH,
                                     serialization.NoEncryption()).decode()
        request = {"pem": pem(refused), "secondPEM": pem(accepted), "journalDirectory": str(directory)}
        try:
            process = subprocess.run([PROBE, str(server.address[1]), base64.b64encode(server.host_blob).decode(), mode],
                                     input=json.dumps(request).encode(), capture_output=True, timeout=20)
            self.assertEqual(process.returncode, 0, process.stderr.decode(errors="replace")[:12000])
            self.assertLess(len(process.stdout), 2048)
            result = json.loads(process.stdout)
            self.assertTrue(result["closed"])
            authentication = [event for event in server.events if event[0] == "authentication"]
            side_effects = [event[0] for event in server.events if event[0] in ["channel", "pty", "shell"]]
            if mode == "orderedSuccess":
                self.assertTrue(server.done.wait(2), "second connection retained server work")
                self.assertEqual(server.accepted_connections, 2)
                self.assertEqual([event[1] for event in authentication], [0, 0, 1])
                self.assertEqual([event[3] for event in authentication],
                                 [fixture.public_blob(refused), fixture.public_blob(accepted), fixture.public_blob(accepted)])
                self.assertEqual(side_effects, ["channel", "pty", "shell"])
                self.assertEqual(server.events.creation_observations, 3)
                self.assertEqual(server.received, "protected workspace → actual SSH 🦀\r\nEND-SESSION\r".encode())
                self.assertEqual(result["bytes"], len(server.received))
                self.assertEqual(result["signs"], 1, "the rejected first key must never sign")
                records = json.loads((directory / "catalog.json").read_bytes())["records"]
                self.assertEqual([record["state"] for record in records], ["acknowledged"])
                # The first connection must have closed before this immutable
                # listener accepted the second key's fresh transport.
                positions = [index for index, event in enumerate(server.events) if event[0] == "connection"]
                self.assertEqual(len(positions), 2)
                self.assertLess(next(index for index, event in enumerate(server.events) if event[0] == "authentication"),
                                positions[1])
            else:
                self.assertEqual(server.accepted_connections, 1)
                self.assertEqual([event[1] for event in authentication], [0])
                self.assertEqual([event[3] for event in authentication], [fixture.public_blob(refused)])
                self.assertEqual(side_effects, [])
                self.assertEqual(server.events.creation_observations, 0)
                self.assertEqual(server.received, b"")
                self.assertEqual(result["bytes"], 0)
                self.assertEqual(result["signs"], 0)
                if (directory / "catalog.json").exists():
                    self.assertEqual(json.loads((directory / "catalog.json").read_bytes())["records"], [])
        finally:
            server.close()

    def test_refused_first_key_advances_to_second_on_the_same_endpoint_and_opens_one_real_terminal(self):
        self.run_ordered("orderedSuccess")

    def test_cancelled_native_owner_cannot_offer_the_next_key(self):
        self.run_ordered("orderedCancel")

    def test_revoking_the_next_key_between_attempts_prevents_its_offer(self):
        self.run_ordered("orderedRevokeNext")

    def test_reimport_between_attempts_rejects_the_old_workspace_revision(self):
        self.run_ordered("orderedStaleRevision")

    def test_suspending_connections_between_attempts_prevents_the_next_offer(self):
        self.run_ordered("orderedSuspend")


@unittest.skipUnless(PROBE, "provide WorkspaceSSHProbe for protected-worker and journal scenarios")
class ProtectedWorkspaceInteropTests(unittest.TestCase):
    def run_probe(self, mode, key=None, user_signature=None):
        key = key or ed25519.Ed25519PrivateKey.generate()
        server = fixture.LoopbackServer(key, user_signature=user_signature)
        host = fixture.public_blob(ec.generate_private_key(ec.SECP256R1())) if mode == "wrongPin" else server.host_blob
        pem = key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.OpenSSH, serialization.NoEncryption())
        request = {"pem": pem.decode()}
        if mode in ["managed", "managedClose", "managedFinalWriteFailure", "managedBindingClose", "managedRevokeFinalWriteFailure"]:
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
            if mode in ["io", "foreignCancel", "managed", "managedFinalWriteFailure", "managedRevokeFinalWriteFailure"]:
                self.assertEqual(offered, [0, 1], "libssh must validate the production signature")
                self.assertEqual(side_effects, ["channel", "pty", "shell"])
                self.assertEqual(server.received, "protected workspace → actual SSH 🦀\r\nEND-SESSION\r".encode())
                self.assertEqual(result["bytes"], len(server.received))
                self.assertEqual(result["signs"], 1)
                if mode in ["managed", "managedFinalWriteFailure", "managedRevokeFinalWriteFailure"]:
                    self.assertEqual(server.events.creation_observations, 3)
                    self.assertIn(("resize", 132, 42), server.events)
                    self.assertNotIn(("resize", 160, 50), server.events)
                    journal = (directory / "catalog.json").read_bytes()
                    records = json.loads(journal)["records"]
                    self.assertEqual([record["state"] for record in records], ["acknowledged"])
                    self.assertEqual(records[0]["input_evidence"] & 3, 3)
                    for forbidden in [b"localhost", b"operator", b"PRIVATE KEY", b"END-SESSION", public]:
                        self.assertNotIn(forbidden, journal)
            elif mode == "managedBindingClose":
                self.assertEqual(offered, [0, 1])
                self.assertEqual(side_effects, ["channel", "pty", "shell"])
                self.assertEqual(server.events.creation_observations, 3)
                self.assertEqual(result["signs"], 1)
                records = json.loads((directory / "catalog.json").read_bytes())["records"]
                self.assertEqual([record["state"] for record in records], ["interrupted"])
                self.assertEqual(records[0]["input_evidence"], 0)
            elif mode in ["beforeShellRevocation", "managedClose", "managedRevokeAfterReimport"]:
                self.assertEqual(offered, [0, 1])
                self.assertEqual(side_effects, [], "a revoked or retired connection must not create a channel or PTY")
                self.assertEqual(result["signs"], 1)
                if mode == "managedClose":
                    self.assertEqual(server.events.creation_observations, 0)
                    records = json.loads((directory / "catalog.json").read_bytes())["records"]
                    self.assertEqual([record["state"] for record in records], ["interrupted"])
            elif mode in ["revokeKeyDuring", "revokeHostDuring", "reimportDuring", "cancelDuring", "suspendDuring",
                          "managedRevokeDuring", "managedRevokeDuringReimport"]:
                self.assertEqual(offered, [0], "a stale or cancelled signature must never reach SSH")
                self.assertEqual(side_effects, [])
                self.assertEqual(result["signs"], 1)
                if mode in ["cancelDuring", "suspendDuring", "managedRevokeDuring", "managedRevokeDuringReimport"]:
                    self.assertGreater(result["cancellations"], 0)
            else:
                self.assertEqual(offered, [], "untrusted or stale identity must not even be offered")
                self.assertEqual(side_effects, [])
                self.assertEqual(result["signs"], 0)
            if mode not in ["io", "foreignCancel", "managed", "managedFinalWriteFailure", "managedRevokeFinalWriteFailure"]:
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

    def test_imported_rsa_credentials_use_each_sha2_digest_with_the_production_signer(self):
        for bits in [2048, 4096, 8192]:
            for algorithm in ["rsa-sha2-256", "rsa-sha2-512"]:
                with self.subTest(bits=bits, algorithm=algorithm):
                    self.run_probe("io", fixture.rsa_client_key(bits), user_signature=algorithm)

    def test_rsa_protected_worker_cannot_publish_a_signature_after_policy_or_owner_changes(self):
        key = fixture.rsa_client_key(2048)
        for mode in ["wrongPin", "revokeKeyBefore", "revokeKeyDuring", "reimportDuring", "cancelDuring"]:
            with self.subTest(mode=mode):
                self.run_probe(mode, key, user_signature="rsa-sha2-512")

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

    def test_closed_source_retries_failed_journal_publication_without_retaining_input_or_replaying(self):
        self.run_probe("managedFinalWriteFailure")

    def test_managed_revocation_persists_before_cancelling_protected_worker_and_survives_caller_cancellation(self):
        for mode in ["managedRevokeDuring", "managedRevokeDuringReimport"]:
            with self.subTest(mode=mode):
                self.run_probe(mode)

    def test_managed_revocation_finds_live_identity_removed_by_reimport(self):
        self.run_probe("managedRevokeAfterReimport")

    def test_managed_revocation_retains_closed_owner_through_input_and_failed_journal_settlement(self):
        self.run_probe("managedRevokeFinalWriteFailure")

    def test_suspension_joins_parked_creation_without_reviving_old_grants_or_overriding_resume(self):
        self.run_probe("managedClose")

    def test_concurrent_close_retires_unpublished_source_once_after_binding_settles(self):
        self.run_probe("managedBindingClose")


if __name__ == "__main__":
    unittest.main(verbosity=2)
