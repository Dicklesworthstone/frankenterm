#!/usr/bin/env python3
"""Fail-closed 50-pane sustained-load evidence harness and offline verifier.

Bead: ft-d0ez0.5

The bundled fake WezTerm transport is useful for exercising the harness, but it
is never authoritative performance evidence.  Only an explicitly authorized,
isolated native-mux report can pass the verifier.  This script does not target
an operator's live workspace or session.

Phases:
  1. Warm-up: Spawn 50 panes, verify all discovered.
  2. Sustained load: All panes emit continuous output for configurable duration.
  3. Detection probe: Inject rate-limit pattern, measure detection latency.
  4. Robot benchmark: Time robot state, search, get-text under load.
  5. Cooldown: Capture final metrics and verify assertions.

Authoritative gates:
  - All 50 panes discovered within timeout.
  - ft process RSS stays under 1 GB over the run.
  - Exact rule/pane pattern detection latency is below 5 seconds.
  - Fleet pressure transitions from Normal to above Normal.
  - Authoritative mux counters record a hot-to-warm scrollback transition.
  - Robot state response < 2 seconds under load.
  - Robot search response < 5 seconds under load.
  - No crashes or panics during the run.
  - Exact source, binary, configuration, hardware, workload, and authorization
    provenance are retained in a bounded structured report.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import re
import shlex
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[2]
OUTPUT_DIR = REPO_ROOT / "tests" / "e2e" / "output"
SKIP_EXIT_CODE = 77
REPORT_CONTRACT_ID = "ft.e2e_50pane_stress"
REPORT_SCHEMA_VERSION = 4
VERIFIER_VERSION = "ft.e2e_50pane_stress.verifier.v3"
AUTHORIZATION_RECEIPT_SCHEMA_VERSION = 2
AUTHORIZATION_SCOPE = "ft-d0ez0.5.native-isolated-50-pane.v2"
FULL_DURATION_SECS = 300.0
DEFAULT_DURATION_SECS = FULL_DURATION_SECS
DEFAULT_TIMEOUT_SECS = 30.0
NUM_PANES = 50
MAX_REPORT_BYTES = 16 * 1024 * 1024
MAX_TELEMETRY_SAMPLES = 256
MAX_RUNTIME_LOG_SCAN_BYTES = 16 * 1024 * 1024
MAX_COMMAND_OUTPUT_CHARS = 64 * 1024
DETECTION_LATENCY_LIMIT_MS = 5_000.0
ROBOT_STATE_LATENCY_LIMIT_MS = 2_000.0
ROBOT_SEARCH_LATENCY_LIMIT_MS = 5_000.0
RSS_LIMIT_KB = 1024 * 1024
RSS_SAMPLE_INTERVAL_SECS = 10.0
RSS_MAX_SAMPLE_GAP_SECS = 15.0
RSS_FINAL_COVERAGE_SLACK_SECS = 2.0
MAX_SUPPORTED_DURATION_SECS = (
    MAX_TELEMETRY_SAMPLES - 2
) * RSS_SAMPLE_INTERVAL_SECS
DETECTION_RULE_ID = "codex.usage.warning_25"
NORMAL_LINES_PER_SECOND_PER_PANE = 2.0
HIGH_LINES_PER_SECOND_PER_PANE = 20.0
HIGH_OUTPUT_PHASE_FRACTION = 0.6
HIGH_OUTPUT_PHASE_TOLERANCE_SECS = 15.0
OUTPUT_RATE_MINIMUM_FRACTION = 0.85
OUTPUT_PAYLOAD_BYTES_PER_LINE = 448
FLEET_SCROLLBACK_PER_PANE_BUDGET_BYTES = 512 * 1024
FLEET_SCROLLBACK_HIGH_RATIO = 0.8
MUX_SCROLLBACK_HOT_LINES = 1_000
DETECTION_PROBE_PHASE_FRACTION = 0.8
DETECTION_PROBE_PHASE_TOLERANCE_SECS = 15.0
ROBOT_BENCHMARK_PHASE_FRACTION = 0.9
ROBOT_BENCHMARK_PHASE_TOLERANCE_SECS = 15.0
AUTHORITATIVE_BUILD_PROFILES = {"release-interactive", "release-perf"}
AUTHORITATIVE_BINARY_RESOLUTION_PREFIXES = {
    "cargo_target_dir",
    "env_ft_binary",
    "repo_target",
}
BINARY_VERSION_PATTERN = re.compile(
    r"\Aft [^\s]+ \((?P<git_sha>[0-9a-f]{40})(?P<dirty>\+dirty)?\)\Z"
)
DETECTION_PATTERN_TEXT = (
    "Warning: You have less than 25% of your 8h limit remaining. "
    "Usage: 24% of your 8h limit remaining."
)

# Pane generating high output (for pressure differentiation)
HIGH_OUTPUT_PANES = list(range(41, 51))  # last 10 panes get 10x output rate
DETECTION_PROBE_PANE_INDEX = 25  # pane to inject pattern into

CONFIG_TEMPLATE = """\
[ingest]
poll_interval_ms = 200
batch_size = 50
max_segment_bytes = 65536

[fleet_scrollback]
enabled = true
per_pane_budget_bytes = 524288
high_ratio = 0.8

[storage]
retention_days = 1

[logging]
level = "info"
format = "json"

[safety]
require_prompt_active = false
block_alt_screen = false
"""


FAKE_WEZTERM_SCRIPT = """\
#!/usr/bin/env python3
from __future__ import annotations

import contextlib
import fcntl
import json
import os
import pathlib
import signal
import subprocess
import sys
from typing import Any


STATE_ROOT = pathlib.Path(os.environ["FT_FAKE_WEZTERM_STATE"]).resolve()
STATE_ROOT.mkdir(parents=True, exist_ok=True)
LOCK_PATH = STATE_ROOT / "lock"
STATE_PATH = STATE_ROOT / "state.json"
LOG_DIR = STATE_ROOT / "logs"
LOG_DIR.mkdir(parents=True, exist_ok=True)
MAX_SCROLLBACK_LINES = 3500
MAX_SCROLLBACK_CAPTURE_BYTES = 4 * 1024 * 1024


def load_state() -> dict[str, Any]:
    if not STATE_PATH.exists():
        return {"next_pane_id": 1, "panes": {}, "active_pane_id": None}
    return json.loads(STATE_PATH.read_text(encoding="utf-8"))


def save_state(state: dict[str, Any]) -> None:
    tmp_path = STATE_PATH.with_suffix(".tmp")
    tmp_path.write_text(json.dumps(state, indent=2, sort_keys=True), encoding="utf-8")
    tmp_path.replace(STATE_PATH)


def process_alive(pid: int | None) -> bool:
    if pid is None:
        return False
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def cleanup_dead_panes(state: dict[str, Any]) -> None:
    active = state.get("active_pane_id")
    for pane_id, pane in state.get("panes", {}).items():
        pane["alive"] = process_alive(pane.get("pid"))
        if not pane["alive"] and active == int(pane_id):
            active = None
    state["active_pane_id"] = active


def pane_json(pane_id: int, pane: dict[str, Any], active_pane_id: int | None) -> dict[str, Any]:
    return {
        "pane_id": pane_id,
        "tab_id": pane.get("tab_id", pane_id),
        "window_id": pane.get("window_id", 1),
        "domain_name": pane.get("domain_name", "local"),
        "workspace": pane.get("workspace", "default"),
        "title": pane.get("title"),
        "cwd": pane.get("cwd"),
        "is_active": active_pane_id == pane_id,
        "is_zoomed": False,
        "size": {"rows": 24, "cols": 80},
        "cursor_x": 0,
        "cursor_y": 0,
    }


def parse_flag_value(args: list[str], name: str) -> str | None:
    if name in args:
        idx = args.index(name)
        if idx + 1 < len(args):
            return args[idx + 1]
    return None


def split_command(args: list[str]) -> tuple[list[str], list[str]]:
    if "--" not in args:
        return args, []
    idx = args.index("--")
    return args[:idx], args[idx + 1:]


def read_bounded_scrollback(log_path: pathlib.Path) -> str:
    if not log_path.exists():
        return ""
    with log_path.open("rb") as handle:
        size = handle.seek(0, os.SEEK_END)
        start = max(0, size - MAX_SCROLLBACK_CAPTURE_BYTES)
        handle.seek(start)
        raw = handle.read(MAX_SCROLLBACK_CAPTURE_BYTES)
    if start > 0:
        newline = raw.find(b"\\n")
        raw = raw[newline + 1:] if newline >= 0 else b""
    lines = raw.splitlines(keepends=True)[-MAX_SCROLLBACK_LINES:]
    return b"".join(lines).decode("utf-8", errors="replace")


def main() -> int:
    argv = sys.argv[1:]
    if argv == ["--version"]:
        print("wezterm 2026.04.11 ft-e2e-50pane-stress-fake")
        return 0

    if not argv or argv[0] != "cli":
        return 0

    subcommand = argv[1] if len(argv) > 1 else ""
    args = argv[2:]

    with LOCK_PATH.open("a+", encoding="utf-8") as lock_file:
        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        state = load_state()
        cleanup_dead_panes(state)

        if subcommand == "list":
            panes = []
            active_pane_id = state.get("active_pane_id")
            for pane_id_str in sorted(state.get("panes", {}), key=lambda item: int(item)):
                pane = state["panes"][pane_id_str]
                if pane.get("alive", True):
                    panes.append(pane_json(int(pane_id_str), pane, active_pane_id))
            save_state(state)
            print(json.dumps(panes))
            return 0

        if subcommand == "get-text":
            pane_id = parse_flag_value(args, "--pane-id")
            if pane_id is None:
                print("missing --pane-id", file=sys.stderr)
                return 1
            pane = state.get("panes", {}).get(str(int(pane_id)))
            if not pane:
                print(f"pane {pane_id} not found", file=sys.stderr)
                return 1
            save_state(state)
            log_path = pathlib.Path(pane["log_path"])
            sys.stdout.write(read_bounded_scrollback(log_path))
            return 0

        if subcommand == "spawn":
            flag_args, command = split_command(args)
            cwd = parse_flag_value(flag_args, "--cwd") or os.getcwd()
            if not command:
                print("spawn requires command after --", file=sys.stderr)
                return 1

            pane_id = int(state.get("next_pane_id", 1))
            state["next_pane_id"] = pane_id + 1

            log_path = (LOG_DIR / f"pane_{pane_id}.log").resolve()
            log_handle = log_path.open("a", encoding="utf-8")
            process = subprocess.Popen(
                command,
                cwd=cwd,
                stdin=subprocess.DEVNULL,
                stdout=log_handle,
                stderr=subprocess.STDOUT,
                text=True,
                preexec_fn=os.setsid,
            )
            state["panes"][str(pane_id)] = {
                "pid": process.pid,
                "log_path": str(log_path),
                "title": f"stress_pane_{pane_id:02d}",
                "cwd": cwd,
                "domain_name": "local",
                "workspace": "default",
                "tab_id": pane_id,
                "window_id": 1,
                "alive": True,
            }
            if state.get("active_pane_id") is None:
                state["active_pane_id"] = pane_id
            save_state(state)
            print(pane_id)
            return 0

        if subcommand == "send-text":
            pane_id = parse_flag_value(args, "--pane-id")
            if pane_id is None:
                print("missing --pane-id", file=sys.stderr)
                return 1
            pane = state.get("panes", {}).get(str(int(pane_id)))
            if not pane:
                print(f"pane {pane_id} not found", file=sys.stderr)
                return 1
            text_parts = []
            if "--" in args:
                idx = args.index("--")
                text_parts = args[idx + 1:]
            else:
                t = parse_flag_value(args, "--text")
                if t:
                    text_parts = [t]
            log_path = pathlib.Path(pane["log_path"])
            with log_path.open("a", encoding="utf-8") as fh:
                fh.write("\\n".join(text_parts) + "\\n")
            save_state(state)
            return 0

        if subcommand == "kill-pane":
            pane_id = parse_flag_value(args, "--pane-id")
            if pane_id is None:
                print("missing --pane-id", file=sys.stderr)
                return 1
            pane = state.get("panes", {}).get(str(int(pane_id)))
            if not pane:
                save_state(state)
                return 0
            pid = pane.get("pid")
            if pid and process_alive(pid):
                with contextlib.suppress(OSError):
                    os.killpg(os.getpgid(pid), signal.SIGTERM)
            pane["alive"] = False
            if state.get("active_pane_id") == int(pane_id):
                state["active_pane_id"] = None
            save_state(state)
            return 0

        if subcommand == "activate-pane":
            pane_id = parse_flag_value(args, "--pane-id")
            if pane_id is not None:
                state["active_pane_id"] = int(pane_id)
            save_state(state)
            return 0

        save_state(state)
        return 0


if __name__ == "__main__":
    sys.exit(main())
"""


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


@dataclass
class CmdResult:
    argv: list[str]
    returncode: int
    stdout: str
    stderr: str
    duration_ms: int

    def as_dict(self) -> dict[str, Any]:
        return {
            "argv": self.argv,
            "cmd": " ".join(shlex.quote(arg) for arg in self.argv),
            "returncode": self.returncode,
            "stdout": self.stdout[:MAX_COMMAND_OUTPUT_CHARS],
            "stderr": self.stderr[:MAX_COMMAND_OUTPUT_CHARS],
            "stdout_truncated": len(self.stdout) > MAX_COMMAND_OUTPUT_CHARS,
            "stderr_truncated": len(self.stderr) > MAX_COMMAND_OUTPUT_CHARS,
            "duration_ms": self.duration_ms,
        }


@dataclass
class BinaryChoice:
    path: str | None
    source: str | None
    candidates: list[dict[str, Any]]


@dataclass(frozen=True)
class ProbeInjection:
    pane_id: int
    rule_id: str
    injected_at_epoch_ms: int
    injected_at_monotonic: float
    command: CmdResult


@dataclass(frozen=True)
class VerificationCheck:
    name: str
    outcome: str
    reason_code: str
    expected: str
    actual: Any

    def as_dict(self) -> dict[str, Any]:
        return {
            "check_name": self.name,
            "outcome": self.outcome,
            "reason_code": self.reason_code,
            "expected": self.expected,
            "actual": self.actual,
            "passed": self.outcome == "pass",
        }


class SkipScenario(Exception):
    pass


class DuplicateJsonKey(ValueError):
    pass


def strict_json_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise DuplicateJsonKey(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def reject_json_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON number is forbidden: {value}")


def load_json_bounded(path: Path) -> dict[str, Any]:
    with path.open("rb") as report_file:
        raw = report_file.read(MAX_REPORT_BYTES + 1)
    if len(raw) > MAX_REPORT_BYTES:
        raise ValueError(
            f"report exceeds {MAX_REPORT_BYTES} bytes; refusing unbounded verification"
        )
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise ValueError("report is not valid UTF-8") from exc
    value = json.loads(
        text,
        object_pairs_hook=strict_json_object,
        parse_constant=reject_json_constant,
    )
    if not isinstance(value, dict):
        raise ValueError("report root must be a JSON object")
    return value


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def canonical_json_sha256(value: Any) -> str:
    encoded = json.dumps(
        value,
        allow_nan=False,
        ensure_ascii=True,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def is_finite_number(value: Any) -> bool:
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(float(value))
    )


def is_lower_hex(value: Any, length: int) -> bool:
    return (
        isinstance(value, str)
        and len(value) == length
        and all(character in "0123456789abcdef" for character in value)
    )


def parse_binary_version_identity(value: Any) -> tuple[str, bool] | None:
    """Return the embedded exact Git SHA and dirty bit from `ft --version`."""
    if not isinstance(value, str):
        return None
    match = BINARY_VERSION_PATTERN.fullmatch(value.strip())
    if match is None:
        return None
    return match.group("git_sha"), match.group("dirty") is not None


def authoritative_profile_from_resolution_source(value: Any) -> str | None:
    """Infer a build profile only from a recognized Cargo target directory."""
    if not isinstance(value, str):
        return None
    prefix, separator, profile = value.partition(":")
    if (
        separator
        and prefix in AUTHORITATIVE_BINARY_RESOLUTION_PREFIXES
        and profile in AUTHORITATIVE_BUILD_PROFILES
    ):
        return profile
    return None


def binary_path_matches_profile(value: Any, profile: str | None) -> bool:
    if not isinstance(value, str) or profile is None:
        return False
    path = Path(value)
    return path.is_absolute() and path.name == "ft" and path.parent.name == profile


def absolute_path_is_within(value: Any, root: Any) -> bool:
    if not isinstance(value, str) or not isinstance(root, str):
        return False
    path = Path(value)
    root_path = Path(root)
    if (
        not path.is_absolute()
        or not root_path.is_absolute()
        or ".." in path.parts
        or ".." in root_path.parts
        or path == root_path
    ):
        return False
    try:
        path.relative_to(root_path)
    except ValueError:
        return False
    return True


def hardware_fingerprint_payload(hardware: dict[str, Any]) -> dict[str, Any]:
    """Select the normalized hardware fields covered by the retained digest."""
    return {
        "system": hardware.get("system"),
        "machine": hardware.get("machine"),
        "release": hardware.get("release"),
        "cpu_model": hardware.get("cpu_model"),
        "logical_cpu_count": hardware.get("logical_cpu_count"),
        "physical_memory_bytes": hardware.get("physical_memory_bytes"),
        "target_class": hardware.get("target_class"),
    }


def minimum_output_lines_for_pane(pane_index: int, requested_duration: float) -> int:
    high_phase_start = requested_duration * HIGH_OUTPUT_PHASE_FRACTION
    expected_lines = NORMAL_LINES_PER_SECOND_PER_PANE * requested_duration
    if pane_index in HIGH_OUTPUT_PANES:
        expected_lines = (
            NORMAL_LINES_PER_SECOND_PER_PANE * high_phase_start
            + HIGH_LINES_PER_SECOND_PER_PANE
            * (requested_duration - high_phase_start)
        )
    return math.floor(expected_lines * OUTPUT_RATE_MINIMUM_FRACTION)


def is_utc_timestamp(value: Any) -> bool:
    if not isinstance(value, str) or not value.endswith("Z") or len(value) > 64:
        return False
    try:
        parsed = datetime.fromisoformat(value[:-1] + "+00:00")
    except ValueError:
        return False
    return parsed.utcoffset() == timezone.utc.utcoffset(parsed)


def add_verification_check(
    checks: list[VerificationCheck],
    *,
    name: str,
    outcome: str,
    reason_code: str,
    expected: str,
    actual: Any,
) -> None:
    if outcome not in {"pass", "fail", "skipped_not_proven"}:
        raise ValueError(f"invalid verifier outcome: {outcome}")
    checks.append(
        VerificationCheck(
            name=name,
            outcome=outcome,
            reason_code=reason_code,
            expected=expected,
            actual=actual,
        )
    )


def telemetry_sequence_error(
    samples: Any,
    *,
    required_fields: tuple[str, ...],
) -> str | None:
    if not isinstance(samples, list):
        return "samples_not_array"
    if len(samples) > MAX_TELEMETRY_SAMPLES:
        return "sample_limit_exceeded"
    previous_timestamp: int | None = None
    for sample in samples:
        if not isinstance(sample, dict):
            return "sample_not_object"
        if any(field not in sample for field in required_fields):
            return "sample_missing_required_field"
        timestamp = sample.get("timestamp_ms")
        if not isinstance(timestamp, int) or isinstance(timestamp, bool) or timestamp < 0:
            return "invalid_timestamp"
        if previous_timestamp is not None and timestamp <= previous_timestamp:
            return "timestamps_not_strictly_increasing"
        previous_timestamp = timestamp
    return None


def telemetry_elapsed_error(samples: Any, observed_duration: Any) -> str | None:
    if not isinstance(samples, list):
        return "samples_not_array"
    if not is_finite_number(observed_duration) or float(observed_duration) < 0.0:
        return "observed_duration_missing"
    previous_elapsed: float | None = None
    for sample in samples:
        if not isinstance(sample, dict) or not is_finite_number(sample.get("elapsed_secs")):
            return "elapsed_missing_or_invalid"
        elapsed = float(sample["elapsed_secs"])
        if elapsed < 0.0 or elapsed > float(observed_duration):
            return "elapsed_outside_sustained_run"
        if previous_elapsed is not None and elapsed <= previous_elapsed:
            return "elapsed_not_strictly_increasing"
        previous_elapsed = elapsed
    return None


def source_snapshot_timestamp_error(
    samples: Any,
    minimum_source_timestamp_ms: Any,
) -> str | None:
    """Validate producer-owned snapshot identity without inventing freshness."""
    if not isinstance(samples, list):
        return "samples_not_array"
    if (
        not isinstance(minimum_source_timestamp_ms, int)
        or isinstance(minimum_source_timestamp_ms, bool)
        or minimum_source_timestamp_ms < 0
    ):
        return "sustained_start_timestamp_invalid"
    previous_source_timestamp: int | None = None
    for sample in samples:
        if not isinstance(sample, dict):
            return "sample_not_object"
        source_timestamp = sample.get("source_snapshot_timestamp_ms")
        observed_timestamp = sample.get("timestamp_ms")
        if (
            not isinstance(source_timestamp, int)
            or isinstance(source_timestamp, bool)
            or source_timestamp < 0
            or not isinstance(observed_timestamp, int)
            or isinstance(observed_timestamp, bool)
            or source_timestamp < minimum_source_timestamp_ms
            or source_timestamp > observed_timestamp
        ):
            return "source_snapshot_timestamp_invalid"
        if (
            previous_source_timestamp is not None
            and source_timestamp <= previous_source_timestamp
        ):
            return "source_snapshot_timestamp_not_strictly_increasing"
        previous_source_timestamp = source_timestamp
    return None


def verify_stress_report(report: dict[str, Any]) -> dict[str, Any]:
    """Recompute every authority gate from raw evidence; never trust stored pass booleans."""
    checks: list[VerificationCheck] = []
    run_id = report.get("run_id")

    contract_ok = (
        report.get("contract_id") == REPORT_CONTRACT_ID
        and report.get("schema_version") == REPORT_SCHEMA_VERSION
        and report.get("verifier_version") == VERIFIER_VERSION
        and report.get("bead_id") == "ft-d0ez0.5"
        and isinstance(run_id, str)
        and 0 < len(run_id) <= 128
        and is_utc_timestamp(report.get("generated_at_utc"))
    )
    add_verification_check(
        checks,
        name="closed_contract_identity",
        outcome="pass" if contract_ok else "fail",
        reason_code="contract.identity_matches" if contract_ok else "contract.identity_mismatch",
        expected=(
            f"{REPORT_CONTRACT_ID} schema {REPORT_SCHEMA_VERSION}, verifier "
            f"{VERIFIER_VERSION}, bead ft-d0ez0.5, bounded non-empty run_id, and UTC timestamp"
        ),
        actual={
            "contract_id": report.get("contract_id"),
            "schema_version": report.get("schema_version"),
            "verifier_version": report.get("verifier_version"),
            "bead_id": report.get("bead_id"),
            "run_id": run_id,
            "generated_at_utc": report.get("generated_at_utc"),
        },
    )

    thresholds = report.get("thresholds")
    expected_thresholds = {
        "minimum_sustained_duration_secs": FULL_DURATION_SECS,
        "maximum_supported_duration_secs": MAX_SUPPORTED_DURATION_SECS,
        "detection_latency_limit_ms_exclusive": DETECTION_LATENCY_LIMIT_MS,
        "robot_state_latency_limit_ms_exclusive": ROBOT_STATE_LATENCY_LIMIT_MS,
        "robot_search_latency_limit_ms_exclusive": ROBOT_SEARCH_LATENCY_LIMIT_MS,
        "rss_limit_kb_exclusive": RSS_LIMIT_KB,
        "rss_sample_interval_secs": RSS_SAMPLE_INTERVAL_SECS,
        "rss_max_sample_gap_secs": RSS_MAX_SAMPLE_GAP_SECS,
        "rss_final_coverage_slack_secs": RSS_FINAL_COVERAGE_SLACK_SECS,
        "high_output_phase_fraction": HIGH_OUTPUT_PHASE_FRACTION,
        "high_output_phase_tolerance_secs": HIGH_OUTPUT_PHASE_TOLERANCE_SECS,
        "output_rate_minimum_fraction": OUTPUT_RATE_MINIMUM_FRACTION,
        "output_payload_bytes_per_line": OUTPUT_PAYLOAD_BYTES_PER_LINE,
        "fleet_scrollback_per_pane_budget_bytes": FLEET_SCROLLBACK_PER_PANE_BUDGET_BYTES,
        "fleet_scrollback_high_ratio": FLEET_SCROLLBACK_HIGH_RATIO,
        "mux_scrollback_hot_lines": MUX_SCROLLBACK_HOT_LINES,
        "detection_probe_phase_fraction": DETECTION_PROBE_PHASE_FRACTION,
        "detection_probe_phase_tolerance_secs": DETECTION_PROBE_PHASE_TOLERANCE_SECS,
        "robot_benchmark_phase_fraction": ROBOT_BENCHMARK_PHASE_FRACTION,
        "robot_benchmark_phase_tolerance_secs": ROBOT_BENCHMARK_PHASE_TOLERANCE_SECS,
        "required_panes": NUM_PANES,
    }
    thresholds_ok = thresholds == expected_thresholds
    add_verification_check(
        checks,
        name="frozen_thresholds",
        outcome="pass" if thresholds_ok else "fail",
        reason_code="thresholds.frozen" if thresholds_ok else "thresholds.drifted",
        expected=expected_thresholds,
        actual=thresholds,
    )

    provenance = report.get("provenance") if isinstance(report.get("provenance"), dict) else {}
    source = provenance.get("source") if isinstance(provenance.get("source"), dict) else {}
    binary = provenance.get("binary") if isinstance(provenance.get("binary"), dict) else {}
    config = provenance.get("config") if isinstance(provenance.get("config"), dict) else {}
    hardware = provenance.get("hardware") if isinstance(provenance.get("hardware"), dict) else {}
    receipt_workload = (
        report.get("workload") if isinstance(report.get("workload"), dict) else {}
    )
    authority = report.get("authority") if isinstance(report.get("authority"), dict) else {}
    transport = authority.get("transport")
    evidence_state = authority.get("evidence_state")
    authorization_receipt = (
        authority.get("authorization_receipt")
        if isinstance(authority.get("authorization_receipt"), dict)
        else {}
    )
    workspace = report.get("workspace") if isinstance(report.get("workspace"), dict) else {}
    target_workspace = authority.get("target_workspace")
    authorization_receipt_ref = authority.get("authorization_receipt_ref")
    receipt_shape_ok = (
        authorization_receipt.get("schema_version") == AUTHORIZATION_RECEIPT_SCHEMA_VERSION
        and authorization_receipt.get("scope") == AUTHORIZATION_SCOPE
        and authorization_receipt.get("run_id") == run_id
        and is_utc_timestamp(authorization_receipt.get("authorized_at_utc"))
        and isinstance(authorization_receipt.get("authorized_by"), str)
        and 0 < len(authorization_receipt.get("authorized_by")) <= 256
        and isinstance(authorization_receipt.get("target_workspace"), str)
        and 0 < len(authorization_receipt.get("target_workspace")) <= 4096
        and authorization_receipt.get("target_workspace") == target_workspace
        and target_workspace == workspace.get("root")
        and authorization_receipt.get("operator_session_untouched") is True
        and authorization_receipt.get("source_git_sha") == source.get("git_sha")
        and authorization_receipt.get("binary_sha256") == binary.get("sha256")
        and authorization_receipt.get("config_sha256") == config.get("sha256")
        and authorization_receipt.get("hardware_fingerprint_sha256")
        == hardware.get("hardware_fingerprint_sha256")
        and authorization_receipt.get("requested_sustained_duration_secs")
        == receipt_workload.get("requested_sustained_duration_secs")
        and authorization_receipt.get("required_panes") == NUM_PANES
    )
    receipt_digest_ok = (
        receipt_shape_ok
        and is_lower_hex(authority.get("authorization_receipt_sha256"), 64)
        and canonical_json_sha256(authorization_receipt)
        == authority.get("authorization_receipt_sha256")
    )
    if transport == "fixture_wezterm_cli" or evidence_state == "fixture_only":
        authority_outcome = "skipped_not_proven"
        authority_reason = "authority.fixture_transport"
    elif transport != "native_isolated_mux" or evidence_state != "measured":
        authority_outcome = "fail"
        authority_reason = "authority.invalid_or_unknown"
    else:
        authority_ready = (
            authority.get("isolated_workspace") is True
            and authority.get("operator_session_untouched") is True
            and isinstance(authorization_receipt_ref, str)
            and 0 < len(authorization_receipt_ref) <= 4096
            and absolute_path_is_within(authorization_receipt_ref, target_workspace)
            and receipt_digest_ok
        )
        authority_outcome = "pass" if authority_ready else "skipped_not_proven"
        authority_reason = (
            "authority.native_isolated_measured"
            if authority_ready
            else "authority.authorization_receipt_missing"
        )
    add_verification_check(
        checks,
        name="native_isolated_authority",
        outcome=authority_outcome,
        reason_code=authority_reason,
        expected=(
            "measured native_isolated_mux run in an isolated workspace with an explicit "
            "candidate/workload-bound authorization receipt and no operator-session interaction"
        ),
        actual=authority,
    )

    config_content = config.get("content")
    config_content_bytes = (
        config_content.encode("utf-8") if isinstance(config_content, str) else b""
    )
    binary_version_identity = parse_binary_version_identity(binary.get("version"))
    binary_git_sha = (
        binary_version_identity[0] if binary_version_identity is not None else None
    )
    binary_git_dirty = (
        binary_version_identity[1] if binary_version_identity is not None else None
    )
    source_git_sha = source.get("git_sha")
    inferred_build_profile = authoritative_profile_from_resolution_source(
        binary.get("resolution_source")
    )
    hardware_fingerprint = hardware.get("hardware_fingerprint_sha256")
    expected_hardware_fingerprint = canonical_json_sha256(
        hardware_fingerprint_payload(hardware)
    )
    provenance_missing = [
        field
        for field, present in {
            "source.git_sha": is_lower_hex(source.get("git_sha"), 40),
            "source.git_tree_sha": is_lower_hex(source.get("git_tree_sha"), 40),
            "source.tracked_tree_clean": source.get("tracked_tree_clean") is True,
            "binary.path": isinstance(binary.get("path"), str)
            and 0 < len(binary.get("path")) <= 4096,
            "binary.sha256": is_lower_hex(binary.get("sha256"), 64),
            "binary.size_bytes": isinstance(binary.get("size_bytes"), int)
            and not isinstance(binary.get("size_bytes"), bool)
            and binary.get("size_bytes", 0) > 0,
            "binary.version_identity": binary_version_identity is not None,
            "binary.source_git_sha_match": is_lower_hex(source_git_sha, 40)
            and source_git_sha == binary_git_sha,
            "binary.source_git_clean": binary_git_dirty is False,
            "binary.build_profile_resolution": inferred_build_profile is not None
            and binary.get("build_profile") == inferred_build_profile,
            "binary.path_profile_layout": binary_path_matches_profile(
                binary.get("path"), inferred_build_profile
            ),
            "binary.path_isolated_workspace": absolute_path_is_within(
                binary.get("path"), target_workspace
            ),
            "config.path": isinstance(config.get("path"), str)
            and 0 < len(config.get("path")) <= 4096,
            "config.path_isolated_workspace": absolute_path_is_within(
                config.get("path"), target_workspace
            ),
            "config.sha256": is_lower_hex(config.get("sha256"), 64),
            "config.size_bytes": isinstance(config.get("size_bytes"), int)
            and not isinstance(config.get("size_bytes"), bool)
            and config.get("size_bytes", 0) > 0,
            "config.content": config_content == CONFIG_TEMPLATE,
            "config.content_sha256": bool(config_content_bytes)
            and hashlib.sha256(config_content_bytes).hexdigest() == config.get("sha256"),
            "config.content_size": bool(config_content_bytes)
            and len(config_content_bytes) == config.get("size_bytes"),
            "hardware.system": isinstance(hardware.get("system"), str)
            and 0 < len(hardware.get("system")) <= 256,
            "hardware.machine": isinstance(hardware.get("machine"), str)
            and 0 < len(hardware.get("machine")) <= 256,
            "hardware.release": isinstance(hardware.get("release"), str)
            and 0 < len(hardware.get("release")) <= 256,
            "hardware.target_class": isinstance(hardware.get("target_class"), str)
            and 0 < len(hardware.get("target_class")) <= 256,
            "hardware.cpu_model": isinstance(hardware.get("cpu_model"), str)
            and 0 < len(hardware.get("cpu_model")) <= 512,
            "hardware.logical_cpu_count": isinstance(hardware.get("logical_cpu_count"), int)
            and not isinstance(hardware.get("logical_cpu_count"), bool)
            and hardware.get("logical_cpu_count", 0) > 0,
            "hardware.physical_memory_bytes": isinstance(
                hardware.get("physical_memory_bytes"), int
            )
            and not isinstance(hardware.get("physical_memory_bytes"), bool)
            and hardware.get("physical_memory_bytes", 0) > 0,
            "hardware.hardware_fingerprint_sha256": is_lower_hex(
                hardware_fingerprint, 64
            )
            and hardware_fingerprint == expected_hardware_fingerprint,
        }.items()
        if not present
    ]
    provenance_outcome = "pass" if not provenance_missing else "skipped_not_proven"
    add_verification_check(
        checks,
        name="artifact_provenance_complete",
        outcome=provenance_outcome,
        reason_code=(
            "provenance.complete" if not provenance_missing else "provenance.missing_or_dirty"
        ),
        expected=(
            "clean exact source; clean source-matched binary from a recognized "
            "release-interactive/release-perf target layout; frozen embedded config "
            "with matching digest; and digest-bound target hardware identity"
        ),
        actual={"missing_or_invalid": provenance_missing, "provenance": provenance},
    )

    workload = report.get("workload") if isinstance(report.get("workload"), dict) else {}
    requested_duration = workload.get("requested_sustained_duration_secs")
    observed_duration = workload.get("observed_sustained_duration_secs")
    requested_duration_present = (
        is_finite_number(requested_duration) and float(requested_duration) >= 0.0
    )
    duration_present = is_finite_number(observed_duration) and float(observed_duration) >= 0.0
    duration_ok = (
        requested_duration_present
        and duration_present
        and float(requested_duration) >= FULL_DURATION_SECS
        and float(requested_duration) <= MAX_SUPPORTED_DURATION_SECS
        and float(observed_duration) >= float(requested_duration)
    )
    if not requested_duration_present or not duration_present:
        duration_outcome = "skipped_not_proven"
        duration_reason = "duration.missing_or_invalid"
    elif float(requested_duration) > MAX_SUPPORTED_DURATION_SECS:
        duration_outcome = "fail"
        duration_reason = "duration.above_supported_sample_bound"
    elif not duration_ok:
        duration_outcome = "fail"
        duration_reason = "duration.below_minimum"
    else:
        duration_outcome = "pass"
        duration_reason = "duration.full"
    add_verification_check(
        checks,
        name="full_sustained_duration",
        outcome=duration_outcome,
        reason_code=duration_reason,
        expected=(
            f"requested duration >= {FULL_DURATION_SECS:.0f}s and observed duration "
            f">= requested duration, with requested duration <= "
            f"{MAX_SUPPORTED_DURATION_SECS:.0f}s"
        ),
        actual={"requested": requested_duration, "observed": observed_duration},
    )
    pane_counts_present = all(
        isinstance(workload.get(field), int) and not isinstance(workload.get(field), bool)
        for field in ("panes_spawned", "panes_observed")
    )
    panes_ok = pane_counts_present and all(
        workload.get(field) == NUM_PANES for field in ("panes_spawned", "panes_observed")
    )
    if not pane_counts_present:
        pane_outcome = "skipped_not_proven"
        pane_reason = "panes.counts_missing"
    elif not panes_ok:
        pane_outcome = "fail"
        pane_reason = "panes.count_mismatch"
    else:
        pane_outcome = "pass"
        pane_reason = "panes.exact"
    add_verification_check(
        checks,
        name="exact_50_pane_population",
        outcome=pane_outcome,
        reason_code=pane_reason,
        expected={"panes_spawned": NUM_PANES, "panes_observed": NUM_PANES},
        actual={
            "panes_spawned": workload.get("panes_spawned"),
            "panes_observed": workload.get("panes_observed"),
        },
    )
    output_bytes = workload.get("observed_output_bytes")
    output_bytes_at_start = workload.get("output_bytes_at_sustained_start")
    output_bytes_at_end = workload.get("output_bytes_at_sustained_end")
    output_lines = workload.get("observed_output_lines")
    output_lines_at_start = workload.get("output_lines_at_sustained_start")
    output_lines_at_end = workload.get("output_lines_at_sustained_end")
    spawned_pane_ids = workload.get("spawned_pane_ids")
    pane_output_deltas = workload.get("pane_output_deltas")
    normal_rate = workload.get("normal_lines_per_second_per_pane")
    high_rate = workload.get("high_lines_per_second_per_pane")
    configured_phase_fraction = workload.get("configured_high_output_phase_fraction")
    high_phase_elapsed = workload.get("high_output_phase_started_elapsed_secs")
    workload_fields_present = all(
        field in workload
        for field in (
            "high_output_panes",
            "high_output_phase_started",
            "configured_high_output_phase_fraction",
            "high_output_phase_started_elapsed_secs",
            "sustained_started_epoch_ms",
            "observed_output_bytes",
            "output_bytes_at_sustained_start",
            "output_bytes_at_sustained_end",
            "observed_output_lines",
            "output_lines_at_sustained_start",
            "output_lines_at_sustained_end",
            "spawned_pane_ids",
            "pane_output_deltas",
            "normal_lines_per_second_per_pane",
            "high_lines_per_second_per_pane",
        )
    )
    expected_high_phase_elapsed = (
        float(requested_duration) * HIGH_OUTPUT_PHASE_FRACTION
        if requested_duration_present
        else None
    )
    pane_output_error: str | None = None
    pane_output_total = 0
    pane_output_byte_total = 0
    if (
        not isinstance(spawned_pane_ids, list)
        or len(spawned_pane_ids) != NUM_PANES
        or any(
            not isinstance(pane_id, int) or isinstance(pane_id, bool) or pane_id <= 0
            for pane_id in spawned_pane_ids
        )
        or len(set(spawned_pane_ids)) != NUM_PANES
        or not isinstance(pane_output_deltas, list)
        or len(pane_output_deltas) != NUM_PANES
        or not requested_duration_present
    ):
        pane_output_error = "pane_output_population_invalid"
    else:
        seen_indexes: set[int] = set()
        for sample in pane_output_deltas:
            if not isinstance(sample, dict):
                pane_output_error = "pane_output_sample_not_object"
                break
            pane_index = sample.get("pane_index")
            pane_id = sample.get("pane_id")
            lines_at_start = sample.get("lines_at_start")
            lines_at_end = sample.get("lines_at_end")
            observed_lines = sample.get("observed_lines")
            bytes_at_start = sample.get("bytes_at_start")
            bytes_at_end = sample.get("bytes_at_end")
            observed_bytes = sample.get("observed_bytes")
            integer_fields = (
                pane_index,
                pane_id,
                lines_at_start,
                lines_at_end,
                observed_lines,
                bytes_at_start,
                bytes_at_end,
                observed_bytes,
            )
            if any(
                not isinstance(value, int) or isinstance(value, bool)
                for value in integer_fields
            ):
                pane_output_error = "pane_output_integer_field_invalid"
                break
            if (
                pane_index < 1
                or pane_index > NUM_PANES
                or pane_index in seen_indexes
                or pane_id != spawned_pane_ids[pane_index - 1]
                or lines_at_start < 0
                or lines_at_end < lines_at_start
                or observed_lines != lines_at_end - lines_at_start
                or bytes_at_start < 0
                or bytes_at_end < bytes_at_start
                or observed_bytes != bytes_at_end - bytes_at_start
                or observed_lines
                < minimum_output_lines_for_pane(
                    pane_index,
                    float(requested_duration),
                )
                or observed_bytes
                < observed_lines * OUTPUT_PAYLOAD_BYTES_PER_LINE
            ):
                pane_output_error = "pane_output_rate_or_identity_breached"
                break
            seen_indexes.add(pane_index)
            pane_output_total += observed_lines
            pane_output_byte_total += observed_bytes
        if pane_output_error is None and seen_indexes != set(range(1, NUM_PANES + 1)):
            pane_output_error = "pane_output_index_set_incomplete"
    workload_shape_ok = (
        isinstance(workload.get("high_output_panes"), list)
        and workload.get("high_output_panes") == HIGH_OUTPUT_PANES
        and workload.get("high_output_phase_started") is True
        and isinstance(workload.get("sustained_started_epoch_ms"), int)
        and not isinstance(workload.get("sustained_started_epoch_ms"), bool)
        and workload.get("sustained_started_epoch_ms", -1) >= 0
        and isinstance(output_bytes, int)
        and not isinstance(output_bytes, bool)
        and output_bytes > 0
        and isinstance(output_bytes_at_start, int)
        and not isinstance(output_bytes_at_start, bool)
        and output_bytes_at_start >= 0
        and isinstance(output_bytes_at_end, int)
        and not isinstance(output_bytes_at_end, bool)
        and output_bytes_at_end > output_bytes_at_start
        and output_bytes == output_bytes_at_end - output_bytes_at_start
        and output_bytes == pane_output_byte_total
        and isinstance(output_lines, int)
        and not isinstance(output_lines, bool)
        and output_lines > 0
        and isinstance(output_lines_at_start, int)
        and not isinstance(output_lines_at_start, bool)
        and output_lines_at_start >= 0
        and isinstance(output_lines_at_end, int)
        and not isinstance(output_lines_at_end, bool)
        and output_lines_at_end > output_lines_at_start
        and output_lines == output_lines_at_end - output_lines_at_start
        and output_lines == pane_output_total
        and pane_output_error is None
        and normal_rate == NORMAL_LINES_PER_SECOND_PER_PANE
        and high_rate == HIGH_LINES_PER_SECOND_PER_PANE
        and configured_phase_fraction == HIGH_OUTPUT_PHASE_FRACTION
        and is_finite_number(high_phase_elapsed)
        and expected_high_phase_elapsed is not None
        and abs(float(high_phase_elapsed) - expected_high_phase_elapsed)
        <= HIGH_OUTPUT_PHASE_TOLERANCE_SECS
    )
    if not workload_fields_present:
        workload_outcome = "skipped_not_proven"
        workload_reason = "workload.rate_evidence_missing"
    elif workload_shape_ok:
        workload_outcome = "pass"
        workload_reason = "workload.rates_bound"
    else:
        workload_outcome = "fail"
        workload_reason = "workload.rate_or_phase_contract_breached"
    add_verification_check(
        checks,
        name="workload_rates_bound",
        outcome=workload_outcome,
        reason_code=workload_reason,
        expected=(
            "exact pane identities, per-pane line deltas at >= "
            f"{OUTPUT_RATE_MINIMUM_FRACTION:.0%} of the configured rates, phase "
            f"transition, >= {OUTPUT_PAYLOAD_BYTES_PER_LINE} payload bytes per line, "
            "and exact positive aggregate line/byte deltas"
        ),
        actual={"pane_output_error": pane_output_error, "workload": workload},
    )

    evidence = report.get("evidence") if isinstance(report.get("evidence"), dict) else {}
    detection = (
        evidence.get("detection_probe")
        if isinstance(evidence.get("detection_probe"), dict)
        else {}
    )
    matching_events = detection.get("matching_events")
    detection_observed_after_ms = detection.get("observed_after_ms")
    injected_at_elapsed_secs = detection.get("injected_at_elapsed_secs")
    expected_detection_phase_elapsed = (
        float(requested_duration) * DETECTION_PROBE_PHASE_FRACTION
        if requested_duration_present
        else None
    )
    detection_phase_present = (
        is_finite_number(injected_at_elapsed_secs)
        and expected_detection_phase_elapsed is not None
    )
    detection_phase_ok = (
        detection_phase_present
        and abs(float(injected_at_elapsed_secs) - expected_detection_phase_elapsed)
        <= DETECTION_PROBE_PHASE_TOLERANCE_SECS
    )
    add_verification_check(
        checks,
        name="detection_probe_phase",
        outcome=(
            "pass"
            if detection_phase_ok
            else "fail"
            if detection_phase_present
            else "skipped_not_proven"
        ),
        reason_code=(
            "detection.phase_bound"
            if detection_phase_ok
            else "detection.phase_outside_window"
            if detection_phase_present
            else "detection.phase_missing"
        ),
        expected=(
            f"probe injection at {DETECTION_PROBE_PHASE_FRACTION:.0%} of requested duration "
            f"within {DETECTION_PROBE_PHASE_TOLERANCE_SECS:.0f}s"
        ),
        actual=injected_at_elapsed_secs,
    )
    detection_shape_ok = (
        detection.get("rule_id") == DETECTION_RULE_ID
        and isinstance(detection.get("pane_id"), int)
        and not isinstance(detection.get("pane_id"), bool)
        and isinstance(detection.get("injected_at_epoch_ms"), int)
        and not isinstance(detection.get("injected_at_epoch_ms"), bool)
        and isinstance(matching_events, list)
        and len(matching_events) == 1
        and isinstance(matching_events[0], dict)
        and is_finite_number(detection_observed_after_ms)
        and float(detection_observed_after_ms) >= 0.0
    )
    detection_latency: float | None = None
    if detection_shape_ok:
        event = matching_events[0]
        captured_at = event.get("captured_at")
        if (
            event.get("rule_id") == DETECTION_RULE_ID
            and event.get("pane_id") == detection.get("pane_id")
            and isinstance(captured_at, int)
            and not isinstance(captured_at, bool)
        ):
            detection_latency = float(captured_at - detection["injected_at_epoch_ms"])
        else:
            detection_shape_ok = False
    if not detection_shape_ok or detection_latency is None:
        detection_outcome = "skipped_not_proven"
        detection_reason = "detection.exact_event_missing"
    elif (
        detection_latency < 0.0
        or detection_latency >= DETECTION_LATENCY_LIMIT_MS
        or float(detection_observed_after_ms) >= DETECTION_LATENCY_LIMIT_MS
    ):
        detection_outcome = "fail"
        detection_reason = "detection.latency_threshold_breached"
    else:
        detection_outcome = "pass"
        detection_reason = "detection.latency_within_threshold"
    add_verification_check(
        checks,
        name="detection_latency_below_5s",
        outcome=detection_outcome,
        reason_code=detection_reason,
        expected=(
            f"exact pane/rule event timestamp and monotonic observation both yield "
            f"0 <= latency < {DETECTION_LATENCY_LIMIT_MS:.0f}ms"
        ),
        actual={
            "event_latency_ms": detection_latency,
            "observed_after_ms": detection_observed_after_ms,
            "probe": detection,
        },
    )

    expected_pane_ids = (
        sorted(spawned_pane_ids)
        if isinstance(spawned_pane_ids, list)
        and len(spawned_pane_ids) == NUM_PANES
        and all(
            isinstance(pane_id, int)
            and not isinstance(pane_id, bool)
            and pane_id > 0
            for pane_id in spawned_pane_ids
        )
        and len(set(spawned_pane_ids)) == NUM_PANES
        else None
    )
    scrollback_samples = evidence.get("scrollback_samples")
    scrollback_source_timestamps = (
        {
            sample.get("source_snapshot_timestamp_ms")
            for sample in scrollback_samples
            if isinstance(sample, dict)
            and isinstance(sample.get("source_snapshot_timestamp_ms"), int)
            and not isinstance(sample.get("source_snapshot_timestamp_ms"), bool)
        }
        if isinstance(scrollback_samples, list)
        else set()
    )
    pressure_samples = evidence.get("fleet_pressure_samples")
    pressure_error = telemetry_sequence_error(
        pressure_samples,
        required_fields=(
            "timestamp_ms",
            "source_snapshot_timestamp_ms",
            "elapsed_secs",
            "tier",
            "observed_panes",
            "observed_pane_ids",
            "source",
            "evidence_state",
        ),
    )
    if pressure_error is None:
        pressure_error = telemetry_elapsed_error(pressure_samples, observed_duration)
    if pressure_error is None:
        pressure_error = source_snapshot_timestamp_error(
            pressure_samples,
            workload.get("sustained_started_epoch_ms"),
        )
    pressure_transition = False
    if pressure_error is None and pressure_samples:
        rank = {"normal": 0, "elevated": 1, "critical": 2, "emergency": 3}
        normalized: list[tuple[int, int]] = []
        for sample in pressure_samples:
            tier = str(sample["tier"]).lower()
            if (
                sample["source"] != "runtime_health_snapshot"
                or sample["evidence_state"] != "measured"
                or not isinstance(sample["observed_panes"], int)
                or isinstance(sample["observed_panes"], bool)
                or sample["observed_panes"] != NUM_PANES
                or expected_pane_ids is None
                or sample["observed_pane_ids"] != expected_pane_ids
                or sample["source_snapshot_timestamp_ms"]
                not in scrollback_source_timestamps
                or tier not in rank
            ):
                pressure_error = "pressure_sample_not_authoritative"
                break
            normalized.append((rank[tier], sample["source_snapshot_timestamp_ms"]))
        if pressure_error is None:
            pressure_transition = any(
                normalized[earlier][0] == 0
                and normalized[later][0] > 0
                and normalized[later][1] > normalized[earlier][1]
                for earlier in range(len(normalized))
                for later in range(earlier + 1, len(normalized))
            )
    if pressure_error is not None or not pressure_samples:
        pressure_outcome = "skipped_not_proven"
        pressure_reason = f"pressure.{pressure_error or 'samples_missing'}"
    elif not pressure_transition:
        pressure_outcome = "fail"
        pressure_reason = "pressure.transition_not_observed"
    else:
        pressure_outcome = "pass"
        pressure_reason = "pressure.normal_to_above_normal_observed"
    add_verification_check(
        checks,
        name="fleet_pressure_transition",
        outcome=pressure_outcome,
        reason_code=pressure_reason,
        expected=(
            "strictly ordered measured Normal snapshot followed by a distinct newer "
            "Elevated, Critical, or Emergency snapshot for the exact spawned pane "
            "identities and paired mux telemetry source timestamps"
        ),
        actual=pressure_samples,
    )

    scrollback_error = telemetry_sequence_error(
        scrollback_samples,
        required_fields=(
            "timestamp_ms",
            "source_snapshot_timestamp_ms",
            "elapsed_secs",
            "source",
            "evidence_state",
            "observed_panes",
            "sampled_panes",
            "observed_pane_ids",
            "sampled_pane_ids",
            "telemetry_blind",
            "telemetry_partial",
            "tiering_enabled_panes",
            "configured_hot_lines_min",
            "configured_hot_lines_max",
            "warm_spill_lines_total",
            "warm_spill_bytes_total",
        ),
    )
    if scrollback_error is None:
        scrollback_error = telemetry_elapsed_error(scrollback_samples, observed_duration)
    if scrollback_error is None:
        scrollback_error = source_snapshot_timestamp_error(
            scrollback_samples,
            workload.get("sustained_started_epoch_ms"),
        )
    scrollback_transition = False
    if scrollback_error is None and scrollback_samples:
        previous_lines: int | None = None
        previous_bytes: int | None = None
        previous_source_timestamp: int | None = None
        for sample in scrollback_samples:
            lines = sample["warm_spill_lines_total"]
            byte_count = sample["warm_spill_bytes_total"]
            source_timestamp = sample["source_snapshot_timestamp_ms"]
            observed_ids = sample["observed_pane_ids"]
            sampled_ids = sample["sampled_pane_ids"]
            tiering_enabled_panes = sample["tiering_enabled_panes"]
            configured_hot_lines_min = sample["configured_hot_lines_min"]
            configured_hot_lines_max = sample["configured_hot_lines_max"]
            if (
                sample["source"]
                != "runtime_health_snapshot.fleet_scrollback_telemetry"
                or sample["evidence_state"] != "measured"
                or not isinstance(sample["observed_panes"], int)
                or isinstance(sample["observed_panes"], bool)
                or sample["observed_panes"] != NUM_PANES
                or not isinstance(sample["sampled_panes"], int)
                or isinstance(sample["sampled_panes"], bool)
                or sample["sampled_panes"] != NUM_PANES
                or expected_pane_ids is None
                or observed_ids != expected_pane_ids
                or sampled_ids != expected_pane_ids
                or sample["telemetry_blind"] is not False
                or sample["telemetry_partial"] is not False
                or not isinstance(tiering_enabled_panes, int)
                or isinstance(tiering_enabled_panes, bool)
                or tiering_enabled_panes != NUM_PANES
                or configured_hot_lines_min != MUX_SCROLLBACK_HOT_LINES
                or configured_hot_lines_max != MUX_SCROLLBACK_HOT_LINES
                or not isinstance(lines, int)
                or isinstance(lines, bool)
                or lines < 0
                or not isinstance(byte_count, int)
                or isinstance(byte_count, bool)
                or byte_count < 0
                or (previous_lines is not None and lines < previous_lines)
                or (previous_bytes is not None and byte_count < previous_bytes)
            ):
                scrollback_error = "scrollback_sample_not_authoritative"
                break
            if (
                previous_lines is not None
                and previous_source_timestamp is not None
                and source_timestamp > previous_source_timestamp
            ):
                scrollback_transition |= (
                    lines > previous_lines
                )
            previous_lines = lines
            previous_bytes = byte_count
            previous_source_timestamp = source_timestamp
    if scrollback_error is not None or not scrollback_samples:
        scrollback_outcome = "skipped_not_proven"
        scrollback_reason = f"scrollback.{scrollback_error or 'samples_missing'}"
    elif not scrollback_transition:
        scrollback_outcome = "fail"
        scrollback_reason = "scrollback.hot_to_warm_not_observed"
    else:
        scrollback_outcome = "pass"
        scrollback_reason = "scrollback.hot_to_warm_observed"
    add_verification_check(
        checks,
        name="hot_to_warm_scrollback_transition",
        outcome=scrollback_outcome,
        reason_code=scrollback_reason,
        expected=(
            "complete measured mux coverage for the exact spawned pane identities, "
            f"all tiering enabled at exactly {MUX_SCROLLBACK_HOT_LINES} hot lines, "
            "nondecreasing warm-spill line/byte totals, and a line-total increase "
            "across distinct runtime snapshots"
        ),
        actual=scrollback_samples,
    )

    rss_samples = evidence.get("rss_samples")
    rss_error = telemetry_sequence_error(
        rss_samples,
        required_fields=("timestamp_ms", "elapsed_secs", "rss_kb"),
    )
    peak_rss_kb: int | None = None
    if rss_error is None and rss_samples:
        rss_values: list[int] = []
        rss_elapsed_values: list[float] = []
        for sample in rss_samples:
            value = sample["rss_kb"]
            elapsed = sample["elapsed_secs"]
            if (
                not isinstance(value, int)
                or isinstance(value, bool)
                or value <= 0
                or not is_finite_number(elapsed)
                or float(elapsed) < 0.0
            ):
                rss_error = "rss_sample_invalid"
                break
            rss_values.append(value)
            rss_elapsed_values.append(float(elapsed))
        if rss_error is None:
            peak_rss_kb = max(rss_values)
            if len(rss_values) < 2:
                rss_error = "rss_sample_coverage_insufficient"
            elif any(
                later <= earlier
                for earlier, later in zip(rss_elapsed_values, rss_elapsed_values[1:])
            ):
                rss_error = "rss_elapsed_not_strictly_increasing"
            elif rss_elapsed_values[0] > RSS_FINAL_COVERAGE_SLACK_SECS:
                rss_error = "rss_initial_coverage_missing"
            elif any(
                later - earlier > RSS_MAX_SAMPLE_GAP_SECS
                for earlier, later in zip(rss_elapsed_values, rss_elapsed_values[1:])
            ):
                rss_error = "rss_sample_gap_exceeded"
            elif not duration_present:
                rss_error = "rss_duration_missing"
            elif (
                rss_elapsed_values[-1]
                < float(observed_duration) - RSS_FINAL_COVERAGE_SLACK_SECS
            ):
                rss_error = "rss_final_coverage_missing"
    if rss_error is not None or not rss_samples:
        rss_outcome = "skipped_not_proven"
        rss_reason = f"rss.{rss_error or 'samples_missing'}"
    elif peak_rss_kb is None or peak_rss_kb >= RSS_LIMIT_KB:
        rss_outcome = "fail"
        rss_reason = "rss.threshold_breached"
    else:
        rss_outcome = "pass"
        rss_reason = "rss.within_threshold"
    add_verification_check(
        checks,
        name="rss_below_1gib",
        outcome=rss_outcome,
        reason_code=rss_reason,
        expected=(
            f"positive RSS samples cover the full run with gaps <= "
            f"{RSS_MAX_SAMPLE_GAP_SECS:.0f}s and peak < {RSS_LIMIT_KB} KiB"
        ),
        actual={"peak_rss_kb": peak_rss_kb, "samples": rss_samples},
    )

    robot_samples = (
        evidence.get("robot_samples") if isinstance(evidence.get("robot_samples"), dict) else {}
    )
    robot_specs = (
        ("state", ROBOT_STATE_LATENCY_LIMIT_MS, True),
        ("search", ROBOT_SEARCH_LATENCY_LIMIT_MS, False),
    )
    expected_robot_phase_elapsed = (
        float(requested_duration) * ROBOT_BENCHMARK_PHASE_FRACTION
        if requested_duration_present
        else None
    )
    for name, limit_ms, require_panes in robot_specs:
        sample = robot_samples.get(name) if isinstance(robot_samples.get(name), dict) else {}
        duration_ms = sample.get("duration_ms")
        measured_at_elapsed = sample.get("measured_at_elapsed_secs")
        if not sample:
            outcome = "skipped_not_proven"
            reason = f"robot.{name}.sample_missing"
        elif sample.get("returncode") != 0:
            outcome = "fail"
            reason = f"robot.{name}.command_failed"
        elif not is_finite_number(duration_ms) or float(duration_ms) < 0.0:
            outcome = "skipped_not_proven"
            reason = f"robot.{name}.latency_missing"
        elif not is_finite_number(measured_at_elapsed) or expected_robot_phase_elapsed is None:
            outcome = "skipped_not_proven"
            reason = f"robot.{name}.measurement_time_missing"
        elif (
            abs(float(measured_at_elapsed) - expected_robot_phase_elapsed)
            > ROBOT_BENCHMARK_PHASE_TOLERANCE_SECS
            or not duration_present
            or float(measured_at_elapsed) > float(observed_duration)
        ):
            outcome = "fail"
            reason = f"robot.{name}.measurement_outside_load_phase_window"
        elif require_panes and sample.get("pane_count") != NUM_PANES:
            outcome = "fail"
            reason = f"robot.{name}.pane_count_mismatch"
        elif name == "search" and (
            not isinstance(sample.get("hit_count"), int)
            or isinstance(sample.get("hit_count"), bool)
            or sample.get("hit_count", 0) <= 0
        ):
            outcome = "fail"
            reason = "robot.search.expected_results_missing"
        elif float(duration_ms) >= limit_ms:
            outcome = "fail"
            reason = f"robot.{name}.latency_threshold_breached"
        else:
            outcome = "pass"
            reason = f"robot.{name}.latency_within_threshold"
        add_verification_check(
            checks,
            name=f"robot_{name}_latency",
            outcome=outcome,
            reason_code=reason,
            expected=f"successful robot {name} response in < {limit_ms:.0f}ms",
            actual=sample,
        )
    get_text_sample = (
        robot_samples.get("get_text")
        if isinstance(robot_samples.get("get_text"), dict)
        else {}
    )
    if not get_text_sample:
        get_text_outcome = "skipped_not_proven"
        get_text_reason = "robot.get_text.sample_missing"
    elif get_text_sample.get("returncode") != 0:
        get_text_outcome = "fail"
        get_text_reason = "robot.get_text.command_failed"
    elif (
        not is_finite_number(get_text_sample.get("duration_ms"))
        or float(get_text_sample["duration_ms"]) < 0.0
        or not isinstance(get_text_sample.get("response_bytes"), int)
        or isinstance(get_text_sample.get("response_bytes"), bool)
        or get_text_sample.get("response_bytes", 0) <= 0
    ):
        get_text_outcome = "skipped_not_proven"
        get_text_reason = "robot.get_text.measurement_missing"
    elif (
        not is_finite_number(get_text_sample.get("measured_at_elapsed_secs"))
        or expected_robot_phase_elapsed is None
    ):
        get_text_outcome = "skipped_not_proven"
        get_text_reason = "robot.get_text.measurement_time_missing"
    elif (
        abs(
            float(get_text_sample["measured_at_elapsed_secs"])
            - expected_robot_phase_elapsed
        )
        > ROBOT_BENCHMARK_PHASE_TOLERANCE_SECS
        or not duration_present
        or float(get_text_sample["measured_at_elapsed_secs"]) > float(observed_duration)
    ):
        get_text_outcome = "fail"
        get_text_reason = "robot.get_text.measurement_outside_load_phase_window"
    else:
        get_text_outcome = "pass"
        get_text_reason = "robot.get_text.completed"
    add_verification_check(
        checks,
        name="robot_get_text_completed",
        outcome=get_text_outcome,
        reason_code=get_text_reason,
        expected=(
            "successful measured get-text --all completion; latency is recorded "
            "but this contract has no frozen get-text latency threshold"
        ),
        actual=get_text_sample,
    )

    runtime = evidence.get("runtime") if isinstance(evidence.get("runtime"), dict) else {}
    crash_signals = runtime.get("crash_signals")
    corruption_signals = runtime.get("data_corruption_signals")
    runtime_shape_ok = (
        isinstance(crash_signals, list)
        and isinstance(corruption_signals, list)
        and isinstance(runtime.get("watch_log_size_bytes"), int)
        and not isinstance(runtime.get("watch_log_size_bytes"), bool)
        and runtime.get("watch_log_size_bytes", -1) >= 0
        and runtime.get("watch_log_scan_complete") is True
        and isinstance(runtime.get("sqlite_quick_check"), str)
    )
    if not runtime_shape_ok:
        runtime_outcome = "skipped_not_proven"
        runtime_reason = "runtime.signal_telemetry_missing"
    elif runtime.get("sqlite_quick_check") == "database_missing" or str(
        runtime.get("sqlite_quick_check")
    ).startswith("sqlite_error:"):
        runtime_outcome = "skipped_not_proven"
        runtime_reason = "runtime.sqlite_integrity_telemetry_unavailable"
    elif (
        runtime.get("process_survived") is not True
        or runtime.get("watch_exit_code") is not None
        or runtime.get("sqlite_quick_check") != "ok"
        or crash_signals
        or corruption_signals
    ):
        runtime_outcome = "fail"
        runtime_reason = "runtime.crash_or_corruption_observed"
    else:
        runtime_outcome = "pass"
        runtime_reason = "runtime.no_crash_or_corruption_signal"
    add_verification_check(
        checks,
        name="no_crash_panic_or_corruption",
        outcome=runtime_outcome,
        reason_code=runtime_reason,
        expected="watch survives and bounded crash/panic/data-corruption signal lists remain empty",
        actual=runtime,
    )

    cleanup = report.get("cleanup") if isinstance(report.get("cleanup"), dict) else {}
    cleanup_errors = cleanup.get("errors")
    cleanup_shape_ok = (
        isinstance(cleanup_errors, list)
        and cleanup.get("owned_panes_closed") is True
        and cleanup.get("owned_watch_stopped") is True
        and cleanup.get("workspace_disposition_complete") is True
    )
    if not cleanup:
        cleanup_outcome = "skipped_not_proven"
        cleanup_reason = "cleanup.evidence_missing"
    elif not cleanup_shape_ok or cleanup_errors:
        cleanup_outcome = "fail"
        cleanup_reason = "cleanup.owned_resource_cleanup_failed"
    else:
        cleanup_outcome = "pass"
        cleanup_reason = "cleanup.owned_resources_released"
    add_verification_check(
        checks,
        name="isolated_resource_cleanup",
        outcome=cleanup_outcome,
        reason_code=cleanup_reason,
        expected="all harness-owned panes and watch process stopped; workspace retained or removed as requested",
        actual=cleanup,
    )

    failed = sum(check.outcome == "fail" for check in checks)
    skipped = sum(check.outcome == "skipped_not_proven" for check in checks)
    passed = sum(check.outcome == "pass" for check in checks)
    if failed:
        status = "failed"
    elif skipped:
        status = "skipped_not_proven"
    else:
        status = "passed"
    return {
        "verifier_version": VERIFIER_VERSION,
        "status": status,
        "authoritative": status == "passed",
        "summary": {
            "total_checks": len(checks),
            "passed": passed,
            "failed": failed,
            "skipped_not_proven": skipped,
        },
        "checks": [check.as_dict() for check in checks],
        "ignored_precomputed_check_count": len(report.get("checks", []))
        if isinstance(report.get("checks"), list)
        else 0,
    }


def utc_now() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def now_ms() -> int:
    return int(time.time() * 1000)


def ensure_output_dir() -> Path:
    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    return OUTPUT_DIR


def repo_relative(path: Path | None) -> str:
    if path is None:
        return ""
    try:
        return str(path.resolve().relative_to(REPO_ROOT))
    except ValueError:
        return str(path)


def run(
    argv: list[str],
    *,
    cwd: Path | str | None = None,
    env: dict[str, str] | None = None,
    timeout: float = 30.0,
    check: bool = False,
) -> CmdResult:
    t0 = time.monotonic()
    try:
        proc = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            cwd=str(cwd) if cwd else None,
            env=env,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return CmdResult(
            argv=argv,
            returncode=-1,
            stdout="",
            stderr=f"command timed out after {timeout}s",
            duration_ms=int((time.monotonic() - t0) * 1000),
        )
    result = CmdResult(
        argv=argv,
        returncode=proc.returncode,
        stdout=proc.stdout or "",
        stderr=proc.stderr or "",
        duration_ms=int((time.monotonic() - t0) * 1000),
    )
    if check and proc.returncode != 0:
        raise RuntimeError(
            f"command failed (exit {proc.returncode}): {' '.join(shlex.quote(a) for a in argv)}\n"
            f"stderr: {proc.stderr[:500]}"
        )
    return result


def parse_json_output(result: CmdResult) -> Any:
    text = result.stdout.strip()
    if not text:
        raise RuntimeError(f"empty stdout from: {result.argv}")
    return json.loads(
        text,
        object_pairs_hook=strict_json_object,
        parse_constant=reject_json_constant,
    )


def resolve_ft_binary() -> BinaryChoice:
    candidates: list[dict[str, Any]] = []

    def record(path: str | None, source: str, usable: bool, note: str | None = None) -> None:
        entry: dict[str, Any] = {"source": source, "path": path, "usable": usable}
        if note:
            entry["note"] = note
        candidates.append(entry)

    env_binary = os.environ.get("FT_BINARY")
    if env_binary:
        env_path = Path(env_binary).expanduser()
        if env_path.is_file() and os.access(env_path, os.X_OK):
            resolved_env_path = env_path.resolve()
            env_profile = resolved_env_path.parent.name
            resolution_source = f"env_ft_binary:{env_profile}"
            record(str(resolved_env_path), resolution_source, True)
            return BinaryChoice(str(resolved_env_path), resolution_source, candidates)
        record(str(env_path), "env:FT_BINARY", False, "not executable")

    cargo_roots: list[tuple[Path, str]] = []
    cargo_target_dir = os.environ.get("CARGO_TARGET_DIR")
    if cargo_target_dir:
        cargo_roots.append((Path(cargo_target_dir).expanduser(), "cargo_target_dir"))
    cargo_roots.append((REPO_ROOT / "target", "repo_target"))

    seen: set[Path] = set()
    for root, source in cargo_roots:
        for profile in ("release-interactive", "release-perf", "release", "debug"):
            candidate = (root / profile / "ft").resolve()
            if candidate in seen:
                continue
            seen.add(candidate)
            if candidate.is_file() and os.access(candidate, os.X_OK):
                record(str(candidate), f"{source}:{profile}", True)
                return BinaryChoice(str(candidate), f"{source}:{profile}", candidates)
            record(str(candidate), f"{source}:{profile}", False, "not executable")

    path_binary = shutil.which("ft")
    if path_binary:
        record(path_binary, "path:which_ft", True)
        return BinaryChoice(path_binary, "path:which_ft", candidates)
    record(None, "path:which_ft", False, "not found in PATH")
    return BinaryChoice(None, None, candidates)


def make_workspace() -> tuple[Path, Path]:
    workspace = Path(tempfile.mkdtemp(prefix="ft-e2e-50pane-"))
    data_dir = workspace / ".ft"
    data_dir.mkdir(parents=True, exist_ok=True)
    config_path = workspace / "ft.toml"
    config_path.write_text(CONFIG_TEMPLATE, encoding="utf-8")
    return workspace, config_path


def write_fake_wezterm_cli(workspace: Path) -> tuple[Path, Path]:
    state_dir = workspace / ".fake-wezterm"
    state_dir.mkdir(parents=True, exist_ok=True)
    script_path = workspace / "fake_wezterm.py"
    script_path.write_text(FAKE_WEZTERM_SCRIPT, encoding="utf-8")
    script_path.chmod(0o755)
    return script_path, state_dir


def pane_script_text(
    index: int,
    high_output: bool,
    high_output_phase_marker: Path,
) -> str:
    """Render a pane script with a real normal-to-high output phase boundary."""
    body = [
        "#!/bin/bash",
        "set -euo pipefail",
        f"PANE_INDEX={index}",
        f"PAYLOAD={'x' * OUTPUT_PAYLOAD_BYTES_PER_LINE}",
        "COUNTER=0",
        "while true; do",
        "  LINES_PER_CYCLE=1",
    ]
    if high_output:
        body.extend(
            [
                f"  if [[ -f {shlex.quote(str(high_output_phase_marker))} ]]; then",
                "    LINES_PER_CYCLE=10",
                "  fi",
            ]
        )
    body.extend(
        [
            "  i=1",
            "  while [[ $i -le $LINES_PER_CYCLE ]]; do",
            (
                '    printf "[pane-%02d] heartbeat counter=%d line=%d '
                'elapsed_s=%d payload=%s\\n" "$PANE_INDEX" "$COUNTER" "$i" '
                '"$SECONDS" "$PAYLOAD"'
            ),
            "    i=$((i + 1))",
            "  done",
            "  COUNTER=$((COUNTER + 1))",
            "  sleep 0.5",
            "done",
        ]
    )
    return "\n".join(body) + "\n"


def write_pane_script(
    workspace: Path,
    index: int,
    high_output: bool,
    high_output_phase_marker: Path,
) -> Path:
    name = f"stress_pane_{index:02d}"
    script_path = workspace / f"{name}.sh"
    script_path.write_text(
        pane_script_text(index, high_output, high_output_phase_marker),
        encoding="utf-8",
    )
    script_path.chmod(0o755)
    return script_path


def ft_env(workspace: Path, fake_wezterm: Path, fake_state_dir: Path) -> dict[str, str]:
    env = os.environ.copy()
    env["FT_WORKSPACE"] = str(workspace)
    env["FT_DATA_DIR"] = str(workspace / ".ft")
    env["FT_OUTPUT_FORMAT"] = "json"
    env["FT_WEZTERM_CLI"] = str(fake_wezterm)
    env["FT_FAKE_WEZTERM_STATE"] = str(fake_state_dir)
    return env


def ft_cmd(ft_binary: str, workspace: Path, config_path: Path, *args: str) -> list[str]:
    return [ft_binary, "--workspace", str(workspace), "--config", str(config_path), *args]


def spawn_pane(fake_wezterm: Path, workspace: Path, script_path: Path, env: dict[str, str]) -> int:
    result = run(
        [str(fake_wezterm), "cli", "spawn", "--cwd", str(workspace), "--", "bash", str(script_path)],
        cwd=REPO_ROOT,
        env=env,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(f"failed to spawn pane {script_path.name}: {result.stderr or result.stdout}")
    for token in result.stdout.split():
        if token.isdigit():
            return int(token)
    raise RuntimeError(f"fake wezterm spawn did not return pane id for {script_path.name}: {result.stdout}")


def close_pane(fake_wezterm: Path, pane_id: int, env: dict[str, str]) -> CmdResult:
    return run(
        [str(fake_wezterm), "cli", "kill-pane", "--pane-id", str(pane_id)],
        cwd=REPO_ROOT,
        env=env,
    )


def stop_process(proc: subprocess.Popen[str] | None) -> None:
    if proc is None or proc.poll() is not None:
        return
    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def wait_for(description: str, timeout_secs: float, condition: Any) -> Any:
    deadline = time.monotonic() + timeout_secs
    last_value: Any = None
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            last_value = condition()
            last_error = None
        except Exception as exc:  # noqa: BLE001
            last_error = exc
            last_value = None
        if last_value:
            return last_value
        time.sleep(0.5)
    if last_error is not None:
        raise RuntimeError(f"timed out waiting for {description}; last error: {last_error}") from last_error
    raise RuntimeError(f"timed out waiting for {description}")


def extract_panes(payload: Any) -> list[dict[str, Any]]:
    data = payload.get("data") if isinstance(payload, dict) else None
    if isinstance(data, list):
        return data
    if isinstance(data, dict) and isinstance(data.get("panes"), list):
        return data["panes"]
    return []


def extract_events(payload: Any) -> list[dict[str, Any]]:
    data = payload.get("data") if isinstance(payload, dict) else None
    if isinstance(data, dict) and isinstance(data.get("events"), list):
        return data["events"]
    if isinstance(payload, list):
        return payload
    return []


def extract_search_results(payload: Any) -> list[dict[str, Any]]:
    data = payload.get("data") if isinstance(payload, dict) else None
    if isinstance(data, dict) and isinstance(data.get("results"), list):
        return data["results"]
    return []


def get_process_rss_kb(pid: int) -> int | None:
    """Get RSS of a process in kilobytes (macOS/Linux)."""
    try:
        result = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(pid)],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if result.returncode == 0 and result.stdout.strip():
            return int(result.stdout.strip())
    except (subprocess.TimeoutExpired, ValueError, OSError):
        pass
    return None


def inject_pattern(
    fake_wezterm: Path,
    pane_id: int,
    env: dict[str, str],
) -> ProbeInjection:
    """Inject the exact probe and retain wall-clock plus monotonic boundaries."""
    injected_at_epoch_ms = now_ms()
    injected_at_monotonic = time.monotonic()
    command = run(
        [
            str(fake_wezterm),
            "cli",
            "send-text",
            "--pane-id",
            str(pane_id),
            "--text",
            DETECTION_PATTERN_TEXT,
        ],
        cwd=REPO_ROOT,
        env=env,
        check=False,
    )
    if command.returncode != 0:
        raise RuntimeError(
            f"failed to inject detection probe into pane {pane_id}: "
            f"{command.stderr or command.stdout}"
        )
    return ProbeInjection(
        pane_id=pane_id,
        rule_id=DETECTION_RULE_ID,
        injected_at_epoch_ms=injected_at_epoch_ms,
        injected_at_monotonic=injected_at_monotonic,
        command=command,
    )


def capture_diagnostics(
    report: dict[str, Any],
    *,
    ft_binary: str,
    workspace: Path,
    config_path: Path,
    env: dict[str, str],
) -> None:
    diagnostics: dict[str, Any] = {}
    commands = {
        "doctor": ft_cmd(ft_binary, workspace, config_path, "doctor", "--json"),
        "robot_state": ft_cmd(ft_binary, workspace, config_path, "robot", "state"),
        "robot_events": ft_cmd(ft_binary, workspace, config_path, "robot", "events", "--limit", "100"),
    }
    for key, argv in commands.items():
        diagnostics[key] = run(argv, cwd=REPO_ROOT, env=env, check=False).as_dict()
    report["diagnostics"] = diagnostics


def collect_robot_benchmark_evidence(
    report: dict[str, Any],
    *,
    ft_binary: str,
    workspace: Path,
    config_path: Path,
    env: dict[str, str],
    sustained_phase_start: float,
) -> None:
    samples = report["evidence"]["robot_samples"]

    state_started = round(time.monotonic() - sustained_phase_start, 3)
    state_result = run(
        ft_cmd(ft_binary, workspace, config_path, "robot", "state"),
        cwd=REPO_ROOT,
        env=env,
        check=False,
    )
    report["commands"]["robot_state_benchmark"] = state_result.as_dict()
    state_pane_count: int | None = None
    try:
        state_pane_count = len(extract_panes(parse_json_output(state_result)))
    except (RuntimeError, ValueError) as exc:
        report["commands"]["robot_state_benchmark_parse_error"] = str(exc)[:500]
    samples["state"] = {
        "returncode": state_result.returncode,
        "duration_ms": state_result.duration_ms,
        "pane_count": state_pane_count,
        "measured_at_elapsed_secs": state_started,
    }

    search_started = round(time.monotonic() - sustained_phase_start, 3)
    search_result = run(
        ft_cmd(ft_binary, workspace, config_path, "robot", "search", "heartbeat"),
        cwd=REPO_ROOT,
        env=env,
        check=False,
    )
    report["commands"]["robot_search_benchmark"] = search_result.as_dict()
    search_hit_count: int | None = None
    try:
        search_hit_count = len(extract_search_results(parse_json_output(search_result)))
    except (RuntimeError, ValueError) as exc:
        report["commands"]["robot_search_benchmark_parse_error"] = str(exc)[:500]
    samples["search"] = {
        "returncode": search_result.returncode,
        "duration_ms": search_result.duration_ms,
        "hit_count": search_hit_count,
        "measured_at_elapsed_secs": search_started,
    }

    get_text_started = round(time.monotonic() - sustained_phase_start, 3)
    get_text_result = run(
        ft_cmd(
            ft_binary,
            workspace,
            config_path,
            "robot",
            "get-text",
            "--all",
            "--tail",
            "5",
        ),
        cwd=REPO_ROOT,
        env=env,
        check=False,
        timeout=60.0,
    )
    report["commands"]["robot_get_text_benchmark"] = get_text_result.as_dict()
    samples["get_text"] = {
        "returncode": get_text_result.returncode,
        "duration_ms": get_text_result.duration_ms,
        "response_bytes": len(get_text_result.stdout.encode("utf-8")),
        "measured_at_elapsed_secs": get_text_started,
    }


def collect_source_provenance() -> dict[str, Any]:
    head = run(
        ["git", "rev-parse", "--verify", "HEAD"],
        cwd=REPO_ROOT,
        check=False,
    )
    tree = run(
        ["git", "rev-parse", "--verify", "HEAD^{tree}"],
        cwd=REPO_ROOT,
        check=False,
    )
    clean = run(
        ["git", "diff-index", "--quiet", "HEAD", "--"],
        cwd=REPO_ROOT,
        check=False,
    )
    return {
        "git_sha": head.stdout.strip() if head.returncode == 0 else "",
        "git_tree_sha": tree.stdout.strip() if tree.returncode == 0 else "",
        "tracked_tree_clean": clean.returncode == 0,
        "commands": {
            "head": head.as_dict(),
            "tree": tree.as_dict(),
            "tracked_tree_clean": clean.as_dict(),
        },
    }


def collect_binary_provenance(binary_path: str, resolution_source: str | None) -> dict[str, Any]:
    resolved = Path(binary_path).resolve()
    version = run([str(resolved), "--version"], cwd=REPO_ROOT, check=False, timeout=10.0)
    build_profile = authoritative_profile_from_resolution_source(resolution_source) or ""
    return {
        "path": str(resolved),
        "sha256": sha256_file(resolved),
        "size_bytes": resolved.stat().st_size,
        "version": version.stdout.strip() if version.returncode == 0 else "",
        "build_profile": build_profile,
        "resolution_source": resolution_source,
        "version_command": version.as_dict(),
    }


def collect_cpu_model() -> str:
    if platform.system() == "Darwin":
        result = run(
            ["/usr/sbin/sysctl", "-n", "machdep.cpu.brand_string"],
            cwd=REPO_ROOT,
            check=False,
            timeout=5.0,
        )
        if result.returncode == 0 and result.stdout.strip():
            return result.stdout.strip()[:512]
    if platform.system() == "Linux":
        try:
            with Path("/proc/cpuinfo").open("r", encoding="utf-8", errors="replace") as cpuinfo:
                for line in cpuinfo.read(1024 * 1024).splitlines():
                    key, separator, value = line.partition(":")
                    if separator and key.strip() in {"model name", "Hardware"} and value.strip():
                        return value.strip()[:512]
        except OSError:
            pass
    return platform.processor().strip()[:512]


def collect_physical_memory_bytes() -> int | None:
    try:
        page_size = os.sysconf("SC_PAGE_SIZE")
        physical_pages = os.sysconf("SC_PHYS_PAGES")
        memory_bytes = page_size * physical_pages
        if memory_bytes > 0:
            return memory_bytes
    except (OSError, TypeError, ValueError):
        pass
    if platform.system() == "Darwin":
        result = run(
            ["/usr/sbin/sysctl", "-n", "hw.memsize"],
            cwd=REPO_ROOT,
            check=False,
            timeout=5.0,
        )
        try:
            memory_bytes = int(result.stdout.strip()) if result.returncode == 0 else 0
        except ValueError:
            memory_bytes = 0
        if memory_bytes > 0:
            return memory_bytes
    return None


def collect_config_provenance(config_path: Path) -> dict[str, Any]:
    resolved = config_path.resolve()
    content = resolved.read_text(encoding="utf-8")
    return {
        "path": str(resolved),
        "sha256": sha256_file(resolved),
        "size_bytes": resolved.stat().st_size,
        "content": content,
    }


def collect_hardware_provenance() -> dict[str, Any]:
    hardware = {
        "system": platform.system(),
        "machine": platform.machine(),
        "release": platform.release(),
        "cpu_model": collect_cpu_model(),
        "logical_cpu_count": os.cpu_count(),
        "physical_memory_bytes": collect_physical_memory_bytes(),
        "target_class": os.environ.get("FT_E2E_TARGET_CLASS", "")[:256],
    }
    hardware["hardware_fingerprint_sha256"] = canonical_json_sha256(
        hardware_fingerprint_payload(hardware)
    )
    return hardware


def next_sample_timestamp(previous_timestamp_ms: int | None) -> int:
    timestamp = now_ms()
    if previous_timestamp_ms is not None and timestamp <= previous_timestamp_ms:
        return previous_timestamp_ms + 1
    return timestamp


def count_file_lines(path: Path) -> int:
    count = 0
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            count += chunk.count(b"\n")
    return count


def collect_pane_output_stats(
    fake_state_dir: Path,
    pane_ids: list[int],
) -> list[dict[str, int]]:
    log_dir = fake_state_dir / "logs"
    stats: list[dict[str, int]] = []
    for pane_index, pane_id in enumerate(pane_ids, start=1):
        log_path = log_dir / f"pane_{pane_id}.log"
        if log_path.is_file():
            size_bytes = log_path.stat().st_size
            line_count = count_file_lines(log_path)
        else:
            size_bytes = 0
            line_count = 0
        stats.append(
            {
                "pane_index": pane_index,
                "pane_id": pane_id,
                "size_bytes": size_bytes,
                "line_count": line_count,
            }
        )
    return stats


def collect_fleet_pressure_sample(
    result: CmdResult,
    previous_timestamp_ms: int | None,
    previous_source_snapshot_timestamp_ms: int | None,
    elapsed_secs: float,
    minimum_source_timestamp_ms: int,
) -> dict[str, Any] | None:
    if result.returncode != 0:
        return None
    payload = parse_json_output(result)
    data = payload.get("data") if isinstance(payload, dict) else None
    if not isinstance(data, dict) or data.get("watcher_running") is not True:
        return None
    health = data.get("health")
    if not isinstance(health, dict):
        return None
    source_snapshot_timestamp = health.get("timestamp")
    scrollback_telemetry = health.get("fleet_scrollback_telemetry")
    if (
        not isinstance(source_snapshot_timestamp, int)
        or isinstance(source_snapshot_timestamp, bool)
        or source_snapshot_timestamp < minimum_source_timestamp_ms
        or (
            previous_source_snapshot_timestamp_ms is not None
            and source_snapshot_timestamp <= previous_source_snapshot_timestamp_ms
        )
        or not isinstance(scrollback_telemetry, dict)
    ):
        return None
    tier = health.get("fleet_pressure_tier")
    if not isinstance(tier, str) or not tier:
        return None
    return {
        "timestamp_ms": next_sample_timestamp(previous_timestamp_ms),
        "source_snapshot_timestamp_ms": source_snapshot_timestamp,
        "elapsed_secs": round(elapsed_secs, 3),
        "tier": tier,
        "observed_panes": health.get("observed_panes"),
        "observed_pane_ids": scrollback_telemetry.get("observed_pane_ids"),
        "source": "runtime_health_snapshot",
        "evidence_state": "measured",
    }


def collect_scrollback_sample(
    result: CmdResult,
    previous_timestamp_ms: int | None,
    previous_source_snapshot_timestamp_ms: int | None,
    elapsed_secs: float,
    minimum_source_timestamp_ms: int,
) -> dict[str, Any] | None:
    if result.returncode != 0:
        return None
    payload = parse_json_output(result)
    data = payload.get("data") if isinstance(payload, dict) else None
    if not isinstance(data, dict) or data.get("watcher_running") is not True:
        return None
    health = data.get("health")
    if not isinstance(health, dict):
        return None
    source_snapshot_timestamp = health.get("timestamp")
    telemetry = health.get("fleet_scrollback_telemetry")
    if (
        not isinstance(source_snapshot_timestamp, int)
        or isinstance(source_snapshot_timestamp, bool)
        or source_snapshot_timestamp < minimum_source_timestamp_ms
        or (
            previous_source_snapshot_timestamp_ms is not None
            and source_snapshot_timestamp <= previous_source_snapshot_timestamp_ms
        )
        or not isinstance(telemetry, dict)
    ):
        return None
    return {
        "timestamp_ms": next_sample_timestamp(previous_timestamp_ms),
        "source_snapshot_timestamp_ms": source_snapshot_timestamp,
        "elapsed_secs": round(elapsed_secs, 3),
        "source": "runtime_health_snapshot.fleet_scrollback_telemetry",
        "evidence_state": "measured",
        "observed_panes": telemetry.get("observed_panes"),
        "sampled_panes": telemetry.get("sampled_panes"),
        "observed_pane_ids": telemetry.get("observed_pane_ids"),
        "sampled_pane_ids": telemetry.get("sampled_pane_ids"),
        "telemetry_blind": telemetry.get("telemetry_blind"),
        "telemetry_partial": telemetry.get("telemetry_partial"),
        "tiering_enabled_panes": telemetry.get("tiering_enabled_panes"),
        "configured_hot_lines_min": telemetry.get("configured_hot_lines_min"),
        "configured_hot_lines_max": telemetry.get("configured_hot_lines_max"),
        "warm_spill_lines_total": telemetry.get("warm_spill_lines_total"),
        "warm_spill_bytes_total": telemetry.get("warm_spill_bytes_total"),
    }


def sqlite_quick_check(db_path: Path) -> str:
    if not db_path.is_file():
        return "database_missing"
    try:
        uri = f"file:{db_path.resolve()}?mode=ro"
        with sqlite3.connect(uri, uri=True, timeout=5.0) as connection:
            rows = connection.execute("PRAGMA quick_check").fetchall()
    except sqlite3.Error as exc:
        return f"sqlite_error:{type(exc).__name__}"
    if rows == [("ok",)]:
        return "ok"
    return "quick_check_failed"


def scan_runtime_log(log_path: Path) -> dict[str, Any]:
    if not log_path.is_file():
        return {
            "watch_log_size_bytes": 0,
            "watch_log_scan_complete": False,
            "crash_signals": ["watch_log_missing"],
        }
    size = log_path.stat().st_size
    if size > MAX_RUNTIME_LOG_SCAN_BYTES:
        return {
            "watch_log_size_bytes": size,
            "watch_log_scan_complete": False,
            "crash_signals": ["watch_log_exceeds_scan_bound"],
        }
    text = log_path.read_text(encoding="utf-8", errors="replace")
    needles = (
        "panicked at",
        "fatal runtime error",
        "segmentation fault",
        "memory allocation of",
        "stack overflow",
        "signal: 6",
        "signal: 11",
        "database disk image is malformed",
        "database corruption",
    )
    matches: list[str] = []
    for line in text.splitlines():
        lowered = line.lower()
        if any(needle in lowered for needle in needles):
            matches.append(line[:500])
            if len(matches) >= 20:
                break
    return {
        "watch_log_size_bytes": size,
        "watch_log_scan_complete": True,
        "crash_signals": matches,
    }


def persist_report(report_path: Path, report: dict[str, Any]) -> None:
    payload = json.dumps(
        report,
        allow_nan=False,
        ensure_ascii=True,
        indent=2,
        sort_keys=True,
    ) + "\n"
    encoded_size = len(payload.encode("utf-8"))
    if encoded_size > MAX_REPORT_BYTES:
        raise ValueError(
            f"refusing to write {encoded_size}-byte report; maximum is {MAX_REPORT_BYTES}"
        )
    report_path.parent.mkdir(parents=True, exist_ok=True)
    with report_path.open("x", encoding="utf-8") as report_file:
        report_file.write(payload)


def verifier_fixture_report() -> dict[str, Any]:
    """Construct a complete synthetic report for pure verifier boundary tests."""
    run_id = "ft-d0ez0.5-verifier-self-test"
    pane_output_deltas = [
        {
            "pane_index": pane_index,
            "pane_id": pane_index,
            "lines_at_start": 0,
            "lines_at_end": minimum_output_lines_for_pane(
                pane_index,
                FULL_DURATION_SECS,
            ),
            "observed_lines": minimum_output_lines_for_pane(
                pane_index,
                FULL_DURATION_SECS,
            ),
            "bytes_at_start": 0,
            "bytes_at_end": minimum_output_lines_for_pane(
                pane_index,
                FULL_DURATION_SECS,
            )
            * OUTPUT_PAYLOAD_BYTES_PER_LINE,
            "observed_bytes": minimum_output_lines_for_pane(
                pane_index,
                FULL_DURATION_SECS,
            )
            * OUTPUT_PAYLOAD_BYTES_PER_LINE,
        }
        for pane_index in range(1, NUM_PANES + 1)
    ]
    observed_output_lines = sum(
        sample["observed_lines"] for sample in pane_output_deltas
    )
    observed_output_bytes = sum(
        sample["observed_bytes"] for sample in pane_output_deltas
    )
    hardware = {
        "system": "VerifierOS",
        "machine": "verifier64",
        "release": "1",
        "cpu_model": "Verifier CPU 128-core",
        "logical_cpu_count": 128,
        "physical_memory_bytes": 256 * 1024 * 1024 * 1024,
        "target_class": "verifier-self-test",
    }
    hardware["hardware_fingerprint_sha256"] = canonical_json_sha256(
        hardware_fingerprint_payload(hardware)
    )
    receipt = {
        "schema_version": AUTHORIZATION_RECEIPT_SCHEMA_VERSION,
        "scope": AUTHORIZATION_SCOPE,
        "run_id": run_id,
        "authorized_at_utc": "2026-08-08T00:00:00Z",
        "authorized_by": "verifier-self-test",
        "target_workspace": "/isolated/verifier-self-test",
        "operator_session_untouched": True,
        "source_git_sha": "a" * 40,
        "binary_sha256": "c" * 64,
        "config_sha256": hashlib.sha256(CONFIG_TEMPLATE.encode("utf-8")).hexdigest(),
        "hardware_fingerprint_sha256": hardware["hardware_fingerprint_sha256"],
        "requested_sustained_duration_secs": FULL_DURATION_SECS,
        "required_panes": NUM_PANES,
    }
    return {
        "contract_id": REPORT_CONTRACT_ID,
        "schema_version": REPORT_SCHEMA_VERSION,
        "verifier_version": VERIFIER_VERSION,
        "bead_id": "ft-d0ez0.5",
        "run_id": run_id,
        "generated_at_utc": "2026-08-08T00:05:00Z",
        "thresholds": {
            "minimum_sustained_duration_secs": FULL_DURATION_SECS,
            "maximum_supported_duration_secs": MAX_SUPPORTED_DURATION_SECS,
            "detection_latency_limit_ms_exclusive": DETECTION_LATENCY_LIMIT_MS,
            "robot_state_latency_limit_ms_exclusive": ROBOT_STATE_LATENCY_LIMIT_MS,
            "robot_search_latency_limit_ms_exclusive": ROBOT_SEARCH_LATENCY_LIMIT_MS,
            "rss_limit_kb_exclusive": RSS_LIMIT_KB,
            "rss_sample_interval_secs": RSS_SAMPLE_INTERVAL_SECS,
            "rss_max_sample_gap_secs": RSS_MAX_SAMPLE_GAP_SECS,
            "rss_final_coverage_slack_secs": RSS_FINAL_COVERAGE_SLACK_SECS,
            "high_output_phase_fraction": HIGH_OUTPUT_PHASE_FRACTION,
            "high_output_phase_tolerance_secs": HIGH_OUTPUT_PHASE_TOLERANCE_SECS,
            "output_rate_minimum_fraction": OUTPUT_RATE_MINIMUM_FRACTION,
            "output_payload_bytes_per_line": OUTPUT_PAYLOAD_BYTES_PER_LINE,
            "fleet_scrollback_per_pane_budget_bytes": FLEET_SCROLLBACK_PER_PANE_BUDGET_BYTES,
            "fleet_scrollback_high_ratio": FLEET_SCROLLBACK_HIGH_RATIO,
            "mux_scrollback_hot_lines": MUX_SCROLLBACK_HOT_LINES,
            "detection_probe_phase_fraction": DETECTION_PROBE_PHASE_FRACTION,
            "detection_probe_phase_tolerance_secs": DETECTION_PROBE_PHASE_TOLERANCE_SECS,
            "robot_benchmark_phase_fraction": ROBOT_BENCHMARK_PHASE_FRACTION,
            "robot_benchmark_phase_tolerance_secs": ROBOT_BENCHMARK_PHASE_TOLERANCE_SECS,
            "required_panes": NUM_PANES,
        },
        "authority": {
            "transport": "native_isolated_mux",
            "evidence_state": "measured",
            "isolated_workspace": True,
            "operator_session_untouched": True,
            "target_workspace": "/isolated/verifier-self-test",
            "authorization_receipt_ref": (
                "/isolated/verifier-self-test/authorization-receipt.json"
            ),
            "authorization_receipt": receipt,
            "authorization_receipt_sha256": canonical_json_sha256(receipt),
        },
        "workspace": {"root": "/isolated/verifier-self-test"},
        "provenance": {
            "source": {
                "git_sha": "a" * 40,
                "git_tree_sha": "b" * 40,
                "tracked_tree_clean": True,
            },
            "binary": {
                "path": "/isolated/verifier-self-test/target/release-perf/ft",
                "sha256": "c" * 64,
                "size_bytes": 1,
                "version": f"ft 0.1.0 ({'a' * 40})",
                "build_profile": "release-perf",
                "resolution_source": "cargo_target_dir:release-perf",
            },
            "config": {
                "path": "/isolated/verifier-self-test/ft.toml",
                "sha256": hashlib.sha256(CONFIG_TEMPLATE.encode("utf-8")).hexdigest(),
                "size_bytes": len(CONFIG_TEMPLATE.encode("utf-8")),
                "content": CONFIG_TEMPLATE,
            },
            "hardware": hardware,
        },
        "workload": {
            "requested_sustained_duration_secs": FULL_DURATION_SECS,
            "observed_sustained_duration_secs": FULL_DURATION_SECS,
            "panes_spawned": NUM_PANES,
            "panes_observed": NUM_PANES,
            "spawned_pane_ids": list(range(1, NUM_PANES + 1)),
            "high_output_panes": HIGH_OUTPUT_PANES,
            "high_output_phase_started": True,
            "configured_high_output_phase_fraction": HIGH_OUTPUT_PHASE_FRACTION,
            "high_output_phase_started_elapsed_secs": (
                FULL_DURATION_SECS * HIGH_OUTPUT_PHASE_FRACTION
            ),
            "sustained_started_epoch_ms": 1,
            "observed_output_bytes": observed_output_bytes,
            "output_bytes_at_sustained_start": 0,
            "output_bytes_at_sustained_end": observed_output_bytes,
            "observed_output_lines": observed_output_lines,
            "output_lines_at_sustained_start": 0,
            "output_lines_at_sustained_end": observed_output_lines,
            "pane_output_deltas": pane_output_deltas,
            "normal_lines_per_second_per_pane": NORMAL_LINES_PER_SECOND_PER_PANE,
            "high_lines_per_second_per_pane": HIGH_LINES_PER_SECOND_PER_PANE,
        },
        "evidence": {
            "detection_probe": {
                "rule_id": DETECTION_RULE_ID,
                "pane_id": 25,
                "injected_at_epoch_ms": 1_000,
                "injected_at_elapsed_secs": (
                    FULL_DURATION_SECS * DETECTION_PROBE_PHASE_FRACTION
                ),
                "observed_after_ms": 4_999.0,
                "matching_events": [
                    {
                        "id": 1,
                        "rule_id": DETECTION_RULE_ID,
                        "pane_id": 25,
                        "captured_at": 5_999,
                    }
                ],
            },
            "fleet_pressure_samples": [
                {
                    "timestamp_ms": 1,
                    "source_snapshot_timestamp_ms": 1,
                    "elapsed_secs": 0.0,
                    "tier": "Normal",
                    "observed_panes": NUM_PANES,
                    "observed_pane_ids": list(range(1, NUM_PANES + 1)),
                    "source": "runtime_health_snapshot",
                    "evidence_state": "measured",
                },
                {
                    "timestamp_ms": 2,
                    "source_snapshot_timestamp_ms": 2,
                    "elapsed_secs": 180.0,
                    "tier": "Elevated",
                    "observed_panes": NUM_PANES,
                    "observed_pane_ids": list(range(1, NUM_PANES + 1)),
                    "source": "runtime_health_snapshot",
                    "evidence_state": "measured",
                },
            ],
            "scrollback_samples": [
                {
                    "timestamp_ms": 1,
                    "source_snapshot_timestamp_ms": 1,
                    "elapsed_secs": 0.0,
                    "source": "runtime_health_snapshot.fleet_scrollback_telemetry",
                    "evidence_state": "measured",
                    "observed_panes": NUM_PANES,
                    "sampled_panes": NUM_PANES,
                    "observed_pane_ids": list(range(1, NUM_PANES + 1)),
                    "sampled_pane_ids": list(range(1, NUM_PANES + 1)),
                    "telemetry_blind": False,
                    "telemetry_partial": False,
                    "tiering_enabled_panes": NUM_PANES,
                    "configured_hot_lines_min": MUX_SCROLLBACK_HOT_LINES,
                    "configured_hot_lines_max": MUX_SCROLLBACK_HOT_LINES,
                    "warm_spill_lines_total": 0,
                    "warm_spill_bytes_total": 0,
                },
                {
                    "timestamp_ms": 2,
                    "source_snapshot_timestamp_ms": 2,
                    "elapsed_secs": 200.0,
                    "source": "runtime_health_snapshot.fleet_scrollback_telemetry",
                    "evidence_state": "measured",
                    "observed_panes": NUM_PANES,
                    "sampled_panes": NUM_PANES,
                    "observed_pane_ids": list(range(1, NUM_PANES + 1)),
                    "sampled_pane_ids": list(range(1, NUM_PANES + 1)),
                    "telemetry_blind": False,
                    "telemetry_partial": False,
                    "tiering_enabled_panes": NUM_PANES,
                    "configured_hot_lines_min": MUX_SCROLLBACK_HOT_LINES,
                    "configured_hot_lines_max": MUX_SCROLLBACK_HOT_LINES,
                    "warm_spill_lines_total": 100,
                    "warm_spill_bytes_total": 8_192,
                },
            ],
            "rss_samples": [
                {
                    "timestamp_ms": index + 1,
                    "elapsed_secs": float(index * 10),
                    "rss_kb": 1_000 + index,
                }
                for index in range(31)
            ],
            "robot_samples": {
                "state": {
                    "returncode": 0,
                    "duration_ms": 1_999,
                    "pane_count": NUM_PANES,
                    "measured_at_elapsed_secs": (
                        FULL_DURATION_SECS * ROBOT_BENCHMARK_PHASE_FRACTION
                    ),
                },
                "search": {
                    "returncode": 0,
                    "duration_ms": 4_999,
                    "hit_count": 1,
                    "measured_at_elapsed_secs": (
                        FULL_DURATION_SECS * ROBOT_BENCHMARK_PHASE_FRACTION
                    ),
                },
                "get_text": {
                    "returncode": 0,
                    "duration_ms": 9_999,
                    "response_bytes": 1,
                    "measured_at_elapsed_secs": (
                        FULL_DURATION_SECS * ROBOT_BENCHMARK_PHASE_FRACTION
                    ),
                },
            },
            "runtime": {
                "process_survived": True,
                "watch_exit_code": None,
                "watch_log_size_bytes": 0,
                "watch_log_scan_complete": True,
                "sqlite_quick_check": "ok",
                "crash_signals": [],
                "data_corruption_signals": [],
            },
        },
        "cleanup": {
            "owned_panes_closed": True,
            "owned_watch_stopped": True,
            "workspace_disposition_complete": True,
            "workspace_kept": False,
            "errors": [],
        },
        "checks": [{"passed": True, "note": "must be ignored"}],
    }


def clone_json(value: dict[str, Any]) -> dict[str, Any]:
    clone = json.loads(json.dumps(value, allow_nan=False))
    if not isinstance(clone, dict):
        raise AssertionError("JSON clone root changed type")
    return clone


def verification_check(result: dict[str, Any], name: str) -> dict[str, Any]:
    for check in result.get("checks", []):
        if isinstance(check, dict) and check.get("check_name") == name:
            return check
    raise AssertionError(f"verifier result omitted check {name}")


def run_verifier_self_tests() -> None:
    compile(FAKE_WEZTERM_SCRIPT, "<fake-wezterm-fixture>", "exec")
    baseline = verifier_fixture_report()
    result = verify_stress_report(baseline)
    assert result["status"] == "passed", result
    assert result["authoritative"] is True
    assert result["ignored_precomputed_check_count"] == 1
    assert not absolute_path_is_within(
        "/isolated/verifier-self-test/../operator-live/receipt.json",
        "/isolated/verifier-self-test",
    )

    fixture = clone_json(baseline)
    fixture["authority"]["transport"] = "fixture_wezterm_cli"
    fixture["authority"]["evidence_state"] = "fixture_only"
    assert verify_stress_report(fixture)["status"] == "skipped_not_proven"

    short = clone_json(baseline)
    short["workload"]["observed_sustained_duration_secs"] = FULL_DURATION_SECS - 0.001
    short_result = verify_stress_report(short)
    assert short_result["status"] == "failed"
    assert verification_check(short_result, "full_sustained_duration")["reason_code"] == (
        "duration.below_minimum"
    )

    unbounded_duration = clone_json(baseline)
    unbounded_duration["workload"]["requested_sustained_duration_secs"] = (
        MAX_SUPPORTED_DURATION_SECS + 1.0
    )
    unbounded_duration["workload"]["observed_sustained_duration_secs"] = (
        MAX_SUPPORTED_DURATION_SECS + 1.0
    )
    unbounded_result = verify_stress_report(unbounded_duration)
    assert unbounded_result["status"] == "failed"
    assert verification_check(unbounded_result, "full_sustained_duration")[
        "reason_code"
    ] == "duration.above_supported_sample_bound"

    detection_boundary = clone_json(baseline)
    detection_boundary["evidence"]["detection_probe"]["matching_events"][0][
        "captured_at"
    ] = 6_000
    assert verify_stress_report(detection_boundary)["status"] == "failed"

    detection_wrong_phase = clone_json(baseline)
    detection_wrong_phase["evidence"]["detection_probe"]["injected_at_elapsed_secs"] = 60.0
    assert verify_stress_report(detection_wrong_phase)["status"] == "failed"

    rss_boundary = clone_json(baseline)
    rss_boundary["evidence"]["rss_samples"][1]["rss_kb"] = RSS_LIMIT_KB
    assert verify_stress_report(rss_boundary)["status"] == "failed"

    rss_gap = clone_json(baseline)
    rss_gap["evidence"]["rss_samples"] = [
        rss_gap["evidence"]["rss_samples"][0],
        rss_gap["evidence"]["rss_samples"][-1],
    ]
    assert verify_stress_report(rss_gap)["status"] == "skipped_not_proven"

    no_pressure_transition = clone_json(baseline)
    no_pressure_transition["evidence"]["fleet_pressure_samples"][1]["tier"] = "Normal"
    assert verify_stress_report(no_pressure_transition)["status"] == "failed"

    stale_pressure_snapshot = clone_json(baseline)
    stale_pressure_snapshot["evidence"]["fleet_pressure_samples"][1][
        "source_snapshot_timestamp_ms"
    ] = 1
    assert (
        verify_stress_report(stale_pressure_snapshot)["status"]
        == "skipped_not_proven"
    )

    mismatched_pressure_population = clone_json(baseline)
    mismatched_pressure_population["evidence"]["fleet_pressure_samples"][1][
        "observed_pane_ids"
    ][-1] = NUM_PANES + 1
    assert (
        verify_stress_report(mismatched_pressure_population)["status"]
        == "skipped_not_proven"
    )

    spill_without_transition = clone_json(baseline)
    spill_without_transition["evidence"]["scrollback_samples"][1][
        "warm_spill_lines_total"
    ] = 0
    assert verify_stress_report(spill_without_transition)["status"] == "failed"

    disabled_tiering = clone_json(baseline)
    disabled_tiering["evidence"]["scrollback_samples"][1][
        "tiering_enabled_panes"
    ] = NUM_PANES - 1
    assert verify_stress_report(disabled_tiering)["status"] == "skipped_not_proven"

    drifted_hot_tier = clone_json(baseline)
    drifted_hot_tier["evidence"]["scrollback_samples"][1][
        "configured_hot_lines_max"
    ] = MUX_SCROLLBACK_HOT_LINES + 1
    assert verify_stress_report(drifted_hot_tier)["status"] == "skipped_not_proven"

    stale_scrollback_snapshot = clone_json(baseline)
    stale_scrollback_snapshot["evidence"]["scrollback_samples"][1][
        "source_snapshot_timestamp_ms"
    ] = 1
    assert (
        verify_stress_report(stale_scrollback_snapshot)["status"]
        == "skipped_not_proven"
    )

    blind_scrollback = clone_json(baseline)
    blind_scrollback["evidence"]["scrollback_samples"][1]["telemetry_blind"] = True
    assert verify_stress_report(blind_scrollback)["status"] == "skipped_not_proven"

    mismatched_scrollback_population = clone_json(baseline)
    mismatched_scrollback_population["evidence"]["scrollback_samples"][1][
        "sampled_pane_ids"
    ][-1] = NUM_PANES + 1
    assert (
        verify_stress_report(mismatched_scrollback_population)["status"]
        == "skipped_not_proven"
    )

    missing_pressure = clone_json(baseline)
    missing_pressure["evidence"]["fleet_pressure_samples"] = []
    assert verify_stress_report(missing_pressure)["status"] == "skipped_not_proven"

    forged_receipt = clone_json(baseline)
    forged_receipt["authority"]["authorization_receipt"]["authorized_by"] = "mutated"
    assert verify_stress_report(forged_receipt)["status"] == "skipped_not_proven"

    external_receipt_ref = clone_json(baseline)
    external_receipt_ref["authority"]["authorization_receipt_ref"] = (
        "/operator/live/authorization-receipt.json"
    )
    assert verify_stress_report(external_receipt_ref)["status"] == "skipped_not_proven"

    replayed_receipt = clone_json(baseline)
    replayed_receipt["provenance"]["binary"]["sha256"] = "d" * 64
    assert verify_stress_report(replayed_receipt)["status"] == "skipped_not_proven"

    drifted_budget = clone_json(baseline)
    drifted_budget["thresholds"]["fleet_scrollback_per_pane_budget_bytes"] += 1
    assert verify_stress_report(drifted_budget)["status"] == "failed"

    tampered_config = clone_json(baseline)
    tampered_content = CONFIG_TEMPLATE.replace(
        "per_pane_budget_bytes = 524288",
        "per_pane_budget_bytes = 524289",
    )
    tampered_config["provenance"]["config"]["content"] = tampered_content
    tampered_config["provenance"]["config"]["sha256"] = hashlib.sha256(
        tampered_content.encode("utf-8")
    ).hexdigest()
    tampered_config["provenance"]["config"]["size_bytes"] = len(
        tampered_content.encode("utf-8")
    )
    assert verify_stress_report(tampered_config)["status"] == "skipped_not_proven"

    corrupted = clone_json(baseline)
    corrupted["evidence"]["runtime"]["sqlite_quick_check"] = "quick_check_failed"
    corrupted["evidence"]["runtime"]["data_corruption_signals"] = ["quick_check_failed"]
    assert verify_stress_report(corrupted)["status"] == "failed"

    cleanup_failed = clone_json(baseline)
    cleanup_failed["cleanup"]["owned_watch_stopped"] = False
    cleanup_failed["cleanup"]["errors"] = ["owned watch remained alive"]
    assert verify_stress_report(cleanup_failed)["status"] == "failed"

    failed_robot = clone_json(baseline)
    failed_robot["evidence"]["robot_samples"]["state"]["returncode"] = 1
    assert verify_stress_report(failed_robot)["status"] == "failed"

    empty_search = clone_json(baseline)
    empty_search["evidence"]["robot_samples"]["search"]["hit_count"] = 0
    assert verify_stress_report(empty_search)["status"] == "failed"

    robot_wrong_phase = clone_json(baseline)
    robot_wrong_phase["evidence"]["robot_samples"]["state"][
        "measured_at_elapsed_secs"
    ] = FULL_DURATION_SECS
    assert verify_stress_report(robot_wrong_phase)["status"] == "failed"

    wrong_rate = clone_json(baseline)
    wrong_rate["workload"]["high_lines_per_second_per_pane"] = 2.0
    assert verify_stress_report(wrong_rate)["status"] == "failed"

    underproducing_pane = clone_json(baseline)
    underproducing_pane["workload"]["pane_output_deltas"][0][
        "lines_at_end"
    ] = 1
    underproducing_pane["workload"]["pane_output_deltas"][0][
        "observed_lines"
    ] = 1
    assert verify_stress_report(underproducing_pane)["status"] == "failed"

    undersized_payload = clone_json(baseline)
    payload_sample = undersized_payload["workload"]["pane_output_deltas"][0]
    original_observed_bytes = payload_sample["observed_bytes"]
    payload_sample["observed_bytes"] = (
        payload_sample["observed_lines"] * (OUTPUT_PAYLOAD_BYTES_PER_LINE - 1)
    )
    payload_sample["bytes_at_end"] = payload_sample["observed_bytes"]
    removed_bytes = original_observed_bytes - payload_sample["observed_bytes"]
    undersized_payload["workload"]["observed_output_bytes"] -= removed_bytes
    undersized_payload["workload"]["output_bytes_at_sustained_end"] -= removed_bytes
    assert verify_stress_report(undersized_payload)["status"] == "failed"

    duplicate_pane_identity = clone_json(baseline)
    duplicate_pane_identity["workload"]["spawned_pane_ids"][1] = 1
    assert verify_stress_report(duplicate_pane_identity)["status"] == "failed"

    debug_binary = clone_json(baseline)
    debug_binary["provenance"]["binary"]["build_profile"] = "debug"
    assert verify_stress_report(debug_binary)["status"] == "skipped_not_proven"

    source_binary_mismatch = clone_json(baseline)
    source_binary_mismatch["provenance"]["binary"]["version"] = (
        f"ft 0.1.0 ({'d' * 40})"
    )
    assert (
        verify_stress_report(source_binary_mismatch)["status"]
        == "skipped_not_proven"
    )

    dirty_binary = clone_json(baseline)
    dirty_binary["provenance"]["binary"]["version"] = f"ft 0.1.0 ({'a' * 40}+dirty)"
    assert verify_stress_report(dirty_binary)["status"] == "skipped_not_proven"

    caller_labeled_profile = clone_json(baseline)
    caller_labeled_profile["provenance"]["binary"]["resolution_source"] = "env:FT_BINARY"
    assert (
        verify_stress_report(caller_labeled_profile)["status"]
        == "skipped_not_proven"
    )

    wrong_profile_path = clone_json(baseline)
    wrong_profile_path["provenance"]["binary"]["path"] = (
        "/isolated/verifier-self-test/target/debug/ft"
    )
    assert verify_stress_report(wrong_profile_path)["status"] == "skipped_not_proven"

    external_binary_path = clone_json(baseline)
    external_binary_path["provenance"]["binary"]["path"] = (
        "/operator/live/target/release-perf/ft"
    )
    assert (
        verify_stress_report(external_binary_path)["status"]
        == "skipped_not_proven"
    )

    external_config_path = clone_json(baseline)
    external_config_path["provenance"]["config"]["path"] = "/operator/live/ft.toml"
    assert (
        verify_stress_report(external_config_path)["status"]
        == "skipped_not_proven"
    )

    mismatched_profile_source = clone_json(baseline)
    mismatched_profile_source["provenance"]["binary"][
        "resolution_source"
    ] = "cargo_target_dir:release-interactive"
    assert (
        verify_stress_report(mismatched_profile_source)["status"]
        == "skipped_not_proven"
    )

    tampered_hardware = clone_json(baseline)
    tampered_hardware["provenance"]["hardware"]["target_class"] = "forged-target"
    assert verify_stress_report(tampered_hardware)["status"] == "skipped_not_proven"

    false_green = {
        "schema_version": "ft.e2e_50pane_stress.v1",
        "checks": [{"passed": True} for _ in range(100)],
    }
    assert verify_stress_report(false_green)["status"] != "passed"

    for section in (
        "thresholds",
        "authority",
        "workspace",
        "provenance",
        "workload",
        "evidence",
        "cleanup",
    ):
        for hostile_value in (None, False, 1, "hostile", []):
            hostile = clone_json(baseline)
            hostile[section] = hostile_value
            assert verify_stress_report(hostile)["status"] != "passed"

    try:
        json.loads('{"duplicate":1,"duplicate":2}', object_pairs_hook=strict_json_object)
    except DuplicateJsonKey:
        pass
    else:
        raise AssertionError("duplicate JSON key was accepted")

    try:
        json.loads('{"value":NaN}', parse_constant=reject_json_constant)
    except ValueError:
        pass
    else:
        raise AssertionError("non-finite JSON number was accepted")

    marker = Path("/isolated/marker with spaces")
    normal_script = pane_script_text(1, False, marker)
    high_script = pane_script_text(41, True, marker)
    for script in (normal_script, high_script):
        syntax = subprocess.run(
            ["bash", "-n"],
            input=script,
            capture_output=True,
            text=True,
            timeout=5.0,
        )
        assert syntax.returncode == 0, syntax.stderr
    assert str(marker) not in normal_script
    assert shlex.quote(str(marker)) in high_script
    expected_payload_assignment = f"PAYLOAD={'x' * OUTPUT_PAYLOAD_BYTES_PER_LINE}"
    assert expected_payload_assignment in normal_script
    assert expected_payload_assignment in high_script

    print("E2E_50PANE_VERIFIER_SELF_TEST_SUCCESS")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Fail-closed 50-pane stress evidence harness (ft-d0ez0.5)"
    )
    offline_modes = parser.add_mutually_exclusive_group()
    offline_modes.add_argument(
        "--verify-report",
        type=Path,
        help="Verify a retained report without running any FrankenTerm process",
    )
    offline_modes.add_argument(
        "--self-test-verifier",
        action="store_true",
        help="Run deterministic pure verifier tests without launching FrankenTerm",
    )
    parser.add_argument(
        "--duration-secs",
        type=float,
        default=DEFAULT_DURATION_SECS,
        help="Sustained-load duration; authoritative minimum is 300 seconds",
    )
    parser.add_argument("--timeout-secs", type=float, default=DEFAULT_TIMEOUT_SECS)
    parser.add_argument("--output", default="", help="Override report path")
    parser.add_argument("--keep-workspace", action="store_true", help="Preserve temp workspace")
    args = parser.parse_args()

    if args.self_test_verifier:
        run_verifier_self_tests()
        return 0
    if args.verify_report is not None:
        try:
            retained_report = load_json_bounded(args.verify_report)
            verification = verify_stress_report(retained_report)
        except (OSError, ValueError) as exc:
            print(f"report verification failed closed: {exc}", file=sys.stderr)
            return 1
        print(json.dumps(verification, allow_nan=False, indent=2, sort_keys=True))
        if verification["status"] == "passed":
            return 0
        if verification["status"] == "skipped_not_proven":
            return SKIP_EXIT_CODE
        return 1
    if not is_finite_number(args.duration_secs) or args.duration_secs <= 0.0:
        parser.error("--duration-secs must be a positive finite number")
    if args.duration_secs > MAX_SUPPORTED_DURATION_SECS:
        parser.error(
            "--duration-secs exceeds the bounded telemetry capacity "
            f"({MAX_SUPPORTED_DURATION_SECS:.0f}s maximum)"
        )
    if not is_finite_number(args.timeout_secs) or args.timeout_secs <= 0.0:
        parser.error("--timeout-secs must be a positive finite number")

    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    run_id = f"ft-d0ez0.5-{timestamp}"
    output_dir = ensure_output_dir()
    report_path = Path(args.output) if args.output else output_dir / f"e2e_50pane_stress_{timestamp}.json"
    watch_log_path = output_dir / f"e2e_50pane_stress_{timestamp}.watch.log"
    run_started = time.monotonic()

    report: dict[str, Any] = {
        "contract_id": REPORT_CONTRACT_ID,
        "schema_version": REPORT_SCHEMA_VERSION,
        "verifier_version": VERIFIER_VERSION,
        "generated_at_utc": utc_now(),
        "bead_id": "ft-d0ez0.5",
        "run_id": run_id,
        "status": "failed",
        "skip_exit_code": SKIP_EXIT_CODE,
        "report_path": repo_relative(report_path),
        "watch_log_path": repo_relative(watch_log_path),
        "thresholds": {
            "minimum_sustained_duration_secs": FULL_DURATION_SECS,
            "maximum_supported_duration_secs": MAX_SUPPORTED_DURATION_SECS,
            "detection_latency_limit_ms_exclusive": DETECTION_LATENCY_LIMIT_MS,
            "robot_state_latency_limit_ms_exclusive": ROBOT_STATE_LATENCY_LIMIT_MS,
            "robot_search_latency_limit_ms_exclusive": ROBOT_SEARCH_LATENCY_LIMIT_MS,
            "rss_limit_kb_exclusive": RSS_LIMIT_KB,
            "rss_sample_interval_secs": RSS_SAMPLE_INTERVAL_SECS,
            "rss_max_sample_gap_secs": RSS_MAX_SAMPLE_GAP_SECS,
            "rss_final_coverage_slack_secs": RSS_FINAL_COVERAGE_SLACK_SECS,
            "high_output_phase_fraction": HIGH_OUTPUT_PHASE_FRACTION,
            "high_output_phase_tolerance_secs": HIGH_OUTPUT_PHASE_TOLERANCE_SECS,
            "output_rate_minimum_fraction": OUTPUT_RATE_MINIMUM_FRACTION,
            "output_payload_bytes_per_line": OUTPUT_PAYLOAD_BYTES_PER_LINE,
            "fleet_scrollback_per_pane_budget_bytes": FLEET_SCROLLBACK_PER_PANE_BUDGET_BYTES,
            "fleet_scrollback_high_ratio": FLEET_SCROLLBACK_HIGH_RATIO,
            "mux_scrollback_hot_lines": MUX_SCROLLBACK_HOT_LINES,
            "detection_probe_phase_fraction": DETECTION_PROBE_PHASE_FRACTION,
            "detection_probe_phase_tolerance_secs": DETECTION_PROBE_PHASE_TOLERANCE_SECS,
            "robot_benchmark_phase_fraction": ROBOT_BENCHMARK_PHASE_FRACTION,
            "robot_benchmark_phase_tolerance_secs": ROBOT_BENCHMARK_PHASE_TOLERANCE_SECS,
            "required_panes": NUM_PANES,
        },
        "authority": {
            "transport": "fixture_wezterm_cli",
            "evidence_state": "fixture_only",
            "isolated_workspace": True,
            "operator_session_untouched": True,
            "authorization_receipt_ref": None,
            "authorization_receipt": None,
            "authorization_receipt_sha256": None,
        },
        "provenance": {
            "source": collect_source_provenance(),
            "binary": {},
            "config": {},
            "hardware": collect_hardware_provenance(),
        },
        "workload": {
            "requested_sustained_duration_secs": args.duration_secs,
            "observed_sustained_duration_secs": None,
            "panes_spawned": None,
            "panes_observed": None,
            "spawned_pane_ids": [],
            "high_output_panes": HIGH_OUTPUT_PANES,
            "high_output_phase_started": False,
            "configured_high_output_phase_fraction": HIGH_OUTPUT_PHASE_FRACTION,
            "high_output_phase_started_elapsed_secs": None,
            "sustained_started_epoch_ms": None,
            "observed_output_bytes": 0,
            "output_bytes_at_sustained_start": None,
            "output_bytes_at_sustained_end": None,
            "observed_output_lines": 0,
            "output_lines_at_sustained_start": None,
            "output_lines_at_sustained_end": None,
            "pane_output_deltas": [],
            "normal_lines_per_second_per_pane": NORMAL_LINES_PER_SECOND_PER_PANE,
            "high_lines_per_second_per_pane": HIGH_LINES_PER_SECOND_PER_PANE,
        },
        "evidence": {
            "detection_probe": {},
            "fleet_pressure_samples": [],
            "scrollback_samples": [],
            "rss_samples": [],
            "robot_samples": {},
            "runtime": {},
        },
        "binary_resolution": {},
        "workspace": {},
        "commands": {},
        "verification": {},
    }

    workspace: Path | None = None
    config_path: Path | None = None
    watch_proc: subprocess.Popen[str] | None = None
    fake_wezterm: Path | None = None
    fake_state_dir: Path | None = None
    high_output_phase_marker: Path | None = None
    env: dict[str, str] | None = None
    spawned_panes: list[int] = []
    observed_pane_ids: set[int] = set()
    binary: BinaryChoice | None = None
    requested_exit_code = 1

    try:
        # ---------------------------------------------------------------
        # Phase 0: Setup
        # ---------------------------------------------------------------
        binary = resolve_ft_binary()
        report["binary_resolution"] = {
            "selected_path": binary.path,
            "selected_source": binary.source,
            "candidates": binary.candidates,
        }
        if not binary.path:
            raise SkipScenario("ft binary not found via FT_BINARY, cargo build output, or PATH")
        report["provenance"]["binary"] = collect_binary_provenance(
            binary.path,
            binary.source,
        )

        workspace, config_path = make_workspace()
        fake_wezterm, fake_state_dir = write_fake_wezterm_cli(workspace)
        high_output_phase_marker = workspace / ".high-output-phase"
        env = ft_env(workspace, fake_wezterm, fake_state_dir)
        report["provenance"]["config"] = collect_config_provenance(config_path)
        report["workspace"] = {
            "root": str(workspace),
            "data_dir": str(workspace / ".ft"),
            "config_path": str(config_path),
        }
        report["authority"]["target_workspace"] = str(workspace)

        # Start ft watch with fast polling
        watch_argv = ft_cmd(
            binary.path, workspace, config_path,
            "watch", "--foreground", "--poll-interval", "200",
        )
        with watch_log_path.open("x", encoding="utf-8") as watch_log:
            watch_proc = subprocess.Popen(
                watch_argv,
                cwd=str(REPO_ROOT),
                env=env,
                stdout=watch_log,
                stderr=subprocess.STDOUT,
                text=True,
            )

        def watch_ready() -> bool:
            if watch_proc.poll() is not None:
                raise RuntimeError(f"ft watch exited immediately with code {watch_proc.returncode}")
            return (workspace / ".ft" / "ft.db").exists()

        wait_for("ft watch to initialize", min(args.timeout_secs, 15.0), watch_ready)

        # ---------------------------------------------------------------
        # Phase 1: Warm-up — spawn 50 panes
        # ---------------------------------------------------------------
        phase1_start = time.monotonic()

        for index in range(1, NUM_PANES + 1):
            high_output = index in HIGH_OUTPUT_PANES
            script_path = write_pane_script(
                workspace,
                index,
                high_output,
                high_output_phase_marker,
            )
            pane_id = spawn_pane(fake_wezterm, workspace, script_path, env)
            spawned_panes.append(pane_id)
        report["workload"]["panes_spawned"] = len(spawned_panes)
        report["workload"]["spawned_pane_ids"] = list(spawned_panes)

        # Wait for ft watch to discover all panes
        def all_panes_discovered() -> bool:
            nonlocal observed_pane_ids
            result = run(
                ft_cmd(binary.path, workspace, config_path, "robot", "state"),
                cwd=REPO_ROOT,
                env=env,
            )
            report["commands"]["robot_state_discovery"] = result.as_dict()
            payload = parse_json_output(result)
            observed_pane_ids = {
                int(pane["pane_id"])
                for pane in extract_panes(payload)
                if isinstance(pane, dict) and "pane_id" in pane
            }
            report["workload"]["panes_observed"] = len(
                observed_pane_ids.intersection(spawned_panes)
            )
            return all(pane_id in observed_pane_ids for pane_id in spawned_panes)

        wait_for(f"all {NUM_PANES} panes discovered", args.timeout_secs, all_panes_discovered)
        phase1_duration = time.monotonic() - phase1_start

        # Sample initial RSS
        watch_pid = watch_proc.pid
        initial_rss_kb = get_process_rss_kb(watch_pid)

        report["workload"]["discovery_duration_secs"] = round(phase1_duration, 3)

        # ---------------------------------------------------------------
        # Phase 2: Sustained load — monitor metrics, inject pattern mid-run
        # ---------------------------------------------------------------
        phase2_start = time.monotonic()
        phase2_started_epoch_ms = now_ms()
        report["workload"]["sustained_started_epoch_ms"] = phase2_started_epoch_ms
        phase2_end = phase2_start + args.duration_secs
        output_stats_at_sustained_start = collect_pane_output_stats(
            fake_state_dir,
            spawned_panes,
        )
        output_bytes_at_sustained_start = sum(
            sample["size_bytes"] for sample in output_stats_at_sustained_start
        )
        output_lines_at_sustained_start = sum(
            sample["line_count"] for sample in output_stats_at_sustained_start
        )
        report["workload"][
            "output_bytes_at_sustained_start"
        ] = output_bytes_at_sustained_start
        report["workload"][
            "output_lines_at_sustained_start"
        ] = output_lines_at_sustained_start
        sample_interval = RSS_SAMPLE_INTERVAL_SECS
        rss_samples: list[dict[str, Any]] = report["evidence"]["rss_samples"]
        pressure_samples: list[dict[str, Any]] = report["evidence"][
            "fleet_pressure_samples"
        ]
        scrollback_samples: list[dict[str, Any]] = report["evidence"][
            "scrollback_samples"
        ]
        health_commands: list[dict[str, Any]] = []
        report["commands"]["robot_health_samples"] = health_commands
        detection_pane_id = spawned_panes[DETECTION_PROBE_PANE_INDEX - 1]
        probe: ProbeInjection | None = None
        probe_injected_at_elapsed_secs: float | None = None
        matching_detection_event: dict[str, Any] | None = None
        detection_observed_after_ms: float | None = None
        robot_benchmarks_collected = False

        # Record initial sample
        if initial_rss_kb is not None:
            rss_samples.append(
                {
                    "elapsed_secs": 0.0,
                    "rss_kb": initial_rss_kb,
                    "timestamp_ms": next_sample_timestamp(None),
                }
            )

        initial_health = run(
            ft_cmd(binary.path, workspace, config_path, "robot", "health"),
            cwd=REPO_ROOT,
            env=env,
            timeout=5.0,
        )
        health_commands.append(initial_health.as_dict())
        initial_pressure = collect_fleet_pressure_sample(
            initial_health,
            None,
            None,
            0.0,
            phase2_started_epoch_ms,
        )
        if initial_pressure is not None:
            pressure_samples.append(initial_pressure)
        initial_scrollback = collect_scrollback_sample(
            initial_health,
            None,
            None,
            0.0,
            phase2_started_epoch_ms,
        )
        if initial_scrollback is not None:
            scrollback_samples.append(initial_scrollback)

        next_sample = phase2_start + sample_interval
        inject_at = phase2_start + (
            args.duration_secs * DETECTION_PROBE_PHASE_FRACTION
        )
        high_output_phase_at = phase2_start + (
            args.duration_secs * HIGH_OUTPUT_PHASE_FRACTION
        )
        robot_benchmark_at = phase2_start + (
            args.duration_secs * ROBOT_BENCHMARK_PHASE_FRACTION
        )

        while time.monotonic() < phase2_end:
            if watch_proc.poll() is not None:
                raise RuntimeError(f"ft watch crashed during sustained load (exit {watch_proc.returncode})")

            now = time.monotonic()

            if (
                report["workload"]["high_output_phase_started"] is not True
                and now >= high_output_phase_at
            ):
                high_output_phase_marker.write_text("high-output\n", encoding="utf-8")
                report["workload"]["high_output_phase_started"] = True
                report["workload"]["high_output_phase_started_elapsed_secs"] = round(
                    now - phase2_start,
                    3,
                )

            if probe is None and now >= inject_at:
                probe = inject_pattern(fake_wezterm, detection_pane_id, env)
                probe_injected_at_elapsed_secs = round(
                    probe.injected_at_monotonic - phase2_start,
                    3,
                )
                report["commands"]["detection_probe_injection"] = probe.command.as_dict()

            if probe is not None and matching_detection_event is None:
                try:
                    result = run(
                        ft_cmd(
                            binary.path,
                            workspace,
                            config_path,
                            "robot",
                            "events",
                            "--limit",
                            "20",
                            "--pane",
                            str(detection_pane_id),
                            "--rule-id",
                            DETECTION_RULE_ID,
                            "--since",
                            str(probe.injected_at_epoch_ms),
                        ),
                        cwd=REPO_ROOT,
                        env=env,
                        timeout=2.0,
                    )
                    if result.returncode == 0:
                        payload = parse_json_output(result)
                        exact_events = [
                            event
                            for event in extract_events(payload)
                            if isinstance(event, dict)
                            and event.get("pane_id") == detection_pane_id
                            and event.get("rule_id") == DETECTION_RULE_ID
                            and isinstance(event.get("captured_at"), int)
                            and not isinstance(event.get("captured_at"), bool)
                            and event["captured_at"] >= probe.injected_at_epoch_ms
                        ]
                        if exact_events:
                            matching_detection_event = min(
                                exact_events,
                                key=lambda event: (event["captured_at"], event.get("id", 0)),
                            )
                            detection_observed_after_ms = (
                                time.monotonic() - probe.injected_at_monotonic
                            ) * 1000.0
                            report["commands"]["robot_events_detection"] = result.as_dict()
                except Exception:  # noqa: BLE001
                    pass

            if not robot_benchmarks_collected and now >= robot_benchmark_at:
                collect_robot_benchmark_evidence(
                    report,
                    ft_binary=binary.path,
                    workspace=workspace,
                    config_path=config_path,
                    env=env,
                    sustained_phase_start=phase2_start,
                )
                robot_benchmarks_collected = True
                now = time.monotonic()

            if now >= next_sample:
                elapsed = now - phase2_start
                rss_kb = get_process_rss_kb(watch_pid)
                if rss_kb is not None:
                    rss_samples.append(
                        {
                            "elapsed_secs": round(elapsed, 3),
                            "rss_kb": rss_kb,
                            "timestamp_ms": next_sample_timestamp(
                                rss_samples[-1]["timestamp_ms"] if rss_samples else None
                            ),
                        }
                    )

                # Probe health for fleet pressure tier
                try:
                    health_result = run(
                        ft_cmd(binary.path, workspace, config_path, "robot", "health"),
                        cwd=REPO_ROOT,
                        env=env,
                        timeout=5.0,
                    )
                    health_commands.append(health_result.as_dict())
                    pressure_sample = collect_fleet_pressure_sample(
                        health_result,
                        pressure_samples[-1]["timestamp_ms"] if pressure_samples else None,
                        (
                            pressure_samples[-1]["source_snapshot_timestamp_ms"]
                            if pressure_samples
                            else None
                        ),
                        elapsed,
                        phase2_started_epoch_ms,
                    )
                    if pressure_sample is not None:
                        pressure_samples.append(pressure_sample)
                    scrollback_sample = collect_scrollback_sample(
                        health_result,
                        (
                            scrollback_samples[-1]["timestamp_ms"]
                            if scrollback_samples
                            else None
                        ),
                        (
                            scrollback_samples[-1]["source_snapshot_timestamp_ms"]
                            if scrollback_samples
                            else None
                        ),
                        elapsed,
                        phase2_started_epoch_ms,
                    )
                    if scrollback_sample is not None:
                        scrollback_samples.append(scrollback_sample)
                except Exception:  # noqa: BLE001
                    pass

                next_sample = now + sample_interval

            time.sleep(min(1.0, max(0.0, phase2_end - time.monotonic())))

        report["workload"]["observed_sustained_duration_secs"] = round(
            time.monotonic() - phase2_start,
            3,
        )
        output_stats_at_sustained_end = collect_pane_output_stats(
            fake_state_dir,
            spawned_panes,
        )
        output_bytes_at_sustained_end = sum(
            sample["size_bytes"] for sample in output_stats_at_sustained_end
        )
        output_lines_at_sustained_end = sum(
            sample["line_count"] for sample in output_stats_at_sustained_end
        )
        report["workload"]["output_bytes_at_sustained_end"] = output_bytes_at_sustained_end
        report["workload"][
            "output_lines_at_sustained_end"
        ] = output_lines_at_sustained_end
        report["workload"]["observed_output_bytes"] = max(
            0,
            output_bytes_at_sustained_end - output_bytes_at_sustained_start,
        )
        report["workload"]["observed_output_lines"] = max(
            0,
            output_lines_at_sustained_end - output_lines_at_sustained_start,
        )
        report["workload"]["pane_output_deltas"] = [
            {
                "pane_index": start_sample["pane_index"],
                "pane_id": start_sample["pane_id"],
                "lines_at_start": start_sample["line_count"],
                "lines_at_end": end_sample["line_count"],
                "observed_lines": max(
                    0,
                    end_sample["line_count"] - start_sample["line_count"],
                ),
                "bytes_at_start": start_sample["size_bytes"],
                "bytes_at_end": end_sample["size_bytes"],
                "observed_bytes": max(
                    0,
                    end_sample["size_bytes"] - start_sample["size_bytes"],
                ),
            }
            for start_sample, end_sample in zip(
                output_stats_at_sustained_start,
                output_stats_at_sustained_end,
                strict=True,
            )
        ]
        if probe is not None:
            report["evidence"]["detection_probe"] = {
                "pane_id": probe.pane_id,
                "rule_id": probe.rule_id,
                "injected_at_epoch_ms": probe.injected_at_epoch_ms,
                "injected_at_elapsed_secs": probe_injected_at_elapsed_secs,
                "observed_after_ms": detection_observed_after_ms,
                "matching_events": [matching_detection_event]
                if matching_detection_event is not None
                else [],
            }

    except SkipScenario as exc:
        report["skip_reason"] = str(exc)
    except Exception as exc:  # noqa: BLE001
        report["error"] = str(exc)
        if binary and binary.path and workspace and config_path and env:
            capture_diagnostics(
                report,
                ft_binary=binary.path,
                workspace=workspace,
                config_path=config_path,
                env=env,
            )
    finally:
        process_alive = watch_proc is not None and watch_proc.poll() is None
        watch_exit_code = watch_proc.poll() if watch_proc is not None else None

        if process_alive and watch_proc is not None:
            final_rss_kb = get_process_rss_kb(watch_proc.pid)
            rss_samples = report["evidence"].get("rss_samples", [])
            if final_rss_kb is not None and isinstance(rss_samples, list):
                prior_timestamp = (
                    rss_samples[-1].get("timestamp_ms")
                    if rss_samples and isinstance(rss_samples[-1], dict)
                    else None
                )
                observed_duration = report["workload"].get(
                    "observed_sustained_duration_secs"
                )
                rss_samples.append(
                    {
                        "elapsed_secs": observed_duration
                        if is_finite_number(observed_duration)
                        else round(time.monotonic() - run_started, 3),
                        "rss_kb": final_rss_kb,
                        "timestamp_ms": next_sample_timestamp(prior_timestamp),
                    }
                )

        runtime_log = scan_runtime_log(watch_log_path)
        db_check = (
            sqlite_quick_check(workspace / ".ft" / "ft.db")
            if workspace is not None
            else "database_missing"
        )
        report["evidence"]["runtime"] = {
            "process_survived": process_alive,
            "watch_exit_code": watch_exit_code,
            "watch_log_size_bytes": runtime_log["watch_log_size_bytes"],
            "watch_log_scan_complete": runtime_log["watch_log_scan_complete"],
            "sqlite_quick_check": db_check,
            "crash_signals": runtime_log["crash_signals"],
            "data_corruption_signals": [] if db_check == "ok" else [db_check],
        }
        report["elapsed_total_ms"] = int((time.monotonic() - run_started) * 1000)

        cleanup_errors: list[str] = []
        owned_panes_closed = True
        if fake_wezterm and env:
            for pane_id in reversed(spawned_panes):
                try:
                    close_result = close_pane(fake_wezterm, pane_id, env)
                except OSError as exc:
                    owned_panes_closed = False
                    cleanup_errors.append(
                        f"pane {pane_id} close raised {type(exc).__name__}"
                    )
                else:
                    if close_result.returncode != 0:
                        owned_panes_closed = False
                        cleanup_errors.append(
                            f"pane {pane_id} close exited {close_result.returncode}"
                        )

        try:
            stop_process(watch_proc)
        except (OSError, subprocess.TimeoutExpired) as exc:
            cleanup_errors.append(f"watch stop raised {type(exc).__name__}")
        owned_watch_stopped = watch_proc is None or watch_proc.poll() is not None
        if not owned_watch_stopped:
            cleanup_errors.append("owned watch process remained alive after cleanup")

        workspace_disposition_complete = True
        if workspace is not None:
            report.setdefault("workspace", {})
            if isinstance(report["workspace"], dict):
                report["workspace"]["kept"] = bool(args.keep_workspace)
            if args.keep_workspace:
                workspace_disposition_complete = workspace.exists()
                if not workspace_disposition_complete:
                    cleanup_errors.append("requested retained workspace is missing")
            else:
                try:
                    shutil.rmtree(workspace)
                except OSError as exc:
                    workspace_disposition_complete = False
                    cleanup_errors.append(
                        f"temporary workspace cleanup raised {type(exc).__name__}"
                    )
                else:
                    workspace_disposition_complete = not workspace.exists()
                    if not workspace_disposition_complete:
                        cleanup_errors.append("temporary workspace remained after cleanup")

        report["cleanup"] = {
            "owned_panes_closed": owned_panes_closed,
            "owned_watch_stopped": owned_watch_stopped,
            "workspace_disposition_complete": workspace_disposition_complete,
            "workspace_kept": bool(args.keep_workspace and workspace is not None),
            "errors": cleanup_errors[:100],
        }
        report["generated_at_utc"] = utc_now()
        verification = verify_stress_report(report)
        report["verification"] = verification
        report["status"] = verification["status"]
        if verification["status"] == "passed":
            requested_exit_code = 0
        elif verification["status"] == "skipped_not_proven":
            requested_exit_code = SKIP_EXIT_CODE
        else:
            requested_exit_code = 1

        try:
            persist_report(report_path, report)
        except (OSError, ValueError) as exc:
            print(f"failed to persist bounded stress report: {exc}", file=sys.stderr)
            requested_exit_code = 1
        else:
            print(repo_relative(report_path))

    return requested_exit_code


if __name__ == "__main__":
    sys.exit(main())
