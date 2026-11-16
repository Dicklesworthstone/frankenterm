"""Production coordinator/pump/native adapter over independent loopback SSH.
Run with libftssh.so and the separately compiled PumpProbe executable.
Synthetic keys and signing IPC belong to this test, never an Apple product.
"""
import base64
import importlib.util
import json
from pathlib import Path
import selectors
import subprocess
import sys
import time
import unittest
from cryptography.hazmat.primitives.asymmetric import ed25519, ec

if len(sys.argv) < 3:
    raise SystemExit("usage: test_pump.py /path/to/libftssh.so /path/to/PumpProbe")
probe = sys.argv.pop(2)
spec = importlib.util.spec_from_file_location("native_fixture", Path(__file__).parents[1] / "Native" / "test_native.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

class PumpInteropTests(unittest.TestCase):
    def exercise(self, mode, key=None):
        key = key or ed25519.Ed25519PrivateKey.generate()
        server = fixture.LoopbackServer(key)
        process = subprocess.Popen([probe, str(server.address[1]), base64.b64encode(server.host_blob).decode(),
                                    base64.b64encode(fixture.public_blob(key)).decode(), mode],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        buffer, done, signatures = b"", None, 0
        deadline = time.monotonic() + 20
        try:
            while time.monotonic() < deadline:
                events = selector.select(max(0, deadline - time.monotonic()))
                if not events:
                    self.fail("production pump exceeded test deadline")
                data = process.stdout.read(4096)
                if not data:
                    break
                buffer += data
                self.assertLessEqual(len(buffer), 4096)
                while b"\n" in buffer:
                    line, buffer = buffer.split(b"\n", 1)
                    event = json.loads(line)
                    if event["event"] == "sign":
                        challenge = base64.b64decode(event["preimage"], validate=True)
                        sid, rest = fixture.take_string(challenge)
                        self.assertIn(len(sid), (32, 48, 64))
                        algorithm, _ = fixture.take_string(fixture.public_blob(key))
                        self.assertEqual(rest, b"\x32" + fixture.ssh_string(b"operator") + fixture.ssh_string(b"ssh-connection")
                                         + fixture.ssh_string(b"publickey") + b"\x01" + fixture.ssh_string(algorithm)
                                         + fixture.ssh_string(fixture.public_blob(key)))
                        process.stdin.write(base64.b64encode(fixture.signature(key, challenge)) + b"\n")
                        process.stdin.flush()
                        signatures += 1
                    elif event["event"] == "error":
                        self.fail(event)
                    elif event["event"] == "done":
                        done = event
                if done:
                    break
            self.assertIsNotNone(done)
            self.assertEqual(process.wait(timeout=3), 0, process.stderr.read().decode())
            self.assertEqual(signatures, 1)
            self.assertTrue(server.done.wait(3))
            self.assertIn(("pty", b"xterm-256color", 80, 24), server.events)
            self.assertEqual(len([e for e in server.events if e[0] == "shell"]), 1)
            if mode in ("cancelIdle", "usedChannel", "pendingWrite"):
                self.assertEqual(server.received, b"")
            elif mode == "outcomeFailure":
                self.assertTrue(done["retainedOutcome"])
                # Local acceptance is not remote delivery. No equality claim here.
            else:
                expected = b"\x1b[0n" if mode == "reply" else b""
                if mode == "paste":
                    expected += b"\x1b[200~" + ("hello 界🦀\n" * 5000).encode() + b"\x1b[201~"
                else:
                    expected += ("native é界🦀\r\n" + "abcd" * (24000 if mode == "large" else 200)).encode()
                expected += b"END-SESSION\r"
                self.assertEqual(bytes(server.received), expected)
                self.assertEqual(done["bytes"], len(expected))
                self.assertIn(("resize", 132, 42), server.events)
        finally:
            selector.close()
            if process.poll() is None:
                process.kill()
                process.wait(timeout=3)
            for stream in [process.stdin, process.stdout, process.stderr]:
                stream.close()
            server.close()

    def test_live_pump_four_signing_families(self):
        for key in [ed25519.Ed25519PrivateKey.generate()] + [ec.generate_private_key(c) for c in [ec.SECP256R1(), ec.SECP384R1(), ec.SECP521R1()]]:
            with self.subTest(key=type(key).__name__, curve=getattr(key, "curve", None)):
                self.exercise("io", key)

    def test_large_hidden_pane_stream(self): self.exercise("large")
    def test_bracketed_unicode_paste(self): self.exercise("paste")
    def test_hidden_source_answerback(self): self.exercise("reply")
    def test_cancel_idle_wait_and_release_native_handle(self): self.exercise("cancelIdle")
    def test_failed_outcome_delivery_is_recoverable(self): self.exercise("outcomeFailure")
    def test_consumed_channel_cannot_be_relabelled(self): self.exercise("usedChannel")
    def test_pending_unbound_input_cannot_be_relabelled(self): self.exercise("pendingWrite")

if __name__ == "__main__":
    unittest.main(verbosity=2)
