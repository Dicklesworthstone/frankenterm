#!/usr/bin/env python3
"""frankensqlite readiness checker (br-ft-kcdqp).

Tracks whether the upstream frankensqlite project has shipped a
Phase 5+ release that the FrankenSQLiteBackend implementation
depends on. Without an external network call (which the operator
may run from a sandboxed environment), this script reads the
project's checked-in CHANGELOG / VERSION / Cargo.toml when the
operator has it cloned alongside frankenterm and reports the
phase number.

ft-kcdqp is blocked on "frankensqlite Phase 5+ shipping" per the
bead's external-precondition list. This script eliminates the
"is it time yet?" round-trip during weekly swarm sweeps.

## Usage

::

    # Check the default sibling path (../frankensqlite):
    python3 scripts/check_frankensqlite_readiness.py

    # Explicit path override:
    python3 scripts/check_frankensqlite_readiness.py \
        --path /path/to/frankensqlite

    # JSON output (for CI integration):
    python3 scripts/check_frankensqlite_readiness.py --json

## Output

The script reports one of three states:

- `unknown` — the frankensqlite checkout is not at the supplied
  path (or any of the conventional sibling paths). Exit 1.
- `not-ready` — checkout exists but its declared phase is below
  5 OR no phase marker can be parsed. Exit 2.
- `ready` — checkout's CHANGELOG / VERSION / Cargo.toml shows a
  phase >= 5. Exit 0.

The bead's wired-pass slice (FrankenSQLiteBackend impl) is
unblocked when this script reports `ready` against the operator's
checkout.

## What the script reads

In order of preference:
1. ``CHANGELOG.md`` — looks for the most recent "## [Phase N]"
   header.
2. ``VERSION`` — single-line phase number ("5", "5.0", etc.).
3. ``Cargo.toml`` — package.metadata.frankenterm.phase key, when
   present.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

CONVENTIONAL_PATHS = [
    Path("../frankensqlite"),
    Path("../../frankensqlite"),
    Path("/dp/frankensqlite"),
]

PHASE_HEADER_RE = re.compile(r"##\s*\[?Phase\s+(\d+)", re.IGNORECASE)
VERSION_RE = re.compile(r"^\s*(\d+)(?:\.\d+)?\s*$", re.MULTILINE)


def _read_changelog_phase(path: Path) -> int | None:
    """Return the highest Phase N referenced in CHANGELOG.md.

    The substrate's CHANGELOG convention is `## [Phase N] - YYYY-MM-DD`;
    we match the leading-N pattern and take the max so out-of-order
    entries do not understate readiness.
    """
    changelog = path / "CHANGELOG.md"
    if not changelog.exists():
        return None
    try:
        text = changelog.read_text(encoding="utf-8")
    except OSError:
        return None
    matches = PHASE_HEADER_RE.findall(text)
    if not matches:
        return None
    return max(int(m) for m in matches)


def _read_version_phase(path: Path) -> int | None:
    """Return the integer prefix of VERSION when one is present."""
    version_file = path / "VERSION"
    if not version_file.exists():
        return None
    try:
        text = version_file.read_text(encoding="utf-8")
    except OSError:
        return None
    match = VERSION_RE.search(text)
    if not match:
        return None
    return int(match.group(1))


def _read_cargo_phase(path: Path) -> int | None:
    """Return package.metadata.frankenterm.phase from Cargo.toml.

    Hand-rolled TOML scan (no external deps): finds the
    `[package.metadata.frankenterm]` table and the `phase = N`
    line under it.
    """
    cargo = path / "Cargo.toml"
    if not cargo.exists():
        return None
    try:
        text = cargo.read_text(encoding="utf-8")
    except OSError:
        return None
    in_table = False
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_table = stripped.startswith("[package.metadata.frankenterm")
            continue
        if in_table and stripped.startswith("phase"):
            # Match `phase = 5` or `phase = "5"`.
            match = re.search(r"phase\s*=\s*\"?(\d+)\"?", stripped)
            if match:
                return int(match.group(1))
    return None


def detect_phase(path: Path) -> tuple[int | None, str]:
    """Detect the phase + the source the value came from."""
    for reader, source in (
        (_read_changelog_phase, "CHANGELOG.md"),
        (_read_version_phase, "VERSION"),
        (_read_cargo_phase, "Cargo.toml"),
    ):
        result = reader(path)
        if result is not None:
            return result, source
    return None, "none"


def find_checkout(explicit: Path | None) -> Path | None:
    if explicit is not None:
        return explicit if explicit.exists() else None
    for candidate in CONVENTIONAL_PATHS:
        if candidate.exists():
            return candidate
    return None


def build_report(path: Path | None) -> dict:
    if path is None:
        return {
            "bead": "ft-kcdqp",
            "schema_version": 1,
            "status": "unknown",
            "checkout_path": None,
            "phase": None,
            "phase_source": None,
            "ready": False,
            "notes": (
                "no frankensqlite checkout found at any conventional path "
                f"({', '.join(str(p) for p in CONVENTIONAL_PATHS)}); "
                "rerun with --path /explicit/path"
            ),
        }
    phase, source = detect_phase(path)
    if phase is None:
        return {
            "bead": "ft-kcdqp",
            "schema_version": 1,
            "status": "not-ready",
            "checkout_path": str(path),
            "phase": None,
            "phase_source": source,
            "ready": False,
            "notes": (
                "frankensqlite checkout found but no phase marker visible "
                "(CHANGELOG / VERSION / Cargo.toml package.metadata.frankenterm.phase)"
            ),
        }
    if phase >= 5:
        return {
            "bead": "ft-kcdqp",
            "schema_version": 1,
            "status": "ready",
            "checkout_path": str(path),
            "phase": phase,
            "phase_source": source,
            "ready": True,
            "notes": (
                f"frankensqlite phase {phase} (>=5) — FrankenSQLiteBackend "
                "implementation is unblocked"
            ),
        }
    return {
        "bead": "ft-kcdqp",
        "schema_version": 1,
        "status": "not-ready",
        "checkout_path": str(path),
        "phase": phase,
        "phase_source": source,
        "ready": False,
        "notes": (
            f"frankensqlite phase {phase} (<5) — FrankenSQLiteBackend "
            "implementation remains blocked on Phase 5+ shipping"
        ),
    }


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        prog="check_frankensqlite_readiness",
        description="Detect whether frankensqlite Phase 5+ has shipped.",
    )
    parser.add_argument("--path", type=Path, default=None)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)

    checkout = find_checkout(args.path)
    report = build_report(checkout)

    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        status_label = {
            "ready": "READY",
            "not-ready": "NOT READY",
            "unknown": "UNKNOWN",
        }.get(report["status"], report["status"])
        print(f"frankensqlite readiness: {status_label}")
        if report["checkout_path"]:
            print(f"  checkout: {report['checkout_path']}")
        if report["phase"] is not None:
            print(f"  phase:    {report['phase']} (from {report['phase_source']})")
        print(f"  notes:    {report['notes']}")

    if report["status"] == "ready":
        return 0
    if report["status"] == "unknown":
        return 1
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
