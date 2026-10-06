#!/usr/bin/env python3
"""Pins, statistics, verdicts and receipts for scripts/ghostty-headless-h2h.sh.

ft-yccm0.1.3 (plan M.2). The shell runner owns the processes (pin checks,
builds, hyperfine rounds, probes); this helper owns everything that is
arithmetic or schema:

* ``pins``      parse the machine-readable pin block of the incumbent
                contract (docs/perf/incumbents/ghostty.md), strictly.
* ``pin``       print one pin for the shell.
* ``gates``     resolve the effective gates; CLI overrides may only tighten.
* ``app-info``  read a macOS app bundle's version and binary SHA-256.
* ``loadavg``   print the host's 1/5/15-minute load averages.
* ``fingerprint`` describe the measurement host and toolchain.
* ``fact``      accumulate run facts into a JSON file; ``get`` reads one back.
* ``sha256``/``tree-digest`` hash a file / an extracted toolchain tree.
* ``inputs``    hash and order the corpora both arms read (primary first).
* ``row``       fold one (corpus, geometry) row's hyperfine rounds into
                per-arm statistics and a verdict.
* ``parity``    compare the final grid geometry of both engines on the
                emoji corpus (width parity).
* ``receipt``   assemble the receipt; ``validate`` checks its schema;
                ``verdicts`` prints the verdict lines.

Stdlib only; the runner calls it as ``python3 -I``.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import plistlib
import re
import statistics
import subprocess
import sys
from pathlib import Path
from typing import Any, Callable

RECEIPT_SCHEMA = "ft.bench.ghostty-h2h.v1"
ROW_SCHEMA = "ft.bench.ghostty-h2h-row.v1"
PARITY_SCHEMA = "ft.bench.ghostty-h2h-parity.v1"
PROBE_SCHEMA = "ft.bench.ghostty-h2h-probe.v1"
FT_LANE_SCHEMA = "ft.bench.ingest-throughput.v1"
PIN_FENCE = "ghostty-h2h-pins"
ARMS = ("ghostty", "frankenterm")
FT_PROFILES = ("release-perf", "release-interactive")
VERDICT_RE = re.compile(r"^(ft_faster|ghostty_faster|NO_ADMISSIBLE_RATIO \(.+\))$")
HEX40 = re.compile(r"^[0-9a-f]{40}$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
GEOMETRY_RE = re.compile(r"^([1-9][0-9]*)x([1-9][0-9]*)$")


class UsageError(Exception):
    """Bad input to the helper; exit 2."""


class PinError(Exception):
    """The contract's pin block is missing or malformed; exit 3 (fail closed)."""


# --------------------------------------------------------------------------
# Pins


def _nonempty(value: str) -> str:
    if not value:
        raise ValueError("must not be empty")
    return value


def _hex40(value: str) -> str:
    if not HEX40.match(value):
        raise ValueError("must be a full 40-hex-digit lowercase git SHA")
    return value


def _hex64(value: str) -> str:
    if not HEX64.match(value):
        raise ValueError("must be a 64-hex-digit lowercase SHA-256")
    return value


def _version(value: str) -> str:
    if not re.match(r"^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$", value):
        raise ValueError("must look like MAJOR.MINOR.PATCH[-suffix]")
    return value


def _https_url(value: str) -> str:
    if not value.startswith("https://") or any(ch.isspace() for ch in value):
        raise ValueError("must be an https:// URL without whitespace")
    return value


def _file_name(value: str) -> str:
    if not value or "/" in value or value in (".", ".."):
        raise ValueError("must be a bare file name")
    return value


def _relative_path(value: str) -> str:
    if not value or value.startswith("/") or ".." in value.split("/"):
        raise ValueError("must be a relative path without '..'")
    return value


def _absolute_path(value: str) -> str:
    if not value.startswith("/"):
        raise ValueError("must be an absolute path")
    return value


def _positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise ValueError("must be a positive integer")
    return parsed


def _nonnegative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise ValueError("must be a non-negative integer")
    return parsed


def _min_runs(value: str) -> int:
    parsed = int(value)
    if parsed < 2:
        raise ValueError("must be at least 2")
    return parsed


def _positive_float(value: str) -> float:
    parsed = float(value)
    if not math.isfinite(parsed) or parsed <= 0:
        raise ValueError("must be a positive number")
    return parsed


def _build_flags(value: str) -> str:
    flags = value.split()
    if not flags or any(not flag.startswith("-D") for flag in flags):
        raise ValueError("must be space-separated -D options")
    return " ".join(flags)


def _bench_action(value: str) -> str:
    if not re.match(r"^\+[a-z][a-z0-9-]*$", value):
        raise ValueError("must be a ghostty-bench +action")
    return value


def _ft_profile(value: str) -> str:
    if value not in FT_PROFILES:
        raise ValueError(f"must be one of {', '.join(FT_PROFILES)}")
    return value


def parse_geometry(text: str) -> dict[str, int]:
    match = GEOMETRY_RE.match(text.strip())
    if not match:
        raise ValueError(f"bad geometry {text!r}; expected ROWSxCOLS, e.g. 80x120")
    rows, cols = int(match.group(1)), int(match.group(2))
    if rows > 65535 or cols > 65535:
        raise ValueError(f"geometry {text!r} exceeds ghostty-bench's u16 rows/cols")
    return {"rows": rows, "cols": cols}


def _geometries(value: str) -> list[dict[str, int]]:
    items = value.split()
    if not items:
        raise ValueError("must list at least one ROWSxCOLS geometry")
    return [parse_geometry(item) for item in items]


PIN_SPEC: dict[str, Callable[[str], Any]] = {
    "ghostty_commit": _hex40,
    "ghostty_version": _nonempty,
    "zig_version": _version,
    "zig_tarball": _file_name,
    "zig_tarball_url": _https_url,
    "zig_tarball_sha256": _hex64,
    "zig_tarball_top_dir": _file_name,
    "build_flags": _build_flags,
    "bench_binary": _relative_path,
    "bench_action": _bench_action,
    "bench_read_chunk_bytes": _positive_int,
    "bench_default_scrollback_bytes": _nonnegative_int,
    "geometries": _geometries,
    "primary_corpus": _nonempty,
    "app_path": _absolute_path,
    "app_bundle_id": _nonempty,
    "app_short_version": _nonempty,
    "app_build": _nonempty,
    "app_binary": _relative_path,
    "app_binary_sha256": _hex64,
    "ft_profile": _ft_profile,
    "ft_scrollback": _nonnegative_int,
    "ft_chunk_bytes": _positive_int,
    "min_runs": _min_runs,
    "warmup": _nonnegative_int,
    "max_cv_pct": _positive_float,
    "max_load_1m": _positive_float,
}


def extract_pin_block(markdown: str) -> list[str]:
    """Returns the lines of the single ```ghostty-h2h-pins fenced block."""
    lines = markdown.splitlines()
    blocks: list[list[str]] = []
    index = 0
    while index < len(lines):
        if lines[index].strip() == "```" + PIN_FENCE:
            body: list[str] = []
            index += 1
            while index < len(lines) and lines[index].strip() != "```":
                body.append(lines[index])
                index += 1
            if index == len(lines):
                raise PinError(f"the ```{PIN_FENCE} block is not closed")
            blocks.append(body)
        index += 1
    if not blocks:
        raise PinError(f"no ```{PIN_FENCE} block in the contract")
    if len(blocks) > 1:
        raise PinError(f"{len(blocks)} ```{PIN_FENCE} blocks; exactly one is allowed")
    return blocks[0]


def parse_pins(markdown: str) -> dict[str, Any]:
    raw: dict[str, str] = {}
    for number, line in enumerate(extract_pin_block(markdown), start=1):
        text = line.strip()
        if not text or text.startswith("#"):
            continue
        key, sep, value = text.partition("=")
        key, value = key.strip(), value.strip()
        if not sep or not key:
            raise PinError(f"pin line {number} is not 'key = value': {line!r}")
        if key not in PIN_SPEC:
            raise PinError(f"unknown pin {key!r} on pin line {number}")
        if key in raw:
            raise PinError(f"pin {key!r} is set twice")
        raw[key] = value
    missing = [key for key in PIN_SPEC if key not in raw]
    if missing:
        raise PinError(f"missing pins: {', '.join(missing)}")
    pins: dict[str, Any] = {}
    for key, validator in PIN_SPEC.items():
        try:
            pins[key] = validator(raw[key])
        except ValueError as error:
            raise PinError(f"pin {key} = {raw[key]!r}: {error}") from None
    if not raw["zig_tarball_url"].endswith("/" + raw["zig_tarball"]):
        raise PinError("zig_tarball_url must end with the zig_tarball file name")
    return pins


def pin_as_text(pins: dict[str, Any], key: str) -> str:
    if key not in pins:
        raise UsageError(f"no pin named {key!r}")
    value = pins[key]
    if key == "geometries":
        return " ".join(f"{g['rows']}x{g['cols']}" for g in value)
    return str(value)


def resolve_gates(
    pins: dict[str, Any],
    max_cv: float | None,
    max_load: float | None,
    rounds: int | None,
    warmup: int | None,
    self_test: bool,
) -> dict[str, Any]:
    """Effective gates. Overrides may tighten the contract, never loosen it."""
    gates = {
        "max_cv_pct": pins["max_cv_pct"],
        "max_load_1m": pins["max_load_1m"],
        "min_runs": pins["min_runs"],
        "rounds": pins["min_runs"],
        "warmup": pins["warmup"],
        "order": "ABBA",
        "contract_max_cv_pct": pins["max_cv_pct"],
        "contract_max_load_1m": pins["max_load_1m"],
    }
    if max_cv is not None:
        if not math.isfinite(max_cv) or max_cv <= 0 or max_cv > pins["max_cv_pct"]:
            raise UsageError(
                f"--max-cv {max_cv} would loosen the contract's {pins['max_cv_pct']}%; "
                "overrides may only tighten"
            )
        gates["max_cv_pct"] = max_cv
    if max_load is not None:
        if not math.isfinite(max_load) or max_load <= 0 or max_load > pins["max_load_1m"]:
            raise UsageError(
                f"--max-load {max_load} would loosen the contract's {pins['max_load_1m']}; "
                "overrides may only tighten"
            )
        gates["max_load_1m"] = max_load
    if rounds is not None:
        if rounds < 2:
            raise UsageError("--rounds must be at least 2 (one AB and one BA round)")
        if rounds < pins["min_runs"] and not self_test:
            raise UsageError(
                f"--rounds {rounds} is below the contract's min_runs {pins['min_runs']}"
            )
        gates["rounds"] = rounds
    if warmup is not None:
        if warmup < 0:
            raise UsageError("--warmup must be non-negative")
        gates["warmup"] = warmup
    return gates


# --------------------------------------------------------------------------
# Host facts


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def tree_digest(root: Path) -> tuple[int, str]:
    """(file count, SHA-256 over every file's relative path, size and mode).

    Recorded when the runner extracts the verified zig tarball and checked
    on every reuse: a cleanup sweep for `target` directories also matches
    zig's `lib/std/Target` on a case-insensitive file system, and a toolchain
    missing files must never build the incumbent."""
    entries = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for name in sorted(filenames):
            path = Path(dirpath) / name
            info = path.lstat()
            rel = path.relative_to(root).as_posix()
            entries.append(f"{rel}\0{info.st_size}\0{info.st_mode & 0o777:o}")
    digest = hashlib.sha256("\n".join(entries).encode()).hexdigest()
    return len(entries), digest


def app_info(app: Path, binary: str) -> dict[str, Any]:
    plist_path = app / "Contents" / "Info.plist"
    with plist_path.open("rb") as handle:
        info = plistlib.load(handle)
    binary_path = app / binary
    return {
        "path": str(app),
        "bundle_id": info.get("CFBundleIdentifier"),
        "short_version": info.get("CFBundleShortVersionString"),
        "build": info.get("CFBundleVersion"),
        "binary": binary,
        "binary_sha256": sha256_file(binary_path) if binary_path.is_file() else None,
    }


def app_drift(pins: dict[str, Any], info: dict[str, Any]) -> list[str]:
    checks = (
        ("bundle_id", "app_bundle_id"),
        ("short_version", "app_short_version"),
        ("build", "app_build"),
        ("binary_sha256", "app_binary_sha256"),
    )
    return [
        f"{field} is {info.get(field)!r}, pinned {pins[pin]!r}"
        for field, pin in checks
        if info.get(field) != pins[pin]
    ]


def load_averages() -> list[float] | None:
    try:
        return [float(x) for x in Path("/proc/loadavg").read_text().split()[:3]]
    except OSError:
        pass
    try:
        text = subprocess.run(
            ["sysctl", "-n", "vm.loadavg"], capture_output=True, text=True, check=True
        ).stdout
    except (OSError, subprocess.CalledProcessError):
        return None
    fields = [field for field in text.replace("{", " ").replace("}", " ").split()]
    try:
        return [float(x) for x in fields[:3]]
    except ValueError:
        return None


def _run_text(argv: list[str]) -> str | None:
    try:
        result = subprocess.run(argv, capture_output=True, text=True, timeout=30)
    except (OSError, subprocess.TimeoutExpired):
        return None
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


def _sysctl(name: str) -> str | None:
    return _run_text(["sysctl", "-n", name])


def fingerprint(zig: str | None) -> dict[str, Any]:
    def as_int(text: str | None) -> int | None:
        try:
            return int(text) if text is not None else None
        except ValueError:
            return None

    therm = _run_text(["pmset", "-g", "therm"])
    return {
        "system": platform.system(),
        "kernel": platform.release(),
        "machine": platform.machine(),
        "model": _sysctl("hw.model"),
        "cpu": _sysctl("machdep.cpu.brand_string"),
        "ncpu": as_int(_sysctl("hw.ncpu")) or os.cpu_count(),
        "performance_cores": as_int(_sysctl("hw.perflevel0.physicalcpu")),
        "efficiency_cores": as_int(_sysctl("hw.perflevel1.physicalcpu")),
        "memory_bytes": as_int(_sysctl("hw.memsize")),
        "os_version": _run_text(["sw_vers", "-productVersion"]),
        "os_build": _run_text(["sw_vers", "-buildVersion"]),
        "thermal": therm.splitlines() if therm else None,
        "load_avg": load_averages(),
        "hyperfine": _run_text(["hyperfine", "--version"]),
        "zig": _run_text([zig, "version"]) if zig else None,
        "rustc": _run_text(["rustc", "-V"]),
        "macos_sdk": _run_text(["xcrun", "--show-sdk-version"]),
        "python": platform.python_version(),
    }


def set_fact(facts: dict[str, Any], dotted: str, value: Any) -> None:
    node = facts
    parts = dotted.split(".")
    for part in parts[:-1]:
        child = node.setdefault(part, {})
        if not isinstance(child, dict):
            raise UsageError(f"fact {dotted!r} descends into a non-object at {part!r}")
        node = child
    node[parts[-1]] = value


def _read_json(path: Path) -> Any:
    with path.open() as handle:
        return json.load(handle)


def _write_json(path: Path, value: Any) -> None:
    tmp = path.with_name(path.name + ".tmp")
    with tmp.open("w") as handle:
        json.dump(value, handle, indent=2, sort_keys=False)
        handle.write("\n")
    tmp.replace(path)


def update_facts(path: Path, dotted: str, value: Any) -> None:
    facts = _read_json(path) if path.exists() else {}
    set_fact(facts, dotted, value)
    _write_json(path, facts)


# --------------------------------------------------------------------------
# Statistics and verdicts


def percentile_nearest_rank(values: list[float], pct: float) -> float:
    """Nearest-rank percentile: the smallest value with at least pct% of the
    samples at or below it. With 10 samples, p95 is the maximum."""
    if not values:
        raise ValueError("no samples")
    ordered = sorted(values)
    rank = max(1, math.ceil(pct / 100.0 * len(ordered)))
    return ordered[rank - 1]


def arm_stats(times: list[float]) -> dict[str, Any]:
    n = len(times)
    mean = statistics.fmean(times) if n else float("nan")
    stdev = statistics.stdev(times) if n >= 2 else 0.0
    return {
        "n": n,
        "median_s": statistics.median(times) if n else None,
        "p95_s": percentile_nearest_rank(times, 95) if n else None,
        "mean_s": mean if n else None,
        "stddev_s": stdev,
        "cv_pct": (100.0 * stdev / mean) if n and mean > 0 else None,
        "min_s": min(times) if n else None,
        "max_s": max(times) if n else None,
        "times_s": times,
    }


def _fmt(value: float) -> str:
    return f"{value:.4g}"


def decide(
    stats: dict[str, dict[str, Any]],
    gates: dict[str, Any],
    load_peak: float | None,
    ft_problems: list[str],
) -> tuple[str, float | None, list[str]]:
    """Returns (verdict, admitted ft_speedup or None, refusal reasons)."""
    reasons: list[str] = []
    short = [arm for arm in ARMS if stats[arm]["n"] < gates["min_runs"]]
    if short:
        counts = ", ".join(f"{arm} n={stats[arm]['n']}" for arm in short)
        reasons.append(f"runs: {counts} < min_runs {gates['min_runs']}")
    for arm in ARMS:
        cv = stats[arm]["cv_pct"]
        if cv is None or cv > gates["max_cv_pct"]:
            shown = "undefined" if cv is None else f"{cv:.2f}%"
            reasons.append(f"cv: {arm} {shown} > {_fmt(gates['max_cv_pct'])}%")
    if load_peak is None:
        reasons.append("load: the load average could not be read")
    elif load_peak > gates["max_load_1m"]:
        reasons.append(
            f"load: 1m load average peaked at {load_peak:.2f} > {_fmt(gates['max_load_1m'])}"
        )
    reasons.extend(ft_problems)
    g_median = stats["ghostty"]["median_s"]
    f_median = stats["frankenterm"]["median_s"]
    speedup = None
    if g_median and f_median:
        speedup = g_median / f_median
        if speedup == 1.0:
            reasons.append("tie: equal medians")
    else:
        reasons.append("timing: an arm has no positive median")
    if reasons:
        return f"NO_ADMISSIBLE_RATIO ({'; '.join(reasons)})", None, reasons
    assert speedup is not None
    return ("ft_faster" if speedup > 1.0 else "ghostty_faster"), speedup, []


def _round_files(row_dir: Path, rounds: int) -> list[Path]:
    files = [row_dir / f"round-{index:02d}.json" for index in range(1, rounds + 1)]
    missing = [str(path) for path in files if not path.is_file()]
    if missing:
        raise UsageError(f"missing hyperfine exports: {', '.join(missing)}")
    return files


def read_rounds(row_dir: Path, rounds: int) -> tuple[dict[str, list[float]], list[dict[str, Any]]]:
    """Per-arm timed samples and the per-round order/timings, from hyperfine
    exports whose commands are named after the arms."""
    times: dict[str, list[float]] = {arm: [] for arm in ARMS}
    per_round: list[dict[str, Any]] = []
    for index, path in enumerate(_round_files(row_dir, rounds), start=1):
        export = _read_json(path)
        results = export.get("results")
        if not isinstance(results, list) or len(results) != 2:
            raise UsageError(f"{path}: expected exactly two hyperfine results")
        order = [result.get("command") for result in results]
        if sorted(order) != sorted(ARMS):
            raise UsageError(f"{path}: commands are {order}, expected {list(ARMS)}")
        expected = list(ARMS) if index % 2 == 1 else list(reversed(ARMS))
        if order != expected:
            raise UsageError(f"{path}: round {index} ran {order}, ABBA expects {expected}")
        entry: dict[str, Any] = {"round": index, "order": order, "file": path.name}
        for result in results:
            arm = result["command"]
            samples = result.get("times")
            codes = result.get("exit_codes", [])
            if not isinstance(samples, list) or len(samples) != 1:
                raise UsageError(f"{path}: {arm} must have exactly one timed run per round")
            if any(code != 0 for code in codes):
                raise UsageError(f"{path}: {arm} exited with {codes}")
            value = float(samples[0])
            times[arm].append(value)
            entry[f"{arm}_s"] = value
            rss = result.get("memory_usage_byte")
            if isinstance(rss, list) and rss:
                entry[f"{arm}_peak_rss_bytes"] = rss[-1]
        per_round.append(entry)
    return times, per_round


FT_ADMISSION_FILES = ("ft-admission-pre.jsonl", "ft-admission-post.jsonl")


def read_ft_admission(
    row_dir: Path, expected_profile: str, rows: int, cols: int
) -> tuple[list[dict[str, Any]], list[str]]:
    """The FT arm's own JSON lines from the untimed runs before and after the
    timed rounds (hyperfine discards the timed runs' stdout), and every
    reason they make the row inadmissible. The timed runs themselves are
    held to the same sanity checks through their exit status: the bench
    exits 1 on a failed check and hyperfine aborts the row."""
    records: list[dict[str, Any]] = []
    problems: list[str] = []
    for name in FT_ADMISSION_FILES:
        path = row_dir / name
        if not path.is_file():
            problems.append(f"ft admission: {name} is missing")
            continue
        lines = [line for line in path.read_text().splitlines() if line.strip()]
        if len(lines) != 1:
            problems.append(f"ft admission: {name} holds {len(lines)} records, expected 1")
            continue
        record = json.loads(lines[0])
        records.append(record)
        if record.get("schema") != FT_LANE_SCHEMA:
            problems.append(f"ft admission: {name} has schema {record.get('schema')!r}")
        if record.get("lane") != "term":
            problems.append(f"ft admission: {name} ran lane {record.get('lane')!r}")
        if record.get("rows") != rows or record.get("cols") != cols:
            problems.append(
                f"ft admission: {name} ran {record.get('rows')}x{record.get('cols')}, "
                f"expected {rows}x{cols}"
            )
        if record.get("sanity") != "ok":
            problems.append(f"ft sanity: {record.get('sanity')}")
        if record.get("debug_assertions") is not False:
            problems.append("ft build: debug assertions are on")
        if record.get("profile") != expected_profile:
            problems.append(
                f"ft build: profile {record.get('profile')!r}, expected {expected_profile!r}"
            )
    fingerprints = {r.get("state_fingerprint") for r in records}
    if len(records) == len(FT_ADMISSION_FILES) and len(fingerprints) != 1:
        problems.append(
            "ft state: the runs before and after the rounds left different final states"
        )
    # De-duplicate while keeping order, so one bad build is reported once.
    return records, list(dict.fromkeys(problems))


def read_load(path: Path) -> list[dict[str, Any]]:
    samples = []
    if path.is_file():
        for line in path.read_text().splitlines():
            fields = line.split()
            if len(fields) >= 2:
                try:
                    samples.append({"at": fields[0], "load_1m": float(fields[1])})
                except ValueError:
                    samples.append({"at": fields[0], "load_1m": None})
    return samples


def build_row(args: argparse.Namespace) -> dict[str, Any]:
    row_dir = Path(args.dir)
    gates = _read_json(Path(args.gates))
    rounds = gates["rounds"]
    times, per_round = read_rounds(row_dir, rounds)
    admission, ft_problems = read_ft_admission(row_dir, args.ft_profile, args.rows, args.cols)
    load = read_load(row_dir / "load.txt")
    loads = [sample["load_1m"] for sample in load]
    load_peak = None if not loads or any(x is None for x in loads) else max(loads)
    stats = {arm: arm_stats(times[arm]) for arm in ARMS}
    internal = [float(r["secs"]) for r in admission if _is_number(r.get("secs"))]
    # The bench's own clock covers only the feed loop: no process start, file
    # read, input hash or final-state fingerprint. Kept for attribution, never
    # used in the verdict.
    stats["frankenterm"]["internal_lane_secs_untimed_runs"] = internal
    stats["frankenterm"]["state_fingerprints"] = sorted(
        {r["state_fingerprint"] for r in admission if r.get("state_fingerprint")}
    )
    for arm in ARMS:
        rss = [entry.get(f"{arm}_peak_rss_bytes") for entry in per_round]
        rss = [value for value in rss if isinstance(value, int)]
        stats[arm]["peak_rss_bytes_median"] = statistics.median(rss) if rss else None
    verdict, speedup, reasons = decide(stats, gates, load_peak, ft_problems)
    pairs = [(entry["ghostty_s"], entry["frankenterm_s"]) for entry in per_round]
    g_median, f_median = stats["ghostty"]["median_s"], stats["frankenterm"]["median_s"]
    return {
        "schema": ROW_SCHEMA,
        "corpus": args.corpus,
        "input_sha256": args.input_sha256,
        "geometry": {"rows": args.rows, "cols": args.cols},
        "ft_scrollback": args.ft_scrollback,
        "ft_chunk_bytes": args.ft_chunk,
        "commands": {"ghostty": args.ghostty_cmd, "frankenterm": args.ft_cmd},
        "arms": stats,
        "rounds": per_round,
        "load_1m": {"samples": load, "peak": load_peak},
        "paired": {
            "pairs": len(pairs),
            "ft_wins": sum(1 for g, f in pairs if f < g),
            "median_pair_speedup": statistics.median(g / f for g, f in pairs if f > 0)
            if pairs
            else None,
        },
        "ft_speedup": speedup,
        "ft_speedup_unadmitted": (g_median / f_median) if g_median and f_median else None,
        "hyperfine_json": [entry["file"] for entry in per_round],
        "verdict": verdict,
        "refusal_reasons": reasons,
    }


# --------------------------------------------------------------------------
# Emoji width parity


def written_rows(total_rows: int, rows: int, cursor_y: int) -> int:
    """Rows that received output: the screen starts with `rows` blank rows,
    and history only grows once the cursor sits on the last row."""
    return total_rows - (rows - 1 - cursor_y)


def compare_parity(
    ghostty: dict[str, Any],
    ft: dict[str, Any],
    rows: int,
    cols: int,
    ft_scrollback: int,
    line_feeds: int,
) -> dict[str, Any]:
    if ghostty.get("schema") != PROBE_SCHEMA:
        raise UsageError(f"ghostty probe output has schema {ghostty.get('schema')!r}")
    if ft.get("schema") != FT_LANE_SCHEMA:
        raise UsageError(f"FT probe output has schema {ft.get('schema')!r}")
    for name, record in (("ghostty", ghostty), ("frankenterm", ft)):
        if record.get("rows") != rows or record.get("cols") != cols:
            raise UsageError(
                f"{name} probe ran {record.get('rows')}x{record.get('cols')}, expected {rows}x{cols}"
            )
    g_written = written_rows(ghostty["total_rows"], rows, ghostty["cursor_y"])
    f_written = written_rows(ft["retained_rows"], rows, ft["cursor_y"])
    g_wraps = g_written - 1 - line_feeds
    f_wraps = f_written - 1 - line_feeds
    disagreements: list[str] = []
    saturated = ft["retained_rows"] >= rows + ft_scrollback
    if saturated:
        agree = None
        disagreements.append(
            f"undecided: FrankenTerm retained {ft['retained_rows']} rows, its scrollback cap "
            f"({rows} + {ft_scrollback}); rerun with a larger --scrollback"
        )
    else:
        if ft.get("sanity") != "ok":
            disagreements.append(f"FrankenTerm probe sanity: {ft.get('sanity')}")
        if g_written != f_written:
            disagreements.append(
                f"rows written: ghostty {g_written}, frankenterm {f_written} "
                f"(wraps {g_wraps} vs {f_wraps})"
            )
        if ghostty["cursor_x"] != ft["cursor_x"]:
            disagreements.append(
                f"final cursor column: ghostty {ghostty['cursor_x']}, frankenterm {ft['cursor_x']}"
            )
        agree = not disagreements
    return {
        "schema": PARITY_SCHEMA,
        "geometry": {"rows": rows, "cols": cols},
        "line_feeds": line_feeds,
        "ghostty": {
            "cursor_x": ghostty["cursor_x"],
            "cursor_y": ghostty["cursor_y"],
            "pending_wrap": ghostty.get("pending_wrap"),
            "total_rows": ghostty["total_rows"],
            "written_rows": g_written,
            "wraps": g_wraps,
            "wrap_flagged_rows": ghostty.get("wrapped_rows"),
            "bench_equivalent_total_rows": ghostty.get("bench_equivalent_total_rows"),
        },
        "frankenterm": {
            "cursor_x": ft["cursor_x"],
            "cursor_y": ft["cursor_y"],
            "retained_rows": ft["retained_rows"],
            "written_rows": f_written,
            "wraps": f_wraps,
            "scrollback": ft_scrollback,
            "state_fingerprint": ft.get("state_fingerprint"),
            "sanity": ft.get("sanity"),
        },
        "ghostty_wrap_flags_consistent": ghostty.get("wrapped_rows") == g_wraps,
        "agree": agree,
        "disagreements": disagreements,
    }


def count_line_feeds(path: Path) -> int:
    count = 0
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            count += block.count(b"\n")
    return count


def _single_json_line(path: Path) -> dict[str, Any]:
    lines = [line for line in path.read_text().splitlines() if line.strip()]
    if len(lines) != 1:
        raise UsageError(f"{path}: expected one JSON line, found {len(lines)}")
    return json.loads(lines[0])


# --------------------------------------------------------------------------
# Receipt


def primary_verdict(rows: list[dict[str, Any]], primary_corpus: str) -> str:
    for row in rows:
        if row["corpus"] == primary_corpus:
            return row["verdict"]
    return (
        f"NO_ADMISSIBLE_RATIO (primary corpus {primary_corpus} was not measured; "
        f"first row is {rows[0]['corpus']})"
        if rows
        else "NO_ADMISSIBLE_RATIO (no rows were measured)"
    )


def assemble_receipt(args: argparse.Namespace) -> dict[str, Any]:
    facts = _read_json(Path(args.facts))
    pins = _read_json(Path(args.pins))
    gates = _read_json(Path(args.gates))
    rows = [_read_json(Path(path)) for path in args.row]
    parity = [_read_json(Path(path)) for path in args.parity]
    inputs = [json.loads(line) for line in Path(args.inputs).read_text().splitlines() if line.strip()]
    receipt = {
        "schema": RECEIPT_SCHEMA,
        "bead": "ft-yccm0.1.3",
        "mode": args.mode,
        **facts,
        "pins": pins,
        "gates": gates,
        "inputs": inputs,
        "rows": rows,
        "emoji_width_parity": parity,
        "primary": {"corpus": pins["primary_corpus"]},
        "verdict": primary_verdict(rows, pins["primary_corpus"]),
    }
    return receipt


def _require(errors: list[str], condition: bool, message: str) -> bool:
    if not condition:
        errors.append(message)
    return condition


def _is_number(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def validate_receipt(receipt: Any, root: Path | None) -> list[str]:
    errors: list[str] = []
    if not _require(errors, isinstance(receipt, dict), "the receipt is not a JSON object"):
        return errors
    _require(errors, receipt.get("schema") == RECEIPT_SCHEMA, f"schema must be {RECEIPT_SCHEMA}")
    _require(errors, receipt.get("mode") in ("measure", "self-test"), "mode must be measure or self-test")
    for key in ("started_utc", "finished_utc"):
        _require(errors, isinstance(receipt.get(key), str), f"{key} must be a string")
    contract = receipt.get("contract")
    if _require(errors, isinstance(contract, dict), "contract must be an object"):
        _require(errors, HEX64.match(str(contract.get("sha256", ""))) is not None, "contract.sha256 must be a SHA-256")
    pins = receipt.get("pins")
    if _require(errors, isinstance(pins, dict), "pins must be an object"):
        missing = [key for key in PIN_SPEC if key not in pins]
        _require(errors, not missing, f"pins lack {', '.join(missing)}")
    gates = receipt.get("gates")
    if _require(errors, isinstance(gates, dict), "gates must be an object"):
        for key in ("max_cv_pct", "max_load_1m", "min_runs", "rounds", "warmup"):
            _require(errors, _is_number(gates.get(key)), f"gates.{key} must be a number")
        _require(errors, gates.get("order") == "ABBA", "gates.order must be ABBA")
    _require(errors, isinstance(receipt.get("fingerprint"), dict), "fingerprint must be an object")
    binaries = receipt.get("binaries")
    if _require(errors, isinstance(binaries, dict), "binaries must be an object"):
        for name in ("ghostty_bench", "frankenterm", "ghostty_probe"):
            entry = binaries.get(name)
            if _require(errors, isinstance(entry, dict), f"binaries.{name} must be an object"):
                _require(errors, HEX64.match(str(entry.get("sha256", ""))) is not None, f"binaries.{name}.sha256 must be a SHA-256")
                _require(errors, isinstance(entry.get("path"), str), f"binaries.{name}.path must be a string")
    ghostty = receipt.get("ghostty")
    if _require(errors, isinstance(ghostty, dict), "ghostty must be an object"):
        _require(errors, HEX40.match(str(ghostty.get("commit", ""))) is not None, "ghostty.commit must be a git SHA")
        _require(errors, ghostty.get("checkout_clean") is True, "ghostty.checkout_clean must be true")
        if isinstance(pins, dict):
            _require(errors, ghostty.get("commit") == pins.get("ghostty_commit"), "ghostty.commit differs from the pin")
    inputs = receipt.get("inputs")
    input_shas: set[str] = set()
    if _require(errors, isinstance(inputs, list) and bool(inputs), "inputs must be a non-empty list"):
        for index, entry in enumerate(inputs):
            if not _require(errors, isinstance(entry, dict), f"inputs[{index}] must be an object"):
                continue
            _require(errors, isinstance(entry.get("corpus"), str), f"inputs[{index}].corpus must be a string")
            _require(errors, isinstance(entry.get("path"), str), f"inputs[{index}].path must be a string")
            _require(errors, isinstance(entry.get("bytes"), int) and entry["bytes"] > 0, f"inputs[{index}].bytes must be positive")
            if _require(errors, HEX64.match(str(entry.get("sha256", ""))) is not None, f"inputs[{index}].sha256 must be a SHA-256"):
                input_shas.add(entry["sha256"])
    rows = receipt.get("rows")
    if _require(errors, isinstance(rows, list) and bool(rows), "rows must be a non-empty list"):
        for index, row in enumerate(rows):
            errors.extend(_validate_row(row, index, input_shas, root))
        if isinstance(pins, dict) and isinstance(inputs, list) and inputs and isinstance(inputs[0], dict):
            corpora = [entry.get("corpus") for entry in inputs if isinstance(entry, dict)]
            if pins.get("primary_corpus") in corpora:
                _require(errors, inputs[0].get("corpus") == pins["primary_corpus"], "the primary corpus must be listed first")
                _require(errors, rows[0].get("corpus") == pins["primary_corpus"], "the primary corpus's row must come first")
    verdict = receipt.get("verdict")
    _require(errors, isinstance(verdict, str) and VERDICT_RE.match(verdict) is not None, f"verdict {verdict!r} is not ft_faster, ghostty_faster or NO_ADMISSIBLE_RATIO (reason)")
    if isinstance(rows, list) and rows and isinstance(pins, dict) and all(isinstance(r, dict) and "verdict" in r and "corpus" in r for r in rows):
        _require(errors, verdict == primary_verdict(rows, pins.get("primary_corpus", "")), "verdict must equal the primary corpus row's verdict")
    parity = receipt.get("emoji_width_parity")
    if _require(errors, isinstance(parity, list), "emoji_width_parity must be a list"):
        if isinstance(pins, dict) and isinstance(rows, list):
            measured = {r.get("corpus") for r in rows if isinstance(r, dict)}
            if pins.get("primary_corpus") in measured:
                _require(errors, bool(parity), "emoji width parity must be checked when the primary corpus is measured")
        for index, entry in enumerate(parity):
            if not _require(errors, isinstance(entry, dict), f"emoji_width_parity[{index}] must be an object"):
                continue
            _require(errors, entry.get("schema") == PARITY_SCHEMA, f"emoji_width_parity[{index}].schema must be {PARITY_SCHEMA}")
            _require(errors, entry.get("agree") in (True, False, None), f"emoji_width_parity[{index}].agree must be a boolean or null")
            _require(errors, isinstance(entry.get("disagreements"), list), f"emoji_width_parity[{index}].disagreements must be a list")
            if entry.get("agree") is False:
                _require(errors, bool(entry.get("disagreements")), f"emoji_width_parity[{index}] disagrees without saying how")
    return errors


def _validate_row(row: Any, index: int, input_shas: set[str], root: Path | None) -> list[str]:
    errors: list[str] = []
    where = f"rows[{index}]"
    if not _require(errors, isinstance(row, dict), f"{where} must be an object"):
        return errors
    _require(errors, row.get("schema") == ROW_SCHEMA, f"{where}.schema must be {ROW_SCHEMA}")
    _require(errors, isinstance(row.get("corpus"), str), f"{where}.corpus must be a string")
    _require(errors, row.get("input_sha256") in input_shas, f"{where}.input_sha256 is not one of the inputs")
    geometry = row.get("geometry")
    _require(errors, isinstance(geometry, dict) and isinstance(geometry.get("rows"), int) and isinstance(geometry.get("cols"), int), f"{where}.geometry must hold integer rows and cols")
    arms = row.get("arms")
    if _require(errors, isinstance(arms, dict), f"{where}.arms must be an object"):
        for arm in ARMS:
            stats = arms.get(arm)
            if not _require(errors, isinstance(stats, dict), f"{where}.arms.{arm} must be an object"):
                continue
            times = stats.get("times_s")
            if _require(errors, isinstance(times, list) and all(_is_number(t) and t > 0 for t in times), f"{where}.arms.{arm}.times_s must be positive numbers"):
                _require(errors, stats.get("n") == len(times), f"{where}.arms.{arm}.n must equal len(times_s)")
            for key in ("median_s", "p95_s", "cv_pct"):
                _require(errors, _is_number(stats.get(key)), f"{where}.arms.{arm}.{key} must be a number")
    verdict = row.get("verdict")
    if _require(errors, isinstance(verdict, str) and VERDICT_RE.match(verdict) is not None, f"{where}.verdict {verdict!r} is not a recognised verdict"):
        if verdict.startswith("NO_ADMISSIBLE_RATIO"):
            _require(errors, row.get("ft_speedup") is None, f"{where} refused a verdict but still reports ft_speedup")
        else:
            speedup = row.get("ft_speedup")
            if _require(errors, _is_number(speedup), f"{where}.ft_speedup must be a number for an admitted verdict"):
                _require(errors, (speedup > 1.0) == (verdict == "ft_faster"), f"{where}.verdict contradicts ft_speedup {speedup}")
    files = row.get("hyperfine_json")
    if _require(errors, isinstance(files, list) and bool(files), f"{where}.hyperfine_json must list the retained exports"):
        if root is not None and isinstance(row.get("dir"), str):
            for name in files:
                _require(errors, (root / row["dir"] / name).is_file(), f"{where}: retained export {row['dir']}/{name} is missing")
    return errors


# --------------------------------------------------------------------------
# CLI


def _cmd_pins(args: argparse.Namespace) -> int:
    text = Path(args.contract).read_text()
    pins = parse_pins(text)
    json.dump(pins, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


def _cmd_pin(args: argparse.Namespace) -> int:
    print(pin_as_text(_read_json(Path(args.pins)), args.key))
    return 0


def _optional_number(text: str | None, kind: type) -> Any:
    if text is None or text == "":
        return None
    try:
        return kind(text)
    except ValueError:
        raise UsageError(f"{text!r} is not a valid {kind.__name__}") from None


def _cmd_gates(args: argparse.Namespace) -> int:
    pins = _read_json(Path(args.pins))
    gates = resolve_gates(
        pins,
        _optional_number(args.max_cv, float),
        _optional_number(args.max_load, float),
        _optional_number(args.rounds, int),
        _optional_number(args.warmup, int),
        args.self_test,
    )
    json.dump(gates, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


def _cmd_app_info(args: argparse.Namespace) -> int:
    pins = _read_json(Path(args.pins))
    app = Path(args.app)
    try:
        info = app_info(app, pins["app_binary"])
    except (OSError, plistlib.InvalidFileException, ValueError) as error:
        print(f"cannot read {app}: {error}", file=sys.stderr)
        return 3
    json.dump(info, sys.stdout, indent=2)
    sys.stdout.write("\n")
    drift = app_drift(pins, info)
    for line in drift:
        print(f"app pin drift: {line}", file=sys.stderr)
    return 3 if drift else 0


def _cmd_loadavg(_: argparse.Namespace) -> int:
    loads = load_averages()
    if loads is None:
        print("unknown")
        return 1
    print(" ".join(f"{value:.2f}" for value in loads))
    return 0


def _cmd_fingerprint(args: argparse.Namespace) -> int:
    json.dump(fingerprint(args.zig), sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


def _cmd_fact(args: argparse.Namespace) -> int:
    value: Any = args.value
    if args.type == "int":
        value = int(value)
    elif args.type == "bool":
        if value not in ("true", "false"):
            raise UsageError(f"{value!r} is not true or false")
        value = value == "true"
    elif args.type == "json":
        value = json.loads(Path(value).read_text())
    update_facts(Path(args.facts), args.key, value)
    return 0


def _cmd_sha256(args: argparse.Namespace) -> int:
    print(sha256_file(Path(args.path)))
    return 0


def _cmd_tree_digest(args: argparse.Namespace) -> int:
    root = Path(args.dir)
    if not root.is_dir():
        raise UsageError(f"{root} is not a directory")
    count, digest = tree_digest(root)
    print(f"{count} {digest}")
    return 0


def collect_inputs(
    generated: list[dict[str, Any]], files: list[Path], primary: str
) -> list[dict[str, Any]]:
    """The inputs both arms read, primary corpus first. Every file is hashed
    here, independently of the FrankenTerm generator that wrote it, so the
    receipt's identity claim does not rest on one binary's say-so."""
    inputs: list[dict[str, Any]] = []
    for record in generated:
        path = Path(record["corpus_path"])
        sha = sha256_file(path)
        if sha != record["corpus_sha256"]:
            raise UsageError(
                f"{path}: SHA-256 {sha} differs from the generator's {record['corpus_sha256']}"
            )
        inputs.append(
            {
                "corpus": record["corpus"],
                "path": str(path),
                "bytes": path.stat().st_size,
                "sha256": sha,
                "source": "generated",
                "seed": record.get("seed"),
            }
        )
    for path in files:
        inputs.append(
            {
                "corpus": "file:" + path.name,
                "path": str(path.resolve()),
                "bytes": path.stat().st_size,
                "sha256": sha256_file(path),
                "source": "file",
                "seed": None,
            }
        )
    names = [entry["corpus"] for entry in inputs]
    duplicates = sorted({name for name in names if names.count(name) > 1})
    if duplicates:
        raise UsageError(f"inputs repeat: {', '.join(duplicates)}")
    empty = [entry["path"] for entry in inputs if entry["bytes"] == 0]
    if empty:
        raise UsageError(f"empty inputs: {', '.join(empty)}")
    inputs.sort(key=lambda entry: entry["corpus"] != primary)
    return inputs


def _cmd_inputs(args: argparse.Namespace) -> int:
    generated = []
    if args.gen_jsonl:
        for line in Path(args.gen_jsonl).read_text().splitlines():
            if line.strip():
                record = json.loads(line)
                if record.get("schema") != "ft.bench.ingest-corpus.v1":
                    raise UsageError(f"unexpected generator record {record.get('schema')!r}")
                generated.append(record)
    inputs = collect_inputs(generated, [Path(p) for p in args.file], args.primary)
    if not inputs:
        raise UsageError("no inputs")
    with Path(args.out).open("w") as handle:
        for entry in inputs:
            handle.write(json.dumps(entry) + "\n")
    for entry in inputs:
        if "\t" in entry["path"] or "\n" in entry["path"]:
            raise UsageError(f"input path {entry['path']!r} contains a tab or newline")
        print(f"{entry['corpus']}\t{entry['path']}\t{entry['sha256']}")
    return 0


def _cmd_get(args: argparse.Namespace) -> int:
    node: Any = _read_json(Path(args.json))
    for part in args.key.split("."):
        if isinstance(node, list):
            node = node[int(part)]
        elif isinstance(node, dict) and part in node:
            node = node[part]
        else:
            raise UsageError(f"{args.json} has no {args.key!r}")
    print(node if isinstance(node, str) else json.dumps(node))
    return 0


def _cmd_row(args: argparse.Namespace) -> int:
    row = build_row(args)
    row["dir"] = args.rel_dir
    _write_json(Path(args.dir) / "row.json", row)
    g, f = row["arms"]["ghostty"], row["arms"]["frankenterm"]
    print(
        f"ROW corpus={row['corpus']} geometry={args.rows}x{args.cols} "
        f"ghostty_median={g['median_s']:.4f}s cv={_pct(g['cv_pct'])} "
        f"frankenterm_median={f['median_s']:.4f}s cv={_pct(f['cv_pct'])} "
        f"load_peak={row['load_1m']['peak']} verdict={row['verdict']}"
    )
    return 0


def _pct(value: float | None) -> str:
    return "undefined" if value is None else f"{value:.2f}%"


def _cmd_parity(args: argparse.Namespace) -> int:
    ghostty = _single_json_line(Path(args.ghostty))
    ft = _single_json_line(Path(args.ft))
    result = compare_parity(
        ghostty, ft, args.rows, args.cols, args.ft_scrollback, count_line_feeds(Path(args.corpus_file))
    )
    result["corpus"] = args.corpus
    _write_json(Path(args.out), result)
    print(
        f"PARITY corpus={args.corpus} geometry={args.rows}x{args.cols} agree={result['agree']} "
        f"ghostty_rows_written={result['ghostty']['written_rows']} "
        f"frankenterm_rows_written={result['frankenterm']['written_rows']} "
        f"ghostty_cursor_x={result['ghostty']['cursor_x']} "
        f"frankenterm_cursor_x={result['frankenterm']['cursor_x']}"
        + ("" if not result["disagreements"] else " | " + "; ".join(result["disagreements"]))
    )
    return 0


def _cmd_receipt(args: argparse.Namespace) -> int:
    receipt = assemble_receipt(args)
    _write_json(Path(args.out), receipt)
    return 0


def _cmd_validate(args: argparse.Namespace) -> int:
    path = Path(args.receipt)
    errors = validate_receipt(_read_json(path), path.parent)
    for error in errors:
        print(f"receipt invalid: {error}", file=sys.stderr)
    if errors:
        return 1
    print(f"receipt valid: {path}")
    return 0


def _cmd_verdicts(args: argparse.Namespace) -> int:
    receipt = _read_json(Path(args.receipt))
    for row in receipt["rows"]:
        geometry = row["geometry"]
        speedup = row.get("ft_speedup")
        shown = "" if speedup is None else f" ft_speedup={speedup:.3f}"
        print(f"VERDICT corpus={row['corpus']} geometry={geometry['rows']}x{geometry['cols']}{shown} {row['verdict']}")
    print(f"VERDICT primary={receipt['primary']['corpus']} {receipt['verdict']}")
    return 0 if all(not row["verdict"].startswith("NO_ADMISSIBLE_RATIO") for row in receipt["rows"]) else 5


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("pins")
    p.add_argument("contract")
    p.set_defaults(func=_cmd_pins)

    p = sub.add_parser("pin")
    p.add_argument("pins")
    p.add_argument("key")
    p.set_defaults(func=_cmd_pin)

    p = sub.add_parser("gates")
    p.add_argument("pins")
    p.add_argument("--max-cv")
    p.add_argument("--max-load")
    p.add_argument("--rounds")
    p.add_argument("--warmup")
    p.add_argument("--self-test", action="store_true")
    p.set_defaults(func=_cmd_gates)

    p = sub.add_parser("app-info")
    p.add_argument("pins")
    p.add_argument("app")
    p.set_defaults(func=_cmd_app_info)

    p = sub.add_parser("loadavg")
    p.set_defaults(func=_cmd_loadavg)

    p = sub.add_parser("fingerprint")
    p.add_argument("--zig")
    p.set_defaults(func=_cmd_fingerprint)

    p = sub.add_parser("fact")
    p.add_argument("facts")
    p.add_argument("key")
    p.add_argument("value")
    p.add_argument("--type", choices=("str", "int", "bool", "json"), default="str")
    p.set_defaults(func=_cmd_fact)

    p = sub.add_parser("sha256")
    p.add_argument("path")
    p.set_defaults(func=_cmd_sha256)

    p = sub.add_parser("tree-digest")
    p.add_argument("dir")
    p.set_defaults(func=_cmd_tree_digest)

    p = sub.add_parser("inputs")
    p.add_argument("--gen-jsonl")
    p.add_argument("--file", action="append", default=[])
    p.add_argument("--primary", required=True)
    p.add_argument("--out", required=True)
    p.set_defaults(func=_cmd_inputs)

    p = sub.add_parser("get")
    p.add_argument("json")
    p.add_argument("key")
    p.set_defaults(func=_cmd_get)

    p = sub.add_parser("row")
    p.add_argument("--dir", required=True)
    p.add_argument("--rel-dir", required=True)
    p.add_argument("--gates", required=True)
    p.add_argument("--corpus", required=True)
    p.add_argument("--input-sha256", required=True)
    p.add_argument("--rows", type=int, required=True)
    p.add_argument("--cols", type=int, required=True)
    p.add_argument("--ft-scrollback", type=int, required=True)
    p.add_argument("--ft-chunk", type=int, required=True)
    p.add_argument("--ft-profile", required=True)
    p.add_argument("--ghostty-cmd", required=True)
    p.add_argument("--ft-cmd", required=True)
    p.set_defaults(func=_cmd_row)

    p = sub.add_parser("parity")
    p.add_argument("--corpus", required=True)
    p.add_argument("--corpus-file", required=True)
    p.add_argument("--ghostty", required=True)
    p.add_argument("--ft", required=True)
    p.add_argument("--rows", type=int, required=True)
    p.add_argument("--cols", type=int, required=True)
    p.add_argument("--ft-scrollback", type=int, required=True)
    p.add_argument("--out", required=True)
    p.set_defaults(func=_cmd_parity)

    p = sub.add_parser("receipt")
    p.add_argument("--mode", choices=("measure", "self-test"), required=True)
    p.add_argument("--facts", required=True)
    p.add_argument("--pins", required=True)
    p.add_argument("--gates", required=True)
    p.add_argument("--inputs", required=True)
    p.add_argument("--row", action="append", default=[])
    p.add_argument("--parity", action="append", default=[])
    p.add_argument("--out", required=True)
    p.set_defaults(func=_cmd_receipt)

    p = sub.add_parser("validate")
    p.add_argument("receipt")
    p.set_defaults(func=_cmd_validate)

    p = sub.add_parser("verdicts")
    p.add_argument("receipt")
    p.set_defaults(func=_cmd_verdicts)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except PinError as error:
        print(f"contract pins: {error}", file=sys.stderr)
        return 3
    except UsageError as error:
        print(f"ghostty_h2h: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
