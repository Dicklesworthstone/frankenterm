#!/usr/bin/env bash
# E2E smoke test: replay post-incident feedback loop (ft-og6q6.7.6)
#
# Validates pipeline execution, input validation, incident corpus,
# and coverage tracking using the Rust module as ground truth.
#
# Summary JSON: {"test":"post_incident","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay-post-incident-${RUN_ID}"
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "replay_post_incident" "${REPO_ROOT}"

echo "=== Replay Post-Incident Feedback Loop E2E ==="
ensure_rch_ready

# ── Scenario 1: Pipeline execution ──────────────────────────────────────
echo ""
echo "--- Scenario 1: Pipeline Execution ---"

scenario1_log="${LOG_DIR}/replay_post_incident_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_post_incident::tests::pipeline_success \
    && grep -q "ok" "${scenario1_log}"; then
    pass "Pipeline execution succeeds"
    echo '{"test":"post_incident","scenario":1,"status":"pass"}'
else
    fail "Pipeline execution (see $(basename "${scenario1_log}"))"
fi

# ── Scenario 2: Input validation ────────────────────────────────────────
echo ""
echo "--- Scenario 2: Input Validation ---"

scenario2_log="${LOG_DIR}/replay_post_incident_${RUN_ID}.scenario2.log"
if run_rch_cargo_logged "${scenario2_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_post_incident::tests::empty_incident_id_error \
    && grep -q "ok" "${scenario2_log}"; then
    pass "Empty incident_id rejected"
    echo '{"test":"post_incident","scenario":2,"status":"pass"}'
else
    fail "Empty incident_id rejected (see $(basename "${scenario2_log}"))"
fi

# ── Scenario 3: Incident corpus coverage ────────────────────────────────
echo ""
echo "--- Scenario 3: Incident Corpus ---"

scenario3_log="${LOG_DIR}/replay_post_incident_${RUN_ID}.scenario3.log"
if run_rch_cargo_logged "${scenario3_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_post_incident::tests::corpus_gap_detection \
    && grep -q "ok" "${scenario3_log}"; then
    pass "Gap detection works"
    echo '{"test":"post_incident","scenario":3,"status":"pass"}'
else
    fail "Gap detection (see $(basename "${scenario3_log}"))"
fi

# ── Scenario 4: Coverage report ─────────────────────────────────────────
echo ""
echo "--- Scenario 4: Coverage Report ---"

scenario4_log="${LOG_DIR}/replay_post_incident_${RUN_ID}.scenario4.log"
if run_rch_cargo_logged "${scenario4_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_post_incident::tests::coverage_report_counts \
    && grep -q "ok" "${scenario4_log}"; then
    pass "Coverage report counts"
    echo '{"test":"post_incident","scenario":4,"status":"pass"}'
else
    fail "Coverage report counts (see $(basename "${scenario4_log}"))"
fi

# ── Scenario 5: Full module validation ──────────────────────────────────
echo ""
echo "--- Scenario 5: Full Module Validation ---"

scenario5_log="${LOG_DIR}/replay_post_incident_${RUN_ID}.scenario5.log"
if run_rch_cargo_logged "${scenario5_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_post_incident \
    && grep -q "test result: ok" "${scenario5_log}"; then
    pass "All post-incident unit tests"
else
    fail "Post-incident unit tests (see $(basename "${scenario5_log}"))"
fi

scenario6_log="${LOG_DIR}/replay_post_incident_${RUN_ID}.scenario6.log"
if run_rch_cargo_logged "${scenario6_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test proptest_replay_post_incident \
    && grep -q "test result: ok" "${scenario6_log}"; then
    pass "All post-incident property tests (20 tests)"
else
    fail "Post-incident property tests (see $(basename "${scenario6_log}"))"
fi

# ── Summary ─────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"post_incident\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
