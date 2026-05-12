#!/usr/bin/env python3
"""Lockstep guard: Python ↔ Rust cx-propagation allow-list divergence check.

The Python audit script (ft-3kv6e ratchet) and the Rust analyzer
(ft-t9a6q.1, substrate at lints/cx_propagation/) share two
allow-lists: EXEMPT_FILES + WRAPPER_EXEMPTIONS. They MUST stay
in lockstep — divergence means the two enforcement surfaces give
different answers and the runtime-proof contract has a hole.

This script parses both sources and exits 1 on any divergence.

Sources:
  Python: scripts/check_runtime_proof_coverage.py
          (EXEMPT_FILES + WRAPPER_EXEMPTIONS, imported via
          importlib.util)
  Rust:   lints/cx_propagation/src/allow_list.rs
          (EXEMPT_FILES + WRAPPER_EXEMPTIONS, parsed from source)

Usage:
  scripts/check_cx_propagation_lockstep.py            # check + exit 1 on drift
  scripts/check_cx_propagation_lockstep.py --print    # also print the diff
  scripts/check_cx_propagation_lockstep.py --json     # machine-readable output

Bead: ft-jbsbx (BR-RC-RUNTIME-SEMANTICS.G14.1.cont.drift).
Doc: docs/runtime/cx-propagation-lint.md.
Sibling beads:
  ft-t9a6q.1 — Rust analyzer substrate (closed).
  ft-l8bmk   — full-list port (closed; the precondition).
  ft-3kv6e   — Python audit ratchet (foundation).
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
PYTHON_AUDIT = REPO_ROOT / "scripts" / "check_runtime_proof_coverage.py"
RUST_ALLOW_LIST = REPO_ROOT / "lints" / "cx_propagation" / "src" / "allow_list.rs"


def load_python_lists() -> tuple[set[str], set[tuple[str, str]]]:
    """Import the Python audit script and read its lists."""
    spec = importlib.util.spec_from_file_location("audit", PYTHON_AUDIT)
    if spec is None or spec.loader is None:
        sys.stderr.write(
            f"check_cx_propagation_lockstep: cannot import {PYTHON_AUDIT}\n"
        )
        sys.exit(2)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    exempt_files = set(mod.EXEMPT_FILES)
    wrapper_exemptions = set(mod.WRAPPER_EXEMPTIONS)
    return exempt_files, wrapper_exemptions


# Match `("path", "fn"),` entries inside WRAPPER_EXEMPTIONS.
# rustfmt expands long tuples as:
#   (
#       "path",
#       "fn",
#   ),
# so accept an optional trailing comma inside the tuple as well as after it.
RUST_TUPLE_RE = re.compile(r'\(\s*"([^"]+)"\s*,\s*"([^"]+)"\s*,?\s*\)\s*,?')
# Match `"name.rs",` entries inside EXEMPT_FILES.
RUST_STRING_RE = re.compile(r'"([^"]+)"\s*,')


def load_rust_lists() -> tuple[set[str], set[tuple[str, str]]]:
    """Parse lints/cx_propagation/src/allow_list.rs into the same shape."""
    src = RUST_ALLOW_LIST.read_text(encoding="utf-8")

    exempt = _extract_const_block(src, "EXEMPT_FILES")
    wrapper = _extract_const_block(src, "WRAPPER_EXEMPTIONS")

    if exempt is None:
        sys.stderr.write(
            "check_cx_propagation_lockstep: EXEMPT_FILES const not found "
            f"in {RUST_ALLOW_LIST}\n"
        )
        sys.exit(2)
    if wrapper is None:
        sys.stderr.write(
            "check_cx_propagation_lockstep: WRAPPER_EXEMPTIONS const not "
            f"found in {RUST_ALLOW_LIST}\n"
        )
        sys.exit(2)

    exempt_files = set(RUST_STRING_RE.findall(exempt))
    wrapper_exemptions = {(p, n) for p, n in RUST_TUPLE_RE.findall(wrapper)}
    return exempt_files, wrapper_exemptions


def _extract_const_block(src: str, const_name: str) -> str | None:
    """Find `pub const <NAME>: ... = &[ ... ];` and return the array body.

    The type annotation may itself contain `&[` (e.g. `&[(&str, &str)]`),
    so we anchor on `=` after the const name and then find the next `&[`.
    """
    marker = f"pub const {const_name}"
    idx = src.find(marker)
    if idx == -1:
        return None
    eq_idx = src.find("=", idx)
    if eq_idx == -1:
        return None
    bracket_idx = src.find("&[", eq_idx)
    if bracket_idx == -1:
        return None
    # Find matching closing bracket. The array body uses no nested
    # brackets/parens that aren't inside string literals — for the
    # tuple-pair list this is true by construction.
    depth = 0
    in_str = False
    prev_back = False
    body_start = bracket_idx + 2
    for i, c in enumerate(src[body_start:], start=body_start):
        if in_str:
            if prev_back:
                prev_back = False
            elif c == "\\":
                prev_back = True
            elif c == '"':
                in_str = False
            continue
        if c == '"':
            in_str = True
        elif c == "[":
            depth += 1
        elif c == "]":
            if depth == 0:
                return src[body_start:i]
            depth -= 1
    return None


def render_diff(label: str, py: set, rs: set) -> list[str]:
    py_only = py - rs
    rs_only = rs - py
    out = [f"=== {label} ==="]
    if py_only:
        out.append(f"  Python-only ({len(py_only)}):")
        for entry in sorted(py_only):
            out.append(f"    {entry}")
    if rs_only:
        out.append(f"  Rust-only ({len(rs_only)}):")
        for entry in sorted(rs_only):
            out.append(f"    {entry}")
    if not py_only and not rs_only:
        out.append(f"  In sync ({len(py)} entries).")
    return out


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--print",
        dest="print_diff",
        action="store_true",
        help="print the full diff to stdout (always prints in --json mode)",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit machine-readable JSON summary on stdout",
    )
    args = parser.parse_args()

    py_exempt, py_wrapper = load_python_lists()
    rs_exempt, rs_wrapper = load_rust_lists()

    exempt_drift = py_exempt.symmetric_difference(rs_exempt)
    wrapper_drift = py_wrapper.symmetric_difference(rs_wrapper)
    in_sync = not exempt_drift and not wrapper_drift

    if args.json:
        print(
            json.dumps(
                {
                    "in_sync": in_sync,
                    "exempt_files": {
                        "python_only": sorted(py_exempt - rs_exempt),
                        "rust_only": sorted(rs_exempt - py_exempt),
                        "shared_count": len(py_exempt & rs_exempt),
                    },
                    "wrapper_exemptions": {
                        "python_only": sorted(py_wrapper - rs_wrapper),
                        "rust_only": sorted(rs_wrapper - py_wrapper),
                        "shared_count": len(py_wrapper & rs_wrapper),
                    },
                },
                indent=2,
                sort_keys=True,
            )
        )
    elif args.print_diff or not in_sync:
        for line in render_diff("EXEMPT_FILES", py_exempt, rs_exempt):
            print(line)
        for line in render_diff("WRAPPER_EXEMPTIONS", py_wrapper, rs_wrapper):
            print(line)

    if not in_sync:
        sys.stderr.write(
            "\ncheck_cx_propagation_lockstep: DRIFT — Python and Rust "
            "allow-lists are out of sync. "
            "Update both lists in lockstep.\n"
        )
        return 1
    if not args.json:
        print(
            f"check_cx_propagation_lockstep: in sync. "
            f"{len(py_exempt)} EXEMPT_FILES + {len(py_wrapper)} "
            f"WRAPPER_EXEMPTIONS shared between Python and Rust."
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
