#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_3_4_scope_watchdog"
CORRELATION_ID="ft-e34d9.10.3.4-${RUN_ID}"
LOG_FILE="${LOG_DIR}/scope_watchdog_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/scope_watchdog_${RUN_ID}.stdout.log"
REPORT_OK="${LOG_DIR}/scope_watchdog_${RUN_ID}.report.ok.json"
REPORT_FAIL="${LOG_DIR}/scope_watchdog_${RUN_ID}.report.fail.json"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

# ── rch offload infrastructure ────────────────────────────────────
RCH_TARGET_DIR="target/rch-e2e-scope-watchdog-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/scope_watchdog_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/scope_watchdog_${RUN_ID}.smoke.log"

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
    --arg component "scope_watchdog.e2e" \
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
  local step_log="${LOG_DIR}/scope_watchdog_${RUN_ID}.${step_name}.log"
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

# ── Step 1: Compile module ─────────────────────────────────────────
run_step "compile_default" \
  cargo check -p frankenterm-core --tests

# ── Step 2: Compile proptest target ─────────────────────────────────
run_step "compile_proptest" \
  cargo test -p frankenterm-core --test proptest_scope_watchdog --no-run

# ── Step 3: Run all proptest tests ──────────────────────────────────
run_step "proptest_all" \
  cargo test -p frankenterm-core --test proptest_scope_watchdog

# ── Step 4: Determinism rerun ───────────────────────────────────────
run_step "determinism_rerun" \
  cargo test -p frankenterm-core --test proptest_scope_watchdog

# ── Step 5: No unsafe in module ─────────────────────────────────────
run_step "no_unsafe" \
  bash -c 'if grep -q "unsafe " crates/frankenterm-core/src/scope_watchdog.rs; then echo "unsafe found"; exit 1; fi'

# ── Step 6: Unit tests ─────────────────────────────────────────────
run_step "unit_tests" \
  cargo test -p frankenterm-core --lib scope_watchdog::tests

# ── Step 7: Stuck cancellation detection ───────────────────────────
run_step "stuck_cancellation_detection" \
  cargo test -p frankenterm-core --test proptest_scope_watchdog -- stuck_cancellation_threshold_accurate

# ── Step 8: Scope leak detection ───────────────────────────────────
run_step "scope_leak_detection" \
  cargo test -p frankenterm-core --test proptest_scope_watchdog -- scope_leak_threshold_accurate

# ── Step 9: Severity escalation ────────────────────────────────────
run_step "severity_escalation" \
  cargo test -p frankenterm-core --test proptest_scope_watchdog -- severity_escalation_stuck

# ── Step 10: Multiple detectors ────────────────────────────────────
run_step "multiple_detectors" \
  cargo test -p frankenterm-core --test proptest_scope_watchdog -- multiple_detectors_fire_independently

# ── Step 11: Clean tree clean scan ─────────────────────────────────
run_step "clean_tree_clean_scan" \
  cargo test -p frankenterm-core --test proptest_scope_watchdog -- clean_tree_always_clean

# ── Step 12: Config serde roundtrip ────────────────────────────────
run_step "config_serde_roundtrip" \
  cargo test -p frankenterm-core --test proptest_scope_watchdog -- config_serde_roundtrip

# ── Step 13: Scan summary consistency ──────────────────────────────
run_step "scan_summary_consistency" \
  cargo test -p frankenterm-core --test proptest_scope_watchdog -- scan_summary_counts_consistent

# ── Step 14: Alert display variants ────────────────────────────────
run_step "alert_display_variants" \
  cargo test -p frankenterm-core --test proptest_scope_watchdog -- alert_display_variants

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
