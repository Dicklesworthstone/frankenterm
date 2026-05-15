#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="${SWARM_CAPACITY_RESOURCE_BUDGET_BEAD_ID:-ft-b94bx.3}"
RUN_ID="${SWARM_CAPACITY_RESOURCE_BUDGET_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
ARTIFACT_ROOT="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/swarm_capacity_resource_budget/${RUN_ID}"
LOG_FILE="${ARTIFACT_ROOT}/events.jsonl"
DOC_FILE="${ROOT_DIR}/docs/swarm-capacity-resource-budget-model.md"
SOURCE_FILE="${ROOT_DIR}/crates/frankenterm-core/src/runtime_telemetry.rs"
TEST_FILE="${ROOT_DIR}/crates/frankenterm-core/tests/swarm_capacity_resource_budget_model.rs"
TARGET_CLASS_ARTIFACT="${ROOT_DIR}/tests/e2e/artifacts/target-class/linux-x86_64-high-core/20260512T150000Z/summary.json"
RUN_RUST_PROOF=0

mkdir -p "${ARTIFACT_ROOT}"
: >"${LOG_FILE}"

usage() {
  cat <<'USAGE'
Usage: bash tests/e2e/test_swarm_capacity_resource_budget_model.sh [--run-rust-proof]

Static JSONL smoke checks run locally. --run-rust-proof uses rch and refuses
local Cargo fallback.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --run-rust-proof)
      RUN_RUST_PROOF=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

emit_event() {
  local item_id="$1"
  local domain="$2"
  local step="$3"
  local outcome="$4"
  local evidence_state="$5"
  local reason_code="$6"
  local error_code="$7"
  local artifact_path="$8"
  local selected_worker="${9:-}"
  local cargo_reached="${10:-false}"
  local rustc_reached="${11:-false}"
  local test_execution_reached="${12:-false}"

  jq -cn \
    --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg bead_id "${BEAD_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg item_id "${item_id}" \
    --arg domain "${domain}" \
    --arg step "${step}" \
    --arg outcome "${outcome}" \
    --arg evidence_state "${evidence_state}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path#"${ROOT_DIR}/"}" \
    --arg selected_worker "${selected_worker}" \
    --argjson cargo_reached "${cargo_reached}" \
    --argjson rustc_reached "${rustc_reached}" \
    --argjson test_execution_reached "${test_execution_reached}" \
    '{
      timestamp: $timestamp,
      bead_id: $bead_id,
      run_id: $run_id,
      item_id: $item_id,
      domain: $domain,
      step: $step,
      outcome: $outcome,
      evidence_state: $evidence_state,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path,
      selected_worker: ($selected_worker | if . == "" then null else . end),
      cargo_reached: $cargo_reached,
      rustc_reached: $rustc_reached,
      test_execution_reached: $test_execution_reached
    }' >>"${LOG_FILE}"
}

require_command() {
  local command_name="$1"
  if ! command -v "${command_name}" >/dev/null 2>&1; then
    emit_event "${command_name}" "static" "preflight" "failed" "unavailable" "capacity.resource_budget.tool_missing" "${command_name}_not_found" "${LOG_FILE}"
    exit 1
  fi
}

require_file() {
  local path="$1"
  local label="$2"
  if [[ ! -f "${path}" ]]; then
    emit_event "${label}" "static" "preflight" "failed" "unavailable" "capacity.resource_budget.artifact_missing" "missing_artifact" "${path}"
    exit 1
  fi
}

emit_event "suite" "e2e_jsonl" "start" "running" "mixed" "capacity.resource_budget.started" "none" "${LOG_FILE}"

require_command jq
require_file "${DOC_FILE}" "doc"
require_file "${SOURCE_FILE}" "source"
require_file "${TEST_FILE}" "test"
require_file "${TARGET_CLASS_ARTIFACT}" "target_class_artifact"

required_classes=(low mid high high_core)
required_subsystems=(build_slots child_processes memory_tiers sqlite_cache mux_render rch_offload)

for class in "${required_classes[@]}"; do
  if ! grep -q "\`${class}\`" "${DOC_FILE}"; then
    emit_event "${class}" "doc" "hardware_class_documented" "failed" "unavailable" "capacity.resource_budget.class_missing" "class_missing" "${DOC_FILE}"
    exit 1
  fi
  if ! grep -q "${class}" "${SOURCE_FILE}"; then
    emit_event "${class}" "source" "hardware_class_defined" "failed" "unavailable" "capacity.resource_budget.class_missing_source" "class_missing_source" "${SOURCE_FILE}"
    exit 1
  fi
  emit_event "${class}" "hardware_class" "class_contract" "present" "measured" "capacity.resource_budget.class_present" "none" "${DOC_FILE}"
done

for subsystem in "${required_subsystems[@]}"; do
  if ! grep -q "\`${subsystem}\`" "${DOC_FILE}"; then
    emit_event "${subsystem}" "doc" "subsystem_documented" "failed" "unavailable" "capacity.resource_budget.subsystem_missing" "subsystem_missing" "${DOC_FILE}"
    exit 1
  fi
  if ! grep -q "${subsystem}" "${SOURCE_FILE}"; then
    emit_event "${subsystem}" "source" "subsystem_defined" "failed" "unavailable" "capacity.resource_budget.subsystem_missing_source" "subsystem_missing_source" "${SOURCE_FILE}"
    exit 1
  fi
  emit_event "${subsystem}" "subsystem" "subsystem_contract" "present" "measured" "capacity.resource_budget.subsystem_present" "none" "${DOC_FILE}"
done

if ! grep -q "SwarmCapacityHardwareFingerprint::new(None" "${TEST_FILE}"; then
  emit_event "lower_bound" "test" "missing_telemetry_gate" "failed" "unavailable" "capacity.resource_budget.lower_bound_test_missing" "lower_bound_test_missing" "${TEST_FILE}"
  exit 1
fi
emit_event "lower_bound" "test" "missing_telemetry_gate" "present" "measured" "capacity.resource_budget.lower_bound_test_present" "none" "${TEST_FILE}"

if ! grep -q "saturation_per_1000" "${TEST_FILE}"; then
  emit_event "saturation" "test" "overflow_gate" "failed" "unavailable" "capacity.resource_budget.saturation_test_missing" "saturation_test_missing" "${TEST_FILE}"
  exit 1
fi
emit_event "saturation" "test" "overflow_gate" "present" "measured" "capacity.resource_budget.saturation_test_present" "none" "${TEST_FILE}"

if ! grep -q "20260512T150000Z" "${TEST_FILE}"; then
  emit_event "target_class" "test" "artifact_regression_gate" "failed" "unavailable" "capacity.resource_budget.target_class_test_missing" "target_class_test_missing" "${TEST_FILE}"
  exit 1
fi
emit_event "target_class" "test" "artifact_regression_gate" "present" "measured" "capacity.resource_budget.target_class_test_present" "none" "${TARGET_CLASS_ARTIFACT}"

if [[ "$(jq -r '.hardware_predicate.proof_status' "${TARGET_CLASS_ARTIFACT}")" != "skipped_not_proven" ]]; then
  emit_event "target_class" "artifact" "proof_status" "failed" "unavailable" "capacity.resource_budget.target_class_artifact_changed" "target_class_artifact_changed" "${TARGET_CLASS_ARTIFACT}"
  exit 1
fi
emit_event "target_class" "artifact" "proof_status" "passed" "simulated" "capacity.resource_budget.target_class_skipped_not_proven" "none" "${TARGET_CLASS_ARTIFACT}"

if ! grep -q "toon_rust::try_decode" "${TEST_FILE}"; then
  emit_event "toon_parity" "test" "toon_parity_gate" "failed" "unavailable" "capacity.resource_budget.toon_parity_missing" "toon_parity_missing" "${TEST_FILE}"
  exit 1
fi
emit_event "toon_parity" "test" "toon_parity_gate" "present" "measured" "capacity.resource_budget.toon_parity_present" "none" "${TEST_FILE}"

privacy_hits="$(grep -E 'Bearer ft-b94bx-private-token|Cookie: ft_session=private|PROMPT_BODY:|raw pane excerpt with secret|sk-proj-' \
  "${DOC_FILE}" "${SOURCE_FILE}" "${TEST_FILE}" || true)"
if [[ -n "${privacy_hits}" ]]; then
  emit_event "privacy" "static" "sentinel_scan" "failed" "unavailable" "capacity.resource_budget.privacy_raw_content_leak" "privacy_violation" "${LOG_FILE}"
  exit 1
fi
emit_event "privacy" "static" "sentinel_scan" "passed" "measured" "capacity.resource_budget.no_raw_content" "none" "${LOG_FILE}"

if [[ "${RUN_RUST_PROOF}" -eq 1 ]]; then
  # shellcheck source=tests/e2e/lib_rch_guards.sh
  RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
  RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
  source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
  rch_init "${ARTIFACT_ROOT}" "${RUN_ID}" "swarm_capacity_resource_budget" "${ROOT_DIR}"
  set +e
  ensure_rch_ready
  rch_ready_rc=$?
  set -e
  if [[ "${rch_ready_rc}" -ne 0 ]]; then
    preflight_artifact="$(rch_worker_selection_log_path)"
    if [[ ! -f "${preflight_artifact}" ]]; then
      preflight_artifact="$(rch_remote_preflight_log_path)"
    fi
    if [[ ! -f "${preflight_artifact}" ]]; then
      preflight_artifact="$(rch_probe_log_path)"
    fi
    emit_event "rch_preflight" "rch" "remote_preflight" "failed" "unavailable" "capacity.resource_budget.rch_preflight_failed" "rch_preflight_failed" "${preflight_artifact}" "" false false false
    exit "${rch_ready_rc}"
  fi

  RCH_LOG="${ARTIFACT_ROOT}/swarm_capacity_resource_budget_${RUN_ID}.cargo_test.log"
  SAFE_BEAD_ID="${BEAD_ID//[^[:alnum:]]/-}"
  TARGET_DIR="/tmp/${SAFE_BEAD_ID}-resource-budget-${RUN_ID}"
  emit_event "rust_proof" "rch" "cargo_test_start" "running" "mixed" "capacity.resource_budget.rch_started" "none" "${RCH_LOG}" "" false false false

  set +e
  (
    run_rch_cargo_logged "${RCH_LOG}" \
      env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${TARGET_DIR}" RUST_TEST_THREADS=1 \
        cargo test -j 1 -p frankenterm-core --test swarm_capacity_resource_budget_model --no-default-features -- --nocapture
  )
  rch_rc=$?
  set -e

  rch_meta="${RCH_LOG}.rch_meta.json"
  selected_worker="$(jq -r '.selected_worker_id // .worker_id // .selected_worker // empty' "${rch_meta}" 2>/dev/null || true)"
  cargo_reached="false"
  rustc_reached="false"
  test_reached="false"
  if grep -Eq 'Compiling|Finished|Running|test result' "${RCH_LOG}"; then
    cargo_reached="true"
  fi
  if grep -Eq 'rustc|Compiling|Finished' "${RCH_LOG}"; then
    rustc_reached="true"
  fi
  if grep -Eq 'running [0-9]+ tests|test result: ok' "${RCH_LOG}"; then
    test_reached="true"
  fi
  if [[ "${rch_rc}" -ne 0 ]]; then
    emit_event "rust_proof" "rch" "cargo_test_finish" "failed" "unavailable" "capacity.resource_budget.rch_failed" "cargo_test_failed" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
    exit "${rch_rc}"
  fi
  emit_event "rust_proof" "rch" "cargo_test_finish" "passed" "measured" "capacity.resource_budget.rch_passed" "none" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
else
  emit_event "rust_proof" "rch" "cargo_test_skip" "skipped" "unavailable" "capacity.resource_budget.rch_not_requested" "none" "${LOG_FILE}" "" false false false
fi

row_count="$(jq -s 'length' "${LOG_FILE}")"
if [[ "${row_count}" -lt 18 ]]; then
  emit_event "suite" "e2e_jsonl" "row_count" "failed" "unavailable" "capacity.resource_budget.too_few_rows" "row_count_low" "${LOG_FILE}"
  exit 1
fi

emit_event "suite" "e2e_jsonl" "finish" "passed" "mixed" "capacity.resource_budget.completed" "none" "${LOG_FILE}"
printf '%s\n' "${LOG_FILE}"
