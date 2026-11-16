#!/usr/bin/env bash
# ft-e87u6.1 -- null-slot reconciliation E2E harness.
#
# This is intentionally a light local harness: it reads the manifest,
# sidecar, filesystem, git, and Beads state. It sources lib_rch_guards.sh
# so the artifact layout matches the RCH-aware E2E family, but it does not
# run Cargo or any heavy verifier.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-e87u6.1"
SCENARIO_ID="null_slot_reconciliation"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/goal-line/ft-e87u6/${SCENARIO_ID}/${RUN_ID}"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
DERIVED_FILE="${ARTIFACT_DIR}/derived-null-slots.json"
COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"

MANIFEST="${ROOT_DIR}/docs/attestations/manifest.json"
SIDECAR="${ROOT_DIR}/docs/attestations/null-slot-reconciliation.json"
WORKSHEET="${ROOT_DIR}/docs/attestations/null-slot-reconciliation.md"

mkdir -p "${ARTIFACT_DIR}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_e87u6_1_null_slot_reconciliation" "${ROOT_DIR}"

TOTAL=0
PASS=0
FAIL=0

record_command() {
  printf '%s\n' "$*" >> "${COMMANDS_FILE}"
}

emit_log() {
  local step="$1"
  local outcome="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local message="$6"
  local status
  status="${outcome}"
  jq -cn \
    --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg surface "attestation-null-slot-reconciliation" \
    --arg step "${step}" \
    --arg outcome "${outcome}" \
    --arg status "${status}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg backend "local_light" \
    --arg platform "$(uname -srm)" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    --arg redaction "none" \
    --arg message "${message}" \
    --argjson duration_ms 0 \
    '{
      timestamp: $timestamp,
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      surface: $surface,
      step: $step,
      outcome: $outcome,
      status: $status,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path,
      duration_ms: $duration_ms,
      correlation_id: $correlation_id,
      backend: $backend,
      platform: $platform,
      artifact_dir: $artifact_dir,
      redaction: $redaction,
      message: $message
    }' >> "${STRUCTURED_LOG}"
}

record_result() {
  local step="$1"
  local ok="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local message="$6"
  TOTAL=$((TOTAL + 1))
  if [[ "${ok}" == "true" ]]; then
    PASS=$((PASS + 1))
    emit_log "${step}" "passed" "${reason_code}" "${error_code}" "${artifact_path}" "${message}"
  else
    FAIL=$((FAIL + 1))
    emit_log "${step}" "failed" "${reason_code}" "${error_code}" "${artifact_path}" "${message}"
  fi
}

write_summary() {
  local outcome="$1"
  jq -n \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg outcome "${outcome}" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    --arg structured_log "${STRUCTURED_LOG}" \
    --arg sidecar "${SIDECAR}" \
    --arg worksheet "${WORKSHEET}" \
    --arg manifest "${MANIFEST}" \
    --arg derived_null_slots "${DERIVED_FILE}" \
    --argjson total "${TOTAL}" \
    --argjson passed "${PASS}" \
    --argjson failed "${FAIL}" \
    '{
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      run_id: $run_id,
      correlation_id: $correlation_id,
      outcome: $outcome,
      artifact_dir: $artifact_dir,
      structured_log: $structured_log,
      inputs: {
        manifest: $manifest,
        sidecar: $sidecar,
        worksheet: $worksheet
      },
      outputs: {
        derived_null_slots: $derived_null_slots
      },
      counts: {
        total: $total,
        passed: $passed,
        failed: $failed
      }
    }' > "${SUMMARY_FILE}"
}

require_cmd() {
  local cmd="$1"
  if command -v "${cmd}" >/dev/null 2>&1; then
    record_result "preflight.${cmd}" "true" "command_present" "none" "${cmd}" "${cmd} available"
    return 0
  fi
  record_result "preflight.${cmd}" "false" "missing_prerequisite" "E2E-PREREQ" "${cmd}" "${cmd} missing"
  write_summary "failed"
  exit 1
}

sha256_file() {
  local file="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "${file}" | awk '{print $1}'
    return 0
  fi
  sha256sum "${file}" | awk '{print $1}'
}

json_get_slot() {
  local category="$1"
  local bead="$2"
  local expr="$3"
  jq -r --arg category "${category}" --arg bead "${bead}" "${expr}" "${SIDECAR}"
}

cd "${ROOT_DIR}"
: > "${STRUCTURED_LOG}"
: > "${COMMANDS_FILE}"

require_cmd bash
require_cmd jq
require_cmd git
require_cmd br
if command -v shasum >/dev/null 2>&1 || command -v sha256sum >/dev/null 2>&1; then
  record_result "preflight.sha256" "true" "command_present" "none" "sha256" "sha256 tool available"
else
  record_result "preflight.sha256" "false" "missing_prerequisite" "E2E-PREREQ" "sha256" "shasum or sha256sum missing"
  write_summary "failed"
  exit 1
fi

for required in "${MANIFEST}" "${SIDECAR}" "${WORKSHEET}"; do
  if [[ -f "${required}" ]]; then
    record_result "preflight.required_file" "true" "required_file_present" "none" "${required}" "required file present"
  else
    record_result "preflight.required_file" "false" "missing_artifact" "ARTIFACT-MISSING" "${required}" "required file missing"
    write_summary "failed"
    exit 1
  fi
done

record_command "jq empty ${MANIFEST}"
jq empty "${MANIFEST}"
record_result "syntax.manifest_json" "true" "json_valid" "none" "${MANIFEST}" "manifest parses as JSON"

record_command "jq empty ${SIDECAR}"
jq empty "${SIDECAR}"
record_result "syntax.sidecar_json" "true" "json_valid" "none" "${SIDECAR}" "sidecar parses as JSON"

actual_manifest_sha="$(sha256_file "${MANIFEST}")"
expected_manifest_sha="$(jq -r '.manifest.sha256' "${SIDECAR}")"
if [[ "${actual_manifest_sha}" == "${expected_manifest_sha}" ]]; then
  record_result "manifest.sha256" "true" "hash_match" "none" "${MANIFEST}" "manifest hash matches sidecar"
else
  record_result "manifest.sha256" "false" "hash_mismatch" "HASH-MISMATCH" "${MANIFEST}" "manifest hash differs from sidecar"
fi

record_command "jq derive null slots"
jq '[.slots[] | select(.path == null) | {category, produced_by_bead, media_type, description}]' \
  "${MANIFEST}" > "${DERIVED_FILE}"

derived_count="$(jq 'length' "${DERIVED_FILE}")"
sidecar_total="$(jq '.summary.total_null_slots' "${SIDECAR}")"
sidecar_slot_count="$(jq '.slots | length' "${SIDECAR}")"
if [[ "${derived_count}" == "${sidecar_total}" && "${derived_count}" == "${sidecar_slot_count}" ]]; then
  record_result "slots.count" "true" "count_match" "none" "${DERIVED_FILE}" "derived null-slot count matches sidecar"
else
  record_result "slots.count" "false" "count_mismatch" "COUNT-MISMATCH" "${DERIVED_FILE}" "derived=${derived_count} summary=${sidecar_total} sidecar_slots=${sidecar_slot_count}"
fi

if jq -e '
    (.summary.populate_from == ([.slots[] | select(.disposition == "populate-from")] | length)) and
    (.summary.substrate_recovery == ([.slots[] | select(.disposition == "substrate-recovery")] | length)) and
    (.summary.deferred == ([.slots[] | select(.disposition == "deferred")] | length)) and
    (.summary.slot_deletion == ([.slots[] | select(.disposition == "slot-deletion")] | length)) and
    (.summary.human_review_required == ([.slots[] | select(.disposition == "human_review_required")] | length))
  ' "${SIDECAR}" >/dev/null; then
  record_result "slots.summary_counts" "true" "summary_counts_match" "none" "${SIDECAR}" "summary counts match rows"
else
  record_result "slots.summary_counts" "false" "summary_counts_mismatch" "SUMMARY-MISMATCH" "${SIDECAR}" "summary counts do not match rows"
fi

if jq -e '[.slots[] | select(.disposition == "human_review_required")] | length == 0' "${SIDECAR}" >/dev/null; then
  record_result "slots.human_review_required" "true" "no_human_review_required" "none" "${SIDECAR}" "all missing artifacts have deliberate dispositions"
else
  record_result "slots.human_review_required" "false" "human_review_required_present" "HUMAN-REVIEW-REQUIRED" "${SIDECAR}" "sidecar contains unresolved human_review_required rows"
fi

while IFS=$'\t' read -r category bead media_type; do
  row_selector='.slots | map(select(.category == $category and .produced_by_bead == $bead))'
  row_count="$(json_get_slot "${category}" "${bead}" "${row_selector} | length")"
  step_base="slot.${category//\//_}.${bead}"

  if [[ "${row_count}" != "1" ]]; then
    record_result "${step_base}.row_presence" "false" "sidecar_row_mismatch" "SIDECAR-ROW" "${SIDECAR}" "expected one sidecar row, found ${row_count}"
    continue
  fi
  record_result "${step_base}.row_presence" "true" "sidecar_row_present" "none" "${SIDECAR}" "one sidecar row present"

  sidecar_media="$(json_get_slot "${category}" "${bead}" "${row_selector}[0].manifest_media_type")"
  if [[ "${sidecar_media}" == "${media_type}" ]]; then
    record_result "${step_base}.media_type" "true" "media_type_match" "none" "${SIDECAR}" "media type matches manifest"
  else
    record_result "${step_base}.media_type" "false" "media_type_mismatch" "MEDIA-MISMATCH" "${SIDECAR}" "manifest=${media_type} sidecar=${sidecar_media}"
  fi

  disposition="$(json_get_slot "${category}" "${bead}" "${row_selector}[0].disposition")"
  case "${disposition}" in
    populate-from)
      artifact_rel="$(json_get_slot "${category}" "${bead}" "${row_selector}[0].artifact_path")"
      artifact_path="${ROOT_DIR}/${artifact_rel}"
      expected_hash="$(json_get_slot "${category}" "${bead}" "${row_selector}[0].artifact_sha256")"
      if [[ -f "${artifact_path}" ]]; then
        record_result "${step_base}.artifact_exists" "true" "artifact_found" "none" "${artifact_rel}" "artifact exists"
      else
        record_result "${step_base}.artifact_exists" "false" "missing_artifact" "ARTIFACT-MISSING" "${artifact_rel}" "artifact missing"
        continue
      fi
      actual_hash="$(sha256_file "${artifact_path}")"
      if [[ "${actual_hash}" == "${expected_hash}" ]]; then
        record_result "${step_base}.artifact_hash" "true" "hash_match" "none" "${artifact_rel}" "artifact hash matches"
      else
        record_result "${step_base}.artifact_hash" "false" "hash_mismatch" "HASH-MISMATCH" "${artifact_rel}" "actual=${actual_hash} expected=${expected_hash}"
      fi
      if [[ "${media_type}" == "application/json" ]]; then
        record_command "jq empty ${artifact_rel}"
        if jq empty "${artifact_path}"; then
          record_result "${step_base}.artifact_json" "true" "json_valid" "none" "${artifact_rel}" "artifact parses as JSON"
        else
          record_result "${step_base}.artifact_json" "false" "json_invalid" "JSON-INVALID" "${artifact_rel}" "artifact failed JSON parse"
        fi
      fi
      ;;
    substrate-recovery)
      follow_up="$(json_get_slot "${category}" "${bead}" "${row_selector}[0].follow_up_bead")"
      artifact_value="$(json_get_slot "${category}" "${bead}" "${row_selector}[0].artifact_path")"
      if [[ "${artifact_value}" == "null" && "${follow_up}" =~ ^ft- ]]; then
        record_result "${step_base}.recovery_shape" "true" "recovery_bead_recorded" "none" "${follow_up}" "substrate recovery row records follow-up bead"
      else
        record_result "${step_base}.recovery_shape" "false" "recovery_shape_invalid" "RECOVERY-SHAPE" "${SIDECAR}" "artifact_path=${artifact_value} follow_up=${follow_up}"
        continue
      fi
      follow_log="${ARTIFACT_DIR}/${follow_up}.json"
      record_command "br show ${follow_up} --json"
      if br show "${follow_up}" --json > "${follow_log}"; then
        follow_status="$(jq -r '.[0].status // empty' "${follow_log}")"
        if [[ "${follow_status}" == "open" || "${follow_status}" == "in_progress" ]]; then
          record_result "${step_base}.recovery_bead_state" "true" "recovery_bead_live" "none" "${follow_up}" "follow-up bead is live"
        else
          record_result "${step_base}.recovery_bead_state" "false" "recovery_bead_not_live" "BEAD-NOT-LIVE" "${follow_up}" "follow-up bead status=${follow_status}"
        fi
      else
        record_result "${step_base}.recovery_bead_state" "false" "recovery_bead_missing" "BEAD-MISSING" "${follow_up}" "br show failed"
      fi
      ;;
    deferred|slot-deletion)
      record_result "${step_base}.disposition" "true" "explicit_nonpopulate_disposition" "none" "${SIDECAR}" "explicit ${disposition} row"
      ;;
    *)
      record_result "${step_base}.disposition" "false" "unknown_disposition" "DISPOSITION-UNKNOWN" "${SIDECAR}" "unknown disposition ${disposition}"
      ;;
  esac
done < <(jq -r '.[] | [.category, .produced_by_bead, .media_type] | @tsv' "${DERIVED_FILE}")

if [[ "${FAIL}" -eq 0 ]]; then
  write_summary "passed"
  echo "PASS: ${BEAD_ID} null-slot reconciliation (${PASS}/${TOTAL})"
  exit 0
fi

write_summary "failed"
echo "FAIL: ${BEAD_ID} null-slot reconciliation (${FAIL} failed / ${TOTAL} total)" >&2
exit 1
