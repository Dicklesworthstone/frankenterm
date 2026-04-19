#!/usr/bin/env bash
# E2E: validate runtime/search retention compaction for ft-xbnl0.4.3.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-xbnl0.4.3"
SCENARIO_ID="runtime_retention"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
HARNESS_NAME="ft_xbnl0_4_3_runtime_retention"
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
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "${HARNESS_NAME}" "${ROOT_DIR}"
RCH_SKIP_SMOKE_PREFLIGHT=1

PASS=0
FAIL=0
TOTAL=0
REMOTE_TARGET_DIR="/tmp/ft-$(whoami)-target"
EXPECTED_CC="/opt/homebrew/opt/llvm/bin/clang"

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
        printf 'expected_cc=%s\n' "${EXPECTED_CC}"
        printf 'rch_skip_smoke_preflight=%s\n' "${RCH_SKIP_SMOKE_PREFLIGHT}"
        printf 'rch_probe_log=%s\n' "$(rch_probe_log_path)"
        printf 'rch_smoke_log=%s\n' "$(rch_smoke_log_path)"
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
        --arg surface "runtime-search-retention" \
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
        printf 'PASS %s\n' "${step}"
    else
        FAIL=$((FAIL + 1))
        emit_log "${step}" "failed" "${duration_ms}" "${message}"
        printf 'FAIL %s\n' "${step}" >&2
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

run_probe_step() {
    local step="$1"
    local log_file="$2"
    shift 2
    local start_ns end_ns duration_ms
    start_ns="$(date +%s%N)"
    record_command "rch exec -- $*"
    if run_rch_cargo_logged "${log_file}" "$@"; then
        end_ns="$(date +%s%N)"
        duration_ms="$(((end_ns - start_ns) / 1000000))"
        emit_log "${step}" "passed" "${duration_ms}" "${log_file}"
        printf 'PASS %s\n' "${step}"
        return 0
    fi
    end_ns="$(date +%s%N)"
    duration_ms="$(((end_ns - start_ns) / 1000000))"
    emit_log "${step}" "failed" "${duration_ms}" "${log_file}"
    printf 'WARN %s\n' "${step}" >&2
    return 0
}

echo "=== ${BEAD_ID} runtime/search retention verification ==="
write_env
command -v jq >/dev/null 2>&1
command -v rg >/dev/null 2>&1
command -v rch >/dev/null 2>&1
command -v rustfmt >/dev/null 2>&1
record_command "ensure_rch_ready (RCH_SKIP_SMOKE_PREFLIGHT=${RCH_SKIP_SMOKE_PREFLIGHT})"
ensure_rch_ready
rch_write_meta_json "$(rch_probe_log_path)"
rch_write_meta_json "$(rch_smoke_log_path)"

AUDIT_LOG="${ARTIFACT_DIR}/retention_source_audit.log"
run_checked \
    "retention_source_audit" \
    "${AUDIT_LOG}" \
    bash -lc "
        set -euo pipefail
        rg -n 'compact_runtime_pane_state|ft_xbnl0_4_3_runtime_state_compaction_drops_unobserved_panes' \
            '${ROOT_DIR}/crates/frankenterm-core/src/runtime.rs'
        rg -n 'prune_missing_pane_watermarks|ft_xbnl0_4_3_tick_prunes_missing_pane_watermarks' \
            '${ROOT_DIR}/crates/frankenterm-core/src/search/indexing_pipeline.rs'
    "

FMT_LOG="${ARTIFACT_DIR}/rustfmt_check.log"
run_checked \
    "rustfmt_check" \
    "${FMT_LOG}" \
    rustfmt --edition 2024 --check \
        "${ROOT_DIR}/crates/frankenterm-core/src/runtime.rs" \
        "${ROOT_DIR}/crates/frankenterm-core/src/search/indexing_pipeline.rs"

REQUESTED_TEST_LOG="${ARTIFACT_DIR}/requested_worker_test.log"
run_probe_step \
    "requested_worker_test" \
    "${REQUESTED_TEST_LOG}" \
    env CC="${EXPECTED_CC}" \
        CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
        cargo test -p frankenterm-core ft_xbnl0_4_3_ -- --nocapture
rch_write_meta_json "${REQUESTED_TEST_LOG}"

FALLBACK_TEST_LOG="${ARTIFACT_DIR}/worker_compatible_test.log"
run_rch_step \
    "worker_compatible_test" \
    "${FALLBACK_TEST_LOG}" \
    env CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
        cargo test -p frankenterm-core ft_xbnl0_4_3_ -- --nocapture
rch_write_meta_json "${FALLBACK_TEST_LOG}"

CHECK_LOG="${ARTIFACT_DIR}/worker_compatible_check.log"
run_rch_step \
    "worker_compatible_check" \
    "${CHECK_LOG}" \
    env CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
        cargo check -p frankenterm-core --lib
rch_write_meta_json "${CHECK_LOG}"

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
    --arg rch_probe_log "$(rch_probe_log_path)" \
    --arg rch_probe_meta "$(rch_log_meta_path "$(rch_probe_log_path)")" \
    --arg rch_smoke_log "$(rch_smoke_log_path)" \
    --arg rch_smoke_meta "$(rch_log_meta_path "$(rch_smoke_log_path)")" \
    --arg audit_log "${AUDIT_LOG}" \
    --arg fmt_log "${FMT_LOG}" \
    --arg requested_test_log "${REQUESTED_TEST_LOG}" \
    --arg requested_test_meta "$(rch_log_meta_path "${REQUESTED_TEST_LOG}")" \
    --arg fallback_test_log "${FALLBACK_TEST_LOG}" \
    --arg fallback_test_meta "$(rch_log_meta_path "${FALLBACK_TEST_LOG}")" \
    --arg check_log "${CHECK_LOG}" \
    --arg check_meta "$(rch_log_meta_path "${CHECK_LOG}")" \
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
      note: "requested_worker_test records the exact clang-prefixed command even when the Linux worker lacks that Homebrew path",
      artifacts: {
        commands: $commands_file,
        env: $env_file,
        structured_log: $structured_log,
        stdout: $stdout_file,
        stderr: $stderr_file,
        rch_probe: $rch_probe_log,
        rch_probe_meta: $rch_probe_meta,
        rch_smoke: $rch_smoke_log,
        rch_smoke_meta: $rch_smoke_meta,
        retention_source_audit: $audit_log,
        rustfmt_check: $fmt_log,
        requested_worker_test: $requested_test_log,
        requested_worker_test_meta: $requested_test_meta,
        worker_compatible_test: $fallback_test_log,
        worker_compatible_test_meta: $fallback_test_meta,
        worker_compatible_check: $check_log,
        worker_compatible_check_meta: $check_meta
      }
    }' > "${SUMMARY_FILE}"

if [[ "${FAIL}" -ne 0 ]]; then
    echo "ft-xbnl0.4.3 retention verification FAILED. Summary: ${SUMMARY_FILE}" >&2
    exit 1
fi

echo "ft-xbnl0.4.3 retention verification passed. Summary: ${SUMMARY_FILE}"
