#!/usr/bin/env bash
# E2E smoke test: replay merge / stable event ordering (ft-og6q6.3.2)
#
# Validates pane merge resolution, timestamp ordering, tie-breaking,
# and deterministic multi-pane interleaving using Rust tests as ground truth.
#
# Summary JSON: {"test":"replay_merge","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay_merge-${RUN_ID}"
GUARD_LIB="${SCRIPT_DIR}/lib_rch_guards.sh"
# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${GUARD_LIB}"
rch_init "${LOG_DIR}" "${RUN_ID}" "replay_merge" "${REPO_ROOT}"

PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Replay Merge / Event Ordering E2E ==="

ensure_rch_ready

# ── Scenario 1: Basic merge ordering ──────────────────────────────────────
echo ""
echo "--- Scenario 1: Single-pane and multi-pane merge ---"

scenario1_log="${LOG_DIR}/replay_merge_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_merge::tests::single_pane_passthrough && grep -q "ok" "${scenario1_log}"; then
    pass "Single-pane passthrough"
    echo '{"test":"replay_merge","scenario":1,"check":"single_pane","status":"pass"}'
else
    fail "Single-pane passthrough (see $(basename "${scenario1_log}"))"
fi

# ── Scenario 2: Tie-breaking and determinism ──────────────────────────────
echo ""
echo "--- Scenario 2: Timestamp tie-breaking ---"

scenario2_log="${LOG_DIR}/replay_merge_${RUN_ID}.scenario2.log"
if run_rch_cargo_logged "${scenario2_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_merge::tests::same_timestamp_stable_order && grep -q "ok" "${scenario2_log}"; then
    pass "Same-timestamp stable ordering"
    echo '{"test":"replay_merge","scenario":2,"check":"tie_breaking","status":"pass"}'
else
    fail "Same-timestamp stable ordering (see $(basename "${scenario2_log}"))"
fi

# ── Scenario 3: Large-scale merge ─────────────────────────────────────────
echo ""
echo "--- Scenario 3: Large merge (100 panes) ---"

scenario3_log="${LOG_DIR}/replay_merge_${RUN_ID}.scenario3.log"
if run_rch_cargo_logged "${scenario3_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_merge::tests::large_merge_100_panes && grep -q "ok" "${scenario3_log}"; then
    pass "Large merge 100 panes"
    echo '{"test":"replay_merge","scenario":3,"check":"large_merge","status":"pass"}'
else
    fail "Large merge 100 panes (see $(basename "${scenario3_log}"))"
fi

# ── Scenario 4: Full unit test suite ──────────────────────────────────────
echo ""
echo "--- Scenario 4: Full Unit Test Suite ---"

scenario4_log="${LOG_DIR}/replay_merge_${RUN_ID}.scenario4.log"
if run_rch_cargo_logged "${scenario4_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_merge && grep -q "test result: ok" "${scenario4_log}"; then
    pass "All replay merge unit tests (27 tests)"
else
    fail "Replay merge unit tests (see $(basename "${scenario4_log}"))"
fi

# ── Scenario 5: Property tests ────────────────────────────────────────────
echo ""
echo "--- Scenario 5: Property Tests ---"

scenario5_log="${LOG_DIR}/replay_merge_${RUN_ID}.scenario5.log"
if run_rch_cargo_logged "${scenario5_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test proptest_replay_merge && grep -q "test result: ok" "${scenario5_log}"; then
    pass "All replay merge property tests (18 tests)"
else
    fail "Replay merge property tests (see $(basename "${scenario5_log}"))"
fi

# ── Summary ───────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"replay_merge\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
