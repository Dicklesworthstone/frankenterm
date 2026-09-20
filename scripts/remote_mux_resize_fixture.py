#!/usr/bin/env python3
"""Owned real-PTY workload for remote mux profiling (never a production session).

The corpus is generated before measurement. PROBE replies observe the kernel's
PTY dimensions, not mux request admission. This program contains no latency
claims; the persistent protocol client owns the timed interval and oracle.
"""

import argparse
import fcntl
import hashlib
import json
import math
import os
import pathlib
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import time
import tty


def corpus(records):
    """Keep whitespace and Unicode explicit, with unique ordered boundaries."""
    return "".join(
        f"FT_RECORD_{index:05d} A  B\u00a0C\u2003D e\u0301 界面 🚀 "
        + "0123456789 abcdefghijklmnopqrstuvwxyz " * 3
        + f"FT_END_{index:05d}\n"
        for index in range(records)
    )


def write_new(path, data):
    with open(path, "x", encoding="utf-8", newline="",
              opener=lambda name, flags: os.open(name, flags, 0o600)) as output:
        output.write(data)
        output.flush()
        os.fsync(output.fileno())


def file_sha256(path):
    digest = hashlib.sha256()
    with pathlib.Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_bounded(data, deadline):
    pending = memoryview(data)
    while pending:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("owned PTY output deadline expired")
        if not select.select([], [1], [], min(remaining, 0.25))[1]:
            continue
        try:
            count = os.write(1, pending[:16384])
        except BlockingIOError:
            continue
        if count == 0:
            raise RuntimeError("owned PTY output made no progress")
        pending = pending[count:]


def run(args):
    if getattr(args, "arm", "resize") == "swarm-echo":
        run_swarm_echo(args)
    elif getattr(args, "arm", "resize") == "echo":
        run_echo(args)
    else:
        run_resize(args)


def run_resize(args):
    if not os.isatty(0) or not os.isatty(1):
        raise RuntimeError("fixture requires an explicitly owned real PTY")
    source = pathlib.Path(args.corpus).read_bytes()
    if source != corpus(args.records).encode("utf-8"):
        raise ValueError("corpus bytes differ from the deterministic workload")
    digest = hashlib.sha256(source).hexdigest()
    identity = {"pid": os.getpid(), "parent_pid": os.getppid(),
                "corpus_sha256": digest, "records": args.records,
                "started_monotonic_ns": time.monotonic_ns()}
    proc_stat = pathlib.Path("/proc/self/stat")
    if proc_stat.exists():
        identity["linux_start_ticks"] = process_identity(os.getpid())["start_ticks"]
    write_new(args.owner, json.dumps(identity, sort_keys=True) + "\n")
    original = termios.tcgetattr(0)
    original_blocking = os.get_blocking(1)
    deadline = time.monotonic() + args.timeout_seconds
    pending = bytearray()
    received = 0
    try:
        tty.setraw(0)
        os.set_blocking(1, False)
        write_bounded(source.replace(b"\n", b"\r\n"), deadline)
        write_bounded(f"FT_CORPUS_READY {digest}\r\n".encode(), deadline)
        write_new(str(args.owner) + ".ready", json.dumps({"corpus_sha256": digest}) + "\n")
        while time.monotonic() < deadline:
            remaining = deadline - time.monotonic()
            if not select.select([0], [], [], max(0, min(remaining, 0.25)))[0]:
                continue
            chunk = os.read(0, 4096)
            if not chunk:
                raise RuntimeError("owned PTY input closed before explicit EXIT")
            received += len(chunk)
            if received > 65536:
                raise RuntimeError("owned PTY input exceeded 64 KiB")
            pending.extend(chunk)
            while b"\n" in pending:
                command, _, rest = pending.partition(b"\n")
                pending = bytearray(rest)
                match = re.fullmatch(rb"(PROBE|EXIT) ([a-zA-Z0-9_-]{1,64})", command)
                if match is None:
                    raise ValueError("unexpected owned PTY command")
                operation, nonce = match.groups()
                if operation == b"EXIT":
                    write_bounded(b"FT_EXIT " + nonce + b"\r\n", deadline)
                    return
                rows, cols, _, _ = struct.unpack(
                    "HHHH", fcntl.ioctl(0, termios.TIOCGWINSZ, b"\0" * 8))
                write_bounded(b"FT_PROBE " + nonce + f" {rows} {cols}\r\n".encode(), deadline)
            if len(pending) > 256:
                raise ValueError("owned PTY command exceeded 256 bytes")
        raise TimeoutError("owned PTY lifetime deadline expired")
    finally:
        os.set_blocking(1, original_blocking)
        termios.tcsetattr(0, termios.TCSANOW, original)


def run_echo(args):
    """Opt-in sustained output & keystroke echo fixture (ft-4p4uo)."""
    if not os.isatty(0) or not os.isatty(1):
        raise RuntimeError("fixture requires an explicitly owned real PTY")
    identity = {
        "pid": os.getpid(),
        "parent_pid": os.getppid(),
        "arm": "echo",
        "started_monotonic_ns": time.monotonic_ns(),
    }
    proc_stat = pathlib.Path("/proc/self/stat")
    if proc_stat.exists():
        identity["linux_start_ticks"] = process_identity(os.getpid())["start_ticks"]
    write_new(args.owner, json.dumps(identity, sort_keys=True) + "\n")
    original = termios.tcgetattr(0)
    original_blocking_1 = os.get_blocking(1)
    original_blocking_0 = os.get_blocking(0)
    deadline = time.monotonic() + args.timeout_seconds

    # Explicit caps as mandated by root review
    MAX_PENDING_IN = 65536
    MAX_PENDING_OUT = 262144
    MAX_TOTAL_BYTES = 256 * 1024 * 1024
    MAX_LINE_LENGTH = 256

    COMMAND_RE = re.compile(rb"^(START_STREAM|PROBE|STREAM_FINISH|EXIT) ([a-zA-Z0-9_-]{1,64})$")

    bytes_written = 0
    streaming = False
    stream_started = False
    stream_done = False
    active_stream_nonce = None
    stream_finish_nonce = None
    stream_record_seq = 0
    stream_bytes_start = 0
    stream_start_ns = 0
    stream_bytes_end = 0
    stream_end_ns = 0
    probes_served = 0
    pending_out = bytearray()
    pending_in = bytearray()

    # Pre-generate structured stream record template
    # 122 bytes per record: 16-byte prefix + 104-byte payload + 2-byte \r\n
    stream_payload = (b"0123456789abcdef" * 6) + b"01234567"

    try:
        tty.setraw(0)
        os.set_blocking(1, False)
        os.set_blocking(0, False)
        write_bounded(b"FT_ECHO_READY\r\n", deadline)
        bytes_written += len(b"FT_ECHO_READY\r\n")
        write_new(str(args.owner) + ".ready", json.dumps({"arm": "echo", "status": "ready"}) + "\n")

        while time.monotonic() < deadline:
            # If streaming is active, refill pending_out up to 32 KiB
            if streaming and len(pending_out) < 32768:
                chunks = []
                batch_bytes = 0
                while batch_bytes < 32768:
                    rec = f"FT_STR_{stream_record_seq:08d} ".encode("ascii") + stream_payload + b"\r\n"
                    chunks.append(rec)
                    batch_bytes += len(rec)
                    stream_record_seq += 1
                new_data = b"".join(chunks)
                if len(pending_out) + len(new_data) > MAX_PENDING_OUT:
                    raise RuntimeError("owned PTY pending_out exceeded cap during streaming refill")
                pending_out.extend(new_data)

            # If stream finish was requested and pending stream data has drained:
            if stream_finish_nonce is not None and not pending_out:
                stream_bytes_end = bytes_written
                stream_end_ns = time.monotonic_ns()
                marker = f"FT_STREAM_END {stream_finish_nonce} bytes={bytes_written} FT_END\r\n".encode("ascii")
                if len(pending_out) + len(marker) > MAX_PENDING_OUT:
                    raise RuntimeError("owned PTY pending_out exceeded cap")
                pending_out.extend(marker)
                stream_finish_nonce = None
                stream_done = True

            r_list = [0]
            w_list = [1] if pending_out else []
            r, w, _ = select.select(r_list, w_list, [], 0.005)

            if 0 in r:
                try:
                    data = os.read(0, 4096)
                except (BlockingIOError, InterruptedError):
                    data = None
                if data is not None:
                    if len(data) == 0:
                        raise RuntimeError("owned PTY input closed before explicit EXIT")
                    if len(pending_in) + len(data) > MAX_PENDING_IN:
                        raise RuntimeError("owned PTY pending_in exceeded 64 KiB cap")
                    pending_in.extend(data)
                    while b"\n" in pending_in:
                        line, _, rest = pending_in.partition(b"\n")
                        pending_in = bytearray(rest)
                        cmd = line.rstrip(b"\r")
                        if not cmd:
                            continue
                        if len(cmd) > MAX_LINE_LENGTH:
                            raise ValueError("owned PTY command exceeded 256 bytes")
                        match = COMMAND_RE.fullmatch(cmd)
                        if match is None:
                            raise ValueError(f"unexpected owned PTY command: {cmd!r}")
                        operation, nonce_bytes = match.groups()
                        nonce = nonce_bytes.decode("ascii")

                        if operation == b"START_STREAM":
                            if stream_started or streaming or stream_done:
                                raise ValueError(
                                    f"START_STREAM received in invalid state (started={stream_started}, streaming={streaming}, done={stream_done})"
                                )
                            stream_started = True
                            streaming = True
                            active_stream_nonce = nonce
                            stream_bytes_start = bytes_written
                            stream_start_ns = time.monotonic_ns()
                            marker = f"FT_STREAM_START {nonce} bytes={bytes_written} FT_END\r\n".encode("ascii")
                            if len(pending_out) + len(marker) > MAX_PENDING_OUT:
                                raise RuntimeError("owned PTY pending_out exceeded cap")
                            pending_out.extend(marker)
                        elif operation == b"PROBE":
                            if not streaming or stream_done:
                                raise ValueError("PROBE command received when streaming is not active")
                            expected_nonce = f"echo_trial_{probes_served:04}"
                            if nonce != expected_nonce:
                                raise ValueError(f"probe nonce mismatch: expected {expected_nonce}, got {nonce}")
                            resp = f"FT_PROBE {nonce}\r\n".encode("ascii")
                            if len(pending_out) + len(resp) > MAX_PENDING_OUT:
                                raise RuntimeError("owned PTY pending_out exceeded cap")
                            pending_out.extend(resp)
                            probes_served += 1
                        elif operation == b"STREAM_FINISH":
                            if not streaming or not stream_started or stream_done or stream_finish_nonce is not None:
                                raise ValueError(
                                    f"STREAM_FINISH received in invalid state (started={stream_started}, streaming={streaming}, done={stream_done})"
                                )
                            if nonce != active_stream_nonce:
                                raise ValueError(
                                    f"STREAM_FINISH nonce mismatch: expected {active_stream_nonce}, got {nonce}"
                                )
                            streaming = False
                            stream_finish_nonce = nonce
                        elif operation == b"EXIT":
                            if not stream_done:
                                raise ValueError("EXIT command received before stream completion")
                            if probes_served < 1000:
                                raise ValueError(f"EXIT command received with only {probes_served} probes served")
                            resp = f"FT_EXIT {nonce}\r\n".encode("ascii")
                            if len(pending_out) + len(resp) > MAX_PENDING_OUT:
                                raise RuntimeError("owned PTY pending_out exceeded cap")
                            pending_out.extend(resp)
                            exit_deadline = min(deadline, time.monotonic() + 5.0)
                            while pending_out and time.monotonic() < exit_deadline:
                                rem = exit_deadline - time.monotonic()
                                if not select.select([], [1], [], max(0, min(rem, 0.25)))[1]:
                                    continue
                                try:
                                    n = os.write(1, pending_out[:16384])
                                    if n > 0:
                                        del pending_out[:n]
                                        bytes_written += n
                                except (BlockingIOError, InterruptedError):
                                    continue
                            if pending_out:
                                raise TimeoutError("owned PTY exit flush deadline expired")
                            final_receipt = {
                                "arm": "echo",
                                "status": "completed",
                                "bytes_written": bytes_written,
                                "stream_nonce": active_stream_nonce,
                                "stream_bytes_start": stream_bytes_start,
                                "stream_start_ns": stream_start_ns,
                                "stream_bytes_end": stream_bytes_end,
                                "stream_end_ns": stream_end_ns,
                                "actual_stream_bytes": stream_bytes_end - stream_bytes_start,
                                "probes_served": probes_served,
                            }
                            write_new(str(args.owner) + ".final", json.dumps(final_receipt, sort_keys=True) + "\n")
                            return

            if 1 in w and pending_out:
                try:
                    written = os.write(1, pending_out[:16384])
                    if written > 0:
                        del pending_out[:written]
                        bytes_written += written
                        if bytes_written > MAX_TOTAL_BYTES:
                            raise RuntimeError("owned PTY total bytes written exceeded 256 MiB cap")
                except (BlockingIOError, InterruptedError):
                    pass

        raise TimeoutError("owned PTY lifetime deadline expired")
    finally:
        os.set_blocking(1, original_blocking_1)
        os.set_blocking(0, original_blocking_0)
        termios.tcsetattr(0, termios.TCSANOW, original)


def noise_record(sequence):
    if type(sequence) is not int or not 0 <= sequence < 10**16:
        raise ValueError("invalid noise record sequence")
    return f"FT_NOISE {sequence:016d} 0123456789abcdef FT_END\r\n".encode("ascii")


def parsed_noise_sequence(text):
    if not isinstance(text, str) or len(text.encode("utf-8")) > 4096:
        raise ValueError("invalid bounded noise tail")
    latest = None
    for line in text.splitlines():
        line = line.rstrip(" ")
        match = re.fullmatch(r"FT_NOISE ([0-9]{16}) 0123456789abcdef FT_END", line)
        if match is None:
            continue
        sequence = int(match[1])
        if latest is not None and sequence != latest + 1:
            raise ValueError("discontinuous parsed noise tail")
        latest = sequence
    if latest is None:
        raise ValueError("no complete parsed noise record")
    return latest


def run_swarm_echo(args):
    """Separate bounded noise producers and a quiet, strictly ordered echo PTY."""
    if not os.isatty(0) or not os.isatty(1):
        raise RuntimeError("fixture requires an explicitly owned real PTY")
    role = args.swarm_role
    pane = int(os.environ["WEZTERM_PANE"])
    owner_path = args.owner if role == "interactive" else f"{args.owner}-{pane}.json"
    identity = {"pid": os.getpid(), "parent_pid": os.getppid(), "pane_id": pane,
                "arm": "swarm-echo", "role": role,
                "linux_start_ticks": process_identity(os.getpid())["start_ticks"]}
    write_new(owner_path, json.dumps(identity, sort_keys=True) + "\n")
    original = termios.tcgetattr(0)
    blocking = [os.get_blocking(fd) for fd in (0, 1)]
    deadline = time.monotonic() + args.timeout_seconds
    pending_in, pending_out = bytearray(), bytearray()
    streaming = False
    sequence = probes = written = received = 0
    try:
        tty.setraw(0)
        os.set_blocking(0, False)
        os.set_blocking(1, False)
        write_bounded(f"FT_SWARM_READY {role}\r\n".encode(), deadline)
        write_new(owner_path + ".ready", json.dumps(identity) + "\n")
        while time.monotonic() < deadline:
            if streaming and len(pending_out) < 16384:
                while len(pending_out) < 32768:
                    record = noise_record(sequence)
                    if written + len(pending_out) + len(record) > 256 * 1024 * 1024:
                        raise ValueError("swarm producer reached 256 MiB lifetime cap")
                    pending_out.extend(record)
                    sequence += 1
            readable, writable, _ = select.select([0], [1] if pending_out else [], [], 0.005)
            if readable:
                try:
                    chunk = os.read(0, 4096)
                except (BlockingIOError, InterruptedError):
                    chunk = None
                if chunk == b"":
                    raise RuntimeError("owned PTY closed before EXIT")
                if chunk:
                    received += len(chunk)
                    if received > 65536:
                        raise ValueError("swarm input exceeded 64 KiB")
                    pending_in.extend(chunk)
                while b"\n" in pending_in:
                    command, _, rest = pending_in.partition(b"\n")
                    pending_in = bytearray(rest)
                    if command == b"START_STREAM swarm_session_001" and role == "noise" and not streaming and sequence == 0:
                        streaming = True
                    elif command == f"PROBE swarm_trial_{probes:04d}".encode() and role == "interactive" and probes < 1000:
                        pending_out.extend(f"FT_PROBE swarm_trial_{probes:04d}\r\n".encode())
                        probes += 1
                    elif command == b"EXIT swarm_exit" and ((role == "noise" and streaming) or (role == "interactive" and probes == 1000)):
                        # Settlement is outside the counted interval. No final
                        # producer counter is credited as concurrent ingestion.
                        write_bounded(pending_out, min(deadline, time.monotonic() + 5))
                        written += len(pending_out)
                        final = dict(identity, status="completed", probes_served=probes,
                                     records_generated=sequence, bytes_written=written,
                                     record_bytes=len(noise_record(0)))
                        write_new(owner_path + ".final", json.dumps(final, sort_keys=True) + "\n")
                        return
                    else:
                        raise ValueError("unexpected swarm command or command order")
                if len(pending_in) > 256 or len(pending_out) > 65536:
                    raise ValueError("swarm pending buffer exceeded cap")
            if writable and pending_out:
                try:
                    count = os.write(1, pending_out[:16384])
                except (BlockingIOError, InterruptedError):
                    continue
                if count <= 0:
                    raise RuntimeError("swarm output made no progress")
                del pending_out[:count]
                written += count
                if written > 256 * 1024 * 1024:
                    raise ValueError("swarm producer exceeded 256 MiB lifetime cap")
        raise TimeoutError("swarm PTY lifetime deadline expired")
    finally:
        for fd, mode in enumerate(blocking):
            os.set_blocking(fd, mode)
        termios.tcsetattr(0, termios.TCSANOW, original)


def process_identity(pid):
    try:
        # Linux comm is arbitrary bytes and may contain spaces or parentheses.
        fields = pathlib.Path(f"/proc/{pid}/stat").read_bytes().rsplit(b")", 1)[1].split()
        return {"pid": pid, "parent_pid": int(fields[1]),
                "start_ticks": str(int(fields[19])), "state": fields[0].decode("ascii")}
    except (FileNotFoundError, ProcessLookupError):
        return None


class OwnedProcesses:
    """Retain kernel handles while ancestry is live, before any timed workload."""

    def __init__(self):
        self.identities = {}
        self.pidfds = {}

    def retain(self, identity, parent=None):
        pid = identity["pid"]
        if pid in self.identities:
            if self.identities[pid]["start_ticks"] != identity["start_ticks"]:
                raise RuntimeError("owned PID identity changed")
            return
        pidfd = os.pidfd_open(pid)
        try:
            current = process_identity(pid)
            if (not current or current["start_ticks"] != identity["start_ticks"]
                    or select.select([pidfd], [], [], 0)[0]):
                raise RuntimeError("process exited or changed before pidfd capture")
            if parent is not None:
                if parent not in self.pidfds or select.select([self.pidfds[parent]], [], [], 0)[0]:
                    raise RuntimeError("owned parent exited before descendant admission")
                live_parent = process_identity(parent)
                if (not live_parent or current["parent_pid"] != parent
                        or live_parent["start_ticks"] != self.identities[parent]["start_ticks"]):
                    raise RuntimeError("descendant lost its owned ancestry")
            self.identities[pid] = current
            self.pidfds[pid] = pidfd
        except BaseException:
            os.close(pidfd)
            raise

    def remember_descendants(self):
        candidates = []
        for entry in pathlib.Path("/proc").glob("[0-9]*"):
            identity = process_identity(int(entry.name))
            if identity:
                candidates.append(identity)
        changed = True
        while changed:
            changed = False
            for identity in candidates:
                if identity["pid"] in self.identities or identity["parent_pid"] not in self.identities:
                    continue
                try:
                    self.retain(identity, identity["parent_pid"])
                except ProcessLookupError:
                    continue
                changed = True

    def require_fixture(self, owner, server_pid, guardian_path):
        pid = owner["pid"]
        identity = process_identity(pid)
        if not identity or identity["start_ticks"] != str(owner["linux_start_ticks"]):
            raise RuntimeError("fixture receipt does not name a live exact process")
        chain = [identity]
        seen = {pid}
        while chain[-1]["pid"] != server_pid:
            parent = process_identity(chain[-1]["parent_pid"])
            if not parent or parent["pid"] in seen or len(chain) >= 16:
                raise RuntimeError("fixture custody does not reach the owned mux")
            seen.add(parent["pid"])
            chain.append(parent)
        # The supported direct-exec fixture has only guardian processes between
        # Python and the mux. Refuse a different model instead of claiming a tree.
        for ancestor in chain[1:-1]:
            if pathlib.Path(os.readlink(f'/proc/{ancestor["pid"]}/exe')).resolve() != guardian_path:
                raise RuntimeError("unrecognized process in fixture guardian custody")
        for member in reversed(chain[:-1]):
            self.retain(member, member["parent_pid"])
        for member in chain:
            current = process_identity(member["pid"])
            if (not current or current["start_ticks"] != member["start_ticks"]
                    or select.select([self.pidfds[member["pid"]]], [], [], 0)[0]
                    or (member["pid"] != server_pid and current["parent_pid"] != member["parent_pid"])):
                raise RuntimeError("fixture custody changed before measurement")
        return chain

    def cleanup(self, server, timeout=10):
        errors = []
        def failed(stage, error, pid=None):
            errors.append({"stage": stage, "pid": pid,
                           "error": f"{type(error).__name__}: {error}"})
        try:
            try:
                self.remember_descendants()
            except Exception as error:
                failed("final_discovery", error)
            for pid, pidfd in reversed(list(self.pidfds.items())):
                try:
                    signal.pidfd_send_signal(pidfd, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                except Exception as error:
                    failed("signal", error, pid)
            # Popen retains child custody when initial pidfd acquisition fails.
            if server is not None and server.pid not in self.pidfds:
                try:
                    server.terminate()
                except ProcessLookupError:
                    pass
                except Exception as error:
                    failed("server_fallback_terminate", error, server.pid)
            deadline = time.monotonic() + timeout
            pending = dict(self.pidfds)
            while pending and time.monotonic() < deadline:
                try:
                    ready = select.select(list(pending.values()), [], [],
                                          max(0, min(0.1, deadline - time.monotonic())))[0]
                except Exception as error:
                    failed("wait_pidfds", error)
                    break
                pending = {pid: fd for pid, fd in pending.items() if fd not in ready}
            for pid in pending:
                failed("settlement", TimeoutError("owned process did not exit within cleanup budget"), pid)
            if server is not None:
                try:
                    server.wait(timeout=max(0, deadline - time.monotonic()))
                except Exception as error:
                    failed("reap_server", error, server.pid)
        finally:
            for pid, pidfd in self.pidfds.items():
                try:
                    os.close(pidfd)
                except Exception as error:
                    failed("close_pidfd", error, pid)
            self.pidfds.clear()
        return errors


def finish_measurement(root, receipt, owned, server):
    prior_status = receipt["status"]
    receipt["status"] = "failed"
    receipt["cleanup_errors"] = []
    try:
        receipt["cleanup_errors"] = owned.cleanup(server)
        if not receipt["cleanup_errors"]:
            receipt["status"] = prior_status
    except Exception as error:
        receipt["cleanup_errors"].append({"stage": "cleanup", "error": f"{type(error).__name__}: {error}"})
    finally:
        receipt["owned_process_identities"] = list(owned.identities.values())
        write_new(root / "receipt.json", json.dumps(receipt, indent=2) + "\n")


def validate_measurements(rows, instrumented):
    """Require the exact workload and complete, same-clock phase coverage."""
    expected_count = 83 + (324 if instrumented else 0)
    if len(rows) != expected_count:
        raise ValueError("missing, duplicate or unexpected measurement events")
    contract = rows[0]
    if (contract.get("event") != "contract" or contract.get("instrumented") is not instrumented
            or contract.get("trials") != 20 or contract.get("rows") != 24
            or contract.get("columns") != [120, 60, 100, 80]
            or contract.get("phase_clock") != ("CLOCK_MONOTONIC" if instrumented else None)):
        raise ValueError("helper contract does not match requested measurement arm")
    phases = ("resize_admission", "terminal_convergence", "pty_probe", "full_history_oracle")
    cells = [(None, 80, "warmup")]
    cells.extend((trial, cols, f"trial_{trial:03}_cols_{cols}")
                 for trial in range(20) for cols in (120, 60, 100, 80))
    index, previous_end = 1, 0
    for trial, cols, nonce in cells:
        if instrumented:
            for phase_index, name in enumerate(phases):
                phase = rows[index]
                index += 1
                start, end = phase.get("start_ns"), phase.get("end_ns")
                if (phase.get("event") != "phase" or phase.get("instrumented") is not True
                        or phase.get("nonce") != nonce or phase.get("columns") != cols
                        or phase.get("phase") != name or phase.get("clock") != "CLOCK_MONOTONIC"
                        or type(start) is not int or type(end) is not int
                        or start <= 0 or end <= 0 or end < start
                        or start < previous_end or (phase_index and start != previous_end)):
                    raise ValueError(f"invalid or overlapping phase coverage for {nonce}/{name}")
                previous_end = end
        row = rows[index]
        index += 1
        if trial is None:
            if row.get("event") != "warmup":
                raise ValueError("missing ordered warmup event")
            measurement = row.get("measurement", {})
        else:
            if row.get("event") != "trial" or type(row.get("trial")) is not int or row["trial"] != trial:
                raise ValueError("missing, duplicate or out-of-order trial")
            measurement = row
        if (measurement.get("status") != "passed" or measurement.get("instrumented") is not instrumented
                or measurement.get("columns") != cols or measurement.get("rows") != 24
                or measurement.get("exact_corpus_preserved") is not True):
            raise ValueError("sample does not match requested arm, geometry or correctness contract")
    if rows[index] != {"event": "complete", "status": "passed", "trials_per_geometry": 20, "samples": 80}:
        raise ValueError("incomplete or mismatched terminal measurement event")


def nearest_rank_percentile(sorted_list, q):
    """Nearest-rank percentile where q is in [0.0, 1.0] (e.g. 0.50, 0.90, 0.95, 0.99).

    Rank is ceil(q * n), 0-indexed position is ceil(q * n) - 1, clamped to [0, n - 1].
    """
    n = len(sorted_list)
    assert n > 0, "cannot compute percentile of empty list"
    rank = int(math.ceil(q * n))
    idx = max(0, min(rank, n) - 1)
    return sorted_list[idx]


def validate_echo_measurements(rows, fixture_receipt=None):
    """Require exactly 1000 nonces, >=10 MiB/s throughput, and the p99 budget.

    Independently recomputes all statistics from raw trial rows using nearest-rank
    percentiles (ceil(q*n)-1) without trusting client self-reported claims.
    """
    if not rows:
        raise ValueError("empty measurement rows")
    contract = rows[0]
    if contract.get("event") != "contract" or contract.get("arm") != "echo":
        raise ValueError("invalid or missing contract row for echo arm")
    if contract.get("version") != 1:
        raise ValueError("unsupported contract version")
    trials = contract.get("trials")
    if type(trials) is not int or trials != 1000:
        raise ValueError(f"echo arm requires 1000 trials, got {trials!r}")
    p99_budget = contract.get("p99_budget_us")
    if p99_budget != 50_000:
        raise ValueError(f"echo arm p99 budget must be 50,000 us, got {p99_budget!r}")
    min_throughput = contract.get("min_throughput_mb_s")
    if min_throughput != 10.0:
        raise ValueError(f"echo arm min throughput must be 10.0 MB/s, got {min_throughput!r}")

    expected_row_count = trials + 2
    if len(rows) != expected_row_count:
        raise ValueError(f"expected {expected_row_count} measurement rows, got {len(rows)}")

    echo_trials = rows[1:-1]
    latencies = []
    seen_trials = set()
    for idx, row in enumerate(echo_trials):
        if row.get("event") != "echo_trial":
            raise ValueError(f"row {idx + 1} is not an echo_trial event")
        trial_num = row.get("trial")
        if type(trial_num) is not int or trial_num != idx:
            raise ValueError(f"missing, duplicate or out-of-order trial: expected {idx}, got {trial_num!r}")
        if trial_num in seen_trials:
            raise ValueError(f"duplicate trial index: {trial_num}")
        seen_trials.add(trial_num)

        expected_nonce = f"echo_trial_{idx:04}"
        nonce = row.get("nonce")
        if nonce != expected_nonce:
            raise ValueError(f"nonce mismatch in trial {trial_num}: expected {expected_nonce!r}, got {nonce!r}")

        latency = row.get("latency_us")
        if type(latency) is not int:
            raise ValueError(f"non-integer latency in trial {trial_num}: {latency!r}")
        if not math.isfinite(latency) or latency < 0:
            raise ValueError(f"invalid latency value in trial {trial_num}: {latency}")
        latencies.append(latency)

    latencies.sort()
    n = len(latencies)
    if n != trials:
        raise ValueError(f"trial count mismatch: expected {trials}, got {n}")
    indep_p50 = nearest_rank_percentile(latencies, 0.50)
    indep_p90 = nearest_rank_percentile(latencies, 0.90)
    indep_p95 = nearest_rank_percentile(latencies, 0.95)
    indep_p99 = nearest_rank_percentile(latencies, 0.99)

    if indep_p99 > p99_budget:
        raise ValueError(f"independently computed p99 echo latency {indep_p99} us exceeds budget {p99_budget} us")

    complete = rows[-1]
    if complete.get("event") != "complete" or complete.get("status") != "passed" or complete.get("arm") != "echo":
        raise ValueError("incomplete, unpassed, or mismatched terminal measurement event")
    if complete.get("trials") != trials:
        raise ValueError(f"complete event trials {complete.get('trials')} does not match contract {trials}")

    start_bytes = complete.get("start_bytes")
    if type(start_bytes) is not int or start_bytes < 0:
        raise ValueError(f"invalid start_bytes in complete event: {start_bytes!r}")
    end_bytes = complete.get("end_bytes")
    if type(end_bytes) is not int or end_bytes < start_bytes:
        raise ValueError(f"invalid end_bytes in complete event: {end_bytes!r}")

    bytes_ingested = complete.get("bytes_ingested")
    if type(bytes_ingested) is not int or bytes_ingested != (end_bytes - start_bytes) or bytes_ingested < 10 * 1024 * 1024:
        raise ValueError(f"invalid bytes_ingested {bytes_ingested!r} in complete event (expected {end_bytes - start_bytes}, >= 10 MiB)")

    duration_s = complete.get("duration_seconds")
    if type(duration_s) not in (int, float) or not math.isfinite(duration_s) or duration_s <= 0:
        raise ValueError(f"invalid duration in complete event: {duration_s!r}")

    indep_throughput_bytes_s = bytes_ingested / duration_s
    indep_throughput_mb_s = indep_throughput_bytes_s / (1024.0 * 1024.0)
    if not math.isfinite(indep_throughput_mb_s) or indep_throughput_mb_s < 10.0:
        raise ValueError(f"independently computed throughput {indep_throughput_mb_s:.2f} MB/s below 10.0 MB/s requirement")

    client_p99 = complete.get("p99_echo_us")
    if type(client_p99) is not int or client_p99 != indep_p99:
        raise ValueError(f"client self-reported p99 ({client_p99}) does not match independently computed p99 ({indep_p99})")
    client_tp = complete.get("throughput_mb_s")
    if type(client_tp) not in (int, float) or not math.isfinite(client_tp) or client_tp <= 0 or abs(client_tp - indep_throughput_mb_s) > 0.01:
        raise ValueError(f"client self-reported throughput ({client_tp!r}) does not match independently computed throughput ({indep_throughput_mb_s:.2f})")

    if fixture_receipt is not None:
        if fixture_receipt.get("arm") != "echo" or fixture_receipt.get("status") != "completed":
            raise ValueError("fixture receipt status is not completed")
        if fixture_receipt.get("actual_stream_bytes") != bytes_ingested:
            raise ValueError(
                f"fixture recorded bytes {fixture_receipt.get('actual_stream_bytes')} "
                f"mismatches client ingested bytes {bytes_ingested}"
            )
        if fixture_receipt.get("probes_served") != trials:
            raise ValueError(
                f"fixture probes served {fixture_receipt.get('probes_served')} "
                f"mismatches client trials {trials}"
            )
        if fixture_receipt.get("stream_bytes_start") != start_bytes:
            raise ValueError(
                f"fixture stream_bytes_start {fixture_receipt.get('stream_bytes_start')} "
                f"mismatches client start_bytes {start_bytes}"
            )
        if fixture_receipt.get("stream_bytes_end") != end_bytes:
            raise ValueError(
                f"fixture stream_bytes_end {fixture_receipt.get('stream_bytes_end')} "
                f"mismatches client end_bytes {end_bytes}"
            )
        if fixture_receipt.get("actual_stream_bytes") != (fixture_receipt.get("stream_bytes_end") - fixture_receipt.get("stream_bytes_start")):
            raise ValueError("fixture actual_stream_bytes does not equal stream_bytes_end - stream_bytes_start")
        start_ns = fixture_receipt.get("stream_start_ns")
        end_ns = fixture_receipt.get("stream_end_ns")
        if type(start_ns) is not int or type(end_ns) is not int or start_ns <= 0 or end_ns <= start_ns:
            raise ValueError(f"invalid fixture stream timestamps: start_ns={start_ns}, end_ns={end_ns}")
        fixture_dur_s = (end_ns - start_ns) / 1e9
        fixture_tp = (bytes_ingested / fixture_dur_s) / (1024.0 * 1024.0)
        if not math.isfinite(fixture_tp) or fixture_tp < 10.0:
            raise ValueError(f"fixture independently measured throughput {fixture_tp:.2f} MB/s below 10.0 MB/s requirement")


def validate_swarm_measurements(rows, finals, custody):
    """Recompute concurrent parsed-byte lower bounds; final writes are custody only."""
    def integer(value, name, minimum=0):
        if type(value) is not int or value < minimum:
            raise ValueError(f"invalid {name}")
        return value

    if len(rows) != 1402 or len(finals) != 5:
        raise ValueError("incomplete swarm measurement/fixture receipts")
    contract = rows[0]
    noise = contract.get("noise_panes", [])
    if (contract.get("event") != "contract" or contract.get("arm") != "swarm-echo"
            or contract.get("version") != 1 or contract.get("trials") != 1000
            or contract.get("p99_budget_us") != 50000 or contract.get("min_throughput_mb_s") != 10.0
            or contract.get("record_bytes") != len(noise_record(0)) or len(noise) != 4
            or any(type(pane) is not int or pane < 0 for pane in noise)
            or noise != sorted(set(noise)) or contract.get("pane_id") in noise):
        raise ValueError("invalid swarm contract")
    pane = integer(contract.get("pane_id"), "interactive pane")
    server = integer(contract.get("server_pid"), "server PID", 2)
    if custody.get("server_identity", {}).get("pid") != server:
        raise ValueError("swarm server custody mismatch")
    by_pane = {}
    for final in finals:
        fixture_pane = integer(final.get("pane_id"), "fixture pane")
        if fixture_pane in by_pane:
            raise ValueError("duplicate fixture receipt")
        by_pane[fixture_pane] = final
        role = "interactive" if fixture_pane == pane else "noise"
        chain = (custody.get("fixture_custody", []) if role == "interactive"
                 else custody.get("noise_fixture_custody", {}).get(str(fixture_pane), []))
        if (not chain or final.get("arm") != "swarm-echo" or final.get("status") != "completed"
                or final.get("role") != role or chain[0].get("pid") != final.get("pid")
                or chain[0].get("start_ticks") != str(final.get("linux_start_ticks"))
                or chain[-1].get("pid") != server
                or chain[-1].get("start_ticks") != custody["server_identity"].get("start_ticks")):
            raise ValueError("fixture final receipt lacks exact owned custody")
        if role == "interactive":
            if final.get("probes_served") != 1000:
                raise ValueError("quiet fixture did not serve all probes")
        else:
            generated = integer(final.get("records_generated"), "generated record count", 1)
            if (final.get("record_bytes") != len(noise_record(0)) or final.get("probes_served") != 0
                    or final.get("bytes_written") != generated * len(noise_record(0))):
                raise ValueError("noise final receipt byte accounting mismatch")
    if set(by_pane) != {pane, *noise}:
        raise ValueError("fixture pane set mismatch")
    index, previous_time = 1, 0
    sequences = {p: [] for p in noise}
    latencies = []
    for trial in range(1000):
        row = rows[index]
        index += 1
        start = integer(row.get("send_start_us"), "probe start")
        end = integer(row.get("observed_us"), "probe observation")
        latency = integer(row.get("latency_us"), "probe latency")
        if (row.get("event") != "swarm_echo_trial" or row.get("trial") != trial
                or row.get("nonce") != f"swarm_trial_{trial:04d}"
                or start < previous_time or end < start or latency != end - start
                or latency >= 5_000_000):
            raise ValueError("invalid ordered swarm probe")
        previous_time = end
        latencies.append(latency)
        if trial % 10 == 0:
            for noise_pane in noise:
                sample = rows[index]
                index += 1
                start = integer(sample.get("request_start_us"), "noise request")
                end = integer(sample.get("response_end_us"), "noise response")
                seq = integer(sample.get("last_complete_record"), "parsed record")
                prior = sequences[noise_pane]
                if (sample.get("event") != "noise_sample" or sample.get("after_trial") != trial
                        or sample.get("pane_id") != noise_pane or start < previous_time or end < start
                        or seq != parsed_noise_sequence(sample.get("tail"))
                        or (prior and seq < prior[-1])
                        or seq >= by_pane[noise_pane]["records_generated"]):
                    raise ValueError("stale, forged, or out-of-interval noise observation")
                prior.append(seq)
                previous_time = end
    complete = rows[index]
    duration = integer(complete.get("duration_us"), "measurement duration", 1)
    if (complete.get("event") != "complete" or complete.get("arm") != "swarm-echo"
            or complete.get("status") != "passed" or complete.get("trials") != 1000
            or duration < previous_time):
        raise ValueError("invalid swarm completion interval")
    deltas = [values[-1] - values[0] - 2 for values in sequences.values()]
    if any(delta <= 0 for delta in deltas):
        raise ValueError("insufficient fresh parsed noise records")
    ingested = sum(deltas) * len(noise_record(0))
    throughput = ingested * 1_000_000 / duration / (1024 * 1024)
    p99 = nearest_rank_percentile(sorted(latencies), 0.99)
    reported = complete.get("throughput_mb_s_lower_bound")
    if (ingested < 10 * 1024 * 1024 or throughput < 10 or p99 > 50000
            or complete.get("bytes_ingested_lower_bound") != ingested or complete.get("p99_echo_us") != p99
            or type(reported) not in (int, float) or not math.isfinite(reported)
            or abs(reported - throughput) > 1e-9 * max(1, throughput)):
        raise ValueError("swarm byte/latency acceptance or recomputed result mismatch")


def measure(args):
    """Run the bounded client against a newly created Linux-only mux instance."""
    for name, value in (("mux-socket-buffer-bytes", getattr(args, "mux_socket_buffer_bytes", None)),
                        ("mux-parser-buffer-bytes", getattr(args, "mux_parser_buffer_bytes", None))):
        if value is not None:
            if not isinstance(value, int) or isinstance(value, bool) or not 4096 <= value <= 1048576:
                raise ValueError(f"{name} must be an integer between 4096 and 1048576 bytes")
    if not pathlib.Path("/proc/self/stat").exists():
        raise RuntimeError("remote measurement requires the Linux /proc identity contract")
    if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
        raise RuntimeError("owned-process cleanup requires Linux pidfd support")
    # API presence does not establish kernel support or permission.
    pidfd = os.pidfd_open(os.getpid())
    try:
        signal.pidfd_send_signal(pidfd, 0)
    finally:
        os.close(pidfd)
    binary_dir = pathlib.Path(args.bin_dir).resolve(strict=True)
    client = pathlib.Path(args.client).resolve(strict=True)
    root = pathlib.Path(args.artifact_dir).absolute()
    if root.exists():
        raise FileExistsError("measurement artifacts must use a new directory")
    socket = root / "mux.sock"
    if len(str(socket).encode()) > 90:
        raise ValueError("private Unix socket path exceeds 90 bytes")
    binaries = [binary_dir / name for name in
                ("ft", "frankenterm-mux-server", "frankenterm-pty-guardian")]
    for binary in [*binaries, client]:
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise ValueError(f"missing executable: {binary}")
    # Compilation and idle-CPU admission are separate from successful build admission.
    for proc in pathlib.Path("/proc").glob("[0-9]*/comm"):
        try:
            if proc.read_bytes().strip() in {b"cargo", b"rustc", b"cc1", b"cc1plus"}:
                raise RuntimeError(f"measurement host is compiling: {proc.parent.name}")
        except FileNotFoundError:
            pass
    def cpu_sample():
        row = pathlib.Path("/proc/stat").read_text().splitlines()[0].split()[1:9]
        counters = list(map(int, row))
        return sum(counters), counters[3] + counters[4]
    first = cpu_sample()
    time.sleep(1)
    last = cpu_sample()
    total, idle = last[0] - first[0], last[1] - first[1]
    if total <= 0 or idle / total < 0.95:
        raise RuntimeError("measurement host did not reach 95% CPU idle during admission")
    root.mkdir(mode=0o700)
    for name in ("home", "config", "cache", "data", "state", "runtime", "tmp"):
        (root / name).mkdir(mode=0o700)
    corpus_path = root / "corpus.txt"
    write_new(corpus_path, corpus(10000))
    config_path = root / "frankenterm.toml"
    config_text = 'scrollback_lines = 50000\ninitial_rows = 24\ninitial_cols = 80\n'
    script = pathlib.Path(__file__).resolve()
    if args.arm == "swarm-echo":
        noise_command = [sys.executable, str(script), "run", "--arm", "swarm-echo",
                         "--swarm-role", "noise", "--corpus", str(corpus_path),
                         "--owner", str(root / "noise-owner"), "--timeout-seconds", "900"]
        config_text += "default_prog = " + json.dumps(noise_command) + "\n"
    if getattr(args, "mux_socket_buffer_bytes", None) is not None:
        config_text += f'mux_socket_buffer_size = {args.mux_socket_buffer_bytes}\n'
    if getattr(args, "mux_parser_buffer_bytes", None) is not None:
        config_text += f'mux_output_parser_buffer_size = {args.mux_parser_buffer_bytes}\n'
    config_text += ('[[unix_domains]]\nname = "owned-remote-profile"\n'
                    + 'socket_path = ' + json.dumps(str(socket))
                    + '\nno_serve_automatically = true\n')
    write_new(config_path, config_text)
    ft_config = root / "ft.toml"
    write_new(ft_config, '[vendored]\nmux_socket_path = ' + json.dumps(str(socket)) + '\n')
    env = {"PATH": f"{binary_dir}:/usr/bin:/bin", "LANG": "C.UTF-8",
           "HOME": str(root / "home"), "TMPDIR": str(root / "tmp"),
           "WEZTERM_UNIX_SOCKET": str(socket), "FRANKENTERM_UNIX_SOCKET": str(socket),
           "FRANKENTERM_CONFIG_FILE": str(config_path), "FT_WORKSPACE": str(root),
           "FT_WEZTERM_CLI": str(root / "external-cli-disabled"),
           "FT_REMOTE_MUX_PROFILE_WATCHDOG_SECONDS": "600"}
    if args.profile_phases:
        env["FT_REMOTE_MUX_PROFILE_PHASES"] = "1"
        # Headless muxes do not install a metrics recorder. The bounded,
        # content-free refusal and read timing summaries identify contention in this
        # explicitly instrumented arm; latency acceptance stays uninstrumented.
        env["RUST_LOG"] = "info,mux::metadata_refusal=debug,mux::line_read_timing=debug"
    for kind in ("CONFIG", "CACHE", "DATA", "STATE", "RUNTIME"):
        env[f"XDG_{kind}_HOME" if kind != "RUNTIME" else "XDG_RUNTIME_DIR"] = str(root / kind.lower())
    script = pathlib.Path(__file__).resolve()
    argv = [str(binaries[1]), "--config-file", str(config_path), "--daemonize=false",
            "--cwd", str(root), "--", sys.executable, str(script), "run"]
    if args.arm in ("echo", "swarm-echo"):
        argv.extend(["--arm", args.arm])
    argv.extend(["--corpus", str(corpus_path), "--owner", str(root / "pane-owner.json"),
                 "--timeout-seconds", "900"])
    owned = OwnedProcesses()
    server = None
    receipt = {"status": "failed", "scope": "private remote-host Unix socket only",
               "instrumented": args.profile_phases,
               "native_or_network_latency_proven": False, "environment": env,
               "mux_argv": argv, "cpu_idle_admission_fraction": idle / total,
               "binary_sha256": {str(p): file_sha256(p)
                                  for p in [*binaries, client]},
               "fixture_sha256": file_sha256(script)}
    if args.arm in ("echo", "swarm-echo"):
        receipt["arm"] = args.arm
    configuration_experiment = {}
    if getattr(args, "mux_socket_buffer_bytes", None) is not None:
        configuration_experiment["mux_socket_buffer_bytes"] = args.mux_socket_buffer_bytes
        configuration_experiment["mux_socket_buffer_size"] = args.mux_socket_buffer_bytes
    if getattr(args, "mux_parser_buffer_bytes", None) is not None:
        configuration_experiment["mux_parser_buffer_bytes"] = args.mux_parser_buffer_bytes
        configuration_experiment["mux_output_parser_buffer_size"] = args.mux_parser_buffer_bytes
    if configuration_experiment:
        receipt["configuration_experiment"] = configuration_experiment
    write_new(root / "env.json", json.dumps(receipt, indent=2) + "\n")
    try:
        with (root / "mux.stdout").open("xb") as stdout, (root / "mux.stderr").open("xb") as stderr:
            server = subprocess.Popen(argv, cwd=root, env=env, stdout=stdout, stderr=stderr,
                                      start_new_session=True)
        identity = process_identity(server.pid)
        if identity is None:
            raise RuntimeError("owned mux exited before identity capture")
        owned.retain(identity)
        receipt["server_identity"] = identity
        deadline = time.monotonic() + 120
        panes = None
        while time.monotonic() < deadline:
            if server.poll() is not None:
                raise RuntimeError(f"owned mux exited during startup: {server.returncode}")
            owned.remember_descendants()
            lease = pathlib.Path(str(socket) + ".lock")
            if socket.exists() and lease.exists() and f"pid={server.pid}" in lease.read_text().split():
                query = subprocess.run([str(binaries[0]), "-c", str(ft_config), "list", "--json"],
                                       cwd=root, env=env, capture_output=True, timeout=15)
                if query.returncode == 0:
                    panes = json.loads(query.stdout)
                    if len(panes) != 1:
                        raise RuntimeError("owned mux did not expose exactly one pane")
                    # PTY readiness receipt follows the bounded corpus write; mux parse
                    # convergence is checked separately by the persistent client's oracle.
                    if (root / "pane-owner.json.ready").exists():
                        break
            time.sleep(0.1)
        else:
            raise TimeoutError("owned mux and corpus startup deadline expired")
        owner = json.loads((root / "pane-owner.json").read_text())
        ready = json.loads((root / "pane-owner.json.ready").read_text())
        if args.arm in ("echo", "swarm-echo"):
            if owner.get("arm") != args.arm or ready.get("arm") != args.arm:
                raise RuntimeError("fixture readiness/echo receipt mismatch")
        else:
            expected_digest = file_sha256(corpus_path)
            if owner.get("corpus_sha256") != expected_digest or ready.get("corpus_sha256") != expected_digest:
                raise RuntimeError("fixture readiness/corpus receipt mismatch")
        owned.remember_descendants()
        receipt["fixture_custody"] = owned.require_fixture(owner, server.pid, binaries[2].resolve())
        receipt["owned_process_identities"] = list(owned.identities.values())
        if args.arm == "swarm-echo":
            client_argv = [str(client), str(socket), str(server.pid), str(panes[0]["pane_id"]),
                           str(panes[0]["tab_id"]), str(corpus_path), "1000", "swarm-echo"]
            with (root / "setup.jsonl").open("xb") as stdout, (root / "setup.stderr").open("xb") as stderr:
                setup = subprocess.run(client_argv, cwd=root, env=dict(env, FT_SWARM_SETUP_ONLY="1"),
                                       stdout=stdout, stderr=stderr, timeout=30)
            if setup.returncode != 0:
                raise RuntimeError("swarm pane setup failed")
            spawned = [json.loads(line) for line in (root / "setup.jsonl").read_text().splitlines()]
            noise_ids = [row.get("pane_id") for row in spawned]
            if (len(noise_ids) != 4 or any(type(pane) is not int for pane in noise_ids)
                    or len(set(noise_ids)) != 4 or panes[0]["pane_id"] in noise_ids
                    or any(row.get("event") != "swarm_spawn" for row in spawned)):
                raise RuntimeError("invalid swarm setup pane identities")
            receipt["noise_fixture_custody"] = {}
            deadline = time.monotonic() + 30
            for pane in noise_ids:
                path = root / f"noise-owner-{pane}.json"
                while not pathlib.Path(str(path) + ".ready").exists():
                    owned.remember_descendants()
                    if time.monotonic() >= deadline:
                        raise TimeoutError("noise readiness deadline expired")
                    time.sleep(0.05)
                noise_owner = json.loads(path.read_text())
                noise_ready = json.loads(pathlib.Path(str(path) + ".ready").read_text())
                if (noise_owner != noise_ready or noise_owner.get("pane_id") != pane
                        or noise_owner.get("arm") != "swarm-echo" or noise_owner.get("role") != "noise"):
                    raise RuntimeError("noise fixture identity/readiness mismatch")
                receipt["noise_fixture_custody"][str(pane)] = owned.require_fixture(
                    noise_owner, server.pid, binaries[2].resolve())
            receipt["owned_process_identities"] = list(owned.identities.values())
        elif args.arm == "echo":
            client_argv = [str(client), str(socket), str(server.pid), str(panes[0]["pane_id"]),
                           str(panes[0]["tab_id"]), str(corpus_path), "1000", "echo"]
        else:
            client_argv = [str(client), str(socket), str(server.pid), str(panes[0]["pane_id"]),
                           str(panes[0]["tab_id"]), str(corpus_path), "20"]
        custody_artifact = ({"interactive": receipt["fixture_custody"], "noise": receipt["noise_fixture_custody"]}
                            if args.arm == "swarm-echo" else receipt["fixture_custody"])
        write_new(root / "custody.json", json.dumps(custody_artifact, indent=2) + "\n")
        write_new(root / "panes.json", json.dumps(panes, indent=2) + "\n")
        receipt["client_argv"] = client_argv
        with (root / "trials.jsonl").open("xb") as stdout, (root / "client.stderr").open("xb") as stderr:
            trial = subprocess.run(client_argv, cwd=root, env=env, stdout=stdout, stderr=stderr, timeout=600)
        receipt["client_exit_code"] = trial.returncode
        with (root / "trials.jsonl").open("rb") as trace:
            trace_bytes = trace.read(2 * 1024 * 1024 + 1)
        if len(trace_bytes) > 2 * 1024 * 1024:
            raise ValueError("measurement trace exceeds 2 MiB receipt cap")
        rows = [json.loads(line) for line in trace_bytes.splitlines()]
        if trial.returncode != 0:
            raise RuntimeError("real mux baseline client failed")
        if args.arm == "swarm-echo":
            final_paths = [root / "pane-owner.json.final"] + [root / f"noise-owner-{pane}.json.final" for pane in noise_ids]
            deadline = time.monotonic() + 5
            while True:
                try:
                    finals = [json.loads(path.read_text()) for path in final_paths]
                    break
                except (FileNotFoundError, json.JSONDecodeError):
                    pass
                if time.monotonic() >= deadline:
                    raise TimeoutError("swarm final fixture receipts missing")
                time.sleep(0.05)
            validate_swarm_measurements(rows, finals, receipt)
            receipt["swarm_result"] = rows[-1]
            receipt["samples"] = 1000
            receipt["status"] = "passed"
        elif args.arm == "echo":
            receipt_file = root / "pane-owner.json.final"
            deadline = time.monotonic() + 5.0
            fixture_receipt = None
            last_err = None
            while time.monotonic() < deadline:
                if receipt_file.exists():
                    try:
                        content = receipt_file.read_text()
                        if content.strip():
                            fixture_receipt = json.loads(content)
                            break
                    except (json.JSONDecodeError, OSError) as e:
                        last_err = e
                time.sleep(0.05)
            if fixture_receipt is None:
                raise RuntimeError(f"fixture final receipt missing or invalid after 5s deadline: {last_err}")
            validate_echo_measurements(rows, fixture_receipt)
            receipt["fixture_actual_stream_bytes"] = fixture_receipt["actual_stream_bytes"]
            receipt["fixture_bytes_written"] = fixture_receipt["bytes_written"]
            receipt["fixture_probes_served"] = fixture_receipt["probes_served"]
            receipt["fixture_stream_bytes_start"] = fixture_receipt["stream_bytes_start"]
            receipt["fixture_stream_bytes_end"] = fixture_receipt["stream_bytes_end"]
            receipt["start_bytes"] = rows[-1]["start_bytes"]
            receipt["end_bytes"] = rows[-1]["end_bytes"]
            receipt["throughput_mb_s"] = rows[-1]["throughput_mb_s"]
            receipt["bytes_ingested"] = rows[-1]["bytes_ingested"]
            receipt["p99_echo_us"] = rows[-1]["p99_echo_us"]
            receipt["samples"] = rows[-1]["trials"]
            receipt["status"] = "passed"
        else:
            validate_measurements(rows, args.profile_phases)
            receipt["status"] = "passed"
            receipt["samples"] = 80
    except Exception as error:
        receipt["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        finish_measurement(root, receipt, owned, server)
    if receipt["status"] != "passed":
        raise RuntimeError("baseline or owned-process cleanup failed")


def test_swarm_receipt_contract():
    """Short deterministic receipts test verification, never live performance."""
    import copy
    import unittest

    class SwarmReceiptTests(unittest.TestCase):
        def setUp(self):
            self.custody = {"server_identity": {"pid": 50, "start_ticks": "10"},
                            "noise_fixture_custody": {}}
            self.finals = []
            for pane in range(5):
                identity = {"pid": 100 + pane, "start_ticks": str(20 + pane)}
                chain = [identity, self.custody["server_identity"]]
                if pane == 0:
                    self.custody["fixture_custody"] = chain
                else:
                    self.custody["noise_fixture_custody"][str(pane)] = chain
                self.finals.append({"pane_id": pane, "pid": 100 + pane,
                                    "linux_start_ticks": str(20 + pane), "arm": "swarm-echo",
                                    "status": "completed", "role": "interactive" if pane == 0 else "noise",
                                    "probes_served": 1000 if pane == 0 else 0,
                                    "records_generated": 100001, "record_bytes": len(noise_record(0)),
                                    "bytes_written": 100001 * len(noise_record(0))})
            self.rows = [{"event": "contract", "arm": "swarm-echo", "version": 1,
                          "trials": 1000, "noise_panes": [1, 2, 3, 4], "pane_id": 0,
                          "server_pid": 50, "p99_budget_us": 50000,
                          "min_throughput_mb_s": 10.0, "record_bytes": len(noise_record(0))}]
            clock = 0
            for trial in range(1000):
                self.rows.append({"event": "swarm_echo_trial", "trial": trial,
                                  "nonce": f"swarm_trial_{trial:04d}", "send_start_us": clock,
                                  "observed_us": clock + 100, "latency_us": 100})
                clock += 100
                if trial % 10 == 0:
                    for pane in range(1, 5):
                        self.rows.append({"event": "noise_sample", "after_trial": trial,
                                          "pane_id": pane, "request_start_us": clock,
                                          "response_end_us": clock + 100,
                                          "last_complete_record": trial * 100 + 1,
                                          "tail": noise_record(trial * 100 + 1).decode("ascii")})
                        clock += 100
            ingested = 4 * (99000 - 2) * len(noise_record(0))
            self.rows.append({"event": "complete", "arm": "swarm-echo", "status": "passed",
                              "trials": 1000, "duration_us": clock,
                              "bytes_ingested_lower_bound": ingested,
                              "throughput_mb_s_lower_bound": ingested * 1_000_000 / clock / (1024 * 1024),
                              "p99_echo_us": 100})

        def test_complete_contract(self):
            validate_swarm_measurements(self.rows, self.finals, self.custody)
            self.assertEqual(len(noise_record(0)), len(noise_record(999999)))
            self.assertLess(len(noise_record(0)), 80)
            self.assertEqual(parsed_noise_sequence(noise_record(41).decode().rstrip() + " " * 31 + "\n"), 41)

        def test_forged_bytes_latency_and_missing_custody(self):
            cases = [(0, "record_bytes", 128), (-1, "bytes_ingested_lower_bound", 2**63),
                     (-1, "p99_echo_us", 0), (1, "latency_us", 0),
                     (2, "last_complete_record", 100002), (2, "last_complete_record", 999),
                     (2, "response_end_us", 2**63)]
            for index, field, value in cases:
                with self.subTest(field=field):
                    rows = copy.deepcopy(self.rows)
                    rows[index][field] = value
                    with self.assertRaises(ValueError):
                        validate_swarm_measurements(rows, self.finals, self.custody)
            custody = copy.deepcopy(self.custody)
            custody["noise_fixture_custody"].pop("4")
            with self.assertRaises(ValueError):
                validate_swarm_measurements(self.rows, self.finals, custody)

        def test_stale_counter_and_final_bytes_cannot_manufacture_load(self):
            rows = copy.deepcopy(self.rows)
            for row in rows:
                if row.get("event") == "noise_sample":
                    row["last_complete_record"] = 1
                    row["tail"] = noise_record(1).decode("ascii")
            with self.assertRaises(ValueError):
                validate_swarm_measurements(rows, self.finals, self.custody)
            finals = copy.deepcopy(self.finals)
            finals[1]["bytes_written"] += 1
            with self.assertRaises(ValueError):
                validate_swarm_measurements(self.rows, finals, self.custody)

        def test_intermediate_plateau_preserves_interval_throughput(self):
            rows = copy.deepcopy(self.rows)
            # Both the parsed text and reported counter agree. The first and
            # final samples, timing, byte lower bound, and latency stay intact.
            for row in rows:
                if row.get("event") == "noise_sample" and row["after_trial"] == 10:
                    row["last_complete_record"] = 1
                    row["tail"] = noise_record(1).decode("ascii")
            validate_swarm_measurements(rows, self.finals, self.custody)

        def test_genuine_counter_regression_is_rejected(self):
            rows = copy.deepcopy(self.rows)
            for row in rows:
                if row.get("event") == "noise_sample" and row["after_trial"] == 20:
                    row["last_complete_record"] = 1000
                    row["tail"] = noise_record(1000).decode("ascii")
            with self.assertRaisesRegex(ValueError, "noise observation"):
                validate_swarm_measurements(rows, self.finals, self.custody)

        def test_insufficient_interval_throughput_is_rejected(self):
            rows = copy.deepcopy(self.rows)
            # Keep the reported result consistent with the measured bytes and
            # a longer interval; acceptance must fail on the actual rate.
            rows[-1]["duration_us"] *= 1000
            rows[-1]["throughput_mb_s_lower_bound"] /= 1000
            with self.assertRaisesRegex(ValueError, "acceptance"):
                validate_swarm_measurements(rows, self.finals, self.custody)

    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(SwarmReceiptTests))
    if not result.wasSuccessful():
        raise RuntimeError("swarm receipt contract tests failed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("prepare", "run", "measure", "self-test"))
    parser.add_argument("--corpus")
    parser.add_argument("--owner")
    parser.add_argument("--bin-dir")
    parser.add_argument("--client")
    parser.add_argument("--artifact-dir")
    parser.add_argument("--arm", choices=("resize", "echo", "swarm-echo"), default="resize",
                        help="diagnostic arm: default resize or opt-in echo (ft-4p4uo)")
    parser.add_argument("--swarm-role", choices=("interactive", "noise"), default="interactive")
    parser.add_argument("--profile-phases", action="store_true",
                        help="instrumentation arm only; emit sampler-clock phase intervals")
    parser.add_argument("--records", type=int, default=10000)
    parser.add_argument("--timeout-seconds", type=int, default=600)
    parser.add_argument("--mux-socket-buffer-bytes", type=int, default=None,
                        help="measure-only experimental config override: mux_socket_buffer_size (4096..1048576)")
    parser.add_argument("--mux-parser-buffer-bytes", type=int, default=None,
                        help="measure-only experimental config override: mux_output_parser_buffer_size (4096..1048576)")
    args = parser.parse_args()
    if not 1 <= args.records <= 10000:
        parser.error("records must be between 1 and 10000")
    if not 10 <= args.timeout_seconds <= 1800:
        parser.error("timeout must be between 10 and 1800 seconds")
    if args.mux_socket_buffer_bytes is not None and not 4096 <= args.mux_socket_buffer_bytes <= 1048576:
        parser.error("--mux-socket-buffer-bytes must be between 4096 and 1048576")
    if args.mux_parser_buffer_bytes is not None and not 4096 <= args.mux_parser_buffer_bytes <= 1048576:
        parser.error("--mux-parser-buffer-bytes must be between 4096 and 1048576")
    if args.mode != "measure":
        if args.mux_socket_buffer_bytes is not None or args.mux_parser_buffer_bytes is not None:
            parser.error("--mux-socket-buffer-bytes and --mux-parser-buffer-bytes are measure-only")
    if args.mode == "self-test":
        test_swarm_receipt_contract()
    elif args.mode == "measure":
        if not all((args.bin_dir, args.client, args.artifact_dir)):
            parser.error("measure requires --bin-dir, --client, --artifact-dir")
        measure(args)
    elif not args.corpus:
        parser.error("prepare/run require --corpus")
    elif args.mode == "prepare":
        write_new(args.corpus, corpus(args.records))
    else:
        if not args.owner:
            parser.error("run requires --owner for exact process identity")
        run(args)


if __name__ == "__main__":
    main()
