#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOG_DIR="${PROJECT_ROOT}/target/e2e"
LOG_FILE="${LOG_DIR}/snapshot_e2e.log"
RUN_ID="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
DEFAULT_CARGO_TARGET_DIR="target/rch-snapshot-e2e-${RUN_ID}"
REQUESTED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "$REQUESTED_CARGO_TARGET_DIR" && "$REQUESTED_CARGO_TARGET_DIR" != /* ]]; then
    CARGO_TARGET_DIR="$REQUESTED_CARGO_TARGET_DIR"
else
    CARGO_TARGET_DIR="$DEFAULT_CARGO_TARGET_DIR"
fi

mkdir -p "$LOG_DIR"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$PROJECT_ROOT/tests/e2e/lib_rch_guards.sh"
RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
rch_init "$LOG_DIR" "$RUN_ID" "snapshot_e2e" "$PROJECT_ROOT"
ensure_rch_ready

echo "=== FrankenTerm Snapshot E2E ==="
echo "Started: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "Log: ${LOG_FILE}"
echo "RCH cargo target: ${CARGO_TARGET_DIR}"

run_rch_cargo_logged "$LOG_FILE" \
    env CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
    cargo test -p frankenterm-core --test snapshot_e2e -- --nocapture
cat "$LOG_FILE"

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
