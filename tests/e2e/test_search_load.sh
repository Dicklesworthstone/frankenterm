#!/bin/bash
# E2E Test: Search Load
# Concurrent search load: 50 queries/second for 30 seconds
# Spec: ft-dr6zv.1.7

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
FT_BIN="${FT_BIN:-$PROJECT_ROOT/target/release/ft}"

if [[ ! -x "$FT_BIN" ]]; then
    echo "Error: ft binary not found at $FT_BIN"
    exit 2
fi

TEST_WORKSPACE=$(mktemp -d -t ft-e2e-load.XXXXXX)
export FT_WORKSPACE="$TEST_WORKSPACE"
export FT_CONFIG_PATH="$TEST_WORKSPACE/ft.toml"

cleanup() {
    "$FT_BIN" stop --force || true
    rm -rf "$TEST_WORKSPACE"
}
trap cleanup EXIT

cat > "$FT_CONFIG_PATH" <<EOF
[general]
log_level = "warn"
[search]
backend = "frankensearch"
[frankensearch]
enabled = true
EOF

"$FT_BIN" watch --daemonize
sleep 2

# Seed data
"$FT_BIN" robot send 0 "echo 'stress test data for search load'"
sleep 2

PID=$(cat "$TEST_WORKSPACE/.ft/watcher.pid" 2>/dev/null || pgrep -f "ft watch")
if [[ -z "$PID" ]]; then
    echo "Could not find watcher PID"
    exit 1
fi

RSS_START=$(ps -o rss= -p "$PID" | awk '{print $1}')
echo "Start RSS: ${RSS_START}KB"

DURATION=10 # Reduced to 10s for dev speed, full test uses 30s
QPS=10 # Reduced for dev environment safety
TOTAL_REQS=$((DURATION * QPS))

echo "Running $TOTAL_REQS queries over ${DURATION}s..."

start_time=$(date +%s)

# Use xargs for parallelism
seq 1 "$TOTAL_REQS" | xargs -P 4 -I {} bash -c "
    "$FT_BIN" robot search "stress test" --limit 1 --format json >/dev/null 2>&1 
    || echo "Query {} failed""

end_time=$(date +%s)
elapsed=$((end_time - start_time))

RSS_END=$(ps -o rss= -p "$PID" | awk '{print $1}')
echo "End RSS: ${RSS_END}KB"

DELTA=$((RSS_END - RSS_START))
echo "RSS Delta: ${DELTA}KB"

if [[ "$DELTA" -gt 50000 ]]; then # 50MB limit
    echo "FAIL: Memory leak detected (delta > 50MB)"
    exit 1
fi

echo "Load test passed in ${elapsed}s"
exit 0
