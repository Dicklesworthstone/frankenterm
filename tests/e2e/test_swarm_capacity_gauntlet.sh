#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="${SWARM_CAPACITY_GAUNTLET_BEAD_ID:-ft-b94bx.5}"
RUN_ID="${SWARM_CAPACITY_GAUNTLET_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
TARGET_CLASS="${SWARM_CAPACITY_GAUNTLET_TARGET_CLASS:-rch-remote-rust-worker}"
ARTIFACT_ROOT="${SWARM_CAPACITY_GAUNTLET_ARTIFACT_ROOT:-${ROOT_DIR}/tests/e2e/artifacts/swarm-capacity/${TARGET_CLASS}/${RUN_ID}}"
SUMMARY_FILE="${ARTIFACT_ROOT}/summary.json"
EVENTS_FILE="${ARTIFACT_ROOT}/events.jsonl"
COMMANDS_FILE="${ARTIFACT_ROOT}/commands.txt"
STATIC_LOG="${ARTIFACT_ROOT}/static-checks.log"
CARGO_LOG_PREFIX="${ARTIFACT_ROOT}/swarm_capacity_gauntlet_${RUN_ID}"
PRIMARY_CARGO_LOG="${CARGO_LOG_PREFIX}.capacity_models.cargo_test.log"
PROOF_TARGETS_FILE="${ARTIFACT_ROOT}/proof-targets.jsonl"
REMOTE_TARGET_DIR="${SWARM_CAPACITY_GAUNTLET_CARGO_TARGET_DIR:-/tmp/${BEAD_ID//[^[:alnum:]]/-}-capacity-gauntlet-${RUN_ID}}"
NEGATIVE_FIXTURE="${ROOT_DIR}/tests/e2e/fixtures/swarm_capacity_gauntlet/stale_worker_missing_telemetry.rch_meta.json"

RUN_RUST_PROOF=0
SUMMARY_STATUS="failed"
FAILURE_CLASSIFICATION="not_run"
SOURCE_VERDICT="not_run"
REASON_CODE="capacity.gauntlet.not_run"
ERROR_CODE="none"
HARNESS_EXIT_CODE=1
RCH_WRAPPER_EXIT_CODE=""
RCH_REMOTE_EXIT_CODE=""
SELECTED_WORKER=""
CARGO_REACHED=false
RUSTC_REACHED=false
TEST_EXECUTION_REACHED=false
TEST_RESULT_OK_COUNT=0
EXTRACTED_CARGO_REACHED=false
EXTRACTED_RUSTC_REACHED=false
EXTRACTED_TEST_EXECUTION_REACHED=false
EXTRACTED_TEST_RESULT_OK_COUNT=0

mkdir -p "${ARTIFACT_ROOT}"
: >"${EVENTS_FILE}"
: >"${COMMANDS_FILE}"
: >"${STATIC_LOG}"
: >"${PROOF_TARGETS_FILE}"

usage() {
  cat <<'USAGE'
Usage: bash tests/e2e/test_swarm_capacity_gauntlet.sh [--run-rust-proof]

Static checks validate the gauntlet contract and negative RCH fixture locally.
--run-rust-proof runs the three swarm-capacity Rust proof targets through RCH
and refuses local Cargo fallback.
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

json_bool() {
  if [[ "$1" == "true" ]]; then
    printf 'true\n'
  else
    printf 'false\n'
  fi
}

json_num_or_null_arg() {
  if [[ "$1" =~ ^-?[0-9]+$ ]]; then
    printf '%s\n' "$1"
  else
    printf 'null\n'
  fi
}

repo_rel() {
  local path="$1"
  if [[ "${path}" == "${ROOT_DIR}/"* ]]; then
    printf '%s\n' "${path#"${ROOT_DIR}/"}"
  else
    printf '%s\n' "${path}"
  fi
}

emit_event() {
  local step="$1"
  local outcome="$2"
  local classification="$3"
  local reason_code="$4"
  local error_code="$5"
  local artifact_path="$6"
  local message="$7"

  jq -cn \
    --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg bead_id "${BEAD_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg target_class "${TARGET_CLASS}" \
    --arg step "${step}" \
    --arg outcome "${outcome}" \
    --arg classification "${classification}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "$(repo_rel "${artifact_path}")" \
    --arg message "${message}" \
    '{
      timestamp: $timestamp,
      bead_id: $bead_id,
      run_id: $run_id,
      target_class: $target_class,
      step: $step,
      outcome: $outcome,
      failure_classification: $classification,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path,
      message: $message
    }' >>"${EVENTS_FILE}"
}

record_command() {
  printf '%s\n' "$*" >>"${COMMANDS_FILE}"
}

write_summary() {
  local rch_meta="${PRIMARY_CARGO_LOG}.rch_meta.json"
  local event_count wrapper_exit remote_exit selected_worker selected_workers_json proof_targets_json

  event_count="$(jq -s 'length' "${EVENTS_FILE}" 2>/dev/null || printf '0')"
  selected_worker="${SELECTED_WORKER}"
  wrapper_exit="${RCH_WRAPPER_EXIT_CODE}"
  remote_exit="${RCH_REMOTE_EXIT_CODE}"
  selected_workers_json="[]"
  proof_targets_json="[]"

  if [[ -f "${rch_meta}" ]]; then
    selected_worker="$(jq -r '.selected_worker // empty' "${rch_meta}" 2>/dev/null || true)"
    wrapper_exit="$(jq -r '.wrapper_exit_code // empty' "${rch_meta}" 2>/dev/null || true)"
    remote_exit="$(jq -r '.remote_exit_code // empty' "${rch_meta}" 2>/dev/null || true)"
  fi
  if [[ -s "${PROOF_TARGETS_FILE}" ]]; then
    selected_workers_json="$(jq -s '[.[].selected_worker | select(. != null and . != "")] | unique' "${PROOF_TARGETS_FILE}" 2>/dev/null || printf '[]')"
    proof_targets_json="$(jq -s '.' "${PROOF_TARGETS_FILE}" 2>/dev/null || printf '[]')"
  fi

  jq -cn \
    --arg schema_version "1.0.0" \
    --arg bead_id "${BEAD_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg status "${SUMMARY_STATUS}" \
    --arg failure_classification "${FAILURE_CLASSIFICATION}" \
    --arg source_verdict "${SOURCE_VERDICT}" \
    --arg reason_code "${REASON_CODE}" \
    --arg error_code "${ERROR_CODE}" \
    --arg target_class "${TARGET_CLASS}" \
    --arg artifact_dir "$(repo_rel "${ARTIFACT_ROOT}")" \
    --arg selected_worker "${selected_worker}" \
    --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
    --argjson harness_exit_code "$(json_num_or_null_arg "${HARNESS_EXIT_CODE}")" \
    --argjson rch_wrapper_exit_code "$(json_num_or_null_arg "${wrapper_exit}")" \
    --argjson rch_remote_exit_code "$(json_num_or_null_arg "${remote_exit}")" \
    --argjson selected_workers "${selected_workers_json}" \
    --argjson proof_targets "${proof_targets_json}" \
    --argjson cargo_reached "$(json_bool "${CARGO_REACHED}")" \
    --argjson rustc_reached "$(json_bool "${RUSTC_REACHED}")" \
    --argjson test_execution_reached "$(json_bool "${TEST_EXECUTION_REACHED}")" \
    --argjson test_result_ok_count "${TEST_RESULT_OK_COUNT}" \
    --argjson event_count "${event_count}" \
    '{
      schema_version: $schema_version,
      bead_id: $bead_id,
      run_id: $run_id,
      status: $status,
      failure_classification: $failure_classification,
      source_verdict: $source_verdict,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_dir: $artifact_dir,
      target_class: {
        requested: $target_class,
        hardware_class: $target_class,
        selected_worker: (if $selected_worker == "" then null else $selected_worker end),
        selected_workers: $selected_workers,
        high_scale_claim_allowed: false,
        evidence_policy: "remote RCH proof may prove source execution; target-class high-scale claims still require matching retained hardware telemetry"
      },
      remote: {
        selected_worker: (if $selected_worker == "" then null else $selected_worker end),
        selected_workers: $selected_workers,
        cargo_target_dir: $remote_target_dir,
        exit_codes: {
          harness: $harness_exit_code,
          rch_wrapper: $rch_wrapper_exit_code,
          rch_remote: $rch_remote_exit_code
        },
        material_execution: {
          cargo_reached: $cargo_reached,
          rustc_reached: $rustc_reached,
          test_execution_reached: $test_execution_reached,
          test_result_ok_count: $test_result_ok_count
        }
      },
      proof_targets: $proof_targets,
      counts: {
        events: $event_count
      },
      artifacts: {
        summary: "summary.json",
        events: "events.jsonl",
        commands: "commands.txt",
        static_log: "static-checks.log",
        proof_targets: "proof-targets.jsonl",
        cargo_log: "swarm_capacity_gauntlet_\($run_id).capacity_models.cargo_test.log",
        cargo_logs: {
          capacity_models: "swarm_capacity_gauntlet_\($run_id).capacity_models.cargo_test.log"
        },
        rch_meta: "swarm_capacity_gauntlet_\($run_id).capacity_models.cargo_test.log.rch_meta.json",
        rch_probe: "swarm_capacity_gauntlet_\($run_id).rch_probe.log",
        rch_queue: "swarm_capacity_gauntlet_\($run_id).rch_queue.log",
        rch_preflight: "swarm_capacity_gauntlet_\($run_id).rch_preflight.json",
        rch_mirror_preflight: "swarm_capacity_gauntlet_\($run_id).rch_mirror_preflight.json",
        rch_worker_selection: "swarm_capacity_gauntlet_\($run_id).rch_worker_selection.json"
      }
    }' >"${SUMMARY_FILE}"
}

trap write_summary EXIT

require_command() {
  local command_name="$1"
  if ! command -v "${command_name}" >/dev/null 2>&1; then
    SUMMARY_STATUS="failed"
    FAILURE_CLASSIFICATION="infrastructure_blocked"
    SOURCE_VERDICT="not_reached"
    REASON_CODE="capacity.gauntlet.tool_missing"
    ERROR_CODE="${command_name}_not_found"
    emit_event "preflight.${command_name}" "failed" "infrastructure_blocked" "${REASON_CODE}" "${ERROR_CODE}" "${STATIC_LOG}" "${command_name} is required."
    exit 1
  fi
}

require_file() {
  local path="$1"
  local label="$2"
  if [[ ! -f "${path}" ]]; then
    SUMMARY_STATUS="failed"
    FAILURE_CLASSIFICATION="infrastructure_blocked"
    SOURCE_VERDICT="not_reached"
    REASON_CODE="capacity.gauntlet.artifact_missing"
    ERROR_CODE="missing_${label}"
    emit_event "preflight.${label}" "failed" "infrastructure_blocked" "${REASON_CODE}" "${ERROR_CODE}" "${path}" "Required ${label} file is missing."
    exit 1
  fi
}

material_bool_from_fixture() {
  local meta_file="$1"
  local field="$2"
  jq -r --arg field "${field}" '.material_execution[$field] // false' "${meta_file}"
}

classify_rch_meta() {
  local meta_file="$1"
  local cargo_reached="$2"
  local rustc_reached="$3"
  local test_execution_reached="$4"
  local wrapper_exit remote_exit failure_reason timed_out fail_open telemetry_state

  wrapper_exit="$(jq -r '.wrapper_exit_code // empty' "${meta_file}" 2>/dev/null || true)"
  remote_exit="$(jq -r '.remote_exit_code // empty' "${meta_file}" 2>/dev/null || true)"
  failure_reason="$(jq -r '.failure_reason_code // empty' "${meta_file}" 2>/dev/null || true)"
  timed_out="$(jq -r '.timed_out // false' "${meta_file}" 2>/dev/null || printf 'false')"
  fail_open="$(jq -r '.fail_open_detected // false' "${meta_file}" 2>/dev/null || printf 'false')"
  telemetry_state="$(jq -r '.hardware.telemetry_state // empty' "${meta_file}" 2>/dev/null || true)"

  if [[ "${wrapper_exit}" == "0" && "${remote_exit}" == "0" && "${cargo_reached}" == "true" && "${rustc_reached}" == "true" && "${test_execution_reached}" == "true" ]]; then
    printf 'passed\n'
    return 0
  fi

  case "${failure_reason}" in
    RCH-REMOTE-MIRROR-MISSING-FILE|RCH-REMOTE-STALL|RCH-CARGO-DEP-INFO-MISSING|RCH-WORKER-SELECTION-TIMEOUT|RCH-QUEUE-TIMEOUT)
      printf 'infrastructure_blocked\n'
      return 0
      ;;
  esac

  if [[ "${timed_out}" == "true" || "${fail_open}" == "true" || "${wrapper_exit}" == "124" || "${wrapper_exit}" == "137" ]]; then
    printf 'infrastructure_blocked\n'
    return 0
  fi

  if [[ "${telemetry_state}" == "stale" || "${telemetry_state}" == "missing" ]]; then
    printf 'infrastructure_blocked\n'
    return 0
  fi

  if [[ "${cargo_reached}" != "true" || "${rustc_reached}" != "true" ]]; then
    printf 'infrastructure_blocked\n'
    return 0
  fi

  printf 'source_failed\n'
}

extract_material_execution_flags() {
  local log_file="$1"

  EXTRACTED_CARGO_REACHED=false
  EXTRACTED_RUSTC_REACHED=false
  EXTRACTED_TEST_EXECUTION_REACHED=false
  EXTRACTED_TEST_RESULT_OK_COUNT=0

  if [[ ! -f "${log_file}" ]]; then
    return 0
  fi
  if grep -Eq 'Compiling|Checking|Finished|Running|test result|error:' "${log_file}"; then
    EXTRACTED_CARGO_REACHED=true
  fi
  if grep -Eq 'Compiling|Checking|Finished|error\[E[0-9]+\]' "${log_file}"; then
    EXTRACTED_RUSTC_REACHED=true
  fi
  if grep -Eq 'running [0-9]+ tests|test result: ok|test result: FAILED' "${log_file}"; then
    EXTRACTED_TEST_EXECUTION_REACHED=true
  fi
  EXTRACTED_TEST_RESULT_OK_COUNT="$(grep -Ec 'test result: ok' "${log_file}" || true)"
}

run_static_checks() {
  local harness

  require_command jq
  require_command grep
  require_file "${NEGATIVE_FIXTURE}" "negative_fixture"
  require_file "${ROOT_DIR}/tests/e2e/test_swarm_capacity_resource_budget_model.sh" "resource_budget_harness"
  require_file "${ROOT_DIR}/tests/e2e/test_swarm_capacity_workload_admission_model.sh" "workload_admission_harness"
  require_file "${ROOT_DIR}/tests/e2e/test_swarm_capacity_simulation_corpus.sh" "simulation_corpus_harness"

  for harness in \
    "${BASH_SOURCE[0]}" \
    "${ROOT_DIR}/tests/e2e/test_swarm_capacity_resource_budget_model.sh" \
    "${ROOT_DIR}/tests/e2e/test_swarm_capacity_workload_admission_model.sh" \
    "${ROOT_DIR}/tests/e2e/test_swarm_capacity_simulation_corpus.sh"
  do
    record_command "bash -n ${harness}"
    bash -n "${harness}" >>"${STATIC_LOG}" 2>&1
    emit_event "static.$(basename "${harness}")" "passed" "none" "capacity.gauntlet.shell_syntax_passed" "none" "${harness}" "Shell syntax passed."
  done

  record_command "jq empty ${NEGATIVE_FIXTURE}"
  jq empty "${NEGATIVE_FIXTURE}" >>"${STATIC_LOG}" 2>&1

  local fixture_cargo fixture_rustc fixture_test fixture_classification fixture_expected
  fixture_cargo="$(material_bool_from_fixture "${NEGATIVE_FIXTURE}" "cargo_reached")"
  fixture_rustc="$(material_bool_from_fixture "${NEGATIVE_FIXTURE}" "rustc_reached")"
  fixture_test="$(material_bool_from_fixture "${NEGATIVE_FIXTURE}" "test_execution_reached")"
  fixture_classification="$(classify_rch_meta "${NEGATIVE_FIXTURE}" "${fixture_cargo}" "${fixture_rustc}" "${fixture_test}")"
  fixture_expected="$(jq -r '.expected_classification' "${NEGATIVE_FIXTURE}")"
  if [[ "${fixture_classification}" != "${fixture_expected}" || "${fixture_classification}" != "infrastructure_blocked" ]]; then
    SUMMARY_STATUS="failed"
    FAILURE_CLASSIFICATION="source_failed"
    SOURCE_VERDICT="negative_fixture_failed"
    REASON_CODE="capacity.gauntlet.negative_fixture_misclassified"
    ERROR_CODE="negative_fixture_misclassified"
    emit_event "negative_fixture.classification" "failed" "source_failed" "${REASON_CODE}" "${ERROR_CODE}" "${NEGATIVE_FIXTURE}" "Negative fixture did not classify as infrastructure_blocked."
    return 1
  fi
  emit_event "negative_fixture.classification" "passed" "none" "capacity.gauntlet.infrastructure_blocked_fixture_passed" "none" "${NEGATIVE_FIXTURE}" "Stale worker and missing telemetry fixture classified as infrastructure_blocked."

  SUMMARY_STATUS="passed"
  FAILURE_CLASSIFICATION="none"
  SOURCE_VERDICT="static_passed"
  REASON_CODE="capacity.gauntlet.static_passed"
  ERROR_CODE="none"
  HARNESS_EXIT_CODE=0
}

run_capacity_target() {
  local target_slug="$1"
  shift
  local cargo_target_text="$*"
  local cargo_target_args=("$@")
  local log_file="${CARGO_LOG_PREFIX}.${target_slug}.cargo_test.log"
  local rch_rc rch_meta selected_worker wrapper_exit remote_exit classification return_rc

  record_command "run_rch_cargo_logged ${log_file} env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=${REMOTE_TARGET_DIR} RUST_TEST_THREADS=1 cargo test -j 1 -p frankenterm-core --no-default-features ${cargo_target_text} -- --nocapture"
  emit_event "rch.${target_slug}" "running" "none" "capacity.gauntlet.rch_target_started" "none" "${log_file}" "Starting remote Cargo gauntlet target selection: ${cargo_target_text}."

  set +e
  (
    run_rch_cargo_logged "${log_file}" \
      env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" RUST_TEST_THREADS=1 \
        cargo test -j 1 -p frankenterm-core --no-default-features "${cargo_target_args[@]}" -- --nocapture
  )
  rch_rc=$?
  set -e

  extract_material_execution_flags "${log_file}"
  if [[ "${EXTRACTED_CARGO_REACHED}" == "true" ]]; then
    CARGO_REACHED=true
  fi
  if [[ "${EXTRACTED_RUSTC_REACHED}" == "true" ]]; then
    RUSTC_REACHED=true
  fi
  if [[ "${EXTRACTED_TEST_EXECUTION_REACHED}" == "true" ]]; then
    TEST_EXECUTION_REACHED=true
  fi
  TEST_RESULT_OK_COUNT=$((TEST_RESULT_OK_COUNT + EXTRACTED_TEST_RESULT_OK_COUNT))

  rch_meta="${log_file}.rch_meta.json"
  selected_worker=""
  wrapper_exit="${rch_rc}"
  remote_exit=""
  if [[ -f "${rch_meta}" ]]; then
    selected_worker="$(jq -r '.selected_worker // empty' "${rch_meta}" 2>/dev/null || true)"
    wrapper_exit="$(jq -r '.wrapper_exit_code // empty' "${rch_meta}" 2>/dev/null || true)"
    remote_exit="$(jq -r '.remote_exit_code // empty' "${rch_meta}" 2>/dev/null || true)"
  fi
  SELECTED_WORKER="${selected_worker}"
  RCH_WRAPPER_EXIT_CODE="${wrapper_exit}"
  RCH_REMOTE_EXIT_CODE="${remote_exit}"
  HARNESS_EXIT_CODE="${rch_rc}"

  if [[ -f "${rch_meta}" ]]; then
    classification="$(classify_rch_meta "${rch_meta}" "${EXTRACTED_CARGO_REACHED}" "${EXTRACTED_RUSTC_REACHED}" "${EXTRACTED_TEST_EXECUTION_REACHED}")"
  else
    classification="infrastructure_blocked"
  fi

  jq -cn \
    --arg target_slug "${target_slug}" \
    --arg test_target "${cargo_target_text}" \
    --arg log_file "$(repo_rel "${log_file}")" \
    --arg rch_meta "$(repo_rel "${rch_meta}")" \
    --arg selected_worker "${selected_worker}" \
    --arg classification "${classification}" \
    --argjson wrapper_exit_code "$(json_num_or_null_arg "${wrapper_exit}")" \
    --argjson remote_exit_code "$(json_num_or_null_arg "${remote_exit}")" \
    --argjson cargo_reached "$(json_bool "${EXTRACTED_CARGO_REACHED}")" \
    --argjson rustc_reached "$(json_bool "${EXTRACTED_RUSTC_REACHED}")" \
    --argjson test_execution_reached "$(json_bool "${EXTRACTED_TEST_EXECUTION_REACHED}")" \
    --argjson test_result_ok_count "${EXTRACTED_TEST_RESULT_OK_COUNT}" \
    '{
      target_slug: $target_slug,
      test_target: $test_target,
      log_file: $log_file,
      rch_meta: $rch_meta,
      selected_worker: (if $selected_worker == "" then null else $selected_worker end),
      classification: $classification,
      exit_codes: {
        rch_wrapper: $wrapper_exit_code,
        rch_remote: $remote_exit_code
      },
      material_execution: {
        cargo_reached: $cargo_reached,
        rustc_reached: $rustc_reached,
        test_execution_reached: $test_execution_reached,
        test_result_ok_count: $test_result_ok_count
      }
    }' >>"${PROOF_TARGETS_FILE}"

  case "${classification}" in
    passed)
      emit_event "rch.${target_slug}" "passed" "none" "capacity.gauntlet.rch_target_passed" "none" "${log_file}" "Remote Cargo gauntlet target selection passed: ${cargo_target_text}."
      return 0
      ;;
    infrastructure_blocked)
      SUMMARY_STATUS="infrastructure_blocked"
      FAILURE_CLASSIFICATION="infrastructure_blocked"
      SOURCE_VERDICT="not_reached"
      REASON_CODE="capacity.gauntlet.infrastructure_blocked"
      ERROR_CODE="rch_material_execution_not_proven"
      emit_event "rch.${target_slug}" "failed" "infrastructure_blocked" "${REASON_CODE}" "${ERROR_CODE}" "${log_file}" "RCH failed before a trustworthy source verdict for ${cargo_target_text}."
      if [[ "${rch_rc}" -eq 0 ]]; then
        return 1
      fi
      return "${rch_rc}"
      ;;
    *)
      SUMMARY_STATUS="failed"
      FAILURE_CLASSIFICATION="source_failed"
      SOURCE_VERDICT="failed"
      REASON_CODE="capacity.gauntlet.source_failed"
      ERROR_CODE="cargo_test_failed"
      emit_event "rch.${target_slug}" "failed" "source_failed" "${REASON_CODE}" "${ERROR_CODE}" "${log_file}" "Remote Cargo target selection reached rustc; failure is a source/test verdict for ${cargo_target_text}."
      return_rc="${rch_rc}"
      if [[ "${return_rc}" -eq 0 ]]; then
        return_rc=1
      fi
      return "${return_rc}"
      ;;
  esac
}

run_rch_proof() {
  export RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
  export RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
  export RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-7200}"
  export RCH_BUILD_SLOTS="${RCH_BUILD_SLOTS:-1}"
  export RCH_TEST_SLOTS="${RCH_TEST_SLOTS:-1}"
  export RCH_CHECK_SLOTS="${RCH_CHECK_SLOTS:-1}"

  # shellcheck source=tests/e2e/lib_rch_guards.sh
  source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
  rch_init "${ARTIFACT_ROOT}" "${RUN_ID}" "swarm_capacity_gauntlet" "${ROOT_DIR}"

  set +e
  ( ensure_rch_ready )
  local preflight_rc=$?
  set -e
  if [[ "${preflight_rc}" -ne 0 ]]; then
    SUMMARY_STATUS="infrastructure_blocked"
    FAILURE_CLASSIFICATION="infrastructure_blocked"
    SOURCE_VERDICT="not_reached"
    REASON_CODE="capacity.gauntlet.rch_preflight_blocked"
    ERROR_CODE="rch_preflight_failed"
    HARNESS_EXIT_CODE="${preflight_rc}"
    RCH_WRAPPER_EXIT_CODE="${preflight_rc}"
    emit_event "rch.preflight" "failed" "infrastructure_blocked" "${REASON_CODE}" "${ERROR_CODE}" "${ARTIFACT_ROOT}" "RCH preflight failed before material Cargo execution."
    return "${preflight_rc}"
  fi
  emit_event "rch.preflight" "passed" "none" "capacity.gauntlet.rch_preflight_passed" "none" "${ARTIFACT_ROOT}" "RCH preflight passed."

  emit_event "rch.gauntlet" "running" "none" "capacity.gauntlet.rch_started" "none" "${ARTIFACT_ROOT}" "Starting direct remote Cargo gauntlet targets."

  run_capacity_target \
    "capacity_models" \
    --test swarm_capacity_resource_budget_model \
    --test swarm_capacity_workload_admission_model \
    --test swarm_capacity_simulation_corpus

  SUMMARY_STATUS="passed"
  FAILURE_CLASSIFICATION="none"
  SOURCE_VERDICT="passed"
  REASON_CODE="capacity.gauntlet.rch_passed"
  ERROR_CODE="none"
  HARNESS_EXIT_CODE=0
  emit_event "rch.gauntlet" "passed" "none" "${REASON_CODE}" "none" "${SUMMARY_FILE}" "Remote gauntlet reached Cargo, rustc, and test execution for all targets."
  return 0
}

emit_event "suite" "running" "none" "capacity.gauntlet.started" "none" "${ARTIFACT_ROOT}" "Swarm capacity gauntlet started."
run_static_checks

if [[ "${RUN_RUST_PROOF}" -eq 1 ]]; then
  set +e
  run_rch_proof
  proof_rc=$?
  set -e
  if [[ "${proof_rc}" -ne 0 ]]; then
    exit "${proof_rc}"
  fi
else
  emit_event "rch.gauntlet" "skipped" "none" "capacity.gauntlet.rch_not_requested" "none" "${EVENTS_FILE}" "RCH proof was not requested."
fi

HARNESS_EXIT_CODE=0
emit_event "suite" "passed" "${FAILURE_CLASSIFICATION}" "capacity.gauntlet.completed" "none" "${SUMMARY_FILE}" "Swarm capacity gauntlet completed."
printf '%s\n' "${SUMMARY_FILE}"
