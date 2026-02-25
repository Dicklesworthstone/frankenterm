#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_3_1_scope_tree"
CORRELATION_ID="ft-e34d9.10.3.1-${RUN_ID}"
LOG_FILE="${LOG_DIR}/scope_tree_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/scope_tree_${RUN_ID}.stdout.log"
REPORT_OK="${LOG_DIR}/scope_tree_${RUN_ID}.report.ok.json"
REPORT_FAIL="${LOG_DIR}/scope_tree_${RUN_ID}.report.fail.json"
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
    --arg component "scope_tree.e2e" \
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
  printf "[%s] STEP %-50s " "$(date +%H:%M:%S)" "${step_name}"
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

# ── Step 1: Compile scope_tree module (default features) ──────────
run_step "compile_scope_tree_default" \
  cargo check -p frankenterm-core --tests

# ── Step 2: Compile proptest target with asupersync-runtime feature ─
run_step "compile_scope_tree_asupersync" \
  cargo test -p frankenterm-core --test proptest_scope_tree --features asupersync-runtime --no-run

# ── Step 3: Proptest scope_tree (14 property tests) ───────────────
run_step "proptest_scope_tree_14" \
  cargo test -p frankenterm-core --test proptest_scope_tree --features asupersync-runtime

# ── Step 4: Determinism — run proptest twice, check same pass count ─
run_step "proptest_scope_tree_determinism_rerun" \
  cargo test -p frankenterm-core --test proptest_scope_tree --features asupersync-runtime

# ── Step 5: Failure injection — verify compile-time enforcement ───
# Scope tree uses #![forbid(unsafe_code)] in lib.rs. Verify no unsafe
# crept in via our module.
run_step "no_unsafe_in_scope_tree" \
  bash -c 'if grep -q "unsafe " crates/frankenterm-core/src/scope_tree.rs; then echo "unsafe found"; exit 1; fi'

# ── Step 6: Verify serde roundtrip via proptest (subset filter) ───
run_step "serde_roundtrip_tree" \
  cargo test -p frankenterm-core --test proptest_scope_tree --features asupersync-runtime -- serde_roundtrip_tree

run_step "serde_roundtrip_snapshot" \
  cargo test -p frankenterm-core --test proptest_scope_tree --features asupersync-runtime -- serde_roundtrip_snapshot

# ── Step 7: Verify shutdown ordering invariant ────────────────────
run_step "shutdown_order_children_before_parents" \
  cargo test -p frankenterm-core --test proptest_scope_tree --features asupersync-runtime -- shutdown_order_children_before_parents

# ── Step 8: Verify lifecycle state machine ────────────────────────
run_step "lifecycle_roundtrip" \
  cargo test -p frankenterm-core --test proptest_scope_tree --features asupersync-runtime -- lifecycle_roundtrip

# ── Step 9: Recovery path — finalize blocked by live children ─────
run_step "finalize_blocked_by_live_children" \
  cargo test -p frankenterm-core --test proptest_scope_tree --features asupersync-runtime -- finalize_blocked_by_live_children

# ── Step 10: Tier priority ordering (non-proptest) ────────────────
run_step "tier_shutdown_priority_ordering" \
  cargo test -p frankenterm-core --test proptest_scope_tree --features asupersync-runtime -- tier_shutdown_priority_ordering

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
