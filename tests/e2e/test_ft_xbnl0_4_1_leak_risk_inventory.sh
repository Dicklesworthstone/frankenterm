#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_ID="$(date +"%Y%m%dT%H%M%SZ")"
BEAD_ID="ft-xbnl0.4.1"
SCENARIO_ID="leak_risk_inventory"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/artifacts/goal-line/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}"
TARGET_DIR="target/rch-${BEAD_ID//./-}-${SCENARIO_ID}"
mkdir -p "${ARTIFACT_DIR}"

COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
ENV_FILE="${ARTIFACT_DIR}/env.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"

exec > >(tee -a "${STDOUT_FILE}") 2> >(tee -a "${STDERR_FILE}" >&2)

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
RCH_STEP_TIMEOUT_SECS=2400
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_xbnl0_4_1_leak_risk_inventory"
export RCH_SKIP_SMOKE_PREFLIGHT=1
ensure_rch_ready

printf 'bead_id=%s\nscenario_id=%s\ncorrelation_id=%s\n' \
  "${BEAD_ID}" "${SCENARIO_ID}" "${CORRELATION_ID}" > "${COMMANDS_FILE}"
env | sort > "${ENV_FILE}"
PLATFORM="$(uname -s)-$(uname -m)"

emit_log() {
  local step="$1"
  local status="$2"
  local duration_ms="$3"
  local command="$4"
  jq -cn \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg surface "leak-risk-inventory" \
    --arg step "${step}" \
    --arg status "${status}" \
    --arg duration_ms "${duration_ms}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg backend "rch" \
    --arg platform "${PLATFORM}" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    --arg redaction "none" \
    --arg command "${command}" \
    '{
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      surface: $surface,
      step: $step,
      status: $status,
      duration_ms: ($duration_ms | tonumber),
      correlation_id: $correlation_id,
      backend: $backend,
      platform: $platform,
      artifact_dir: $artifact_dir,
      redaction: $redaction,
      command: $command
    }' >> "${STRUCTURED_LOG}"
}

write_failure_summary() {
  local step="$1"
  local command="$2"
  local artifact="$3"
  jq -cn \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    --arg failed_step "${step}" \
    --arg command "${command}" \
    --arg artifact "${artifact}" \
    '{
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      status: "failed",
      correlation_id: $correlation_id,
      artifact_dir: $artifact_dir,
      failed_step: $failed_step,
      command: $command,
      artifact: $artifact
    }' > "${SUMMARY_FILE}"
}

run_cargo_step() {
  local step="$1"
  shift
  local output_file="${ARTIFACT_DIR}/${step}.log"
  local command="cargo $*"
  printf '%s\n' "${command}" >> "${COMMANDS_FILE}"
  local start_ns
  start_ns="$(date +%s%N)"
  emit_log "${step}" "started" "0" "${command}"
  if run_rch_cargo_logged "${output_file}" env CARGO_TARGET_DIR="${TARGET_DIR}" cargo "$@"; then
    local end_ns
    end_ns="$(date +%s%N)"
    local duration_ms=$(( (end_ns - start_ns) / 1000000 ))
    emit_log "${step}" "passed" "${duration_ms}" "${command}"
  else
    local end_ns
    end_ns="$(date +%s%N)"
    local duration_ms=$(( (end_ns - start_ns) / 1000000 ))
    emit_log "${step}" "failed" "${duration_ms}" "${command}"
    write_failure_summary "${step}" "${command}" "${output_file}"
    exit 1
  fi
}

run_shell_step() {
  local step="$1"
  local command="$2"
  local output_file="${ARTIFACT_DIR}/${step}.log"
  printf '%s\n' "${command}" >> "${COMMANDS_FILE}"
  local start_ns
  start_ns="$(date +%s%N)"
  emit_log "${step}" "started" "0" "${command}"
  if (cd "${ROOT_DIR}" && eval "${command}") >"${output_file}" 2>&1; then
    local end_ns
    end_ns="$(date +%s%N)"
    local duration_ms=$(( (end_ns - start_ns) / 1000000 ))
    emit_log "${step}" "passed" "${duration_ms}" "${command}"
  else
    local end_ns
    end_ns="$(date +%s%N)"
    local duration_ms=$(( (end_ns - start_ns) / 1000000 ))
    emit_log "${step}" "failed" "${duration_ms}" "${command}"
    write_failure_summary "${step}" "${command}" "${output_file}"
    exit 1
  fi
}

echo "=== ${BEAD_ID} leak-risk inventory verification ==="
echo "Artifacts: ${ARTIFACT_DIR}"

run_cargo_step "runtime_inventory_unit" \
  test -p frankenterm-core --lib leak_risk_inventory_counts_registry_and_watchdog_state -- --nocapture
run_cargo_step "plain_surface_capture" \
  test -p frankenterm-core --lib health_snapshot_plain_shows_leak_risk_inventory -- --nocapture
run_cargo_step "compact_surface_capture" \
  test -p frankenterm-core --lib health_compact_shows_leak_risk_summary -- --nocapture
run_cargo_step "schema_roundtrip" \
  test -p frankenterm-core --lib health_snapshot_with_all_optional_fields -- --nocapture
run_cargo_step "schema_backcompat" \
  test -p frankenterm-core --lib health_snapshot_default_optional_fields_deserialize -- --nocapture
run_cargo_step "core_check" check -p frankenterm-core --lib --tests
run_cargo_step "core_clippy" clippy --no-deps -p frankenterm-core --lib --tests -- -D warnings
run_cargo_step "fmt_check" fmt --check

run_shell_step "source_inventory_audit" \
  "rg -n 'LeakRiskInventorySnapshot|LeakRiskWatchdogSnapshot|build_leak_risk_inventory|lifecycle inventory|watchdog health' crates/frankenterm-core/src/crash.rs crates/frankenterm-core/src/runtime.rs crates/frankenterm-core/src/output/renderers.rs"
run_shell_step "docs_inventory_surface_map" \
  "rg -n 'Leak-Risk Inventory Surface Map|tracked_pane_entries|pane_arena_count|watchdog\\.overall|diagnostic_checks' docs/ft-xbnl0-verification-contract.md"

jq -cn \
  --arg bead_id "${BEAD_ID}" \
  --arg scenario_id "${SCENARIO_ID}" \
  --arg correlation_id "${CORRELATION_ID}" \
  --arg artifact_dir "${ARTIFACT_DIR}" \
  --arg commands_file "${COMMANDS_FILE}" \
  --arg env_file "${ENV_FILE}" \
  --arg structured_log "${STRUCTURED_LOG}" \
  --arg stdout_file "${STDOUT_FILE}" \
  --arg stderr_file "${STDERR_FILE}" \
  --arg rch_probe_log "$(rch_probe_log_path)" \
  --arg rch_probe_meta "$(rch_log_meta_path "$(rch_probe_log_path)")" \
  --arg rch_smoke_log "$(rch_smoke_log_path)" \
  --arg rch_smoke_meta "$(rch_log_meta_path "$(rch_smoke_log_path)")" \
  --arg runtime_inventory_unit "${ARTIFACT_DIR}/runtime_inventory_unit.log" \
  --arg plain_surface_capture "${ARTIFACT_DIR}/plain_surface_capture.log" \
  --arg compact_surface_capture "${ARTIFACT_DIR}/compact_surface_capture.log" \
  --arg schema_roundtrip "${ARTIFACT_DIR}/schema_roundtrip.log" \
  --arg schema_backcompat "${ARTIFACT_DIR}/schema_backcompat.log" \
  --arg core_check "${ARTIFACT_DIR}/core_check.log" \
  --arg core_clippy "${ARTIFACT_DIR}/core_clippy.log" \
  --arg fmt_check "${ARTIFACT_DIR}/fmt_check.log" \
  --arg source_inventory_audit "${ARTIFACT_DIR}/source_inventory_audit.log" \
  --arg docs_inventory_surface_map "${ARTIFACT_DIR}/docs_inventory_surface_map.log" \
  '{
    bead_id: $bead_id,
    scenario_id: $scenario_id,
    status: "passed",
    correlation_id: $correlation_id,
    artifact_dir: $artifact_dir,
    artifacts: {
      commands: $commands_file,
      env: $env_file,
      structured_log: $structured_log,
      stdout: $stdout_file,
      stderr: $stderr_file,
      rch_probe: $rch_probe_log,
      rch_probe_meta: $rch_probe_meta,
      rch_smoke: $rch_smoke_log,
      rch_smoke_meta: $rch_smoke_meta,
      runtime_inventory_unit: $runtime_inventory_unit,
      plain_surface_capture: $plain_surface_capture,
      compact_surface_capture: $compact_surface_capture,
      schema_roundtrip: $schema_roundtrip,
      schema_backcompat: $schema_backcompat,
      core_check: $core_check,
      core_clippy: $core_clippy,
      fmt_check: $fmt_check,
      source_inventory_audit: $source_inventory_audit,
      docs_inventory_surface_map: $docs_inventory_surface_map
    }
  }' > "${SUMMARY_FILE}"

echo "Summary: ${SUMMARY_FILE}"
