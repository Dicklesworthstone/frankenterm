#!/bin/bash
# E4.F1.T4: FrankenSqlite rollback E2E — inject failures, verify rollback tiers
set -euo pipefail
SCRIPT_NAME=$(basename "$0")
LOG_DIR="test_results"
LOG_FILE="${LOG_DIR}/${SCRIPT_NAME%.sh}_$(date +%Y%m%d_%H%M%S).log"
mkdir -p "$LOG_DIR"

exec > >(tee -a "$LOG_FILE") 2>&1

echo "=== [$SCRIPT_NAME] Starting at $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"frankensqlite_rollback","step":"start","result":"running"}'

# Filter to rollback-specific tests
echo "--- Running rollback scenario tests ---"
ROLLBACK_FILTER="rollback|corruption|data_loss|data_integrity|checkpoint_regression|cardinality|digest_mismatch|suspected"
if cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- "$ROLLBACK_FILTER" 2>&1; then
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"rollback_scenarios","step":"complete","result":"pass"}'
else
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"rollback_scenarios","step":"complete","result":"fail"}'
    echo "=== [$SCRIPT_NAME] RESULT: FAIL ==="
    exit 1
fi

echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"frankensqlite_rollback","step":"finish","result":"pass"}'
echo "=== [$SCRIPT_NAME] RESULT: PASS ==="
