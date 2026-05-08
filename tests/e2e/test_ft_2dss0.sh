#!/usr/bin/env bash
# ────────────────────────────────────────────────────────────────────────────
# E2E Harness: ft-2dss0 — Rate limit detection and quota-aware scheduling
#
# Validates that:
# 1. rate_limit_tracker module compiles and passes all unit tests
# 2. Pattern rules for rate_limit.detected are present and functional
# 3. Fixture corpus tests pass (no cross-rule interference)
# 4. Property tests pass (rate_limit_tracker, cost_tracker, quota_gate)
# 5. cost_tracker and quota_gate modules compile and pass unit tests
# 6. Integration tests for the full quota gate pipeline pass
#
# Execution: rch exec -- bash tests/e2e/test_ft_2dss0.sh
# ────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SCENARIO_ID="ft-2dss0"
LOG_DIR="$SCRIPT_DIR/logs"
TIMESTAMP="$(date -u +%Y%m%d_%H%M%S)"
LOG_FILE="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}.jsonl"

mkdir -p "$LOG_DIR"

DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-ft2dss0"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
    CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
    CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

GUARD_LIB="$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"

# ── Structured log helper ──────────────────────────────────────────────────
json_escape() {
    local value="$1"
    value=${value//\\/\\\\}
    value=${value//\"/\\\"}
    value=${value//$'\n'/\\n}
    value=${value//$'\r'/\\r}
    value=${value//$'\t'/\\t}
    printf '%s' "$value"
}

log_event() {
    local component="$1"
    local decision_path="$2"
    local input_summary="$3"
    local outcome="$4"
    local reason_code="${5:-nominal}"
    local error_code="${6:-none}"
    printf '{"timestamp":"%s","component":"%s","scenario_id":"%s","correlation_id":"%s-%s","decision_path":"%s","input_summary":"%s","outcome":"%s","reason_code":"%s","error_code":"%s"}\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        "$(json_escape "$component")" \
        "$(json_escape "$SCENARIO_ID")" \
        "$(json_escape "$SCENARIO_ID")" "$(json_escape "$TIMESTAMP")" \
        "$(json_escape "$decision_path")" \
        "$(json_escape "$input_summary")" \
        "$(json_escape "$outcome")" \
        "$(json_escape "$reason_code")" \
        "$(json_escape "$error_code")" >> "$LOG_FILE"
}

run_cargo_step() {
    shift
    local output_file="$1"
    shift
    run_rch_cargo_logged "${output_file}" env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo "$@"
}

count_matches() {
    local pattern="$1"
    local file="$2"
    local count
    count=$(grep -c -- "$pattern" "$file") || {
        local rc=$?
        if [[ ${rc} -eq 1 ]]; then
            count=0
        else
            return "${rc}"
        fi
    }
    printf '%s\n' "$count"
}

# ── Preflight ──────────────────────────────────────────────────────────────
log_event "preflight" "startup" "checking_rch" "started"

if ! command -v jq >/dev/null 2>&1; then
    log_event "preflight" "startup" "jq_missing" "fail" "jq_required_missing" "JQ-E001"
    echo "jq is required for rch metadata and structured logging" >&2
    exit 1
fi

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${GUARD_LIB}"
rch_init "${LOG_DIR}" "${TIMESTAMP}" "2dss0"
ensure_rch_ready

cd "$PROJECT_ROOT"

log_event "preflight" "startup" "cargo_target=$CARGO_TARGET_DIR" "ready"

# ── Test matrix ────────────────────────────────────────────────────────────
TOTAL_STEPS=9
PASSED=0
FAILED=0

echo "Running ft-2dss0 rate limit detection validation..."
log_event "harness" "nominal_path" "steps=$TOTAL_STEPS" "started"

# ── Step 1: Compile check ─────────────────────────────────────────────────
echo "[1/$TOTAL_STEPS] Compile check (frankenterm-core)..."
COMPILE_OUTPUT="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_compile.log"
if run_cargo_step "rate_limit_compile" "$COMPILE_OUTPUT" check -p frankenterm-core --lib; then
    log_event "compile" "nominal_path" "cargo_check" "pass"
    echo "  ✓ Compile check passed"
    PASSED=$((PASSED + 1))
else
    log_event "compile" "failure_injection_path" "cargo_check" "fail" "compile_error" "CARGO-E001"
    echo "  ✗ Compile check FAILED"
    echo "Scenario: $SCENARIO_ID"
    echo "Logs: $LOG_FILE"
    exit 1
fi

# ── Step 2: rate_limit_tracker unit tests ──────────────────────────────────
echo "[2/$TOTAL_STEPS] Testing rate_limit_tracker module..."
TEST_OUTPUT="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_unit.log"
if run_cargo_step "rate_limit_tracker_tests" "$TEST_OUTPUT" test -p frankenterm-core --lib -- rate_limit_tracker::tests; then
    test_count=$(count_matches "test result: ok" "$TEST_OUTPUT")
    log_event "unit_tests" "nominal_path" "rate_limit_tracker" "pass" "tests_ok=$test_count"
    echo "  ✓ rate_limit_tracker tests passed"
    PASSED=$((PASSED + 1))
else
    log_event "unit_tests" "failure_injection_path" "rate_limit_tracker" "fail" "test_failure" "TEST-E001"
    echo "  ✗ rate_limit_tracker tests FAILED"
    FAILED=$((FAILED + 1))
fi

# ── Step 3: Pattern fixture tests ──────────────────────────────────────────
echo "[3/$TOTAL_STEPS] Testing pattern fixtures..."
TEST_OUTPUT="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_fixtures.log"
if run_cargo_step "pattern_fixture_tests" "$TEST_OUTPUT" test -p frankenterm-core --lib -- patterns::tests::fixture; then
    log_event "fixture_tests" "nominal_path" "pattern_fixtures" "pass"
    echo "  ✓ Pattern fixture tests passed"
    PASSED=$((PASSED + 1))
else
    log_event "fixture_tests" "failure_injection_path" "pattern_fixtures" "fail" "fixture_failure" "TEST-E002"
    echo "  ✗ Pattern fixture tests FAILED"
    FAILED=$((FAILED + 1))
fi

# ── Step 4: Pattern rate limit detection tests ─────────────────────────────
echo "[4/$TOTAL_STEPS] Testing rate limit pattern detection..."
TEST_OUTPUT="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_patterns.log"
if run_cargo_step "rate_limit_pattern_tests" "$TEST_OUTPUT" test -p frankenterm-core --lib -- patterns::tests; then
    test_count=$(grep "test result:" "$TEST_OUTPUT" | head -1 | grep -o '[0-9]* passed' || echo "? passed")
    log_event "pattern_tests" "nominal_path" "rate_limit_patterns" "pass" "tests=$test_count"
    echo "  ✓ Pattern detection tests passed"
    PASSED=$((PASSED + 1))
else
    log_event "pattern_tests" "failure_injection_path" "rate_limit_patterns" "fail" "pattern_failure" "TEST-E003"
    echo "  ✗ Pattern detection tests FAILED"
    FAILED=$((FAILED + 1))
fi

# ── Step 5: Property tests ─────────────────────────────────────────────────
echo "[5/$TOTAL_STEPS] Running property tests..."
TEST_OUTPUT="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_proptest.log"
if run_cargo_step "rate_limit_proptests" "$TEST_OUTPUT" test -p frankenterm-core --test proptest_rate_limit_tracker; then
    test_count=$(grep "test result:" "$TEST_OUTPUT" | head -1 | grep -o '[0-9]* passed' || echo "? passed")
    log_event "proptest" "nominal_path" "rate_limit_proptests" "pass" "tests=$test_count"
    echo "  ✓ Property tests passed"
    PASSED=$((PASSED + 1))
else
    log_event "proptest" "failure_injection_path" "rate_limit_proptests" "fail" "proptest_failure" "TEST-E004"
    echo "  ✗ Property tests FAILED"
    FAILED=$((FAILED + 1))
fi

# ── Step 6: cost_tracker unit tests ───────────────────────────────────
echo "[6/$TOTAL_STEPS] Testing cost_tracker module..."
TEST_OUTPUT="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_cost_tracker.log"
if run_cargo_step "cost_tracker_tests" "$TEST_OUTPUT" test -p frankenterm-core --lib -- cost_tracker::tests; then
    log_event "unit_tests" "nominal_path" "cost_tracker" "pass"
    echo "  ✓ cost_tracker tests passed"
    PASSED=$((PASSED + 1))
else
    log_event "unit_tests" "failure_injection_path" "cost_tracker" "fail" "test_failure" "TEST-E005"
    echo "  ✗ cost_tracker tests FAILED"
    FAILED=$((FAILED + 1))
fi

# ── Step 7: quota_gate unit tests ────────────────────────────────────
echo "[7/$TOTAL_STEPS] Testing quota_gate module..."
TEST_OUTPUT="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_quota_gate.log"
if run_cargo_step "quota_gate_tests" "$TEST_OUTPUT" test -p frankenterm-core --lib -- quota_gate::tests; then
    log_event "unit_tests" "nominal_path" "quota_gate" "pass"
    echo "  ✓ quota_gate tests passed"
    PASSED=$((PASSED + 1))
else
    log_event "unit_tests" "failure_injection_path" "quota_gate" "fail" "test_failure" "TEST-E006"
    echo "  ✗ quota_gate tests FAILED"
    FAILED=$((FAILED + 1))
fi

# ── Step 8: cost_tracker + quota_gate property tests ─────────────────
echo "[8/$TOTAL_STEPS] Running cost_tracker + quota_gate property tests..."
TEST_OUTPUT="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_proptest_cq.log"
if run_cargo_step "cost_quota_proptests" "$TEST_OUTPUT" test -p frankenterm-core --test proptest_cost_tracker --test proptest_quota_gate; then
    log_event "proptest" "nominal_path" "cost_quota_proptests" "pass"
    echo "  ✓ cost_tracker + quota_gate property tests passed"
    PASSED=$((PASSED + 1))
else
    log_event "proptest" "failure_injection_path" "cost_quota_proptests" "fail" "proptest_failure" "TEST-E007"
    echo "  ✗ cost_tracker + quota_gate property tests FAILED"
    FAILED=$((FAILED + 1))
fi

# ── Step 9: quota_gate integration tests ─────────────────────────────
echo "[9/$TOTAL_STEPS] Running quota_gate integration tests..."
TEST_OUTPUT="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_integration.log"
if run_cargo_step "quota_gate_integration" "$TEST_OUTPUT" test -p frankenterm-core --test quota_gate_integration; then
    log_event "integration" "nominal_path" "quota_gate_integration" "pass"
    echo "  ✓ quota_gate integration tests passed"
    PASSED=$((PASSED + 1))
else
    log_event "integration" "failure_injection_path" "quota_gate_integration" "fail" "integration_failure" "TEST-E008"
    echo "  ✗ quota_gate integration tests FAILED"
    FAILED=$((FAILED + 1))
fi

# ── Summary ────────────────────────────────────────────────────────────────
TOTAL=$((PASSED + FAILED))
echo ""
echo "═══════════════════════════════════════════"
echo "  Scenario: $SCENARIO_ID"
echo "  Passed: $PASSED / $TOTAL"
echo "  Failed: $FAILED / $TOTAL"
echo "  Logs: $LOG_FILE"
echo "═══════════════════════════════════════════"

log_event "summary" "completed" "passed=$PASSED,failed=$FAILED,total=$TOTAL" \
    "$([ "$FAILED" -eq 0 ] && echo 'pass' || echo 'partial_fail')"

[ "$FAILED" -eq 0 ] && exit 0 || exit 1
