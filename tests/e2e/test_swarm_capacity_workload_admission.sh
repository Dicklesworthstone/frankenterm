#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="${SWARM_CAPACITY_WORKLOAD_ADMISSION_BEAD_ID:-ft-b94bx.2}"
RUN_ID="${SWARM_CAPACITY_WORKLOAD_ADMISSION_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
ARTIFACT_ROOT="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/swarm_capacity_workload_admission/${RUN_ID}"
LOG_FILE="${ARTIFACT_ROOT}/events.jsonl"
FIXTURE_FILE="${ROOT_DIR}/crates/frankenterm-core/tests/fixtures/swarm_capacity_workload_admission/examples.json"
DOC_FILE="${ROOT_DIR}/docs/swarm-capacity-workload-admission.md"
RUN_RUST_PROOF=0

mkdir -p "${ARTIFACT_ROOT}"
: >"${LOG_FILE}"

usage() {
  cat <<'USAGE'
Usage: bash tests/e2e/test_swarm_capacity_workload_admission.sh [--run-rust-proof]

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
  local class_id="$1"
  local step="$2"
  local outcome="$3"
  local action="$4"
  local reason_code="$5"
  local error_code="$6"
  local artifact_path="$7"
  local selected_worker="${8:-}"
  local cargo_reached="${9:-false}"
  local rustc_reached="${10:-false}"
  local test_execution_reached="${11:-false}"

  jq -cn \
    --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg bead_id "${BEAD_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg class_id "${class_id}" \
    --arg step "${step}" \
    --arg outcome "${outcome}" \
    --arg action "${action}" \
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
      class_id: $class_id,
      step: $step,
      outcome: $outcome,
      action: $action,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path,
      selected_worker: ($selected_worker | if . == "" then null else . end),
      cargo_reached: $cargo_reached,
      rustc_reached: $rustc_reached,
      test_execution_reached: $test_execution_reached
    }' >>"${LOG_FILE}"
}

require_file() {
  local path="$1"
  local label="$2"
  if [[ ! -f "${path}" ]]; then
    emit_event "${label}" "preflight" "failed" "unavailable" "capacity.workload_admission.artifact_missing" "missing_artifact" "${path}"
    exit 1
  fi
}

require_command() {
  local command_name="$1"
  if ! command -v "${command_name}" >/dev/null 2>&1; then
    emit_event "${command_name}" "preflight" "failed" "unavailable" "capacity.workload_admission.tool_missing" "${command_name}_not_found" "${LOG_FILE}"
    exit 1
  fi
}

emit_event "suite" "start" "running" "mixed" "capacity.workload_admission.started" "none" "${LOG_FILE}"

require_command jq
require_file "${FIXTURE_FILE}" "fixture"
require_file "${DOC_FILE}" "doc"

jq empty "${FIXTURE_FILE}"
emit_event "fixture" "jq_empty" "passed" "measured" "capacity.workload_admission.fixture_json" "none" "${FIXTURE_FILE}"

class_count="$(jq '.required_workload_classes | length' "${FIXTURE_FILE}")"
if [[ "${class_count}" -ne 9 ]]; then
  emit_event "fixture" "class_count" "failed" "unavailable" "capacity.workload_admission.class_count_invalid" "class_count_invalid" "${FIXTURE_FILE}"
  exit 1
fi
emit_event "fixture" "class_count" "passed" "measured" "capacity.workload_admission.class_count" "none" "${FIXTURE_FILE}"

signal_count="$(jq '.required_signal_kinds | length' "${FIXTURE_FILE}")"
if [[ "${signal_count}" -ne 4 ]]; then
  emit_event "fixture" "signal_count" "failed" "unavailable" "capacity.workload_admission.signal_count_invalid" "signal_count_invalid" "${FIXTURE_FILE}"
  exit 1
fi
emit_event "fixture" "signal_count" "passed" "measured" "capacity.workload_admission.signal_count" "none" "${FIXTURE_FILE}"

gap_state_count="$(jq '.telemetry_gap_states | length' "${FIXTURE_FILE}")"
if [[ "${gap_state_count}" -ne 4 ]]; then
  emit_event "fixture" "telemetry_gap_state_count" "failed" "unavailable" "capacity.workload_admission.telemetry_gap_state_count_invalid" "telemetry_gap_state_count_invalid" "${FIXTURE_FILE}"
  exit 1
fi
emit_event "fixture" "telemetry_gap_state_count" "passed" "measured" "capacity.workload_admission.telemetry_gap_state_count" "none" "${FIXTURE_FILE}"

missing_gap_state_count="$(jq '["open","stagger_recommended","pause_admission","kill_switch"] as $required | [.telemetry_gap_states[]] as $actual | [$required[] | select(. as $state | $actual | index($state) | not)] | length' "${FIXTURE_FILE}")"
if [[ "${missing_gap_state_count}" -ne 0 ]]; then
  emit_event "fixture" "telemetry_gap_state_coverage" "failed" "unavailable" "capacity.workload_admission.telemetry_gap_state_missing" "telemetry_gap_state_missing" "${FIXTURE_FILE}"
  exit 1
fi
emit_event "fixture" "telemetry_gap_state_coverage" "passed" "measured" "capacity.workload_admission.telemetry_gap_state_coverage" "none" "${FIXTURE_FILE}"

fail_closed_reason_count="$(jq '.fail_closed_reason_codes | length' "${FIXTURE_FILE}")"
if [[ "${fail_closed_reason_count}" -lt 4 ]]; then
  emit_event "fixture" "fail_closed_reason_catalog" "failed" "unavailable" "capacity.workload_admission.fail_closed_reason_catalog_invalid" "fail_closed_reason_catalog_invalid" "${FIXTURE_FILE}"
  exit 1
fi
emit_event "fixture" "fail_closed_reason_catalog" "passed" "measured" "capacity.workload_admission.fail_closed_reason_catalog" "none" "${FIXTURE_FILE}"

decision_count="$(jq '.expected_decisions | length' "${FIXTURE_FILE}")"
if [[ "${decision_count}" -ne 4 ]]; then
  emit_event "fixture" "decision_count" "failed" "unavailable" "capacity.workload_admission.decision_count_invalid" "decision_count_invalid" "${FIXTURE_FILE}"
  exit 1
fi
emit_event "fixture" "decision_count" "passed" "measured" "capacity.workload_admission.decision_count" "none" "${FIXTURE_FILE}"

missing_scale_count="$(jq '[50,100,200,500] as $required | [.expected_decisions[].pane_scale] as $actual | [$required[] | select(. as $scale | $actual | index($scale) | not)] | length' "${FIXTURE_FILE}")"
if [[ "${missing_scale_count}" -ne 0 ]]; then
  emit_event "fixture" "scale_coverage" "failed" "unavailable" "capacity.workload_admission.scale_missing" "scale_missing" "${FIXTURE_FILE}"
  exit 1
fi
emit_event "fixture" "scale_coverage" "passed" "measured" "capacity.workload_admission.scale_50_100_200_500" "none" "${FIXTURE_FILE}"

bad_units="$(jq '[.expected_decisions[] | select((.admitted_units > .requested_units) or (.action == "admit" and .admitted_units == 0) or (.action != "admit" and .admitted_units != 0))] | length' "${FIXTURE_FILE}")"
if [[ "${bad_units}" -ne 0 ]]; then
  emit_event "fixture" "admitted_units" "failed" "unavailable" "capacity.workload_admission.units_invalid" "units_invalid" "${FIXTURE_FILE}"
  exit 1
fi
emit_event "fixture" "admitted_units" "passed" "measured" "capacity.workload_admission.units_consistent" "none" "${FIXTURE_FILE}"

bad_gap_flags="$(jq '[.expected_decisions[] | select((.telemetry_gap_state == "open" and (.pause_admission or .kill_switch_active)) or (.telemetry_gap_state == "stagger_recommended" and (.pause_admission or .kill_switch_active)) or (.telemetry_gap_state == "pause_admission" and ((.pause_admission | not) or .kill_switch_active)) or (.telemetry_gap_state == "kill_switch" and ((.pause_admission | not) or (.kill_switch_active | not))))] | length' "${FIXTURE_FILE}")"
if [[ "${bad_gap_flags}" -ne 0 ]]; then
  emit_event "fixture" "telemetry_gap_flags" "failed" "unavailable" "capacity.workload_admission.telemetry_gap_flags_invalid" "telemetry_gap_flags_invalid" "${FIXTURE_FILE}"
  exit 1
fi
emit_event "fixture" "telemetry_gap_flags" "passed" "measured" "capacity.workload_admission.telemetry_gap_flags" "none" "${FIXTURE_FILE}"

while IFS= read -r reason_code; do
  if ! grep -Fq "${reason_code}" "${DOC_FILE}"; then
    emit_event "doc" "fail_closed_reason_catalog" "failed" "unavailable" "capacity.workload_admission.doc_reason_missing" "doc_reason_missing" "${DOC_FILE}"
    exit 1
  fi
done < <(jq -r '.fail_closed_reason_codes[]' "${FIXTURE_FILE}")
emit_event "doc" "fail_closed_reason_catalog" "passed" "measured" "capacity.workload_admission.doc_reason_catalog" "none" "${DOC_FILE}"

while IFS= read -r decision; do
  class_id="$(jq -r '.workload_class' <<<"${decision}")"
  action="$(jq -r '.action' <<<"${decision}")"
  scale="$(jq -r '.pane_scale' <<<"${decision}")"
  emit_event "${class_id}" "decision_fixture" "passed" "${action}" "capacity.workload_admission.scale_${scale}.${action}" "none" "${FIXTURE_FILE}"
done < <(jq -c '.expected_decisions[]' "${FIXTURE_FILE}")

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
  emit_event "rust_proof" "cargo_test_start" "running" "mixed" "capacity.workload_admission.rch_started" "none" "${RCH_LOG}" "" false false false

  set +e
  (
    run_rch_cargo_logged "${RCH_LOG}" \
      env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${TARGET_DIR}" RUST_TEST_THREADS=1 \
        cargo test -j 1 -p frankenterm-core --test swarm_capacity_workload_admission --no-default-features -- --nocapture
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
    fail_open_detected="$(jq -r '.fail_open_detected // false' "${rch_meta}" 2>/dev/null || printf 'false')"
    if [[ -z "${selected_worker}" && "${fail_open_detected}" == "true" ]]; then
      emit_event "rust_proof" "cargo_test_finish" "failed" "unavailable" "capacity.workload_admission.remote_required_failed" "rch_remote_unavailable_or_refused_local_fallback" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
    else
      emit_event "rust_proof" "cargo_test_finish" "failed" "unavailable" "capacity.workload_admission.rch_failed" "cargo_test_failed" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
    fi
    exit "${rch_rc}"
  fi
  emit_event "rust_proof" "cargo_test_finish" "passed" "measured" "capacity.workload_admission.rch_passed" "none" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
else
  emit_event "rust_proof" "cargo_test_skip" "skipped" "unavailable" "capacity.workload_admission.rch_not_requested" "none" "${LOG_FILE}" "" false false false
fi

emit_event "suite" "finish" "passed" "mixed" "capacity.workload_admission.completed" "none" "${LOG_FILE}"
printf '%s\n' "${LOG_FILE}"
