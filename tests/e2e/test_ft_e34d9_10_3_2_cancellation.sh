#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_3_2_cancellation"
CORRELATION_ID="ft-e34d9.10.3.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/cancellation_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/cancellation_${RUN_ID}.stdout.log"
REPORT_OK="${LOG_DIR}/cancellation_${RUN_ID}.report.ok.json"
REPORT_FAIL="${LOG_DIR}/cancellation_${RUN_ID}.report.fail.json"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

# ── rch offload infrastructure ────────────────────────────────────
RCH_TARGET_DIR="target/rch-e2e-cancellation-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/cancellation_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/cancellation_${RUN_ID}.smoke.log"

fatal() { echo "FATAL: $1" >&2; exit 1; }

run_rch() { TMPDIR=/tmp rch "$@"; }

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
    local output_file="$1"; shift
    set +e
    ( cd "${ROOT_DIR}"; run_rch_cargo "$@" ) >"${output_file}" 2>&1
    local rc=$?; set -e
    check_rch_fallback "${output_file}"
    return "${rc}"
}

ensure_rch_ready() {
    if ! command -v rch >/dev/null 2>&1; then
        fatal "rch is required for this e2e harness; refusing local cargo execution."
    fi
    set +e; run_rch --json workers probe --all >"${RCH_PROBE_LOG}" 2>&1; local probe_rc=$?; set -e
    if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${RCH_PROBE_LOG}"; then
        fatal "rch workers are unavailable; refusing local cargo execution. See ${RCH_PROBE_LOG}"
    fi
    set +e; run_rch_cargo check --help >"${RCH_SMOKE_LOG}" 2>&1; local smoke_rc=$?; set -e
    check_rch_fallback "${RCH_SMOKE_LOG}"
    if [[ ${smoke_rc} -ne 0 ]]; then
        fatal "rch remote smoke preflight failed; refusing local cargo execution. See ${RCH_SMOKE_LOG}"
    fi
}

# ── end rch infrastructure ────────────────────────────────────────

emit_log() {
  local outcome="$1"
  local decision_path="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local input_summary="$6"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "cancellation.e2e" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg input_summary "${input_summary}" \
    --arg decision_path "${decision_path}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path}" \
    '{timestamp: $timestamp, component: $component, scenario_id: $scenario_id, correlation_id: $correlation_id, run_id: $run_id, input: $input_summary, decision_path: $decision_path, outcome: $outcome, reason_code: $reason_code, error_code: $error_code, artifact: $artifact_path}' \
    >> "${LOG_FILE}"
}

pass=0
fail=0

run_step() {
  local step_name="$1"
  shift
  local step_log="${LOG_DIR}/cancellation_${RUN_ID}.${step_name}.log"
  printf "[%s] STEP %-55s " "$(date +%H:%M:%S)" "${step_name}"
  local output rc
  if [[ "$1" == "cargo" ]]; then
    # Route cargo commands through rch offloading
    shift  # strip "cargo" — run_rch_cargo_logged re-adds it
    set +e; output=$(run_rch_cargo_logged "${step_log}" "$@" 2>&1); rc=$?; set -e
  else
    # Non-cargo commands run locally
    set +e; output=$("$@" 2>&1); rc=$?; set -e
  fi
  if [[ ${rc} -eq 0 ]]; then
    echo "PASS"
    emit_log "pass" "${step_name}" "ok" "" "" "${step_name}"
    pass=$((pass + 1))
  else
    echo "FAIL"
    if [[ -f "${step_log}" ]]; then
      tail -20 "${step_log}"
    else
      echo "${output}" | tail -20
    fi
    emit_log "fail" "${step_name}" "command_failed" "E-STEP" "${STDOUT_FILE}" "${step_name}"
    fail=$((fail + 1))
  fi
}

echo "================================================================"
echo "  E2E: ${SCENARIO_ID}"
echo "  Run: ${RUN_ID}"
echo "  Log: ${LOG_FILE_REL}"
echo "================================================================"
echo ""

# ── rch preflight ─────────────────────────────────────────────────
ensure_rch_ready

# ── Step 1: Compile module (default features) ─────────────────────
run_step "compile_default_features" \
  cargo check -p frankenterm-core --tests

# ── Step 2: Compile proptest target ─────────────────────────────────
run_step "compile_proptest_target" \
  cargo test -p frankenterm-core --test proptest_cancellation --no-run

# ── Step 3: Run all proptest tests ──────────────────────────────────
run_step "proptest_cancellation_all" \
  cargo test -p frankenterm-core --test proptest_cancellation

# ── Step 4: Determinism rerun ───────────────────────────────────────
run_step "proptest_determinism_rerun" \
  cargo test -p frankenterm-core --test proptest_cancellation

# ── Step 5: No unsafe in module ─────────────────────────────────────
run_step "no_unsafe_in_module" \
  bash -c 'if grep -q "unsafe " crates/frankenterm-core/src/cancellation.rs; then echo "unsafe found"; exit 1; fi'

# ── Step 6: Unit tests ─────────────────────────────────────────────
run_step "unit_tests_cancellation" \
  cargo test -p frankenterm-core --lib cancellation::tests

# ── Step 7: Serde roundtrip — all reason variants ──────────────────
run_step "serde_roundtrip_reasons" \
  cargo test -p frankenterm-core --test proptest_cancellation -- shutdown_reason_serde_roundtrip

# ── Step 8: Serde roundtrip — policy ───────────────────────────────
run_step "serde_roundtrip_policy" \
  cargo test -p frankenterm-core --test proptest_cancellation -- shutdown_policy_serde_roundtrip

# ── Step 9: Cancellation propagation depth ─────────────────────────
run_step "cancellation_propagation_depth" \
  cargo test -p frankenterm-core --test proptest_cancellation -- cancellation_propagates_depth

# ── Step 10: Child independence ─────────────────────────────────────
run_step "cancellation_child_independence" \
  cargo test -p frankenterm-core --test proptest_cancellation -- cancellation_child_independence

# ── Step 11: Grace period detection ─────────────────────────────────
run_step "grace_period_detection" \
  cargo test -p frankenterm-core --test proptest_cancellation -- grace_period_detection_consistent

# ── Step 12: Finalizer ordering ─────────────────────────────────────
run_step "finalizer_priority_ordering" \
  cargo test -p frankenterm-core --test proptest_cancellation -- finalizer_priority_ordering_maintained

# ── Step 13: Full two-phase lifecycle ───────────────────────────────
run_step "full_two_phase_lifecycle" \
  cargo test -p frankenterm-core --test proptest_cancellation -- full_two_phase_with_cascade_and_finalizers

# ── Step 14: Force-close escalation ─────────────────────────────────
run_step "force_close_escalation" \
  cargo test -p frankenterm-core --test proptest_cancellation -- force_close_skips_finalizers

# ── Step 15: Shutdown summary invariants ────────────────────────────
run_step "shutdown_summary_invariants" \
  cargo test -p frankenterm-core --test proptest_cancellation -- shutdown_summary_invariants

# ── Step 16: Tier grace period ordering ─────────────────────────────
run_step "tier_grace_ordering" \
  cargo test -p frankenterm-core --test proptest_cancellation -- tier_default_policy_grace_ordering

# ── Report ────────────────────────────────────────────────────────
total=$((pass + fail))
echo ""
echo "================================================================"
echo "  RESULT: ${pass}/${total} passed, ${fail} failed"
echo "  Log:    ${LOG_FILE_REL}"
echo "================================================================"

if [ "${fail}" -gt 0 ]; then
  jq -cn \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg run_id "${RUN_ID}" \
    --argjson pass "${pass}" \
    --argjson fail "${fail}" \
    --argjson total "${total}" \
    '{scenario_id: $scenario_id, run_id: $run_id, result: "FAIL", pass: $pass, fail: $fail, total: $total}' \
    > "${REPORT_FAIL}"
  exit 1
else
  jq -cn \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg run_id "${RUN_ID}" \
    --argjson pass "${pass}" \
    --argjson total "${total}" \
    '{scenario_id: $scenario_id, run_id: $run_id, result: "PASS", pass: $pass, total: $total}' \
    > "${REPORT_OK}"
  exit 0
fi
