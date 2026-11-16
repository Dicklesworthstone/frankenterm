#!/usr/bin/env bash
# E2E: Validate ft-akx00.6.4 terminfo image rendering behavior.
#
# Scenarios:
#   1. Placeholder image branches in the terminfo renderer are gone.
#   2. Raw/cropped/animated image cases pass via rch-offloaded tests.
#   3. Termwiz check/clippy stay green for the touched surface.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-akx00.6.4"
SCENARIO_ID="terminfo_image_rendering"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
HARNESS_NAME="ft_akx00_6_4_terminfo_image_rendering"
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

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "${HARNESS_NAME}" "${ROOT_DIR}"
RCH_SKIP_SMOKE_PREFLIGHT=1

PASS=0
FAIL=0
TOTAL=0
CARGO_TARGET_DIR="target/rch-e2e-ft-akx00-6-4-${RUN_ID}"
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
        --arg surface "rendering" \
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
        if command -v jq >/dev/null 2>&1; then
            emit_log "preflight:${cmd}" "failed" 0 "missing command ${cmd}"
        fi
        printf 'missing required command: %s\n' "${cmd}" >&2
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
    record_command "run_rch_cargo_logged ${log_file} $*"
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

echo "=== ft-akx00.6.4 Terminfo Image Rendering E2E ==="
write_env
require_cmd jq
require_cmd rg
require_cmd rch
record_command "ensure_rch_ready (RCH_SKIP_SMOKE_PREFLIGHT=${RCH_SKIP_SMOKE_PREFLIGHT})"
ensure_rch_ready
rch_write_meta_json "$(rch_probe_log_path)"
rch_write_meta_json "$(rch_smoke_log_path)"

AUDIT_LOG="${ARTIFACT_DIR}/renderer_contract.log"
run_checked \
    "renderer_contract" \
    "${AUDIT_LOG}" \
    bash -lc "
        set -euo pipefail
        ! rg -n 'TODO: slice out the requested region|and encode as a PNG|AnimRgba8 \\{ \\.\\. \\} => \\{[[:space:]]*unimplemented!\\(\\)' '${ROOT_DIR}/frankenterm/termwiz/src/render/terminfo.rs' >/dev/null
        rg -n 'fn encode_iterm_inline_image' '${ROOT_DIR}/frankenterm/termwiz/src/render/terminfo.rs'
        rg -n 'iterm2_full_rgba_image_is_encoded_as_png' '${ROOT_DIR}/frankenterm/termwiz/src/render/terminfo.rs'
        rg -n 'iterm2_cropped_rgba_image_uses_only_requested_region' '${ROOT_DIR}/frankenterm/termwiz/src/render/terminfo.rs'
        rg -n 'iterm2_animated_rgba_image_is_encoded_as_gif' '${ROOT_DIR}/frankenterm/termwiz/src/render/terminfo.rs'
        rg -n 'iterm2_cropped_undecodable_image_returns_an_explicit_error' '${ROOT_DIR}/frankenterm/termwiz/src/render/terminfo.rs'
    " || true

TEST_LOG="${ARTIFACT_DIR}/termwiz_iterm2_tests.log"
run_rch_step \
    "termwiz_iterm2_tests" \
    "${TEST_LOG}" \
    env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test -p termwiz --features use_image --lib iterm2_ || true
rch_write_meta_json "${TEST_LOG}"

CHECK_LOG="${ARTIFACT_DIR}/termwiz_check.log"
run_rch_step \
    "termwiz_check" \
    "${CHECK_LOG}" \
    env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo check -p termwiz --features use_image --all-targets || true
rch_write_meta_json "${CHECK_LOG}"

CLIPPY_LOG="${ARTIFACT_DIR}/termwiz_clippy.log"
run_rch_step \
    "termwiz_clippy" \
    "${CLIPPY_LOG}" \
    env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo clippy -p termwiz --features use_image --all-targets -- -D warnings || true
rch_write_meta_json "${CLIPPY_LOG}"

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
    --arg test_log "${TEST_LOG}" \
    --arg test_meta "$(rch_log_meta_path "${TEST_LOG}")" \
    --arg check_log "${CHECK_LOG}" \
    --arg check_meta "$(rch_log_meta_path "${CHECK_LOG}")" \
    --arg clippy_log "${CLIPPY_LOG}" \
    --arg clippy_meta "$(rch_log_meta_path "${CLIPPY_LOG}")" \
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
        renderer_contract: $audit_log,
        termwiz_iterm2_tests: $test_log,
        termwiz_iterm2_tests_meta: $test_meta,
        termwiz_check: $check_log,
        termwiz_check_meta: $check_meta,
        termwiz_clippy: $clippy_log,
        termwiz_clippy_meta: $clippy_meta
      }
    }' > "${SUMMARY_FILE}"

cat "${SUMMARY_FILE}"
[[ "${FAIL}" -eq 0 ]]
