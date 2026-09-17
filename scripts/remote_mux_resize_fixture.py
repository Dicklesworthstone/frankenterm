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
    with pathlib.Path(path).open("x", encoding="utf-8", newline="") as output:
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


def measure(args):
    """Run the bounded client against a newly created Linux-only mux instance."""
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
    write_new(config_path, 'scrollback_lines = 50000\ninitial_rows = 24\ninitial_cols = 80\n'
              + '[[unix_domains]]\nname = "owned-remote-profile"\n'
              + 'socket_path = ' + json.dumps(str(socket))
              + '\nno_serve_automatically = true\n')
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
    for kind in ("CONFIG", "CACHE", "DATA", "STATE", "RUNTIME"):
        env[f"XDG_{kind}_HOME" if kind != "RUNTIME" else "XDG_RUNTIME_DIR"] = str(root / kind.lower())
    script = pathlib.Path(__file__).resolve()
    argv = [str(binaries[1]), "--config-file", str(config_path), "--daemonize=false",
            "--cwd", str(root), "--", sys.executable, str(script), "run",
            "--corpus", str(corpus_path), "--owner", str(root / "pane-owner.json"),
            "--timeout-seconds", "900"]
    owned = OwnedProcesses()
    server = None
    receipt = {"status": "failed", "scope": "private remote-host Unix socket only",
               "instrumented": args.profile_phases,
               "native_or_network_latency_proven": False, "environment": env,
               "mux_argv": argv, "cpu_idle_admission_fraction": idle / total,
               "binary_sha256": {str(p): file_sha256(p)
                                  for p in [*binaries, client]},
               "fixture_sha256": file_sha256(script)}
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
        expected_digest = file_sha256(corpus_path)
        if owner.get("corpus_sha256") != expected_digest or ready.get("corpus_sha256") != expected_digest:
            raise RuntimeError("fixture readiness/corpus receipt mismatch")
        owned.remember_descendants()
        receipt["fixture_custody"] = owned.require_fixture(owner, server.pid, binaries[2].resolve())
        receipt["owned_process_identities"] = list(owned.identities.values())
        write_new(root / "custody.json", json.dumps(receipt["fixture_custody"], indent=2) + "\n")
        write_new(root / "panes.json", json.dumps(panes, indent=2) + "\n")
        client_argv = [str(client), str(socket), str(server.pid), str(panes[0]["pane_id"]),
                       str(panes[0]["tab_id"]), str(corpus_path), "20"]
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("prepare", "run", "measure"))
    parser.add_argument("--corpus")
    parser.add_argument("--owner")
    parser.add_argument("--bin-dir")
    parser.add_argument("--client")
    parser.add_argument("--artifact-dir")
    parser.add_argument("--profile-phases", action="store_true",
                        help="instrumentation arm only; emit sampler-clock phase intervals")
    parser.add_argument("--records", type=int, default=10000)
    parser.add_argument("--timeout-seconds", type=int, default=600)
    args = parser.parse_args()
    if not 1 <= args.records <= 10000:
        parser.error("records must be between 1 and 10000")
    if not 10 <= args.timeout_seconds <= 1800:
        parser.error("timeout must be between 10 and 1800 seconds")
    if args.mode == "measure":
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
