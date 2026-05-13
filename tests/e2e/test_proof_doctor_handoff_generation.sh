#!/usr/bin/env bash
# E2E: fail-closed proof-doctor handoff generation wrapper.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-782hw.4"
SCENARIO_ID="proof_doctor_handoff_generation"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
FIXTURE_DIR="${ARTIFACT_DIR}/fixtures"
PROOF_RECORDS_FILE="${ARTIFACT_DIR}/proof-records.jsonl"
REMOTE_TARGET_DIR="${FT_CARGO_TARGET_DIR:-/tmp/ft-782hw-4-proof-doctor-handoff-${RUN_ID}}"

mkdir -p "${ARTIFACT_DIR}" "${FIXTURE_DIR}"
: >"${STRUCTURED_LOG}"
: >"${COMMANDS_FILE}"
: >"${PROOF_RECORDS_FILE}"

exec > >(tee -a "${STDOUT_FILE}")
exec 2> >(tee -a "${STDERR_FILE}" >&2)

FT_BIN="${FT_BIN:-$(command -v ft || true)}"

TOTAL=0
PASS=0
FAIL=0

proof_command=(
  rch exec --
  env CARGO_INCREMENTAL=0 "CARGO_TARGET_DIR=${REMOTE_TARGET_DIR}"
  cargo test -p frankenterm-core-caut-types --lib proof_doctor_handoff_fixture -- --nocapture
)

emit_log() {
  local step="$1"
  local status="$2"
  local reason_code="$3"
  local verdict_status="$4"
  local proof_record_status="$5"
  local artifact_path="$6"
  local message="$7"
  jq -cn \
    --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg step "${step}" \
    --arg status "${status}" \
    --arg reason_code "${reason_code}" \
    --arg verdict_status "${verdict_status}" \
    --arg proof_record_status "${proof_record_status}" \
    --arg artifact_path "${artifact_path}" \
    --arg message "${message}" \
    '{
      timestamp: $timestamp,
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      surface: "proof-doctor-handoff",
      step: $step,
      status: $status,
      reason_code: $reason_code,
      verdict_status: $verdict_status,
      proof_record_status: $proof_record_status,
      artifact_path: $artifact_path,
      correlation_id: $correlation_id,
      backend: "local_light_wrapper_for_retained_artifacts",
      required_proof_backend: "rch",
      message: $message
    }' >>"${STRUCTURED_LOG}"
}

record_result() {
  local step="$1"
  local ok="$2"
  local reason_code="$3"
  local verdict_status="$4"
  local proof_record_status="$5"
  local artifact_path="$6"
  local message="$7"
  TOTAL=$((TOTAL + 1))
  if [[ "${ok}" == "true" ]]; then
    PASS=$((PASS + 1))
    emit_log "${step}" "passed" "${reason_code}" "${verdict_status}" "${proof_record_status}" "${artifact_path}" "${message}"
  else
    FAIL=$((FAIL + 1))
    emit_log "${step}" "failed" "${reason_code}" "${verdict_status}" "${proof_record_status}" "${artifact_path}" "${message}"
  fi
}

write_summary() {
  local modes
  modes="$(jq -sc '.' "${STRUCTURED_LOG}" 2>/dev/null || printf '[]')"
  jq -n \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    --arg structured_log "${STRUCTURED_LOG}" \
    --arg commands "${COMMANDS_FILE}" \
    --arg proof_records "${PROOF_RECORDS_FILE}" \
    --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
    --argjson total "${TOTAL}" \
    --argjson passed "${PASS}" \
    --argjson failed "${FAIL}" \
    --argjson modes "${modes}" \
    '{
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      run_id: $run_id,
      correlation_id: $correlation_id,
      status: (if $failed == 0 then "passed" else "failed" end),
      artifact_dir: $artifact_dir,
      remote_cargo_target_dir: $remote_target_dir,
      counts: { total: $total, passed: $passed, failed: $failed },
      artifacts: {
        structured_log: $structured_log,
        commands: $commands,
        proof_records: $proof_records,
        stdout: "stdout.txt",
        stderr: "stderr.txt"
      },
      modes: $modes
    }' >"${SUMMARY_FILE}"
}

trap write_summary EXIT

require_cmd() {
  local cmd="$1"
  if command -v "${cmd}" >/dev/null 2>&1; then
    record_result "preflight.${cmd}" "true" "command_present" "not_applicable" "not_requested" "${cmd}" "${cmd} available"
    return
  fi
  record_result "preflight.${cmd}" "false" "missing_prerequisite" "not_applicable" "not_requested" "${cmd}" "${cmd} missing"
  exit 1
}

write_fixture() {
  local name="$1"
  local path="${FIXTURE_DIR}/${name}.json"
  case "${name}" in
    observed-pass)
      jq -n \
        --arg artifact_dir "${ARTIFACT_DIR}" \
        '{
          status: "passed",
          artifact_dir: $artifact_dir,
          remote: {
            selected_workers: ["fixture-rch-worker"],
            remote_cargo_reached: true,
            remote_rustc_reached: true,
            test_binary_reached: true
          },
          evidence: { local_cargo_counted_as_proof: false },
          artifacts: { stdout: "fixture-observed-pass.log" }
        }' >"${path}"
      ;;
    source-failure)
      jq -n \
        --arg artifact_dir "${ARTIFACT_DIR}" \
        '{
          status: "failed",
          failure_classification: "source_compile_error",
          diagnostic_summary: "fixture remote rustc compile error",
          diagnostic_paths: ["crates/frankenterm/src/main.rs"],
          artifact_dir: $artifact_dir,
          remote: {
            selected_workers: ["fixture-rch-worker"],
            remote_cargo_reached: true,
            remote_rustc_reached: true,
            test_binary_reached: false
          },
          artifacts: { stderr: "fixture-source-failure.log" }
        }' >"${path}"
      ;;
    test-failure)
      jq -n \
        --arg artifact_dir "${ARTIFACT_DIR}" \
        '{
          status: "failed",
          failure_classification: "test_assertion_failed",
          diagnostic_summary: "fixture remote assertion failed",
          diagnostic_paths: ["crates/frankenterm/src/main.rs"],
          artifact_dir: $artifact_dir,
          remote: {
            selected_workers: ["fixture-rch-worker"],
            remote_cargo_reached: true,
            remote_rustc_reached: true,
            test_binary_reached: true
          },
          artifacts: { stderr: "fixture-test-failure.log" }
        }' >"${path}"
      ;;
    infra-blocked)
      jq -n \
        --arg artifact_dir "${ARTIFACT_DIR}" \
        '{
          status: "rch_substrate_blocked",
          failure_classification: "rch_substrate_blocked",
          diagnostic_summary: "fixture RCH substrate blocked before remote Cargo",
          artifact_dir: $artifact_dir,
          remote: {
            selected_workers: [],
            remote_cargo_reached: false,
            remote_rustc_reached: false,
            test_binary_reached: false
          },
          artifacts: { stderr: "fixture-infra-blocked.log" }
        }' >"${path}"
      ;;
    skipped-not-proven)
      jq -n \
        --arg artifact_dir "${ARTIFACT_DIR}" \
        '{
          high_scale_predicate_met: true,
          artifact_dir: $artifact_dir,
          scale_lab_artifact: {
            required: true,
            artifact_path: "fixtures/skipped-not-proven.json",
            schema_version: "ft.scale_lab.staged_proof.v1",
            artifact_stale: false,
            artifact_malformed: false,
            release_claim_status: "skipped_not_proven",
            required_release_claim_status: "real-hardware-proven",
            manifest_status: "skipped_not_proven",
            evidence_mode: "synthetic_smoke",
            live_mux_available: false,
            pane_scales: [50, 200, 500],
            required_pane_scales: [50, 200, 500],
            max_requested_logical_cores: 8,
            min_required_logical_cores: 64,
            max_requested_memory_bytes: 34359738368,
            min_required_memory_bytes: 274877906944
          },
          artifacts: { scale_lab: "fixtures/skipped-not-proven.json" }
        }' >"${path}"
      ;;
    *)
      echo "unknown fixture: ${name}" >&2
      exit 1
      ;;
  esac
  printf '%s\n' "${path}"
}

run_proof_doctor() {
  local step="$1"
  local expected_status="$2"
  shift 2
  local output="${ARTIFACT_DIR}/${step}.json"
  local comment="${ARTIFACT_DIR}/${step}.beads-comment.md"
  local command=("${FT_BIN}" proof-doctor -f json --bead "${BEAD_ID}" --agent Codex --scope cargo-test --required-backend rch --target-dir "${REMOTE_TARGET_DIR}" "$@" --proof-record-output "${PROOF_RECORDS_FILE}" -- "${proof_command[@]}")

  printf '%q ' "${command[@]}" >>"${COMMANDS_FILE}"
  printf '\n' >>"${COMMANDS_FILE}"

  set +e
  "${command[@]}" >"${output}" 2>"${output}.stderr"
  local rc=$?
  set -e
  if [[ "${rc}" -ne 0 ]]; then
    record_result "${step}" "false" "proof_doctor_cli_failed" "cli_exit_${rc}" "not_written" "${output}.stderr" "proof-doctor exited ${rc}"
    return "${rc}"
  fi

  if ! jq empty "${output}" >/dev/null 2>&1; then
    record_result "${step}" "false" "proof_doctor_invalid_json" "invalid_json" "not_written" "${output}" "proof-doctor output was not valid JSON"
    return 1
  fi

  local verdict_status reason_code proof_record_status safe_to_close
  verdict_status="$(jq -r '.verdict.status // "missing"' "${output}")"
  reason_code="$(jq -r '.handoff.reason_code // .verdict.blockers[0].reason_code // "proof.no_blocker"' "${output}")"
  proof_record_status="$(jq -r '.proof_record.write_status // "missing"' "${output}")"
  safe_to_close="$(jq -r '.handoff.safe_to_close // false' "${output}")"
  jq -r '.handoff.beads_comment // empty' "${output}" >"${comment}"

  local ok="true"
  local message="verdict=${verdict_status}; reason=${reason_code}; proof_record=${proof_record_status}; safe_to_close=${safe_to_close}"
  if [[ "${expected_status}" != "*" && "${verdict_status}" != "${expected_status}" ]]; then
    ok="false"
    message="expected ${expected_status}; ${message}"
  fi
  if [[ ! -s "${comment}" ]]; then
    ok="false"
    reason_code="proof_handoff_comment_missing"
    message="${message}; handoff comment missing"
  fi
  if [[ "${proof_record_status}" != "written" && "${proof_record_status}" != "refused" ]]; then
    ok="false"
    reason_code="proof_record_write_decision_missing"
    message="${message}; proof-record write decision missing"
  fi

  record_result "${step}" "${ok}" "${reason_code}" "${verdict_status}" "${proof_record_status}" "${output}" "${message}"
  [[ "${ok}" == "true" ]]
}

cd "${ROOT_DIR}"

require_cmd bash
require_cmd jq
require_cmd git
if [[ -z "${FT_BIN}" || ! -x "${FT_BIN}" ]]; then
  record_result "preflight.ft" "false" "missing_prerequisite" "not_applicable" "not_requested" "ft" "ft binary missing; set FT_BIN to a built FrankenTerm binary"
  exit 1
fi
record_result "preflight.ft" "true" "command_present" "not_applicable" "not_requested" "${FT_BIN}" "ft binary available"
if "${FT_BIN}" proof-doctor --help >"${ARTIFACT_DIR}/ft-proof-doctor-help.txt" 2>&1; then
  record_result "preflight.proof_doctor_surface" "true" "command_present" "not_applicable" "not_requested" "${ARTIFACT_DIR}/ft-proof-doctor-help.txt" "ft proof-doctor available"
else
  record_result "preflight.proof_doctor_surface" "false" "missing_prerequisite" "not_applicable" "not_requested" "${ARTIFACT_DIR}/ft-proof-doctor-help.txt" "ft binary does not expose proof-doctor; set FT_BIN to a current build"
  exit 1
fi

observed_pass_fixture="$(write_fixture observed-pass)"
source_failure_fixture="$(write_fixture source-failure)"
test_failure_fixture="$(write_fixture test-failure)"
infra_blocked_fixture="$(write_fixture infra-blocked)"
skipped_fixture="$(write_fixture skipped-not-proven)"

run_proof_doctor "preflight" "runnable"
run_proof_doctor "observed_artifact_pass" "passed" --evidence-artifact "${observed_pass_fixture}"
run_proof_doctor "observed_source_failure" "source_blocked" --evidence-artifact "${source_failure_fixture}"
run_proof_doctor "observed_test_failure" "test_blocked" --evidence-artifact "${test_failure_fixture}"
run_proof_doctor "observed_infra_blocker" "infra_blocked" --evidence-artifact "${infra_blocked_fixture}"
run_proof_doctor "observed_skipped_not_proven" "skipped_not_proven" --evidence-artifact "${skipped_fixture}"

# This probe is intentionally tolerant: in a clean checkout it should be
# runnable; in a shared dirty checkout it must classify as dirty_tree_blocked.
dirty_command=(
  rch exec --
  env CARGO_INCREMENTAL=0 "CARGO_TARGET_DIR=${REMOTE_TARGET_DIR}"
  cargo test -p frankenterm-core --lib swarm_scheduler_dirty_probe -- --nocapture
)
proof_command=("${dirty_command[@]}")
run_proof_doctor "dirty_tree_probe" "*"

echo "summary=${SUMMARY_FILE}"
[[ "${FAIL}" -eq 0 ]]
