#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="${SWARM_CAPACITY_WORKLOAD_ADMISSION_BEAD_ID:-ft-b94bx.2}"
RUN_ID="${SWARM_CAPACITY_WORKLOAD_ADMISSION_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
ARTIFACT_ROOT="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/swarm_capacity_workload_admission/${RUN_ID}"
LOG_FILE="${ARTIFACT_ROOT}/events.jsonl"
DOC_FILE="${ROOT_DIR}/docs/swarm-capacity-workload-admission-model.md"
SOURCE_FILE="${ROOT_DIR}/crates/frankenterm-core/src/runtime_telemetry.rs"
TEST_FILE="${ROOT_DIR}/crates/frankenterm-core/tests/swarm_capacity_workload_admission_model.rs"
RUN_RUST_PROOF=0

mkdir -p "${ARTIFACT_ROOT}"
: >"${LOG_FILE}"

usage() {
  cat <<'USAGE'
Usage: bash tests/e2e/test_swarm_capacity_workload_admission_model.sh [--run-rust-proof]

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
    emit_event "${command_name}" "static" "preflight" "failed" "unavailable" "capacity.workload_admission.tool_missing" "${command_name}_not_found" "${LOG_FILE}"
    exit 1
  fi
}

require_file() {
  local path="$1"
  local label="$2"
  if [[ ! -f "${path}" ]]; then
    emit_event "${label}" "static" "preflight" "failed" "unavailable" "capacity.workload_admission.artifact_missing" "missing_artifact" "${path}"
    exit 1
  fi
}

emit_event "suite" "e2e_jsonl" "start" "running" "mixed" "capacity.workload_admission.started" "none" "${LOG_FILE}"

require_command jq
require_file "${DOC_FILE}" "doc"
require_file "${SOURCE_FILE}" "source"
require_file "${TEST_FILE}" "test"

required_classes=(
  coding
  reviewing
  building
  testing
  idle
  blocked
  rate_limited
  context_saturated
  stuck_tui_heavy
)
required_signals=(
  context_horizon
  blocker_radar
  herd_wave
  resource_pressure
)
required_scales=(50 100 200 500)

for class in "${required_classes[@]}"; do
  if ! grep -q "\`${class}\`" "${DOC_FILE}"; then
    emit_event "${class}" "doc" "class_documented" "failed" "unavailable" "capacity.workload_admission.class_missing" "class_missing" "${DOC_FILE}"
    exit 1
  fi
  if ! grep -q "${class}" "${SOURCE_FILE}"; then
    emit_event "${class}" "source" "class_defined" "failed" "unavailable" "capacity.workload_admission.class_missing_source" "class_missing_source" "${SOURCE_FILE}"
    exit 1
  fi
  emit_event "${class}" "workload_class" "class_contract" "present" "measured" "capacity.workload_admission.class_present" "none" "${DOC_FILE}"
done

for signal in "${required_signals[@]}"; do
  if ! grep -q "\`${signal}\`" "${DOC_FILE}"; then
    emit_event "${signal}" "doc" "signal_documented" "failed" "unavailable" "capacity.workload_admission.signal_missing" "signal_missing" "${DOC_FILE}"
    exit 1
  fi
  emit_event "${signal}" "signal" "signal_contract" "present" "measured" "capacity.workload_admission.signal_present" "none" "${DOC_FILE}"
done

for scale in "${required_scales[@]}"; do
  if ! grep -q "| ${scale} |" "${DOC_FILE}"; then
    emit_event "${scale}" "doc" "dry_run_scale" "failed" "unavailable" "capacity.workload_admission.scale_missing" "scale_missing" "${DOC_FILE}"
    exit 1
  fi
  emit_event "${scale}" "dry_run_example" "scale_documented" "present" "simulated" "capacity.workload_admission.scale_present" "none" "${DOC_FILE}"
done

if ! grep -q "toon_rust::try_decode" "${TEST_FILE}"; then
  emit_event "toon_parity" "test" "toon_parity_gate" "failed" "unavailable" "capacity.workload_admission.toon_parity_missing" "toon_parity_missing" "${TEST_FILE}"
  exit 1
fi
emit_event "toon_parity" "test" "toon_parity_gate" "present" "measured" "capacity.workload_admission.toon_parity_present" "none" "${TEST_FILE}"

if ! grep -q "evidence_degradation_never_upgrades_admission" "${TEST_FILE}"; then
  emit_event "degradation_property" "test" "property_gate" "failed" "unavailable" "capacity.workload_admission.property_missing" "property_missing" "${TEST_FILE}"
  exit 1
fi
emit_event "degradation_property" "test" "property_gate" "present" "measured" "capacity.workload_admission.property_present" "none" "${TEST_FILE}"

privacy_pattern="$(printf '%s|%s|%s|%s|%s' \
  'Bearer ft-b94bx-''private-token' \
  'Cookie: ft_session=pri''vate' \
  'PROMPT_''BODY:' \
  'raw pane ''excerpt with secret' \
  'sk-''proj-')"
privacy_hits="$(grep -E "${privacy_pattern}" \
  "${DOC_FILE}" "${SOURCE_FILE}" || true)"
if [[ -n "${privacy_hits}" ]]; then
  emit_event "privacy" "static" "sentinel_scan" "failed" "unavailable" "capacity.workload_admission.privacy_raw_content_leak" "privacy_violation" "${LOG_FILE}"
  exit 1
fi
emit_event "privacy" "static" "sentinel_scan" "passed" "measured" "capacity.workload_admission.no_raw_content" "none" "${LOG_FILE}"

if [[ "${RUN_RUST_PROOF}" -eq 1 ]]; then
  # shellcheck source=tests/e2e/lib_rch_guards.sh
  RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
  RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
  source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
  rch_init "${ARTIFACT_ROOT}" "${RUN_ID}" "swarm_capacity_workload_admission" "${ROOT_DIR}"
  ensure_rch_ready

  RCH_LOG="${ARTIFACT_ROOT}/swarm_capacity_workload_admission_${RUN_ID}.cargo_test.log"
  SAFE_BEAD_ID="${BEAD_ID//[^[:alnum:]]/-}"
  TARGET_DIR="/tmp/${SAFE_BEAD_ID}-workload-admission-${RUN_ID}"
  emit_event "rust_proof" "rch" "cargo_test_start" "running" "mixed" "capacity.workload_admission.rch_started" "none" "${RCH_LOG}" "" false false false

  set +e
  (
    run_rch_cargo_logged "${RCH_LOG}" \
      env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${TARGET_DIR}" RUST_TEST_THREADS=1 \
        cargo test -j 1 -p frankenterm-core --test swarm_capacity_workload_admission_model --no-default-features -- --nocapture
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
    emit_event "rust_proof" "rch" "cargo_test_finish" "failed" "unavailable" "capacity.workload_admission.rch_failed" "cargo_test_failed" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
    exit "${rch_rc}"
  fi
  emit_event "rust_proof" "rch" "cargo_test_finish" "passed" "measured" "capacity.workload_admission.rch_passed" "none" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
else
  emit_event "rust_proof" "rch" "cargo_test_skip" "skipped" "unavailable" "capacity.workload_admission.rch_not_requested" "none" "${LOG_FILE}" "" false false false
fi

row_count="$(jq -s 'length' "${LOG_FILE}")"
if [[ "${row_count}" -lt 18 ]]; then
  emit_event "suite" "e2e_jsonl" "row_count" "failed" "unavailable" "capacity.workload_admission.too_few_rows" "row_count_low" "${LOG_FILE}"
  exit 1
fi

emit_event "suite" "e2e_jsonl" "finish" "passed" "mixed" "capacity.workload_admission.completed" "none" "${LOG_FILE}"
printf '%s\n' "${LOG_FILE}"
