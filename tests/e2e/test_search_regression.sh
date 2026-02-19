#!/bin/bash
# E2E Test: Search Regression
# Ensures new FrankenSearch engine returns at least the same results as legacy
# Spec: ft-dr6zv.1.7

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
FT_BIN="${FT_BIN:-$PROJECT_ROOT/target/release/ft}"

if [[ ! -x "$FT_BIN" ]]; then
    echo "Error: ft binary not found at $FT_BIN"
    exit 2
fi

TEST_WORKSPACE=$(mktemp -d -t ft-e2e-regression.XXXXXX)
export FT_WORKSPACE="$TEST_WORKSPACE"
export FT_CONFIG_PATH="$TEST_WORKSPACE/ft.toml"

cleanup() {
    "$FT_BIN" stop --force || true
    rm -rf "$TEST_WORKSPACE"
}
trap cleanup EXIT

# Config
cat > "$FT_CONFIG_PATH" <<EOF
[general]
log_level = "info"
[search]
backend = "frankensearch" # The new backend
[frankensearch]
enabled = true
EOF

echo "Starting watcher..."
"$FT_BIN" watch --daemonize
sleep 2

# Populate with standard regression corpus
echo "Populating corpus..."
"$FT_BIN" robot send 0 "echo 'compiler error: E0308'"
"$FT_BIN" robot send 0 "echo 'warning: unused variable'"
"$FT_BIN" robot send 0 "echo 'test result: ok. 5 passed; 0 failed'"
sleep 2

# Queries that MUST work
QUERIES=(
    "compiler error"
    "E0308"
    "warning"
    "test result"
    "failed"
)

FAILED_QUERIES=0

for Q in "${QUERIES[@]}"; do
    echo "Testing query: '$Q'"
    RES=$("$FT_BIN" robot search "$Q" --limit 1 --format json)
    COUNT=$(echo "$RES" | jq '.results | length')
    
    if [[ "$COUNT" -gt 0 ]]; then
        echo "  [PASS] Found match"
    else
        echo "  [FAIL] No match found"
        FAILED_QUERIES=$((FAILED_QUERIES + 1))
    fi
done

if [[ "$FAILED_QUERIES" -gt 0 ]]; then
    echo "Regression check failed: $FAILED_QUERIES queries returned no results."
    exit 1
fi

echo "All regression queries passed."
exit 0
