#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="${SWARM_CAPACITY_SIGNAL_INVENTORY_BEAD_ID:-ft-b94bx.1}"
RUN_ID="${SWARM_CAPACITY_SIGNAL_INVENTORY_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
ARTIFACT_ROOT="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/swarm_capacity_signal_inventory/${RUN_ID}"
LOG_FILE="${ARTIFACT_ROOT}/events.jsonl"
SCHEMA_FILE="${ROOT_DIR}/docs/json-schema/ft-swarm-capacity-signal-inventory.json"
FIXTURE_FILE="${ROOT_DIR}/crates/frankenterm-core/tests/fixtures/swarm_capacity_signal_inventory/complete.json"
DOC_FILE="${ROOT_DIR}/docs/swarm-capacity-signal-inventory.md"
RUN_RUST_PROOF=0

mkdir -p "${ARTIFACT_ROOT}"
: >"${LOG_FILE}"

usage() {
  cat <<'USAGE'
Usage: bash tests/e2e/test_swarm_capacity_signal_inventory.sh [--run-rust-proof]

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
  local signal_id="$1"
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
    --arg signal_id "${signal_id}" \
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
      signal_id: $signal_id,
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

require_file() {
  local path="$1"
  local label="$2"
  if [[ ! -f "${path}" ]]; then
    emit_event "${label}" "static" "preflight" "failed" "unavailable" "capacity.signal_inventory.artifact_missing" "missing_artifact" "${path}"
    exit 1
  fi
}

require_command() {
  local command_name="$1"
  if ! command -v "${command_name}" >/dev/null 2>&1; then
    emit_event "${command_name}" "static" "preflight" "failed" "unavailable" "capacity.signal_inventory.tool_missing" "${command_name}_not_found" "${LOG_FILE}"
    exit 1
  fi
}

e2e_outcome_for_evidence_state() {
  case "$1" in
    measured|inferred|mixed)
      printf 'present\n'
      ;;
    stale)
      printf 'stale\n'
      ;;
    unavailable)
      printf 'unavailable\n'
      ;;
    simulated)
      printf 'simulated\n'
      ;;
    *)
      return 1
      ;;
  esac
}

emit_event "suite" "e2e_jsonl" "start" "running" "mixed" "capacity.signal_inventory.started" "none" "${LOG_FILE}"

require_command jq
require_command git
require_file "${SCHEMA_FILE}" "schema"
require_file "${FIXTURE_FILE}" "fixture"
require_file "${DOC_FILE}" "doc"

require_repo_relative_file() {
  local path="$1"
  local label="$2"

  if [[ -z "${path}" || "${path}" == "." || "${path}" == ".." ]]; then
    emit_event "${label}" "static" "artifact_path_shape" "failed" "unavailable" "capacity.signal_inventory.artifact_path_unsafe" "empty_or_dot_path" "${LOG_FILE}"
    exit 1
  fi
  if [[ "${path}" == /* || "${path}" == ./* || "${path}" == ../* || "${path}" == */ ]]; then
    emit_event "${label}" "static" "artifact_path_shape" "failed" "unavailable" "capacity.signal_inventory.artifact_path_unsafe" "absolute_or_dot_segment_path" "${LOG_FILE}"
    exit 1
  fi
  if [[ "${path}" == *\\* ]]; then
    emit_event "${label}" "static" "artifact_path_shape" "failed" "unavailable" "capacity.signal_inventory.artifact_path_unsafe" "backslash_path" "${LOG_FILE}"
    exit 1
  fi

  local segment
  local -a path_segments
  IFS='/' read -r -a path_segments <<<"${path}"
  for segment in "${path_segments[@]}"; do
    if [[ -z "${segment}" || "${segment}" == "." || "${segment}" == ".." || "${segment}" == ".git" ]]; then
      emit_event "${label}" "static" "artifact_path_shape" "failed" "unavailable" "capacity.signal_inventory.artifact_path_unsafe" "unsafe_path_segment" "${LOG_FILE}"
      exit 1
    fi
  done

  if [[ ! -f "${ROOT_DIR}/${path}" ]]; then
    emit_event "${label}" "static" "artifact_path_exists" "failed" "unavailable" "capacity.signal_inventory.artifact_missing" "missing_artifact" "${path}"
    exit 1
  fi
  if ! git ls-files --error-unmatch -- "${path}" >/dev/null 2>&1; then
    emit_event "${label}" "static" "artifact_path_tracked" "failed" "unavailable" "capacity.signal_inventory.artifact_untracked" "untracked_artifact" "${path}"
    exit 1
  fi
}

jq empty "${SCHEMA_FILE}"
emit_event "schema" "static" "jq_empty" "passed" "measured" "capacity.signal_inventory.schema_json" "none" "${SCHEMA_FILE}"

jq empty "${FIXTURE_FILE}"
emit_event "fixture" "static" "jq_empty" "passed" "measured" "capacity.signal_inventory.fixture_json" "none" "${FIXTURE_FILE}"

while IFS= read -r artifact_path; do
  require_repo_relative_file "${artifact_path}" "fixture_artifact_path"
done < <(jq -r '.artifact_paths[]' "${FIXTURE_FILE}")

while IFS= read -r source_path; do
  require_repo_relative_file "${source_path}" "signal_source_ref"
done < <(jq -r '.signals[].source_refs[].path' "${FIXTURE_FILE}")

while IFS= read -r redacted_artifact_path; do
  require_repo_relative_file "${redacted_artifact_path}" "redacted_artifact_path"
done < <(jq -r '.signals[] | select(.redacted_artifact_path != null) | .redacted_artifact_path' "${FIXTURE_FILE}")
emit_event "fixture" "static" "artifact_paths" "passed" "measured" "capacity.signal_inventory.artifact_paths_safe" "none" "${FIXTURE_FILE}"

signal_count="$(jq '.signals | length' "${FIXTURE_FILE}")"
if [[ "${signal_count}" -lt 12 ]]; then
  emit_event "fixture" "static" "signal_count" "failed" "unavailable" "capacity.signal_inventory.too_few_signals" "signal_count_low" "${FIXTURE_FILE}"
  exit 1
fi
emit_event "fixture" "static" "signal_count" "passed" "measured" "capacity.signal_inventory.signal_count" "none" "${FIXTURE_FILE}"

required_gap_count="$(jq '[.gap_map[] | .gap_id] | length' "${FIXTURE_FILE}")"
if [[ "${required_gap_count}" -ne 6 ]]; then
  emit_event "fixture" "static" "gap_count" "failed" "unavailable" "capacity.signal_inventory.gap_count_invalid" "gap_count_invalid" "${FIXTURE_FILE}"
  exit 1
fi
emit_event "fixture" "static" "gap_count" "passed" "measured" "capacity.signal_inventory.gap_count" "none" "${FIXTURE_FILE}"

while IFS= read -r signal; do
  signal_id="$(jq -r '.signal_id' <<<"${signal}")"
  domain="$(jq -r '.domain' <<<"${signal}")"
  evidence_state="$(jq -r '.current_evidence_state' <<<"${signal}")"
  if ! outcome="$(e2e_outcome_for_evidence_state "${evidence_state}")"; then
    emit_event "${signal_id}" "${domain}" "signal_emit" "failed" "${evidence_state}" "capacity.signal_inventory.bad_evidence_state" "bad_evidence_state" "${FIXTURE_FILE}"
    exit 1
  fi
  emit_event "${signal_id}" "${domain}" "signal_emit" "${outcome}" "${evidence_state}" "capacity.signal_inventory.signal.${outcome}" "none" "${FIXTURE_FILE}"
done < <(jq -c '.signals[]' "${FIXTURE_FILE}")

emitted_signal_count="$(jq -s '[.[] | select(.step == "signal_emit")] | length' "${LOG_FILE}")"
if [[ "${emitted_signal_count}" -ne "${signal_count}" ]]; then
  emit_event "fixture" "e2e_jsonl" "row_count" "failed" "unavailable" "capacity.signal_inventory.row_count_mismatch" "row_count_mismatch" "${LOG_FILE}"
  exit 1
fi
emit_event "fixture" "e2e_jsonl" "row_count" "passed" "measured" "capacity.signal_inventory.one_row_per_signal" "none" "${LOG_FILE}"

bad_outcomes="$(jq -sr '[.[] | select(.step == "signal_emit") | select(.outcome as $o | ["present", "stale", "unavailable", "simulated"] | index($o) | not)] | length' "${LOG_FILE}")"
if [[ "${bad_outcomes}" -ne 0 ]]; then
  emit_event "fixture" "e2e_jsonl" "outcome_vocab" "failed" "unavailable" "capacity.signal_inventory.bad_outcome" "bad_outcome" "${LOG_FILE}"
  exit 1
fi
emit_event "fixture" "e2e_jsonl" "outcome_vocab" "passed" "measured" "capacity.signal_inventory.outcome_vocab" "none" "${LOG_FILE}"

privacy_pattern="$(printf '%s|%s|%s|%s' \
  'Bearer ft-b94bx-''private-token' \
  'Cookie: ft_session=pri''vate' \
  'PROMPT_''BODY:' \
  'raw pane ''excerpt with secret')"
privacy_hits="$(grep -E "${privacy_pattern}" \
  "${FIXTURE_FILE}" "${DOC_FILE}" "${SCHEMA_FILE}" || true)"
if [[ -n "${privacy_hits}" ]]; then
  emit_event "privacy" "static" "sentinel_scan" "failed" "unavailable" "capacity.signal_inventory.privacy_raw_content_leak" "privacy_violation" "${LOG_FILE}"
  exit 1
fi
emit_event "privacy" "static" "sentinel_scan" "passed" "measured" "capacity.signal_inventory.no_raw_content" "none" "${LOG_FILE}"

if [[ "${RUN_RUST_PROOF}" -eq 1 ]]; then
  # shellcheck source=tests/e2e/lib_rch_guards.sh
  RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
  RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
  source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
  rch_init "${ARTIFACT_ROOT}" "${RUN_ID}" "swarm_capacity_signal_inventory" "${ROOT_DIR}"
  ensure_rch_ready

  RCH_LOG="${ARTIFACT_ROOT}/swarm_capacity_signal_inventory_${RUN_ID}.cargo_test.log"
  SAFE_BEAD_ID="${BEAD_ID//[^[:alnum:]]/-}"
  TARGET_DIR="/tmp/${SAFE_BEAD_ID}-signal-inventory-${RUN_ID}"
  emit_event "rust_proof" "rch" "cargo_test_start" "running" "mixed" "capacity.signal_inventory.rch_started" "none" "${RCH_LOG}" "" false false false

  set +e
  (
    run_rch_cargo_logged "${RCH_LOG}" \
      env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${TARGET_DIR}" RUST_TEST_THREADS=1 \
        cargo test -j 1 -p frankenterm-core --test swarm_capacity_signal_inventory_schema --no-default-features -- --nocapture
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
      emit_event "rust_proof" "rch" "cargo_test_finish" "failed" "unavailable" "capacity.signal_inventory.remote_required_failed" "rch_remote_unavailable_or_refused_local_fallback" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
    else
      emit_event "rust_proof" "rch" "cargo_test_finish" "failed" "unavailable" "capacity.signal_inventory.rch_failed" "cargo_test_failed" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
    fi
    exit "${rch_rc}"
  fi
  emit_event "rust_proof" "rch" "cargo_test_finish" "passed" "measured" "capacity.signal_inventory.rch_passed" "none" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
else
  emit_event "rust_proof" "rch" "cargo_test_skip" "skipped" "unavailable" "capacity.signal_inventory.rch_not_requested" "none" "${LOG_FILE}" "" false false false
fi

emit_event "suite" "e2e_jsonl" "finish" "passed" "mixed" "capacity.signal_inventory.completed" "none" "${LOG_FILE}"
printf '%s\n' "${LOG_FILE}"
