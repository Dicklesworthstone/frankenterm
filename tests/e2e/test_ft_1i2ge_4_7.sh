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

DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-ft1i2ge-4-7"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
    CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
    CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

json_escape() {
    local value="$1"
    value=${value//\\/\\\\}
    value=${value//\"/\\\"}
    value=${value//$'\n'/\\n}
    value=${value//$'\r'/\\r}
    value=${value//$'\t'/\\t}
    printf '%s' "$value"
}

json_field() {
    local key="$1"
    local value="$2"
    printf ',"%s":"%s"' "$key" "$(json_escape "$value")"
}

log_structured() {
    local outcome="$1" reason_code="$2" error_code="${3:-}" extra="${4:-}"
    printf '{"timestamp":"%s","component":"%s","scenario_id":"%s","correlation_id":"%s","outcome":"%s","reason_code":"%s","error_code":"%s"%s}\n' \
        "$(json_escape "$TIMESTAMP")" \
        "$(json_escape "$COMPONENT")" \
        "$(json_escape "$SCENARIO_ID")" \
        "$(json_escape "$CORRELATION_ID")" \
        "$(json_escape "$outcome")" \
        "$(json_escape "$reason_code")" \
        "$(json_escape "$error_code")" \
        "$extra" \
        | tee -a "$LOG_DIR/results.jsonl"
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

run_cargo_step() {
    local label="$1"
    shift

    local step_log="$LOG_DIR/${label}.log"
    local test_cmd=(env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo "$@")

    set +e
    run_rch_cargo_logged "$step_log" "${test_cmd[@]}"
    local rc=$?
    set -e

    cat "$step_log" >>"$LOG_DIR/test_stdout.log"
    return "$rc"
}

# ── Preflight ────────────────────────────────────────────────────────────────

if ! command -v jq &>/dev/null; then
    log_structured "SKIP" "jq_missing" "jq_not_found" ',"input_summary":"jq binary not in PATH"'
    echo "SKIP: jq not found — install jq to run structured assertions"
    exit 0
fi

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${CORRELATION_ID}" "1i2ge_4_7"
ensure_rch_ready

: >"$LOG_DIR/test_stdout.log"

echo "=== E2E: ${SCENARIO_ID} — Safety Guardrail Adversarial Suite ==="
echo "    cargo_cmd=run_rch_cargo_logged"
echo "    log_dir=${LOG_DIR}"

# ── Test 1: Full adversarial suite ────────────────────────────────────────────

echo "[1/2] Running all 25 adversarial tests (ADV-01 through ADV-25)..."
if run_cargo_step "mission_safety_adversarial" test --test mission_safety_adversarial --features subprocess-bridge; then
    PASS_COUNT=$(count_matches '\.\.\..*ok' "$LOG_DIR/test_stdout.log")
    log_structured "PASS" "adversarial_suite_pass" "" "$(json_field "input_summary" "25 adversarial tests")$(json_field "decision_path" "cargo test")$(json_field "artifact_path" "$LOG_DIR/test_stdout.log")$(json_field "pass_count" "$PASS_COUNT")"
    echo "    ✓ ${PASS_COUNT} adversarial tests passed"
else
    log_structured "FAIL" "adversarial_suite_fail" "E2E001" "$(json_field "input_summary" "adversarial tests")$(json_field "artifact_path" "$LOG_DIR/mission_safety_adversarial.log")"
    echo "    ✗ Adversarial tests failed — see $LOG_DIR/mission_safety_adversarial.log"
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
    log_structured "PASS" "category_coverage_pass" "" "$(json_field "input_summary" "envelope + conflict + serde + metrics categories")"
    echo "    ✓ All test categories covered"
else
    log_structured "FAIL" "category_coverage_fail" "E2E002" "$(json_field "missing_count" "$MISSING")"
    echo "    ✗ $MISSING expected tests missing"
    exit 1
fi

echo ""
echo "=== E2E: ${SCENARIO_ID} — ALL PASSED ==="
echo "    Logs: ${LOG_DIR}/results.jsonl"
log_structured "PASS" "e2e_suite_complete" "" "$(json_field "input_summary" "all test groups passed")"
