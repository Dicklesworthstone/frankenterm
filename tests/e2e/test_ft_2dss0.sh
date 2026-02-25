#!/usr/bin/env bash
# ────────────────────────────────────────────────────────────────────────────
# E2E Harness: ft-2dss0 — Rate limit detection and quota-aware scheduling
#
# Validates that:
# 1. rate_limit_tracker module compiles and passes all unit tests
# 2. Pattern rules for rate_limit.detected are present and functional
# 3. Fixture corpus tests pass (no cross-rule interference)
# 4. Property tests pass
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

# ── Structured log helper ──────────────────────────────────────────────────
log_event() {
    local component="$1"
    local decision_path="$2"
    local input_summary="$3"
    local outcome="$4"
    local reason_code="${5:-nominal}"
    local error_code="${6:-none}"
    printf '{"timestamp":"%s","component":"%s","scenario_id":"%s","correlation_id":"%s-%s","decision_path":"%s","input_summary":"%s","outcome":"%s","reason_code":"%s","error_code":"%s"}\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        "$component" \
        "$SCENARIO_ID" \
        "$SCENARIO_ID" "$TIMESTAMP" \
        "$decision_path" \
        "$input_summary" \
        "$outcome" \
        "$reason_code" \
        "$error_code" >> "$LOG_FILE"
}

# ── Preflight ──────────────────────────────────────────────────────────────
log_event "preflight" "startup" "checking_rch" "started"

if ! command -v rch &>/dev/null; then
    log_event "preflight" "startup" "rch_binary" "skip" "rch_not_found" "RCH-E001"
    echo "WARN: rch not found, falling back to local cargo"
    CARGO_CMD="cargo"
else
    if rch workers probe --all 2>&1 | grep -q '✓'; then
        CARGO_CMD="rch exec -- cargo"
        log_event "preflight" "startup" "rch_workers" "available" "rch_ok"
    else
        log_event "preflight" "startup" "rch_workers" "unavailable" "rch_workers_down" "RCH-E100"
        echo "WARN: rch workers unavailable, falling back to local cargo"
        CARGO_CMD="cargo"
    fi
fi

cd "$PROJECT_ROOT"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/rch-e2e-ft2dss0}"
export CARGO_TARGET_DIR

log_event "preflight" "startup" "cargo_target=$CARGO_TARGET_DIR" "ready"

# ── Test matrix ────────────────────────────────────────────────────────────
TOTAL_STEPS=5
PASSED=0
FAILED=0

echo "Running ft-2dss0 rate limit detection validation..."
log_event "harness" "nominal_path" "steps=$TOTAL_STEPS" "started"

# ── Step 1: Compile check ─────────────────────────────────────────────────
echo "[1/$TOTAL_STEPS] Compile check (frankenterm-core)..."
if $CARGO_CMD check -p frankenterm-core --lib 2>>"$LOG_FILE"; then
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
if $CARGO_CMD test -p frankenterm-core --lib -- rate_limit_tracker::tests 2>&1 | tee "$TEST_OUTPUT"; then
    test_count=$(grep -c "test result: ok" "$TEST_OUTPUT" 2>/dev/null || echo "0")
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
if $CARGO_CMD test -p frankenterm-core --lib -- patterns::tests::fixture 2>&1 | tee "$TEST_OUTPUT"; then
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
if $CARGO_CMD test -p frankenterm-core --lib -- patterns::tests 2>&1 | tee "$TEST_OUTPUT"; then
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
if $CARGO_CMD test -p frankenterm-core --test proptest_rate_limit_tracker 2>&1 | tee "$TEST_OUTPUT"; then
    test_count=$(grep "test result:" "$TEST_OUTPUT" | head -1 | grep -o '[0-9]* passed' || echo "? passed")
    log_event "proptest" "nominal_path" "rate_limit_proptests" "pass" "tests=$test_count"
    echo "  ✓ Property tests passed"
    PASSED=$((PASSED + 1))
else
    log_event "proptest" "failure_injection_path" "rate_limit_proptests" "fail" "proptest_failure" "TEST-E004"
    echo "  ✗ Property tests FAILED"
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
