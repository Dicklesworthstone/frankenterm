#!/usr/bin/env bash
# E2E smoke test: replay side-effect isolation barrier (ft-og6q6.3.3)
#
# Validates side-effect capture, replay-mode blocking, log auditing,
# and barrier enforcement using Rust tests as ground truth.
#
# Summary JSON: {"test":"side_effect_barrier","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay-side-effect-barrier-${RUN_ID}"
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "replay_side_effect_barrier" "${REPO_ROOT}"

echo "=== Replay Side-Effect Isolation Barrier E2E ==="
ensure_rch_ready

# ── Scenario 1: Effect capture and classification ─────────────────────────
echo ""
echo "--- Scenario 1: Effect Capture ---"

scenario1_log="${LOG_DIR}/replay_side_effect_barrier_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib -- replay_side_effect_barrier::tests::capture \
    && grep -q "ok" "${scenario1_log}"; then
    pass "Effect capture and classification"
    echo '{"test":"side_effect_barrier","scenario":1,"check":"capture","status":"pass"}'
else
    fail "Effect capture and classification (see $(basename "${scenario1_log}"))"
fi

# ── Scenario 2: Replay-mode blocking ─────────────────────────────────────
echo ""
echo "--- Scenario 2: Replay-Mode Blocking ---"

scenario2_log="${LOG_DIR}/replay_side_effect_barrier_${RUN_ID}.scenario2.log"
if run_rch_cargo_logged "${scenario2_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib -- replay_side_effect_barrier::tests::replay_mode \
    && grep -q "ok" "${scenario2_log}"; then
    pass "Replay-mode side-effect blocking"
    echo '{"test":"side_effect_barrier","scenario":2,"check":"replay_block","status":"pass"}'
else
    fail "Replay-mode side-effect blocking (see $(basename "${scenario2_log}"))"
fi

# ── Scenario 3: Full unit test suite ──────────────────────────────────────
echo ""
echo "--- Scenario 3: Full Unit Test Suite ---"

scenario3_log="${LOG_DIR}/replay_side_effect_barrier_${RUN_ID}.scenario3.log"
if run_rch_cargo_logged "${scenario3_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_side_effect_barrier \
    && grep -q "test result: ok" "${scenario3_log}"; then
    pass "All side-effect barrier unit tests (46 tests)"
else
    fail "Side-effect barrier unit tests (see $(basename "${scenario3_log}"))"
fi

# ── Scenario 4: Property tests ────────────────────────────────────────────
echo ""
echo "--- Scenario 4: Property Tests ---"

scenario4_log="${LOG_DIR}/replay_side_effect_barrier_${RUN_ID}.scenario4.log"
if run_rch_cargo_logged "${scenario4_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test proptest_replay_side_effect_barrier \
    && grep -q "test result: ok" "${scenario4_log}"; then
    pass "All side-effect barrier property tests (21 tests)"
else
    fail "Side-effect barrier property tests (see $(basename "${scenario4_log}"))"
fi

# ── Summary ───────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"side_effect_barrier\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
