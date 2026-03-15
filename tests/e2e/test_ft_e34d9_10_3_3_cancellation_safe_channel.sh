#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_3_3_cancellation_safe_channel"
CORRELATION_ID="ft-e34d9.10.3.3-${RUN_ID}"
LOG_FILE="${LOG_DIR}/csc_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/csc_${RUN_ID}.stdout.log"
REPORT_OK="${LOG_DIR}/csc_${RUN_ID}.report.ok.json"
REPORT_FAIL="${LOG_DIR}/csc_${RUN_ID}.report.fail.json"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

# ── rch offload infrastructure ────────────────────────────────────
RCH_TARGET_DIR="target/rch-e2e-csc-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/csc_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/csc_${RUN_ID}.smoke.log"

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
    --arg component "csc.e2e" \
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
  local step_log="${LOG_DIR}/csc_${RUN_ID}.${step_name}.log"
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
  cargo test -p frankenterm-core --test proptest_cancellation_safe_channel --no-run

# ── Step 3: Run all proptest tests ──────────────────────────────────
run_step "proptest_csc_all" \
  cargo test -p frankenterm-core --test proptest_cancellation_safe_channel

# ── Step 4: Determinism rerun ───────────────────────────────────────
run_step "proptest_determinism_rerun" \
  cargo test -p frankenterm-core --test proptest_cancellation_safe_channel

# ── Step 5: No unsafe in module ─────────────────────────────────────
run_step "no_unsafe_in_module" \
  bash -c 'if grep -q "unsafe " crates/frankenterm-core/src/cancellation_safe_channel.rs; then echo "unsafe found"; exit 1; fi'

# ── Step 6: Unit tests ─────────────────────────────────────────────
run_step "unit_tests_csc" \
  cargo test -p frankenterm-core --lib cancellation_safe_channel::tests

# ── Step 7: Capacity invariant property ────────────────────────────
run_step "capacity_invariant" \
  cargo test -p frankenterm-core --test proptest_cancellation_safe_channel -- capacity_invariant_maintained

# ── Step 8: Sequence monotonicity ──────────────────────────────────
run_step "sequence_monotonicity" \
  cargo test -p frankenterm-core --test proptest_cancellation_safe_channel -- sequence_numbers_always_monotonic

# ── Step 9: Selective commit/rollback ──────────────────────────────
run_step "selective_commit_rollback" \
  cargo test -p frankenterm-core --test proptest_cancellation_safe_channel -- selective_commit_rollback

# ── Step 10: No message loss ───────────────────────────────────────
run_step "no_message_loss" \
  cargo test -p frankenterm-core --test proptest_cancellation_safe_channel -- no_message_loss_reserve_commit_cycle

# ── Step 11: Reserve guard commit ──────────────────────────────────
run_step "reserve_guard_commit" \
  cargo test -p frankenterm-core --test proptest_cancellation_safe_channel -- reserve_guard_commit_delivers

# ── Step 12: Reserve guard rollback on drop ────────────────────────
run_step "reserve_guard_rollback_on_drop" \
  cargo test -p frankenterm-core --test proptest_cancellation_safe_channel -- reserve_guard_drop_is_rollback

# ── Step 13: Consumer drain after close ────────────────────────────
run_step "consumer_drain_after_close" \
  cargo test -p frankenterm-core --test proptest_cancellation_safe_channel -- consumer_drain_after_close

# ── Step 14: Metrics serde roundtrip ───────────────────────────────
run_step "metrics_serde_roundtrip" \
  cargo test -p frankenterm-core --test proptest_cancellation_safe_channel -- serde_roundtrip_metrics

# ── Step 15: Capacity-1 edge case ──────────────────────────────────
run_step "capacity_one_edge_case" \
  cargo test -p frankenterm-core --test proptest_cancellation_safe_channel -- capacity_one_channel

# ── Step 16: Mixed reserve/send integrity ──────────────────────────
run_step "mixed_reserve_send_integrity" \
  cargo test -p frankenterm-core --test proptest_cancellation_safe_channel -- mixed_reserve_send_doesnt_corrupt

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
