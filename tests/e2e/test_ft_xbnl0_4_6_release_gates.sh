#!/usr/bin/env bash
# E2E: validate ft-xbnl0.4.6 release-gate contract and diagnostics.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-xbnl0.4.6"
SCENARIO_ID="release_gates"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}"
mkdir -p "${ARTIFACT_DIR}"

COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
ENV_FILE="${ARTIFACT_DIR}/env.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"

exec > >(tee -a "${STDOUT_FILE}")
exec 2> >(tee -a "${STDERR_FILE}" >&2)

source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
RCH_SKIP_SMOKE_PREFLIGHT=1
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_xbnl0_4_6_release_gates"

PASS=0
FAIL=0
TOTAL=0
REMOTE_TARGET_DIR="/tmp/ft-$(whoami)-target"

record_command() {
    printf '%s\n' "$*" >> "${COMMANDS_FILE}"
}

write_env() {
    {
        printf 'timestamp=%s\n' "$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
        printf 'bead_id=%s\n' "${BEAD_ID}"
        printf 'scenario_id=%s\n' "${SCENARIO_ID}"
        printf 'correlation_id=%s\n' "${CORRELATION_ID}"
        printf 'artifact_dir=%s\n' "${ARTIFACT_DIR}"
        printf 'platform=%s\n' "$(uname -srm)"
        printf 'cwd=%s\n' "${ROOT_DIR}"
        printf 'remote_cargo_target_dir=%s\n' "${REMOTE_TARGET_DIR}"
        printf 'rch_skip_smoke_preflight=%s\n' "${RCH_SKIP_SMOKE_PREFLIGHT}"
    } > "${ENV_FILE}"
}

emit_log() {
    local step="$1"
    local status="$2"
    local duration_ms="$3"
    local message="$4"
    jq -cn \
        --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg surface "release-gates" \
        --arg step "${step}" \
        --arg status "${status}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg backend "rch" \
        --arg platform "$(uname -srm)" \
        --arg artifact_dir "${ARTIFACT_DIR}" \
        --arg redaction "none" \
        --arg message "${message}" \
        --argjson duration_ms "${duration_ms}" \
        '{
          timestamp: $timestamp,
          bead_id: $bead_id,
          scenario_id: $scenario_id,
          surface: $surface,
          step: $step,
          status: $status,
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
    local duration_ms="$3"
    local message="$4"
    TOTAL=$((TOTAL + 1))
    if [[ "${ok}" == "true" ]]; then
        PASS=$((PASS + 1))
        emit_log "${step}" "passed" "${duration_ms}" "${message}"
    else
        FAIL=$((FAIL + 1))
        emit_log "${step}" "failed" "${duration_ms}" "${message}"
    fi
}

run_checked() {
    local step="$1"
    local log_file="$2"
    shift 2
    local start_ns end_ns duration_ms
    start_ns="$(date +%s%N)"
    record_command "$*"
    if "$@" > "${log_file}" 2>&1; then
        end_ns="$(date +%s%N)"
        duration_ms="$(((end_ns - start_ns) / 1000000))"
        record_result "${step}" "true" "${duration_ms}" "${log_file}"
        return 0
    fi
    end_ns="$(date +%s%N)"
    duration_ms="$(((end_ns - start_ns) / 1000000))"
    record_result "${step}" "false" "${duration_ms}" "${log_file}"
    return 1
}

run_rch_step() {
    local step="$1"
    local log_file="$2"
    shift 2
    local start_ns end_ns duration_ms
    start_ns="$(date +%s%N)"
    record_command "rch exec -- $*"
    if run_rch_cargo_logged "${log_file}" "$@"; then
        end_ns="$(date +%s%N)"
        duration_ms="$(((end_ns - start_ns) / 1000000))"
        record_result "${step}" "true" "${duration_ms}" "${log_file}"
        return 0
    fi
    end_ns="$(date +%s%N)"
    duration_ms="$(((end_ns - start_ns) / 1000000))"
    record_result "${step}" "false" "${duration_ms}" "${log_file}"
    return 1
}

echo "=== ${BEAD_ID} release gates ==="
write_env
command -v jq >/dev/null 2>&1
command -v rch >/dev/null 2>&1
command -v rustfmt >/dev/null 2>&1
record_command "ensure_rch_ready (RCH_SKIP_SMOKE_PREFLIGHT=${RCH_SKIP_SMOKE_PREFLIGHT})"
ensure_rch_ready

SOURCE_AUDIT_LOG="${ARTIFACT_DIR}/source_audit.log"
run_checked \
    "source_audit" \
    "${SOURCE_AUDIT_LOG}" \
    bash -lc "
        set -euo pipefail
        rg -n 'ft_xbnl0_4_6_release_gate_' '${ROOT_DIR}/crates/frankenterm-core/src/release_readiness_gates.rs'
        test -f '${ROOT_DIR}/docs/ft-xbnl0-4-6-release-gates.json'
        test -f '${ROOT_DIR}/scripts/check_ft_xbnl0_4_6_release_gates.sh'
    "

FMT_LOG="${ARTIFACT_DIR}/rustfmt_check.log"
run_checked \
    "rustfmt_check" \
    "${FMT_LOG}" \
    rustfmt --edition 2024 --check \
        "${ROOT_DIR}/crates/frankenterm-core/src/release_readiness_gates.rs"

SELF_TEST_LOG="${ARTIFACT_DIR}/validator_self_test.log"
run_checked \
    "validator_self_test" \
    "${SELF_TEST_LOG}" \
    bash "${ROOT_DIR}/scripts/check_ft_xbnl0_4_6_release_gates.sh" \
        --self-test \
        --output "${ARTIFACT_DIR}/release_gate_self_test.json"

LIB_TEST_LOG="${ARTIFACT_DIR}/frankenterm_core_release_gate_tests.log"
if ! run_rch_step \
    "frankenterm_core_release_gate_tests" \
    "${LIB_TEST_LOG}" \
    env CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
        cargo test -p frankenterm-core --lib ft_xbnl0_4_6_release_gate_ -- --nocapture
then
    :
fi
rch_write_meta_json "${LIB_TEST_LOG}"

VALIDATION_LOG="${ARTIFACT_DIR}/release_gate_repo_eval.log"
record_command "bash ${ROOT_DIR}/scripts/check_ft_xbnl0_4_6_release_gates.sh --output ${ARTIFACT_DIR}/release_gate_repo_eval.json"
set +e
start_ns="$(date +%s%N)"
bash "${ROOT_DIR}/scripts/check_ft_xbnl0_4_6_release_gates.sh" \
    --output "${ARTIFACT_DIR}/release_gate_repo_eval.json" > "${VALIDATION_LOG}" 2>&1
repo_eval_rc=$?
set -e
end_ns="$(date +%s%N)"
duration_ms="$(((end_ns - start_ns) / 1000000))"
if [[ ${repo_eval_rc} -eq 0 || ${repo_eval_rc} -eq 1 ]]; then
    record_result "release_gate_repo_eval" "true" "${duration_ms}" "${VALIDATION_LOG}"
else
    record_result "release_gate_repo_eval" "false" "${duration_ms}" "${VALIDATION_LOG}"
fi

jq -cn \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    --arg commands_file "${COMMANDS_FILE}" \
    --arg env_file "${ENV_FILE}" \
    --arg structured_log "${STRUCTURED_LOG}" \
    --arg stdout_file "${STDOUT_FILE}" \
    --arg stderr_file "${STDERR_FILE}" \
    --arg source_audit_log "${SOURCE_AUDIT_LOG}" \
    --arg fmt_log "${FMT_LOG}" \
    --arg self_test_log "${SELF_TEST_LOG}" \
    --arg lib_test_log "${LIB_TEST_LOG}" \
    --arg lib_test_meta "$(rch_log_meta_path "${LIB_TEST_LOG}")" \
    --arg repo_eval_log "${VALIDATION_LOG}" \
    --arg repo_eval_json "${ARTIFACT_DIR}/release_gate_repo_eval.json" \
    --argjson pass_count "${PASS}" \
    --argjson fail_count "${FAIL}" \
    --argjson total_count "${TOTAL}" \
    '{
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      status: (if $fail_count == 0 then "passed" else "failed" end),
      correlation_id: $correlation_id,
      artifact_dir: $artifact_dir,
      pass_count: $pass_count,
      fail_count: $fail_count,
      total_count: $total_count,
      artifacts: {
        commands: $commands_file,
        env: $env_file,
        structured_log: $structured_log,
        stdout: $stdout_file,
        stderr: $stderr_file,
        source_audit: $source_audit_log,
        rustfmt_check: $fmt_log,
        validator_self_test: $self_test_log,
        frankenterm_core_release_gate_tests: $lib_test_log,
        frankenterm_core_release_gate_tests_meta: $lib_test_meta,
        release_gate_repo_eval: $repo_eval_log,
        release_gate_repo_eval_json: $repo_eval_json
      }
    }' > "${SUMMARY_FILE}"

if [[ "${FAIL}" -ne 0 ]]; then
    echo "ft-xbnl0.4.6 release-gate verification FAILED. Summary: ${SUMMARY_FILE}" >&2
    exit 1
fi

echo "ft-xbnl0.4.6 release-gate verification passed. Summary: ${SUMMARY_FILE}"
