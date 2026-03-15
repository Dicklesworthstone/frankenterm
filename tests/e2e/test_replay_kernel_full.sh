#!/usr/bin/env bash
# E2E smoke test: kernel determinism full integration suite (ft-og6q6.3.6)
#
# Validates cross-module kernel determinism: VirtualClock, ReplayScheduler,
# PaneMergeResolver, SideEffectBarrier, and ProvenanceEmitter working together.
#
# Summary JSON: {"test":"kernel_full","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay_kernel_full-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/replay_kernel_full_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/replay_kernel_full_${RUN_ID}.smoke.log"

PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

fatal() { echo "FATAL: $1" >&2; exit 1; }

run_rch() {
    TMPDIR=/tmp rch "$@"
}

run_rch_cargo() {
    run_rch exec -- env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo "$@"
}

probe_has_reachable_workers() {
    grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"
}

check_rch_fallback() {
    local output_file="$1"
    if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_file}" 2>/dev/null; then
        fatal "rch fell back to local execution; refusing offload policy violation. See ${output_file}"
    fi
}

run_rch_cargo_logged() {
    local output_file="$1"
    shift

    set +e
    (
        cd "${REPO_ROOT}"
        run_rch_cargo "$@"
    ) >"${output_file}" 2>&1
    local rc=$?
    set -e

    check_rch_fallback "${output_file}"
    return "${rc}"
}

ensure_rch_ready() {
    if ! command -v rch >/dev/null 2>&1; then
        fatal "rch is required for this e2e harness; refusing local cargo execution."
    fi

    set +e
    run_rch --json workers probe --all >"${RCH_PROBE_LOG}" 2>&1
    local probe_rc=$?
    set -e
    if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${RCH_PROBE_LOG}"; then
        fatal "rch workers are unavailable; refusing local cargo execution. See ${RCH_PROBE_LOG}"
    fi

    set +e
    run_rch_cargo check --help >"${RCH_SMOKE_LOG}" 2>&1
    local smoke_rc=$?
    set -e
    check_rch_fallback "${RCH_SMOKE_LOG}"
    if [[ ${smoke_rc} -ne 0 ]]; then
        fatal "rch remote smoke preflight failed; refusing local cargo execution. See ${RCH_SMOKE_LOG}"
    fi
}

echo "=== Replay Kernel Determinism Full Integration Suite ==="

ensure_rch_ready

# ── Scenario 1: Single-pane and multi-pane replay ─────────────────────────
echo ""
echo "--- Scenario 1: Replay Roundtrip ---"

scenario1_log="${LOG_DIR}/replay_kernel_full_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" test -p frankenterm-core --test replay_kernel_determinism_integration -- i01 i02 && grep -q "test result: ok" "${scenario1_log}"; then
    pass "Single-pane and multi-pane replay roundtrip"
    echo '{"test":"kernel_full","scenario":1,"check":"replay_roundtrip","status":"pass"}'
else
    fail "Replay roundtrip (see $(basename "${scenario1_log}"))"
fi

# ── Scenario 2: Checkpoint/resume and determinism ─────────────────────────
echo ""
echo "--- Scenario 2: Checkpoint and Determinism ---"

scenario2_log="${LOG_DIR}/replay_kernel_full_${RUN_ID}.scenario2.log"
if run_rch_cargo_logged "${scenario2_log}" test -p frankenterm-core --test replay_kernel_determinism_integration -- i03 i14 && grep -q "test result: ok" "${scenario2_log}"; then
    pass "Checkpoint/resume equivalence and deterministic decision IDs"
    echo '{"test":"kernel_full","scenario":2,"check":"checkpoint_determinism","status":"pass"}'
else
    fail "Checkpoint/determinism (see $(basename "${scenario2_log}"))"
fi

# ── Scenario 3: Side-effect isolation ─────────────────────────────────────
echo ""
echo "--- Scenario 3: Side-Effect Isolation ---"

scenario3_log="${LOG_DIR}/replay_kernel_full_${RUN_ID}.scenario3.log"
if run_rch_cargo_logged "${scenario3_log}" test -p frankenterm-core --test replay_kernel_determinism_integration -- i04 i11 i12 i19 && grep -q "test result: ok" "${scenario3_log}"; then
    pass "Side-effect barrier: replay/live/counterfactual modes"
    echo '{"test":"kernel_full","scenario":3,"check":"side_effect_isolation","status":"pass"}'
else
    fail "Side-effect isolation (see $(basename "${scenario3_log}"))"
fi

# ── Scenario 4: Clock and merge ───────────────────────────────────────────
echo ""
echo "--- Scenario 4: Clock and Merge ---"

scenario4_log="${LOG_DIR}/replay_kernel_full_${RUN_ID}.scenario4.log"
if run_rch_cargo_logged "${scenario4_log}" test -p frankenterm-core --test replay_kernel_determinism_integration -- i05 i06 i07 i16 && grep -q "test result: ok" "${scenario4_log}"; then
    pass "Clock anomaly, speed control, merge+schedule pipeline"
    echo '{"test":"kernel_full","scenario":4,"check":"clock_merge","status":"pass"}'
else
    fail "Clock and merge (see $(basename "${scenario4_log}"))"
fi

# ── Scenario 5: Provenance and audit ──────────────────────────────────────
echo ""
echo "--- Scenario 5: Provenance and Audit ---"

scenario5_log="${LOG_DIR}/replay_kernel_full_${RUN_ID}.scenario5.log"
if run_rch_cargo_logged "${scenario5_log}" test -p frankenterm-core --test replay_kernel_determinism_integration -- i08 i09 i10 i13 i17 i18 i22 i23 && grep -q "test result: ok" "${scenario5_log}"; then
    pass "Provenance tracking, audit chain, JSONL roundtrip"
    echo '{"test":"kernel_full","scenario":5,"check":"provenance_audit","status":"pass"}'
else
    fail "Provenance and audit (see $(basename "${scenario5_log}"))"
fi

# ── Scenario 6: Full integration suite ────────────────────────────────────
echo ""
echo "--- Scenario 6: Full Integration Suite ---"

scenario6_log="${LOG_DIR}/replay_kernel_full_${RUN_ID}.scenario6.log"
if run_rch_cargo_logged "${scenario6_log}" test -p frankenterm-core --test replay_kernel_determinism_integration && grep -q "test result: ok" "${scenario6_log}"; then
    pass "All kernel determinism integration tests (24 tests)"
else
    fail "Full integration suite (see $(basename "${scenario6_log}"))"
fi

# ── Scenario 7: Existing kernel unit tests ────────────────────────────────
echo ""
echo "--- Scenario 7: Kernel Unit Tests ---"

scenario7_log="${LOG_DIR}/replay_kernel_full_${RUN_ID}.scenario7.log"
if run_rch_cargo_logged "${scenario7_log}" test -p frankenterm-core --lib recorder_replay::tests && grep -q "test result: ok" "${scenario7_log}"; then
    pass "All recorder_replay unit tests (84 tests)"
else
    fail "Kernel unit tests (see $(basename "${scenario7_log}"))"
fi

# ── Scenario 8: Determinism property tests ────────────────────────────────
echo ""
echo "--- Scenario 8: Determinism Property Tests ---"

scenario8_log="${LOG_DIR}/replay_kernel_full_${RUN_ID}.scenario8.log"
if run_rch_cargo_logged "${scenario8_log}" test -p frankenterm-core --test proptest_replay_determinism && grep -q "test result: ok" "${scenario8_log}"; then
    pass "All determinism property tests (19 tests)"
else
    fail "Determinism property tests (see $(basename "${scenario8_log}"))"
fi

# ── Summary ───────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"kernel_full\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
