"""Real workspace SFTP interoperability with an independent OpenSSH server.

Run: python Tests/Interop/test_sftp.py <libftssh.so> <Swift XCTest product>
An optional third argument selects the native Python fixture source. The SSH
server is libssh; its dedicated SFTP subsystem bridges to the installed OpenSSH
sftp-server process. Only generated keys and loopback sockets are used. Test
directories are retained for inspection. No sshd service or configuration is used.
"""
import base64
import ctypes as C
import importlib.util
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest

from cryptography.hazmat.primitives.asymmetric import ed25519


NATIVE_LIBRARY, TEST_PRODUCT = sys.argv[1:3]
NATIVE_SOURCE = (Path(sys.argv[3]) if len(sys.argv) > 3 else
                 Path(__file__).resolve().parents[1] / "Native" / "test_native.py")
sys.argv = [sys.argv[0], NATIVE_LIBRARY]
spec = importlib.util.spec_from_file_location("sftp_native_fixture", NATIVE_SOURCE)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
fixture.bind(fixture.DRIVER, "ftssh_supports_sftp", fixture.I)

LIVE_TEST = "FrankenTermSSHTests.WorkspaceSFTPLiveTests/testLiveSFTPTransfersStayBoundToWorkspaceAuthority"
PROMPT_TEST = "FrankenTermSSHTests.WorkspaceAuthenticationPrompterTests/testLivePromptDelegatesRemainBoundToOneFileConnectionAttempt"
MODES = ("direct", "jump", "cancel-stall", "cancel-init", "cancel-upload", "timeout-stall",
         "ownership", "revoke", "stale", "scope-password", "scope-interactive", "scope-publickey-interactive")
DOWNLOAD = bytes((index * 37 + 11) & 255 for index in range(131113))
UPLOAD = bytes((index * 17 + 73) & 255 for index in range(98327))
EXISTING = b"This pre-existing remote file must remain byte-for-byte intact.\n"


def xctest_command(mode):
    selected = PROMPT_TEST if mode.startswith("scope-") else LIVE_TEST
    if Path(TEST_PRODUCT).is_dir():
        return ["xcrun", "xctest", "-XCTest", selected, TEST_PRODUCT]
    return [TEST_PRODUCT, selected]


class OpenSSHSFTPBridge:
    """Transport real SFTP packets; selected cases pause normal server traffic."""

    def __init__(self, root, mode):
        self.root, self.mode = root, mode
        self.requests, self.after_retirement = [], []
        self.write_ids = set()
        self.stalled = threading.Event()
        self.response_stalled = threading.Event()
        self.initialization_fragment_delivered = False

    def marker(self, name):
        with (self.root / name).open("xb") as stream:
            stream.write(b"observed\n")

    def request(self, packet):
        kind = packet[4]
        self.requests.append(kind)
        if len(self.requests) > 8192:
            raise AssertionError("SFTP fixture operation budget exceeded")
        if (self.root / "retired").exists():
            self.after_retirement.append(kind)
        if kind == 6:
            self.write_ids.add(struct.unpack_from(">I", packet, 5)[0])
        if self.mode in ("cancel-stall", "timeout-stall") and kind == 5:
            if not self.stalled.is_set():
                self.stalled.set()
                self.marker("read-seen")
            return b""
        return packet

    def response(self, packet):
        if self.response_stalled.is_set():
            return b""
        if self.mode == "cancel-init" and packet[4] == 2:
            # OpenSSH generated this normal VERSION reply. Deliver its real
            # length and first body byte, then hold the remainder until cancel.
            self.response_stalled.set()
            self.initialization_fragment_delivered = True
            return packet[:5]
        if self.mode == "cancel-upload" and packet[4] == 101 and len(packet) >= 13:
            request_id, status = struct.unpack_from(">II", packet, 5)
            if request_id in self.write_ids and status == 0:
                # A successful OpenSSH WRITE status proves the file was opened
                # and modified before cancellation. The client receives no ACK.
                self.response_stalled.set()
                self.marker("write-seen")
                return b""
        return packet

    @staticmethod
    def packets(pending):
        packets = []
        while len(pending) >= 4:
            size = struct.unpack_from(">I", pending)[0]
            if not 1 <= size <= 1024 * 1024:
                raise AssertionError("SFTP fixture packet bound exceeded")
            if len(pending) < size + 4:
                break
            packets.append(pending[:size + 4])
            pending = pending[size + 4:]
        return pending, packets

    def __call__(self, server, session, channel):
        executable = os.environ.get("FT_OPENSSH_SFTP_SERVER", "/usr/lib/openssh/sftp-server")
        if not Path(executable).is_file():
            raise AssertionError("Install OpenSSH sftp-server or set FT_OPENSSH_SFTP_SERVER")
        bridge, child = socket.socketpair()
        process = subprocess.Popen([executable, "-d", str(self.root)], stdin=child, stdout=child,
                                   stderr=subprocess.DEVNULL, close_fds=True)
        child.close()
        bridge.setblocking(False)
        fixture.SERVER.ssh_set_blocking(session, 0)
        incoming, outgoing, to_server, to_channel = b"", b"", b"", b""
        buffer = C.create_string_buffer(32768)
        deadline = time.monotonic() + 30
        try:
            while not server.stop.is_set():
                if time.monotonic() >= deadline:
                    raise AssertionError("SFTP fixture did not settle within its deadline")
                if fixture.SERVER.ssh_channel_is_closed(channel) or fixture.SERVER.ssh_channel_is_eof(channel):
                    return
                message = fixture.SERVER.ssh_message_get(session)
                if message:
                    try:
                        server.events.append(("rejected-channel-request", fixture.SERVER.ssh_message_subtype(message)))
                        fixture.SERVER.ssh_message_reply_default(message)
                    finally:
                        fixture.SERVER.ssh_message_free(message)
                if len(to_server) < 65536:
                    count = fixture.SERVER.ssh_channel_read_nonblocking(channel, buffer, len(buffer), 0)
                    if count > 0:
                        incoming += buffer.raw[:count]
                        incoming, packets = self.packets(incoming)
                        for packet in packets:
                            to_server += self.request(packet)
                    elif count not in (0, -2):
                        return
                if to_server:
                    try:
                        count = bridge.send(to_server[:32768])
                        to_server = to_server[count:]
                    except BlockingIOError:
                        pass
                    except (BrokenPipeError, ConnectionResetError):
                        return
                if len(to_channel) < 65536:
                    try:
                        received = bridge.recv(32768)
                        if not received:
                            return
                        outgoing += received
                        outgoing, packets = self.packets(outgoing)
                        for packet in packets:
                            to_channel += self.response(packet)
                    except BlockingIOError:
                        pass
                if to_channel:
                    count = fixture.SERVER.ssh_channel_write(channel, to_channel, min(32768, len(to_channel)))
                    if count > 0:
                        to_channel = to_channel[count:]
                        if not to_channel and self.initialization_fragment_delivered:
                            self.initialization_fragment_delivered = False
                            self.marker("init-seen")
                    elif count == -1:
                        return
                server.stop.wait(0.001)
        finally:
            bridge.close()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.terminate()
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=2)


class WorkspaceSFTPInteropTests(unittest.TestCase):
    def test_workspace_files_use_real_sftp(self):
        self.assertEqual(fixture.DRIVER.ftssh_supports_sftp(), 1,
                         "The product must use its patched SFTP-capable libssh2")
        selected = os.environ.get("FT_SFTP_MODES", ",".join(MODES)).split(",")
        self.assertTrue(selected and all(mode in MODES for mode in selected))
        for mode in selected:
            with self.subTest(mode=mode):
                self.exercise(mode)

    def exercise(self, mode):
        root = Path(tempfile.mkdtemp(prefix="frankenterm-sftp-"))
        (root / "download-source.bin").write_bytes(DOWNLOAD)
        (root / "existing.bin").write_bytes(EXISTING)
        (root / "résumé λ.bin").write_bytes(b"unicode name\n")
        (root / "directory").mkdir()
        (root / "local").mkdir()
        (root / "local" / "upload-source.bin").write_bytes(UPLOAD)
        key = ed25519.Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
        servers, bridges, peers = [], {}, []

        def add(role, sftp=True, **options):
            bridge = OpenSSHSFTPBridge(root, mode) if sftp else None
            server = fixture.LoopbackServer(key, sftp_handler=bridge, **options)
            servers.append((role, server))
            if bridge:
                bridges[role] = bridge
            peers.append({"id": role, "port": server.address[1],
                          "hostKey": base64.b64encode(server.host_blob).decode("ascii")})
            return server

        if mode.startswith("scope-"):
            options = {"connections": 2}
            if mode == "scope-password":
                options["password"] = b"files owner password"
            else:
                options["interactive_rounds"] = [{"name": b"Files ownership", "instruction": b"Confirm this file connection.",
                                                  "prompts": [(b"Approval:", False)], "responses": [b"approved"]}]
                if mode == "scope-publickey-interactive":
                    options["auth_mode"] = "publickeyInteractive"
            target = add("target", **options)
        else:
            target = add("target")
        if mode == "jump":
            add("hop", sftp=False, forward_to=target.address, forward_host="target.internal.invalid")
        elif mode == "ownership":
            add("terminal", sftp=False)
            add("second")
            add("third")
        try:
            environment = dict(os.environ, FT_SFTP_TEST_MODE=mode, FT_SFTP_TEST_ROOT=str(root),
                               FT_SFTP_TEST_PEERS=json.dumps(peers))
            result = subprocess.run(xctest_command(mode), env=environment, capture_output=True, text=True, timeout=35)
            output = result.stdout + result.stderr
            self.assertEqual(result.returncode, 0, output[-20000:])
            self.assertIn("Executed 1 test", output)
            self.assertNotIn("skipped", output.lower())
            for role, server in servers:
                self.assertTrue(server.done.wait(2), (role, "SSH owner did not close"))
                self.assertEqual(server.errors, [], role)
                logins = 2 if mode.startswith("scope-") else 1
                self.assertEqual(server.accepted_connections, logins, role)
                self.assertFalse(any(event[0] in ("pty", "shell", "exec", "resize", "rejected-channel-request")
                                     for event in server.events), (role, server.events))
                authentication = [event for event in server.events if event[0] == "authentication"]
                expected_keys = [] if mode in ("scope-password", "scope-interactive") else [0, 1] * logins
                self.assertEqual([event[1] for event in authentication], expected_keys, role)
                self.assertTrue(all(event[2] == b"operator" and event[3] == fixture.public_blob(key)
                                    for event in authentication), role)
                if mode == "scope-password":
                    self.assertEqual([event for event in server.events if event[0] == "password-authentication"],
                                     [("password-authentication", b"operator", len(b"files owner password"), True)] * 2)
                elif mode in ("scope-interactive", "scope-publickey-interactive"):
                    self.assertEqual([event for event in server.events if event[0] == "interactive-request"],
                                     [("interactive-request", b"operator")] * 2)
                    self.assertEqual([event for event in server.events if event[0] == "interactive-response"],
                                     [("interactive-response", 1, 1, True)] * 2)
                subsystems = [event for event in server.events if event[0] == "subsystem"]
                self.assertEqual(subsystems, [("subsystem", b"sftp")] * logins if role in bridges else [], role)
            self.assertEqual((root / "existing.bin").read_bytes(), EXISTING)
            if mode in ("direct", "jump"):
                self.assertEqual((root / "local" / "downloaded.bin").read_bytes(), DOWNLOAD)
                self.assertEqual((root / "uploaded.bin").read_bytes(), UPLOAD)
                self.assertIn(3, bridges["target"].requests)  # Actual OPEN operations.
                self.assertIn(5, bridges["target"].requests)  # Actual READ operations.
                self.assertIn(6, bridges["target"].requests)  # Actual WRITE operations.
            elif mode in ("cancel-stall", "timeout-stall"):
                self.assertTrue(bridges["target"].stalled.is_set())
                self.assertTrue((root / "read-seen").is_file())
            elif mode == "cancel-init":
                self.assertTrue(bridges["target"].response_stalled.is_set())
                self.assertTrue((root / "init-seen").is_file())
            elif mode == "cancel-upload":
                self.assertTrue(bridges["target"].response_stalled.is_set())
                partial = (root / "cancelled-upload.bin").read_bytes()
                self.assertGreater(len(partial), 0)
                self.assertLessEqual(len(partial), len(UPLOAD))
                self.assertEqual(partial, UPLOAD[:len(partial)])
            elif mode in ("revoke", "stale"):
                self.assertTrue((root / "retired").is_file())
                self.assertTrue(all(kind == 4 for kind in bridges["target"].after_retirement),
                                bridges["target"].after_retirement)
            print(f"SFTP {mode}: real peer assertions passed; retained fixture {root}", flush=True)
        finally:
            for _, server in reversed(servers):
                server.close()


if __name__ == "__main__":
    unittest.main()
