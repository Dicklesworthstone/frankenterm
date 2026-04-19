#!/usr/bin/env bash
# E2E: validate the wa-2toy3 MCP feature slice runs on native asupersync bootstrap.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="wa-2toy3"
SCENARIO_ID="mcp_feature_native_runtime"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/feature-gates/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}"
mkdir -p "${ARTIFACT_DIR}"

COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
ENV_FILE="${ARTIFACT_DIR}/env.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
REMOTE_TARGET_DIR="/tmp/ft-cod2-target"

exec > >(tee -a "${STDOUT_FILE}")
exec 2> >(tee -a "${STDERR_FILE}" >&2)

source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
RCH_SKIP_SMOKE_PREFLIGHT=1
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "wa_2toy3_mcp_feature_native_runtime"

PASS=0
FAIL=0
TOTAL=0

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
    --arg surface "mcp-feature-native-runtime" \
    --arg step "${step}" \
    --arg status "${status}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg backend "rch-fork-bypass" \
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

run_fork_bypass_rch_step() {
  local step="$1"
  local log_file="$2"
  shift 2
  local start_ns end_ns duration_ms rc
  start_ns="$(date +%s%N)"
  record_command "python3 <fork-bypass> $*"
  set +e
  python3 - "${ROOT_DIR}" "${log_file}" "$@" <<'PY'
import os
import subprocess
import sys

root_dir = sys.argv[1]
log_file = sys.argv[2]
cmd = sys.argv[3:]

with open(log_file, "w", encoding="utf-8") as fh:
    proc = subprocess.run(
        cmd,
        cwd=root_dir,
        env=os.environ.copy(),
        stdout=fh,
        stderr=subprocess.STDOUT,
        text=True,
    )

sys.exit(proc.returncode)
PY
  rc=$?
  set -e
  check_rch_fallback "${log_file}"
  rch_write_meta_json "${log_file}" "${rc}"
  end_ns="$(date +%s%N)"
  duration_ms="$(((end_ns - start_ns) / 1000000))"
  if [[ ${rc} -eq 0 ]]; then
    record_result "${step}" "true" "${duration_ms}" "${log_file}"
    return 0
  fi
  record_result "${step}" "false" "${duration_ms}" "${log_file}"
  return "${rc}"
}

echo "=== ${BEAD_ID} MCP feature native runtime ==="
write_env
command -v jq >/dev/null 2>&1
command -v python3 >/dev/null 2>&1
command -v rch >/dev/null 2>&1
record_command "ensure_rch_ready (RCH_SKIP_SMOKE_PREFLIGHT=${RCH_SKIP_SMOKE_PREFLIGHT})"
ensure_rch_ready

SOURCE_AUDIT_LOG="${ARTIFACT_DIR}/source_audit.log"
run_checked \
  "source_audit" \
  "${SOURCE_AUDIT_LOG}" \
  bash -lc "
    set -euo pipefail
    rg -n 'native asupersync runtime for MCP audit' '${ROOT_DIR}/crates/frankenterm-core/src/mcp.rs'
    rg -n 'record_mcp_audit_sync_bootstraps_native_runtime_for_mcp_feature' '${ROOT_DIR}/crates/frankenterm-core/src/mcp.rs'
  "

SYNTAX_LOG="${ARTIFACT_DIR}/shell_syntax.log"
run_checked \
  "shell_syntax" \
  "${SYNTAX_LOG}" \
  bash -lc "
    set -euo pipefail
    bash -n '${ROOT_DIR}/tests/e2e/test_wa_2toy3_mcp_feature_native_runtime.sh'
  "

MCP_CHECK_LOG="${ARTIFACT_DIR}/frankenterm_core_mcp_check.log"
run_fork_bypass_rch_step \
  "frankenterm_core_mcp_check" \
  "${MCP_CHECK_LOG}" \
  rch exec -- env CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" cargo check -p frankenterm-core --features mcp

FEATURE_MATRIX_LOG="${ARTIFACT_DIR}/frankenterm_core_feature_matrix_check.log"
run_fork_bypass_rch_step \
  "frankenterm_core_feature_matrix_check" \
  "${FEATURE_MATRIX_LOG}" \
  rch exec -- env CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" cargo check -p frankenterm-core --features mcp,web,distributed,browser

MCP_TEST_LOG="${ARTIFACT_DIR}/frankenterm_core_mcp_test.log"
run_fork_bypass_rch_step \
  "frankenterm_core_mcp_test" \
  "${MCP_TEST_LOG}" \
  rch exec -- env CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" cargo test -p frankenterm-core record_mcp_audit_sync_bootstraps_native_runtime_for_mcp_feature --features mcp -- --nocapture

jq -cn \
  --arg bead_id "${BEAD_ID}" \
  --arg scenario_id "${SCENARIO_ID}" \
  --arg status "$([[ ${FAIL} -eq 0 ]] && printf 'passed' || printf 'failed')" \
  --arg correlation_id "${CORRELATION_ID}" \
  --arg artifact_dir "${ARTIFACT_DIR}" \
  --arg source_audit "${SOURCE_AUDIT_LOG}" \
  --arg syntax_log "${SYNTAX_LOG}" \
  --arg mcp_check "${MCP_CHECK_LOG}" \
  --arg mcp_check_meta "$(rch_log_meta_path "${MCP_CHECK_LOG}")" \
  --arg feature_matrix "${FEATURE_MATRIX_LOG}" \
  --arg feature_matrix_meta "$(rch_log_meta_path "${FEATURE_MATRIX_LOG}")" \
  --arg mcp_test "${MCP_TEST_LOG}" \
  --arg mcp_test_meta "$(rch_log_meta_path "${MCP_TEST_LOG}")" \
  --arg structured_log "${STRUCTURED_LOG}" \
  --arg stdout_file "${STDOUT_FILE}" \
  --arg stderr_file "${STDERR_FILE}" \
  --arg commands_file "${COMMANDS_FILE}" \
  --arg env_file "${ENV_FILE}" \
  --argjson pass_count "${PASS}" \
  --argjson fail_count "${FAIL}" \
  --argjson total_count "${TOTAL}" \
  '{
    bead_id: $bead_id,
    scenario_id: $scenario_id,
    status: $status,
    correlation_id: $correlation_id,
    artifact_dir: $artifact_dir,
    pass_count: $pass_count,
    fail_count: $fail_count,
    total_count: $total_count,
    artifacts: {
      source_audit: $source_audit,
      shell_syntax: $syntax_log,
      frankenterm_core_mcp_check: $mcp_check,
      frankenterm_core_mcp_check_meta: $mcp_check_meta,
      frankenterm_core_feature_matrix_check: $feature_matrix,
      frankenterm_core_feature_matrix_check_meta: $feature_matrix_meta,
      frankenterm_core_mcp_test: $mcp_test,
      frankenterm_core_mcp_test_meta: $mcp_test_meta,
      structured_log: $structured_log,
      stdout: $stdout_file,
      stderr: $stderr_file,
      commands: $commands_file,
      env: $env_file
    }
  }' > "${SUMMARY_FILE}"

if [[ ${FAIL} -ne 0 ]]; then
  echo "${BEAD_ID} MCP feature native runtime verification failed. Summary: ${SUMMARY_FILE}" >&2
  exit 1
fi

echo "${BEAD_ID} MCP feature native runtime verification passed. Summary: ${SUMMARY_FILE}"
