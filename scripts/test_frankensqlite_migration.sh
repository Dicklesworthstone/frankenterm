#!/bin/bash
# E4.F1.T4: FrankenSqlite migration E2E — full M0-M5 pipeline + rollback
set -euo pipefail
SCRIPT_NAME=$(basename "$0")
LOG_DIR="test_results"
LOG_FILE="${LOG_DIR}/${SCRIPT_NAME%.sh}_$(date +%Y%m%d_%H%M%S).log"
mkdir -p "$LOG_DIR"

exec > >(tee -a "$LOG_FILE") 2>&1

echo "=== [$SCRIPT_NAME] Starting at $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"frankensqlite_migration","step":"start","result":"running"}'

# Run E2E migration tests
echo "--- Running E2E migration tests ---"
if cargo test -p frankenterm-core --test frankensqlite_e2e_tests 2>&1; then
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"e2e_migration","step":"complete","result":"pass"}'
else
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"e2e_migration","step":"complete","result":"fail"}'
    echo "=== [$SCRIPT_NAME] RESULT: FAIL ==="
    exit 1
fi

echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"frankensqlite_migration","step":"finish","result":"pass"}'
echo "=== [$SCRIPT_NAME] RESULT: PASS ==="
