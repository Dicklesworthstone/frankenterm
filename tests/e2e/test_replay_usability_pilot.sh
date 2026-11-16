#!/usr/bin/env bash
# E2E smoke test: replay usability pilot (ft-og6q6.7.8)
#
# Validates pilot framework, feedback log, metrics, evaluation,
# and improvement extraction using the Rust module as ground truth.
#
# Summary JSON: {"test":"usability_pilot","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
GUARD_LIB="${REPO_ROOT}/tests/e2e/lib_rch_guards.sh"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay-usability-pilot-${RUN_ID}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${GUARD_LIB}"
rch_init "${LOG_DIR}" "${RUN_ID}" "replay_usability_pilot" "${REPO_ROOT}"

PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Replay Usability Pilot E2E ==="
ensure_rch_ready

# ── Scenario 1: Pilot scenarios and metrics ─────────────────────────────
echo ""
echo "--- Scenario 1: Pilot Scenario Enum ---"

scenario1_log="${LOG_DIR}/replay_usability_pilot_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_usability_pilot::tests::scenario_str_roundtrip \
    && grep -q "ok" "${scenario1_log}"; then
    pass "Scenario enum roundtrip"
    echo '{"test":"usability_pilot","scenario":1,"status":"pass"}'
else
    fail "Scenario enum roundtrip"
fi

# ── Scenario 2: Feedback log and metrics ────────────────────────────────
echo ""
echo "--- Scenario 2: Feedback Log Metrics ---"

scenario2_log="${LOG_DIR}/replay_usability_pilot_${RUN_ID}.scenario2.log"
if run_rch_cargo_logged "${scenario2_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_usability_pilot::tests::metrics_calculation \
    && grep -q "ok" "${scenario2_log}"; then
    pass "Metrics calculation"
    echo '{"test":"usability_pilot","scenario":2,"status":"pass"}'
else
    fail "Metrics calculation"
fi

# ── Scenario 3: Pilot evaluation ────────────────────────────────────────
echo ""
echo "--- Scenario 3: Pilot Evaluation ---"

scenario3_log="${LOG_DIR}/replay_usability_pilot_${RUN_ID}.scenario3.log"
if run_rch_cargo_logged "${scenario3_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_usability_pilot::tests::evaluation_passes_default_criteria \
    && grep -q "ok" "${scenario3_log}"; then
    pass "Evaluation passes default criteria"
    echo '{"test":"usability_pilot","scenario":3,"status":"pass"}'
else
    fail "Evaluation passes default criteria"
fi

# ── Scenario 4: Improvement extraction ──────────────────────────────────
echo ""
echo "--- Scenario 4: Improvement Extraction ---"

scenario4_log="${LOG_DIR}/replay_usability_pilot_${RUN_ID}.scenario4.log"
if run_rch_cargo_logged "${scenario4_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_usability_pilot::tests::extract_improvements_from_log \
    && grep -q "ok" "${scenario4_log}"; then
    pass "Improvement extraction"
    echo '{"test":"usability_pilot","scenario":4,"status":"pass"}'
else
    fail "Improvement extraction"
fi

# ── Scenario 5: Full module validation ──────────────────────────────────
echo ""
echo "--- Scenario 5: Full Module Validation ---"

scenario5_log="${LOG_DIR}/replay_usability_pilot_${RUN_ID}.scenario5.log"
if run_rch_cargo_logged "${scenario5_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_usability_pilot \
    && grep -q "test result: ok" "${scenario5_log}"; then
    pass "All usability pilot unit tests"
else
    fail "Usability pilot unit tests"
fi

scenario5b_log="${LOG_DIR}/replay_usability_pilot_${RUN_ID}.scenario5b.log"
if run_rch_cargo_logged "${scenario5b_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test proptest_replay_usability_pilot \
    && grep -q "test result: ok" "${scenario5b_log}"; then
    pass "All usability pilot property tests (20 tests)"
else
    fail "Usability pilot property tests"
fi

# ── Summary ─────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"usability_pilot\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
