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
  printf "[%s] STEP %-55s " "$(date +%H:%M:%S)" "${step_name}"
  if output=$("$@" 2>&1); then
    echo "PASS"
    emit_log "pass" "${step_name}" "ok" "" "" "${step_name}"
    pass=$((pass + 1))
  else
    echo "FAIL"
    echo "${output}" | tail -20
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
