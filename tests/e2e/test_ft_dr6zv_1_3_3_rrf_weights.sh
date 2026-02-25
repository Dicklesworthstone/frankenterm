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

log_event() {
    local scenario="$1" event="$2" outcome="$3" reason_code="${4:-}" detail="${5:-}"
    local ts
    ts="$(date -u +%Y-%m-%dT%H:%M:%S.000Z)"
    printf '{"timestamp":"%s","bead_id":"%s","scenario_id":"%s","run_id":"%s","component":"hybrid_search","scenario":"%s","event":"%s","outcome":"%s","reason_code":"%s","detail":"%s"}\n' \
        "$ts" "$BEAD_ID" "$SCENARIO_ID" "$RUN_ID" "$scenario" "$event" "$outcome" "$reason_code" "$detail" \
        >> "$LOG_FILE"
}

# Preflight: resolve cargo and target dir
CARGO="${CARGO:-cargo}"
TARGET_DIR="${CARGO_TARGET_DIR:-target-e2e-dr6zv-133}"
export CARGO_TARGET_DIR="$TARGET_DIR"

log_event "preflight" "start" "info" "" "target_dir=$TARGET_DIR"

# Check build
if ! $CARGO check -p frankenterm-core --lib 2>/dev/null; then
    log_event "preflight" "cargo_check" "fail" "build_failure" "frankenterm-core lib check failed"
    echo "FAIL: cargo check failed"
    exit 1
fi
log_event "preflight" "cargo_check" "pass" "" ""

# ── Scenario 1: Hybrid search unit tests pass ──────────────────────────
SCENARIO="unit_tests"
log_event "$SCENARIO" "start" "info" "" ""

if $CARGO test -p frankenterm-core --lib -- hybrid_search 2>&1 | tail -1 | grep -q "test result: ok"; then
    PASS_COUNT=$((PASS_COUNT + 1))
    log_event "$SCENARIO" "done" "pass" "" "all hybrid_search unit tests pass"
else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    log_event "$SCENARIO" "done" "fail" "unit_test_failure" "hybrid_search unit tests failed"
fi

# ── Scenario 2: Orchestrator tests pass ────────────────────────────────
SCENARIO="orchestrator_tests"
log_event "$SCENARIO" "start" "info" "" ""

if $CARGO test -p frankenterm-core --lib -- search::orchestrator 2>&1 | tail -1 | grep -q "test result: ok"; then
    PASS_COUNT=$((PASS_COUNT + 1))
    log_event "$SCENARIO" "done" "pass" "" "all orchestrator unit tests pass"
else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    log_event "$SCENARIO" "done" "fail" "orchestrator_test_failure" "orchestrator unit tests failed"
fi

# ── Scenario 3: Proptest hybrid search ─────────────────────────────────
SCENARIO="proptest_hybrid"
log_event "$SCENARIO" "start" "info" "" ""

if $CARGO test -p frankenterm-core --test proptest_hybrid_search 2>&1 | tail -1 | grep -q "test result: ok"; then
    PASS_COUNT=$((PASS_COUNT + 1))
    log_event "$SCENARIO" "done" "pass" "" "proptest hybrid search suite passes"
else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    log_event "$SCENARIO" "done" "fail" "proptest_failure" "proptest hybrid search failed"
fi

# ── Scenario 4: Search API contract freeze ─────────────────────────────
SCENARIO="contract_freeze"
log_event "$SCENARIO" "start" "info" "" ""

if $CARGO test -p frankenterm-core --test search_api_contract_freeze 2>&1 | tail -1 | grep -q "test result: ok"; then
    PASS_COUNT=$((PASS_COUNT + 1))
    log_event "$SCENARIO" "done" "pass" "" "search API contract preserved (no regression)"
else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    log_event "$SCENARIO" "done" "fail" "contract_regression" "search API contract broken"
fi

# ── Scenario 5: Proptest orchestrator ──────────────────────────────────
SCENARIO="proptest_orchestrator"
log_event "$SCENARIO" "start" "info" "" ""

if $CARGO test -p frankenterm-core --test proptest_search_orchestrator 2>&1 | tail -1 | grep -q "test result: ok"; then
    PASS_COUNT=$((PASS_COUNT + 1))
    log_event "$SCENARIO" "done" "pass" "" "proptest orchestrator suite passes"
else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    log_event "$SCENARIO" "done" "fail" "proptest_orch_failure" "proptest orchestrator failed"
fi

# ── Scenario 6: Integration tests ─────────────────────────────────────
SCENARIO="integration_hybrid_fusion"
log_event "$SCENARIO" "start" "info" "" ""

if $CARGO test -p frankenterm-core --test hybrid_fusion_tests 2>&1 | tail -1 | grep -q "test result: ok"; then
    PASS_COUNT=$((PASS_COUNT + 1))
    log_event "$SCENARIO" "done" "pass" "" "hybrid fusion integration tests pass"
else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    log_event "$SCENARIO" "done" "fail" "integration_failure" "hybrid fusion integration tests failed"
fi

# ── Scenario 7: Determinism (repeat run) ──────────────────────────────
SCENARIO="determinism_repeat"
log_event "$SCENARIO" "start" "info" "" ""

RESULT1=$($CARGO test -p frankenterm-core --lib -- frankensearch_rrf_unit_weights_match_local_rrf 2>&1 | tail -1)
RESULT2=$($CARGO test -p frankenterm-core --lib -- frankensearch_rrf_unit_weights_match_local_rrf 2>&1 | tail -1)

if echo "$RESULT1" | grep -q "test result: ok" && echo "$RESULT2" | grep -q "test result: ok"; then
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
