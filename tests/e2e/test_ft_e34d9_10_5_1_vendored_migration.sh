#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_5_1_vendored_migration"
CORRELATION_ID="ft-e34d9.10.5.1-${RUN_ID}"
LOG_FILE="${LOG_DIR}/vendored_migration_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/vendored_migration_${RUN_ID}.stdout.log"
REPORT_OK="${LOG_DIR}/vendored_migration_${RUN_ID}.report.ok.json"
REPORT_FAIL="${LOG_DIR}/vendored_migration_${RUN_ID}.report.fail.json"
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
    --arg component "vendored_migration.e2e" \
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

# ── Step 1: Compile module (default features) ─────────────────────
run_step "compile_default_features" \
  cargo check -p frankenterm-core --tests

# ── Step 2: Compile test target with asupersync-runtime ───────────
run_step "compile_asupersync_feature" \
  cargo test -p frankenterm-core --test proptest_vendored_migration_map --features asupersync-runtime --no-run

# ── Step 3: Run all 19 tests ─────────────────────────────────────
run_step "proptest_vendored_migration_19" \
  cargo test -p frankenterm-core --test proptest_vendored_migration_map --features asupersync-runtime

# ── Step 4: Determinism rerun ─────────────────────────────────────
run_step "proptest_determinism_rerun" \
  cargo test -p frankenterm-core --test proptest_vendored_migration_map --features asupersync-runtime

# ── Step 5: No unsafe in module ───────────────────────────────────
run_step "no_unsafe_in_module" \
  bash -c 'if grep -q "unsafe " crates/frankenterm-core/src/vendored_migration_map.rs; then echo "unsafe found"; exit 1; fi'

# ── Step 6: JSON artifact exists and is valid ─────────────────────
run_step "json_artifact_valid" \
  jq -e '.version == 1 and .bead_id == "ft-e34d9.10.5.1" and .total_vendored_crates == 29' \
    docs/asupersync-vendored-migration-map.json

# ── Step 7: JSON artifact invariants ──────────────────────────────
run_step "json_artifact_smol_count" \
  jq -e '.global_smol_refs == 68' docs/asupersync-vendored-migration-map.json

run_step "json_artifact_zero_tokio" \
  jq -e '.global_tokio_refs == 0' docs/asupersync-vendored-migration-map.json

run_step "json_artifact_wave_count" \
  jq -e '.migration_waves | length == 6' docs/asupersync-vendored-migration-map.json

# ── Step 8: Dependency graph acyclicity ───────────────────────────
run_step "dependency_graph_acyclicity" \
  cargo test -p frankenterm-core --test proptest_vendored_migration_map --features asupersync-runtime -- no_entry_depends_on_itself

run_step "dependency_wave_ordering" \
  cargo test -p frankenterm-core --test proptest_vendored_migration_map --features asupersync-runtime -- dependency_wave_ordering

# ── Step 9: Serde roundtrip subset ────────────────────────────────
run_step "serde_roundtrip_difficulty" \
  cargo test -p frankenterm-core --test proptest_vendored_migration_map --features asupersync-runtime -- difficulty_serde_roundtrip

run_step "serde_roundtrip_map" \
  cargo test -p frankenterm-core --test proptest_vendored_migration_map --features asupersync-runtime -- canonical_map_serde_roundtrip

# ── Step 10: Failure injection — wave ordering violation ──────────
run_step "wave0_asupersync_gate_check" \
  cargo test -p frankenterm-core --test proptest_vendored_migration_map --features asupersync-runtime -- wave0_all_have_asupersync_gate

# ── Step 11: Recovery — all deps exist in map ─────────────────────
run_step "all_deps_exist" \
  cargo test -p frankenterm-core --test proptest_vendored_migration_map --features asupersync-runtime -- all_dependencies_exist_in_map

# ── Step 12: Inventory cross-check ────────────────────────────────
run_step "inventory_cross_check" \
  cargo test -p frankenterm-core --test proptest_vendored_migration_map --features asupersync-runtime -- total_refs_match_inventory

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
