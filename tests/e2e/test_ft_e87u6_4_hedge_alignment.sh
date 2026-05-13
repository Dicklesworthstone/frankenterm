#!/usr/bin/env bash
# E2E: ft-e87u6.4 README/AGENTS hedge alignment regression wrapper.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-e87u6.4"
SCENARIO_ID="hedge_alignment"
RUN_ID="${FT_E87U6_4_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${FT_E87U6_4_ARTIFACT_DIR:-${ROOT_DIR}/tests/e2e/artifacts/goal-line/ft-e87u6/hedge-alignment/${RUN_ID}}"
REMOTE_TARGET_DIR="${FT_E87U6_4_CARGO_TARGET_DIR:-/tmp/ft-e87u6-4-hedge-alignment-${RUN_ID}}"

COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SHELL_LOG="${ARTIFACT_DIR}/shell-static.log"
CARGO_LOG="${ARTIFACT_DIR}/readme-hedge-alignment-rch.log"
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
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_e87u6_4_hedge_alignment" "${ROOT_DIR}"

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
    local reason_code="$3"
    local error_code="$4"
    local artifact_path="$5"
    local message="$6"
    local manifest_category="${7:-}"

    jq -cn \
        --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg surface "README.md+AGENTS.md hedge alignment" \
        --arg step "${step}" \
        --arg outcome "${outcome}" \
        --arg reason_code "${reason_code}" \
        --arg error_code "${error_code}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg artifact_path "${artifact_path#"${ROOT_DIR}/"}" \
        --arg message "${message}" \
        --arg manifest_category "${manifest_category}" \
        '{
          timestamp: $timestamp,
          bead_id: $bead_id,
          scenario_id: $scenario_id,
          surface: $surface,
          step: $step,
          outcome: $outcome,
          reason_code: $reason_code,
          error_code: $error_code,
          correlation_id: $correlation_id,
          artifact_path: $artifact_path,
          message: $message
        } + (if $manifest_category == "" then {} else {manifest_category: $manifest_category} end)' \
        >>"${STRUCTURED_LOG}"
}

record_result() {
    local step="$1"
    local ok="$2"
    local reason_code="$3"
    local error_code="$4"
    local artifact_path="$5"
    local message="$6"
    local manifest_category="${7:-}"

    TOTAL=$((TOTAL + 1))
    if [[ "${ok}" == "true" ]]; then
        PASS=$((PASS + 1))
        emit_event "${step}" "passed" "${reason_code}" "${error_code}" "${artifact_path}" "${message}" "${manifest_category}"
    else
        FAIL=$((FAIL + 1))
        emit_event "${step}" "failed" "${reason_code}" "${error_code}" "${artifact_path}" "${message}" "${manifest_category}"
    fi
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
          status: (if $rch_substrate_blocked then "rch_substrate_blocked" elif $fail_count == 0 then "passed" else "failed" end),
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
            cargo_log: "readme-hedge-alignment-rch.log",
            proof_ledger: "proof-ledger.jsonl"
          }
        }' >"${SUMMARY_FILE}"
}

trap write_summary EXIT

run_shell_static() {
    record_command "bash -n ${BASH_SOURCE[0]}"
    if bash -n "${BASH_SOURCE[0]}" >"${SHELL_LOG}" 2>&1; then
        record_result "hedge.audit" "true" "shell_static_passed" "none" "${SHELL_LOG}" "Harness shell syntax passed." "harness"
    else
        record_result "hedge.audit" "false" "shell_static_failed" "orphan_hedge_harness" "${SHELL_LOG}" "Harness shell syntax failed." "harness"
        return 1
    fi
}

run_rust_alignment_test() {
    record_command "run_rch_cargo_logged env CARGO_TARGET_DIR=${REMOTE_TARGET_DIR} cargo test -p frankenterm-core --test readme_hedge_alignment -- --nocapture"
    set +e
    run_rch_cargo_logged "${CARGO_LOG}" \
        env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" RUST_TEST_THREADS=1 \
        cargo test -p frankenterm-core --test readme_hedge_alignment -- --nocapture
    local rc=$?
    set -e

    if [[ "${rc}" -eq 0 ]]; then
        while IFS= read -r category; do
            record_result "hedge.audit" "true" "manifest_slot_aligned" "none" "${CARGO_LOG}" "Manifest slot aligned with README/AGENTS hedge table." "${category}"
        done < <(jq -r '.slots[].category' "${ROOT_DIR}/docs/attestations/manifest.json" | sort -u)
        return 0
    fi

    if [[ -f "${CARGO_LOG}.rch_meta.json" ]] \
        && jq -e '.timed_out == true or .failure_reason_code == "RCH-REMOTE-MIRROR-MISSING-FILE" or .failure_reason_code == "RCH-REMOTE-STALL" or .wrapper_exit_code == 124' "${CARGO_LOG}.rch_meta.json" >/dev/null 2>&1
    then
        RCH_SUBSTRATE_BLOCKED=true
        record_result "hedge.audit" "false" "rch_substrate_blocked" "orphan_hedge_rch_substrate_blocked" "${CARGO_LOG}" "RCH substrate blocked before a trustworthy hedge-alignment verdict." "unknown"
    else
        record_result "hedge.audit" "false" "rust_test_failed" "orphan_hedge_alignment" "${CARGO_LOG}" "Rust hedge-alignment test failed." "unknown"
    fi
    return "${rc}"
}

run_shell_static
ensure_rch_ready
run_rust_alignment_test || true

if [[ "${FAIL}" -eq 0 ]]; then
    record_result "summary" "true" "hedge_alignment_passed" "none" "${SUMMARY_FILE}" "README/AGENTS hedge alignment passed."
    echo "PASS ${BEAD_ID} ${SCENARIO_ID}: artifacts at ${ARTIFACT_DIR#"${ROOT_DIR}/"}"
    exit 0
fi

record_result "summary" "false" "hedge_alignment_failed" "orphan_hedge_summary" "${SUMMARY_FILE}" "README/AGENTS hedge alignment failed."
echo "FAIL ${BEAD_ID} ${SCENARIO_ID}: ${FAIL} failed row(s)" >&2
exit 1
