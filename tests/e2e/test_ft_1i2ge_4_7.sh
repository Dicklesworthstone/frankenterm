#!/usr/bin/env bash
# test_ft_1i2ge_4_7.sh — E2E harness for ft-1i2ge.4.7
# Safety guardrail adversarial test suite and audit-log verification
#
# Validates:
#   1. All 25 adversarial tests (ADV-01 through ADV-25) pass
#   2. Safety envelope boundary enforcement
#   3. Conflict detection across all 3 types + 3 strategies
#   4. Serde roundtrip for reports, config, state, and input types
#   5. Metrics capture conflict rejections
#
# Usage: bash tests/e2e/test_ft_1i2ge_4_7.sh
# Requires: rch (falls back to local cargo if workers offline)

set -euo pipefail

SCENARIO_ID="ft-1i2ge-4-7"
COMPONENT="mission_loop::adversarial"
TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
CORRELATION_ID="${SCENARIO_ID}-$(date +%s)"
LOG_DIR="${TMPDIR:-/tmp}/ft-e2e-${SCENARIO_ID}"
mkdir -p "$LOG_DIR"

log_structured() {
    local outcome="$1" reason_code="$2" error_code="${3:-}" extra="${4:-}"
    printf '{"timestamp":"%s","component":"%s","scenario_id":"%s","correlation_id":"%s","outcome":"%s","reason_code":"%s","error_code":"%s"%s}\n' \
        "$TIMESTAMP" "$COMPONENT" "$SCENARIO_ID" "$CORRELATION_ID" \
        "$outcome" "$reason_code" "$error_code" "$extra" \
        | tee -a "$LOG_DIR/results.jsonl"
}

# ── Preflight ────────────────────────────────────────────────────────────────

# Determine cargo runner (rch or local)
CARGO_CMD="cargo"
if command -v rch &>/dev/null; then
    if rch check --quiet 2>/dev/null; then
        CARGO_CMD="rch exec cargo"
    fi
fi

echo "=== E2E: ${SCENARIO_ID} — Safety Guardrail Adversarial Suite ==="
echo "    cargo_cmd=${CARGO_CMD}"
echo "    log_dir=${LOG_DIR}"

# ── Test 1: Full adversarial suite ────────────────────────────────────────────

echo "[1/2] Running all 25 adversarial tests (ADV-01 through ADV-25)..."
if $CARGO_CMD test --test mission_safety_adversarial --features subprocess-bridge \
    2>"$LOG_DIR/test_stderr.log" | tee "$LOG_DIR/test_stdout.log"; then
    PASS_COUNT=$(grep -c '\.\.\..*ok' "$LOG_DIR/test_stdout.log" || echo "0")
    log_structured "PASS" "adversarial_suite_pass" "" "$(printf ',\"input_summary\":\"25 adversarial tests\",\"decision_path\":\"cargo test\",\"artifact_path\":\"%s/test_stdout.log\",\"pass_count\":\"%s\"' "$LOG_DIR" "$PASS_COUNT")"
    echo "    ✓ ${PASS_COUNT} adversarial tests passed"
else
    log_structured "FAIL" "adversarial_suite_fail" "E2E001" "$(printf ',\"input_summary\":\"adversarial tests\",\"artifact_path\":\"%s/test_stderr.log\"' "$LOG_DIR")"
    echo "    ✗ Adversarial tests failed — see $LOG_DIR/test_stderr.log"
    exit 1
fi

# ── Test 2: Verify test coverage spans all categories ─────────────────────────

echo "[2/2] Verifying test category coverage..."
EXPECTED_TESTS=(
    "adv_01_envelope_at_exact_cap_allows_all"
    "adv_05_all_conflict_types_in_single_cycle"
    "adv_07_strategy_affects_winner"
    "adv_15_full_report_serde_roundtrip"
    "adv_24_metrics_count_conflict_rejections"
    "adv_25_deconfliction_message_serde"
)
MISSING=0
for test_name in "${EXPECTED_TESTS[@]}"; do
    if ! grep -q "$test_name" "$LOG_DIR/test_stdout.log"; then
        echo "    ✗ Missing expected test: $test_name"
        MISSING=$((MISSING + 1))
    fi
done

if [ "$MISSING" -eq 0 ]; then
    log_structured "PASS" "category_coverage_pass" "" ',\"input_summary\":\"envelope + conflict + serde + metrics categories\"'
    echo "    ✓ All test categories covered"
else
    log_structured "FAIL" "category_coverage_fail" "E2E002" "$(printf ',\"missing_count\":\"%s\"' "$MISSING")"
    echo "    ✗ $MISSING expected tests missing"
    exit 1
fi

echo ""
echo "=== E2E: ${SCENARIO_ID} — ALL PASSED ==="
echo "    Logs: ${LOG_DIR}/results.jsonl"
log_structured "PASS" "e2e_suite_complete" "" ',\"input_summary\":\"all test groups passed\"'
