#!/usr/bin/env python3
"""Cx-propagation burn-down dashboard generator (ft-t9a6q.2).

Reads the audit script's --json output, categorizes every
`pub async fn` site into one of 7 priority buckets per the
parent bead's call-path criticality ranking, and emits:

  docs/runtime/cx-propagation.json
      Latest snapshot. Overwritten on each run. Shape:
      {
        "schema_version": 1,
        "generated_at": "<UTC ISO-8601>",
        "totals": { "total_sites", "covered_sites",
                    "wrapper_exempt_sites", "exempt_file_sites",
                    "uncovered_sites" },
        "buckets": {
          "capture":     { "total", "covered", "wrapper_exempt",
                           "uncovered", "files": ["..."] },
          "workflow":    {...},
          "web_sse":     {...},
          "mcp":         {...},
          "distributed": {...},
          "connectors":  {...},
          "tx":          {...},
          "other":       {...}
        }
      }

  docs/runtime/cx-propagation-trend.jsonl
      Append-only weekly trend. One JSON object per line:
      { "timestamp": "...", "totals": {...}, "buckets": {...} }
      Run from CI on a weekly cron — tail-truncated reads
      give the burn-down curve.

Usage:
  scripts/cx_propagation_burndown.py            # write snapshot + append trend
  scripts/cx_propagation_burndown.py --no-trend # snapshot only (CI dry-runs)
  scripts/cx_propagation_burndown.py --check    # exit 1 if uncovered_sites > 0

Bead: ft-t9a6q.2 (BR-RC-RUNTIME-SEMANTICS.G14.2).
Doc:  docs/runtime/cx-propagation-burndown.md.
Sibling audit: scripts/check_runtime_proof_coverage.py (ft-3kv6e).
Sibling lint:  lints/cx_propagation/ (ft-t9a6q.1).
"""
from __future__ import annotations

import argparse
import datetime as dt
import json
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
AUDIT_SCRIPT = REPO_ROOT / "scripts" / "check_runtime_proof_coverage.py"
SNAPSHOT_PATH = REPO_ROOT / "docs" / "runtime" / "cx-propagation.json"
TREND_PATH = REPO_ROOT / "docs" / "runtime" / "cx-propagation-trend.jsonl"
CORE_SRC = REPO_ROOT / "crates" / "frankenterm-core" / "src"
CORE_TESTS = REPO_ROOT / "crates" / "frankenterm-core" / "tests"
SCHEMA_VERSION = 2

# Regex matching the LabRuntime fixture's call sites. Three
# variants (lab_runtime_test / _with_seed / _with_config) plus
# tolerance for both function-call and qualified-path forms.
LAB_RUNTIME_TEST_RE = re.compile(
    r"\blab_runtime_test(?:_with_seed|_with_config)?\s*\("
)


# Per the parent bead's call-path criticality ranking. Order
# matters: a file matching multiple patterns is assigned to the
# first bucket it matches, so put the most specific patterns
# (e.g. `mcp_proxy`) above the more general ones (e.g. `mcp`).
#
# Each entry: (bucket_name, list_of_path_regexes).
BUCKET_RULES: list[tuple[str, list[str]]] = [
    (
        "capture",
        [
            r"^ingest(_|\.|/)",
            r"^recorder(_|\.|/)",
            r"^scan_pipeline\.",
            r"^scrollback(_|\.|/)",
            r"^capture(_|\.|/)",
            r"^display_pipeline\.",
            r"^differential_snapshot\.",
            r"^continuous_backpressure\.",
            r"^aegis_backpressure\.",
        ],
    ),
    (
        "workflow",
        [
            r"^workflows(/|\.)",
            r"^workflow_(.*)\.",
            r"^plan\.",
            r"^action_plan_",
            r"^dry_run\.",
        ],
    ),
    (
        "web_sse",
        [
            r"^web(/|\.)",
            r"^webhook",
            r"^email_notify\.",
            r"^desktop_notify\.",
        ],
    ),
    (
        "mcp",
        [
            r"^mcp(/|_|\.)",
        ],
    ),
    (
        "distributed",
        [
            r"^distributed(_|/|\.)",
            r"^fleet(_|/|\.)",
            r"^federation(_|/|\.)",
            r"^headless(_|/|\.)",
            r"^connector_mesh\.",
        ],
    ),
    (
        "connectors",
        [
            r"^connector(_|/|\.)",
        ],
    ),
    (
        "tx",
        [
            r"^tx_",
            r"^mission(_|/|\.)",
            r"^canary_rollout",
            r"^shadow_mode_",
        ],
    ),
]
ALL_BUCKETS: list[str] = [name for name, _ in BUCKET_RULES] + ["other"]


def categorize(rel_path: str) -> str:
    for bucket, patterns in BUCKET_RULES:
        for pat in patterns:
            if re.match(pat, rel_path):
                return bucket
    return "other"


def run_audit() -> dict:
    """Invoke the existing audit script's --json mode."""
    res = subprocess.run(
        [sys.executable, str(AUDIT_SCRIPT), "--json"],
        capture_output=True,
        text=True,
        check=False,
    )
    # The audit script returns exit 1 when uncovered > baseline, but
    # always emits valid JSON on stdout. We don't propagate the exit
    # code; the dashboard's own --check flag handles the gating.
    if not res.stdout.strip():
        sys.stderr.write(
            "cx_propagation_burndown: audit script produced no JSON output. "
            "stderr:\n" + res.stderr
        )
        sys.exit(2)
    return json.loads(res.stdout)


def empty_bucket() -> dict:
    return {
        "total": 0,
        "covered": 0,
        "wrapper_exempt": 0,
        "exempt_file": 0,
        "uncovered": 0,
        "files": [],
    }


def empty_labruntime_bucket() -> dict:
    return {
        "call_sites": 0,
        "files_with_calls": 0,
        "files": [],
    }


def scan_labruntime_calls() -> dict:
    """Walk crates/frankenterm-core/{src,tests}/ for lab_runtime_test*
    invocations. Returns a per-file count keyed by repo-relative path
    so the caller can categorize by bucket using the same taxonomy
    as the audit's by_file map.

    The categorize() function is keyed on the file *stem*
    (relative to crates/frankenterm-core/src/) — for src/ files
    we strip the src/ prefix; for tests/ files we keep them under
    a synthetic "tests/" prefix so they fall into the "other"
    bucket by default. That matches the audit script's behavior
    (it only walks src/) while still letting the dashboard see
    the test corpus.
    """
    counts: dict[str, int] = {}
    for root in (CORE_SRC, CORE_TESTS):
        if not root.is_dir():
            continue
        for path in root.rglob("*.rs"):
            try:
                src = path.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            n = len(LAB_RUNTIME_TEST_RE.findall(src))
            if n == 0:
                continue
            if root == CORE_SRC:
                rel = path.relative_to(CORE_SRC).as_posix()
            else:
                rel = "tests/" + path.relative_to(CORE_TESTS).as_posix()
            counts[rel] = n
    return counts


def build_labruntime_coverage(
    audit: dict, lab_calls: dict[str, int]
) -> dict:
    """Build the labruntime_coverage subkey of the snapshot.

    Three top-level fields:
      - total_call_sites: sum of all lab_runtime_test* invocations.
      - files_with_calls: count of distinct .rs files with ≥1 call.
      - tested_files_intersect_covered: distinct files that BOTH
        contain a lab_runtime_test* call AND have at least one
        covered pub async fn per the audit's by_file map.
        Coarse heuristic — proximity-based, not 1:1.
      - by_bucket: per-bucket breakdown matching the existing
        8-bucket taxonomy.
    """
    by_file: dict = audit.get("by_file", {})
    files_with_covered_async: set[str] = {
        rel for rel, stats in by_file.items() if stats.get("covered", 0) > 0
    }
    intersect = sum(1 for rel in lab_calls if rel in files_with_covered_async)

    by_bucket: dict[str, dict] = {
        name: empty_labruntime_bucket() for name in ALL_BUCKETS
    }
    for rel, n in lab_calls.items():
        bucket = categorize(rel)
        b = by_bucket[bucket]
        b["call_sites"] += n
        b["files_with_calls"] += 1
        b["files"].append(rel)
    for b in by_bucket.values():
        b["files"].sort()

    return {
        "total_call_sites": sum(lab_calls.values()),
        "files_with_calls": len(lab_calls),
        "tested_files_intersect_covered": intersect,
        "by_bucket": by_bucket,
    }


def build_snapshot(audit: dict) -> dict:
    by_file: dict = audit.get("by_file", {})
    buckets: dict[str, dict] = {name: empty_bucket() for name in ALL_BUCKETS}
    for rel_path, stats in by_file.items():
        bucket = categorize(rel_path)
        b = buckets[bucket]
        if stats.get("total", 0) == 0:
            continue
        b["total"] += stats["total"]
        b["covered"] += stats.get("covered", 0)
        b["wrapper_exempt"] += stats.get("wrapper_exempt", 0)
        b["uncovered"] += stats.get("uncovered", 0)
        if stats.get("exempt_file"):
            b["exempt_file"] += stats["total"]
        b["files"].append(rel_path)
    for bucket in buckets.values():
        bucket["files"].sort()

    totals = {
        "total_sites": audit.get("total_sites", 0),
        "covered_sites": audit.get("covered_sites", 0),
        "wrapper_exempt_sites": audit.get("wrapper_exempt_sites", 0),
        "exempt_file_sites": audit.get("exempt_files_sites", 0),
        "uncovered_sites": audit.get("uncovered_sites", 0),
    }
    lab_calls = scan_labruntime_calls()
    labruntime_coverage = build_labruntime_coverage(audit, lab_calls)

    return {
        "schema_version": SCHEMA_VERSION,
        "generated_at": dt.datetime.now(dt.timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ),
        "totals": totals,
        "buckets": buckets,
        "labruntime_coverage": labruntime_coverage,
    }


def append_trend(snapshot: dict) -> None:
    lab = snapshot.get("labruntime_coverage", {})
    trend_row = {
        "timestamp": snapshot["generated_at"],
        "totals": snapshot["totals"],
        "buckets": {
            name: {
                k: v
                for k, v in snapshot["buckets"][name].items()
                if k != "files"
            }
            for name in ALL_BUCKETS
        },
        "labruntime_coverage": {
            "total_call_sites": lab.get("total_call_sites", 0),
            "files_with_calls": lab.get("files_with_calls", 0),
            "tested_files_intersect_covered": lab.get(
                "tested_files_intersect_covered", 0
            ),
            "by_bucket": {
                name: {
                    k: v
                    for k, v in lab.get("by_bucket", {})
                    .get(name, {})
                    .items()
                    if k != "files"
                }
                for name in ALL_BUCKETS
            },
        },
    }
    TREND_PATH.parent.mkdir(parents=True, exist_ok=True)
    with TREND_PATH.open("a", encoding="utf-8") as f:
        f.write(json.dumps(trend_row, sort_keys=True) + "\n")


def write_snapshot(snapshot: dict) -> None:
    SNAPSHOT_PATH.parent.mkdir(parents=True, exist_ok=True)
    SNAPSHOT_PATH.write_text(
        json.dumps(snapshot, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--no-trend",
        action="store_true",
        help="skip appending to docs/runtime/cx-propagation-trend.jsonl",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="exit 1 if uncovered_sites > 0 (regression guard mode)",
    )
    parser.add_argument(
        "--print-summary",
        action="store_true",
        help="print human-readable summary to stdout after writing",
    )
    args = parser.parse_args()

    audit = run_audit()
    snapshot = build_snapshot(audit)
    write_snapshot(snapshot)
    if not args.no_trend:
        append_trend(snapshot)

    if args.print_summary:
        t = snapshot["totals"]
        print(
            f"cx-propagation: total={t['total_sites']} "
            f"covered={t['covered_sites']} "
            f"wrapper_exempt={t['wrapper_exempt_sites']} "
            f"exempt_file={t['exempt_file_sites']} "
            f"uncovered={t['uncovered_sites']}"
        )
        for name in ALL_BUCKETS:
            b = snapshot["buckets"][name]
            if b["total"] == 0:
                continue
            print(
                f"  {name:11s}: total={b['total']:4d} "
                f"covered={b['covered']:4d} "
                f"wrapper={b['wrapper_exempt']:4d} "
                f"uncovered={b['uncovered']:4d} "
                f"files={len(b['files'])}"
            )
        lab = snapshot.get("labruntime_coverage", {})
        print(
            f"labruntime: call_sites={lab.get('total_call_sites', 0)} "
            f"files_with_calls={lab.get('files_with_calls', 0)} "
            f"tested_files_intersect_covered="
            f"{lab.get('tested_files_intersect_covered', 0)}"
        )
        for name in ALL_BUCKETS:
            lb = lab.get("by_bucket", {}).get(name, {})
            if lb.get("call_sites", 0) == 0:
                continue
            print(
                f"  {name:11s}: call_sites={lb['call_sites']:4d} "
                f"files_with_calls={lb['files_with_calls']:4d}"
            )

    if args.check and snapshot["totals"]["uncovered_sites"] > 0:
        print(
            f"cx_propagation_burndown: --check failed: "
            f"{snapshot['totals']['uncovered_sites']} uncovered site(s)",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
