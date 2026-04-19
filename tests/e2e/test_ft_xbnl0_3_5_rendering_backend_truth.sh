#!/usr/bin/env bash
# E2E: validate rendering/window-backend truth for ft-xbnl0.3.5.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-xbnl0.3.5"
SCENARIO_ID="rendering_backend_truth"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
HARNESS_NAME="ft_xbnl0_3_5_rendering_backend_truth"
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
        --arg surface "rendering-window-backend" \
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

echo "=== ${BEAD_ID} rendering/window-backend truth ==="
write_env
require_cmd jq
require_cmd rg
require_cmd rch
require_cmd rustfmt
record_command "ensure_rch_ready (RCH_SKIP_SMOKE_PREFLIGHT=${RCH_SKIP_SMOKE_PREFLIGHT})"
ensure_rch_ready
rch_write_meta_json "$(rch_probe_log_path)"
rch_write_meta_json "$(rch_smoke_log_path)"

AUDIT_LOG="${ARTIFACT_DIR}/rendering_truth_audit.log"
run_checked \
    "rendering_truth_audit" \
    "${AUDIT_LOG}" \
    bash -lc "
        set -euo pipefail
        ! rg -n 'ImageDataType::placeholder\\(' '${ROOT_DIR}/crates/frankenterm-gui/src/glyphcache.rs' >/dev/null
        rg -n 'LoadState::Failed|new_single_frame\\(1, 1, vec!\\[0, 0, 0, 0\\]\\)' \
            '${ROOT_DIR}/crates/frankenterm-gui/src/glyphcache.rs'
        rg -n 'load_state != LoadState::Loaded|load_state != crate::glyphcache::LoadState::Loaded' \
            '${ROOT_DIR}/crates/frankenterm-gui/src/termwindow/background.rs' \
            '${ROOT_DIR}/crates/frankenterm-gui/src/termwindow/render/mod.rs'
        rg -n 'GUI image decode/bootstrap failure handling|ft-akx00\\.6\\.3|texture readback only' \
            '${ROOT_DIR}/docs/ft-xbnl0-3-6-supported-path-truth-sweep.md'
    " || true

FMT_LOG="${ARTIFACT_DIR}/rustfmt_check.log"
run_checked \
    "rustfmt_check" \
    "${FMT_LOG}" \
    rustfmt --edition 2024 --check \
        "${ROOT_DIR}/crates/frankenterm-gui/src/glyphcache.rs" \
        "${ROOT_DIR}/crates/frankenterm-gui/src/termwindow/background.rs" \
        "${ROOT_DIR}/crates/frankenterm-gui/src/termwindow/render/mod.rs" || true

TEST_LOG="${ARTIFACT_DIR}/frankenterm_gui_placeholder_state_tests.log"
run_rch_step \
    "frankenterm_gui_placeholder_state_tests" \
    "${TEST_LOG}" \
    env CC=/opt/homebrew/opt/llvm/bin/clang \
        CXX=/opt/homebrew/opt/llvm/bin/clang++ \
        CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
        cargo test -p frankenterm-gui gui_visual_placeholder_ -- --nocapture || true
rch_write_meta_json "${TEST_LOG}"

CHECK_LOG="${ARTIFACT_DIR}/frankenterm_gui_check.log"
run_rch_step \
    "frankenterm_gui_check" \
    "${CHECK_LOG}" \
    env CC=/opt/homebrew/opt/llvm/bin/clang \
        CXX=/opt/homebrew/opt/llvm/bin/clang++ \
        CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
        cargo check -p frankenterm-gui || true
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
    --arg test_log "${TEST_LOG}" \
    --arg test_meta "$(rch_log_meta_path "${TEST_LOG}")" \
    --arg check_log "${CHECK_LOG}" \
    --arg check_meta "$(rch_log_meta_path "${CHECK_LOG}")" \
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
        rch_probe: $rch_probe_log,
        rch_probe_meta: $rch_probe_meta,
        rch_smoke: $rch_smoke_log,
        rch_smoke_meta: $rch_smoke_meta,
        audit_log: $audit_log,
        fmt_log: $fmt_log,
        test_log: $test_log,
        test_meta: $test_meta,
        check_log: $check_log,
        check_meta: $check_meta
      }
    }' > "${SUMMARY_FILE}"

printf 'Summary: %s\n' "${SUMMARY_FILE}"
