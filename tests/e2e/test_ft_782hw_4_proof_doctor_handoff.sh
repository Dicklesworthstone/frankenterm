#!/usr/bin/env bash
# E2E: proof-doctor handoff and durable proof-record wrapper.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-782hw.4"
SCENARIO_ID="proof_doctor_handoff"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
PROOF_RECORDS="${ARTIFACT_DIR}/proof-records.jsonl"
REMOTE_TARGET_DIR="${FT_CARGO_TARGET_DIR:-/tmp/ft-782hw-4-proof-doctor-handoff-${RUN_ID}}"
FT_BINARY="${FT_BINARY:-${FT_BIN:-}}"

mkdir -p "${ARTIFACT_DIR}"
: > "${STRUCTURED_LOG}"
: > "${COMMANDS_FILE}"

TOTAL=0
PASS=0
FAIL=0

record_command() {
  printf '%q ' "$@" >> "${COMMANDS_FILE}"
  printf '\n' >> "${COMMANDS_FILE}"
}

emit_event() {
  local step="$1" phase="$2" status="$3" reason_code="$4" artifact_path="$5" message="$6"
  jq -cn \
    --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg step "${step}" \
    --arg phase "${phase}" \
    --arg status "${status}" \
    --arg reason_code "${reason_code}" \
    --arg artifact_path "${artifact_path}" \
    --arg message "${message}" \
    '{
      timestamp: $timestamp,
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      surface: "proof-doctor-handoff",
      correlation_id: $correlation_id,
      step: $step,
      phase: $phase,
      status: $status,
      reason_code: $reason_code,
      artifact_path: $artifact_path,
      message: $message
    }' >> "${STRUCTURED_LOG}"
}

record_result() {
  local step="$1" ok="$2" phase="$3" status="$4" reason_code="$5" artifact_path="$6" message="$7"
  TOTAL=$((TOTAL + 1))
  if [[ "${ok}" == "true" ]]; then
    PASS=$((PASS + 1))
    emit_event "${step}" "${phase}" "${status}" "${reason_code}" "${artifact_path}" "${message}"
  else
    FAIL=$((FAIL + 1))
    emit_event "${step}" "${phase}" "failed" "${reason_code}" "${artifact_path}" "${message}"
    write_summary "failed"
    exit 1
  fi
}

write_summary() {
  local outcome="${1:-unknown}"
  local record_count=0
  if [[ -f "${PROOF_RECORDS}" ]]; then
    record_count="$(grep -cve '^[[:space:]]*$' "${PROOF_RECORDS}" || true)"
  fi
  jq -n \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg outcome "${outcome}" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    --arg structured_log "${STRUCTURED_LOG}" \
    --arg commands "${COMMANDS_FILE}" \
    --arg proof_records "${PROOF_RECORDS}" \
    --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
    --argjson total "${TOTAL}" \
    --argjson passed "${PASS}" \
    --argjson failed "${FAIL}" \
    --argjson proof_record_count "${record_count}" \
    '{
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      run_id: $run_id,
      correlation_id: $correlation_id,
      outcome: $outcome,
      artifact_dir: $artifact_dir,
      structured_log: $structured_log,
      commands: $commands,
      proof_records: $proof_records,
      remote_target_dir: $remote_target_dir,
      counts: {
        total: $total,
        passed: $passed,
        failed: $failed,
        proof_records: $proof_record_count
      }
    }' > "${SUMMARY_FILE}"
}

write_bootstrap_failure_summary() {
  local missing_command="$1"
  TOTAL=1
  FAIL=1
  cat > "${STRUCTURED_LOG}" <<JSON
{"timestamp":"$(date -u +"%Y-%m-%dT%H:%M:%SZ")","bead_id":"${BEAD_ID}","scenario_id":"${SCENARIO_ID}","surface":"proof-doctor-handoff","correlation_id":"${CORRELATION_ID}","step":"preflight.${missing_command}","phase":"preflight","status":"failed","reason_code":"missing_prerequisite","artifact_path":"${missing_command}","message":"${missing_command} missing"}
JSON
  cat > "${SUMMARY_FILE}" <<JSON
{"bead_id":"${BEAD_ID}","scenario_id":"${SCENARIO_ID}","run_id":"${RUN_ID}","correlation_id":"${CORRELATION_ID}","outcome":"failed","artifact_dir":"${ARTIFACT_DIR}","structured_log":"${STRUCTURED_LOG}","commands":"${COMMANDS_FILE}","proof_records":"${PROOF_RECORDS}","remote_target_dir":"${REMOTE_TARGET_DIR}","counts":{"total":${TOTAL},"passed":${PASS},"failed":${FAIL},"proof_records":0}}
JSON
}

find_ft_binary() {
  if [[ -n "${FT_BINARY}" ]]; then
    [[ -x "${FT_BINARY}" ]] && return 0
    echo "FT_BINARY/FT_BIN is set but not executable: ${FT_BINARY}" >&2
    return 1
  fi

  local target_dir="${CARGO_TARGET_DIR:-${ROOT_DIR}/target}"
  local candidates=(
    "${target_dir}/debug/ft"
    "${target_dir}/release/ft"
    "${ROOT_DIR}/target/debug/ft"
    "${ROOT_DIR}/target/release/ft"
  )

  local candidate
  for candidate in "${candidates[@]}"; do
    if [[ -x "${candidate}" ]]; then
      FT_BINARY="${candidate}"
      return 0
    fi
  done

  echo "Could not find ft binary." >&2
  echo "[INFO] Build through RCH first, for example:" >&2
  echo "[INFO]   rch exec -- env CARGO_TARGET_DIR=/tmp/ft-782hw-4-proof cargo build -p frankenterm --bin ft" >&2
  echo "[INFO] Then rerun with FT_BINARY=/tmp/ft-782hw-4-proof/debug/ft." >&2
  return 1
}

require_cmd() {
  local command_name="$1"
  if command -v "${command_name}" >/dev/null 2>&1; then
    record_result "preflight.${command_name}" "true" "preflight" "available" "command_present" "${command_name}" "${command_name} available"
    return 0
  fi
  record_result "preflight.${command_name}" "false" "preflight" "missing" "missing_prerequisite" "${command_name}" "${command_name} missing"
}

write_fixture_artifacts() {
  PASS_ARTIFACT="${ARTIFACT_DIR}/observed-pass.json"
  SOURCE_FAIL_ARTIFACT="${ARTIFACT_DIR}/observed-source-fail.json"
  TEST_FAIL_ARTIFACT="${ARTIFACT_DIR}/observed-test-fail.json"
  INFRA_BLOCKED_ARTIFACT="${ARTIFACT_DIR}/observed-infra-blocked.json"
  DIRTY_BLOCKED_ARTIFACT="${ARTIFACT_DIR}/observed-dirty-tree.json"
  SYNC_GAP_ARTIFACT="${ARTIFACT_DIR}/observed-sync-gap.json"
  SKIPPED_ARTIFACT="${ARTIFACT_DIR}/observed-skipped-not-proven.json"

  cat > "${PASS_ARTIFACT}" <<JSON
{"status":"passed","selected_worker":"vmi-proof","remote_cargo_reached":true,"rustc_reached":true,"test_binary_started":true,"remote_exit_code":0,"wrapper_exit_code":0,"artifact_retrieval_status":"complete","artifact_dir":"${ARTIFACT_DIR}","artifacts":{"command_log":"observed-pass.log"}}
JSON
  cat > "${SOURCE_FAIL_ARTIFACT}" <<JSON
{"remote_cargo_reached":true,"rustc_reached":true,"remote_exit_code":101,"wrapper_exit_code":101,"artifact_retrieval_status":"complete","diagnostic_summary":"missing field initializer in proof lane fixture","diagnostic_paths":["crates/frankenterm-core-audit-types/src/proof_lane.rs"],"artifact_dir":"${ARTIFACT_DIR}"}
JSON
  cat > "${TEST_FAIL_ARTIFACT}" <<JSON
{"remote_cargo_reached":true,"rustc_reached":true,"test_binary_started":true,"remote_exit_code":101,"wrapper_exit_code":101,"artifact_retrieval_status":"complete","diagnostic_summary":"assertion failed in proof handoff fixture","diagnostic_paths":["crates/frankenterm-core-audit-types/src/proof_lane.rs"],"artifact_dir":"${ARTIFACT_DIR}"}
JSON
  cat > "${INFRA_BLOCKED_ARTIFACT}" <<JSON
{"failure_reason_code":"queue_timeout_before_assignment","failure_reason_detail":"remote worker queue timed out before assignment","wrapper_exit_code":1,"artifact_retrieval_status":"partial","artifact_dir":"${ARTIFACT_DIR}"}
JSON
  cat > "${DIRTY_BLOCKED_ARTIFACT}" <<JSON
{"dirty_paths":[{"path":"crates/frankenterm-core-audit-types/src/proof_lane.rs","status":" M","affects_proof":true}],"artifact_retrieval_status":"partial","artifact_dir":"${ARTIFACT_DIR}"}
JSON
  cat > "${SYNC_GAP_ARTIFACT}" <<JSON
{"selected_worker":"vmi-sync-gap","sync_duration_ms":1250,"wrapper_exit_code":130,"artifact_retrieval_status":"partial","artifact_dir":"${ARTIFACT_DIR}"}
JSON
  cat > "${SKIPPED_ARTIFACT}" <<JSON
{"high_scale_predicate_met":false,"artifact_retrieval_status":"partial","artifact_dir":"${ARTIFACT_DIR}"}
JSON
}

run_proof_doctor() {
  local step="$1"
  local phase="$2"
  local scope="$3"
  local expected_status="$4"
  local expected_reason="$5"
  local expected_record_status="$6"
  local expected_safe="$7"
  local artifact_path="$8"
  local command_shape="$9"
  local output_json="${ARTIFACT_DIR}/${step}.json"
  local stderr_log="${ARTIFACT_DIR}/${step}.stderr"

  local cmd=(
    "${FT_BINARY}" proof-doctor
    --format json
    --bead "${BEAD_ID}"
    --agent Codex
    --scope "${scope}"
    --phase "${phase}"
    --required-backend rch
    --target-dir "${REMOTE_TARGET_DIR}"
  )

  if [[ -n "${artifact_path}" ]]; then
    cmd+=(--evidence-artifact "${artifact_path}")
  fi
  if [[ "${expected_record_status}" != "not_requested" ]]; then
    cmd+=(--proof-record-output "${PROOF_RECORDS}" --proof-record-redaction-status none-needed)
  fi

  if [[ "${command_shape}" == "local-cargo" ]]; then
    cmd+=(-- cargo test proof_lane)
  else
    cmd+=(-- rch exec -- env "CARGO_TARGET_DIR=${REMOTE_TARGET_DIR}" cargo test proof_lane)
  fi

  record_command "${cmd[@]}"
  set +e
  "${cmd[@]}" > "${output_json}" 2> "${stderr_log}"
  local rc=$?
  set -e
  if [[ "${rc}" -ne 0 ]]; then
    record_result "${step}" "false" "${phase}" "command_failed" "proof_doctor_command_failed" "${stderr_log}" "proof-doctor exited ${rc}"
  fi

  set +e
  jq -e \
    --arg status "${expected_status}" \
    --arg reason "${expected_reason}" \
    --arg phase_json "${phase//-/_}" \
    --arg record_status "${expected_record_status}" \
    --arg expected_safe "${expected_safe}" \
    '
      .verdict.status == $status
      and (.verdict.reason_code // .handoff.reason_code // .verdict.ledger_projection.reason_code) == $reason
      and .verdict.phase == $phase_json
      and (.handoff.comment_markdown // .handoff.bead_comment // .handoff.beads_comment) != null
      and .proof_record.write_status == $record_status
      and (
        $record_status == "not_requested"
        or (.proof_record.safe_to_close_source_bead | tostring) == $expected_safe
      )
    ' "${output_json}" >/dev/null
  local assertion_rc=$?
  set -e
  if [[ "${assertion_rc}" -ne 0 ]]; then
    record_result "${step}" "false" "${phase}" "classification_mismatch" "proof_doctor_verdict_mismatch" "${output_json}" "proof-doctor verdict did not match expected ${expected_status}/${expected_reason}"
  fi

  record_result "${step}" "true" "${phase}" "${expected_status}" "${expected_reason}" "${output_json}" "proof-doctor verdict matched expected fail-closed classification"
}

main() {
  cd "${ROOT_DIR}"
  if ! command -v jq >/dev/null 2>&1; then
    write_bootstrap_failure_summary jq
    cat "${SUMMARY_FILE}" >&2
    exit 1
  fi
  require_cmd jq
  if ! find_ft_binary; then
    record_result "preflight.ft_binary" "false" "preflight" "missing" "missing_prerequisite" "${FT_BINARY:-ft}" "ft binary missing; build through RCH and pass FT_BINARY"
  fi
  record_result "preflight.ft_binary" "true" "preflight" "available" "binary_present" "${FT_BINARY}" "ft binary available"
  write_fixture_artifacts

  run_proof_doctor "preflight_runnable" "preflight" "cargo-test" "runnable" "proof.runnable" "not_requested" "false" "" "rch-cargo"
  run_proof_doctor "observed_pass" "terminal-classified" "cargo-test" "passed" "proof.runnable" "written" "true" "${PASS_ARTIFACT}" "rch-cargo"
  run_proof_doctor "observed_source_fail" "terminal-classified" "cargo-test" "source_blocked" "proof.source.remote_compile_error" "written" "false" "${SOURCE_FAIL_ARTIFACT}" "rch-cargo"
  run_proof_doctor "observed_test_fail" "terminal-classified" "cargo-test" "test_blocked" "proof.test.remote_assertion_failed" "written" "false" "${TEST_FAIL_ARTIFACT}" "rch-cargo"
  run_proof_doctor "observed_infra_blocked" "terminal-classified" "cargo-test" "infra_blocked" "proof.rch.queue_timeout_before_assignment" "written" "false" "${INFRA_BLOCKED_ARTIFACT}" "rch-cargo"
  run_proof_doctor "observed_dirty_tree_blocked" "terminal-classified" "cargo-test" "dirty_tree_blocked" "proof.dirty.unowned_path_overlap" "written" "false" "${DIRTY_BLOCKED_ARTIFACT}" "rch-cargo"
  run_proof_doctor "observed_sync_gap" "terminal-classified" "cargo-test" "inconclusive" "proof.rch.sync_not_proof" "written" "false" "${SYNC_GAP_ARTIFACT}" "rch-cargo"
  run_proof_doctor "observed_local_fallback_invalid" "terminal-classified" "cargo-test" "invalid" "proof.command.local_cargo_invalid" "written" "false" "" "local-cargo"
  run_proof_doctor "observed_skipped_not_proven" "terminal-classified" "high-scale" "skipped_not_proven" "proof.high_scale.predicate_absent" "written" "false" "${SKIPPED_ARTIFACT}" "rch-cargo"

  local records_expected=8
  local records_actual
  records_actual="$(grep -cve '^[[:space:]]*$' "${PROOF_RECORDS}")"
  if [[ "${records_actual}" -ne "${records_expected}" ]]; then
    record_result "proof_records.count" "false" "terminal-classified" "failed" "proof_record_count_mismatch" "${PROOF_RECORDS}" "expected ${records_expected} proof records, found ${records_actual}"
  fi
  set +e
  jq -s -e '
    length == 8
    and (map(.state) | sort == ["INCONCLUSIVE","INCONCLUSIVE","INFRA_BLOCKED_PRE_CARGO","LOCAL_INVALID","PASS","SKIPPED_NOT_PROVEN","SOURCE_COMPILE_FAIL","TEST_FAIL"])
    and (map(select(.state == "PASS" and (.claims_allowed | index("focused_remote_proof_passed")))) | length == 1)
  ' "${PROOF_RECORDS}" >/dev/null
  local ledger_rc=$?
  set -e
  if [[ "${ledger_rc}" -ne 0 ]]; then
    record_result "proof_records.ledger_shape" "false" "terminal-classified" "failed" "proof_records_shape_mismatch" "${PROOF_RECORDS}" "proof-record JSONL did not capture expected states"
  fi
  record_result "proof_records.ledger_shape" "true" "terminal-classified" "passed" "proof_records_validated" "${PROOF_RECORDS}" "proof-record JSONL captured expected states"

  write_summary "passed"
  cat "${SUMMARY_FILE}"
}

main "$@"
