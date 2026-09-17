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
        # comm may contain spaces and parentheses; starttime is field 22.
        identity["linux_start_ticks"] = proc_stat.read_text().rsplit(")", 1)[1].split()[19]
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
        fields = pathlib.Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return {"pid": pid, "parent_pid": int(fields[1]), "start_ticks": fields[19], "state": fields[0]}
    except FileNotFoundError:
        return None


def measure(args):
    """Run the bounded client against a newly created Linux-only mux instance."""
    if not pathlib.Path("/proc/self/stat").exists():
        raise RuntimeError("remote measurement requires the Linux /proc identity contract")
    if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
        raise RuntimeError("owned-process cleanup requires Linux pidfd support")
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
            if proc.read_text().strip() in {"cargo", "rustc", "cc1", "cc1plus"}:
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
    for kind in ("CONFIG", "CACHE", "DATA", "STATE", "RUNTIME"):
        env[f"XDG_{kind}_HOME" if kind != "RUNTIME" else "XDG_RUNTIME_DIR"] = str(root / kind.lower())
    script = pathlib.Path(__file__).resolve()
    argv = [str(binaries[1]), "--config-file", str(config_path), "--daemonize=false",
            "--cwd", str(root), "--", sys.executable, str(script), "run",
            "--corpus", str(corpus_path), "--owner", str(root / "pane-owner.json"),
            "--timeout-seconds", "900"]
    identities = {}
    server = None
    receipt = {"status": "failed", "scope": "private remote-host Unix socket only",
               "native_or_network_latency_proven": False, "environment": env,
               "mux_argv": argv, "cpu_idle_admission_fraction": idle / total,
               "binary_sha256": {str(p): file_sha256(p)
                                  for p in [*binaries, client]},
               "fixture_sha256": file_sha256(script)}
    write_new(root / "env.json", json.dumps(receipt, indent=2) + "\n")
    def remember_descendants():
        if server is None:
            return
        candidates = []
        for entry in pathlib.Path("/proc").glob("[0-9]*"):
            identity = process_identity(int(entry.name))
            if identity:
                candidates.append(identity)
        changed = True
        while changed:
            changed = False
            for identity in candidates:
                parent = identities.get(identity["parent_pid"])
                if identity["pid"] in identities or parent is None:
                    continue
                live_parent = process_identity(parent["pid"])
                if live_parent and live_parent["start_ticks"] == parent["start_ticks"]:
                    identities[identity["pid"]] = identity
                    changed = True
    try:
        with (root / "mux.stdout").open("xb") as stdout, (root / "mux.stderr").open("xb") as stderr:
            server = subprocess.Popen(argv, cwd=root, env=env, stdout=stdout, stderr=stderr,
                                      start_new_session=True)
        identity = process_identity(server.pid)
        if identity is None:
            raise RuntimeError("owned mux exited before identity capture")
        identities[server.pid] = identity
        receipt["server_identity"] = identity
        deadline = time.monotonic() + 120
        panes = None
        while time.monotonic() < deadline:
            if server.poll() is not None:
                raise RuntimeError(f"owned mux exited during startup: {server.returncode}")
            remember_descendants()
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
        write_new(root / "panes.json", json.dumps(panes, indent=2) + "\n")
        client_argv = [str(client), str(socket), str(server.pid), str(panes[0]["pane_id"]),
                       str(panes[0]["tab_id"]), str(corpus_path), "20"]
        receipt["client_argv"] = client_argv
        with (root / "trials.jsonl").open("xb") as stdout, (root / "client.stderr").open("xb") as stderr:
            trial = subprocess.run(client_argv, cwd=root, env=env, stdout=stdout, stderr=stderr, timeout=600)
        receipt["client_exit_code"] = trial.returncode
        rows = [json.loads(line) for line in (root / "trials.jsonl").read_text().splitlines()]
        samples = [row for row in rows if row.get("event") == "trial"]
        if (trial.returncode != 0 or len(samples) != 80
                or any(row.get("status") != "passed" for row in samples)
                or not rows or rows[-1].get("event") != "complete"):
            raise RuntimeError("incomplete or failed 20-trial real mux baseline")
        receipt["status"] = "passed"
        receipt["samples"] = 80
    except Exception as error:
        receipt["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        remember_descendants()
        receipt["owned_process_identities"] = list(identities.values())
        # Address only exact descendant identities of this newly launched server.
        for identity in reversed(list(identities.values())):
            try:
                pidfd = os.pidfd_open(identity["pid"])
            except ProcessLookupError:
                continue
            try:
                current = process_identity(identity["pid"])
                if current and current["start_ticks"] == identity["start_ticks"]:
                    try:
                        signal.pidfd_send_signal(pidfd, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
            finally:
                os.close(pidfd)
        if server is not None:
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                receipt["cleanup_error"] = "owned mux did not exit after SIGTERM; no force kill attempted"
                receipt["status"] = "failed"
        survivors = []
        for identity in identities.values():
            current = process_identity(identity["pid"])
            if (current and current["start_ticks"] == identity["start_ticks"]
                    and current["state"] != "Z"):
                survivors.append(current)
        if survivors:
            receipt["cleanup_survivors"] = survivors
            receipt["status"] = "failed"
        write_new(root / "receipt.json", json.dumps(receipt, indent=2) + "\n")
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
