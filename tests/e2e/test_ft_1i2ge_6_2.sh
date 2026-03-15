#!/usr/bin/env bash
# ────────────────────────────────────────────────────────────────────────────
# E2E Harness: ft-1i2ge.6.2 — Structured event/log taxonomy
#
# Validates mission_events module: event kind taxonomy, reason codes,
# builder ergonomics, bounded event log, cycle emitter, serde roundtrips,
# and phase/kind filtering.
#
# Execution: rch exec -- bash tests/e2e/test_ft_1i2ge_6_2.sh
# ────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SCENARIO_ID="ft-1i2ge-6-2"
LOG_DIR="$SCRIPT_DIR/logs"
TIMESTAMP="$(date -u +%Y%m%d_%H%M%S)"
LOG_FILE="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}.jsonl"

mkdir -p "$LOG_DIR"

DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-ft1i2ge-6-2"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
    CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
    CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${TIMESTAMP}" "1i2ge_6_2"
ensure_rch_ready

# ── Structured log helper ──────────────────────────────────────────────────
log_event() {
    local component="$1"
    local decision_path="$2"
    local input_summary="$3"
    local outcome="$4"
    local reason_code="${5:-nominal}"
    local error_code="${6:-none}"
    printf '{"timestamp":"%s","component":"%s","scenario_id":"%s","correlation_id":"%s-%s","decision_path":"%s","input_summary":"%s","outcome":"%s","reason_code":"%s","error_code":"%s"}\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        "$component" \
        "$SCENARIO_ID" \
        "$SCENARIO_ID" "$TIMESTAMP" \
        "$decision_path" \
        "$input_summary" \
        "$outcome" \
        "$reason_code" \
        "$error_code" >> "$LOG_FILE"
}

RCH_FAIL_OPEN_REGEX='\[RCH\] local|running locally'
RCH_PROBE_LOG="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_rch_probe.log"
RCH_SMOKE_LOG="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_rch_smoke.log"

run_rch() {
    TMPDIR=/tmp rch "$@"
}

run_rch_cargo() {
    run_rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo "$@"
}

probe_has_reachable_workers() {
    grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"
}

check_rch_fallback_in_logs() {
    local label="$1"
    shift

    if grep -Eq "$RCH_FAIL_OPEN_REGEX" "$@" 2>/dev/null; then
        log_event "rch_offload" "cargo_step" "$label" "fail" "rch_local_fallback_detected" "RCH-LOCAL-FALLBACK"
        echo "rch fell back to local execution during ${label}; refusing offload policy violation." >&2
        exit 3
    fi
}

# ── Preflight ──────────────────────────────────────────────────────────────
log_event "preflight" "startup" "checking_rch" "started"

if ! command -v rch &>/dev/null; then
    log_event "preflight" "startup" "rch_binary_missing" "fail" "rch_required_missing" "RCH-E001"
    echo "rch is required; refusing local cargo execution." >&2
    exit 1
fi

set +e
run_rch --json workers probe --all >"$RCH_PROBE_LOG" 2>&1
probe_rc=$?
set -e
if [[ $probe_rc -ne 0 ]] || ! probe_has_reachable_workers "$RCH_PROBE_LOG"; then
    log_event "preflight" "startup" "rch_workers_probe" "fail" "rch_workers_unhealthy" "RCH-E100"
    echo "rch workers are unavailable; refusing local cargo execution." >&2
    exit 1
fi

set +e
run_rch_cargo check --help >"$RCH_SMOKE_LOG" 2>&1
smoke_rc=$?
set -e
check_rch_fallback_in_logs "rch_remote_smoke" "$RCH_SMOKE_LOG"
if [[ $smoke_rc -ne 0 ]]; then
    log_event "preflight" "startup" "cargo_check_help" "fail" "rch_remote_smoke_failed" "RCH-E101"
    echo "rch remote smoke preflight failed; refusing local cargo execution." >&2
    exit 1
fi

CARGO_CMD="run_rch_cargo"
log_event "preflight" "startup" "cargo_check_help" "pass" "rch_remote_smoke_ok" "none"

cd "$PROJECT_ROOT"

log_event "preflight" "startup" "cargo_target=$CARGO_TARGET_DIR" "ready"

# ── Test matrix ────────────────────────────────────────────────────────────
TESTS=(
    "mission_events::tests::event_kind_phase_mapping_plan"
    "mission_events::tests::event_kind_phase_mapping_safety"
    "mission_events::tests::event_kind_phase_mapping_dispatch"
    "mission_events::tests::event_kind_phase_mapping_reconcile"
    "mission_events::tests::event_kind_phase_mapping_lifecycle"
    "mission_events::tests::builder_produces_correct_event"
    "mission_events::tests::builder_detail_f64_non_finite_becomes_null"
    "mission_events::tests::builder_detail_strings_empty"
    "mission_events::tests::log_emit_increments_sequence"
    "mission_events::tests::log_disabled_rejects_emit"
    "mission_events::tests::log_fifo_eviction_at_capacity"
    "mission_events::tests::log_max_events_one_never_grows_past_one"
    "mission_events::tests::filter_by_phase"
    "mission_events::tests::filter_by_cycle"
    "mission_events::tests::filter_by_kind"
    "mission_events::tests::count_by_kind_and_phase"
    "mission_events::tests::drain_matching_removes_and_returns"
    "mission_events::tests::summary_reflects_log_state"
    "mission_events::tests::event_serde_roundtrip"
    "mission_events::tests::event_log_serde_roundtrip"
    "mission_events::tests::event_log_summary_serde_roundtrip"
    "mission_events::tests::reason_codes_follow_naming_convention"
    "mission_events::tests::cycle_emitter_binds_context"
    "mission_events::tests::cycle_emitter_readiness_empty_uses_correct_reason"
    "mission_events::tests::cycle_emitter_scoring_below_threshold_uses_correct_reason"
    "mission_events::tests::cycle_emitter_safety_gate_rejection_maps_gate_name"
    "mission_events::tests::cycle_emitter_assignment_emitted_carries_details"
    "mission_events::tests::cycle_emitter_conflict_detected_maps_type"
    "mission_events::tests::clear_removes_all_events_preserves_counters"
    "mission_events::tests::sequence_monotonically_increases_across_eviction"
    "mission_events::tests::full_pipeline_emits_ordered_events"
)

TOTAL=${#TESTS[@]}
PASSED=0
FAILED=0

echo "Running $TOTAL mission_events tests..."
log_event "harness" "nominal_path" "test_count=$TOTAL" "started"

# ── Step 1: Compile check ─────────────────────────────────────────────────
echo "[1/$((TOTAL+2))] Compile check..."
COMPILE_OUTPUT="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_compile.log"
if $CARGO_CMD check -p frankenterm-core --features subprocess-bridge >"$COMPILE_OUTPUT" 2>&1; then
    check_rch_fallback_in_logs "mission_events_compile" "$COMPILE_OUTPUT"
    log_event "compile" "nominal_path" "cargo_check" "pass"
    echo "  ✓ Compile check passed"
else
    check_rch_fallback_in_logs "mission_events_compile" "$COMPILE_OUTPUT"
    log_event "compile" "failure_injection_path" "cargo_check" "fail" "compile_error" "CARGO-E001"
    echo "  ✗ Compile check FAILED"
    echo "Scenario: $SCENARIO_ID"
    echo "Logs: $LOG_FILE"
    exit 1
fi

# ── Step 2: Run all unit tests ─────────────────────────────────────────────
echo "[2/$((TOTAL+2))] Running unit tests..."
TEST_OUTPUT="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_test_stdout.log"

if $CARGO_CMD test -p frankenterm-core --features subprocess-bridge mission_events 2>&1 | tee "$TEST_OUTPUT"; then
    check_rch_fallback_in_logs "mission_events_suite" "$TEST_OUTPUT"
    log_event "unit_tests" "nominal_path" "mission_events_suite" "pass"
else
    check_rch_fallback_in_logs "mission_events_suite" "$TEST_OUTPUT"
    log_event "unit_tests" "failure_injection_path" "mission_events_suite" "partial_fail" "test_failure" "TEST-E001"
fi

# ── Step 3: Verify individual test results ─────────────────────────────────
echo "[3/$((TOTAL+2))] Verifying individual test results..."
for test_name in "${TESTS[@]}"; do
    short_name="${test_name##*::}"
    if grep -q "test ${test_name} ... ok" "$TEST_OUTPUT" 2>/dev/null || \
       grep -q "${short_name} ... ok" "$TEST_OUTPUT" 2>/dev/null; then
        PASSED=$((PASSED + 1))
        log_event "verify" "nominal_path" "$short_name" "pass"
    else
        FAILED=$((FAILED + 1))
        log_event "verify" "failure_injection_path" "$short_name" "fail" "test_not_found_in_output" "VERIFY-E001"
        echo "  ✗ $short_name"
    fi
done

# ── Summary ────────────────────────────────────────────────────────────────
echo ""
echo "═══════════════════════════════════════════"
echo "  Scenario: $SCENARIO_ID"
echo "  Passed: $PASSED / $TOTAL"
echo "  Failed: $FAILED / $TOTAL"
echo "  Logs: $LOG_FILE"
echo "═══════════════════════════════════════════"

log_event "summary" "completed" "passed=$PASSED,failed=$FAILED,total=$TOTAL" \
    "$([ "$FAILED" -eq 0 ] && echo 'pass' || echo 'partial_fail')"

[ "$FAILED" -eq 0 ] && exit 0 || exit 1
