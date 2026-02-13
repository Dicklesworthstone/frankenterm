#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOG_DIR="${PROJECT_ROOT}/target/e2e"
LOG_FILE="${LOG_DIR}/snapshot_e2e.log"

mkdir -p "$LOG_DIR"

echo "=== FrankenTerm Snapshot E2E ==="
echo "Started: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "Log: ${LOG_FILE}"

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${PROJECT_ROOT}/target/e2e-target}" \
    cargo test -p frankenterm-core --test snapshot_e2e -- --nocapture 2>&1 | tee "$LOG_FILE"

echo
echo "=== E2E Report Summary ==="
if command -v rg >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
    if rg -q "\\[E2E_REPORT\\]" "$LOG_FILE"; then
        rg "\\[E2E_REPORT\\]" "$LOG_FILE" \
            | sed 's/^.*\[E2E_REPORT\] //' \
            | jq -r '"- \(.test_name): " + (if .passed then "PASS" else "FAIL" end) + " (" + (.total_duration_ms|tostring) + "ms)"'
    else
        echo "- No structured [E2E_REPORT] lines found in log."
    fi
else
    echo "- Install rg + jq for parsed summary output."
fi

echo
echo "Done: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
