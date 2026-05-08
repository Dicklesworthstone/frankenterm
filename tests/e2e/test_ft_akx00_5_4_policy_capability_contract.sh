#!/usr/bin/env bash
# E2E: Validate ft-akx00.5.4 policy capability contract.
#
# Scenarios:
#   1. Policy docs no longer claim PaneCapabilities is a stub.
#   2. Policy docs describe unknown alt-screen behavior accurately.
#   3. Unknown alt-screen regression tests pass via rch-offloaded cargo only.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-akx00.5.4"
SCENARIO_ID="policy_capability_contract"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
HARNESS_NAME="ft_akx00_5_4_policy_capability_contract"
ARTIFACT_DIR="${ROOT_DIR}/artifacts/placeholder-remediation/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}"
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
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "${HARNESS_NAME}" "${ROOT_DIR}"

PASS=0
FAIL=0
TOTAL=0
CARGO_TARGET_DIR="target/rch-e2e-ft-akx00-5-4-${RUN_ID}"
export CARGO_TARGET_DIR

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
        printf 'shell=%s\n' "${SHELL:-unknown}"
        printf 'cwd=%s\n' "${ROOT_DIR}"
        printf 'cargo_target_dir=%s\n' "${CARGO_TARGET_DIR}"
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
        --arg surface "policy" \
        --arg step "${step}" \
        --arg status "${status}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg backend "rch" \
        --arg platform "$(uname -srm)" \
        --arg artifact_dir "${ARTIFACT_DIR}" \
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
        printf 'PASS %s\n' "${step}"
    else
        FAIL=$((FAIL + 1))
        emit_log "${step}" "failed" "${duration_ms}" "${message}"
        printf 'FAIL %s\n' "${step}" >&2
    fi
}

require_cmd() {
    local cmd="$1"
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        emit_log "preflight:${cmd}" "failed" 0 "missing command ${cmd}"
        exit 1
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

echo "=== ft-akx00.5.4 Policy Capability Contract E2E ==="
write_env
require_cmd jq
require_cmd rg
require_cmd rch
record_command "ensure_rch_ready"
ensure_rch_ready

DOC_LOG="${ARTIFACT_DIR}/doc_contract.log"
if run_checked \
    "doc_contract" \
    "${DOC_LOG}" \
    bash -lc "
        set -euo pipefail
        ! rg -n 'Pane Capabilities \\(stub' '${ROOT_DIR}/crates/frankenterm-core/src/policy.rs' >/dev/null
        rg -n 'untrusted actors require approval before injection' '${ROOT_DIR}/crates/frankenterm-core/src/policy.rs'
        rg -n 'unknown state preserved in the decision trace' '${ROOT_DIR}/crates/frankenterm-core/src/policy.rs'
        rg -n 'Unknown alt-screen state requires approval for untrusted actors' '${ROOT_DIR}/crates/frankenterm-core/src/policy.rs'
        rg -n 'trusted actor bypassed approval' '${ROOT_DIR}/crates/frankenterm-core/src/policy.rs'
        ! rg -n 'Unknown alt-screen state also triggers denial' '${ROOT_DIR}/crates/frankenterm-core/src/policy.rs' >/dev/null
    "; then
    :
fi

TEST_LOG="${ARTIFACT_DIR}/unknown_alt_screen_tests.log"
record_command "run_rch_cargo_logged env CARGO_TARGET_DIR=${CARGO_TARGET_DIR} cargo test -p frankenterm-core --lib unknown_alt_screen -- --nocapture"
start_ns="$(date +%s%N)"
if run_rch_cargo_logged \
    "${TEST_LOG}" \
    env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test -p frankenterm-core --lib unknown_alt_screen -- --nocapture; then
    end_ns="$(date +%s%N)"
    record_result "unknown_alt_screen_tests" "true" "$(((end_ns - start_ns) / 1000000))" "${TEST_LOG}"
else
    end_ns="$(date +%s%N)"
    record_result "unknown_alt_screen_tests" "false" "$(((end_ns - start_ns) / 1000000))" "${TEST_LOG}"
fi

TRACE_LOG="${ARTIFACT_DIR}/trace_contract.log"
if run_checked \
    "trace_contract" \
    "${TRACE_LOG}" \
    bash -lc "
        set -euo pipefail
        rg -n 'authorize_allows_human_with_unknown_alt_screen' '${ROOT_DIR}/crates/frankenterm-core/src/policy.rs'
        rg -n 'unknown alt-screen state must not be misreported as inactive' '${ROOT_DIR}/crates/frankenterm-core/src/policy.rs'
        rg -n 'policy.alt_screen_unknown' '${ROOT_DIR}/crates/frankenterm-core/src/policy.rs'
    "; then
    :
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
    --arg doc_log "${DOC_LOG}" \
    --arg test_log "${TEST_LOG}" \
    --arg trace_log "${TRACE_LOG}" \
    --argjson total "${TOTAL}" \
    --argjson passed "${PASS}" \
    --argjson failed "${FAIL}" \
    '{
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      artifact_dir: $artifact_dir,
      status: (if $failed == 0 then "passed" else "failed" end),
      totals: {total: $total, passed: $passed, failed: $failed},
      artifacts: {
        commands: $commands_file,
        env: $env_file,
        structured_log: $structured_log,
        stdout: $stdout_file,
        stderr: $stderr_file,
        doc_contract: $doc_log,
        unknown_alt_screen_tests: $test_log,
        trace_contract: $trace_log
      }
    }' > "${SUMMARY_FILE}"

cat "${SUMMARY_FILE}"
[[ "${FAIL}" -eq 0 ]]
