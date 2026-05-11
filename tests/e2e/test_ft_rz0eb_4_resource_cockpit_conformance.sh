#!/usr/bin/env bash
# E2E: RCH-backed resource cockpit v1 schema/golden conformance proof lane.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-rz0eb.4"
SCENARIO_ID="resource_cockpit_conformance"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}"
mkdir -p "${ARTIFACT_DIR}"

COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
SCHEMA_STATIC_LOG="${ARTIFACT_DIR}/schema-static.log"
SHELL_STATIC_LOG="${ARTIFACT_DIR}/shell-static.log"
SCHEMA_TEST_LOG="${ARTIFACT_DIR}/schema-golden-rch.log"
RUNTIME_TEST_LOG="${ARTIFACT_DIR}/runtime-telemetry-rch.log"
PROOF_LEDGER_FILE="${ARTIFACT_DIR}/proof-ledger.jsonl"

exec > >(tee -a "${STDOUT_FILE}")
exec 2> >(tee -a "${STDERR_FILE}" >&2)

export RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
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
REMOTE_TARGET_DIR="${FT_CARGO_TARGET_DIR:-/tmp/ft-rz0eb-4-resource-cockpit-conformance-${RUN_ID}}"
export CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_rz0eb_4_resource_cockpit_conformance"

# The shared guard's force-refresh capabilities path can time out under swarm
# contention even when cached daemon capabilities are fresh enough to prove
# Rust-capable workers exist. Keep this lane fail-closed, but avoid making a
# slow refresh a false proof blocker before Cargo starts.
ensure_rch_runtime_capabilities() {
    set +e
    run_rch --json workers capabilities >"${_RCH_CAPABILITIES_LOG}" 2>&1
    local capabilities_rc=$?
    set -e
    rch_write_meta_json "${_RCH_CAPABILITIES_LOG}" "${capabilities_rc}"
    rch_emit_proof_ledger_entry \
        "rch --json workers capabilities" \
        "${_RCH_CAPABILITIES_LOG}" \
        "${capabilities_rc}" \
        "not_applicable" \
        "not_applicable" \
        "read cached daemon runtime capabilities before remote-only cargo proof"

    if [[ "${capabilities_rc}" -ne 0 ]] || ! jq -e . "${_RCH_CAPABILITIES_LOG}" >/dev/null 2>&1; then
        rch_fatal "rch worker capability check failed. See ${_RCH_CAPABILITIES_LOG}"
    fi

    local rust_worker_count
    rust_worker_count="$(jq -r '
        (.data.workers // .workers // [])
        | map(select(.capabilities.rustc_version? != null))
        | length
    ' "${_RCH_CAPABILITIES_LOG}" 2>/dev/null || printf '0')"
    if ! rch_is_unsigned_int "${rust_worker_count}" || [[ "${rust_worker_count}" -eq 0 ]]; then
        rch_fatal "rch worker capability check found no Rust-capable workers. See ${_RCH_CAPABILITIES_LOG}"
    fi
}

PASS=0
FAIL=0
TOTAL=0
LOCAL_STATIC_STATUS="not_run"
REMOTE_REDUCED_STATUS="not_run"
TARGET_HARDWARE_STATUS="skipped_not_proven"
RCH_SUBSTRATE_BLOCKED="false"

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
        --arg step "${step}" \
        --arg status "${status}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg message "${message}" \
        --arg reason_code "${reason_code}" \
        '{
          timestamp: $timestamp,
          bead_id: $bead_id,
          scenario_id: $scenario_id,
          surface: "resource-cockpit-v1",
          step: $step,
          status: $status,
          correlation_id: $correlation_id,
          backend: "rch",
          message: $message
        } + (if $reason_code == "" then {} else {reason_code: $reason_code} end)' >>"${STRUCTURED_LOG}"
}

record_result() {
    local step="$1"
    local ok="$2"
    local message="$3"
    local reason_code="${4:-}"
    TOTAL=$((TOTAL + 1))
    if [[ "${ok}" == "true" ]]; then
        PASS=$((PASS + 1))
        emit_log "${step}" "passed" "${message}" "${reason_code}"
    else
        FAIL=$((FAIL + 1))
        emit_log "${step}" "failed" "${message}" "${reason_code}"
    fi
}

write_summary() {
    local selected_workers remote_cargo_reached remote_rustc_reached test_binary_reached substrate_failure
    selected_workers="$(jq -scr '[.[].runs[]?.selected_worker_id | select(. != null and . != "required;")] | unique' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf '[]')"
    remote_cargo_reached="$(jq -sr 'any(.[].runs[]?; .remote_cargo_reached == true)' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"
    remote_rustc_reached="$(jq -sr 'any(.[].runs[]?; .remote_rustc_reached == true)' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"
    test_binary_reached="$(jq -sr 'any(.[].runs[]?; .test_binary_reached == true)' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"
    substrate_failure="$(jq -se '
        any(.[].runs[]?;
          .source_mirror_status == "missing"
          or .source_mirror_reason_code == "RCH-REMOTE-MIRROR-MISSING-FILE"
          or .fallback_reason_code == "RCH-REMOTE-MIRROR-MISSING-FILE"
          or .fallback_reason_code == "RCH-REMOTE-STALL"
          or .validation_status == "timeout"
        )
    ' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"
    if [[ "${substrate_failure}" == "true" ]]; then
        RCH_SUBSTRATE_BLOCKED="true"
        if [[ "${REMOTE_REDUCED_STATUS}" == "failed" || "${REMOTE_REDUCED_STATUS}" == "not_run" ]]; then
            REMOTE_REDUCED_STATUS="rch_substrate_blocked"
        fi
    fi
    if [[ -f "$(rch_remote_preflight_log_path)" ]] \
        && jq -e '.status == "blocked"' "$(rch_remote_preflight_log_path)" >/dev/null 2>&1
    then
        RCH_SUBSTRATE_BLOCKED="true"
        REMOTE_REDUCED_STATUS="rch_substrate_blocked"
    fi
    if [[ "${REMOTE_REDUCED_STATUS}" == "failed" && "${test_binary_reached}" != "true" ]]; then
        RCH_SUBSTRATE_BLOCKED="true"
        REMOTE_REDUCED_STATUS="rch_substrate_blocked"
    fi
    if jq -se 'any(.[].runs[]?; .is_heavy == true and .exit_status != 0 and .remote_cargo_reached != true)' "${PROOF_LEDGER_FILE}" >/dev/null 2>&1; then
        RCH_SUBSTRATE_BLOCKED="true"
        if [[ "${REMOTE_REDUCED_STATUS}" == "not_run" ]]; then
            REMOTE_REDUCED_STATUS="rch_substrate_blocked"
        fi
    fi
    if [[ "${LOCAL_STATIC_STATUS}" == "passed" && "${REMOTE_REDUCED_STATUS}" == "not_run" ]]; then
        RCH_SUBSTRATE_BLOCKED="true"
        REMOTE_REDUCED_STATUS="rch_substrate_blocked"
    fi
    jq -cn \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg artifact_dir "${ARTIFACT_DIR}" \
        --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
        --arg local_static_status "${LOCAL_STATIC_STATUS}" \
        --arg remote_reduced_status "${REMOTE_REDUCED_STATUS}" \
        --arg target_hardware_status "${TARGET_HARDWARE_STATUS}" \
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
          evidence: {
            local_static: $local_static_status,
            remote_reduced: $remote_reduced_status,
            target_hardware: $target_hardware_status,
            skipped_not_proven: ($target_hardware_status == "skipped_not_proven"),
            rch_substrate_blocked: $rch_substrate_blocked
          },
          remote: {
            selected_workers: $selected_workers,
            remote_cargo_reached: $remote_cargo_reached,
            remote_rustc_reached: $remote_rustc_reached,
            test_binary_reached: $test_binary_reached
          },
          artifacts: {
            commands: "commands.txt",
            structured_log: "structured.log",
            stdout: "stdout.txt",
            stderr: "stderr.txt",
            schema_static: "schema-static.log",
            shell_static: "shell-static.log",
            schema_golden_rch: "schema-golden-rch.log",
            runtime_telemetry_rch: "runtime-telemetry-rch.log",
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
        record_result "${step}" "false" "${log_file}" "resource.proof.failed_static_check"
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
        record_result "${step}" "true" "${log_file}"
        return 0
    fi
    local failure_reason_code="resource.proof.remote_reduced_failed"
    local rch_meta_substrate="false"
    if [[ -f "${log_file}.rch_meta.json" ]] \
        && jq -e '
          .timed_out == true
          or .failure_reason_code == "RCH-REMOTE-MIRROR-MISSING-FILE"
          or .failure_reason_code == "RCH-REMOTE-STALL"
          or .wrapper_exit_code == 124
        ' "${log_file}.rch_meta.json" >/dev/null 2>&1
    then
        rch_meta_substrate="true"
    fi
    if [[ "${rch_meta_substrate}" == "true" ]] \
        || grep -Eq 'could not parse/generate dep info|debug/deps/[^[:space:]]+\.d' "${log_file}" 2>/dev/null
    then
        RCH_SUBSTRATE_BLOCKED="true"
        REMOTE_REDUCED_STATUS="rch_substrate_blocked"
        failure_reason_code="resource.proof.rch_substrate_blocked"
    else
        REMOTE_REDUCED_STATUS="failed"
    fi
    record_result "${step}" "false" "${log_file}" "${failure_reason_code}"
    return "${rc}"
}

echo "=== ${BEAD_ID} resource cockpit conformance ==="
: >"${PROOF_LEDGER_FILE}"
: >"${COMMANDS_FILE}"
: >"${STRUCTURED_LOG}"
printf 'Combined schema_golden + runtime cockpit proof is captured in schema-golden-rch.log.\n' >"${RUNTIME_TEST_LOG}"

run_static_step "schema-json-valid" "${SCHEMA_STATIC_LOG}" jq empty "${ROOT_DIR}/docs/json-schema/ft-resource-pressure-cockpit.json"
run_static_step "e2e-shell-valid" "${SHELL_STATIC_LOG}" bash -n "${BASH_SOURCE[0]}"
LOCAL_STATIC_STATUS="passed"

ensure_rch_ready

if run_rch_step \
    "schema-golden-and-runtime-resource-cockpit" \
    "${SCHEMA_TEST_LOG}" \
    env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" RUST_TEST_THREADS=1 \
    cargo test -p frankenterm-core --lib --test schema_golden cockpit -- --nocapture
then
    :
fi

if [[ "${FAIL}" -eq 0 ]]; then
    REMOTE_REDUCED_STATUS="passed"
fi

echo "summary=${SUMMARY_FILE}"
[[ "${FAIL}" -eq 0 ]]
