#!/usr/bin/env python3
"""storage.rs callsite migration analyzer (br-ft-l1jgo aid).

Scans crates/frankenterm-core/src/storage.rs for direct
``rusqlite::Connection`` usage patterns that the wired-pass call-
site migration must convert onto ``StorageBackend``. Emits a
prioritized migration plan as JSON + a human-readable summary so
the migration can be sequenced one pattern cluster at a time
(per the bead's "per-module migration cadence: one storage
submodule at a time" guidance).

The analyzer is read-only — it does NOT touch storage.rs. It
accelerates the migration by:

1. Counting each known raw-rusqlite pattern in storage.rs.
2. Mapping each pattern to its substrate replacement (helpers from
   storage_backend_helpers / storage_backend_row_helpers /
   storage_backend_cells).
3. Sorting patterns by (replacement_helper_present, count desc) so
   the operator works through the high-frequency, well-supported
   sites first.
4. Surfacing patterns that have *no* substrate yet — those flag a
   missing helper that ft-qgj81 wired-pass must add before the
   call-site migration can complete.

## Usage

::

    python3 scripts/storage_backend_callsites.py
        # — emit docs/storage/callsite-migration-plan.json + a
        # short summary on stderr.

    python3 scripts/storage_backend_callsites.py --check
        # — exit 2 if the plan on disk has drifted from the
        # current storage.rs pattern set (volatile fields like
        # generated_at_utc are stripped before the comparison).

    python3 scripts/storage_backend_callsites.py --json-only
        # — write the JSON to stdout instead of disk.

## Output schema (callsite-migration-plan.json, schema_version: 1)

::

    {
        "bead": "ft-l1jgo",
        "schema_version": 1,
        "generated_at_utc": "<iso8601>",
        "source": "crates/frankenterm-core/src/storage.rs",
        "source_line_count": <int>,
        "total_callsites": <int>,
        "patterns": [
            {
                "name": "<pattern_id>",
                "regex": "<regex>",
                "occurrences": <int>,
                "replacement": "<substrate_helper>" | null,
                "replacement_module": "<module>" | null,
                "notes": "<hint>"
            },
            ...
        ],
        "migration_priority": [<pattern_name>, ...],
        "missing_substrate": [<pattern_name>, ...]
    }
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import re
import sys
from pathlib import Path

SCHEMA_VERSION = 1
DEFAULT_SOURCE = Path("crates/frankenterm-core/src/storage.rs")
DEFAULT_OUTPUT = Path("docs/storage/callsite-migration-plan.json")

# Pattern catalogue. Each entry is (name, regex, replacement,
# module, notes). `replacement` is None when the substrate does
# not yet expose a typed equivalent.
PATTERNS: list[tuple[str, str, str | None, str | None, str]] = [
    (
        "conn_query_row_scalar",
        r"\.query_row\s*\(",
        "query_row_typed",
        "storage_backend_trait",
        "Single-row read with typed params. Use the trait's ToSqlValue path.",
    ),
    (
        "conn_query_map",
        r"\.query_map\s*\(",
        "query_map_typed",
        "storage_backend_trait",
        "Multi-row read returning Vec<Vec<String>>; typed cells via storage_backend_cells.",
    ),
    (
        "conn_execute_batch",
        r"\bconn(?:ection)?\.execute_batch\s*\(",
        "execute_batch",
        "storage_backend_trait",
        "Raw Connection DDL / multi-statement execution. Trait method already covers this.",
    ),
    (
        "conn_execute",
        r"\bconn(?:ection)?\.execute\s*\(",
        "execute_typed",
        "storage_backend_helpers",
        "Single-statement execute with typed params. Helper wraps the trait.",
    ),
    (
        "conn_prepare_cached",
        r"\.prepare_cached\s*\(",
        "execute_many",
        "storage_backend_trait",
        "Bulk-execute pattern (prepare_cached + loop execute). Migrate to StorageBackend::execute_many (ft-qgj81 slice 5).",
    ),
    (
        "conn_prepare",
        r"\.prepare\s*\(",
        "query_row_typed",
        "storage_backend_trait",
        "prepare followed by query_row/query_map: migrate to query_row_typed/query_map_typed. prepare followed by loop execute: migrate to execute_many. See the prepare/prepare_cached recipe in the migration guide.",
    ),
    (
        "conn_pragma_query",
        r"\bpragma_query(?:_value)?\s*\(",
        "pragma_value",
        "storage_backend_helpers",
        "Single PRAGMA reads — helper wraps query_row_typed with the PRAGMA-name guard.",
    ),
    (
        "conn_transaction",
        r"\bconn(?:ection)?\.transaction\s*\(",
        None,
        None,
        "Transaction lifecycle is on the trait but the call-site borrowing pattern (TX as type) needs a follow-on closure-based helper.",
    ),
    (
        "rusqlite_params_macro",
        r"\brusqlite::params!\s*\[",
        "ToSqlValue",
        "storage_backend_trait",
        "Parameter binding via ToSqlValue — convert each param at call time.",
    ),
    (
        "params_from_iter",
        r"\bparams_from_iter\s*\(",
        "ToSqlValue",
        "storage_backend_trait",
        "Same conversion target as rusqlite::params!.",
    ),
    (
        "rusqlite_optional",
        r"\.optional\s*\(\s*\)",
        "query_row_typed",
        "storage_backend_trait",
        "rusqlite's ::optional() chains — replace with the trait's Option<...> return.",
    ),
    (
        "row_get_typed",
        r"row\.get(?:_unwrap)?::\s*<",
        "RowReader",
        "storage_backend_row_helpers",
        "Per-column typed reads inside a query_map closure — RowReader bundles them.",
    ),
    (
        "row_get_index",
        r"row\.get(?:_unwrap)?\s*\(\s*\d+",
        "RowReader",
        "storage_backend_row_helpers",
        "Indexed row reads — RowReader's i64/text/blob accessors mirror this.",
    ),
    (
        "rusqlite_connection_open",
        r"\brusqlite::Connection::open",
        "RusqliteBackend::open",
        "storage_backend_trait",
        "Direct Connection::open construction — replace with RusqliteBackend::open.",
    ),
    (
        "to_sql_call",
        r"\.to_sql\s*\(\s*\)",
        "ToSqlValue::from",
        "storage_backend_trait",
        "Manual rusqlite::ToSql conversions — ToSqlValue's From impls cover the scalar cases.",
    ),
]


def strip_full_line_rust_comments(source: str) -> str:
    """Drop full-line Rust comments so examples do not count as callsites."""
    return "\n".join(
        line for line in source.splitlines() if not line.lstrip().startswith("//")
    )


def count_pattern(source: str, regex: str) -> int:
    return len(re.findall(regex, source))


def build_plan(source_path: Path) -> dict:
    if not source_path.exists():
        raise FileNotFoundError(f"source file not found: {source_path}")
    text = source_path.read_text(encoding="utf-8")
    line_count = text.count("\n") + 1
    scan_text = strip_full_line_rust_comments(text)

    pattern_rows: list[dict] = []
    for name, regex, replacement, module, notes in PATTERNS:
        pattern_rows.append(
            {
                "name": name,
                "regex": regex,
                "occurrences": count_pattern(scan_text, regex),
                "replacement": replacement,
                "replacement_module": module,
                "notes": notes,
            }
        )

    total = sum(p["occurrences"] for p in pattern_rows)

    # Migration priority: rows with a replacement, ordered by
    # descending occurrence count, ties broken by name for
    # determinism.
    actionable = sorted(
        (p for p in pattern_rows if p["replacement"] is not None),
        key=lambda p: (-p["occurrences"], p["name"]),
    )
    priority = [p["name"] for p in actionable if p["occurrences"] > 0]

    missing_substrate = sorted(
        p["name"]
        for p in pattern_rows
        if p["replacement"] is None and p["occurrences"] > 0
    )

    return {
        "bead": "ft-l1jgo",
        "schema_version": SCHEMA_VERSION,
        "generated_at_utc": _dt.datetime.now(_dt.UTC).replace(microsecond=0).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "source": str(source_path),
        "source_line_count": line_count,
        "total_callsites": total,
        "patterns": pattern_rows,
        "migration_priority": priority,
        "missing_substrate": missing_substrate,
    }


def _strip_volatile(payload: dict) -> dict:
    stripped = dict(payload)
    stripped.pop("generated_at_utc", None)
    return stripped


def _canonical_dump(payload: dict) -> str:
    return json.dumps(payload, indent=2, sort_keys=True) + "\n"


def _print_summary(plan: dict, stream) -> None:
    print(
        f"storage.rs: {plan['source_line_count']} lines, "
        f"{plan['total_callsites']} callsite hits across "
        f"{len([p for p in plan['patterns'] if p['occurrences'] > 0])} pattern(s)",
        file=stream,
    )
    for name in plan["migration_priority"][:5]:
        match = next(p for p in plan["patterns"] if p["name"] == name)
        print(
            f"  - {name}: {match['occurrences']} hits → "
            f"{match['replacement_module']}::{match['replacement']}",
            file=stream,
        )
    if plan["missing_substrate"]:
        print(
            f"missing substrate: {', '.join(plan['missing_substrate'])}",
            file=stream,
        )


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="storage_backend_callsites")
    parser.add_argument("--source", type=Path, default=DEFAULT_SOURCE)
    parser.add_argument("--out", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--check", action="store_true")
    parser.add_argument(
        "--json-only",
        action="store_true",
        help="Emit JSON to stdout instead of writing to --out.",
    )
    args = parser.parse_args(argv)

    plan = build_plan(args.source)

    if args.json_only:
        sys.stdout.write(_canonical_dump(plan))
        return 0

    if args.check:
        if not args.out.exists():
            print(f"--check: {args.out} does not exist", file=sys.stderr)
            return 2
        existing = json.loads(args.out.read_text(encoding="utf-8"))
        if _strip_volatile(existing) != _strip_volatile(plan):
            print(
                f"--check: {args.out} is stale vs current storage.rs callsites",
                file=sys.stderr,
            )
            return 2
        print(f"{args.out}: in sync", file=sys.stderr)
        return 0

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(_canonical_dump(plan), encoding="utf-8")
    _print_summary(plan, sys.stderr)
    print(f"wrote {args.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
