#!/usr/bin/env python3
"""Aggregate competitor resize-bench JSON and update regression state.

This is the integration half of ft-t101b.  It deliberately mirrors the
pure policy shipped in `frankenterm_core::competitor_delta`: ft is compared
against WezTerm, Ghostty, and Rio on six metrics; a delta <= -10% is a
regression; two consecutive regressed releases emit one P1 filing command.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import math
import pathlib
import subprocess
import sys
from dataclasses import dataclass
from typing import Any


COMPETITORS = ("ft", "wezterm", "ghostty", "rio")
REFERENCE_COMPETITORS = ("wezterm", "ghostty", "rio")
METRICS = (
    "fps_p50",
    "fps_p95",
    "fps_p99",
    "frame_time_p95_ms",
    "gpu_memory_peak_mb",
    "cpu_peak_pct",
)
LOWER_IS_BETTER = {"frame_time_p95_ms", "gpu_memory_peak_mb", "cpu_peak_pct"}
MAX_REGRESSION_PCT = -10.0
SNAPSHOT_SCHEMA = "ft.competitor.resize.snapshot.v1"
STATE_SCHEMA = "ft.competitor.resize.regression_state.v1"


@dataclass(frozen=True)
class RawBench:
    competitor: str
    release_version: str
    hardware_baseline: str
    runner_sku: str
    workload: dict[str, Any]
    metrics: dict[str, float]
    source_path: str


def utc_now() -> str:
    return _dt.datetime.now(_dt.UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input-dir", type=pathlib.Path, required=True)
    parser.add_argument("--release-version", required=True)
    parser.add_argument("--hardware-baseline", required=True)
    parser.add_argument("--runner-sku", default="")
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--state-file", type=pathlib.Path)
    parser.add_argument("--file-p1", action="store_true")
    parser.add_argument("--br-bin", default="br")
    return parser.parse_args()


def load_raw_file(path: pathlib.Path) -> RawBench:
    data = json.loads(path.read_text())
    competitor = str(data.get("competitor", ""))
    if competitor not in COMPETITORS:
        raise ValueError(f"{path}: unsupported competitor {competitor!r}")

    metrics_raw = data.get("metrics")
    if not isinstance(metrics_raw, dict):
        raise ValueError(f"{path}: missing metrics object")
    metrics: dict[str, float] = {}
    for metric in METRICS:
        raw = metrics_raw.get(metric)
        if not isinstance(raw, (int, float)) or not math.isfinite(float(raw)):
            raise ValueError(f"{path}: metric {metric!r} must be finite number")
        metrics[metric] = float(raw)

    workload = data.get("workload")
    if not isinstance(workload, dict):
        workload = {}

    return RawBench(
        competitor=competitor,
        release_version=str(data.get("release_version", "")),
        hardware_baseline=str(data.get("hardware_baseline", "")),
        runner_sku=str(data.get("runner_sku", "")),
        workload=workload,
        metrics=metrics,
        source_path=str(path),
    )


def load_raw_benches(input_dir: pathlib.Path) -> dict[str, RawBench]:
    benches: dict[str, RawBench] = {}
    for path in sorted(input_dir.glob("*.json")):
        bench = load_raw_file(path)
        if bench.competitor in benches:
            raise ValueError(f"duplicate raw bench for {bench.competitor}")
        benches[bench.competitor] = bench
    missing = [name for name in COMPETITORS if name not in benches]
    if missing:
        raise ValueError(f"missing raw competitor JSON for: {', '.join(missing)}")
    return benches


def delta_pct(metric: str, ft_value: float, competitor_value: float) -> float | None:
    if competitor_value == 0.0:
        return None
    if metric in LOWER_IS_BETTER:
        return (competitor_value - ft_value) / competitor_value * 100.0
    return (ft_value - competitor_value) / competitor_value * 100.0


def regression_class(delta: float | None) -> str:
    if delta is None:
        return "clean"
    return "regressed" if delta <= MAX_REGRESSION_PCT else "clean"


def observe(previous: str, klass: str) -> tuple[str, str]:
    if previous == "clean" and klass == "clean":
        return "clean", "no_change"
    if previous == "clean" and klass == "regressed":
        return "single_regression", "entered_single_regression"
    if previous == "single_regression" and klass == "clean":
        return "clean", "recovered"
    if previous == "single_regression" and klass == "regressed":
        return "consecutive_regression", "entered_consecutive"
    if previous == "consecutive_regression" and klass == "clean":
        return "clean", "recovered"
    if previous == "consecutive_regression" and klass == "regressed":
        return "consecutive_regression", "no_change"
    raise ValueError(f"unsupported previous state {previous!r}")


def load_state(path: pathlib.Path | None) -> dict[str, str]:
    if path is None or not path.exists():
        return {}
    state: dict[str, str] = {}
    for line in path.read_text().splitlines():
        if not line.strip():
            continue
        row = json.loads(line)
        key = str(row.get("key", ""))
        value = str(row.get("state", "clean"))
        if key:
            state[key] = value
    return state


def append_state(path: pathlib.Path | None, rows: list[dict[str, Any]]) -> None:
    if path is None:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        for row in rows:
            handle.write(json.dumps(row, sort_keys=True, separators=(",", ":")))
            handle.write("\n")


def build_snapshot(args: argparse.Namespace, benches: dict[str, RawBench]) -> dict[str, Any]:
    ft = benches["ft"]
    samples = []
    deltas = []
    previous_state = load_state(args.state_file)
    state_rows = []
    p1_regressions = []
    generated_at = utc_now()

    for competitor in COMPETITORS:
        for metric in METRICS:
            samples.append(
                {
                    "competitor": competitor,
                    "metric": metric,
                    "value": benches[competitor].metrics[metric],
                }
            )

    for competitor in REFERENCE_COMPETITORS:
        for metric in METRICS:
            ft_value = ft.metrics[metric]
            other_value = benches[competitor].metrics[metric]
            delta = delta_pct(metric, ft_value, other_value)
            klass = regression_class(delta)
            key = f"{args.hardware_baseline}:{competitor}:{metric}"
            previous = previous_state.get(key, "clean")
            state, transition = observe(previous, klass)
            row = {
                "schema_version": STATE_SCHEMA,
                "key": key,
                "release_version": args.release_version,
                "hardware_baseline": args.hardware_baseline,
                "competitor": competitor,
                "metric": metric,
                "class": klass,
                "previous_state": previous,
                "state": state,
                "transition": transition,
                "delta_pct": delta,
                "generated_at": generated_at,
            }
            state_rows.append(row)
            deltas.append(
                {
                    "competitor": competitor,
                    "metric": metric,
                    "ft_value": ft_value,
                    "competitor_value": other_value,
                    "delta_pct": delta,
                    "regression_class": klass,
                    "regression_state": state,
                    "transition": transition,
                }
            )
            if transition == "entered_consecutive":
                title = (
                    "[BR-TERM-EMULATOR-UPLIFT.PERF-REGRESSION] "
                    f"ft is >=10% slower than {competitor} on {metric} "
                    f"for 2 consecutive releases"
                )
                p1_regressions.append(
                    {
                        "title": title,
                        "competitor": competitor,
                        "metric": metric,
                        "delta_pct": delta,
                        "snapshot": str(args.output),
                        "br_command": [
                            args.br_bin,
                            "create",
                            "--type=bug",
                            "--priority=1",
                            f"--title={title}",
                            "--description",
                            (
                                f"Auto-filed by ft-t101b competitor bench on {args.release_version}. "
                                f"Snapshot: {args.output}. Hardware baseline: {args.hardware_baseline}. "
                                f"Delta pct: {delta}."
                            ),
                        ],
                    }
                )

    snapshot = {
        "schema_version": SNAPSHOT_SCHEMA,
        "release_version": args.release_version,
        "hardware_baseline": args.hardware_baseline,
        "runner_sku": args.runner_sku or ft.runner_sku,
        "generated_at": generated_at,
        "source_raw_files": {name: bench.source_path for name, bench in benches.items()},
        "workload": ft.workload,
        "samples": samples,
        "deltas": deltas,
        "state_events": state_rows,
        "p1_regressions": p1_regressions,
    }
    append_state(args.state_file, state_rows)
    return snapshot


def file_p1s(snapshot: dict[str, Any]) -> None:
    for regression in snapshot["p1_regressions"]:
        cmd = regression["br_command"]
        subprocess.run(cmd, check=True)


def main() -> int:
    args = parse_args()
    try:
        benches = load_raw_benches(args.input_dir)
        snapshot = build_snapshot(args, benches)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(snapshot, indent=2, sort_keys=True) + "\n")
        if args.file_p1:
            file_p1s(snapshot)
    except Exception as exc:
        print(f"competitor-bench-state: {exc}", file=sys.stderr)
        return 2
    print(
        "competitor-bench-state:",
        f"wrote={args.output}",
        f"p1_regressions={len(snapshot['p1_regressions'])}",
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
