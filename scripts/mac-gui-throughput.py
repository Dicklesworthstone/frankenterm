#!/usr/bin/env python3
"""GUI end-to-end PTY-drain and FPS harness (ft-yccm0.1.4, plan label M.3).

Run through scripts/mac-gui-throughput.sh; `--help` lists every option.

What one run measures, identically for every arm (FrankenTerm, Ghostty.app,
or a second FrankenTerm build/config for A/B gates):

* The operator's own method: the pane runs `time cat <corpus>` in `zsh -f`
  and the harness keeps zsh's `time` line (user, system, cpu%, total).
* The drain curve: cat's write offset on the tty (proc_pidfdinfo, the number
  `lsof -o` prints as 0t<bytes>) sampled every --sample-ms until cat exits,
  with the exit time taken from kqueue. Gives MB/s and stall episodes.
* FPS, terminal-agnostic: ScreenCaptureKit captures the measured window
  (scripts/mac-gui-frame-meter.swift) and counts frames whose content
  changed, over the drain window. FrankenTerm also publishes its own
  presented-frame count (the frames section of its resource snapshot), which
  validates the meter.
* Beach balls: the window's main thread is asked for its window list over
  the Accessibility API every --probe-ms; a request outstanding for
  --hang-ms (default 2 s, the spinning-cursor threshold) is a beach ball and
  triggers `sample`.
* Footprint after the run (scripts/mac-gui-footprint.sh capture) and, on the
  first run of each arm, the cell width the terminal gives every emoji in the
  T0 pool (DSR cursor reports), since wrapping changes the work done.

Arms interleave ABBA (A B B A per --reps pair). A verdict is refused when
either arm's coefficient of variation exceeds --cv-max percent, when
FrankenTerm's max_fps throttle would cap it below the display refresh rate,
when the screen-capture meter disagrees with FrankenTerm's own present count,
or when Ghostty is not pinned.

Safety: FrankenTerm runs under a fully replaced environment (its own HOME,
XDG dirs and TMPDIR; no WEZTERM_*, FRANKENTERM_* or FT_* variable from the
caller) with --always-new-process, after a read-only `ft list` probe under
that environment finds no mux. Every process the harness signals is one it
started, identified by exact PID and checked against its command line first.
"""

import argparse
import ctypes
import datetime
import hashlib
import json
import math
import os
import re
import select
import shlex
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time

SCHEMA = "frankenterm.gui_throughput_receipt.v1"
SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
METER_SOURCE = os.path.join(SCRIPT_DIR, "mac-gui-frame-meter.swift")
FOOTPRINT = os.path.join(SCRIPT_DIR, "mac-gui-footprint.sh")
REPO_ROOT = os.path.dirname(SCRIPT_DIR)
GHOSTTY_CONTRACT = os.path.join(REPO_ROOT, "docs", "perf", "incumbents", "ghostty.md")

# Scoreboard rows -> M.1 generator corpus names (frankenterm/term/benches/ingest).
CORPORA = {
    "T0": "color_emoji_random",
    "T1": "color_random",
    "T2": "seq_lines",
    "T3": "long_lines",
    "T4": "unicode_mix",
    "T5": "tui_repaint",
}
# The operator's color-emoji-random.bin: 30M frames.
OPERATOR_T0_BYTES = 749_801_000
DEFAULT_SIZES = {"color_emoji_random": OPERATOR_T0_BYTES}
DEFAULT_SIZE = 256 * 1024 * 1024
OPERATOR_FONT = "Pragmasevka Nerd Font"
PROTECTED_PIDS = {47759}  # the operator's FrankenTerm (standing orders)
SCK_COMPLETE = 0  # SCFrameStatus.complete
PROC_PIDFDVNODEINFO = 1

LOG_HANDLE = None


def log(message):
    line = f"{datetime.datetime.now(datetime.timezone.utc).strftime('%H:%M:%S.%f')[:-3]}Z {message}"
    print(line, file=sys.stderr, flush=True)
    if LOG_HANDLE is not None:
        LOG_HANDLE.write(line + "\n")
        LOG_HANDLE.flush()


def die(message, code=2):
    log(f"FATAL: {message}")
    sys.exit(code)


def uptime_ns():
    return time.clock_gettime_ns(time.CLOCK_UPTIME_RAW)


def loadavg():
    text = subprocess.run(["sysctl", "-n", "vm.loadavg"], capture_output=True, text=True).stdout
    return [float(value) for value in text.strip("{} \n").split()]


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def parse_size(text):
    match = re.fullmatch(r"\s*(\d+)\s*([A-Za-z]*)\s*", text)
    if not match:
        raise argparse.ArgumentTypeError(f"bad size {text!r}")
    units = {"": 1, "b": 1, "k": 1 << 10, "kib": 1 << 10, "m": 1 << 20, "mib": 1 << 20,
             "g": 1 << 30, "gib": 1 << 30, "kb": 10**3, "mb": 10**6, "gb": 10**9}
    unit = match.group(2).lower()
    if unit not in units:
        raise argparse.ArgumentTypeError(f"bad size unit in {text!r}")
    return int(match.group(1)) * units[unit]


def percentile(values, fraction):
    """Nearest-rank percentile of a non-empty list."""
    ordered = sorted(values)
    rank = max(1, math.ceil(fraction * len(ordered)))
    return ordered[rank - 1]


def summary(values):
    if not values:
        return None
    mean = statistics.fmean(values)
    out = {"n": len(values), "values": values, "mean": mean, "median": statistics.median(values),
           "min": min(values), "max": max(values)}
    out["cv_pct"] = (statistics.stdev(values) / mean * 100) if len(values) > 1 and mean else 0.0
    return out


# --------------------------------------------------------------------------
# Processes

_LIBSYSTEM = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
_LIBSYSTEM.proc_pidfdinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
_LIBSYSTEM.proc_pidfdinfo.restype = ctypes.c_int


def fd_offset(pid, fd):
    """The file offset of `fd` in `pid` (struct proc_fileinfo.fi_offset), or None."""
    buffer = ctypes.create_string_buffer(4096)
    filled = _LIBSYSTEM.proc_pidfdinfo(pid, fd, PROC_PIDFDVNODEINFO, buffer, len(buffer))
    if filled < 16:
        return None
    return struct.unpack_from("<q", buffer.raw, 8)[0]


def lsof_offset(pid, fd):
    """Cross-check of fd_offset through lsof, which prints 0t<bytes>."""
    result = subprocess.run(["lsof", "-n", "-P", "-w", "-a", "-p", str(pid), "-d", str(fd), "-o", "-o", "0", "-F", "o"],
                            capture_output=True, text=True)
    for line in result.stdout.splitlines():
        if line.startswith("o0t"):
            return int(line[3:])
    return None


def ps_field(pid, field):
    # -ww: never truncate a command line, which the exact-PID checks read.
    result = subprocess.run(["ps", "-ww", "-o", f"{field}=", "-p", str(pid)], capture_output=True, text=True)
    return result.stdout.strip() if result.returncode == 0 else None


def alive(pid):
    return ps_field(pid, "pid") is not None


def ancestors(pid):
    chain = []
    seen = set()
    while pid and pid > 1 and pid not in seen:
        seen.add(pid)
        chain.append(pid)
        parent = ps_field(pid, "ppid")
        pid = int(parent) if parent and parent.isdigit() else 0
    return chain


def terminate_exact(pid, token, label, grace=10.0):
    """SIGTERM then SIGKILL `pid`, only if it is still the process we started:
    its command line must contain `token` (a path inside this run)."""
    if not isinstance(pid, int) or pid <= 1 or pid in PROTECTED_PIDS:
        log(f"refusing to signal pid {pid!r} ({label})")
        return
    command = ps_field(pid, "command")
    if command is None:
        return
    if token not in command:
        log(f"not signalling pid {pid} ({label}): its command line no longer carries {token}")
        return
    os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + grace
    while time.monotonic() < deadline:
        if not alive(pid):
            log(f"{label} pid {pid} exited after SIGTERM")
            return
        time.sleep(0.2)
    if token in (ps_field(pid, "command") or ""):
        log(f"{label} pid {pid} ignored SIGTERM for {grace:.0f} s; SIGKILL")
        os.kill(pid, signal.SIGKILL)


def wait_for(predicate, timeout, interval=0.02):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(interval)
    return None


def read_text(path):
    try:
        with open(path) as handle:
            return handle.read()
    except OSError:
        return None


def touch(path):
    with open(path, "w") as handle:
        handle.write(f"{uptime_ns()}\n")


# --------------------------------------------------------------------------
# Generated files

PANE_SCRIPT = r"""#!/bin/zsh -f
# Pane workload of scripts/mac-gui-throughput.sh (ft-yccm0.1.4). Generated per
# run directory; never edit it while a run is going.
#   measure DIR CORPUS WIDTHS(0|1) PYTHON PROBE
#   flood   DIR CORPUS
zmodload zsh/zselect zsh/system
role=$1 dir=$2 corpus=$3
if [[ $role == flood ]]; then
  print -r -- $$ > $dir/flood.pid
  while [[ ! -e $dir/flood.go ]]; do [[ -e $dir/stop ]] && exit 0; zselect -t 2; done
  integer passes=0
  while [[ ! -e $dir/stop ]]; do
    cat -- $corpus
    (( passes++ ))
    print -r -- $passes > $dir/flood.passes
  done
  exit 0
fi
widths=$4 python=$5 probe=$6
print -r -- $$ > $dir/shell.pid.tmp && mv $dir/shell.pid.tmp $dir/shell.pid
print -rn -- $'\e[0m\e[2J\e[H'
print -r -- "ft-gui-throughput: waiting for go"
while [[ ! -e $dir/go ]]; do [[ -e $dir/stop ]] && exit 0; zselect -t 1; done
# The operator's method, time cat, in a zsh whose stderr is the time file; the
# subshell writes its own pid and execs cat, so cat's pid is known up front.
zsh -f -c 'zmodload zsh/system; time ( print -r -- $sysparams[pid] > "$1.tmp" && mv "$1.tmp" "$1"; exec cat -- "$2" ); exit $?' \
  zsh $dir/cat.pid $corpus 2> $dir/time.txt
print -r -- $? > $dir/done.tmp && mv $dir/done.tmp $dir/done
if [[ $widths == 1 ]]; then
  $python -I $probe $dir/widths.json
fi
print -r -- ok > $dir/widths.done
while [[ ! -e $dir/stop ]]; do zselect -t 20; done
"""

WIDTH_PROBE = r'''"""Emoji cell widths by DSR cursor reports (ft-yccm0.1.4); runs in the pane."""
import json, os, re, select, sys, termios, time, tty

RANGES = ((0x1F600, 0x1F64F), (0x1F300, 0x1F5FF), (0x1F680, 0x1F6FF),
          (0x1F900, 0x1F9FF), (0x1FA70, 0x1FAFF))
REPLY = re.compile(rb"\x1b\[(\d+);(\d+)R")
started = time.monotonic()
fd = os.open("/dev/tty", os.O_RDWR)
saved = termios.tcgetattr(fd)
widths, failed = {}, []
codepoints = [cp for low, high in RANGES for cp in range(low, high + 1)]
try:
    tty.setraw(fd)
    os.write(fd, b"\x1b[0m\x1b[2J\x1b[H")
    for start in range(0, len(codepoints), 32):
        batch = codepoints[start:start + 32]
        os.write(fd, b"".join(b"\r" + chr(cp).encode() + b"\x1b[6n" for cp in batch) + b"\r\x1b[K")
        received = b""
        deadline = time.monotonic() + 5
        while len(REPLY.findall(received)) < len(batch) and time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                received += os.read(fd, 4096)
        columns = [int(match[1]) for match in REPLY.findall(received)]
        for cp, column in zip(batch, columns):
            widths["%X" % cp] = column - 1
        failed.extend("%X" % cp for cp in batch[len(columns):])
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, saved)
    os.write(fd, b"\r\x1b[K")
histogram = {}
for width in widths.values():
    histogram[str(width)] = histogram.get(str(width), 0) + 1
with open(sys.argv[1], "w") as handle:
    json.dump({"widths": widths, "failed": failed, "histogram": histogram,
               "seconds": round(time.monotonic() - started, 3)}, handle, sort_keys=True)
'''


def lua_string(text):
    return "'" + text.replace("\\", "\\\\").replace("'", "\\'") + "'"


def lua_list(items):
    return "{ " + ", ".join(lua_string(item) for item in items) + " }"


def ft_lua_config(arm, args, max_fps, sibling_args):
    cols = args.cols * 2 + 1 if sibling_args else args.cols
    lines = [
        "-- Generated by scripts/mac-gui-throughput.sh (ft-yccm0.1.4); do not edit.",
        "local ft = require 'frankenterm'",
        "local config = ft.config_builder()",
        f"config.font = ft.font({{ family = {lua_string(args.font_family)} }})",
        f"config.font_size = {args.font_size}",
        f"config.initial_cols = {cols}",
        f"config.initial_rows = {args.rows}",
        f"config.max_fps = {max_fps}",
        f"config.front_end = {lua_string(args.ft_front_end)}",
        "config.webgpu_power_preference = 'HighPerformance'",
        f"config.scrollback_lines = {args.ft_scrollback_lines}",
        # The operator's parser settings (their frankenterm.lua).
        "config.mux_output_parser_buffer_size = 512 * 1024",
        "config.mux_output_parser_coalesce_delay_ms = 3",
        "config.window_close_confirmation = 'NeverPrompt'",
        "config.check_for_updates = false",
        "config.automatically_reload_config = false",
    ]
    lines += arm["lua"]
    if sibling_args:
        # Pane 1 floods; pane 2, split to its right, runs the measured command.
        lines += [
            "ft.on('gui-startup', function(cmd)",
            f"  local _, flood, _ = ft.mux.spawn_window({{ args = {lua_list(sibling_args)} }})",
            "  flood:split({ direction = 'Right', size = 0.5, args = cmd.args })",
            "end)",
        ]
    lines.append("return config")
    return "\n".join(lines) + "\n"


# --------------------------------------------------------------------------
# Analysis (pure functions; --analysis-self-test exercises them)

def parse_time_line(text):
    """zsh's default TIMEFMT: '%J  %U user %S system %P cpu %*E total'."""
    if not text:
        return None
    match = re.search(r"([\d.]+)s user ([\d.]+)s system (\d+)% cpu ([\d:.]+) total", text)
    if not match:
        return None
    total = 0.0
    for part in match.group(4).split(":"):
        total = total * 60 + float(part)
    return {"raw": text.strip().splitlines()[-1], "user_s": float(match.group(1)),
            "system_s": float(match.group(2)), "cpu_pct": int(match.group(3)), "total_s": total}


def drain_metrics(samples, size, start_ns, exit_ns):
    """samples: [(uptime_ns, offset)] of cat's tty offset; start_ns is when
    cat's pid was seen, exit_ns when kqueue reported its exit."""
    result = {"bytes": size, "samples": len(samples)}
    if exit_ns is None or start_ns is None or exit_ns <= start_ns:
        return result
    seconds = (exit_ns - start_ns) / 1e9
    result["drain_s"] = seconds
    result["mb_s"] = size / 1e6 / seconds
    result["mib_s"] = size / (1 << 20) / seconds
    if len(samples) < 2:
        return result
    rates, episodes = [], []
    run_start = None
    points = list(samples) + [(exit_ns, size)]
    for (t0, b0), (t1, b1) in zip(points, points[1:]):
        if t1 <= t0:
            continue
        rates.append((b1 - b0) / 1e6 / ((t1 - t0) / 1e9))
        if b1 == b0:
            run_start = t0 if run_start is None else run_start
        elif run_start is not None:
            episodes.append((t0 - run_start) / 1e9 if t0 > run_start else 0.0)
            run_start = None
    if run_start is not None:
        episodes.append((points[-1][0] - run_start) / 1e9)
    # A stall episode spans consecutive samples without progress.
    episodes = [episode for episode in episodes if episode > 0]
    result["interval_mb_s"] = {"p5": percentile(rates, 0.05), "p50": percentile(rates, 0.5),
                               "p95": percentile(rates, 0.95)}
    result["stalls"] = {
        "episodes": len(episodes),
        "p50_s": percentile(episodes, 0.5) if episodes else 0.0,
        "p95_s": percentile(episodes, 0.95) if episodes else 0.0,
        "max_s": max(episodes) if episodes else 0.0,
        "total_s": sum(episodes),
        "episodes_ge_1s": sum(1 for episode in episodes if episode >= 1.0),
    }
    return result


INTERVAL_BUCKETS = (("1", 1.5), ("2", 2.5), ("3", 3.5), ("4-5", 5.5), ("6-10", 10.5), (">10", float("inf")))


def fps_metrics(frames, start_ns, end_ns, refresh_hz):
    """frames: meter frame records. Content-changed frames are SCK 'complete'
    frames (new window content) whose dirty area is not zero."""
    if not frames or end_ns <= start_ns:
        return None
    duration = (end_ns - start_ns) / 1e9

    def stamp(frame):
        return frame.get("display_ns") or frame.get("arrival_ns")

    changed, distinct = [], []
    previous_hash = None
    for frame in frames:
        if frame.get("status") != SCK_COMPLETE:
            continue
        if frame.get("dirty_fraction") == 0:
            continue
        moment = stamp(frame)
        if moment is None or not (start_ns <= moment <= end_ns):
            previous_hash = frame.get("hash", previous_hash)
            continue
        changed.append(moment)
        if frame.get("hash") != previous_hash:
            distinct.append(moment)
        previous_hash = frame.get("hash")
    changed.sort()
    period_ms = 1000.0 / refresh_hz if refresh_hz else 1000.0 / 60
    out = {"window_s": duration, "changed_frames": len(changed), "distinct_frames": len(distinct),
           "mean_fps": len(changed) / duration, "distinct_fps": len(distinct) / duration}
    whole_seconds = int(duration)
    if whole_seconds >= 1:
        bins = [0] * whole_seconds
        for moment in changed:
            index = int((moment - start_ns) / 1e9)
            if index < whole_seconds:
                bins[index] += 1
        out["per_second_fps"] = {"p5": percentile(bins, 0.05), "p50": percentile(bins, 0.5),
                                 "p95": percentile(bins, 0.95), "seconds": whole_seconds}
    edges = [start_ns] + changed + [end_ns]
    gaps_ms = [(b - a) / 1e6 for a, b in zip(edges, edges[1:])]
    out["longest_gap_ms"] = max(gaps_ms)
    intervals = [(b - a) / 1e6 for a, b in zip(changed, changed[1:])]
    histogram = {name: 0 for name, _ in INTERVAL_BUCKETS}
    for interval in intervals:
        for name, limit in INTERVAL_BUCKETS:
            if interval / period_ms <= limit:
                histogram[name] += 1
                break
    out["interval_histogram_refresh_periods"] = histogram
    out["interval_ms"] = ({"p50": percentile(intervals, 0.5), "p95": percentile(intervals, 0.95)}
                          if intervals else None)
    return out


def internal_present_fps(points, start_ns, end_ns):
    """points: [(uptime_ns at publish, presented_total)] from FrankenTerm's
    resource snapshots; linear interpolation at both ends of the window."""
    points = sorted(set(points))
    if len(points) < 2 or end_ns <= start_ns:
        return None

    def at(moment):
        before = [p for p in points if p[0] <= moment]
        after = [p for p in points if p[0] >= moment]
        if not before or not after:
            return None
        (t0, v0), (t1, v1) = before[-1], after[0]
        return v0 if t1 == t0 else v0 + (v1 - v0) * (moment - t0) / (t1 - t0)

    first, last = at(start_ns), at(end_ns)
    if first is None or last is None:
        return None
    return {"presented": last - first, "fps": (last - first) / ((end_ns - start_ns) / 1e9)}


def beachball_metrics(pings, hang_ms):
    if not pings:
        return None
    latencies = [ping["latency_ns"] / 1e6 for ping in pings]
    hangs = [latency for latency in latencies if latency >= hang_ms]
    return {"probes": len(pings), "p50_ms": percentile(latencies, 0.5), "p95_ms": percentile(latencies, 0.95),
            "max_ms": max(latencies), "beach_balls": len(hangs), "unresponsive_s": sum(hangs) / 1000,
            "ax_errors": sum(1 for ping in pings if ping.get("ax_error", 0) != 0)}


def width_parity(per_arm):
    """per_arm: {arm: widths dict}. Lists the codepoints the arms disagree on."""
    names = [name for name, widths in per_arm.items() if widths]
    if len(names) < 2:
        return {"compared": names, "disagreements": None}
    first, second = per_arm[names[0]], per_arm[names[1]]
    keys = sorted(set(first) | set(second), key=lambda key: int(key, 16))
    diffs = [{"codepoint": key, names[0]: first.get(key), names[1]: second.get(key)}
             for key in keys if first.get(key) != second.get(key)]
    return {"compared": names[:2], "codepoints": len(keys), "disagreements": len(diffs), "differences": diffs}


def throttle_admissible(max_fps, refresh_hz):
    """FrankenTerm's repaint throttle waits ceil(1000 / max_fps) ms between
    paints (config::frame_interval_for_max_fps); it must be shorter than one
    refresh period or it caps FrankenTerm below the display by construction."""
    interval_ms = math.ceil(1000 / max_fps)
    return interval_ms, interval_ms < 1000 / refresh_hz


def verdict(arms, metric, higher_is_better, cv_max, refusals):
    """arms: {name: [values]} with exactly two arms, A first."""
    names = list(arms)
    reasons = list(refusals)
    stats = {name: summary(values) for name, values in arms.items()}
    for name in names:
        if not stats[name]:
            reasons.append(f"{name} has no {metric} values")
        elif stats[name]["cv_pct"] > cv_max:
            reasons.append(f"{name} {metric} CV {stats[name]['cv_pct']:.1f}% > {cv_max}%")
    out = {"metric": metric, "arms": stats}
    if reasons:
        out["verdict"] = f"NO_ADMISSIBLE_RATIO ({'; '.join(reasons)})"
        return out
    a, b = (stats[name]["median"] for name in names)
    ratio = (b / a) if not higher_is_better else (a / b)
    out["ratio_a_over_b"] = ratio
    out["ratio_meaning"] = (f"{names[1]} {metric} / {names[0]} {metric}" if not higher_is_better
                            else f"{names[0]} {metric} / {names[1]} {metric}")
    out["verdict"] = f"{names[0]}_faster" if ratio > 1 else (f"{names[1]}_faster" if ratio < 1 else "tie")
    return out


RECEIPT_REQUIRED = {
    "schema": str, "started_utc": str, "finished_utc": str, "harness": dict, "fingerprint": dict,
    "corpus": dict, "geometry": dict, "arms": dict, "order": list, "runs": list, "aggregate": dict,
    "verdicts": dict, "width_parity": dict, "tcc": dict, "isolation": dict,
}
RUN_REQUIRED = {"index": int, "arm": str, "pids": dict, "load_start": list, "drain": dict,
                "time": (dict, type(None)), "fps": (dict, type(None)), "beachball": (dict, type(None)),
                "errors": list}


def validate_receipt(receipt):
    problems = []
    for key, kind in RECEIPT_REQUIRED.items():
        if not isinstance(receipt.get(key), kind):
            problems.append(f"receipt.{key} missing or not {kind}")
    if receipt.get("schema") != SCHEMA:
        problems.append("wrong schema")
    for run in receipt.get("runs") or []:
        for key, kind in RUN_REQUIRED.items():
            if not isinstance(run.get(key), kind):
                problems.append(f"runs[{run.get('index')}].{key} missing or not {kind}")
    for name in ("drain_total_s", "fps"):
        if "verdict" not in (receipt.get("verdicts") or {}).get(name, {}):
            problems.append(f"verdicts.{name}.verdict missing")
    return problems


def analysis_self_test():
    s = 1_000_000_000
    timing = parse_time_line("( ... )  0.01s user 5.30s system 12% cpu 41.827 total\n")
    assert timing == {"raw": "( ... )  0.01s user 5.30s system 12% cpu 41.827 total", "user_s": 0.01,
                      "system_s": 5.3, "cpu_pct": 12, "total_s": 41.827}, timing
    assert parse_time_line("x  1.00s user 2.00s system 50% cpu 1:02.500 total")["total_s"] == 62.5
    assert parse_time_line("garbage") is None
    # 10 MB in 10 s with one 2 s stall (two zero-progress samples at 1 s cadence).
    samples = [(0, 0), (1 * s, 1_000_000), (2 * s, 2_000_000), (3 * s, 2_000_000), (4 * s, 2_000_000),
               (5 * s, 4_000_000), (6 * s, 6_000_000), (7 * s, 7_000_000), (8 * s, 8_000_000), (9 * s, 9_000_000)]
    drain = drain_metrics(samples, 10_000_000, 0, 10 * s)
    assert drain["drain_s"] == 10 and drain["mb_s"] == 1.0, drain
    assert drain["stalls"]["episodes"] == 1 and drain["stalls"]["max_s"] == 2.0, drain["stalls"]
    assert drain["stalls"]["episodes_ge_1s"] == 1
    # 60 Hz for 2 s with a 250 ms hole, one idle frame and one zero-dirty frame.
    # (A display_ns of 0 means the meter got no display time, so the synthetic
    # clock starts at `base`.)
    period, base = s // 60, 100 * s
    frames = [{"status": 0, "display_ns": base + i * period, "hash": str(i), "dirty_fraction": 0.5}
              for i in range(120) if not (60 <= i < 75)]
    frames.append({"status": 1, "display_ns": base + 61 * period})
    frames.append({"status": 0, "display_ns": base + 76 * period + 1, "hash": "75", "dirty_fraction": 0})
    frames.append({"status": 0, "display_ns": 0, "arrival_ns": base + 5, "hash": "x", "dirty_fraction": 0.5})
    frames.sort(key=lambda frame: frame["display_ns"])
    fps = fps_metrics(frames, base, base + 2 * s, 60)
    # The frame without a display time counts at its arrival time.
    assert fps["changed_frames"] == 106 and abs(fps["mean_fps"] - 53.0) < 1e-9, fps
    assert fps["per_second_fps"]["p5"] == 45 and fps["per_second_fps"]["p95"] == 61, fps
    assert 249 < fps["longest_gap_ms"] < 268, fps["longest_gap_ms"]
    assert fps["interval_histogram_refresh_periods"]["1"] == 104, fps
    assert fps["interval_histogram_refresh_periods"]["6-10"] == 0
    assert fps["interval_histogram_refresh_periods"][">10"] == 1
    repeated = [{"status": 0, "display_ns": base + i * period, "hash": "same", "dirty_fraction": 0.1}
                for i in range(60)]
    assert fps_metrics(repeated, base, base + s, 60)["distinct_frames"] == 1
    internal = internal_present_fps([(0, 100), (s, 160), (2 * s, 220)], s // 2, 3 * s // 2)
    assert internal == {"presented": 60.0, "fps": 60.0}, internal
    assert internal_present_fps([(s, 1)], 0, s) is None
    pings = [{"latency_ns": 1_000_000}] * 98 + [{"latency_ns": 2_500_000_000, "ax_error": -25204}] * 2
    balls = beachball_metrics(pings, 2000)
    assert balls["beach_balls"] == 2 and balls["ax_errors"] == 2 and balls["unresponsive_s"] == 5.0, balls
    assert throttle_admissible(60, 60) == (17, False), "max_fps = refresh still caps below it"
    assert throttle_admissible(120, 60) == (9, True)
    assert throttle_admissible(240, 120) == (5, True)
    parity = width_parity({"ft": {"1F600": 2, "1FA70": 1}, "ghostty": {"1F600": 2, "1FA70": 2}})
    assert parity["disagreements"] == 1 and parity["differences"][0]["codepoint"] == "1FA70", parity
    drain_v = verdict({"ft": [10.0, 10.2], "ghostty": [20.0, 20.4]}, "total_s", False, 5.0, [])
    assert drain_v["verdict"] == "ft_faster" and abs(drain_v["ratio_a_over_b"] - 2.0) < 1e-9, drain_v
    noisy = verdict({"ft": [10.0, 14.0], "ghostty": [20.0, 20.0]}, "total_s", False, 5.0, [])
    assert noisy["verdict"].startswith("NO_ADMISSIBLE_RATIO (ft total_s CV"), noisy
    fps_v = verdict({"ft": [60.0, 59.0], "ghostty": [50.0, 50.0]}, "mean_fps", True, 5.0, [])
    assert fps_v["verdict"] == "ft_faster", fps_v
    refused = verdict({"ft": [1.0], "ghostty": [1.0]}, "mean_fps", True, 5.0, ["max_fps caps ft"])
    assert refused["verdict"] == "NO_ADMISSIBLE_RATIO (max_fps caps ft)", refused
    assert abc_order(2) == ["A", "B", "B", "A", "A", "B", "B", "A"][:4]
    assert abc_order(3) == ["A", "B", "B", "A", "A", "B"]
    problems = validate_receipt({"schema": SCHEMA})
    assert problems and any("runs" in problem for problem in problems), problems
    print("analysis self-test: ok")


def pty_self_test():
    """Runs the real pane script in a private pseudo-terminal that this process
    plays the terminal for: it drains at a capped rate with one deliberate
    stall and answers every cursor-position query as a 2-cell glyph. Checks
    the cat pid handoff, tty offset sampling (against lsof), the kqueue exit,
    zsh's time line and the width probe. No GUI and no real terminal window."""
    import pty
    import random

    work = tempfile.mkdtemp(prefix="ftgt-pty-")
    pane = os.path.join(work, "pane.zsh")
    probe = os.path.join(work, "width_probe.py")
    for path, text in ((pane, PANE_SCRIPT), (probe, WIDTH_PROBE)):
        with open(path, "w") as handle:
            handle.write(text)
    corpus = os.path.join(work, "corpus.bin")
    rng = random.Random(1)
    with open(corpus, "wb") as handle:
        handle.write(bytes(rng.randrange(0x20, 0x7F) for _ in range(4 << 20)))
    size = os.path.getsize(corpus)
    child, master = pty.fork()
    if child == 0:
        os.execv("/bin/zsh", ["/bin/zsh", "-f", pane, "measure", work, corpus, "1", sys.executable, probe])
    drained = {"bytes": 0, "queries": 0}
    stop = threading.Event()

    def terminal():
        # ~16 MB/s, and nothing at all for 600 ms once 1 MiB has arrived.
        stalled = False
        while not stop.is_set():
            ready, _, _ = select.select([master], [], [], 0.05)
            if not ready:
                continue
            try:
                data = os.read(master, 65536)
            except OSError:
                return
            drained["bytes"] += len(data)
            queries = data.count(b"\x1b[6n")
            if queries:
                drained["queries"] += queries
                os.write(master, b"\x1b[1;3R" * queries)
            if not stalled and drained["bytes"] > (1 << 20):
                stalled = True
                time.sleep(0.6)
            time.sleep(len(data) / 16e6)

    reader = threading.Thread(target=terminal, daemon=True)
    reader.start()
    try:
        assert wait_for(lambda: read_text(os.path.join(work, "shell.pid")), 20), "pane shell never started"
        touch(os.path.join(work, "go"))
        cat_text = wait_for(lambda: read_text(os.path.join(work, "cat.pid")), 20, 0.002)
        assert cat_text, "cat never started"
        cat_pid, start_ns = int(cat_text), uptime_ns()
        assert ps_field(cat_pid, "ppid") is not None
        harness = Harness.__new__(Harness)
        harness.args = argparse.Namespace(sample_ms=50, run_timeout=120)
        harness.corpus = {"bytes": size}
        record = {"errors": []}
        drain, exit_ns, samples = harness.sample_drain(cat_pid, start_ns, work, record)
        assert not record["errors"], record["errors"]
        cross = record["offset_cross_check"]
        assert cross["proc_pidfdinfo"] is not None and cross["lsof"] is not None, cross
        assert cross["lsof"] >= cross["proc_pidfdinfo"], cross
        assert len(samples) >= 5 and samples[-1][1] <= size, samples[-3:]
        assert all(b <= a for (_, b), (_, a) in zip(samples, samples[1:])), "offsets never go back"
        assert drain["drain_s"] > 0.5 and drain["stalls"]["max_s"] >= 0.3, drain
        assert wait_for(lambda: read_text(os.path.join(work, "done")), 20)
        assert read_text(os.path.join(work, "done")).strip() == "0", "cat exited non-zero"
        timing = parse_time_line(read_text(os.path.join(work, "time.txt")))
        assert timing and abs(timing["total_s"] - drain["drain_s"]) < 0.5, (timing, drain)
        assert wait_for(lambda: os.path.exists(os.path.join(work, "widths.done")), 60, 0.1)
        widths = json.loads(read_text(os.path.join(work, "widths.json")))
        assert not widths["failed"] and widths["histogram"] == {"2": 1376}, widths["histogram"]
        assert drained["queries"] == 1376, drained
    finally:
        touch(os.path.join(work, "stop"))
        stop.set()
        try:
            os.waitpid(child, 0)
        except ChildProcessError:
            pass
        os.close(master)
    print(f"pty self-test: ok ({size} bytes in {drain['drain_s']:.2f} s, {len(samples)} offset samples, "
          f"longest stall {drain['stalls']['max_s']:.2f} s, time line {timing['raw']!r})")
    # Only this test's own temporary directory; kept when an assertion fails.
    shutil.rmtree(work, ignore_errors=True)


def abc_order(reps):
    """ABBA ABBA ...: `reps` runs of each arm (A B, B A, A B, ...)."""
    order = []
    for index in range(reps):
        order += ["A", "B"] if index % 2 == 0 else ["B", "A"]
    return order


# --------------------------------------------------------------------------
# Harness

def build_meter(cache_dir):
    source_sha = sha256_file(METER_SOURCE)
    binary = os.path.join(cache_dir, f"frame-meter-{source_sha[:16]}")
    if not os.access(binary, os.X_OK):
        os.makedirs(cache_dir, exist_ok=True)
        log(f"compiling {METER_SOURCE} -> {binary}")
        staging = binary + ".tmp"
        result = subprocess.run(["xcrun", "swiftc", "-O", "-swift-version", "5", METER_SOURCE, "-o", staging],
                                capture_output=True, text=True)
        if result.returncode != 0:
            die(f"swiftc failed:\n{result.stderr}")
        os.replace(staging, binary)
    return binary, source_sha


def ghostty_version(app):
    binary = os.path.join(app, "Contents", "MacOS", "ghostty")
    result = subprocess.run([binary, "+version"], capture_output=True, text=True, timeout=30)
    match = re.search(r"version:\s*(\S+)", result.stdout)
    return binary, (match.group(1) if match else None), result.stdout


def ghostty_pin(args):
    if args.ghostty_pin:
        return args.ghostty_pin, "--ghostty-pin"
    text = read_text(GHOSTTY_CONTRACT)
    if text:
        match = re.search(r"ghostty-app-version:\s*`?([^\s`]+)", text)
        if match:
            return match.group(1), os.path.relpath(GHOSTTY_CONTRACT, REPO_ROOT)
    return None, None


def fingerprint(args, displays):
    def sysctl(name):
        return subprocess.run(["sysctl", "-n", name], capture_output=True, text=True).stdout.strip()

    sw = subprocess.run(["sw_vers"], capture_output=True, text=True).stdout
    power = subprocess.run(["pmset", "-g", "ps"], capture_output=True, text=True).stdout.splitlines()
    git = subprocess.run(["git", "-C", REPO_ROOT, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    return {
        "hw_model": sysctl("hw.model"), "cpu": sysctl("machdep.cpu.brand_string"), "ncpu": sysctl("hw.ncpu"),
        "perf_cores": sysctl("hw.perflevel0.physicalcpu"), "efficiency_cores": sysctl("hw.perflevel1.physicalcpu"),
        "memsize": sysctl("hw.memsize"), "os": " ".join(line.split(":", 1)[1].strip() for line in sw.splitlines() if ":" in line),
        "power_source": power[0] if power else None, "displays": displays, "python": sys.version.split()[0],
        "harness_repo_head": git or None,
    }


def corpus_spec(args, run_dir):
    if args.corpus_file:
        path = os.path.abspath(args.corpus_file)
        if not os.path.isfile(path):
            die(f"--corpus-file {path} is not a file")
        log(f"hashing {path} (read-only)")
        return {"source": "file", "path": path, "bytes": os.path.getsize(path), "sha256": sha256_file(path),
                "name": args.corpus_name, "label": args.corpus_label}
    size = args.size or DEFAULT_SIZES.get(args.corpus_name, DEFAULT_SIZE)
    if args.corpus_gen_bin:
        command = [args.corpus_gen_bin, "--gen-only", "--corpus", args.corpus_name, "--size", str(size),
                   "--corpus-dir", args.corpus_dir]
        if args.seed is not None:
            command += ["--seed", str(args.seed)]
        if args.dry_run:
            return {"source": "m1-generator", "path": None, "bytes": size, "sha256": None,
                    "name": args.corpus_name, "label": args.corpus_label, "would_run": shlex.join(command)}
        log(f"generating corpus: {shlex.join(command)}")
        result = subprocess.run(command, capture_output=True, text=True)
        if result.returncode != 0:
            die(f"corpus generator failed ({result.returncode}): {result.stderr.strip()}")
        record = json.loads(result.stdout.strip().splitlines()[-1])
        return {"source": "m1-generator", "path": record["corpus_path"], "bytes": record["bytes"],
                "sha256": record["corpus_sha256"], "seed": record["seed"], "name": args.corpus_name,
                "label": args.corpus_label, "generator": os.path.abspath(args.corpus_gen_bin),
                "generator_record": record}
    if not args.self_test or args.corpus_name != "color_emoji_random":
        if args.dry_run:
            return {"source": None, "path": None, "bytes": size, "sha256": None, "name": args.corpus_name,
                    "label": args.corpus_label, "missing": "--corpus-gen-bin or --corpus-file"}
        die("pass --corpus-gen-bin (the M.1 example: cargo build -p frankenterm-term --profile release-perf "
            "--example ingest_throughput) or --corpus-file")
    # --self-test only: an operator-format replica, not byte-identical to M.1.
    path = os.path.join(run_dir, "selftest-color-emoji.bin")
    import random

    rng = random.Random(20261005)
    pool = [chr(cp) for low, high in ((0x1F600, 0x1F64F), (0x1F300, 0x1F5FF), (0x1F680, 0x1F6FF),
                                      (0x1F900, 0x1F9FF), (0x1FA70, 0x1FAFF)) for cp in range(low, high + 1)]
    pool += [chr(cp) for cp in range(0x21, 0x7F) if chr(cp) not in "$\\"]
    with open(path, "wb") as handle:
        written = 0
        while written < size:
            frame = f"\x1b[38;5;{rng.randrange(256)}m\x1b[48;5;{rng.randrange(256)}m{rng.choice(pool)}".encode()
            handle.write(frame)
            written += len(frame)
    return {"source": "self-test-python-replica", "path": path, "bytes": os.path.getsize(path),
            "sha256": sha256_file(path), "name": args.corpus_name, "label": args.corpus_label}


class SnapshotPoller(threading.Thread):
    """Reads FrankenTerm's published resource snapshot while a run is going."""

    def __init__(self, path, interval):
        super().__init__(daemon=True)
        self.path, self.interval = path, interval
        self.points, self.last = [], None
        self.stop_event = threading.Event()

    def run(self):
        seen = None
        while not self.stop_event.is_set():
            text = read_text(self.path)
            now_ns, now_ms = uptime_ns(), time.time() * 1000
            if text:
                try:
                    snapshot = json.loads(text)
                except ValueError:
                    snapshot = None
                if snapshot and snapshot.get("published_unix_ms") != seen:
                    seen = snapshot.get("published_unix_ms")
                    published_ns = now_ns - int((now_ms - seen) * 1e6)
                    frames = snapshot.get("frames") or {}
                    self.points.append((published_ns, frames.get("presented_total", 0)))
                    self.last = snapshot
            self.stop_event.wait(self.interval)


def read_jsonl(path):
    records = []
    for line in (read_text(path) or "").splitlines():
        try:
            records.append(json.loads(line))
        except ValueError:
            pass
    return records


class Harness:
    def __init__(self, args):
        self.args = args
        self.run_dir = args.out
        self.token = f"ftgt-{os.path.basename(self.run_dir)}"
        self.iso_home = os.path.join(self.run_dir, "home")
        self.iso_env = {
            "HOME": self.iso_home,
            "USER": os.environ.get("USER", ""),
            "LOGNAME": os.environ.get("LOGNAME", os.environ.get("USER", "")),
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "LANG": "en_US.UTF-8",
            "TMPDIR": os.path.join(self.run_dir, "tmp") + "/",
            "XDG_CONFIG_HOME": os.path.join(self.iso_home, ".config"),
            "XDG_DATA_HOME": os.path.join(self.iso_home, ".local", "share"),
            "XDG_CACHE_HOME": os.path.join(self.iso_home, ".cache"),
            "XDG_RUNTIME_DIR": os.path.join(self.run_dir, "xdg-runtime"),
        }
        self.runtime_dirs = [os.path.join(self.iso_home, ".local", "share", "frankenterm"),
                             os.path.join(self.run_dir, "xdg-runtime", "frankenterm")]

    # -- setup ------------------------------------------------------------

    def prepare_dirs(self):
        for path in (self.iso_home, self.iso_env["XDG_CONFIG_HOME"], self.iso_env["XDG_DATA_HOME"],
                     self.iso_env["XDG_CACHE_HOME"], self.iso_env["XDG_RUNTIME_DIR"], self.iso_env["TMPDIR"],
                     os.path.join(self.run_dir, "runs")):
            os.makedirs(path, exist_ok=True)
        self.pane_script = os.path.join(self.run_dir, "pane.zsh")
        with open(self.pane_script, "w") as handle:
            handle.write(PANE_SCRIPT)
        os.chmod(self.pane_script, 0o755)
        self.width_probe = os.path.join(self.run_dir, "width_probe.py")
        with open(self.width_probe, "w") as handle:
            handle.write(WIDTH_PROBE)

    def isolation_probe(self):
        ft_bin = self.args.ft_bin or shutil.which("ft")
        if not ft_bin:
            log("isolation probe: no ft binary; relying on the replaced environment, isolated HOME and "
                "--always-new-process")
            return {"status": "skipped", "reason": "no ft binary"}
        workspace = os.path.join(self.run_dir, "ft-workspace")
        os.makedirs(workspace, exist_ok=True)
        result = subprocess.run([ft_bin, "list", "--json", "--workspace", workspace], cwd=self.run_dir,
                                env=self.iso_env, capture_output=True, text=True, timeout=120)
        record = {"ft_bin": ft_bin, "exit": result.returncode}
        if result.returncode == 0 and '"pane_id"' in result.stdout:
            with open(os.path.join(self.run_dir, "isolation-probe.txt"), "w") as handle:
                handle.write(result.stdout)
            die("isolation probe FAILED: ft under the harness environment sees panes; refusing to launch")
        log(f"isolation probe: no mux visible under the harness environment (ft exit {result.returncode})")
        record["status"] = "ok"
        return record

    # -- one run ----------------------------------------------------------

    def launch_ft(self, arm, run_dir, measure_args, sibling_args):
        lua = os.path.join(run_dir, "frankenterm.lua")
        with open(lua, "w") as handle:
            handle.write(ft_lua_config(arm, self.args, self.ft_max_fps, sibling_args))
        env = dict(self.iso_env)
        env["FT_RESOURCE_SNAPSHOT_INTERVAL_MS"] = str(self.args.snapshot_interval_ms)
        env.update(arm["env"])
        command = [arm["gui_bin"], "--config-file", lua, "start", "--always-new-process", "--"] + measure_args
        log(f"launch {arm['name']}: {shlex.join(command)}")
        with open(os.path.join(run_dir, "gui.stdout"), "w") as out, open(os.path.join(run_dir, "gui.stderr"), "w") as err:
            process = subprocess.Popen(command, env=env, cwd=run_dir, stdout=out, stderr=err, start_new_session=True)
        return process.pid, lua

    def launch_ghostty(self, arm, run_dir, measure_args):
        flags = [
            "--config-default-files=false",
            f"--font-family={self.args.font_family}",
            f"--font-size={self.args.font_size}",
            f"--window-width={self.args.cols}",
            f"--window-height={self.args.rows}",
            "--window-save-state=never",
            "--confirm-close-surface=false",
            "--quit-after-last-window-closed=true",
            "--auto-update=off",
            f"--title={self.token}-{os.path.basename(run_dir)}",
        ]
        if self.args.ghostty_scrollback_bytes:
            flags.append(f"--scrollback-limit={self.args.ghostty_scrollback_bytes}")
        flags += self.args.ghostty_arg
        if self.args.ghostty_launch == "open":
            command = ["open", "-n", "-a", self.args.ghostty_app, "--args"] + flags + ["-e"] + measure_args
        else:
            command = [self.ghostty_binary] + flags + ["-e"] + measure_args
        log(f"launch ghostty: {shlex.join(command)}")
        with open(os.path.join(run_dir, "gui.stdout"), "w") as out, open(os.path.join(run_dir, "gui.stderr"), "w") as err:
            process = subprocess.Popen(command, cwd=run_dir, stdout=out, stderr=err, start_new_session=True)
        if self.args.ghostty_launch == "open":
            process.wait(timeout=60)
            return None, flags
        return process.pid, flags

    def run_once(self, index, arm, width_probe):
        args = self.args
        run_dir = os.path.join(self.run_dir, "runs", f"{index:02d}-{arm['name']}")
        os.makedirs(run_dir, exist_ok=True)
        record = {"index": index, "arm": arm["name"], "kind": arm["kind"], "run_dir": run_dir, "pids": {},
                  "errors": [], "drain": {}, "time": None, "fps": None, "beachball": None,
                  "load_start": loadavg()}
        measure_args = ["/bin/zsh", "-f", self.pane_script, "measure", run_dir, self.corpus["path"],
                        "1" if width_probe else "0", sys.executable, self.width_probe]
        sibling_args = (["/bin/zsh", "-f", self.pane_script, "flood", run_dir, self.flood_corpus["path"]]
                        if args.sibling_flood else None)
        helpers, poller, terminal_pid, token = [], None, None, run_dir
        try:
            if arm["kind"] == "ft":
                terminal_pid, lua = self.launch_ft(arm, run_dir, measure_args, sibling_args)
                record["config_file"] = lua
            else:
                terminal_pid, record["ghostty_flags"] = self.launch_ghostty(arm, run_dir, measure_args)
            shell_text = wait_for(lambda: read_text(os.path.join(run_dir, "shell.pid")), args.launch_timeout)
            if not shell_text:
                raise RuntimeError(f"the pane never started (no shell.pid in {args.launch_timeout} s); "
                                   f"see {run_dir}/gui.stderr")
            shell_pid = int(shell_text)
            chain = ancestors(shell_pid)
            if arm["kind"] == "ft":
                if terminal_pid not in chain:
                    raise RuntimeError(f"pane shell {shell_pid} does not descend from GUI pid {terminal_pid}: {chain}")
            else:
                found = [pid for pid in chain if (ps_field(pid, "command") or "").startswith(self.ghostty_binary)]
                if not found:
                    raise RuntimeError(f"no Ghostty process among the pane shell's ancestors {chain}")
                terminal_pid = found[0]
                if token not in (ps_field(terminal_pid, "command") or ""):
                    raise RuntimeError(f"Ghostty pid {terminal_pid} does not carry this run's token")
            record["pids"] = {"terminal": terminal_pid, "shell": shell_pid, "ancestry": chain}
            log(f"run {index} {arm['name']}: terminal pid {terminal_pid}, pane shell pid {shell_pid}")

            if arm["kind"] == "ft":
                snapshot = wait_for(lambda: next((os.path.join(d, f"frankenterm-resources-{terminal_pid}.json")
                                                  for d in self.runtime_dirs if os.path.exists(
                                                      os.path.join(d, f"frankenterm-resources-{terminal_pid}.json"))), None),
                                    30, 0.1)
                record["isolated_snapshot"] = snapshot
                if snapshot:
                    poller = SnapshotPoller(snapshot, args.snapshot_interval_ms / 2000)
                    poller.start()
                else:
                    record["errors"].append("no resource snapshot in the isolated runtime dir within 30 s")

            stop_helpers = os.path.join(run_dir, "helpers.stop")
            if not args.no_fps:
                capture_out = os.path.join(run_dir, "frames.jsonl")
                ready = os.path.join(run_dir, "capture.ready")
                helper = subprocess.Popen([self.meter, "capture", "--pid", str(terminal_pid), "--out", capture_out,
                                           "--stop-file", stop_helpers, "--ready-file", ready,
                                           "--max-seconds", str(args.run_timeout + 120)],
                                          stdout=open(os.path.join(run_dir, "capture.log"), "w"), stderr=subprocess.STDOUT)
                helpers.append(("capture", helper))
                if not wait_for(lambda: os.path.exists(ready) or helper.poll() is not None, 60, 0.05) \
                        or not os.path.exists(ready):
                    raise RuntimeError(f"frame meter never started capturing (exit {helper.poll()}); "
                                       f"see {run_dir}/capture.log")
            if not args.no_beachball_probe:
                hang_dir = os.path.join(run_dir, "hangs")
                os.makedirs(hang_dir, exist_ok=True)
                helper = subprocess.Popen([self.meter, "probe", "--pid", str(terminal_pid),
                                           "--out", os.path.join(run_dir, "probe.jsonl"), "--stop-file", stop_helpers,
                                           "--interval-ms", str(args.probe_ms), "--hang-ms", str(args.hang_ms),
                                           "--sample-dir", hang_dir],
                                          stdout=open(os.path.join(run_dir, "probe.log"), "w"), stderr=subprocess.STDOUT)
                helpers.append(("probe", helper))

            if args.sibling_flood:
                if not wait_for(lambda: os.path.exists(os.path.join(run_dir, "flood.pid")), 30):
                    raise RuntimeError("the sibling flood pane never started")
                touch(os.path.join(run_dir, "flood.go"))
                log(f"sibling flood started; measuring after {args.flood_lead} s")
                time.sleep(args.flood_lead)
            time.sleep(args.settle)
            record["load_go"] = loadavg()
            go_ns = uptime_ns()
            touch(os.path.join(run_dir, "go"))
            log(f"run {index} {arm['name']}: go (load {record['load_go']})")

            cat_text = wait_for(lambda: read_text(os.path.join(run_dir, "cat.pid")), 60, 0.002)
            if not cat_text:
                raise RuntimeError("cat never started")
            cat_pid = int(cat_text)
            start_ns = uptime_ns()
            record["pids"]["cat"] = cat_pid
            record["drain"], exit_ns, samples = self.sample_drain(cat_pid, start_ns, run_dir, record)
            done = wait_for(lambda: read_text(os.path.join(run_dir, "done")), 60)
            record["cat_exit_status"] = int(done) if done and done.strip().isdigit() else None
            record["time"] = parse_time_line(read_text(os.path.join(run_dir, "time.txt")))
            if record["time"] is None:
                record["errors"].append("no zsh time line")
            if width_probe:
                wait_for(lambda: os.path.exists(os.path.join(run_dir, "widths.done")), 180, 0.1)
            time.sleep(args.tail)
            record["load_end"] = loadavg()
            touch(stop_helpers)
            for name, helper in helpers:
                try:
                    helper.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    record["errors"].append(f"{name} helper did not stop; terminating it")
                    helper.terminate()
            if poller:
                poller.stop_event.set()
                poller.join(timeout=5)

            drain_window = (start_ns, exit_ns) if exit_ns else None
            self.analyse(record, run_dir, drain_window, poller, go_ns)
            record["footprint_after"] = self.footprint(terminal_pid, run_dir, arm)
        except Exception as error:  # noqa: BLE001 -- recorded in the receipt
            record["errors"].append(f"{type(error).__name__}: {error}")
            log(f"run {index} {arm['name']} failed: {error}")
        finally:
            touch(os.path.join(run_dir, "stop"))
            for name, helper in helpers:
                if helper.poll() is None:
                    helper.terminate()
            if poller:
                poller.stop_event.set()
            time.sleep(1)
            if terminal_pid:
                terminate_exact(terminal_pid, token, f"{arm['name']} terminal")
        return record

    def sample_drain(self, cat_pid, start_ns, run_dir, record):
        period_ns = self.args.sample_ms * 1_000_000
        queue = select.kqueue()
        exited = False
        try:
            queue.control([select.kevent(cat_pid, filter=select.KQ_FILTER_PROC,
                                         flags=select.KQ_EV_ADD | select.KQ_EV_ONESHOT,
                                         fflags=select.KQ_NOTE_EXIT)], 0, 0)
        except (ProcessLookupError, OSError):
            exited = True
        cross = None
        if not exited:
            ours, theirs = fd_offset(cat_pid, 1), lsof_offset(cat_pid, 1)
            cross = {"proc_pidfdinfo": ours, "lsof": theirs}
        record["offset_cross_check"] = cross
        samples, exit_ns = [], None
        deadline = start_ns + self.args.run_timeout * 1_000_000_000
        next_ns = uptime_ns()
        with open(os.path.join(run_dir, "offsets.jsonl"), "w") as trace:
            while not exited:
                offset, now = fd_offset(cat_pid, 1), uptime_ns()
                if offset is not None:
                    samples.append((now, offset))
                    trace.write(json.dumps({"t_ns": now, "offset": offset}) + "\n")
                next_ns += period_ns
                timeout = max(0.0, (next_ns - uptime_ns()) / 1e9)
                if queue.control(None, 1, timeout):
                    exit_ns = uptime_ns()
                    break
                if uptime_ns() > deadline:
                    record["errors"].append(f"cat still draining after {self.args.run_timeout} s; stopped waiting")
                    break
        queue.close()
        if exited:
            exit_ns = uptime_ns()
            record["errors"].append("cat exited before sampling began (drain shorter than the pid handoff)")
        log(f"cat pid {cat_pid}: {len(samples)} offset samples, drain {((exit_ns or uptime_ns()) - start_ns) / 1e9:.3f} s")
        return drain_metrics(samples, self.corpus["bytes"], start_ns, exit_ns), exit_ns, samples

    def analyse(self, record, run_dir, window, poller, go_ns):
        args = self.args
        if not args.no_fps:
            records = read_jsonl(os.path.join(run_dir, "frames.jsonl"))
            header = next((r for r in records if r.get("type") == "window"), {})
            frames = [r for r in records if r.get("type") == "frame"]
            record["capture"] = {key: header.get(key) for key in ("window_id", "frame_points", "window_pixels",
                                                                    "capture_pixels", "display", "app", "bundle_id")}
            record["capture"]["summary"] = next((r for r in records if r.get("type") == "summary"), None)
            display = header.get("display") or {}
            refresh = display.get("max_fps") or self.refresh_hz
            record["capture"]["refresh_hz"] = refresh
            if window:
                record["fps"] = fps_metrics(frames, window[0], window[1], refresh)
                if record["fps"] is None:
                    record["errors"].append("the frame meter delivered no frames")
        if not args.no_beachball_probe:
            pings = [r for r in read_jsonl(os.path.join(run_dir, "probe.jsonl")) if r.get("type") == "ping"]
            record["beachball"] = beachball_metrics(pings, args.hang_ms)
            record["hang_samples"] = [r for r in read_jsonl(os.path.join(run_dir, "probe.jsonl"))
                                      if r.get("type") == "hang_sample"]
        if poller is not None:
            last = poller.last or {}
            record["ft_snapshot"] = {"frames": last.get("frames"), "terminal_locks": last.get("terminal_locks"),
                                     "allocator": last.get("allocator"), "points": len(poller.points)}
            if window:
                record["internal_present"] = internal_present_fps(poller.points, window[0], window[1])
        widths = read_text(os.path.join(run_dir, "widths.json"))
        if widths:
            try:
                record["widths"] = json.loads(widths)
            except ValueError:
                record["errors"].append("widths.json unreadable")

    def footprint(self, pid, run_dir, arm):
        if not alive(pid):
            return None
        command = [FOOTPRINT, "capture", "--pid", str(pid), "--out", os.path.join(run_dir, "footprint"),
                   "--label", arm["name"], "--no-unmapped"]
        if arm["kind"] == "ft":
            existing = [d for d in self.runtime_dirs if os.path.isdir(d)]
            if existing:
                command += ["--runtime-dir", existing[0]]
        result = subprocess.run(command, capture_output=True, text=True, timeout=300)
        bundle_dir = result.stdout.strip().splitlines()[-1] if result.stdout.strip() else None
        text = read_text(os.path.join(bundle_dir, "bundle.json")) if bundle_dir else None
        if not text:
            return {"error": result.stderr.strip()[-500:]}
        bundle = json.loads(text)
        return {"bundle": bundle_dir, "rss_bytes": bundle.get("rss_bytes"),
                "phys_footprint": bundle.get("phys_footprint"), "gpu_owned_bytes": bundle.get("gpu_owned_bytes")}


def parse_args(argv):
    parser = argparse.ArgumentParser(
        prog="mac-gui-throughput.sh", formatter_class=argparse.RawDescriptionHelpFormatter,
        description=__doc__.split("\n\n")[0],
        epilog="Exit status: 0 receipt written, 1 a run failed or the receipt is invalid, 2 setup refused.")
    parser.add_argument("--gui-bin", help="frankenterm-gui under test (build from an exact commit)")
    parser.add_argument("--ft-bin", help="ft for the read-only isolation probe (default: ft on PATH)")
    parser.add_argument("--corpus", default="T0", help="T0..T5 or an M.1 corpus name (default T0, color_emoji_random)")
    parser.add_argument("--corpus-file", help="drain this file instead (read-only), e.g. ~/color-emoji-random.bin")
    parser.add_argument("--corpus-gen-bin", help="the M.1 ingest_throughput example binary, used with --gen-only")
    parser.add_argument("--corpus-dir", help="corpus cache (default $TMPDIR/ft-gui-throughput-corpus)")
    parser.add_argument("--size", type=parse_size, help="generated corpus bytes (default: the operator's 749801000 "
                        "for T0, 256MiB otherwise)")
    parser.add_argument("--seed", help="generator seed (default: M.1's)")
    parser.add_argument("--baseline", choices=("ghostty", "ft"), default="ghostty",
                        help="arm B: Ghostty.app (default) or a second FrankenTerm (A/B gates such as A1.8)")
    parser.add_argument("--baseline-gui-bin", help="arm B frankenterm-gui (default --gui-bin)")
    parser.add_argument("--baseline-ft-lua", action="append", default=[], metavar="LINE",
                        help="Lua line appended to arm B's config, e.g. \"config.x = false\" (repeatable)")
    parser.add_argument("--baseline-env", action="append", default=[], metavar="NAME=VALUE")
    parser.add_argument("--ft-lua", action="append", default=[], metavar="LINE",
                        help="Lua line appended to arm A's config (repeatable)")
    parser.add_argument("--ft-env", action="append", default=[], metavar="NAME=VALUE",
                        help="extra variable for arm A's isolated environment (repeatable)")
    parser.add_argument("--ft-max-fps", default="auto",
                        help="max_fps for FrankenTerm (default auto: twice the fastest display refresh)")
    parser.add_argument("--ft-front-end", default="WebGpu", help="front_end (default WebGpu, the operator's)")
    parser.add_argument("--ft-scrollback-lines", type=int, default=100_000, help="scrollback_lines (default 100000)")
    parser.add_argument("--snapshot-interval-ms", type=int, default=500,
                        help="FrankenTerm resource-snapshot cadence (FT_RESOURCE_SNAPSHOT_INTERVAL_MS)")
    parser.add_argument("--ghostty-app", default="/Applications/Ghostty.app")
    parser.add_argument("--ghostty-pin", help="required Ghostty version (+version 'version:' field); default: "
                        "the ghostty-app-version line of docs/perf/incumbents/ghostty.md")
    parser.add_argument("--allow-unpinned-ghostty", action="store_true",
                        help="report a verdict without a pin (marked unpinned)")
    parser.add_argument("--ghostty-launch", choices=("open", "exec"), default="open",
                        help="open -na (default) or exec the app binary directly")
    parser.add_argument("--ghostty-arg", action="append", default=[], metavar="--KEY=VALUE")
    parser.add_argument("--ghostty-scrollback-bytes", type=int, help="Ghostty scrollback-limit (default Ghostty's)")
    parser.add_argument("--cols", type=int, default=120)
    parser.add_argument("--rows", type=int, default=40)
    parser.add_argument("--font-family", help=f"both terminals (default {OPERATOR_FONT!r} if installed, else Menlo)")
    parser.add_argument("--font-size", type=float, default=16.0, help="both terminals (default 16, the operator's)")
    parser.add_argument("--reps", type=int, default=2, help="runs per arm, ABBA-interleaved (default 2)")
    parser.add_argument("--cv-max", type=float, default=5.0, help="refuse a verdict above this CV percent")
    parser.add_argument("--max-load", type=float, default=0.0, help="refuse a verdict when a run starts above "
                        "this 1-minute load average (default 0: record only)")
    parser.add_argument("--sample-ms", type=int, default=250, help="tty offset sampling period")
    parser.add_argument("--probe-ms", type=int, default=100, help="main-thread probe period")
    parser.add_argument("--hang-ms", type=int, default=2000, help="probe latency counted as a beach ball")
    parser.add_argument("--no-fps", action="store_true", help="skip the screen-capture FPS meter (no FPS verdict)")
    parser.add_argument("--no-beachball-probe", action="store_true", help="skip the Accessibility probe")
    parser.add_argument("--no-width-probe", action="store_true", help="skip the emoji width probe")
    parser.add_argument("--sibling-flood", action="store_true",
                        help="FrankenTerm arms only: pane 1 floods --flood-corpus while pane 2 is measured")
    parser.add_argument("--flood-corpus", default="T1", help="sibling flood corpus (default T1)")
    parser.add_argument("--flood-size", type=parse_size, default=64 << 20)
    parser.add_argument("--flood-lead", type=float, default=2.0, help="seconds of flood before go")
    parser.add_argument("--settle", type=float, default=3.0, help="seconds between capture start and go")
    parser.add_argument("--tail", type=float, default=1.0, help="seconds of capture after the drain")
    parser.add_argument("--launch-timeout", type=float, default=90.0)
    parser.add_argument("--run-timeout", type=int, default=1800, help="seconds one drain may take")
    parser.add_argument("--cache-dir", help="frame-meter build cache (default $TMPDIR/ft-gui-throughput-cache)")
    parser.add_argument("--out", help="run directory (default ./gui-throughput-runs/<UTC stamp>)")
    parser.add_argument("--self-test", action="store_true",
                        help="1 MiB T0, one run per arm, then validate the receipt schema")
    parser.add_argument("--dry-run", action="store_true", help="print the plan; launch nothing")
    parser.add_argument("--analysis-self-test", action="store_true",
                        help="check the analysis code on synthetic data; launch nothing")
    parser.add_argument("--pty-self-test", action="store_true",
                        help="run the pane script in a private pty this process plays terminal for; no GUI")
    args = parser.parse_args(argv)
    label = args.corpus.upper()
    if label in CORPORA:
        args.corpus_label, args.corpus_name = label, CORPORA[label]
    elif args.corpus.replace("-", "_") in CORPORA.values():
        args.corpus_name = args.corpus.replace("-", "_")
        args.corpus_label = next(key for key, value in CORPORA.items() if value == args.corpus_name)
    else:
        parser.error(f"unknown corpus {args.corpus!r}")
    if args.self_test:
        args.size = args.size or (1 << 20)
        args.reps = 1
    flood = args.flood_corpus.upper()
    args.flood_name = CORPORA.get(flood, args.flood_corpus.replace("-", "_"))
    tmp = tempfile.gettempdir()
    args.corpus_dir = os.path.abspath(args.corpus_dir or os.path.join(tmp, "ft-gui-throughput-corpus"))
    args.cache_dir = os.path.abspath(args.cache_dir or os.path.join(tmp, "ft-gui-throughput-cache"))
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    args.out = os.path.abspath(args.out or os.path.join("gui-throughput-runs", stamp))
    for item in args.ft_env + args.baseline_env:
        if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=.*", item):
            parser.error(f"--ft-env/--baseline-env needs NAME=VALUE, got {item!r}")
    if args.sibling_flood and args.baseline == "ghostty":
        parser.error("--sibling-flood needs --baseline ft: Ghostty's panes cannot be split from its command line")
    return args


def main(argv):
    global LOG_HANDLE
    args = parse_args(argv)
    if args.analysis_self_test:
        analysis_self_test()
        return 0
    if args.pty_self_test:
        pty_self_test()
        return 0
    if sys.platform != "darwin":
        die("macOS only")
    if not args.gui_bin and not args.dry_run:
        die("--gui-bin is required")
    if args.gui_bin:
        args.gui_bin = os.path.abspath(args.gui_bin)
        if not os.access(args.gui_bin, os.X_OK):
            die(f"--gui-bin {args.gui_bin} is not executable")
    os.makedirs(args.out, exist_ok=True)
    LOG_HANDLE = open(os.path.join(args.out, "run.log"), "a")
    harness = Harness(args)
    started = datetime.datetime.now(datetime.timezone.utc).isoformat()
    log(f"run directory {args.out}; argv {shlex.join(argv)}")

    meter, meter_sha = build_meter(args.cache_dir)
    harness.meter = meter
    preflight = json.loads(subprocess.run([meter, "preflight", "--font", args.font_family or OPERATOR_FONT],
                                          capture_output=True, text=True, check=True).stdout)
    tcc = {"screen_capture": preflight["screen_capture_access"], "accessibility": preflight["accessibility_trusted"]}
    log(f"preflight: screen capture {tcc['screen_capture']}, accessibility {tcc['accessibility']}")
    responsible = "the app that runs this harness (the terminal or tmux host it runs in)"
    if args.dry_run:
        pass
    elif not args.no_fps and not tcc["screen_capture"]:
        die("Screen Recording permission is missing, so the FPS meter would report 0 FPS. Grant it in System "
            f"Settings > Privacy & Security > Screen & System Audio Recording to {responsible}, restart that app, "
            "and rerun; or pass --no-fps to run without an FPS row.")
    elif not args.no_beachball_probe and not tcc["accessibility"]:
        die("Accessibility permission is missing, so the beach-ball probe cannot reach the GUI's main thread. "
            f"Grant it in System Settings > Privacy & Security > Accessibility to {responsible} and rerun; or "
            "pass --no-beachball-probe.")
    displays = preflight["displays"]
    harness.refresh_hz = max((display.get("max_fps") or 60) for display in displays) if displays else 60
    if not args.font_family:
        args.font_family = OPERATOR_FONT if preflight.get("font", {}).get("installed") else "Menlo"
    harness.ft_max_fps = 2 * harness.refresh_hz if args.ft_max_fps == "auto" else int(args.ft_max_fps)
    throttle_ms, throttle_ok = throttle_admissible(harness.ft_max_fps, harness.refresh_hz)
    log(f"display refresh {harness.refresh_hz} Hz; FrankenTerm max_fps {harness.ft_max_fps} "
        f"(throttle {throttle_ms} ms, {'below' if throttle_ok else 'NOT below'} one refresh period)")

    arms = [{"name": "ft", "kind": "ft", "gui_bin": args.gui_bin, "lua": list(args.ft_lua),
             "env": dict(item.split("=", 1) for item in args.ft_env)}]
    incumbent = None
    if args.baseline == "ghostty":
        arms.append({"name": "ghostty", "kind": "ghostty"})
        binary, version, version_text = ghostty_version(args.ghostty_app)
        harness.ghostty_binary = binary
        pin, pin_source = ghostty_pin(args)
        incumbent = {"app": args.ghostty_app, "binary": binary, "version": version, "version_report": version_text,
                     "pin": pin, "pin_source": pin_source, "launch": args.ghostty_launch}
        if pin and pin != version:
            die(f"Ghostty drift: installed {version}, pinned {pin} ({pin_source}); refusing to run")
        log(f"Ghostty {version} ({'pinned by ' + pin_source if pin else 'UNPINNED'})")
    else:
        baseline_bin = os.path.abspath(args.baseline_gui_bin or args.gui_bin or "")
        arms.append({"name": "ft_baseline", "kind": "ft", "gui_bin": baseline_bin, "lua": list(args.baseline_ft_lua),
                     "env": dict(item.split("=", 1) for item in args.baseline_env)})
    order = [arms[0] if slot == "A" else arms[1] for slot in abc_order(args.reps)]

    harness.prepare_dirs()
    corpus = corpus_spec(args, args.out)
    harness.corpus = corpus
    harness.flood_corpus = None
    if args.sibling_flood:
        flood_args = argparse.Namespace(**vars(args))
        flood_args.corpus_file, flood_args.corpus_name, flood_args.size = None, args.flood_name, args.flood_size
        flood_args.corpus_label = next((key for key, value in CORPORA.items() if value == args.flood_name), None)
        harness.flood_corpus = corpus_spec(flood_args, args.out)
    log(f"corpus {corpus['label']} {corpus['name']}: {corpus['path']} ({corpus['bytes']} bytes, sha256 {corpus['sha256']})")

    plan = {"order": [arm["name"] for arm in order], "corpus": corpus, "tcc": tcc, "geometry": {
        "cols": args.cols, "rows": args.rows, "font_family": args.font_family, "font_size": args.font_size},
        "ft_max_fps": harness.ft_max_fps, "refresh_hz": harness.refresh_hz, "isolated_env": harness.iso_env,
        "arms": arms, "incumbent": incumbent}
    if args.dry_run:
        print(json.dumps(plan, indent=2, sort_keys=True, default=str))
        print("ft lua config (arm A):\n" + ft_lua_config(arms[0], args, harness.ft_max_fps,
                                                          ["<flood>"] if args.sibling_flood else None))
        return 0

    isolation = {"probe": harness.isolation_probe(), "env": harness.iso_env}
    runs = []
    widths_done = set()
    for index, arm in enumerate(order):
        probe_widths = not args.no_width_probe and arm["name"] not in widths_done
        runs.append(harness.run_once(index, arm, probe_widths))
        widths_done.add(arm["name"])

    # Aggregate and verdicts.
    names = [arms[0]["name"], arms[1]["name"]]
    per_arm = {name: [run for run in runs if run["arm"] == name] for name in names}

    def values(name, getter):
        out = []
        for run in per_arm[name]:
            try:
                value = getter(run)
            except (KeyError, TypeError):
                value = None
            if value is not None:
                out.append(value)
        return out

    refusals = []
    failed_runs = [run["index"] for run in runs if run["errors"]]
    if failed_runs:
        refusals.append(f"runs {failed_runs} recorded errors")
    if args.max_load and any(run["load_start"][0] > args.max_load for run in runs):
        refusals.append(f"a run started above load {args.max_load}")
    if incumbent and not incumbent["pin"] and not args.allow_unpinned_ghostty:
        refusals.append("Ghostty is not pinned (M.2 contract or --ghostty-pin)")
    fps_refusals = list(refusals)
    if args.no_fps:
        fps_refusals.append("FPS not measured (--no-fps)")
    if not throttle_ok:
        fps_refusals.append(f"FrankenTerm max_fps {harness.ft_max_fps} throttles at {throttle_ms} ms, not below "
                            f"one {harness.refresh_hz} Hz refresh period")
    meter_checks = []
    for run in runs:
        internal, fps = run.get("internal_present"), run.get("fps")
        if run["kind"] == "ft" and internal and fps:
            ceiling = min(internal["fps"], run.get("capture", {}).get("refresh_hz") or harness.refresh_hz)
            ok = fps["mean_fps"] <= internal["fps"] * 1.05 + 1 and fps["mean_fps"] >= 0.8 * ceiling
            meter_checks.append({"run": run["index"], "meter_fps": fps["mean_fps"], "internal_fps": internal["fps"],
                                 "ok": ok})
            if not ok:
                fps_refusals.append(f"run {run['index']}: meter {fps['mean_fps']:.1f} FPS vs FrankenTerm's own "
                                    f"{internal['fps']:.1f} presents/s")
    for run in runs:
        snapshot_fps = ((run.get("ft_snapshot") or {}).get("frames") or {}).get("max_fps")
        if run["kind"] == "ft" and snapshot_fps not in (None, 0, harness.ft_max_fps):
            fps_refusals.append(f"run {run['index']}: the GUI reports max_fps {snapshot_fps}, "
                                f"not the configured {harness.ft_max_fps}")

    aggregate = {}
    for name in names:
        aggregate[name] = {
            "time_total_s": summary(values(name, lambda run: run["time"]["total_s"])),
            "drain_s": summary(values(name, lambda run: run["drain"]["drain_s"])),
            "drain_mb_s": summary(values(name, lambda run: run["drain"]["mb_s"])),
            "mean_fps": summary(values(name, lambda run: run["fps"]["mean_fps"])),
            "beach_balls": sum(values(name, lambda run: run["beachball"]["beach_balls"])),
            "stall_max_s": max(values(name, lambda run: run["drain"]["stalls"]["max_s"]) or [None],
                               key=lambda value: -1 if value is None else value),
            "internal_present_fps": summary(values(name, lambda run: run["internal_present"]["fps"])),
            "footprint_after": [run.get("footprint_after") for run in per_arm[name]],
        }
    pin_note = " (ghostty unpinned)" if incumbent and not incumbent["pin"] else ""
    verdicts = {
        "drain_total_s": verdict({name: values(name, lambda run: run["time"]["total_s"]) for name in names},
                                 "total_s", False, args.cv_max, refusals),
        "drain_offset_s": verdict({name: values(name, lambda run: run["drain"]["drain_s"]) for name in names},
                                  "drain_s", False, args.cv_max, refusals),
        "fps": verdict({name: values(name, lambda run: run["fps"]["mean_fps"]) for name in names},
                       "mean_fps", True, args.cv_max, fps_refusals),
        "meter_validation": meter_checks,
    }
    for key in ("drain_total_s", "drain_offset_s", "fps"):
        verdicts[key]["verdict"] += pin_note
    parity = width_parity({name: next((run["widths"]["widths"] for run in per_arm[name] if run.get("widths")), None)
                           for name in names})

    receipt = {
        "schema": SCHEMA,
        "started_utc": started,
        "finished_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "harness": {"argv": argv, "script_sha256": sha256_file(os.path.abspath(__file__)),
                    "meter_sha256": meter_sha, "token": harness.token},
        "fingerprint": fingerprint(args, displays),
        "corpus": corpus,
        "flood_corpus": harness.flood_corpus,
        "geometry": plan["geometry"] | {"sibling_flood": args.sibling_flood},
        "ft": {"gui_bin": args.gui_bin, "gui_bin_sha256": sha256_file(args.gui_bin), "max_fps": harness.ft_max_fps,
               "throttle_interval_ms": throttle_ms, "front_end": args.ft_front_end,
               "scrollback_lines": args.ft_scrollback_lines, "lua": args.ft_lua, "env": args.ft_env,
               "snapshot_interval_ms": args.snapshot_interval_ms},
        "display": {"refresh_hz": harness.refresh_hz, "scale": [display.get("scale") for display in displays]},
        "incumbent": incumbent,
        "arms": {arm["name"]: {key: value for key, value in arm.items()} for arm in arms},
        "order": [arm["name"] for arm in order],
        "tcc": tcc,
        "isolation": isolation,
        "runs": runs,
        "aggregate": aggregate,
        "verdicts": verdicts,
        "width_parity": parity,
    }
    if args.baseline == "ft":
        receipt["ft"]["baseline_gui_bin_sha256"] = sha256_file(arms[1]["gui_bin"])
    problems = validate_receipt(receipt)
    receipt["schema_problems"] = problems
    path = os.path.join(args.out, "receipt.json")
    with open(path, "w") as handle:
        json.dump(receipt, handle, indent=2, sort_keys=True, default=str)

    def line(name):
        stats = aggregate[name]
        total = stats["time_total_s"]["median"] if stats["time_total_s"] else float("nan")
        fps = stats["mean_fps"]["median"] if stats["mean_fps"] else float("nan")
        return f"{name}: time total {total:.3f} s, FPS {fps:.1f}, beach balls {stats['beach_balls']}"

    print(f"{corpus['label']} {corpus['name']} {corpus['bytes']} bytes, load at go "
          f"{[run.get('load_go') for run in runs]}")
    for name in names:
        print("  " + line(name))
    print(f"  drain verdict: {verdicts['drain_total_s']['verdict']}")
    print(f"  FPS verdict:   {verdicts['fps']['verdict']}")
    print(f"  width parity:  {parity.get('disagreements')} disagreement(s) over {parity.get('codepoints')} codepoints")
    print(f"receipt: {path}")
    if problems:
        log(f"receipt schema problems: {problems}")
    return 1 if (problems or failed_runs) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
