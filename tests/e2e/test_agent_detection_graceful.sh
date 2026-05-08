#!/usr/bin/env bash
# test_agent_detection_graceful.sh — E2E: Graceful degradation with agent-detection feature off (ft-dr6zv.2.5)
#
# Validates:
# - Integration tests compile and pass WITHOUT the agent-detection feature
# - Correlator still works for pattern/title/process detection
# - Feature flag check returns false when disabled
# - No panics or crashes
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOG_DIR="${SCRIPT_DIR}/logs"
RUN_ID="$(date +%Y%m%dT%H%M%S)"
LOG_FILE="${LOG_DIR}/test_agent_detection_graceful_${RUN_ID}.jsonl"
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-agent-detection-graceful-${RUN_ID}"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
    CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
    CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR
PASS=0
FAIL=0

mkdir -p "$LOG_DIR"

json_escape() {
    local value="$1"
    value=${value//\\/\\\\}
    value=${value//\"/\\\"}
    value=${value//$'\n'/\\n}
    value=${value//$'\r'/\\r}
    value=${value//$'\t'/\\t}
    printf '%s' "$value"
}

log_json() {
    local test_name="$1" phase="$2" result="$3" detail="$4"
    local ts_ms
    ts_ms=$(python3 -c "import time; print(int(time.time()*1000))" 2>/dev/null || date +%s000)
    printf '{"test_name":"%s","phase":"%s","timestamp_ms":%s,"result":"%s","detail":"%s"}\n' \
        "$(json_escape "$test_name")" \
        "$(json_escape "$phase")" \
        "$ts_ms" \
        "$(json_escape "$result")" \
        "$(json_escape "$detail")" >> "$LOG_FILE"
}

if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required for shared rch metadata" >&2
    exit 1
fi

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${SCRIPT_DIR}/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "agent_detection_graceful"
ensure_rch_ready

run_cargo_test() {
    local step="$1"
    shift
    local output_file="${LOG_DIR}/test_agent_detection_graceful_${RUN_ID}.${step}.stdout.log"
    local status

    if run_rch_cargo_logged "${output_file}" \
        env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test "$@"; then
        status=0
    else
        status=$?
    fi

    cat "${output_file}"
    return "${status}"
}

# ---- Test 1: Integration tests pass without agent-detection feature ----
echo "=== Test 1: Integration tests without agent-detection feature ==="
log_json "integration_no_feature" "detect" "running" "Building without agent-detection feature"

# The integration_agent_detection tests should work with --no-default-features
# because they test the correlator (not filesystem detection)
if run_cargo_test integration_no_feature \
    -p frankenterm-core integration_agent_detection --no-default-features -- --nocapture; then
    log_json "integration_no_feature" "assert" "pass" "Integration tests pass without agent-detection feature"
    PASS=$((PASS + 1))
else
    log_json "integration_no_feature" "assert" "fail" "Integration tests failed without agent-detection feature"
    FAIL=$((FAIL + 1))
fi

# ---- Test 2: Enrichment tests pass without agent-detection feature ----
echo "=== Test 2: Enrichment tests without agent-detection feature ==="
log_json "enrichment_no_feature" "detect" "running" "Testing enrichment without feature flag"

if run_cargo_test enrichment_no_feature \
    -p frankenterm-core integration_agent_detection_enrichment --no-default-features -- --nocapture; then
    log_json "enrichment_no_feature" "assert" "pass" "Enrichment tests pass without agent-detection feature"
    PASS=$((PASS + 1))
else
    log_json "enrichment_no_feature" "assert" "fail" "Enrichment tests failed"
    FAIL=$((FAIL + 1))
fi

# ---- Test 3: Autoconfig tests pass without agent-detection feature ----
echo "=== Test 3: Autoconfig tests without agent-detection feature ==="
log_json "autoconfig_no_feature" "detect" "running" "Testing autoconfig without feature flag"

if run_cargo_test autoconfig_no_feature \
    -p frankenterm-core integration_agent_autoconfig --no-default-features -- --nocapture; then
    log_json "autoconfig_no_feature" "assert" "pass" "Autoconfig tests pass without agent-detection feature"
    PASS=$((PASS + 1))
else
    log_json "autoconfig_no_feature" "assert" "fail" "Autoconfig tests failed"
    FAIL=$((FAIL + 1))
fi

# ---- Test 4: Feature flag function returns correct value ----
echo "=== Test 4: Feature flag consistency ==="
log_json "feature_flag" "detect" "running" "Verifying feature flag behavior"

if run_cargo_test feature_flag \
    -p frankenterm-core filesystem_detection_available --no-default-features -- --nocapture; then
    log_json "feature_flag" "assert" "pass" "Feature flag test passes"
    PASS=$((PASS + 1))
else
    log_json "feature_flag" "assert" "fail" "Feature flag test failed"
    FAIL=$((FAIL + 1))
fi

# ---- Summary ----
echo ""
echo "=== Graceful Degradation E2E Summary ==="
echo "  Pass: $PASS"
echo "  Fail: $FAIL"
echo "  Log:  $LOG_FILE"

log_json "summary" "teardown" "$([ $FAIL -eq 0 ] && echo pass || echo fail)" "Pass=$PASS Fail=$FAIL"

[ "$FAIL" -eq 0 ]
