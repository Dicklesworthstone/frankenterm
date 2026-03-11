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
# Requires: rch with at least one reachable remote worker

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

RCH_FAIL_OPEN_REGEX='\[RCH\] local|running locally'
RCH_PROBE_LOG="$LOG_DIR/rch_probe.log"
RCH_SMOKE_LOG="$LOG_DIR/rch_smoke.log"

run_rch() {
    TMPDIR=/tmp rch "$@"
}

probe_has_reachable_workers() {
    grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"
}

check_rch_fallback_in_logs() {
    local label="$1"
    shift

    if grep -Eq "$RCH_FAIL_OPEN_REGEX" "$@" 2>/dev/null; then
        log_structured "FAIL" "rch_local_fallback_detected" "RCH-LOCAL-FALLBACK" \
            "$(printf ',\"input_summary\":\"%s\",\"artifact_path\":\"%s\"' "$label" "$1")"
        echo "rch fell back to local execution during ${label}; refusing offload policy violation." >&2
        exit 3
    fi
}

# ── Preflight ────────────────────────────────────────────────────────────────

if ! command -v rch &>/dev/null; then
    log_structured "FAIL" "rch_required_missing" "RCH-E001" ',"input_summary":"rch binary missing"'
    echo "rch is required; refusing local cargo execution." >&2
    exit 1
fi

set +e
run_rch --json workers probe --all >"$RCH_PROBE_LOG" 2>&1
probe_rc=$?
set -e
if [[ $probe_rc -ne 0 ]] || ! probe_has_reachable_workers "$RCH_PROBE_LOG"; then
    log_structured "FAIL" "rch_workers_unhealthy" "RCH-E100" \
        "$(printf ',\"input_summary\":\"rch workers probe\",\"artifact_path\":\"%s\"' "$RCH_PROBE_LOG")"
    echo "rch workers are unavailable; refusing local cargo execution." >&2
    exit 1
fi

set +e
run_rch exec -- cargo check --help >"$RCH_SMOKE_LOG" 2>&1
smoke_rc=$?
set -e
check_rch_fallback_in_logs "rch_remote_smoke" "$RCH_SMOKE_LOG"
if [[ $smoke_rc -ne 0 ]]; then
    log_structured "FAIL" "rch_remote_smoke_failed" "RCH-E101" \
        "$(printf ',\"input_summary\":\"cargo check --help\",\"artifact_path\":\"%s\"' "$RCH_SMOKE_LOG")"
    echo "rch remote smoke preflight failed; refusing local cargo execution." >&2
    exit 1
fi

CARGO_CMD="run_rch exec -- cargo"

echo "=== E2E: ${SCENARIO_ID} — Safety Guardrail Adversarial Suite ==="
echo "    cargo_cmd=${CARGO_CMD}"
echo "    log_dir=${LOG_DIR}"

# ── Test 1: Full adversarial suite ────────────────────────────────────────────

echo "[1/2] Running all 25 adversarial tests (ADV-01 through ADV-25)..."
if $CARGO_CMD test --test mission_safety_adversarial --features subprocess-bridge \
    2>"$LOG_DIR/test_stderr.log" | tee "$LOG_DIR/test_stdout.log"; then
    check_rch_fallback_in_logs "mission_safety_adversarial" "$LOG_DIR/test_stdout.log" "$LOG_DIR/test_stderr.log"
    PASS_COUNT=$(grep -c '\.\.\..*ok' "$LOG_DIR/test_stdout.log" || echo "0")
    log_structured "PASS" "adversarial_suite_pass" "" "$(printf ',\"input_summary\":\"25 adversarial tests\",\"decision_path\":\"cargo test\",\"artifact_path\":\"%s/test_stdout.log\",\"pass_count\":\"%s\"' "$LOG_DIR" "$PASS_COUNT")"
    echo "    ✓ ${PASS_COUNT} adversarial tests passed"
else
    check_rch_fallback_in_logs "mission_safety_adversarial" "$LOG_DIR/test_stdout.log" "$LOG_DIR/test_stderr.log"
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
