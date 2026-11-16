#!/usr/bin/env bash
# E2E smoke test: replay provenance logs and decision trace (ft-og6q6.3.4)
#
# Validates provenance chain integrity, decision trace emission,
# verbosity levels, and trace serde roundtrips using Rust tests as ground truth.
#
# Summary JSON: {"test":"replay_provenance","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay-provenance-${RUN_ID}"
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "replay_provenance" "${REPO_ROOT}"

echo "=== Replay Provenance Logs E2E ==="
ensure_rch_ready

# ── Scenario 1: Chain integrity ───────────────────────────────────────────
echo ""
echo "--- Scenario 1: Provenance Chain Integrity ---"

scenario1_log="${LOG_DIR}/replay_provenance_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib -- replay_provenance::tests::chain \
    && grep -q "ok" "${scenario1_log}"; then
    pass "Provenance chain integrity"
    echo '{"test":"replay_provenance","scenario":1,"check":"chain_integrity","status":"pass"}'
else
    fail "Provenance chain integrity (see $(basename "${scenario1_log}"))"
fi

# ── Scenario 2: Verbosity levels ──────────────────────────────────────────
echo ""
echo "--- Scenario 2: Verbosity Levels ---"

scenario2_log="${LOG_DIR}/replay_provenance_${RUN_ID}.scenario2.log"
if run_rch_cargo_logged "${scenario2_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib -- replay_provenance::tests::verbosity \
    && grep -q "ok" "${scenario2_log}"; then
    pass "Verbosity level control"
    echo '{"test":"replay_provenance","scenario":2,"check":"verbosity","status":"pass"}'
else
    fail "Verbosity level control (see $(basename "${scenario2_log}"))"
fi

# ── Scenario 3: Trace serde roundtrip ─────────────────────────────────────
echo ""
echo "--- Scenario 3: Trace Serde Roundtrip ---"

scenario3_log="${LOG_DIR}/replay_provenance_${RUN_ID}.scenario3.log"
if run_rch_cargo_logged "${scenario3_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib -- replay_provenance::tests::trace_serde_roundtrip \
    && grep -q "ok" "${scenario3_log}"; then
    pass "Trace serde roundtrip"
    echo '{"test":"replay_provenance","scenario":3,"check":"serde","status":"pass"}'
else
    fail "Trace serde roundtrip (see $(basename "${scenario3_log}"))"
fi

# ── Scenario 4: Full unit test suite ──────────────────────────────────────
echo ""
echo "--- Scenario 4: Full Unit Test Suite ---"

scenario4_log="${LOG_DIR}/replay_provenance_${RUN_ID}.scenario4.log"
if run_rch_cargo_logged "${scenario4_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_provenance \
    && grep -q "test result: ok" "${scenario4_log}"; then
    pass "All provenance unit tests (34 tests)"
else
    fail "Provenance unit tests (see $(basename "${scenario4_log}"))"
fi

# ── Scenario 5: Property tests ────────────────────────────────────────────
echo ""
echo "--- Scenario 5: Property Tests ---"

scenario5_log="${LOG_DIR}/replay_provenance_${RUN_ID}.scenario5.log"
if run_rch_cargo_logged "${scenario5_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test proptest_replay_provenance \
    && grep -q "test result: ok" "${scenario5_log}"; then
    pass "All provenance property tests (23 tests)"
else
    fail "Provenance property tests (see $(basename "${scenario5_log}"))"
fi

# ── Summary ───────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"replay_provenance\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
