#!/usr/bin/env python3
"""ft-i2eni.6 — Generate the vendored-fork provenance manifest.

Walks the vendored crate tree under `frankenterm/` (the wezterm fork
that ft owns), reads each crate's Cargo.toml, and queries `git log`
for the fork-side commit history per crate. Emits a machine-readable
JSON manifest at `frankenterm/PROVENANCE.json`.

The manifest captures, per vendored crate:
- name and path
- declared version
- declared upstream authors (as a hint to the wezterm origin)
- count of fork-side commits since the divergence point
- last modified commit (sha + subject + author + date)
- a coarse classification summary derived from commit-subject keywords
  (security-fix / runtime-swap / cleanup / cosmetic / other)

Plus top-level metadata:
- divergence-point commit SHA (the "Import WezTerm source" import commit)
- total fork-side commits across the vendored tree
- total tracked vendored crates
- generated_at timestamp

Usage:
  scripts/regen-provenance.py                    # rewrite frankenterm/PROVENANCE.json
  scripts/regen-provenance.py --check            # exit 1 if drift > 0 lines
  scripts/regen-provenance.py --output PATH      # write elsewhere

Cross-references:
  ft-i2eni.6 — this bead
  ft-zoxxq — vendored fork stance + boundary truth
  scripts/check-provenance.sh — shell wrapper for the --check mode
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
DEFAULT_OUTPUT = REPO_ROOT / "frankenterm" / "PROVENANCE.json"

CLASSIFICATION_KEYWORDS = [
    # Order matters: first match wins.
    ("security-fix", re.compile(r"\b(cve|sec|security|vuln|escape|injection)\b", re.I)),
    ("runtime-swap", re.compile(r"\b(asupersync|tokio|smol|runtime[_-]?compat|runtime[_-]?async)\b", re.I)),
    ("rename", re.compile(r"\b(rename|wezterm\s*->|->\s*frankenterm)\b", re.I)),
    ("fix", re.compile(r"^\s*fix(\([^)]+\))?\s*[:!]", re.I)),
    ("feat", re.compile(r"^\s*feat(\([^)]+\))?\s*[:!]", re.I)),
    ("test", re.compile(r"^\s*test(\([^)]+\))?\s*[:!]", re.I)),
    ("docs", re.compile(r"^\s*docs(\([^)]+\))?\s*[:!]", re.I)),
    ("refactor", re.compile(r"^\s*refactor(\([^)]+\))?\s*[:!]", re.I)),
    ("cleanup", re.compile(r"\b(clean[ -]?up|tidy|remove dead|drop unused|cleanup\b)", re.I)),
    ("cosmetic", re.compile(r"^\s*style(\([^)]+\))?\s*[:!]|\b(rustfmt|clippy)\b", re.I)),
    ("chore", re.compile(r"^\s*chore(\([^)]+\))?\s*[:!]", re.I)),
]


def run(*args: str) -> str:
    out = subprocess.run(args, capture_output=True, text=True, check=True, cwd=REPO_ROOT)
    return out.stdout


def workspace_vendored_paths() -> list[str]:
    """Return the sorted list of `frankenterm/<crate>` workspace members
    declared in the top-level Cargo.toml."""
    cargo = (REPO_ROOT / "Cargo.toml").read_text()
    in_block = False
    paths: list[str] = []
    for line in cargo.splitlines():
        stripped = line.strip()
        if stripped.startswith("members = ["):
            in_block = True
            continue
        if in_block and stripped == "]":
            break
        if in_block:
            m = re.search(r'"(frankenterm/[^"]+)"', stripped)
            if m:
                paths.append(m.group(1))
    return sorted(set(paths))


def parse_cargo_toml(path: Path) -> dict:
    """Best-effort parse of [package] section. Returns dict with
    name/version/authors. We avoid bringing in tomllib so the script
    runs on any python3 the CI machine ships."""
    if not path.is_file():
        return {}
    text = path.read_text()
    # Trim to the [package] section.
    pkg = re.search(r"\[package\](?P<body>.+?)(?=^\[)", text, re.DOTALL | re.MULTILINE)
    body = pkg.group("body") if pkg else text
    out: dict = {}
    name = re.search(r'^\s*name\s*=\s*"([^"]+)"', body, re.MULTILINE)
    if name:
        out["name"] = name.group(1)
    version = re.search(r'^\s*version\s*=\s*"([^"]+)"', body, re.MULTILINE)
    if version:
        out["version"] = version.group(1)
    authors = re.search(r'^\s*authors\s*=\s*(\[[^\]]*\])', body, re.MULTILINE)
    if authors:
        out["authors_raw"] = authors.group(1).strip()
    return out


def divergence_commit() -> dict:
    """Find the import commit that brought wezterm sources in as the
    fork. We use the first commit that introduced
    frankenterm/codec/Cargo.toml as a stable proxy."""
    raw = run(
        "git", "log",
        "--diff-filter=A",
        "--format=%H%x09%aI%x09%s",
        "--", "frankenterm/codec/Cargo.toml",
    ).strip().splitlines()
    if not raw:
        return {}
    # Last line is the earliest add (`git log` is reverse-chrono).
    sha, ts, subject = raw[-1].split("\t", 2)
    return {"sha": sha, "committed_at": ts, "subject": subject}


def fork_side_commits(rel_path: str) -> list[dict]:
    raw = run(
        "git", "log",
        "--format=%H%x09%aI%x09%an%x09%s",
        "--",
        rel_path,
    ).strip()
    if not raw:
        return []
    out: list[dict] = []
    for line in raw.splitlines():
        parts = line.split("\t", 3)
        if len(parts) != 4:
            continue
        sha, when, author, subject = parts
        out.append({
            "sha": sha,
            "committed_at": when,
            "author": author,
            "subject": subject,
            "classification": classify_subject(subject),
        })
    return out


def classify_subject(subject: str) -> str:
    for label, pat in CLASSIFICATION_KEYWORDS:
        if pat.search(subject):
            return label
    return "other"


def summarize_classifications(commits: list[dict]) -> dict:
    summary: dict[str, int] = {}
    for c in commits:
        summary[c["classification"]] = summary.get(c["classification"], 0) + 1
    return {k: summary[k] for k in sorted(summary, key=lambda k: (-summary[k], k))}


def build_crate_entry(rel_path: str) -> dict:
    cargo_path = REPO_ROOT / rel_path / "Cargo.toml"
    cargo = parse_cargo_toml(cargo_path)
    commits = fork_side_commits(rel_path)
    last = commits[0] if commits else None
    return {
        "name": cargo.get("name", Path(rel_path).name),
        "path": rel_path,
        "version": cargo.get("version"),
        "authors_raw": cargo.get("authors_raw"),
        "fork_side_commit_count": len(commits),
        "last_modified": last,
        "classification_summary": summarize_classifications(commits),
    }


def build_manifest() -> dict:
    paths = workspace_vendored_paths()
    crates = [build_crate_entry(p) for p in paths]
    total_commits = sum(c["fork_side_commit_count"] for c in crates)
    return {
        "schema_version": "1",
        "fork_repo": "frankenterm",
        "vendored_root": "frankenterm/",
        "divergence_point": divergence_commit(),
        "vendored_crate_count": len(crates),
        "fork_side_commit_total": total_commits,
        "generated_at": dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z"),
        "produced_by": "scripts/regen-provenance.py",
        "crates": crates,
    }


def emit_json(manifest: dict) -> str:
    return json.dumps(manifest, indent=2, sort_keys=True) + "\n"


def main() -> int:
    p = argparse.ArgumentParser(description="ft-i2eni.6 vendored-fork provenance generator")
    p.add_argument("--output", default=str(DEFAULT_OUTPUT),
                   help="Output path (default: frankenterm/PROVENANCE.json)")
    p.add_argument("--check", action="store_true",
                   help="Compare regeneration vs on-disk; exit 1 on drift.")
    p.add_argument("--ignore-timestamp", action="store_true",
                   help="In --check, treat the generated_at field as not-significant.")
    args = p.parse_args()

    manifest = build_manifest()
    payload = emit_json(manifest)
    out_path = Path(args.output)

    if args.check:
        if not out_path.is_file():
            print(f"ft-i2eni.6: PROVENANCE.json missing at {out_path}", file=sys.stderr)
            return 1
        existing = out_path.read_text()
        if args.ignore_timestamp:
            existing = re.sub(r'"generated_at":\s*"[^"]*"', '"generated_at": "<stripped>"', existing)
            payload_cmp = re.sub(r'"generated_at":\s*"[^"]*"', '"generated_at": "<stripped>"', payload)
        else:
            payload_cmp = payload
        if existing == payload_cmp:
            print(f"ft-i2eni.6: PROVENANCE.json is current ({manifest['vendored_crate_count']} crates, "
                  f"{manifest['fork_side_commit_total']} fork-side commits).")
            return 0
        # Drift summary.
        existing_lines = existing.splitlines()
        regen_lines = payload_cmp.splitlines()
        diff_count = sum(1 for a, b in zip(existing_lines, regen_lines) if a != b)
        diff_count += abs(len(existing_lines) - len(regen_lines))
        print(f"ft-i2eni.6: PROVENANCE.json drift detected ({diff_count} lines differ).", file=sys.stderr)
        print("           Run scripts/regen-provenance.py to refresh the manifest.", file=sys.stderr)
        return 1

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(payload)
    print(f"ft-i2eni.6: wrote {out_path}: {manifest['vendored_crate_count']} crates, "
          f"{manifest['fork_side_commit_total']} fork-side commits, divergence "
          f"{manifest['divergence_point'].get('sha', '?')[:12] if manifest['divergence_point'] else '?'}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
