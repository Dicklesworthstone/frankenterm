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

# ── rch offload infrastructure ────────────────────────────────────
RCH_TARGET_DIR="target/rch-e2e-vendored-migration-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/vendored_migration_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/vendored_migration_${RUN_ID}.smoke.log"

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
  local step_log="${LOG_DIR}/vendored_migration_${RUN_ID}.${step_name}.log"
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
