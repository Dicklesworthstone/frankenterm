#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="${SWARM_CAPACITY_COMPLETION_BEAD_ID:-ft-b94bx.10}"
RUN_ID="${SWARM_CAPACITY_COMPLETION_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
ARTIFACT_ROOT="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/swarm_capacity_completion/${RUN_ID}"
LOG_FILE="${ARTIFACT_ROOT}/events.jsonl"
READINESS_FILE="${ROOT_DIR}/docs/attestations/proofs/swarm-capacity-readiness.json"
MANIFEST_FILE="${ROOT_DIR}/docs/attestations/manifest.json"
ENVELOPE_FILE="${ROOT_DIR}/docs/attestations/perf/swarm-capacity-envelope.json"
TARGET_CLASS_ARTIFACT="${ROOT_DIR}/docs/attestations/proofs/resource-cockpit-target-class.json"
RUST_TEST_FILE="${ROOT_DIR}/crates/frankenterm-core/tests/swarm_capacity_completion_readiness.rs"
RUN_RUST_PROOF=0

mkdir -p "${ARTIFACT_ROOT}"
: >"${LOG_FILE}"

usage() {
  cat <<'USAGE'
Usage: bash tests/e2e/test_swarm_capacity_completion_audit.sh [--run-rust-proof]

Static readiness, manifest, and path checks run locally. --run-rust-proof uses
rch and refuses local Cargo fallback for the Rust parser test.
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

fail_step() {
  local item_id="$1"
  local domain="$2"
  local step="$3"
  local evidence_state="$4"
  local reason_code="$5"
  local error_code="$6"
  local artifact_path="$7"
  emit_event "${item_id}" "${domain}" "${step}" "failed" "${evidence_state}" "${reason_code}" "${error_code}" "${artifact_path}"
  exit 1
}

require_command() {
  local command_name="$1"
  if ! command -v "${command_name}" >/dev/null 2>&1; then
    fail_step "${command_name}" "static" "preflight" "unavailable" "capacity.completion.tool_missing" "${command_name}_not_found" "${LOG_FILE}"
  fi
}

require_file() {
  local path="$1"
  local label="$2"
  if [[ ! -f "${path}" ]]; then
    fail_step "${label}" "static" "preflight" "unavailable" "capacity.completion.artifact_missing" "missing_artifact" "${path}"
  fi
}

require_repo_relative_file() {
  local rel_path="$1"
  local label="$2"

  if [[ -z "${rel_path}" || "${rel_path}" == "." || "${rel_path}" == ".." ]]; then
    fail_step "${label}" "static" "path_shape" "unavailable" "capacity.completion.path_unsafe" "empty_or_dot_path" "${LOG_FILE}"
  fi
  if [[ "${rel_path}" == /* || "${rel_path}" == ./* || "${rel_path}" == ../* || "${rel_path}" == */ ]]; then
    fail_step "${label}" "static" "path_shape" "unavailable" "capacity.completion.path_unsafe" "absolute_or_dot_segment_path" "${rel_path}"
  fi
  if [[ "${rel_path}" == *\\* ]]; then
    fail_step "${label}" "static" "path_shape" "unavailable" "capacity.completion.path_unsafe" "backslash_path" "${rel_path}"
  fi

  local segment
  local -a path_segments
  IFS='/' read -r -a path_segments <<<"${rel_path}"
  for segment in "${path_segments[@]}"; do
    if [[ -z "${segment}" || "${segment}" == "." || "${segment}" == ".." || "${segment}" == ".git" ]]; then
      fail_step "${label}" "static" "path_shape" "unavailable" "capacity.completion.path_unsafe" "unsafe_path_segment" "${rel_path}"
    fi
  done

  if [[ ! -f "${ROOT_DIR}/${rel_path}" ]]; then
    fail_step "${label}" "static" "path_exists" "unavailable" "capacity.completion.path_missing" "missing_artifact" "${rel_path}"
  fi
  if ! git -C "${ROOT_DIR}" ls-files --error-unmatch -- "${rel_path}" >/dev/null 2>&1; then
    fail_step "${label}" "static" "path_tracked" "unavailable" "capacity.completion.path_untracked" "untracked_artifact" "${rel_path}"
  fi
}

emit_event "suite" "static" "start" "running" "mixed" "capacity.completion.started" "none" "${LOG_FILE}"

require_command jq
require_command git
require_file "${READINESS_FILE}" "readiness"
require_file "${MANIFEST_FILE}" "manifest"
require_file "${ENVELOPE_FILE}" "envelope"
require_file "${TARGET_CLASS_ARTIFACT}" "target_class"
require_file "${RUST_TEST_FILE}" "rust_test"

for json_file in "${READINESS_FILE}" "${MANIFEST_FILE}" "${ENVELOPE_FILE}" "${TARGET_CLASS_ARTIFACT}"; do
  jq empty "${json_file}"
done
emit_event "json" "static" "jq_empty" "passed" "measured" "capacity.completion.json_valid" "none" "${READINESS_FILE}"

required_states=(measured simulated skipped stale unavailable production_proven)
for state in "${required_states[@]}"; do
  if ! jq -e --arg state "${state}" '.checklist_states[] | select(.state == $state)' "${READINESS_FILE}" >/dev/null; then
    fail_step "${state}" "readiness" "state_catalog" "unavailable" "capacity.completion.state_missing" "state_missing" "${READINESS_FILE}"
  fi
  emit_event "${state}" "readiness" "state_catalog" "passed" "measured" "capacity.completion.state_present" "none" "${READINESS_FILE}"
done

expected_beads=(ft-b94bx.1 ft-b94bx.2 ft-b94bx.3 ft-b94bx.4 ft-b94bx.5 ft-b94bx.6 ft-b94bx.7 ft-b94bx.8 ft-b94bx.9 ft-b94bx.10 ft-b94bx.11 ft-b94bx.12)
for bead in "${expected_beads[@]}"; do
  if ! jq -e --arg bead "${bead}" '.claim_matrix[] | select(.bead_id == $bead)' "${READINESS_FILE}" >/dev/null; then
    fail_step "${bead}" "readiness" "claim_matrix" "unavailable" "capacity.completion.claim_missing" "claim_missing" "${READINESS_FILE}"
  fi
  emit_event "${bead}" "readiness" "claim_matrix" "passed" "measured" "capacity.completion.claim_present" "none" "${READINESS_FILE}"
done

claim_count="$(jq '.claim_matrix | length' "${READINESS_FILE}")"
summary_claim_count="$(jq '.summary.claim_count' "${READINESS_FILE}")"
if [[ "${claim_count}" -ne "${summary_claim_count}" || "${claim_count}" -ne "${#expected_beads[@]}" ]]; then
  fail_step "summary" "readiness" "claim_count" "unavailable" "capacity.completion.claim_count_invalid" "claim_count_invalid" "${READINESS_FILE}"
fi
emit_event "summary" "readiness" "claim_count" "passed" "measured" "capacity.completion.claim_count" "none" "${READINESS_FILE}"

for state in "${required_states[@]}"; do
  key="${state}_claims"
  count="$(jq --arg state "${state}" '[.claim_matrix[] | select(.readiness_state == $state)] | length' "${READINESS_FILE}")"
  summary_count="$(jq --arg key "${key}" '.summary[$key]' "${READINESS_FILE}")"
  if [[ "${count}" -ne "${summary_count}" ]]; then
    fail_step "${state}" "readiness" "summary_counts" "unavailable" "capacity.completion.summary_count_invalid" "summary_count_invalid" "${READINESS_FILE}"
  fi
done
emit_event "summary" "readiness" "summary_counts" "passed" "measured" "capacity.completion.summary_counts" "none" "${READINESS_FILE}"

missing_evidence_claims="$(jq -r '.claim_matrix[] | select((.implementation_surfaces | length == 0) or (.tests | length == 0) or (.retained_artifacts | length == 0)) | .claim_id' "${READINESS_FILE}")"
if [[ -n "${missing_evidence_claims}" ]]; then
  fail_step "claim_matrix" "readiness" "required_evidence" "unavailable" "capacity.completion.claim_lacks_evidence" "claim_lacks_evidence" "${READINESS_FILE}"
fi
emit_event "claim_matrix" "readiness" "required_evidence" "passed" "measured" "capacity.completion.claims_have_evidence" "none" "${READINESS_FILE}"

while IFS= read -r rel_path; do
  require_repo_relative_file "${rel_path}" "claim_path"
done < <(
  jq -r '
    [
      .claim_matrix[] as $claim
      | ($claim.implementation_surfaces[]?.path),
        ($claim.tests[]?.path),
        ($claim.retained_artifacts[]?.path),
        ($claim.target_class_proof.artifact),
        ($claim.release_attestation.manifest_path // empty)
    ]
    | unique[]
    | select(. != "")
  ' "${READINESS_FILE}"
)
emit_event "claim_path" "static" "paths_tracked" "passed" "measured" "capacity.completion.paths_tracked" "none" "${READINESS_FILE}"

if ! jq -e --arg path "docs/attestations/proofs/swarm-capacity-readiness.json" '
  .slots[]
  | select(
      .path == $path
      and .category == "proofs/robot-contracts"
      and .media_type == "application/json"
      and .produced_by_bead == "ft-b94bx.10"
      and (.proof_categories | index(4) != null)
      and (.proof_categories | index(5) != null)
    )
' "${MANIFEST_FILE}" >/dev/null; then
  fail_step "manifest" "attestation" "slot" "unavailable" "capacity.completion.manifest_slot_missing" "manifest_slot_missing" "${MANIFEST_FILE}"
fi
emit_event "manifest" "attestation" "slot" "passed" "measured" "capacity.completion.manifest_slot" "none" "${MANIFEST_FILE}"

if [[ "$(jq -r '.status' "${TARGET_CLASS_ARTIFACT}")" == "skipped_not_proven" && "$(jq -r '.current_artifact.status' "${TARGET_CLASS_ARTIFACT}")" == "skipped_not_proven" ]]; then
  if ! jq -e '
    .overall_status == "blocked_target_class_not_proven"
    and .summary.target_class_high_scale_claim_allowed == false
    and .summary.release_wording_allowed == false
    and ([.claim_matrix[] | select(.target_class_proof.state == "production_proven" or .release_attestation.state == "production_proven")] | length == 0)
  ' "${READINESS_FILE}" >/dev/null; then
    fail_step "target_class" "readiness" "skip_gate" "skipped" "capacity.completion.target_class_overclaim" "target_class_overclaim" "${READINESS_FILE}"
  fi
fi
if [[ "$(jq -r '.status' "${ENVELOPE_FILE}")" != "blocked_target_class_not_proven" ]]; then
  fail_step "envelope" "attestation" "status" "unavailable" "capacity.completion.envelope_status_changed" "envelope_status_changed" "${ENVELOPE_FILE}"
fi
emit_event "target_class" "readiness" "skip_gate" "passed" "skipped" "capacity.completion.target_class_blocked" "none" "${TARGET_CLASS_ARTIFACT}"

if [[ "${RUN_RUST_PROOF}" -eq 1 ]]; then
  # shellcheck source=tests/e2e/lib_rch_guards.sh
  RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
  RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
  source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
  rch_init "${ARTIFACT_ROOT}" "${RUN_ID}" "swarm_capacity_completion" "${ROOT_DIR}"
  ensure_rch_ready

  RCH_LOG="${ARTIFACT_ROOT}/swarm_capacity_completion_${RUN_ID}.cargo_test.log"
  SAFE_BEAD_ID="${BEAD_ID//[^[:alnum:]]/-}"
  TARGET_DIR="/tmp/${SAFE_BEAD_ID}-completion-${RUN_ID}"
  emit_event "rust_proof" "rch" "cargo_test_start" "running" "mixed" "capacity.completion.rch_started" "none" "${RCH_LOG}" "" false false false

  set +e
  (
    run_rch_cargo_logged "${RCH_LOG}" \
      env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${TARGET_DIR}" RUST_TEST_THREADS=1 \
        cargo test -j 1 -p frankenterm-core --test swarm_capacity_completion_readiness --no-default-features -- --nocapture
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
    emit_event "rust_proof" "rch" "cargo_test_finish" "failed" "unavailable" "capacity.completion.rch_failed" "cargo_test_failed" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
    exit "${rch_rc}"
  fi
  emit_event "rust_proof" "rch" "cargo_test_finish" "passed" "measured" "capacity.completion.rch_passed" "none" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
else
  emit_event "rust_proof" "rch" "cargo_test_skip" "skipped" "capacity.completion.rch_not_requested" "none" "${LOG_FILE}" "" false false false
fi

emit_event "suite" "static" "finish" "passed" "mixed" "capacity.completion.completed" "none" "${LOG_FILE}"
printf 'swarm capacity completion audit: static verifier passed (%s claims)\n' "${claim_count}"
