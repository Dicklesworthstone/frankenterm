#!/usr/bin/env bash
# ────────────────────────────────────────────────────────────────────────────
# E2E Harness: ft-1i2ge.6.4 — Canary rollout controller and rollback triggers
#
# Validates canary_rollout_controller module: phase state machine (Shadow →
# Canary → Full), health-check-driven auto-advance, automatic rollback on
# degraded fidelity, assignment filtering per phase, canary agent selection,
# force transitions, metrics accumulation, and serde roundtrips.
#
# Execution: rch exec -- bash tests/e2e/test_ft_1i2ge_6_4.sh
# ────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SCENARIO_ID="ft-1i2ge-6-4"
LOG_DIR="$SCRIPT_DIR/logs"
TIMESTAMP="$(date -u +%Y%m%d_%H%M%S)"
LOG_FILE="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}.jsonl"

mkdir -p "$LOG_DIR"

DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-ft1i2ge-6-4"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
    CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
    CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${TIMESTAMP}" "1i2ge_6_4"
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

PASS=0
FAIL=0
SKIP=0

run_test() {
    local name="$1"
    shift
    local start
    local output_file="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_${name}.log"
    start=$(date +%s)
    log_event "test" "run" "$name" "started"

    if eval "$@" >"$output_file" 2>&1; then
        check_rch_fallback_in_logs "$name" "$output_file"
        local elapsed=$(( $(date +%s) - start ))
        log_event "test" "run" "$name" "pass" "nominal" "none"
        echo "  ✓ $name (${elapsed}s)"
        PASS=$((PASS + 1))
    else
        check_rch_fallback_in_logs "$name" "$output_file"
        local elapsed=$(( $(date +%s) - start ))
        log_event "test" "run" "$name" "fail" "assertion_failed" "TEST-E001"
        echo "  ✗ $name (${elapsed}s)"
        FAIL=$((FAIL + 1))
    fi
}

echo "═══════════════════════════════════════════════════════════════"
echo "  E2E: $SCENARIO_ID — Canary Rollout Controller"
echo "═══════════════════════════════════════════════════════════════"

# ── 1. Module existence and feature gate ──────────────────────────────────
echo ""
echo "── Phase 1: Module existence & feature gate ──"

run_test "module_exists" \
    "test -f crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "module_declared_in_lib" \
    "grep -q 'pub mod canary_rollout_controller' crates/frankenterm-core/src/lib.rs"

run_test "feature_gated_subprocess_bridge" \
    "grep -B1 'pub mod canary_rollout_controller' crates/frankenterm-core/src/lib.rs | grep -q 'subprocess-bridge'"

# ── 2. Compilation ────────────────────────────────────────────────────────
echo ""
echo "── Phase 2: Compilation ──"

run_test "compiles_with_feature" \
    "$CARGO_CMD check -p frankenterm-core --features subprocess-bridge 2>&1"

run_test "compiles_without_feature" \
    "$CARGO_CMD check -p frankenterm-core 2>&1"

# ── 3. Unit tests ─────────────────────────────────────────────────────────
echo ""
echo "── Phase 3: Unit tests (42 tests) ──"

run_test "unit_tests_pass" \
    "$CARGO_CMD test -p frankenterm-core --lib --features subprocess-bridge -- canary_rollout_controller 2>&1"

# ── 4. Property tests ────────────────────────────────────────────────────
echo ""
echo "── Phase 4: Property tests (10 tests) ──"

run_test "proptest_file_exists" \
    "test -f crates/frankenterm-core/tests/proptest_canary_rollout.rs"

run_test "proptests_pass" \
    "$CARGO_CMD test -p frankenterm-core --test proptest_canary_rollout --features subprocess-bridge 2>&1"

# ── 5. API surface checks ────────────────────────────────────────────────
echo ""
echo "── Phase 5: API surface ──"

run_test "exports_canary_phase" \
    "grep -q 'pub enum CanaryPhase' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "exports_canary_rollout_config" \
    "grep -q 'pub struct CanaryRolloutConfig' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "exports_canary_rollout_controller" \
    "grep -q 'pub struct CanaryRolloutController' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "exports_canary_decision" \
    "grep -q 'pub struct CanaryDecision' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "exports_canary_health_check" \
    "grep -q 'pub struct CanaryHealthCheck' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "exports_canary_action" \
    "grep -q 'pub enum CanaryAction' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "exports_canary_metrics" \
    "grep -q 'pub struct CanaryMetrics' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "exports_health_failure_reason" \
    "grep -q 'pub enum HealthFailureReason' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "exports_canary_phase_transition" \
    "grep -q 'pub struct CanaryPhaseTransition' crates/frankenterm-core/src/canary_rollout_controller.rs"

# ── 6. Phase state machine invariants ─────────────────────────────────────
echo ""
echo "── Phase 6: Phase state machine invariants ──"

run_test "phase_enum_has_shadow" \
    "grep -q 'Shadow' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "phase_enum_has_canary" \
    "grep -q 'Canary' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "phase_enum_has_full" \
    "grep -q 'Full' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "phase_next_method" \
    "grep -q 'fn next' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "can_advance_to_method" \
    "grep -q 'fn can_advance_to' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "can_rollback_to_method" \
    "grep -q 'fn can_rollback_to' crates/frankenterm-core/src/canary_rollout_controller.rs"

# ── 7. Controller methods ────────────────────────────────────────────────
echo ""
echo "── Phase 7: Controller methods ──"

run_test "evaluate_health_method" \
    "grep -q 'fn evaluate_health' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "filter_assignments_method" \
    "grep -q 'fn filter_assignments' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "force_transition_method" \
    "grep -q 'fn force_transition' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "update_canary_agents_method" \
    "grep -q 'fn update_canary_agents' crates/frankenterm-core/src/canary_rollout_controller.rs"

run_test "reset_method" \
    "grep -q 'fn reset' crates/frankenterm-core/src/canary_rollout_controller.rs"

# ── 8. Serde support ─────────────────────────────────────────────────────
echo ""
echo "── Phase 8: Serde support ──"

run_test "config_serde" \
    "grep -A1 'pub struct CanaryRolloutConfig' crates/frankenterm-core/src/canary_rollout_controller.rs | head -1 && grep -B3 'pub struct CanaryRolloutConfig' crates/frankenterm-core/src/canary_rollout_controller.rs | grep -q 'Serialize'"

run_test "phase_serde" \
    "grep -B3 'pub enum CanaryPhase' crates/frankenterm-core/src/canary_rollout_controller.rs | grep -q 'Serialize'"

run_test "decision_serde" \
    "grep -B3 'pub struct CanaryDecision' crates/frankenterm-core/src/canary_rollout_controller.rs | grep -q 'Serialize'"

# ── 9. Clippy ─────────────────────────────────────────────────────────────
echo ""
echo "── Phase 9: Clippy ──"

run_test "clippy_clean" \
    "! $CARGO_CMD clippy -p frankenterm-core --features subprocess-bridge 2>&1 | grep 'canary_rollout_controller' | grep -q 'error'"

# ── Summary ──────────────────────────────────────────────────────────────
echo ""
echo "═══════════════════════════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed, $SKIP skipped"
echo "  Log: $LOG_FILE"
echo "═══════════════════════════════════════════════════════════════"

log_event "summary" "complete" "pass=${PASS},fail=${FAIL},skip=${SKIP}" \
    "$([ "$FAIL" -eq 0 ] && echo pass || echo fail)"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
