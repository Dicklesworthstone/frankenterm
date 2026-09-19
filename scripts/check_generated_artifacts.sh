#!/usr/bin/env bash
# =============================================================================
# CI: Regenerate schema/docs/types and fail on drift.
#
# This script runs any available generator scripts and fails if the working tree
# changes. Generator scripts are optional and can be added over time.
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

run_generator() {
    local script="$1"
    if [[ -f "$script" ]]; then
        echo "[INFO] Running generator: $script"
        if (cd "$PROJECT_ROOT" && CI=1 bash "$script"); then
            return 0
        else
            local status=$?
            # Callers use this function as an if condition, which disables
            # errexit. A failed generator must terminate the gate explicitly.
            echo "[ERROR] Generator failed (exit $status): $script" >&2
            exit "$status"
        fi
    fi

    echo "[INFO] Skipping generator (not found): $script"
    return 1
}

cd "$PROJECT_ROOT"

# Compare worktree bytes, not the shared index: unrelated agent edits are not
# generated drift. Include untracked contents so rewriting an already-untracked
# generated file cannot pass merely because its filename stayed the same.
snapshot() {
    git --no-pager diff --binary HEAD -- || return $?
    python3 - <<'PY'
import hashlib
import os
import stat
import subprocess
import sys

paths = subprocess.check_output(["git", "ls-files", "--others", "--exclude-standard", "-z"])
for path in sorted(filter(None, paths.split(b"\0"))):
    metadata = os.lstat(path)
    digest = hashlib.sha256()
    if stat.S_ISLNK(metadata.st_mode):
        digest.update(os.readlink(path))
    elif stat.S_ISREG(metadata.st_mode):
        # A concurrent replacement must not turn the open into a blocking FIFO
        # read or make us follow a symlink. Check the opened descriptor as well.
        flags = os.O_RDONLY | getattr(os, "O_NONBLOCK", 0) | getattr(os, "O_NOFOLLOW", 0)
        with os.fdopen(os.open(path, flags), "rb") as source:
            opened = os.fstat(source.fileno())
            if not stat.S_ISREG(opened.st_mode) or (
                opened.st_dev, opened.st_ino, opened.st_mode
            ) != (metadata.st_dev, metadata.st_ino, metadata.st_mode):
                sys.exit(f"[ERROR] Artifact changed during snapshot: {path!r}")
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
    else:
        sys.exit(f"[ERROR] Unsupported special file in artifact snapshot: {path!r}")
    print(repr(path), oct(metadata.st_mode), digest.hexdigest())
PY
}

before=$(snapshot)

# Track whether any generator ran (informational only).
ran_any=0

if run_generator "$PROJECT_ROOT/scripts/generate_schema_docs.sh"; then
    ran_any=1
fi

if run_generator "$PROJECT_ROOT/scripts/generate_types.sh"; then
    ran_any=1
fi

if run_generator "$PROJECT_ROOT/scripts/generate_cli_reference.sh"; then
    ran_any=1
fi

if [[ $ran_any -eq 0 ]]; then
    echo "[INFO] No generators found; checking that worktree artifacts stayed unchanged."
fi

# Fail if regeneration changed even an already-dirty generated artifact.
after=$(snapshot)
if [[ "$before" != "$after" ]]; then
    echo "[ERROR] Generated artifacts are out of date."
    echo "[ERROR] Run the generator scripts locally and commit the results."
    diff -u <(printf '%s\n' "$before") <(printf '%s\n' "$after") || true
    exit 1
fi

echo "[INFO] Generators succeeded without changing worktree artifacts."
