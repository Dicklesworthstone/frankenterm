#!/usr/bin/env bash
# E2E: ft-e87u6.6 attestation closing-template smoke wrapper.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-e87u6.6"
SCENARIO_ID="template_smoke"
RUN_ID="${FT_E87U6_6_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${FT_E87U6_6_ARTIFACT_DIR:-${ROOT_DIR}/tests/e2e/artifacts/goal-line/ft-e87u6/template_smoke/${RUN_ID}}"
REMOTE_TARGET_DIR="${FT_E87U6_6_CARGO_TARGET_DIR:-/tmp/ft-e87u6-6-template-smoke-${RUN_ID}}"

COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SHELL_LOG="${ARTIFACT_DIR}/shell-static.log"
CARGO_LOG="${ARTIFACT_DIR}/release-template-smoke-rch.log"
PROOF_LEDGER_FILE="${ARTIFACT_DIR}/proof-ledger.jsonl"

mkdir -p "${ARTIFACT_DIR}"
: >"${COMMANDS_FILE}"
: >"${STRUCTURED_LOG}"
: >"${PROOF_LEDGER_FILE}"

exec > >(tee -a "${STDOUT_FILE}")
exec 2> >(tee -a "${STDERR_FILE}" >&2)

export RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
export RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
export RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-2400}"
export RCH_BUILD_SLOTS="${RCH_BUILD_SLOTS:-1}"
export RCH_TEST_SLOTS="${RCH_TEST_SLOTS:-1}"
export RCH_CHECK_SLOTS="${RCH_CHECK_SLOTS:-1}"
export RCH_PROOF_LEDGER_FILE="${PROOF_LEDGER_FILE}"
export RCH_PROOF_LEDGER_BEAD_ID="${BEAD_ID}"
export RCH_PROOF_LEDGER_SCENARIO_ID="${SCENARIO_ID}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_e87u6_6_template_smoke" "${ROOT_DIR}"

PASS=0
FAIL=0
TOTAL=0
RCH_SUBSTRATE_BLOCKED=false

record_command() {
    printf '%s\n' "$*" >>"${COMMANDS_FILE}"
}

emit_event() {
    local step="$1"
    local outcome="$2"
    local error_code="$3"
    local artifact_path="$4"
    local message="$5"

    jq -cn \
        --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg surface "docs/release/attestation-bead-closing-template.md" \
        --arg step "${step}" \
        --arg outcome "${outcome}" \
        --arg error_code "${error_code}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg artifact_path "${artifact_path#"${ROOT_DIR}/"}" \
        --arg message "${message}" \
        '{
          timestamp: $timestamp,
          bead_id: $bead_id,
          scenario_id: $scenario_id,
          surface: $surface,
          step: $step,
          outcome: $outcome,
          error_code: $error_code,
          correlation_id: $correlation_id,
          artifact_path: $artifact_path,
          message: $message
        }' >>"${STRUCTURED_LOG}"
}

record_result() {
    local outcome="$1"
    local error_code="$2"
    local artifact_path="$3"
    local message="$4"

    TOTAL=$((TOTAL + 1))
    if [[ "${outcome}" == "passed" ]]; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
    fi
    emit_event "release_template.parse" "${outcome}" "${error_code}" "${artifact_path}" "${message}"
}

write_summary() {
    local selected_workers remote_cargo_reached remote_rustc_reached test_binary_reached
    selected_workers="$(jq -scr '[.[].runs[]?.selected_worker_id | select(. != null and . != "required;")] | unique' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf '[]')"
    remote_cargo_reached="$(jq -sr 'any(.[].runs[]?; .remote_cargo_reached == true)' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"
    remote_rustc_reached="$(jq -sr 'any(.[].runs[]?; .remote_rustc_reached == true)' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"
    test_binary_reached="$(jq -sr 'any(.[].runs[]?; .test_binary_reached == true)' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"

    jq -cn \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg run_id "${RUN_ID}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg artifact_dir "${ARTIFACT_DIR#"${ROOT_DIR}/"}" \
        --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
        --argjson pass_count "${PASS}" \
        --argjson fail_count "${FAIL}" \
        --argjson total_count "${TOTAL}" \
        --argjson rch_substrate_blocked "${RCH_SUBSTRATE_BLOCKED}" \
        --argjson selected_workers "${selected_workers}" \
        --argjson remote_cargo_reached "${remote_cargo_reached}" \
        --argjson remote_rustc_reached "${remote_rustc_reached}" \
        --argjson test_binary_reached "${test_binary_reached}" \
        '{
          bead_id: $bead_id,
          scenario_id: $scenario_id,
          run_id: $run_id,
          correlation_id: $correlation_id,
          status: (if $rch_substrate_blocked then "rch_substrate_blocked" elif $total_count > 0 and $fail_count == 0 then "passed" else "failed" end),
          artifact_dir: $artifact_dir,
          remote_cargo_target_dir: $remote_target_dir,
          counts: {
            total: $total_count,
            passed: $pass_count,
            failed: $fail_count
          },
          rch_substrate_blocked: $rch_substrate_blocked,
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
            shell_static: "shell-static.log",
            cargo_log: "release-template-smoke-rch.log",
            proof_ledger: "proof-ledger.jsonl"
          }
        }' >"${SUMMARY_FILE}"
}

trap write_summary EXIT

run_shell_static() {
    record_command "bash -n ${BASH_SOURCE[0]}"
    if bash -n "${BASH_SOURCE[0]}" >"${SHELL_LOG}" 2>&1; then
        return 0
    fi

    record_result "failed" "missing_section" "${SHELL_LOG}" "Harness shell syntax failed."
    return 1
}

run_rch_preflight() {
    set +e
    (
        ensure_rch_ready
    )
    local rc=$?
    set -e

    if [[ "${rc}" -eq 0 ]]; then
        return 0
    fi

    RCH_SUBSTRATE_BLOCKED=true
    record_result "failed" "missing_section" "${ARTIFACT_DIR}" "RCH preflight blocked before a trustworthy template-smoke verdict."
    return "${rc}"
}

run_rust_template_test() {
    record_command "run_rch_cargo_logged env CARGO_TARGET_DIR=${REMOTE_TARGET_DIR} cargo test -p frankenterm-core --test release_template_smoke --no-default-features -- --nocapture"
    set +e
    (
        run_rch_cargo_logged "${CARGO_LOG}" \
            env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" RUST_TEST_THREADS=1 \
            cargo test -p frankenterm-core --test release_template_smoke --no-default-features -- --nocapture
    )
    local rc=$?
    set -e

    if [[ "${rc}" -eq 0 ]]; then
        record_result "passed" "none" "${CARGO_LOG}" "Attestation closing template contains required sections and bead refs."
        return 0
    fi

    if [[ -f "${CARGO_LOG}.rch_meta.json" ]] \
        && jq -e '.timed_out == true or .fail_open_detected == true or .failure_reason_code == "RCH-REMOTE-MIRROR-MISSING-FILE" or .failure_reason_code == "RCH-REMOTE-STALL" or .wrapper_exit_code == 124' "${CARGO_LOG}.rch_meta.json" >/dev/null 2>&1
    then
        RCH_SUBSTRATE_BLOCKED=true
        record_result "failed" "missing_section" "${CARGO_LOG}" "RCH substrate blocked before a trustworthy template-smoke verdict."
    elif grep -E "ft-187kv|ft-e87u6\\.4|ft-e87u6\\.5" "${CARGO_LOG}" >/dev/null 2>&1; then
        record_result "failed" "missing_bead_ref" "${CARGO_LOG}" "Template-smoke test reported a missing bead reference."
    else
        record_result "failed" "missing_section" "${CARGO_LOG}" "Template-smoke test reported a missing placeholder section."
    fi
    return "${rc}"
}

run_shell_static
if run_rch_preflight; then
    run_rust_template_test || true
fi

if [[ "${FAIL}" -eq 0 ]]; then
    echo "PASS ${BEAD_ID} ${SCENARIO_ID}: artifacts at ${ARTIFACT_DIR#"${ROOT_DIR}/"}"
    exit 0
fi

echo "FAIL ${BEAD_ID} ${SCENARIO_ID}: ${FAIL} failed row(s)" >&2
exit 1
