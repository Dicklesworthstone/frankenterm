#!/bin/bash
# E2E Test: FrankenSearch Integration
# Tests the full search pipeline: indexing -> query -> progressive results
# Spec: ft-dr6zv.1.7

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
FT_BIN="${FT_BIN:-$PROJECT_ROOT/target/release/ft}"

# Check prerequisites
if [[ ! -x "$FT_BIN" ]]; then
    echo "Error: ft binary not found at $FT_BIN"
    exit 2
fi

if ! command -v jq &>/dev/null; then
    echo "Error: jq is required"
    exit 2
fi

# Setup test workspace
TEST_WORKSPACE=$(mktemp -d -t ft-e2e-search.XXXXXX)
export FT_WORKSPACE="$TEST_WORKSPACE"
export FT_CONFIG_PATH="$TEST_WORKSPACE/ft.toml"

echo "Using test workspace: $TEST_WORKSPACE"

cleanup() {
    echo "Stopping watcher..."
    "$FT_BIN" stop --force || true
    echo "Cleaning up workspace..."
    rm -rf "$TEST_WORKSPACE"
}
trap cleanup EXIT

# 1. Initialize config with frankensearch enabled
cat > "$FT_CONFIG_PATH" <<EOF
[general]
log_level = "debug"

[search]
backend = "frankensearch"
index_path = "$TEST_WORKSPACE/search_index"

[frankensearch]
enabled = true
fast_only = false
EOF

# 2. Start watcher
echo "Starting watcher..."
"$FT_BIN" watch --daemonize
sleep 2

# 3. Generate test data (populate a pane with known content)
echo "Generating test data..."
"$FT_BIN" robot send 0 "echo 'The quick brown fox jumps over the lazy dog'"
"$FT_BIN" robot send 0 "echo 'unique_string_alpha_beta_gamma'"
"$FT_BIN" robot send 0 "echo 'Another distinct line for searching'"
sleep 2 # Allow indexing

# 4. Check index stats
echo "Checking index stats..."
STATS=$("$FT_BIN" robot search-index stats --format json)
DOC_COUNT=$(echo "$STATS" | jq '.doc_count')
echo "Index doc count: $DOC_COUNT"

if [[ "$DOC_COUNT" -lt 1 ]]; then
    echo "FAIL: Index is empty"
    exit 1
fi

# 5. Run search queries

# Case A: Lexical match (Exact string)
echo "Testing lexical match..."
RESULTS=$("$FT_BIN" robot search "unique_string_alpha_beta_gamma" --format json)
COUNT=$(echo "$RESULTS" | jq '.results | length')
if [[ "$COUNT" -eq 0 ]]; then
    echo "FAIL: Lexical match found 0 results"
    exit 1
fi
MATCH=$(echo "$RESULTS" | jq -r '.results[0].content')
if [[ "$MATCH" != *"unique_string_alpha_beta_gamma"* ]]; then
    echo "FAIL: Content mismatch. Got: $MATCH"
    exit 1
fi
echo "PASS: Lexical match"

# Case B: Semantic match (Conceptual) - Note: depends on embedding model
# For now, we test partial match or fuzzy logic if supported, or just basic search
echo "Testing basic search..."
RESULTS_FOX=$("$FT_BIN" robot search "quick fox" --format json)
COUNT_FOX=$(echo "$RESULTS_FOX" | jq '.results | length')
if [[ "$COUNT_FOX" -eq 0 ]]; then
    echo "FAIL: 'quick fox' found 0 results"
    exit 1
fi
echo "PASS: Basic search"

# Case C: Progressive delivery (JSONL)
echo "Testing progressive delivery..."
"$FT_BIN" robot search "brown" --format jsonl > "$TEST_WORKSPACE/stream.jsonl"

PHASE_INITIAL=$(grep -c '"phase":"initial"' "$TEST_WORKSPACE/stream.jsonl" || true)
# Refined phase might not trigger if fast_only is default or if dataset is too small for reranking latency
# But we expect at least initial results.

if [[ "$PHASE_INITIAL" -eq 0 ]]; then
    echo "FAIL: No initial phase marker in JSONL output"
    exit 1
fi
echo "PASS: Progressive delivery markers found"

# Case D: Explain mode
echo "Testing explain mode..."
EXPLAIN=$("$FT_BIN" robot search "lazy dog" --explain --format json)
HAS_SCORES=$(echo "$EXPLAIN" | jq '.results[0].scores | has("lexical")')
if [[ "$HAS_SCORES" != "true" ]]; then
    echo "FAIL: Explain mode missing scores"
    exit 1
fi
echo "PASS: Explain mode"

echo "All tests passed!"
exit 0
