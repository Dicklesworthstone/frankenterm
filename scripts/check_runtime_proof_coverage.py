#!/usr/bin/env python3
"""ft-3kv6e — RuntimeProof coverage audit for frankenterm-core.

Walks `crates/frankenterm-core/src/` and classifies every `pub async fn`
site as **covered** or **uncovered**. A site is *covered* if its
signature contains any of:

  * `&Cx` / `&mut Cx` parameter (Cx is sealed — see `runtime_proof.rs`)
  * `impl RuntimeProof`
  * `: RuntimeProof` (generic bound)

A site is *exempt* (not counted) if it lives in one of the runtime-layer
files listed in `EXEMPT_FILES` — those modules ARE the seal and cannot
take a `RuntimeProof` bound on themselves.

The script ratchets a baseline at `tests/runtime_proof_coverage_baseline.json`.
A run fails (exit 1) if the live uncovered count exceeds the baseline;
each adoption commit is expected to lower the baseline. Update the
baseline in the same commit that lowers the count.

Usage:
  scripts/check_runtime_proof_coverage.py
  scripts/check_runtime_proof_coverage.py --update-baseline   # rewrite baseline to match current state
  scripts/check_runtime_proof_coverage.py --json              # emit machine-readable summary

Cross-references:
  ft-i2eni.1 — RuntimeProof sealed trait foundation (e990cec00)
  ft-3kv6e   — this adoption sweep
  docs/runtime/runtime-proof-trait.md — doctrine
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SRC_ROOT = REPO_ROOT / "crates" / "frankenterm-core" / "src"
BASELINE_PATH = (
    REPO_ROOT / "crates" / "frankenterm-core" / "tests" / "runtime_proof_coverage_baseline.json"
)

# Runtime-layer files that ARE the seal — they cannot take a
# RuntimeProof bound on themselves without circular dependency.
# Add sparingly; every entry is a permanent doctrine carve-out.
EXEMPT_FILES: set[str] = {
    "runtime_async.rs",       # the wrapper module — primitives are sealed elsewhere
    "runtime_proof.rs",       # defines the seal itself
    "cx.rs",                  # Cx is the canonical structured-async witness; sealed in runtime_proof.rs
    "cx_stub.rs",             # build-time stub of cx.rs (no-op shim)
}

# Functions that are ergonomic wrappers around a `_with_cx` / `_cx` sibling.
# Each entry is a (relative_file, fn_name) pair. The wrapper itself is
# considered transitively covered because it constructs a default `Cx`
# internally and immediately delegates to the covered sibling.
#
# Entries here MUST be paired with a real covered sibling — i.e. the same
# file must contain `pub async fn <name>_with_cx(cx: &Cx, ...)` or
# `pub async fn <name>_cx(cx: &Cx, ...)`. The audit cross-checks this so
# the allowlist can't drift into a silent escape hatch.
WRAPPER_EXEMPTIONS: set[tuple[str, str]] = set()  # populated by sub-beads

PUB_ASYNC_RE = re.compile(r"^\s*pub(?:\([^)]*\))? async fn\b")
COVERED_PATTERNS = [
    re.compile(r"&\s*(?:mut\s+)?Cx\b"),
    re.compile(r"\bimpl\s+RuntimeProof\b"),
    re.compile(r":\s*RuntimeProof\b"),
]
FN_NAME_RE = re.compile(r"pub(?:\([^)]*\))? async fn\s+([A-Za-z_][A-Za-z0-9_]*)")


def signature_blocks(path: Path):
    """Yield (start_line_num, fn_name, signature_text) for each pub async fn."""
    text = path.read_text(encoding="utf-8", errors="replace")
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        if PUB_ASYNC_RE.match(lines[i]):
            start = i
            buf = []
            ended = False
            while i < len(lines):
                line = lines[i]
                buf.append(line)
                if "{" in line or line.rstrip().endswith(";") or re.search(r"\bwhere\b", line):
                    if re.search(r"\bwhere\b", line):
                        # Consume up to the next `{` or `;`
                        j = i + 1
                        while j < len(lines) and "{" not in lines[j] and ";" not in lines[j]:
                            buf.append(lines[j])
                            j += 1
                        if j < len(lines):
                            buf.append(lines[j])
                            i = j
                    ended = True
                    break
                i += 1
            if not ended:
                return
            sig = "\n".join(buf)
            m = FN_NAME_RE.search(sig)
            name = m.group(1) if m else "<unknown>"
            yield start + 1, name, sig
        i += 1


def is_covered(sig: str) -> bool:
    return any(pat.search(sig) for pat in COVERED_PATTERNS)


def audit() -> dict:
    files = sorted(SRC_ROOT.rglob("*.rs"))
    results = {
        "total_sites": 0,
        "exempt_files_sites": 0,
        "wrapper_exempt_sites": 0,
        "covered_sites": 0,
        "uncovered_sites": 0,
        "by_file": {},
        "uncovered_examples": [],  # First N uncovered for debug
        "wrapper_audit_errors": [],
    }
    file_data: dict[str, dict] = {}
    for path in files:
        rel = path.relative_to(SRC_ROOT).as_posix()
        is_exempt_file = path.name in EXEMPT_FILES
        # Index function names per file so we can sanity-check the
        # WRAPPER_EXEMPTIONS allowlist.
        fn_names: set[str] = set()
        local_total = local_covered = local_uncovered = local_wrapper = 0
        local_uncovered_lines = []
        for line_no, name, sig in signature_blocks(path):
            results["total_sites"] += 1
            local_total += 1
            fn_names.add(name)
            if is_exempt_file:
                results["exempt_files_sites"] += 1
                continue
            if is_covered(sig):
                results["covered_sites"] += 1
                local_covered += 1
                continue
            if (rel, name) in WRAPPER_EXEMPTIONS:
                results["wrapper_exempt_sites"] += 1
                local_wrapper += 1
                continue
            results["uncovered_sites"] += 1
            local_uncovered += 1
            local_uncovered_lines.append((line_no, name))
            if len(results["uncovered_examples"]) < 25:
                results["uncovered_examples"].append({
                    "file": rel,
                    "line": line_no,
                    "fn": name,
                })
        # Cross-check wrapper allowlist: each (file, name) must point
        # at a real fn in this file whose name has a covered sibling.
        if not is_exempt_file:
            for ef, ename in WRAPPER_EXEMPTIONS:
                if ef != rel:
                    continue
                if ename not in fn_names:
                    results["wrapper_audit_errors"].append(
                        f"{rel}::{ename} listed in WRAPPER_EXEMPTIONS but no such pub async fn"
                    )
                    continue
                expected_siblings = [f"{ename}_with_cx", f"{ename}_cx"]
                if not any(s in fn_names for s in expected_siblings):
                    results["wrapper_audit_errors"].append(
                        f"{rel}::{ename} wrapper-exempt but no _with_cx/_cx sibling found"
                    )
        file_data[rel] = {
            "exempt_file": is_exempt_file,
            "total": local_total,
            "covered": local_covered,
            "uncovered": local_uncovered,
            "wrapper_exempt": local_wrapper,
            "uncovered_lines": local_uncovered_lines,
        }
    results["by_file"] = file_data
    return results


def load_baseline() -> dict | None:
    if not BASELINE_PATH.is_file():
        return None
    return json.loads(BASELINE_PATH.read_text())


def save_baseline(audit_data: dict) -> None:
    payload = {
        "comment": "ft-3kv6e ratchet baseline. Lower with each adoption commit. "
                   "Generated by scripts/check_runtime_proof_coverage.py.",
        "uncovered_sites": audit_data["uncovered_sites"],
        "covered_sites": audit_data["covered_sites"],
        "exempt_files_sites": audit_data["exempt_files_sites"],
        "wrapper_exempt_sites": audit_data["wrapper_exempt_sites"],
        "total_sites": audit_data["total_sites"],
        "by_file_uncovered": {
            f: data["uncovered"]
            for f, data in sorted(audit_data["by_file"].items())
            if data["uncovered"] > 0
        },
    }
    BASELINE_PATH.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")


def main() -> int:
    p = argparse.ArgumentParser(description="ft-3kv6e RuntimeProof coverage audit")
    p.add_argument("--update-baseline", action="store_true",
                   help="Rewrite the baseline JSON to match current state.")
    p.add_argument("--json", action="store_true",
                   help="Emit a machine-readable JSON summary instead of human text.")
    p.add_argument("--summary", action="store_true",
                   help="Print only the headline numbers (no per-file detail).")
    args = p.parse_args()

    data = audit()

    if data["wrapper_audit_errors"]:
        print("ft-3kv6e: WRAPPER_EXEMPTIONS allowlist is inconsistent:", file=sys.stderr)
        for err in data["wrapper_audit_errors"]:
            print(f"  - {err}", file=sys.stderr)
        return 2

    if args.update_baseline:
        save_baseline(data)
        print(f"Baseline updated: uncovered={data['uncovered_sites']} "
              f"covered={data['covered_sites']} exempt={data['exempt_files_sites']}")
        return 0

    if args.json:
        print(json.dumps(data, indent=2, sort_keys=True))
        return 0

    print(f"ft-3kv6e RuntimeProof coverage audit")
    print(f"  total pub async fn      : {data['total_sites']}")
    print(f"  in exempt runtime files : {data['exempt_files_sites']}")
    print(f"  covered (Cx/RuntimeProof): {data['covered_sites']}")
    print(f"  wrapper-exempt          : {data['wrapper_exempt_sites']}")
    print(f"  uncovered               : {data['uncovered_sites']}")

    baseline = load_baseline()
    if baseline is None:
        print()
        print("WARNING: no baseline file at", BASELINE_PATH)
        print("Run with --update-baseline to seed it.")
        return 0

    baseline_uncovered = baseline.get("uncovered_sites", 0)
    print()
    print(f"Baseline (from {BASELINE_PATH.name}): uncovered={baseline_uncovered}")
    if data["uncovered_sites"] > baseline_uncovered:
        delta = data["uncovered_sites"] - baseline_uncovered
        print(f"FAIL: uncovered count grew by {delta} since baseline.", file=sys.stderr)
        if not args.summary:
            print("Newly-introduced sites likely include:", file=sys.stderr)
            for ex in data["uncovered_examples"][:15]:
                print(f"  {ex['file']}:{ex['line']} :: {ex['fn']}", file=sys.stderr)
        return 1
    if data["uncovered_sites"] < baseline_uncovered:
        delta = baseline_uncovered - data["uncovered_sites"]
        print(f"PROGRESS: uncovered count dropped by {delta}. "
              f"Re-run with --update-baseline in this commit.")
    else:
        print("Uncovered count matches baseline (no regression).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
