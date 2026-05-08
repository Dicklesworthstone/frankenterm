#!/usr/bin/env bash
# test_agent_detection.sh — E2E: Agent filesystem detection validation (ft-dr6zv.2.5)
#
# Creates mock agent installations in a temporary directory and validates
# detection accuracy: all mock-installed agents detected, others not.
# Outputs structured JSON logs for every phase.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOG_DIR="${SCRIPT_DIR}/logs"
RUN_ID="$(date +%Y%m%dT%H%M%S)"
LOG_FILE="${LOG_DIR}/test_agent_detection_${RUN_ID}.jsonl"
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-agent-detection-${RUN_ID}"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
    CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
    CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR
PASS=0
FAIL=0
SKIP=0

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
rch_init "${LOG_DIR}" "${RUN_ID}" "agent_detection"
ensure_rch_ready

run_cargo_test() {
    local step="$1"
    shift
    local output_file="${LOG_DIR}/test_agent_detection_${RUN_ID}.${step}.stdout.log"
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

# ---- Setup: Create mock agent installations in tempdir ----
TEST_HOME="$(mktemp -d)"
trap 'rm -rf "$TEST_HOME"' EXIT

log_json "setup" "setup" "pass" "Created test home at $TEST_HOME"

# All 9 known connectors
ALL_SLUGS=(claude cline codex cursor factory gemini github-copilot opencode windsurf)
INSTALLED_SLUGS=(claude codex cursor gemini)

log_json "setup" "setup" "pass" "Known agents=${#ALL_SLUGS[@]} installed=${#INSTALLED_SLUGS[@]}"

for slug in "${INSTALLED_SLUGS[@]}"; do
    agent_dir="${TEST_HOME}/.${slug}"
    mkdir -p "$agent_dir"
    echo "{\"agent\": \"${slug}\", \"version\": \"1.0.0\"}" > "$agent_dir/config.json"
done

log_json "setup" "setup" "pass" "Created fixtures for ${#INSTALLED_SLUGS[@]} agents"

# ---- Test 1: Cargo test for filesystem detection ----
# Run the agent detection integration tests with the agent-detection feature
echo "=== Test 1: Running cargo test for agent detection filesystem tests ==="
log_json "filesystem_detection_tests" "detect" "running" "Starting cargo test"

if run_cargo_test filesystem_detection_tests \
    -p frankenterm-core e2e_agent_detection_filesystem --features agent-detection --no-default-features -- --nocapture; then
    log_json "filesystem_detection_tests" "detect" "pass" "All filesystem detection tests passed"
    PASS=$((PASS + 1))
else
    log_json "filesystem_detection_tests" "detect" "fail" "Some filesystem detection tests failed"
    FAIL=$((FAIL + 1))
fi

# ---- Test 2: Cargo test for integration tests ----
echo "=== Test 2: Running cargo test for agent detection integration tests ==="
log_json "integration_tests" "detect" "running" "Starting integration tests"

if run_cargo_test integration_tests \
    -p frankenterm-core integration_agent_detection --no-default-features -- --nocapture; then
    log_json "integration_tests" "detect" "pass" "All integration tests passed"
    PASS=$((PASS + 1))
else
    log_json "integration_tests" "detect" "fail" "Some integration tests failed"
    FAIL=$((FAIL + 1))
fi

# ---- Test 3: Cargo test for autoconfig integration tests ----
echo "=== Test 3: Running cargo test for autoconfig integration tests ==="
log_json "autoconfig_tests" "detect" "running" "Starting autoconfig tests"

if run_cargo_test autoconfig_tests \
    -p frankenterm-core integration_agent_autoconfig --no-default-features -- --nocapture; then
    log_json "autoconfig_tests" "detect" "pass" "All autoconfig tests passed"
    PASS=$((PASS + 1))
else
    log_json "autoconfig_tests" "detect" "fail" "Some autoconfig tests failed"
    FAIL=$((FAIL + 1))
fi

# ---- Test 4: Cargo test for enrichment integration tests ----
echo "=== Test 4: Running cargo test for detection enrichment tests ==="
log_json "enrichment_tests" "detect" "running" "Starting enrichment tests"

if run_cargo_test enrichment_tests \
    -p frankenterm-core integration_agent_detection_enrichment --no-default-features -- --nocapture; then
    log_json "enrichment_tests" "detect" "pass" "All enrichment tests passed"
    PASS=$((PASS + 1))
else
    log_json "enrichment_tests" "detect" "fail" "Some enrichment tests failed"
    FAIL=$((FAIL + 1))
fi

# ---- Summary ----
echo ""
echo "=== Agent Detection E2E Summary ==="
echo "  Pass: $PASS"
echo "  Fail: $FAIL"
echo "  Skip: $SKIP"
echo "  Log:  $LOG_FILE"

log_json "summary" "teardown" "$([ $FAIL -eq 0 ] && echo pass || echo fail)" "Pass=$PASS Fail=$FAIL Skip=$SKIP"

[ "$FAIL" -eq 0 ]
