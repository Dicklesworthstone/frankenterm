#!/bin/bash
# E4.F1.T4: FrankenSqlite migration E2E — full M0-M5 pipeline + rollback
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
RUN_ID="${RUN_ID:-$(date -u +%Y%m%d_%H%M%S)-$$}"
SCRIPT_NAME=$(basename "$0")
LOG_DIR="test_results"
LOG_FILE="${LOG_DIR}/${SCRIPT_NAME%.sh}_${RUN_ID}.log"
DEFAULT_RCH_CARGO_TARGET_DIR="target/rch-frankensqlite-migration-${RUN_ID}"
REQUESTED_RCH_CARGO_TARGET_DIR="${RCH_CARGO_TARGET_DIR:-}"
if [[ -n "$REQUESTED_RCH_CARGO_TARGET_DIR" && "$REQUESTED_RCH_CARGO_TARGET_DIR" != /* ]]; then
    RCH_CARGO_TARGET_DIR="$REQUESTED_RCH_CARGO_TARGET_DIR"
else
    RCH_CARGO_TARGET_DIR="$DEFAULT_RCH_CARGO_TARGET_DIR"
fi
RCH_READY=0
RCH_STEP_INDEX=0
mkdir -p "$LOG_DIR"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$PROJECT_ROOT/tests/e2e/lib_rch_guards.sh"
RCH_SKIP_SMOKE_PREFLIGHT=1

exec > >(tee -a "$LOG_FILE") 2>&1

ensure_rch_for_cargo_tests() {
    if [[ "$RCH_READY" -eq 1 ]]; then
        return 0
    fi

    rch_init "$LOG_DIR" "$RUN_ID" "frankensqlite_migration" "$PROJECT_ROOT"
    ensure_rch_ready
    RCH_READY=1
}

run_rch_cargo() {
    ensure_rch_for_cargo_tests

    RCH_STEP_INDEX=$((RCH_STEP_INDEX + 1))
    local output_file="${LOG_DIR}/${SCRIPT_NAME%.sh}_${RUN_ID}_step${RCH_STEP_INDEX}.rch.log"

    set +e
    run_rch_cargo_logged "$output_file" env CARGO_TARGET_DIR="$RCH_CARGO_TARGET_DIR" cargo "$@"
    local rc=$?
    set -e

    cat "$output_file"
    return "$rc"
}

echo "=== [$SCRIPT_NAME] Starting at $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"frankensqlite_migration","step":"start","result":"running"}'

# Run E2E migration tests
echo "--- Running E2E migration tests ---"
if run_rch_cargo test -p frankenterm-core --test frankensqlite_e2e_tests 2>&1; then
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"e2e_migration","step":"complete","result":"pass"}'
else
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"e2e_migration","step":"complete","result":"fail"}'
    echo "=== [$SCRIPT_NAME] RESULT: FAIL ==="
    exit 1
fi

echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"frankensqlite_migration","step":"finish","result":"pass"}'
echo "=== [$SCRIPT_NAME] RESULT: PASS ==="
