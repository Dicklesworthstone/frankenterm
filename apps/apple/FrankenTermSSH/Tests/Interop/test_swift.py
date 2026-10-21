"""Production Swift actor + production C + independent libssh loopback server.

Run after building the package and SessionProbe (README instructions):
python Tests/Interop/test_swift.py /path/to/libftssh.so /path/to/SessionProbe
All private keys and host policy belong exclusively to this synthetic test.
"""
import base64
import importlib.util
import json
from pathlib import Path
import selectors
import socket
import subprocess
import sys
import time
import unittest
from cryptography.hazmat.primitives.asymmetric import ed25519, ec

NATIVE_LIBRARY, PROBE = sys.argv[1:3]
sys.argv = [sys.argv[0], NATIVE_LIBRARY]
spec = importlib.util.spec_from_file_location("native_fixture", Path(__file__).resolve().parents[1] / "Native" / "test_native.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

class SwiftInteropTests(unittest.TestCase):
    def run_probe(self, mode, key=None):
        key = key or ed25519.Ed25519PrivateKey.generate()
        completion_modes = {"completionStatus7": "status7", "completionHighStatus": "highStatus",
                            "completionSignal": "signalTERM", "completionUnknown": "unknown",
                            "completionZero": "status0", "completionEOFOnly": "eofOnly",
                            "completionStatusWithoutClose": "statusWithoutClose",
                            "completionDelayedStatus": "delayedStatus", "completionReset": "resetAfterEOF",
                            "completionCancel": "eofOnly"}
        server = fixture.LoopbackServer(key, completion=completion_modes.get(mode, "unknown"))
        sock = None
        if mode == "connect":
            endpoint, inherited = server.address[1], []
        else:
            sock = socket.socket()
            sock.connect(server.address)
            endpoint, inherited = sock.fileno(), [sock.fileno()]
        process = subprocess.Popen([PROBE, str(endpoint), base64.b64encode(server.host_blob).decode(),
                                    base64.b64encode(fixture.public_blob(key)).decode(), mode],
                                   pass_fds=inherited, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if sock:
            sock.close()
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        completed, signs = None, 0
        try:
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                if not selector.select(0.2):
                    if process.poll() is not None:
                        break
                    continue
                line = process.stdout.readline(4096)
                if not line:
                    break
                self.assertLess(len(line), 4096)
                event = json.loads(line)
                if event["event"] == "sign":
                    signs += 1
                    self.assertEqual(signs, 1)
                    preimage = base64.b64decode(event["preimage"], validate=True)
                    session, tail = fixture.take_string(preimage)
                    self.assertIn(len(session), [32, 48, 64])
                    blob = fixture.public_blob(key)
                    algorithm, _ = fixture.take_string(blob)
                    expected = fixture.ssh_string(session) + bytes([50]) + fixture.ssh_string(b"operator")
                    expected += fixture.ssh_string(b"ssh-connection") + fixture.ssh_string(b"publickey") + b"\x01"
                    expected += fixture.ssh_string(algorithm) + fixture.ssh_string(blob)
                    self.assertEqual(preimage, expected)
                    if mode == "deadlineSigner":
                        time.sleep(1.2)
                    process.stdin.write(base64.b64encode(fixture.signature(key, preimage)) + b"\n")
                    process.stdin.flush()
                elif event["event"] == "done":
                    completed = event
                    break
                elif event["event"] == "waitingForCompletion":
                    self.assertIn(mode, ["completionDelayedStatus", "completionReset"])
                    self.assertTrue(server.output_eof.is_set())
                    server.release_completion.set()
                else:
                    self.fail(event)
            self.assertIsNotNone(completed, "Swift integration did not finish before its outer deadline")
            self.assertEqual(process.wait(timeout=3), 0, process.stderr.read(8192).decode(errors="replace"))
            if mode in completion_modes:
                self.assertEqual(len(server.received), completed["bytes"])
                self.assertEqual([e[1] for e in server.events if e[0] == "authentication"], [0, 1])
                self.assertEqual([e[0] for e in server.events if e[0] in ["channel", "pty", "shell"]],
                                 ["channel", "pty", "shell"])
                if mode in ["completionReset", "completionCancel"]:
                    self.assertEqual(completed["origin"], "transportLost" if mode == "completionReset" else "cancelled")
                else:
                    self.assertEqual(completed["origin"], "remoteCompletion")
                    self.assertEqual(completed["channelClosed"], mode not in ["completionEOFOnly", "completionStatusWithoutClose"])
                    if mode in ["completionStatus7", "completionDelayedStatus", "completionStatusWithoutClose", "completionHighStatus"]:
                        self.assertEqual(completed["exitKind"], "status")
                        self.assertEqual(completed["exitStatus"], 0xffffffff if mode == "completionHighStatus" else 7)
                    elif mode == "completionSignal":
                        self.assertEqual((completed["exitKind"], completed["signal"]), ("signal", "TERM"))
                    else:
                        self.assertEqual(completed["exitKind"], "unknown")
            elif mode in ["io", "idle", "connect", "wake", "pump", "pumpCancel"]:
                self.assertEqual(len(server.received), completed["bytes"])
                if mode != "pumpCancel":
                    self.assertIn(("resize", 132, 42), server.events)
                self.assertEqual([e[1] for e in server.events if e[0] == "authentication"], [0, 1])
                self.assertTrue(any(e[0] == "shell" for e in server.events))
            else:
                self.assertFalse(any(e[0] == "shell" for e in server.events))
                self.assertEqual(signs, 0 if mode in ["rejectHost", "cancelPeer"] else 1)
                if mode in ["cancelSigner", "deadlineSigner", "revokeBeforeSign"]:
                    self.assertEqual([e[1] for e in server.events if e[0] == "authentication"], [0])
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=3)
            for stream in [process.stdin, process.stdout, process.stderr]:
                stream.close()
            selector.close()
            server.close()

    def test_swift_session_really_authenticates_and_pumps_all_four_families(self):
        for key in [ed25519.Ed25519PrivateKey.generate()] + [ec.generate_private_key(c) for c in [ec.SECP256R1(), ec.SECP384R1(), ec.SECP521R1()]]:
            with self.subTest(curve=getattr(key, "curve", None)):
                self.run_probe("io", key)

    def test_rejected_host_and_delayed_peer_approval_cannot_authenticate(self):
        for mode in ["rejectHost", "cancelPeer"]:
            with self.subTest(mode=mode):
                self.run_probe(mode)

    def test_idle_readiness_timeout_preserves_session_for_later_input(self):
        self.run_probe("idle")

    def test_hostname_connector_drives_real_authentication_and_pty_io(self):
        self.run_probe("connect")

    def test_native_service_wake_is_latched_before_and_during_idle_wait(self):
        self.run_probe("wake")

    def test_terminal_coordinator_pump_preserves_large_lease_fifo_resize_and_eof(self):
        for key in [ed25519.Ed25519PrivateKey.generate()] + [ec.generate_private_key(c) for c in [ec.SECP256R1(), ec.SECP384R1(), ec.SECP521R1()]]:
            with self.subTest(curve=getattr(key, "curve", None)):
                self.run_probe("pump", key)

    def test_terminal_pump_stop_interrupts_a_quiet_native_reader_and_settles_close(self):
        self.run_probe("pumpCancel")

    def test_cancelled_expired_or_revoked_signature_cannot_reach_peer(self):
        for mode in ["cancelSigner", "deadlineSigner", "revokeBeforeSign"]:
            with self.subTest(mode=mode):
                self.run_probe(mode)

    def test_final_validation_and_pre_shell_revocation_prevent_side_effect(self):
        for mode in ["cancelFinalValidation", "revokeBeforeShell"]:
            with self.subTest(mode=mode):
                self.run_probe(mode)

    def test_typed_completion_distinguishes_reported_evidence_and_unknown_zero(self):
        for mode in ["completionStatus7", "completionHighStatus", "completionSignal", "completionUnknown", "completionZero"]:
            with self.subTest(mode=mode):
                self.run_probe(mode)

    def test_eof_is_half_close_and_waits_for_delayed_remote_status(self):
        self.run_probe("completionDelayedStatus")

    def test_eof_without_channel_close_has_bounded_completion_wait(self):
        for mode in ["completionEOFOnly", "completionStatusWithoutClose"]:
            with self.subTest(mode=mode):
                self.run_probe(mode)

    def test_loss_and_cancellation_during_completion_keep_their_local_origins(self):
        for mode in ["completionReset", "completionCancel"]:
            with self.subTest(mode=mode):
                self.run_probe(mode)

if __name__ == "__main__":
    unittest.main(verbosity=2)
