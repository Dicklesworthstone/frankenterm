#!/usr/bin/env bash
# E2E: RCH-backed blocker-radar fixture/conformance proof lane.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-9ntud.4"
SCENARIO_ID="blocker_radar_conformance"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}"
mkdir -p "${ARTIFACT_DIR}"

COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.jsonl"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
FIXTURE_STATIC_LOG="${ARTIFACT_DIR}/fixture-static.log"
SHELL_STATIC_LOG="${ARTIFACT_DIR}/shell-static.log"
DOCS_STATIC_LOG="${ARTIFACT_DIR}/docs-static.log"
CONFORMANCE_RCH_LOG="${ARTIFACT_DIR}/blocker-radar-conformance-rch.log"
PROOF_LEDGER_FILE="${ARTIFACT_DIR}/proof-ledger.jsonl"

exec > >(tee -a "${STDOUT_FILE}")
exec 2> >(tee -a "${STDERR_FILE}" >&2)

if [[ "${RCH_REQUIRE_REMOTE:-1}" != "1" ]]; then
    echo "FATAL: RCH_REQUIRE_REMOTE=1 is required; refusing local Cargo proof." >&2
    exit 2
fi

export RCH_REQUIRE_REMOTE=1
export RCH_QUEUE_WHEN_BUSY="${RCH_QUEUE_WHEN_BUSY:-1}"
export RCH_DAEMON_TIMEOUT_MS="${RCH_DAEMON_TIMEOUT_MS:-60000}"
export RCH_DAEMON_RESPONSE_TIMEOUT_SECS="${RCH_DAEMON_RESPONSE_TIMEOUT_SECS:-120}"
export RCH_DAEMON_WAIT_RESPONSE_TIMEOUT_SECS="${RCH_DAEMON_WAIT_RESPONSE_TIMEOUT_SECS:-1200}"
export RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
export RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-3600}"
export RCH_BUILD_SLOTS="${RCH_BUILD_SLOTS:-2}"
export RCH_TEST_SLOTS="${RCH_TEST_SLOTS:-2}"
export RCH_CHECK_SLOTS="${RCH_CHECK_SLOTS:-2}"
export RCH_PROOF_LEDGER_FILE="${PROOF_LEDGER_FILE}"
export RCH_PROOF_LEDGER_BEAD_ID="${BEAD_ID}"
export RCH_PROOF_LEDGER_SCENARIO_ID="${SCENARIO_ID}"
REMOTE_TARGET_DIR="${FT_CARGO_TARGET_DIR:-/tmp/ft-9ntud-4-blocker-radar-conformance-${RUN_ID}}"
export CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_9ntud_4_blocker_radar_conformance"

PASS=0
FAIL=0
TOTAL=0
LOCAL_STATIC_STATUS="not_run"
REMOTE_STATUS="not_run"
RCH_SUBSTRATE_BLOCKED="false"
FAILURE_CLASSIFICATION="not_applicable"

record_command() {
    printf '%s\n' "$*" >>"${COMMANDS_FILE}"
}

emit_log() {
    local step="$1"
    local status="$2"
    local artifact_path="$3"
    local failure_class="${4:-not_applicable}"
    local source_freshness="${5:-fixture_fixed}"
    jq -cn \
        --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg step "${step}" \
        --arg status "${status}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg artifact_path "${artifact_path}" \
        --arg failure_class "${failure_class}" \
        --arg source_freshness "${source_freshness}" \
        '{
          timestamp: $timestamp,
          bead_id: $bead_id,
          scenario_id: $scenario_id,
          surface: "blocker-radar-v1",
          step: $step,
          status: $status,
          correlation_id: $correlation_id,
          artifact_path: $artifact_path,
          source_freshness: $source_freshness,
          failure_classification: $failure_class
        }' >>"${STRUCTURED_LOG}"
}

record_result() {
    local step="$1"
    local ok="$2"
    local artifact_path="$3"
    local failure_class="${4:-not_applicable}"
    TOTAL=$((TOTAL + 1))
    if [[ "${ok}" == "true" ]]; then
        PASS=$((PASS + 1))
        emit_log "${step}" "passed" "${artifact_path}" "${failure_class}"
    else
        FAIL=$((FAIL + 1))
        FAILURE_CLASSIFICATION="${failure_class}"
        emit_log "${step}" "failed" "${artifact_path}" "${failure_class}"
    fi
}

write_summary() {
    local selected_workers remote_cargo_reached remote_rustc_reached test_binary_reached
    selected_workers="$(jq -scr '[.[].runs[]?.selected_worker_id | select(. != null and . != "required;")] | unique' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf '[]')"
    remote_cargo_reached="$(jq -sr 'any(.[].runs[]?; .remote_cargo_reached == true)' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"
    remote_rustc_reached="$(jq -sr 'any(.[].runs[]?; .remote_rustc_reached == true)' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"
    test_binary_reached="$(jq -sr 'any(.[].runs[]?; .test_binary_reached == true)' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"
    if [[ "${FAIL}" -eq 0 && "${REMOTE_STATUS}" == "passed" ]]; then
        FAILURE_CLASSIFICATION="not_applicable"
    elif [[ "${RCH_SUBSTRATE_BLOCKED}" == "true" ]]; then
        FAILURE_CLASSIFICATION="environment_blocked"
    elif [[ "${FAILURE_CLASSIFICATION}" == "not_applicable" ]]; then
        FAILURE_CLASSIFICATION="source_regression"
    fi

    jq -cn \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg artifact_dir "${ARTIFACT_DIR}" \
        --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
        --arg local_static_status "${LOCAL_STATIC_STATUS}" \
        --arg remote_status "${REMOTE_STATUS}" \
        --arg failure_classification "${FAILURE_CLASSIFICATION}" \
        --argjson rch_substrate_blocked "${RCH_SUBSTRATE_BLOCKED}" \
        --argjson pass_count "${PASS}" \
        --argjson fail_count "${FAIL}" \
        --argjson total_count "${TOTAL}" \
        --argjson selected_workers "${selected_workers}" \
        --argjson remote_cargo_reached "${remote_cargo_reached}" \
        --argjson remote_rustc_reached "${remote_rustc_reached}" \
        --argjson test_binary_reached "${test_binary_reached}" \
        '{
          bead_id: $bead_id,
          scenario_id: $scenario_id,
          status: (
            if $rch_substrate_blocked then
              "rch_substrate_blocked"
            elif $fail_count == 0 then
              "passed"
            else
              "failed"
            end
          ),
          correlation_id: $correlation_id,
          artifact_dir: $artifact_dir,
          remote_cargo_target_dir: $remote_target_dir,
          pass_count: $pass_count,
          fail_count: $fail_count,
          total_count: $total_count,
          failure_classification: $failure_classification,
          evidence: {
            local_static: $local_static_status,
            remote_conformance: $remote_status,
            rch_substrate_blocked: $rch_substrate_blocked,
            local_cargo_counted_as_proof: false
          },
          remote: {
            selected_workers: $selected_workers,
            remote_cargo_reached: $remote_cargo_reached,
            remote_rustc_reached: $remote_rustc_reached,
            test_binary_reached: $test_binary_reached
          },
          artifacts: {
            commands: "commands.txt",
            structured_log: "structured.jsonl",
            stdout: "stdout.txt",
            stderr: "stderr.txt",
            fixture_static: "fixture-static.log",
            shell_static: "shell-static.log",
            docs_static: "docs-static.log",
            conformance_rch: "blocker-radar-conformance-rch.log",
            proof_ledger: "proof-ledger.jsonl"
          }
        }' >"${SUMMARY_FILE}"
}

trap write_summary EXIT

run_static_step() {
    local step="$1"
    local log_file="$2"
    shift 2
    record_command "$*"
    set +e
    "$@" >"${log_file}" 2>&1
    local rc=$?
    set -e
    if [[ ${rc} -eq 0 ]]; then
        record_result "${step}" "true" "${log_file}"
    else
        LOCAL_STATIC_STATUS="failed"
        record_result "${step}" "false" "${log_file}" "source_regression"
        return "${rc}"
    fi
}

run_rch_step() {
    local step="$1"
    local log_file="$2"
    shift 2
    record_command "run_rch_cargo_logged $*"
    set +e
    run_rch_cargo_logged "${log_file}" "$@"
    local rc=$?
    set -e
    if [[ ${rc} -eq 0 ]]; then
        REMOTE_STATUS="passed"
        record_result "${step}" "true" "${log_file}"
        return 0
    fi
    REMOTE_STATUS="failed"
    local failure_class="source_regression"
    if [[ -f "${log_file}.rch_meta.json" ]] \
        && jq -e '
          .timed_out == true
          or .failure_reason_code == "RCH-REMOTE-MIRROR-MISSING-FILE"
          or .failure_reason_code == "RCH-REMOTE-STALL"
          or .wrapper_exit_code == 124
        ' "${log_file}.rch_meta.json" >/dev/null 2>&1
    then
        RCH_SUBSTRATE_BLOCKED="true"
        REMOTE_STATUS="rch_substrate_blocked"
        failure_class="environment_blocked"
    fi
    record_result "${step}" "false" "${log_file}" "${failure_class}" "rch_live"
    return "${rc}"
}

echo "=== ${BEAD_ID} blocker-radar conformance ==="
: >"${PROOF_LEDGER_FILE}"
: >"${COMMANDS_FILE}"
: >"${STRUCTURED_LOG}"

run_static_step \
    "fixture-json-valid" \
    "${FIXTURE_STATIC_LOG}" \
    jq -e '.schema_version == 1 and (.cases | length) >= 12 and (.requirements | length) >= 12' \
    "${ROOT_DIR}/crates/frankenterm-core/tests/fixtures/blocker_radar/conformance_cases.json"

run_static_step "e2e-shell-valid" "${SHELL_STATIC_LOG}" bash -n "${BASH_SOURCE[0]}"

run_static_step \
    "docs-conformance-matrix-linked" \
    "${DOCS_STATIC_LOG}" \
    grep -F "blocker_radar/conformance_cases.json" \
    "${ROOT_DIR}/docs/blocker-radar-contract.md"

LOCAL_STATIC_STATUS="passed"

ensure_rch_ready

if run_rch_step \
    "blocker-radar-conformance-rch" \
    "${CONFORMANCE_RCH_LOG}" \
    env CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" RUST_TEST_THREADS=1 \
    cargo test -p frankenterm-core --test blocker_radar_conformance blocker_radar_conformance -- --nocapture
then
    :
fi

echo "summary=${SUMMARY_FILE}"
[[ "${FAIL}" -eq 0 ]]
