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
        (cd "$PROJECT_ROOT" && CI=1 bash "$script")
        return 0
    fi

    echo "[INFO] Skipping generator (not found): $script"
    return 1
}

cd "$PROJECT_ROOT"

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

# Fail if regeneration produced diffs.
if ! git diff --exit-code >/dev/null; then
    echo "[ERROR] Generated artifacts are out of date."
    echo "[ERROR] Run the generator scripts locally and commit the results."
    git --no-pager diff
    exit 1
fi

untracked=$(git ls-files --others --exclude-standard)
if [[ -n "$untracked" ]]; then
    echo "[ERROR] Generated artifacts created untracked files."
    echo "$untracked"
    exit 1
fi

echo "[INFO] Generated artifacts are up to date."
