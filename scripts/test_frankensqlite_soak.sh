#!/bin/bash
# E4.F1.T4: FrankenSqlite soak E2E — sustained ingest + perf SLO gates
set -euo pipefail
SCRIPT_NAME=$(basename "$0")
LOG_DIR="test_results"
LOG_FILE="${LOG_DIR}/${SCRIPT_NAME%.sh}_$(date +%Y%m%d_%H%M%S).log"
mkdir -p "$LOG_DIR"

exec > >(tee -a "$LOG_FILE") 2>&1

echo "=== [$SCRIPT_NAME] Starting at $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"frankensqlite_soak","step":"start","result":"running"}'

# Run performance and soak tests
echo "--- Running soak/perf tests ---"
if cargo test -p frankenterm-core --test frankensqlite_perf_tests 2>&1; then
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"soak_perf","step":"complete","result":"pass"}'
else
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"soak_perf","step":"complete","result":"fail"}'
    echo "=== [$SCRIPT_NAME] RESULT: FAIL ==="
    exit 1
fi

# Run logging/observability tests
echo "--- Running logging tests ---"
if cargo test -p frankenterm-core --test frankensqlite_logging_tests 2>&1; then
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"logging_tests","step":"complete","result":"pass"}'
else
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"logging_tests","step":"complete","result":"fail"}'
    echo "=== [$SCRIPT_NAME] RESULT: FAIL ==="
    exit 1
fi

echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","test_name":"frankensqlite_soak","step":"finish","result":"pass"}'
echo "=== [$SCRIPT_NAME] RESULT: PASS ==="
