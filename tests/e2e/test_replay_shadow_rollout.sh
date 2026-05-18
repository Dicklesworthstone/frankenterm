#!/usr/bin/env bash
# E2E smoke test: replay shadow rollout (ft-og6q6.7.5)
#
# Validates shadow mode, enforcement, kill switch, and flaky detection
# using the Rust module as ground truth.
#
# Summary JSON: {"test":"shadow_rollout","scenario":N,"mode":"shadow|enforce|killswitch",
#                "gate_result":"pass|fail","pr_blocked":true|false,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay-shadow-rollout-${RUN_ID}"
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "replay_shadow_rollout" "${REPO_ROOT}"

echo "=== Replay Shadow Rollout E2E ==="
ensure_rch_ready

# ── Scenario 1: Shadow mode, gate fails, PR not blocked ───────────────
echo ""
echo "--- Scenario 1: Shadow Mode ---"

scenario1_log="${LOG_DIR}/replay_shadow_rollout_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_shadow_rollout::tests::shadow_mode_fail_not_blocked \
    && grep -q "ok" "${scenario1_log}"; then
    pass "Shadow mode: fail not blocked"
    echo '{"test":"shadow_rollout","scenario":1,"mode":"shadow","gate_result":"fail","pr_blocked":false,"status":"pass"}'
else
    fail "Shadow mode: fail not blocked (see $(basename "${scenario1_log}"))"
fi

# ── Scenario 2: Enforcement mode, gate fails, PR blocked ─────────────
echo ""
echo "--- Scenario 2: Enforcement Mode ---"

scenario2_log="${LOG_DIR}/replay_shadow_rollout_${RUN_ID}.scenario2.log"
if run_rch_cargo_logged "${scenario2_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_shadow_rollout::tests::enforce_mode_fail_blocked \
    && grep -q "ok" "${scenario2_log}"; then
    pass "Enforce mode: fail blocked"
    echo '{"test":"shadow_rollout","scenario":2,"mode":"enforce","gate_result":"fail","pr_blocked":true,"status":"pass"}'
else
    fail "Enforce mode: fail blocked (see $(basename "${scenario2_log}"))"
fi

# ── Scenario 3: Kill switch, no enforcement ───────────────────────────
echo ""
echo "--- Scenario 3: Kill Switch ---"

scenario3_log="${LOG_DIR}/replay_shadow_rollout_${RUN_ID}.scenario3.log"
if run_rch_cargo_logged "${scenario3_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_shadow_rollout::tests::kill_switch_overrides_enforcement \
    && grep -q "ok" "${scenario3_log}"; then
    pass "Kill switch overrides"
    echo '{"test":"shadow_rollout","scenario":3,"mode":"killswitch","gate_result":"fail","pr_blocked":false,"status":"pass"}'
else
    fail "Kill switch overrides (see $(basename "${scenario3_log}"))"
fi

# ── Scenario 4: Flaky test detection ──────────────────────────────────
echo ""
echo "--- Scenario 4: Flaky Detection ---"

scenario4_log="${LOG_DIR}/replay_shadow_rollout_${RUN_ID}.scenario4.log"
if run_rch_cargo_logged "${scenario4_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_shadow_rollout::tests::flaky_rate_critical \
    && grep -q "ok" "${scenario4_log}"; then
    pass "Flaky rate detection"
    echo '{"test":"shadow_rollout","scenario":4,"mode":"shadow","gate_result":"flaky","pr_blocked":false,"status":"pass"}'
else
    fail "Flaky rate detection (see $(basename "${scenario4_log}"))"
fi

# ── Scenario 5: Full module validation ────────────────────────────────
echo ""
echo "--- Scenario 5: Full Module Validation ---"

scenario5_log="${LOG_DIR}/replay_shadow_rollout_${RUN_ID}.scenario5.log"
if run_rch_cargo_logged "${scenario5_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_shadow_rollout \
    && grep -q "test result: ok" "${scenario5_log}"; then
    pass "All shadow rollout unit tests (35 tests)"
else
    fail "Shadow rollout unit tests (see $(basename "${scenario5_log}"))"
fi

scenario6_log="${LOG_DIR}/replay_shadow_rollout_${RUN_ID}.scenario6.log"
if run_rch_cargo_logged "${scenario6_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test proptest_replay_shadow_rollout \
    && grep -q "test result: ok" "${scenario6_log}"; then
    pass "All shadow rollout property tests (20 tests)"
else
    fail "Shadow rollout property tests (see $(basename "${scenario6_log}"))"
fi

# ── Summary ─────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"shadow_rollout\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
