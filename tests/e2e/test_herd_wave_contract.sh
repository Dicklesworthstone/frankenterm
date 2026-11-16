#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="${HERD_WAVE_BEAD_ID:-ft-5bwjf.7}"
RUN_ID="${HERD_WAVE_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
ARTIFACT_ROOT="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/herd_wave_contract/${RUN_ID}"
LOG_FILE="${ARTIFACT_ROOT}/events.jsonl"
FIXTURE_MATRIX="${ROOT_DIR}/crates/frankenterm-core/tests/fixtures/herd_wave_contract/fixture_matrix.json"
CONFORMANCE_MATRIX="${ROOT_DIR}/crates/frankenterm-core/tests/fixtures/herd_wave_contract/conformance_matrix.json"
SCHEMA_FILE="${ROOT_DIR}/docs/json-schema/ft-herd-wave.json"
RUN_RUST_PROOF=0
UPDATE_GOLDENS=0

mkdir -p "${ARTIFACT_ROOT}"

usage() {
  cat <<'USAGE'
Usage: tests/e2e/test_herd_wave_contract.sh [--run-rust-proof] [--update-goldens]

Static checks run locally. --run-rust-proof uses rch and refuses local Cargo fallback.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --run-rust-proof)
      RUN_RUST_PROOF=1
      shift
      ;;
    --update-goldens)
      UPDATE_GOLDENS=1
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
  local scenario_id="$1"
  local surface="$2"
  local step="$3"
  local outcome="$4"
  local reason_code="$5"
  local error_code="$6"
  local artifact_path="$7"
  local selected_worker="${8:-}"
  local cargo_reached="${9:-false}"
  local rustc_reached="${10:-false}"
  local test_execution_reached="${11:-false}"
  local pane_count="${12:-null}"
  local cohort_count="${13:-null}"
  local dominant_kind="${14:-}"
  local target_class_proof_available="${15:-null}"
  local ts

  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  jq -cn \
    --arg timestamp "${ts}" \
    --arg bead_id "${BEAD_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg scenario_id "${scenario_id}" \
    --arg surface "${surface}" \
    --arg step "${step}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path}" \
    --arg selected_worker "${selected_worker}" \
    --argjson cargo_reached "${cargo_reached}" \
    --argjson rustc_reached "${rustc_reached}" \
    --argjson test_execution_reached "${test_execution_reached}" \
    --argjson pane_count "${pane_count}" \
    --argjson cohort_count "${cohort_count}" \
    --arg dominant_kind "${dominant_kind}" \
    --argjson target_class_proof_available "${target_class_proof_available}" \
    '{
      timestamp: $timestamp,
      bead_id: $bead_id,
      run_id: $run_id,
      scenario_id: $scenario_id,
      surface: $surface,
      step: $step,
      outcome: $outcome,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path,
      selected_worker: ($selected_worker | if . == "" then null else . end),
      cargo_reached: $cargo_reached,
      rustc_reached: $rustc_reached,
      test_execution_reached: $test_execution_reached,
      pane_count: $pane_count,
      cohort_count: $cohort_count,
      dominant_kind: ($dominant_kind | if . == "" then null else . end),
      target_class_proof_available: $target_class_proof_available
    }' >> "${LOG_FILE}"
}

require_file() {
  local path="$1"
  local scenario="$2"
  if [[ ! -f "${path}" ]]; then
    emit_event "${scenario}" "static" "preflight" "failed" "herd_wave.artifact.missing" "missing_artifact" "${path}"
    exit 1
  fi
}

emit_event "suite" "e2e_jsonl" "start" "running" "herd_wave.e2e.started" "none" "${LOG_FILE}"

if ! command -v jq >/dev/null 2>&1; then
  emit_event "suite" "static" "preflight_jq" "failed" "herd_wave.tool.missing_jq" "jq_not_found" "${LOG_FILE}"
  exit 1
fi

require_file "${FIXTURE_MATRIX}" "fixture_matrix"
require_file "${CONFORMANCE_MATRIX}" "conformance_matrix"
require_file "${SCHEMA_FILE}" "schema"

if [[ "${UPDATE_GOLDENS}" -eq 1 ]]; then
  if [[ "${UPDATE_HERD_WAVE_GOLDENS:-}" != "1" ]]; then
    emit_event "golden_update" "e2e_jsonl" "update_gate" "failed" "herd_wave.goldens.update_gate_missing" "update_gate_missing" "${LOG_FILE}"
    exit 1
  fi
  emit_event "golden_update" "e2e_jsonl" "update_gate" "passed" "herd_wave.goldens.review_required" "none" "${LOG_FILE}"
fi

jq empty "${FIXTURE_MATRIX}"
emit_event "fixture_matrix" "static" "jq_empty" "passed" "herd_wave.fixture_matrix.valid_json" "none" "${FIXTURE_MATRIX}"

jq empty "${CONFORMANCE_MATRIX}"
emit_event "conformance_matrix" "static" "jq_empty" "passed" "herd_wave.conformance_matrix.valid_json" "none" "${CONFORMANCE_MATRIX}"

jq empty "${SCHEMA_FILE}"
emit_event "schema" "static" "jq_empty" "passed" "herd_wave.schema.valid_json" "none" "${SCHEMA_FILE}"

scenario_count="$(jq '.scenarios | length' "${FIXTURE_MATRIX}")"
must_total="$(jq '[.requirements[] | select(.level == "MUST")] | length' "${CONFORMANCE_MATRIX}")"
must_uncovered="$(jq '[.requirements[] | select(.level == "MUST" and .status != "covered")] | length' "${CONFORMANCE_MATRIX}")"

if [[ "${scenario_count}" -lt 12 ]]; then
  emit_event "fixture_matrix" "static" "scenario_count" "failed" "herd_wave.fixture_matrix.too_few_scenarios" "scenario_count_low" "${FIXTURE_MATRIX}"
  exit 1
fi
emit_event "fixture_matrix" "static" "scenario_count" "passed" "herd_wave.fixture_matrix.scenario_count" "none" "${FIXTURE_MATRIX}"

high_scale_rows="$(jq '[.scenarios[] | select(.scenario_id == "synthetic_200_pane_high_scale")] | length' "${FIXTURE_MATRIX}")"
if [[ "${high_scale_rows}" -ne 1 ]]; then
  emit_event "synthetic_200_pane_high_scale" "static" "fixture_present" "failed" "herd_wave.scale.high_scale_fixture_missing" "high_scale_fixture_missing" "${FIXTURE_MATRIX}"
  exit 1
fi
high_scale_pane_count="$(jq -r '.scenarios[] | select(.scenario_id == "synthetic_200_pane_high_scale") | .pane_count' "${FIXTURE_MATRIX}")"
high_scale_cohort_count="$(jq -r '.scenarios[] | select(.scenario_id == "synthetic_200_pane_high_scale") | .scale_proof.cohort_count' "${FIXTURE_MATRIX}")"
high_scale_dominant_kind="$(jq -r '.scenarios[] | select(.scenario_id == "synthetic_200_pane_high_scale") | .expected.dominant_kind' "${FIXTURE_MATRIX}")"
target_class_available="$(jq -r '.scenarios[] | select(.scenario_id == "synthetic_200_pane_high_scale") | .scale_proof.target_class_hardware_proof.available' "${FIXTURE_MATRIX}")"
if [[ "${high_scale_pane_count}" -lt 200 || "${high_scale_cohort_count}" -lt 200 || "${target_class_available}" != "false" ]]; then
  emit_event "synthetic_200_pane_high_scale" "static" "target_class_gate" "failed" "herd_wave.scale.high_scale_fixture_invalid" "high_scale_fixture_invalid" "${FIXTURE_MATRIX}" "" false false false "${high_scale_pane_count}" "${high_scale_cohort_count}" "${high_scale_dominant_kind}" "${target_class_available}"
  exit 1
fi
emit_event "synthetic_200_pane_high_scale" "static" "target_class_gate" "passed" "herd_wave.target_class.proof_unavailable" "none" "${FIXTURE_MATRIX}" "" false false false "${high_scale_pane_count}" "${high_scale_cohort_count}" "${high_scale_dominant_kind}" "${target_class_available}"

if [[ "${must_total}" -eq 0 || "${must_uncovered}" -ne 0 ]]; then
  emit_event "conformance_matrix" "static" "must_coverage" "failed" "herd_wave.conformance.must_not_covered" "must_uncovered" "${CONFORMANCE_MATRIX}"
  exit 1
fi
emit_event "conformance_matrix" "static" "must_coverage" "passed" "herd_wave.conformance.must_covered" "none" "${CONFORMANCE_MATRIX}"

privacy_hits="$(grep -R -E 'Bearer ft-5bwjf-private-token|Cookie: ft_session=private|PROMPT_BODY: deploy prod|raw pane excerpt with secret' \
  "${FIXTURE_MATRIX}" "${CONFORMANCE_MATRIX}" "${SCHEMA_FILE}" || true)"
if [[ -n "${privacy_hits}" ]]; then
  emit_event "privacy" "static" "sentinel_scan" "failed" "herd_wave.privacy.raw_content_leak" "privacy_violation" "${LOG_FILE}"
  exit 1
fi
emit_event "privacy" "static" "sentinel_scan" "passed" "herd_wave.privacy.no_raw_content" "none" "${LOG_FILE}"

if [[ "${RUN_RUST_PROOF}" -eq 1 ]]; then
  # shellcheck source=tests/e2e/lib_rch_guards.sh
  RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
  RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
  source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
  rch_init "${ARTIFACT_ROOT}" "${RUN_ID}" "herd_wave_contract" "${ROOT_DIR}"
  ensure_rch_ready

  RCH_LOG="${ARTIFACT_ROOT}/herd_wave_contract_${RUN_ID}.cargo_test.log"
  SAFE_BEAD_ID="${BEAD_ID//[^[:alnum:]]/-}"
  TARGET_DIR="/tmp/${SAFE_BEAD_ID}-herd-wave-contract-${RUN_ID}"
  CARGO_BUILD_JOBS="${HERD_WAVE_CARGO_BUILD_JOBS:-2}"
  emit_event "rust_proof" "rch" "cargo_test_start" "running" "herd_wave.rch.cargo_test_started" "none" "${RCH_LOG}" "" false false false

  set +e
  (
    run_rch_cargo_logged "${RCH_LOG}" \
      env CARGO_TARGET_DIR="${TARGET_DIR}" CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS}" cargo test -j "${CARGO_BUILD_JOBS}" \
        -p frankenterm-core --test herd_wave_conformance -- --nocapture
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
      emit_event "rust_proof" "rch" "cargo_test_finish" "failed" "herd_wave.rch.remote_required_failed" "rch_remote_unavailable_or_refused_local_fallback" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
    else
      emit_event "rust_proof" "rch" "cargo_test_finish" "failed" "herd_wave.rch.cargo_test_failed" "cargo_test_failed" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
    fi
    exit "${rch_rc}"
  fi
  emit_event "rust_proof" "rch" "cargo_test_finish" "passed" "herd_wave.rch.remote_test_passed" "none" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
  emit_event "synthetic_200_pane_high_scale" "rch" "target_class_proof_status" "passed" "herd_wave.target_class.proof_unavailable" "none" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}" "${high_scale_pane_count}" "${high_scale_cohort_count}" "${high_scale_dominant_kind}" "${target_class_available}"
else
  emit_event "rust_proof" "rch" "cargo_test_skip" "skipped" "herd_wave.rch.not_requested" "none" "${LOG_FILE}" "" false false false
fi

emit_event "suite" "e2e_jsonl" "finish" "passed" "herd_wave.e2e.completed" "none" "${LOG_FILE}"
printf '%s\n' "${LOG_FILE}"
