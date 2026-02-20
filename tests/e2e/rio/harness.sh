#!/usr/bin/env bash
# Shared e2e test harness for Rio validation scenarios.
# Bead: ft-34sko.8
#
# Sources this file in each test_*.sh to get:
# - JSONL logging helpers
# - Artifact directory management
# - Assertion helpers
# - Run ID generation

set -euo pipefail

# ── Defaults ────────────────────────────────────────────────────
HARNESS_VERSION="1.0.0"
RUN_ID="${RUN_ID:-$(date +%Y%m%d_%H%M%S)_$$}"
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")"/../../.. && pwd)"
ARTIFACT_BASE="${PROJECT_ROOT}/e2e-artifacts/rio"
FIXTURES_BASE="${PROJECT_ROOT}/fixtures/rio"
PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0

# ── Argument parsing ────────────────────────────────────────────
parse_harness_args() {
    QUICK_MODE=0
    VERBOSE=0
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --fixtures)  FIXTURES_DIR="$2"; shift 2 ;;
            --run-id)    RUN_ID="$2"; shift 2 ;;
            --verbose)   VERBOSE=1; shift ;;
            --quick)     QUICK_MODE=1; shift ;;
            *)           shift ;;
        esac
    done
}

# ── Directory setup ─────────────────────────────────────────────
setup_artifact_dir() {
    local scenario="$1"
    ARTIFACT_DIR="${ARTIFACT_BASE}/${scenario}/${RUN_ID}"
    mkdir -p "${ARTIFACT_DIR}"
    echo "${ARTIFACT_DIR}"
}

# ── JSONL logging ───────────────────────────────────────────────
# Emits structured JSONL per the validation matrix contract:
#   run_id, scenario_id, pane_id, window_id, phase, decision, elapsed_ms, error_code, outcome
# Plus scenario-specific fields passed as extra key=value pairs.

log_jsonl() {
    local output_file="$1"
    local scenario_id="$2"
    local phase="$3"
    local outcome="$4"
    shift 4

    local ts
    ts=$(date -u +%Y-%m-%dT%H:%M:%S.000Z)
    local elapsed_ms="${ELAPSED_MS:-0}"
    local pane_id="${PANE_ID:-null}"
    local window_id="${WINDOW_ID:-null}"
    local error_code="${ERROR_CODE:-null}"
    local decision="${DECISION:-null}"

    # Build base JSON
    local json
    json=$(printf '{"run_id":"%s","scenario_id":"%s","timestamp":"%s","pane_id":%s,"window_id":%s,"phase":"%s","decision":%s,"elapsed_ms":%s,"error_code":%s,"outcome":"%s"' \
        "$RUN_ID" "$scenario_id" "$ts" "$pane_id" "$window_id" "$phase" "$decision" "$elapsed_ms" "$error_code" "$outcome")

    # Append scenario-specific fields
    for kv in "$@"; do
        local key="${kv%%=*}"
        local val="${kv#*=}"
        # Check if value is numeric or boolean
        if [[ "$val" =~ ^[0-9]+(\.[0-9]+)?$ ]] || [[ "$val" == "true" ]] || [[ "$val" == "false" ]]; then
            json="${json},\"${key}\":${val}"
        else
            json="${json},\"${key}\":\"${val}\""
        fi
    done

    json="${json}}"
    echo "$json" >> "$output_file"
}

# ── Summary generation ──────────────────────────────────────────
write_summary() {
    local output_file="$1"
    local scenario="$2"
    local total=$((PASS_COUNT + FAIL_COUNT + SKIP_COUNT))

    cat > "$output_file" <<SUMMARY_EOF
{
  "run_id": "${RUN_ID}",
  "scenario": "${scenario}",
  "harness_version": "${HARNESS_VERSION}",
  "timestamp": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "results": {
    "total": ${total},
    "passed": ${PASS_COUNT},
    "failed": ${FAIL_COUNT},
    "skipped": ${SKIP_COUNT}
  },
  "verdict": "$([ $FAIL_COUNT -eq 0 ] && echo "PASS" || echo "FAIL")"
}
SUMMARY_EOF
}

# ── Assertion helpers ───────────────────────────────────────────
assert_eq() {
    local desc="$1" expected="$2" actual="$3"
    if [[ "$expected" == "$actual" ]]; then
        PASS_COUNT=$((PASS_COUNT + 1))
        echo "  PASS: ${desc}"
    else
        FAIL_COUNT=$((FAIL_COUNT + 1))
        echo "  FAIL: ${desc} (expected=${expected}, actual=${actual})"
    fi
}

assert_ge() {
    local desc="$1" expected="$2" actual="$3"
    if [[ "$actual" -ge "$expected" ]]; then
        PASS_COUNT=$((PASS_COUNT + 1))
        echo "  PASS: ${desc}"
    else
        FAIL_COUNT=$((FAIL_COUNT + 1))
        echo "  FAIL: ${desc} (expected>=${expected}, actual=${actual})"
    fi
}

assert_le() {
    local desc="$1" expected="$2" actual="$3"
    if [[ "$actual" -le "$expected" ]]; then
        PASS_COUNT=$((PASS_COUNT + 1))
        echo "  PASS: ${desc}"
    else
        FAIL_COUNT=$((FAIL_COUNT + 1))
        echo "  FAIL: ${desc} (expected<=${expected}, actual=${actual})"
    fi
}

assert_file_exists() {
    local desc="$1" path="$2"
    if [[ -f "$path" ]]; then
        PASS_COUNT=$((PASS_COUNT + 1))
        echo "  PASS: ${desc}"
    else
        FAIL_COUNT=$((FAIL_COUNT + 1))
        echo "  FAIL: ${desc} (file not found: ${path})"
    fi
}

assert_jsonl_field() {
    local desc="$1" file="$2" field="$3" expected="$4"
    local actual
    actual=$(head -1 "$file" 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('$field',''))" 2>/dev/null || echo "PARSE_ERROR")
    assert_eq "$desc" "$expected" "$actual"
}

assert_jsonl_count() {
    local desc="$1" file="$2" min_count="$3"
    local actual
    actual=$(wc -l < "$file" 2>/dev/null | tr -d ' ')
    assert_ge "$desc" "$min_count" "$actual"
}

# ── FrankenTerm binary helpers ──────────────────────────────────
ft_bin() {
    local bin="${PROJECT_ROOT}/target/release/ft"
    if [[ ! -x "$bin" ]]; then
        bin="${PROJECT_ROOT}/target/debug/ft"
    fi
    if [[ ! -x "$bin" ]]; then
        # Try CARGO_TARGET_DIR locations
        for dir in /tmp/ft-target "${CARGO_TARGET_DIR:-}"; do
            if [[ -n "$dir" && -x "${dir}/release/ft" ]]; then
                bin="${dir}/release/ft"
                break
            elif [[ -n "$dir" && -x "${dir}/debug/ft" ]]; then
                bin="${dir}/debug/ft"
                break
            fi
        done
    fi
    echo "$bin"
}

CARGO_TEST_SKIPPED=0

cargo_test() {
    local filter="${1:-}"
    CARGO_TEST_SKIPPED=0
    # In quick mode, skip cargo tests entirely
    if [[ "${QUICK_MODE:-0}" -eq 1 ]]; then
        CARGO_TEST_SKIPPED=1
        SKIP_COUNT=$((SKIP_COUNT + 1))
        echo "  SKIP: cargo test (quick mode, filter=$filter)"
        return 0
    fi
    local timeout_sec="${CARGO_TEST_TIMEOUT:-120}"
    if command -v rch &>/dev/null; then
        timeout "$timeout_sec" rch exec -- cargo test -p frankenterm-core ${filter:+-- "$filter"} 2>&1
    else
        timeout "$timeout_sec" env CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/ft-target}" \
            cargo test -p frankenterm-core ${filter:+-- "$filter"} 2>&1
    fi
}

# ── Scenario runner ─────────────────────────────────────────────
scenario_header() {
    local name="$1"
    echo "================================================================"
    echo "Rio E2E Scenario: ${name}"
    echo "Run ID: ${RUN_ID}"
    echo "Timestamp: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "================================================================"
}

scenario_footer() {
    local name="$1"
    echo "----------------------------------------------------------------"
    echo "Results: ${PASS_COUNT} passed, ${FAIL_COUNT} failed, ${SKIP_COUNT} skipped"
    echo "Verdict: $([ $FAIL_COUNT -eq 0 ] && echo "PASS" || echo "FAIL")"
    echo "================================================================"
    return $FAIL_COUNT
}

skip_if_no_ft() {
    local bin
    bin=$(ft_bin)
    if [[ ! -x "$bin" ]]; then
        echo "SKIP: ft binary not found (build with cargo build -p frankenterm)"
        SKIP_COUNT=$((SKIP_COUNT + 1))
        return 1
    fi
    return 0
}
