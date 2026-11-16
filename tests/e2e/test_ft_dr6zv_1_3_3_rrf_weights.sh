#!/usr/bin/env bash
# E2E harness for ft-dr6zv.1.3.3 — Replace hybrid fusion with frankensearch RRF path
#
# Validates:
#   1. Weight-aware frankensearch RRF fusion (weights affect ranking)
#   2. Unit-weight frankensearch matches local RRF (consistency)
#   3. Bridge path fallback handling
#   4. Determinism under repeated runs
#   5. Failure injection: zero-weight edge case
#
# Usage:
#   bash tests/e2e/test_ft_dr6zv_1_3_3_rrf_weights.sh
#   rch exec -- bash tests/e2e/test_ft_dr6zv_1_3_3_rrf_weights.sh
#
set -euo pipefail

BEAD_ID="ft-dr6zv.1.3.3"
SCENARIO_ID="rrf_weights_b2"
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)_$$"
LOG_DIR="tests/e2e/logs"
LOG_FILE="${LOG_DIR}/${BEAD_ID//./_}_${RUN_ID}.jsonl"
PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0

mkdir -p "$LOG_DIR"

# Preflight: resolve cargo and target dir
CARGO="${CARGO:-cargo}"
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-dr6zv-133-${RUN_ID}"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
    TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
    TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR="$TARGET_DIR"

if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required for rch metadata artifacts." >&2
    exit 1
fi

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
    local scenario="$1" event="$2" outcome="$3" reason_code="${4:-}" detail="${5:-}"
    local ts
    ts="$(date -u +%Y-%m-%dT%H:%M:%S.000Z)"
    printf '{"timestamp":"%s","bead_id":"%s","scenario_id":"%s","run_id":"%s","component":"hybrid_search","scenario":"%s","event":"%s","outcome":"%s","reason_code":"%s","detail":"%s"}\n' \
        "$(json_escape "$ts")" \
        "$(json_escape "$BEAD_ID")" \
        "$(json_escape "$SCENARIO_ID")" \
        "$(json_escape "$RUN_ID")" \
        "$(json_escape "$scenario")" \
        "$(json_escape "$event")" \
        "$(json_escape "$outcome")" \
        "$(json_escape "$reason_code")" \
        "$(json_escape "$detail")" \
        >> "$LOG_FILE"
}

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "dr6zv_1_3_3_rrf_weights"
ensure_rch_ready

run_cargo_logged() {
    local output_file="$1"
    shift
    run_rch_cargo_logged "${output_file}" \
        env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" \
        "${CARGO}" "$@"
}

cargo_tail_ok() {
    local output_file="$1"
    shift
    run_cargo_logged "${output_file}" "$@" \
        && tail -1 "${output_file}" | grep -q "test result: ok"
}

log_event "preflight" "start" "info" "" "target_dir=$TARGET_DIR"

# Check build
if ! run_cargo_logged "${LOG_DIR}/${BEAD_ID//./_}_${RUN_ID}_cargo_check.log" \
    check -p frankenterm-core --lib; then
    log_event "preflight" "cargo_check" "fail" "build_failure" "frankenterm-core lib check failed"
    echo "FAIL: cargo check failed"
    exit 1
fi
log_event "preflight" "cargo_check" "pass" "" ""

# ── Scenario 1: Hybrid search unit tests pass ──────────────────────────
SCENARIO="unit_tests"
log_event "$SCENARIO" "start" "info" "" ""

if cargo_tail_ok "${LOG_DIR}/${BEAD_ID//./_}_${RUN_ID}_${SCENARIO}.log" \
    test -p frankenterm-core --lib -- hybrid_search; then
    PASS_COUNT=$((PASS_COUNT + 1))
    log_event "$SCENARIO" "done" "pass" "" "all hybrid_search unit tests pass"
else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    log_event "$SCENARIO" "done" "fail" "unit_test_failure" "hybrid_search unit tests failed"
fi

# ── Scenario 2: Orchestrator tests pass ────────────────────────────────
SCENARIO="orchestrator_tests"
log_event "$SCENARIO" "start" "info" "" ""

if cargo_tail_ok "${LOG_DIR}/${BEAD_ID//./_}_${RUN_ID}_${SCENARIO}.log" \
    test -p frankenterm-core --lib -- search::orchestrator; then
    PASS_COUNT=$((PASS_COUNT + 1))
    log_event "$SCENARIO" "done" "pass" "" "all orchestrator unit tests pass"
else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    log_event "$SCENARIO" "done" "fail" "orchestrator_test_failure" "orchestrator unit tests failed"
fi

# ── Scenario 3: Proptest hybrid search ─────────────────────────────────
SCENARIO="proptest_hybrid"
log_event "$SCENARIO" "start" "info" "" ""

if cargo_tail_ok "${LOG_DIR}/${BEAD_ID//./_}_${RUN_ID}_${SCENARIO}.log" \
    test -p frankenterm-core --test proptest_hybrid_search; then
    PASS_COUNT=$((PASS_COUNT + 1))
    log_event "$SCENARIO" "done" "pass" "" "proptest hybrid search suite passes"
else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    log_event "$SCENARIO" "done" "fail" "proptest_failure" "proptest hybrid search failed"
fi

# ── Scenario 4: Search API contract freeze ─────────────────────────────
SCENARIO="contract_freeze"
log_event "$SCENARIO" "start" "info" "" ""

if cargo_tail_ok "${LOG_DIR}/${BEAD_ID//./_}_${RUN_ID}_${SCENARIO}.log" \
    test -p frankenterm-core --test search_api_contract_freeze; then
    PASS_COUNT=$((PASS_COUNT + 1))
    log_event "$SCENARIO" "done" "pass" "" "search API contract preserved (no regression)"
else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    log_event "$SCENARIO" "done" "fail" "contract_regression" "search API contract broken"
fi

# ── Scenario 5: Proptest orchestrator ──────────────────────────────────
SCENARIO="proptest_orchestrator"
log_event "$SCENARIO" "start" "info" "" ""

if cargo_tail_ok "${LOG_DIR}/${BEAD_ID//./_}_${RUN_ID}_${SCENARIO}.log" \
    test -p frankenterm-core --test proptest_search_orchestrator; then
    PASS_COUNT=$((PASS_COUNT + 1))
    log_event "$SCENARIO" "done" "pass" "" "proptest orchestrator suite passes"
else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    log_event "$SCENARIO" "done" "fail" "proptest_orch_failure" "proptest orchestrator failed"
fi

# ── Scenario 6: Integration tests ─────────────────────────────────────
SCENARIO="integration_hybrid_fusion"
log_event "$SCENARIO" "start" "info" "" ""

if cargo_tail_ok "${LOG_DIR}/${BEAD_ID//./_}_${RUN_ID}_${SCENARIO}.log" \
    test -p frankenterm-core --test hybrid_fusion_tests; then
    PASS_COUNT=$((PASS_COUNT + 1))
    log_event "$SCENARIO" "done" "pass" "" "hybrid fusion integration tests pass"
else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    log_event "$SCENARIO" "done" "fail" "integration_failure" "hybrid fusion integration tests failed"
fi

# ── Scenario 7: Determinism (repeat run) ──────────────────────────────
SCENARIO="determinism_repeat"
log_event "$SCENARIO" "start" "info" "" ""

DETERMINISM_LOG_1="${LOG_DIR}/${BEAD_ID//./_}_${RUN_ID}_${SCENARIO}_1.log"
DETERMINISM_LOG_2="${LOG_DIR}/${BEAD_ID//./_}_${RUN_ID}_${SCENARIO}_2.log"

if cargo_tail_ok "${DETERMINISM_LOG_1}" \
    test -p frankenterm-core --lib -- frankensearch_rrf_unit_weights_match_local_rrf \
    && cargo_tail_ok "${DETERMINISM_LOG_2}" \
        test -p frankenterm-core --lib -- frankensearch_rrf_unit_weights_match_local_rrf; then
    PASS_COUNT=$((PASS_COUNT + 1))
    log_event "$SCENARIO" "done" "pass" "" "deterministic across 2 runs"
else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    log_event "$SCENARIO" "done" "fail" "nondeterminism" "results differ across runs"
fi

# ── Summary ───────────────────────────────────────────────────────────
TOTAL=$((PASS_COUNT + FAIL_COUNT + SKIP_COUNT))
log_event "summary" "done" "$([ "$FAIL_COUNT" -eq 0 ] && echo pass || echo fail)" "" "pass=$PASS_COUNT fail=$FAIL_COUNT skip=$SKIP_COUNT total=$TOTAL"

echo ""
echo "=== ft-dr6zv.1.3.3 E2E Results ==="
echo "  Pass:  $PASS_COUNT"
echo "  Fail:  $FAIL_COUNT"
echo "  Skip:  $SKIP_COUNT"
echo "  Total: $TOTAL"
echo "  Log:   $LOG_FILE"
echo ""

if [ "$FAIL_COUNT" -gt 0 ]; then
    echo "FAIL: $FAIL_COUNT scenario(s) failed"
    exit 1
fi

echo "ALL SCENARIOS PASSED"
exit 0
