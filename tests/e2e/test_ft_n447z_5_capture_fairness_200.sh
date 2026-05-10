#!/usr/bin/env bash
# E2E: RCH-backed 200-pane capture fairness proof lane.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-n447z.5"
SCENARIO_ID="capture_fairness_200_pane_reduced"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}"
RUST_ARTIFACT_DIR_REL="tests/e2e/artifacts/goal-line/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}/rust"
RUST_ARTIFACT_DIR="${ROOT_DIR}/${RUST_ARTIFACT_DIR_REL}"
COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
ENV_FILE="${ARTIFACT_DIR}/env.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
RCH_LOG="${ARTIFACT_DIR}/capture_fairness_200_pane_rch.log"
RUST_SUMMARY_FILE="${RUST_ARTIFACT_DIR}/capture_fairness_200_pane_summary.json"
PROOF_LEDGER_FILE="${ARTIFACT_DIR}/proof-ledger.jsonl"
PROOF_LEDGER_VALIDATION_DIR=""
REMOTE_TARGET_DIR="${FT_N447Z5_CARGO_TARGET_DIR:-${CARGO_TARGET_DIR:-target/rch-ft-n447z-5-capture-fairness-200/${RUN_ID}}}"

mkdir -p "${ARTIFACT_DIR}" "${RUST_ARTIFACT_DIR}"

exec > >(tee -a "${STDOUT_FILE}")
exec 2> >(tee -a "${STDERR_FILE}" >&2)

export RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
export RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
export RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-3600}"
export RCH_ENV_ALLOWLIST="${RCH_ENV_ALLOWLIST:-FT_N447Z5_ARTIFACT_DIR,CARGO_TARGET_DIR,CARGO_BUILD_JOBS,CARGO_NET_GIT_FETCH_WITH_CLI}"
export RCH_PROOF_LEDGER_FILE="${PROOF_LEDGER_FILE}"
export RCH_PROOF_LEDGER_BEAD_ID="${BEAD_ID}"
export RCH_PROOF_LEDGER_SCENARIO_ID="${SCENARIO_ID}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_n447z_5_capture_fairness_200" "${ROOT_DIR}"

FINAL_STATUS="failed"
FINAL_CLASSIFICATION="environment"
FINAL_REASON="harness_interrupted_before_summary"

record_command() {
    printf '%s\n' "$*" >>"${COMMANDS_FILE}"
}

emit_log() {
    local step="$1"
    local status="$2"
    local message="$3"
    local reason_code="${4:-}"
    jq -cn \
        --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg surface "capture-fairness-200-pane-proof" \
        --arg step "${step}" \
        --arg status "${status}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg backend "rch" \
        --arg platform "$(uname -srm)" \
        --arg artifact_dir "${ARTIFACT_DIR}" \
        --arg message "${message}" \
        --arg reason_code "${reason_code}" \
        '{
          timestamp: $timestamp,
          bead_id: $bead_id,
          scenario_id: $scenario_id,
          surface: $surface,
          step: $step,
          status: $status,
          correlation_id: $correlation_id,
          backend: $backend,
          platform: $platform,
          artifact_dir: $artifact_dir,
          message: $message
        } + (if $reason_code == "" then {} else {reason_code: $reason_code} end)' >>"${STRUCTURED_LOG}"
}

classify_failure() {
    local meta_file preflight_status preflight_reason fail_open timed_out remote_cargo test_binary
    meta_file="$(rch_log_meta_path "${RCH_LOG}")"
    preflight_status="$(jq -r '.status // ""' "$(rch_remote_preflight_log_path)" 2>/dev/null || true)"
    preflight_reason="$(jq -r '.reason_code // ""' "$(rch_remote_preflight_log_path)" 2>/dev/null || true)"

    if [[ "${preflight_status}" == "blocked" ]]; then
        FINAL_CLASSIFICATION="rch_substrate"
        FINAL_REASON="${preflight_reason:-remote_preflight_blocked}"
        return 0
    fi

    fail_open="$(jq -r '.fail_open_detected // false' "${meta_file}" 2>/dev/null || printf 'false')"
    timed_out="$(jq -r '.timed_out // false' "${meta_file}" 2>/dev/null || printf 'false')"
    remote_cargo="$(jq -r '.remote_cargo_reached // false' "${meta_file}" 2>/dev/null || printf 'false')"
    test_binary="$(jq -r '.test_binary_reached // false' "${meta_file}" 2>/dev/null || printf 'false')"
    if [[ "${fail_open}" == "true" || "${timed_out}" == "true" || "${remote_cargo}" != "true" ]]; then
        FINAL_CLASSIFICATION="rch_substrate"
        FINAL_REASON="$(jq -r '.failure_reason_code // "rch_substrate_blocked"' "${meta_file}" 2>/dev/null || printf 'rch_substrate_blocked')"
    elif [[ "${test_binary}" != "true" ]]; then
        FINAL_CLASSIFICATION="environment"
        FINAL_REASON="test_binary_not_reached"
    else
        FINAL_CLASSIFICATION="source_or_test"
        FINAL_REASON="rust_test_failed"
    fi
}

write_summary() {
    local rust_status="missing"
    local proof_ledger_status="missing"
    if [[ -f "${RUST_SUMMARY_FILE}" ]]; then
        rust_status="$(jq -r '.pass_fail.status // "unknown"' "${RUST_SUMMARY_FILE}" 2>/dev/null || printf 'unknown')"
    fi
    if [[ -n "${PROOF_LEDGER_VALIDATION_DIR}" ]]; then
        proof_ledger_status="validated"
    elif [[ -s "${PROOF_LEDGER_FILE}" ]]; then
        proof_ledger_status="present_unvalidated"
    fi

    jq -cn \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg status "${FINAL_STATUS}" \
        --arg failure_classification "${FINAL_CLASSIFICATION}" \
        --arg reason_code "${FINAL_REASON}" \
        --arg artifact_dir "${ARTIFACT_DIR}" \
        --arg rust_summary "${RUST_SUMMARY_FILE}" \
        --arg rust_summary_status "${rust_status}" \
        --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
        --arg proof_ledger "${PROOF_LEDGER_FILE}" \
        --arg proof_ledger_validation_dir "${PROOF_LEDGER_VALIDATION_DIR}" \
        --arg proof_ledger_status "${proof_ledger_status}" \
        --arg commands "${COMMANDS_FILE}" \
        --arg env "${ENV_FILE}" \
        --arg structured_log "${STRUCTURED_LOG}" \
        --arg stdout "${STDOUT_FILE}" \
        --arg stderr "${STDERR_FILE}" \
        --arg rch_log "${RCH_LOG}" \
        --arg rch_meta "$(rch_log_meta_path "${RCH_LOG}")" \
        --arg rch_probe "$(rch_probe_log_path)" \
        --arg rch_queue "$(rch_queue_log_path)" \
        --arg rch_preflight "$(rch_remote_preflight_log_path)" \
        '{
          bead_id: $bead_id,
          scenario_id: $scenario_id,
          correlation_id: $correlation_id,
          status: $status,
          proof_interpretation: {
            evidence_level: "remote_reduced",
            target_class_hardware: "skipped_not_proven",
            live_gui_dependency: false
          },
          failure_classification: $failure_classification,
          reason_code: $reason_code,
          artifact_dir: $artifact_dir,
          rust_summary_status: $rust_summary_status,
          remote_target_dir: $remote_target_dir,
          proof_ledger_status: $proof_ledger_status,
          artifacts: {
            rust_summary: $rust_summary,
            proof_ledger: $proof_ledger,
            proof_ledger_validation_dir: $proof_ledger_validation_dir,
            commands: $commands,
            env: $env,
            structured_log: $structured_log,
            stdout: $stdout,
            stderr: $stderr,
            rch_log: $rch_log,
            rch_meta: $rch_meta,
            rch_probe: $rch_probe,
            rch_queue: $rch_queue,
            rch_preflight: $rch_preflight
          }
        }' >"${SUMMARY_FILE}"
}

finish_summary() {
    if [[ ! -f "${SUMMARY_FILE}" ]]; then
        write_summary
    fi
}
trap finish_summary EXIT

{
    printf 'timestamp=%s\n' "$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    printf 'bead_id=%s\n' "${BEAD_ID}"
    printf 'scenario_id=%s\n' "${SCENARIO_ID}"
    printf 'correlation_id=%s\n' "${CORRELATION_ID}"
    printf 'artifact_dir=%s\n' "${ARTIFACT_DIR}"
    printf 'rust_artifact_dir_rel=%s\n' "${RUST_ARTIFACT_DIR_REL}"
    printf 'remote_target_dir=%s\n' "${REMOTE_TARGET_DIR}"
    printf 'rch_require_remote=%s\n' "${RCH_REQUIRE_REMOTE}"
    printf 'rch_step_timeout_secs=%s\n' "${RCH_STEP_TIMEOUT_SECS}"
} >"${ENV_FILE}"

echo "=== ${BEAD_ID} 200-pane capture fairness proof ==="
echo "Artifacts: ${ARTIFACT_DIR}"
: >"${PROOF_LEDGER_FILE}"

emit_log "preflight.rch" "started" "checking remote-only RCH readiness"
ensure_rch_ready
emit_log "preflight.rch" "passed" "RCH remote-only preflight accepted"

record_command "FT_N447Z5_ARTIFACT_DIR=${RUST_ARTIFACT_DIR_REL} CARGO_TARGET_DIR=${REMOTE_TARGET_DIR} CARGO_BUILD_JOBS=1 CARGO_NET_GIT_FETCH_WITH_CLI=true cargo test -p frankenterm-core --lib tailer_scheduler_slo_200_pane_reduced_fairness_proof_artifact -- --nocapture"

set +e
run_rch_cargo_logged "${RCH_LOG}" \
    env \
    FT_N447Z5_ARTIFACT_DIR="${RUST_ARTIFACT_DIR_REL}" \
    CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
    CARGO_BUILD_JOBS=1 \
    CARGO_NET_GIT_FETCH_WITH_CLI=true \
    cargo test -p frankenterm-core --lib tailer_scheduler_slo_200_pane_reduced_fairness_proof_artifact -- --nocapture
RCH_RC=$?
set -e

if [[ "${RCH_RC}" -ne 0 ]]; then
    classify_failure
    emit_log "proof.rch" "failed" "${RCH_LOG}" "${FINAL_REASON}"
    write_summary
    exit "${RCH_RC}"
fi

if [[ ! -f "${RUST_SUMMARY_FILE}" ]]; then
    FINAL_CLASSIFICATION="source_or_test"
    FINAL_REASON="missing_rust_summary_artifact"
    emit_log "proof.artifact" "failed" "${RUST_SUMMARY_FILE}" "${FINAL_REASON}"
    write_summary
    exit 1
fi

if ! jq -e '
  .pass_fail.status == "passed" and
  .inputs.total_panes == 200 and
  .proof_interpretation.target_class_hardware == "skipped_not_proven" and
  .pass_fail.every_pane_serviced == true and
  .pass_fail.snapshot_rows_untruncated == true and
  .pass_fail.timeout_events == 0 and
  .pass_fail.backpressure_events == 0
' "${RUST_SUMMARY_FILE}" >/dev/null; then
    FINAL_CLASSIFICATION="source_or_test"
    FINAL_REASON="rust_summary_assertions_failed"
    emit_log "proof.artifact" "failed" "${RUST_SUMMARY_FILE}" "${FINAL_REASON}"
    write_summary
    exit 1
fi

if [[ ! -s "${PROOF_LEDGER_FILE}" ]]; then
    FINAL_CLASSIFICATION="rch_substrate"
    FINAL_REASON="missing_proof_ledger"
    emit_log "proof.ledger" "failed" "${PROOF_LEDGER_FILE}" "${FINAL_REASON}"
    write_summary
    exit 1
fi

PROOF_LEDGER_VALIDATION_DIR="$(rch_validate_proof_ledger_file "${PROOF_LEDGER_FILE}")"

FINAL_STATUS="passed"
FINAL_CLASSIFICATION="none"
FINAL_REASON="assertions_satisfied"
emit_log "proof.rch" "passed" "${RUST_SUMMARY_FILE}" "${FINAL_REASON}"
write_summary

echo "Summary: ${SUMMARY_FILE}"
