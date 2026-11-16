"""Real same-machine terminal ownership through the production Swift manager.

  python Tests/Interop/test_multiple_terminals.py libftssh.so NativeTests.xctest

One bounded loopback endpoint relays opaque TCP bytes to independent libssh
peers. Every peer uses the same generated host identity; each records its own
authentication, PTY, resize and exact input. No operating-system shell is run.
"""
import base64
import importlib.util
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import threading
import time
import unittest

LIBRARY, TEST_BUNDLE = sys.argv[1:3]
sys.argv = [sys.argv[0], LIBRARY]
spec = importlib.util.spec_from_file_location("multiple_terminal_peer_helpers",
    Path(__file__).resolve().parents[1] / "Native/test_native.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
sys.argv = [sys.argv[0]]
LIVE_TEST = "FrankenTermSSHTests.WorkspaceMultipleTerminalTests/testLiveSameMachineTerminalOwnersRemainIndependent"


class SameEndpointPeers:
    """Only transport forwarding is shared; SSH sessions stay independent."""
    def __init__(self, maximum):
        if not 1 <= maximum <= 4:
            raise ValueError("same-endpoint test peer limit must be 1...4")
        self.maximum = maximum
        self.client = fixture.ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
        self.host = fixture.ec.generate_private_key(fixture.ec.SECP256R1())
        self.host_blob = fixture.public_blob(self.host)
        self.peers, self.workers, self.controls, self.errors = [], [], [], []
        self.accepted = 0
        self.lock, self.stop = threading.Lock(), threading.Event()
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(4)
        self.listener.settimeout(0.1)
        self.address = self.listener.getsockname()
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    def accept(self):
        try:
            while not self.stop.is_set():
                try:
                    connection, _ = self.listener.accept()
                except socket.timeout:
                    continue
                except OSError:
                    if self.stop.is_set():
                        return
                    raise
                self.accepted += 1
                if self.accepted > self.maximum:
                    connection.close()
                    self.errors.append("An extra connection crossed the slot admission boundary")
                    continue
                peer = fixture.LoopbackServer(self.client, host_key=self.host)
                self.peers.append(peer)
                worker = threading.Thread(target=self.relay, args=(connection, peer.address), daemon=True)
                self.workers.append(worker)
                worker.start()
        except Exception as error:
            if not self.stop.is_set():
                self.errors.append(repr(error))

    def relay(self, incoming, address):
        outgoing = None
        try:
            outgoing = socket.create_connection(address, timeout=2)
            incoming.settimeout(2); outgoing.settimeout(2)
            with self.lock:
                self.controls += [incoming, outgoing]
            readable = [incoming, outgoing]
            total, deadline = 0, time.monotonic() + 25
            while readable and not self.stop.is_set():
                if time.monotonic() > deadline:
                    raise TimeoutError("Same-endpoint fixture exceeded its bounded lifetime")
                ready, _, _ = select.select(readable, [], [], 0.1)
                for source in ready:
                    target = outgoing if source is incoming else incoming
                    data = source.recv(16384)
                    if not data:
                        readable.remove(source)
                        try:
                            target.shutdown(socket.SHUT_WR)
                        except OSError:
                            pass
                        continue
                    total += len(data)
                    if total > 2 * 1024 * 1024:
                        raise RuntimeError("Same-endpoint fixture exceeded its byte bound")
                    target.sendall(data)
        except (ConnectionResetError, BrokenPipeError):
            pass  # Normal terminal close can discard a final encrypted reply.
        except Exception as error:
            if not self.stop.is_set():
                self.errors.append(repr(error))
        finally:
            incoming.close()
            if outgoing is not None:
                outgoing.close()

    def close(self):
        self.stop.set(); self.listener.close()
        self.thread.join(3)
        with self.lock:
            controls = list(self.controls)
        for control in controls:
            try:
                control.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        for peer in self.peers:
            peer.close()
        for worker in self.workers:
            worker.join(3)
        if self.thread.is_alive() or any(worker.is_alive() for worker in self.workers):
            self.errors.append("Owned same-endpoint fixture workers failed to stop")


class MultipleTerminalInteropTests(unittest.TestCase):
    def exercise(self, mode, count=2):
        server = SameEndpointPeers(count)
        try:
            environment = dict(os.environ, FT_MULTIPLE_TERMINALS_MODE=mode,
                FT_MULTIPLE_TERMINALS_PORT=str(server.address[1]),
                FT_MULTIPLE_TERMINALS_HOSTKEY=base64.b64encode(server.host_blob).decode(),
                PYTHONDONTWRITEBYTECODE="1")
            command = (["xcrun", "xctest", "-XCTest", LIVE_TEST, TEST_BUNDLE] if os.path.isdir(TEST_BUNDLE)
                       else [TEST_BUNDLE, LIVE_TEST])
            result = subprocess.run(command, env=environment, capture_output=True, text=True, timeout=30)
            output = result.stdout + result.stderr
            self.assertEqual(result.returncode, 0, output[-22000:])
            self.assertIn("Executed 1 test", output)
            self.assertNotIn("skipped", output.lower())
            self.assertEqual(server.accepted, count)
            self.assertEqual(len(server.peers), count)
            for peer in server.peers:
                self.assertTrue(peer.done.wait(2))
                self.assertEqual(peer.accepted_connections, 1)
                self.assertEqual(peer.errors, [])
                self.assertEqual(peer.host_blob, server.host_blob)
                signed = [event for event in peer.events if event[0] == "authentication"]
                self.assertEqual([event[1] for event in signed], [0, 1])
                self.assertTrue(all(event[2] == b"operator" and event[3] == fixture.public_blob(server.client)
                                    for event in signed))
                session = [event[0] for event in peer.events if event[0] in {"channel", "pty", "shell"}]
                self.assertEqual(session, [] if mode == "pending" else ["channel", "pty", "shell"])
                self.assertFalse(any(event[0] in {"exec", "forward", "subsystem"} for event in peer.events))
            left = "first terminal α\r".encode()
            right = "second terminal β\r".encode()
            if mode == "isolation":
                self.assertEqual(bytes(server.peers[0].received), left + b"\x1b[0n")
                self.assertEqual(bytes(server.peers[1].received), right + b"still-running-B\rEND-SESSION\r")
                self.assertIn(("resize", 111, 31), server.peers[0].events)
                self.assertIn(("resize", 99, 29), server.peers[1].events)
                self.assertNotIn(("resize", 99, 29), server.peers[0].events)
                self.assertNotIn(("resize", 111, 31), server.peers[1].events)
                self.assertTrue(all(("resize", 73, 21) not in peer.events for peer in server.peers))
            elif mode in {"suspend", "revoke-credential", "revoke-host"}:
                self.assertEqual(bytes(server.peers[0].received), left)
                self.assertEqual(bytes(server.peers[1].received), right)
            else:
                self.assertTrue(all(not peer.received for peer in server.peers))
            self.assertEqual(server.errors, [])
            print(json.dumps({"mode": mode, "connections": server.accepted,
                "shells": sum(sum(event[0] == "shell" for event in peer.events) for peer in server.peers),
                "bytes": [len(peer.received) for peer in server.peers]}), flush=True)
        finally:
            server.close()
        self.assertEqual(server.errors, [])

    def test_same_endpoint_input_resize_and_close_are_owned_by_each_terminal(self):
        self.exercise("isolation")

    def test_suspension_closes_every_terminal_slot(self):
        self.exercise("suspend")

    def test_credential_revocation_closes_every_terminal_slot(self):
        self.exercise("revoke-credential")

    def test_host_revocation_closes_every_terminal_slot(self):
        self.exercise("revoke-host")

    def test_duplicate_pending_slot_never_opens_another_connection(self):
        self.exercise("pending", 1)

    def test_fifth_terminal_is_refused_before_dialing(self):
        self.exercise("capacity", 4)


if __name__ == "__main__":
    unittest.main(verbosity=2)
