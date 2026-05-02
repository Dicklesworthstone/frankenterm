#!/usr/bin/env python3
"""Side-by-side rusqlite vs frankensqlite Criterion-results aggregator.

br-ft-giisk wired-pass — scope item 3 of the bench bead. Walks the
target/criterion/<group>/<backend>/new/estimates.json artifacts
Criterion produces, computes a per-group ranking by mean execution
time, and emits docs/perf/storage-backend-comparison.json with a
stable shape the burn-down dashboards consume.

The script does NOT run benchmarks — it only aggregates the JSON
Criterion already wrote. CI runs the bench separately
(``cargo bench --bench storage_backend_comparison``) and then this
script lifts the result.

## Output schema (storage-backend-comparison.json)

::

    {
        "bead": "ft-giisk",
        "schema_version": 1,
        "generated_at_utc": "<iso8601>",
        "criterion_root": "<path>",
        "groups": {
            "<bench_group>": {
                "backends": {
                    "<backend_label>": {
                        "mean_ns": <float>,
                        "stddev_ns": <float | null>,
                        "median_ns": <float | null>,
                        "samples": <int | null>
                    },
                    ...
                },
                "ranking": [
                    {"backend": "<label>",
                     "rank": 1,
                     "mean_ns": <float>,
                     "pct_of_fastest": 100.0},
                    ...
                ]
            },
            ...
        },
        "all_passed": <bool>,
        "notes": "<string | null>"
    }

The ``pct_of_fastest`` field is the relative cost vs the fastest
backend in the group: 100 means equal to fastest, 200 means 2x as
slow. ``rank`` is 1-indexed, smaller = faster.

## Usage

::

    # Parse + emit the artifact (for the canonical CI lane):
    python3 scripts/storage_backend_compare.py

    # Verify a previously-emitted artifact has not drifted from the
    # current Criterion outputs (for a PR check):
    python3 scripts/storage_backend_compare.py --check

    # Use a different criterion root or output path:
    python3 scripts/storage_backend_compare.py \
        --criterion-root ./target/criterion \
        --out docs/perf/storage-backend-comparison.json

Exit codes: 0 = success, 1 = parse/IO error, 2 = drift on --check.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import sys
from pathlib import Path


SCHEMA_VERSION = 1
DEFAULT_CRITERION_ROOT = Path("target/criterion")
DEFAULT_OUTPUT_PATH = Path("docs/perf/storage-backend-comparison.json")
EXPECTED_GROUPS = (
    "single_writer_throughput",
    "concurrent_writer_throughput",
    "checkpoint_latency",
)


def _parse_estimate_file(path: Path) -> dict[str, float | int | None]:
    """Pull the fields we care about out of one Criterion estimates.json."""
    raw = json.loads(path.read_text(encoding="utf-8"))
    # Criterion's estimates.json shape (as of criterion 0.5+):
    #   { "mean": {"point_estimate": ns, "standard_error": ns, "confidence_interval": {...}},
    #     "median": {"point_estimate": ns, ...}, ...}
    mean = raw.get("mean", {}) or {}
    median = raw.get("median", {}) or {}
    stddev = raw.get("std_dev", {}) or {}
    return {
        "mean_ns": float(mean.get("point_estimate")) if mean.get("point_estimate") is not None else None,
        "median_ns": float(median.get("point_estimate")) if median.get("point_estimate") is not None else None,
        "stddev_ns": float(stddev.get("point_estimate")) if stddev.get("point_estimate") is not None else None,
        # Criterion writes sample_size into a sibling sample.json; we
        # don't read it here to keep the parser scoped to one file.
        "samples": None,
    }


def _walk_group(group_dir: Path) -> dict[str, dict]:
    """Walk the per-backend subdirs of one bench group.

    Criterion lays out one directory per benchmark id under the group;
    each contains ``new/estimates.json``. We treat each direct
    subdirectory of the group as a backend label.
    """
    backends: dict[str, dict] = {}
    if not group_dir.is_dir():
        return backends
    for backend_dir in sorted(group_dir.iterdir()):
        if not backend_dir.is_dir():
            continue
        # Skip Criterion's "report" directory which is not a benchmark.
        if backend_dir.name == "report":
            continue
        estimate_path = backend_dir / "new" / "estimates.json"
        if not estimate_path.exists():
            continue
        try:
            backends[backend_dir.name] = _parse_estimate_file(estimate_path)
        except (OSError, json.JSONDecodeError) as e:
            print(
                f"warn: failed to parse {estimate_path}: {e}",
                file=sys.stderr,
            )
    return backends


def _rank(backends: dict[str, dict]) -> list[dict]:
    """Compute the ranking + pct_of_fastest list for one group."""
    items = [
        (label, payload)
        for label, payload in backends.items()
        if payload.get("mean_ns") is not None
    ]
    items.sort(key=lambda kv: kv[1]["mean_ns"])
    if not items:
        return []
    fastest = items[0][1]["mean_ns"]
    ranked: list[dict] = []
    for idx, (label, payload) in enumerate(items, start=1):
        mean = payload["mean_ns"]
        ranked.append(
            {
                "backend": label,
                "rank": idx,
                "mean_ns": mean,
                "pct_of_fastest": (mean / fastest * 100.0) if fastest else 0.0,
            }
        )
    return ranked


def build_artifact(criterion_root: Path) -> dict:
    """Walk the Criterion outputs + assemble the typed artifact."""
    groups: dict[str, dict] = {}
    if criterion_root.is_dir():
        for group_dir in sorted(criterion_root.iterdir()):
            if not group_dir.is_dir():
                continue
            backends = _walk_group(group_dir)
            if not backends:
                continue
            groups[group_dir.name] = {
                "backends": backends,
                "ranking": _rank(backends),
            }
    notes: str | None = None
    missing = [g for g in EXPECTED_GROUPS if g not in groups]
    if missing:
        notes = (
            "missing expected groups: "
            + ", ".join(sorted(missing))
            + " — run cargo bench --bench storage_backend_comparison first"
        )
    return {
        "bead": "ft-giisk",
        "schema_version": SCHEMA_VERSION,
        "generated_at_utc": _dt.datetime.now(_dt.UTC).replace(microsecond=0).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "criterion_root": str(criterion_root),
        "groups": groups,
        "all_passed": not missing and all(
            len(g.get("ranking", [])) >= 1 for g in groups.values()
        ),
        "notes": notes,
    }


def _canonical_dump(payload: dict) -> str:
    """Produce a stable, sorted-keys JSON string with trailing newline."""
    return json.dumps(payload, indent=2, sort_keys=True) + "\n"


def _strip_volatile(payload: dict) -> dict:
    """Drop fields that change every run so --check stays meaningful."""
    stripped = dict(payload)
    stripped.pop("generated_at_utc", None)
    return stripped


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        prog="storage_backend_compare",
        description=__doc__.split("\n\n")[0] if __doc__ else None,
    )
    parser.add_argument(
        "--criterion-root",
        type=Path,
        default=DEFAULT_CRITERION_ROOT,
        help="Where Criterion writes its per-group artifacts.",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=DEFAULT_OUTPUT_PATH,
        help="Output JSON path.",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help=(
            "Verify the on-disk artifact matches the current Criterion "
            "outputs (volatile fields like generated_at_utc are ignored). "
            "Exit code 2 on drift."
        ),
    )
    args = parser.parse_args(argv)

    artifact = build_artifact(args.criterion_root)

    if args.check:
        if not args.out.exists():
            print(f"--check: {args.out} does not exist", file=sys.stderr)
            return 2
        existing = json.loads(args.out.read_text(encoding="utf-8"))
        if _strip_volatile(existing) != _strip_volatile(artifact):
            print(
                f"--check: {args.out} is stale vs the current Criterion outputs",
                file=sys.stderr,
            )
            return 2
        print(f"{args.out}: in sync with Criterion outputs")
        return 0

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(_canonical_dump(artifact), encoding="utf-8")
    print(
        f"wrote {args.out} ({len(artifact['groups'])} group(s))",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
