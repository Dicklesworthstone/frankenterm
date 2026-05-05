#!/bin/bash
# E4.F1.T4: FrankenSqlite basic E2E — contract + unit tests
set -euo pipefail
SCRIPT_NAME=$(basename "$0")
LOG_DIR="test_results"
LOG_FILE="${LOG_DIR}/${SCRIPT_NAME%.sh}_$(date +%Y%m%d_%H%M%S).log"
RCH_BIN="${RCH_BIN:-rch}"
RCH_CARGO_TARGET_DIR="${RCH_CARGO_TARGET_DIR:-/tmp/ft-frankensqlite-basic-target}"
mkdir -p "$LOG_DIR"

exec > >(tee -a "$LOG_FILE") 2>&1

run_rch_cargo() {
    "$RCH_BIN" exec -- env CARGO_TARGET_DIR="$RCH_CARGO_TARGET_DIR" cargo "$@"
}

echo "=== [$SCRIPT_NAME] Starting at $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"frankensqlite_basic","step":"start","result":"running"}'

# Run contract-level tests
echo "--- Running contract tests ---"
if run_rch_cargo test -p frankenterm-core --test frankensqlite_contract_tests 2>&1; then
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"contract_tests","step":"complete","result":"pass"}'
else
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"contract_tests","step":"complete","result":"fail"}'
    echo "=== [$SCRIPT_NAME] RESULT: FAIL ==="
    exit 1
fi

echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"frankensqlite_basic","step":"finish","result":"pass"}'
echo "=== [$SCRIPT_NAME] RESULT: PASS ==="
