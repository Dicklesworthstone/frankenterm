#!/usr/bin/env python3
"""Emit a JSON-line list of past-threshold STALE entries in a MEMORY.md.

Usage:
    memory_staleness_check.py [--memory PATH] [--repo PATH] [--self-test]

The MEMORY.md is expected to be the user's auto-memory index file. STALE
entries follow this convention (set in older sweeps):

    - [name](file.md): summary — STALE (N commits since SHA <hex>, threshold T; ...)

For each STALE entry, this script asks `git -C <repo> rev-list <sha>..HEAD
--count` for the *current* commit count from <hex> and emits a JSON-line
record when current_count >= threshold.

Exit codes:
    0 — script ran cleanly (regardless of whether anything was stale)
    1 — IO/git error
    2 — --self-test failed
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass

DEFAULT_MEMORY_PATH = os.path.expanduser(
    "~/.claude/projects/-Users-jemanuel-projects-frankenterm/memory/MEMORY.md"
)
DEFAULT_REPO = os.path.expanduser("~/projects/frankenterm")

ENTRY_RE = re.compile(
    r"^- \[(?P<id>[^\]]+)\]\((?P<file>[^\)]+)\):.*?STALE \((?P<noted_count>\d+) commits since SHA (?P<sha>[0-9a-f]{7,40}), threshold (?P<threshold>\d+)",
    re.IGNORECASE,
)


@dataclass(frozen=True)
class Entry:
    id: str
    file: str
    sha: str
    threshold: int
    noted_count: int


def parse_memory(text: str) -> list[Entry]:
    out: list[Entry] = []
    for line in text.splitlines():
        m = ENTRY_RE.match(line.strip())
        if not m:
            continue
        out.append(
            Entry(
                id=m.group("id"),
                file=m.group("file"),
                sha=m.group("sha"),
                threshold=int(m.group("threshold")),
                noted_count=int(m.group("noted_count")),
            )
        )
    return out


def commit_count(repo: str, sha: str) -> int:
    res = subprocess.run(
        ["git", "-C", repo, "rev-list", f"{sha}..HEAD", "--count"],
        capture_output=True,
        text=True,
        check=False,
    )
    if res.returncode != 0:
        raise RuntimeError(f"git rev-list failed for {sha}: {res.stderr.strip()}")
    return int(res.stdout.strip())


def emit_records(entries: list[Entry], repo: str, sink) -> int:
    flagged = 0
    for e in entries:
        try:
            current = commit_count(repo, e.sha)
        except Exception as ex:
            sink.write(
                json.dumps(
                    {
                        "entry_id": e.id,
                        "sha": e.sha,
                        "threshold": e.threshold,
                        "current_commit_count": None,
                        "stale": None,
                        "error": str(ex),
                    }
                )
                + "\n"
            )
            continue
        is_stale = current >= e.threshold
        if is_stale:
            flagged += 1
        sink.write(
            json.dumps(
                {
                    "entry_id": e.id,
                    "file": e.file,
                    "sha": e.sha,
                    "threshold": e.threshold,
                    "noted_count": e.noted_count,
                    "current_commit_count": current,
                    "stale": is_stale,
                }
            )
            + "\n"
        )
    return flagged


SELF_TEST_FIXTURE = """\
# MEMORY.md
- [agent-mail-fetch-inbox-timeouts](project_agent_mail_timeouts.md): chronic timeouts
- [old-clean-sweep](project_old_clean.md): 0 cat-A — STALE (24 commits since SHA 2ca20e18, threshold 5; ft-hph8i)
- [fresh-sweep-2026-04-28](project_fresh.md): 0 stubs — STALE (1 commits since SHA deadbeef, threshold 999)
- not a memory entry
- [tagged-no-stale](file.md): never been STALE; this line should not parse
"""


def self_test() -> int:
    entries = parse_memory(SELF_TEST_FIXTURE)
    expected_ids = {"old-clean-sweep", "fresh-sweep-2026-04-28"}
    got_ids = {e.id for e in entries}
    if got_ids != expected_ids:
        print(f"SELF-TEST FAIL: expected ids {expected_ids}, got {got_ids}", file=sys.stderr)
        return 2
    old = next(e for e in entries if e.id == "old-clean-sweep")
    if old.threshold != 5 or old.sha != "2ca20e18" or old.noted_count != 24:
        print(f"SELF-TEST FAIL: old-clean-sweep parsed wrong: {old}", file=sys.stderr)
        return 2
    fresh = next(e for e in entries if e.id == "fresh-sweep-2026-04-28")
    if fresh.threshold != 999:
        print(f"SELF-TEST FAIL: fresh threshold {fresh.threshold} != 999", file=sys.stderr)
        return 2
    print("SELF-TEST PASS")
    return 0


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--memory", default=DEFAULT_MEMORY_PATH, help="path to MEMORY.md")
    p.add_argument("--repo", default=DEFAULT_REPO, help="path to git repo for commit counting")
    p.add_argument("--self-test", action="store_true", help="run parser self-test and exit")
    args = p.parse_args(argv)

    if args.self_test:
        return self_test()

    try:
        with open(args.memory, "r", encoding="utf-8") as f:
            text = f.read()
    except OSError as e:
        print(f"could not read {args.memory}: {e}", file=sys.stderr)
        return 1

    entries = parse_memory(text)
    flagged = emit_records(entries, args.repo, sys.stdout)
    print(
        json.dumps({"summary": {"total_stale_entries": len(entries), "past_threshold": flagged}}),
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
