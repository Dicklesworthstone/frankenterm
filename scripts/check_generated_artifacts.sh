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
import subprocess

paths = subprocess.check_output(["git", "ls-files", "--others", "--exclude-standard", "-z"])
for path in sorted(filter(None, paths.split(b"\0"))):
    if os.path.islink(path):
        content = os.readlink(path)
    else:
        with open(path, "rb") as source:
            content = source.read()
    print(repr(path), hashlib.sha256(content).hexdigest())
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
    echo "[INFO] No generators found; drift check will still verify clean tree."
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
