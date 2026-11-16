#!/usr/bin/env bash
# E2E: Validate ft-akx00.2.3 client spawn-pane and alt-screen truth closure.
#
# Scenarios:
#   1. Client-domain spawn response resolution succeeds and fails explicitly.
#   2. Alt-screen truth flows through pane listing + unilateral render deltas.
#   3. The mux server and codec surfaces preserve alt-screen state.
#   4. No audited placeholder strings remain in the touched client-domain paths.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-akx00.2.3"
SCENARIO_ID="client_spawn_alt_screen"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
HARNESS_NAME="ft_akx00_2_3_client_spawn_alt_screen"
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
CARGO_TARGET_DIR="target/rch-e2e-ft-akx00-2-3-${RUN_ID}"
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
        --arg surface "client-domain" \
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

echo "=== ft-akx00.2.3 Client Spawn/Alt-Screen E2E ==="
write_env
require_cmd jq
require_cmd rg
require_cmd rch
require_cmd rustfmt
record_command "ensure_rch_ready (RCH_SKIP_SMOKE_PREFLIGHT=${RCH_SKIP_SMOKE_PREFLIGHT})"
ensure_rch_ready
rch_write_meta_json "$(rch_probe_log_path)"
rch_write_meta_json "$(rch_smoke_log_path)"

PLACEHOLDER_AUDIT_LOG="${ARTIFACT_DIR}/placeholder_audit.log"
run_checked \
    "placeholder_audit" \
    "${PLACEHOLDER_AUDIT_LOG}" \
    bash -lc "
        set -euo pipefail
        ! rg -n 'spawn_pane not implemented for ClientDomain|hardcoded \x60false\x60|FIXME.*alt-screen|state should come from the remote' \
            '${ROOT_DIR}/frankenterm/client/src/domain.rs' \
            '${ROOT_DIR}/frankenterm/client/src/pane/clientpane.rs' >/dev/null
    " || true

PROTOCOL_SURFACE_AUDIT_LOG="${ARTIFACT_DIR}/protocol_surface_audit.log"
run_checked \
    "protocol_surface_audit" \
    "${PROTOCOL_SURFACE_AUDIT_LOG}" \
    bash -lc "
        set -euo pipefail
        rg -n 'fn spawn_pane|sync_remote_topology|resolve_remote_spawn_entities|alt_screen_active' \
            '${ROOT_DIR}/frankenterm/client/src/domain.rs' \
            '${ROOT_DIR}/frankenterm/client/src/pane/clientpane.rs' \
            '${ROOT_DIR}/frankenterm/codec/src/lib.rs' \
            '${ROOT_DIR}/frankenterm/mux/src/tab.rs' \
            '${ROOT_DIR}/crates/frankenterm-mux-server-impl/src/sessionhandler.rs'
        rg -n 'alt_screen_active: payload.alt_screen_active|alt_screen_active: false' \
            '${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_client.rs' \
            '${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs' \
            '${ROOT_DIR}/crates/frankenterm-core/tests/vendored_async_contract_behavioral.rs' \
            '${ROOT_DIR}/crates/frankenterm-core/benches/pdu_pipelining.rs' \
            '${ROOT_DIR}/crates/frankenterm-core/benches/mux_client_ops.rs'
    " || true

FMT_LOG="${ARTIFACT_DIR}/fmt_check.log"
run_rch_step \
    "fmt_check" \
    "${FMT_LOG}" \
    env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo fmt --check || true
rch_write_meta_json "${FMT_LOG}"

SPAWN_RESOLUTION_TESTS_LOG="${ARTIFACT_DIR}/spawn_resolution_tests.log"
run_rch_step \
    "spawn_resolution_tests" \
    "${SPAWN_RESOLUTION_TESTS_LOG}" \
    env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test -p wezterm-client resolve_remote_spawn_entities -- --nocapture || true
rch_write_meta_json "${SPAWN_RESOLUTION_TESTS_LOG}"

ALT_SCREEN_STATE_TESTS_LOG="${ARTIFACT_DIR}/alt_screen_state_tests.log"
run_rch_step \
    "alt_screen_state_tests" \
    "${ALT_SCREEN_STATE_TESTS_LOG}" \
    env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test -p wezterm-client alt_screen_state -- --nocapture || true
rch_write_meta_json "${ALT_SCREEN_STATE_TESTS_LOG}"

MUX_SERVER_ALT_SCREEN_LOG="${ARTIFACT_DIR}/mux_server_alt_screen_delta.log"
run_rch_step \
    "mux_server_alt_screen_delta" \
    "${MUX_SERVER_ALT_SCREEN_LOG}" \
    env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test -p frankenterm-mux-server-impl compute_changes_detects_alt_screen_transition_without_other_deltas -- --nocapture || true
rch_write_meta_json "${MUX_SERVER_ALT_SCREEN_LOG}"

CODEC_ROUNDTRIP_LOG="${ARTIFACT_DIR}/codec_roundtrip_render_changes.log"
run_rch_step \
    "codec_roundtrip_render_changes" \
    "${CODEC_ROUNDTRIP_LOG}" \
    env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test -p codec get_pane_render_changes_response_roundtrip -- --nocapture || true
rch_write_meta_json "${CODEC_ROUNDTRIP_LOG}"

FRANKENTERM_CORE_VENDORED_CHECK_LOG="${ARTIFACT_DIR}/frankenterm_core_vendored_check.log"
run_rch_step \
    "frankenterm_core_vendored_check" \
    "${FRANKENTERM_CORE_VENDORED_CHECK_LOG}" \
    env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo check -p frankenterm-core --tests --benches || true
rch_write_meta_json "${FRANKENTERM_CORE_VENDORED_CHECK_LOG}"

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
    --arg placeholder_audit_log "${PLACEHOLDER_AUDIT_LOG}" \
    --arg protocol_surface_audit_log "${PROTOCOL_SURFACE_AUDIT_LOG}" \
    --arg fmt_log "${FMT_LOG}" \
    --arg spawn_resolution_tests_log "${SPAWN_RESOLUTION_TESTS_LOG}" \
    --arg spawn_resolution_tests_meta "$(rch_log_meta_path "${SPAWN_RESOLUTION_TESTS_LOG}")" \
    --arg alt_screen_state_tests_log "${ALT_SCREEN_STATE_TESTS_LOG}" \
    --arg alt_screen_state_tests_meta "$(rch_log_meta_path "${ALT_SCREEN_STATE_TESTS_LOG}")" \
    --arg mux_server_alt_screen_log "${MUX_SERVER_ALT_SCREEN_LOG}" \
    --arg mux_server_alt_screen_meta "$(rch_log_meta_path "${MUX_SERVER_ALT_SCREEN_LOG}")" \
    --arg codec_roundtrip_log "${CODEC_ROUNDTRIP_LOG}" \
    --arg codec_roundtrip_meta "$(rch_log_meta_path "${CODEC_ROUNDTRIP_LOG}")" \
    --arg frankenterm_core_vendored_check_log "${FRANKENTERM_CORE_VENDORED_CHECK_LOG}" \
    --arg frankenterm_core_vendored_check_meta "$(rch_log_meta_path "${FRANKENTERM_CORE_VENDORED_CHECK_LOG}")" \
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
        placeholder_audit: $placeholder_audit_log,
        protocol_surface_audit: $protocol_surface_audit_log,
        fmt_check: $fmt_log,
        fmt_check_meta: (if $fmt_log == "" then null else ($fmt_log + ".rch_meta.json") end),
        spawn_resolution_tests: $spawn_resolution_tests_log,
        spawn_resolution_tests_meta: $spawn_resolution_tests_meta,
        alt_screen_state_tests: $alt_screen_state_tests_log,
        alt_screen_state_tests_meta: $alt_screen_state_tests_meta,
        mux_server_alt_screen_delta: $mux_server_alt_screen_log,
        mux_server_alt_screen_delta_meta: $mux_server_alt_screen_meta,
        codec_roundtrip_render_changes: $codec_roundtrip_log,
        codec_roundtrip_render_changes_meta: $codec_roundtrip_meta,
        frankenterm_core_vendored_check: $frankenterm_core_vendored_check_log,
        frankenterm_core_vendored_check_meta: $frankenterm_core_vendored_check_meta
      }
    }' > "${SUMMARY_FILE}"

cat "${SUMMARY_FILE}"
[[ "${FAIL}" -eq 0 ]]
