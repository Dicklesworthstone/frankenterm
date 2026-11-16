#!/usr/bin/env python3
"""Per-PR diff helper for the storage.rs callsite migration plan.

br-ft-l1jgo companion to scripts/storage_backend_callsites.py.
Shows what changed in the migration plan between two git refs +
emits an operator-friendly summary so PR reviewers can baseline
"this PR migrated N callsites of pattern X" without running the
analyzer twice and diffing manually.

## Usage

::

    # Compare HEAD against the previous commit:
    python3 scripts/storage_backend_callsites_diff.py

    # Compare against a specific base ref:
    python3 scripts/storage_backend_callsites_diff.py --base origin/main

    # JSON output for CI consumption:
    python3 scripts/storage_backend_callsites_diff.py --json

## What it prints

Per-pattern change rows (occurrences before → after with delta),
sorted by absolute delta descending so the dominant migration in
the PR is visible first. Plus an aggregate footer:

    storage.rs callsite migration delta:
      conn_execute             77 →  62  (-15)   migrated to execute_typed
      row_get_index           501 → 480  (-21)
      conn_query_row_scalar    49 →  44   (-5)
      ─────────────────────────────────────────
      total                   768 → 727  (-41)
      missing substrate: conn_prepare, conn_prepare_cached (no change)

Exit codes:
    0  — diff computed.
    1  — argument or git error.
    2  — no diff (working tree clean vs base).
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
ANALYZER = REPO_ROOT / "scripts" / "storage_backend_callsites.py"
SOURCE = "crates/frankenterm-core/src/storage.rs"


def _run(cmd: list[str], cwd: Path | None = None) -> str:
    return subprocess.check_output(cmd, cwd=cwd, text=True)


def _checkout_source_at(ref: str) -> str:
    """Return the contents of `SOURCE` at the given git ref, or the
    empty string when the file did not exist there."""
    try:
        return subprocess.check_output(
            ["git", "show", f"{ref}:{SOURCE}"],
            cwd=REPO_ROOT,
            text=True,
            stderr=subprocess.DEVNULL,
        )
    except subprocess.CalledProcessError:
        return ""


def _analyze_text(source_text: str) -> dict:
    """Run the analyzer against `source_text` by writing it to a
    temp file and pointing --source at it."""
    with tempfile.TemporaryDirectory() as td:
        # The analyzer expects a path; write the snapshot under a
        # path matching the canonical location so the plan's
        # 'source' field stays comparable.
        tmp_source = Path(td) / "storage.rs"
        tmp_source.write_text(source_text or "// empty\n")
        out = _run(
            [
                sys.executable,
                str(ANALYZER),
                "--source",
                str(tmp_source),
                "--json-only",
            ]
        )
        return json.loads(out)


def diff_plans(base: dict, head: dict) -> dict:
    """Compute per-pattern + aggregate deltas between two analyzer
    plan outputs."""
    base_patterns = {p["name"]: p for p in base.get("patterns", [])}
    head_patterns = {p["name"]: p for p in head.get("patterns", [])}
    names = sorted(set(base_patterns) | set(head_patterns))
    rows = []
    for name in names:
        b = base_patterns.get(name, {})
        h = head_patterns.get(name, {})
        b_count = int(b.get("occurrences", 0))
        h_count = int(h.get("occurrences", 0))
        delta = h_count - b_count
        if b_count == 0 and h_count == 0:
            continue
        rows.append(
            {
                "name": name,
                "before": b_count,
                "after": h_count,
                "delta": delta,
                "replacement": (b.get("replacement") or h.get("replacement")),
                "replacement_module": (
                    b.get("replacement_module") or h.get("replacement_module")
                ),
            }
        )
    rows.sort(key=lambda r: (-abs(r["delta"]), r["name"]))
    base_total = sum(int(p.get("occurrences", 0)) for p in base.get("patterns", []))
    head_total = sum(int(p.get("occurrences", 0)) for p in head.get("patterns", []))
    return {
        "bead": "ft-l1jgo",
        "schema_version": 1,
        "base_total_callsites": base_total,
        "head_total_callsites": head_total,
        "total_delta": head_total - base_total,
        "patterns": rows,
        "missing_substrate_base": base.get("missing_substrate", []),
        "missing_substrate_head": head.get("missing_substrate", []),
    }


def _print_human(diff: dict, stream) -> None:
    print("storage.rs callsite migration delta:", file=stream)
    if not diff["patterns"]:
        print("  (no callsite differences across the supplied refs)", file=stream)
    else:
        for row in diff["patterns"]:
            sign = "+" if row["delta"] > 0 else ""
            replacement = ""
            if row["delta"] != 0 and row["replacement"]:
                replacement = (
                    f"   migrated to {row['replacement_module']}::{row['replacement']}"
                    if row["delta"] < 0
                    else f"   added {row['replacement_module']}::{row['replacement']} (regression?)"
                )
            print(
                f"  {row['name']:<26} {row['before']:>5} → {row['after']:<5}  "
                f"({sign}{row['delta']}){replacement}",
                file=stream,
            )
    print("  " + "─" * 60, file=stream)
    sign = "+" if diff["total_delta"] > 0 else ""
    print(
        f"  total                      "
        f"{diff['base_total_callsites']:>5} → {diff['head_total_callsites']:<5} "
        f"({sign}{diff['total_delta']})",
        file=stream,
    )
    base_missing = set(diff.get("missing_substrate_base", []))
    head_missing = set(diff.get("missing_substrate_head", []))
    if base_missing != head_missing:
        added = sorted(head_missing - base_missing)
        removed = sorted(base_missing - head_missing)
        if added:
            print(f"  missing substrate added: {', '.join(added)}", file=stream)
        if removed:
            print(f"  missing substrate cleared: {', '.join(removed)}", file=stream)
    elif head_missing:
        print(
            f"  missing substrate (unchanged): {', '.join(sorted(head_missing))}",
            file=stream,
        )


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="storage_backend_callsites_diff")
    parser.add_argument(
        "--base",
        default="HEAD~1",
        help="Git ref to compare against (default: HEAD~1).",
    )
    parser.add_argument(
        "--head",
        default="HEAD",
        help="Git ref representing the new state (default: HEAD).",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit JSON to stdout instead of human prose.",
    )
    args = parser.parse_args(argv)

    try:
        base_text = _checkout_source_at(args.base)
        head_text = _checkout_source_at(args.head)
    except subprocess.CalledProcessError as e:
        print(f"git error: {e}", file=sys.stderr)
        return 1

    base_plan = _analyze_text(base_text)
    head_plan = _analyze_text(head_text)
    diff = diff_plans(base_plan, head_plan)

    if args.json:
        print(json.dumps(diff, indent=2, sort_keys=True))
    else:
        _print_human(diff, sys.stdout)

    nonzero_delta = any(row["delta"] != 0 for row in diff["patterns"])
    if not nonzero_delta and diff["total_delta"] == 0:
        # Identical pattern counts on both sides — no migration in
        # the supplied refs. Useful for PR-time gating that wants
        # to assert "this PR migrated something".
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
