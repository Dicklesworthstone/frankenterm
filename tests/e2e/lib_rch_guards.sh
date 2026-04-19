#!/usr/bin/env bash
# Shared rch fail-closed guard library for E2E test harnesses.
#
# Source this from any E2E harness that uses `rch exec -- cargo ...`:
#
#   source "$(dirname "$0")/lib_rch_guards.sh"
#   rch_init "${LOG_DIR}" "${run_id}" "harness_name"
#   ensure_rch_ready
#
# Then use `run_rch_cargo_logged <output_file> <cargo args...>` instead of
# bare `rch exec -- env ... cargo ...`.
#
# Provides:
#   rch_init()                 - Set up variables (call once at start)
#   ensure_rch_ready()         - Preflight: probe workers + smoke cargo check
#   run_rch_cargo_logged()     - Timeout-wrapped rch cargo with stall/fallback detection
#   rch_write_meta_json()      - Persist worker/exit/timing metadata for an rch log
#   check_rch_fallback()       - Fatal if rch entered a fail-open/off-policy path
#   run_rch()                  - TMPDIR-safe rch wrapper
#   resolve_timeout_bin()      - Find timeout or gtimeout

# Guard against double-sourcing.
[[ -n "${_LIB_RCH_GUARDS_LOADED:-}" ]] && return 0
_LIB_RCH_GUARDS_LOADED=1

RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
RCH_SMOKE_TIMEOUT_SECS="${RCH_SMOKE_TIMEOUT_SECS:-600}"
# Set this to 1 for harnesses whose first material verification steps already
# run through `run_rch_cargo_logged`. That keeps remote execution fail-closed
# without paying a duplicate full-repo sync for a cargo smoke command.
RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-0}"

# Populated by rch_init().
_RCH_PROBE_LOG=""
_RCH_SMOKE_LOG=""
_RCH_SMOKE_TARGET_DIR=""
_RCH_REPO_ROOT=""
TIMEOUT_BIN=""

rch_fatal() {
    echo "FATAL: $1" >&2
    exit 1
}

run_rch() {
    TMPDIR=/tmp rch "$@"
}

resolve_timeout_bin() {
    if command -v timeout >/dev/null 2>&1; then
        TIMEOUT_BIN="timeout"
    elif command -v gtimeout >/dev/null 2>&1; then
        TIMEOUT_BIN="gtimeout"
    else
        TIMEOUT_BIN=""
    fi
}

rch_probe_log_path() {
    printf '%s\n' "${_RCH_PROBE_LOG}"
}

rch_smoke_log_path() {
    printf '%s\n' "${_RCH_SMOKE_LOG}"
}

rch_log_meta_path() {
    printf '%s.rch_meta.json\n' "$1"
}

rch_write_meta_json() {
    local log_file="$1"
    local wrapper_exit_code="${2:-}"
    local meta_file probe_worker_count selected_worker sync_duration_ms remote_duration_ms remote_exit_code
    local failure_reason_code failure_reason_detail
    local probe_worker_ids_json="[]"
    local probe_worker_ids_raw=""
    local skipped_smoke_preflight="false"
    local reachable_workers_detected="false"
    local fail_open_detected="false"
    local timed_out="false"

    if ! command -v jq >/dev/null 2>&1; then
        rch_fatal "jq is required to write rch metadata artifacts."
    fi

    meta_file="$(rch_log_meta_path "${log_file}")"

    if [[ ! -f "${log_file}" ]]; then
        jq -cn \
            --arg log_file "${log_file}" \
            --arg wrapper_exit_code "${wrapper_exit_code}" \
            '{
              log_file: $log_file,
              missing: true,
              wrapper_exit_code: (if $wrapper_exit_code == "" then null else ($wrapper_exit_code | tonumber) end)
            }' > "${meta_file}"
        return 0
    fi

    probe_worker_ids_raw="$(rch_extract_probe_worker_ids "${log_file}")"
    if [[ -n "${probe_worker_ids_raw}" ]]; then
        probe_worker_ids_json="$(printf '%s\n' "${probe_worker_ids_raw}" | jq -R . | jq -s .)"
    fi

    if grep -q 'Smoke preflight skipped because RCH_SKIP_SMOKE_PREFLIGHT=1' "${log_file}" 2>/dev/null; then
        skipped_smoke_preflight="true"
    fi

    if probe_has_reachable_workers "${log_file}"; then
        reachable_workers_detected="true"
    fi

    if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${log_file}" 2>/dev/null; then
        fail_open_detected="true"
    fi

    if [[ "${wrapper_exit_code}" == "124" || "${wrapper_exit_code}" == "137" ]]; then
        timed_out="true"
    fi

    probe_worker_count="$(rch_extract_probe_worker_count "${log_file}")"
    selected_worker="$(rch_extract_selected_worker "${log_file}")"
    sync_duration_ms="$(rch_extract_sync_duration_ms "${log_file}")"
    remote_duration_ms="$(rch_extract_remote_duration_ms "${log_file}")"
    remote_exit_code="$(rch_extract_remote_exit_code "${log_file}")"
    failure_reason_code="$(rch_extract_failure_reason_code "${log_file}")"
    failure_reason_detail="$(rch_extract_failure_reason_detail "${log_file}")"

    jq -cn \
        --arg log_file "${log_file}" \
        --arg selected_worker "${selected_worker}" \
        --arg probe_worker_count "${probe_worker_count}" \
        --arg sync_duration_ms "${sync_duration_ms}" \
        --arg remote_duration_ms "${remote_duration_ms}" \
        --arg remote_exit_code "${remote_exit_code}" \
        --arg wrapper_exit_code "${wrapper_exit_code}" \
        --arg failure_reason_code "${failure_reason_code}" \
        --arg failure_reason_detail "${failure_reason_detail}" \
        --arg repo_root "${_RCH_REPO_ROOT}" \
        --arg smoke_target_dir "${_RCH_SMOKE_TARGET_DIR}" \
        --arg timeout_bin "${TIMEOUT_BIN}" \
        --arg step_timeout_secs "${RCH_STEP_TIMEOUT_SECS}" \
        --arg smoke_timeout_secs "${RCH_SMOKE_TIMEOUT_SECS}" \
        --arg rch_skip_smoke_preflight_requested "${RCH_SKIP_SMOKE_PREFLIGHT}" \
        --argjson probe_worker_ids "${probe_worker_ids_json}" \
        --argjson skipped_smoke_preflight "${skipped_smoke_preflight}" \
        --argjson reachable_workers_detected "${reachable_workers_detected}" \
        --argjson fail_open_detected "${fail_open_detected}" \
        --argjson timed_out "${timed_out}" \
        '{
          log_file: $log_file,
          selected_worker: (if $selected_worker == "" then null else $selected_worker end),
          probe_worker_count: (if $probe_worker_count == "" then null else ($probe_worker_count | tonumber) end),
          probe_worker_ids: $probe_worker_ids,
          sync_duration_ms: (if $sync_duration_ms == "" then null else ($sync_duration_ms | tonumber) end),
          remote_duration_ms: (if $remote_duration_ms == "" then null else ($remote_duration_ms | tonumber) end),
          remote_exit_code: (if $remote_exit_code == "" then null else ($remote_exit_code | tonumber) end),
          wrapper_exit_code: (if $wrapper_exit_code == "" then null else ($wrapper_exit_code | tonumber) end),
          failure_reason_code: (if $failure_reason_code == "" then null else $failure_reason_code end),
          failure_reason_detail: (if $failure_reason_detail == "" then null else $failure_reason_detail end),
          repo_root: (if $repo_root == "" then null else $repo_root end),
          smoke_target_dir: (if $smoke_target_dir == "" then null else $smoke_target_dir end),
          timeout_bin: (if $timeout_bin == "" then null else $timeout_bin end),
          step_timeout_secs: (if $step_timeout_secs == "" then null else ($step_timeout_secs | tonumber) end),
          smoke_timeout_secs: (if $smoke_timeout_secs == "" then null else ($smoke_timeout_secs | tonumber) end),
          rch_skip_smoke_preflight_requested: (if $rch_skip_smoke_preflight_requested == "" then null else ($rch_skip_smoke_preflight_requested == "1") end),
          skipped_smoke_preflight: $skipped_smoke_preflight,
          reachable_workers_detected: $reachable_workers_detected,
          fail_open_detected: $fail_open_detected,
          timed_out: $timed_out
        }' > "${meta_file}"
}

rch_extract_selected_worker() {
    local output_file="$1"
    sed -nE 's/.*Selected worker: ([^ ]+) at .*/\1/p' "${output_file}" 2>/dev/null | tail -n 1
}

rch_extract_probe_worker_ids() {
    local output_file="$1"
    grep -Eo '"id"[[:space:]]*:[[:space:]]*"[^"]+"' "${output_file}" 2>/dev/null \
        | sed -E 's/.*"id"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/' \
        | sort -u || true
}

rch_extract_probe_worker_count() {
    local output_file="$1"
    local count
    count="$(rch_extract_probe_worker_ids "${output_file}" | sed '/^$/d' | wc -l | tr -d ' ')"
    if [[ -n "${count}" ]]; then
        printf '%s\n' "${count}"
    fi
}

rch_extract_sync_duration_ms() {
    local output_file="$1"
    sed -nE 's/.*Sync complete: .* in ([0-9]+)ms.*/\1/p' "${output_file}" 2>/dev/null | tail -n 1 | tr -cd '0-9'
}

rch_extract_remote_duration_ms() {
    local output_file="$1"
    sed -nE 's/.*Remote command finished: exit=[-0-9]+ in ([0-9]+)ms.*/\1/p' "${output_file}" 2>/dev/null | tail -n 1 | tr -cd '0-9'
}

rch_extract_remote_exit_code() {
    local output_file="$1"
    sed -nE 's/.*Remote command finished: exit=([-0-9]+) in [0-9]+ms.*/\1/p' "${output_file}" 2>/dev/null | tail -n 1 | tr -cd '0-9-'
}

rch_extract_failure_reason_code() {
    local output_file="$1"

    if grep -Fq "can't find crate for \`core\`" "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-CROSS-RUST-TARGET-MISSING"
    elif grep -Fq "x86_64-w64-mingw32-gcc: not found" "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-CROSS-CC-MISSING-WINDOWS"
    elif grep -Eq "cc: error: unrecognized command-line option '-arch'|cc: error: unrecognized command-line option '-mmacosx-version-min=" "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-CROSS-CC-MISSING-DARWIN"
    elif grep -Eq "No package 'wayland-client' found|Package wayland-client was not found in the pkg-config search path" "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-PKG-CONFIG-MISSING-WAYLAND"
    elif grep -Eq "was not found in the pkg-config search path|No package '.*' found" "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-PKG-CONFIG-DEPENDENCY-MISSING"
    elif grep -Fq "Error building OpenSSL:" "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-VENDORED-OPENSSL-BUILD-FAILED"
    fi
}

rch_extract_failure_reason_detail() {
    local output_file="$1"

    if grep -Fq "can't find crate for \`core\`" "${output_file}" 2>/dev/null; then
        grep -F "can't find crate for \`core\`" "${output_file}" 2>/dev/null | tail -n 1
    elif grep -Fq "x86_64-w64-mingw32-gcc: not found" "${output_file}" 2>/dev/null; then
        grep -F "x86_64-w64-mingw32-gcc: not found" "${output_file}" 2>/dev/null | tail -n 1
    elif grep -Eq "cc: error: unrecognized command-line option '-arch'|cc: error: unrecognized command-line option '-mmacosx-version-min=" "${output_file}" 2>/dev/null; then
        grep -E "cc: error: unrecognized command-line option '-arch'|cc: error: unrecognized command-line option '-mmacosx-version-min=" "${output_file}" 2>/dev/null | head -n 1
    elif grep -Eq "No package 'wayland-client' found|Package wayland-client was not found in the pkg-config search path" "${output_file}" 2>/dev/null; then
        grep -E "No package 'wayland-client' found|Package wayland-client was not found in the pkg-config search path" "${output_file}" 2>/dev/null | head -n 1
    elif grep -Eq "was not found in the pkg-config search path|No package '.*' found" "${output_file}" 2>/dev/null; then
        grep -E "was not found in the pkg-config search path|No package '.*' found" "${output_file}" 2>/dev/null | head -n 1
    elif grep -Fq "Error building OpenSSL:" "${output_file}" 2>/dev/null; then
        grep -F "Error building OpenSSL:" "${output_file}" 2>/dev/null | tail -n 1
    fi
}

probe_has_reachable_workers() {
    grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"
}

check_rch_fallback() {
    local output_file="$1"
    if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_file}" 2>/dev/null; then
        rch_fatal "rch entered a fail-open or off-policy execution path; refusing offload policy violation. See ${output_file}"
    fi
}

child_pids() {
    local pid="$1"
    if command -v pgrep >/dev/null 2>&1; then
        pgrep -P "${pid}" 2>/dev/null || true
    fi
}

terminate_process_tree() {
    local pid="$1"
    local signal="${2:-TERM}"
    local child
    for child in $(child_pids "${pid}"); do
        terminate_process_tree "${child}" "${signal}"
    done
    kill -"${signal}" "${pid}" 2>/dev/null || true
}

start_rch_fallback_monitor() {
    local runner_pid="$1"
    local output_file="$2"

    (
        while kill -0 "${runner_pid}" 2>/dev/null; do
            if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_file}" 2>/dev/null; then
                terminate_process_tree "${runner_pid}" TERM
                sleep 2
                terminate_process_tree "${runner_pid}" KILL
                exit 0
            fi
            sleep 1
        done
    ) &
    printf '%s\n' "$!"
}

stop_rch_fallback_monitor() {
    local monitor_pid="$1"
    if [[ -n "${monitor_pid}" ]]; then
        kill "${monitor_pid}" 2>/dev/null || true
        wait "${monitor_pid}" 2>/dev/null || true
    fi
}

rch_timeout_queue_log() {
    local output_file="$1"
    local queue_log="${output_file%.log}.rch_queue_timeout.log"
    if ! run_rch queue >"${queue_log}" 2>&1; then
        queue_log="${output_file}"
    fi
    printf '%s\n' "${queue_log}"
}

rch_timeout_reason_code() {
    local output_file="$1"
    if grep -Eq 'Retrieving (build )?artifacts?( from)?' "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-ARTIFACT-STALL"
    else
        printf '%s\n' "RCH-REMOTE-STALL"
    fi
}

rch_timeout_reason_message() {
    local reason_code="$1"
    local timeout_secs="$2"
    if [[ "${reason_code}" == "RCH-ARTIFACT-STALL" ]]; then
        printf '%s\n' "rch remote command timed out after ${timeout_secs}s while retrieving artifacts from the worker"
    else
        printf '%s\n' "rch remote command timed out after ${timeout_secs}s"
    fi
}

# Usage: run_rch_cargo_logged_with_timeout <timeout_secs> <output_file> <args passed to rch exec -- ...>
# The caller is responsible for including `env CARGO_TARGET_DIR=... cargo ...`
# in the args.
run_rch_cargo_logged_with_timeout() {
    local timeout_secs="$1"
    local output_file="$2"
    shift 2
    local runner_pid=""
    local monitor_pid=""

    if [[ -z "${TIMEOUT_BIN}" ]]; then
        resolve_timeout_bin
    fi
    if [[ -z "${TIMEOUT_BIN}" ]]; then
        rch_fatal "timeout or gtimeout is required to fail closed on stalled remote execution."
    fi

    : >"${output_file}"

    set +e
    (
        cd "${_RCH_REPO_ROOT}"
        exec env TMPDIR=/tmp "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${timeout_secs}" \
            rch exec -- "$@"
    ) >"${output_file}" 2>&1 &
    runner_pid="$!"
    monitor_pid="$(start_rch_fallback_monitor "${runner_pid}" "${output_file}")"

    wait "${runner_pid}"
    local rc=$?
    set -e
    stop_rch_fallback_monitor "${monitor_pid}"
    rch_write_meta_json "${output_file}" "${rc}"

    check_rch_fallback "${output_file}"
    if [[ ${rc} -eq 124 || ${rc} -eq 137 ]]; then
        local queue_log
        queue_log="$(rch_timeout_queue_log "${output_file}")"
        local reason_code
        reason_code="$(rch_timeout_reason_code "${output_file}")"
        rch_fatal "${reason_code}: $(rch_timeout_reason_message "${reason_code}" "${timeout_secs}"). See ${queue_log}"
    fi
    return "${rc}"
}

# Usage: run_rch_cargo_logged <output_file> <args passed to rch exec -- ...>
# The caller is responsible for including `env CARGO_TARGET_DIR=... cargo ...`
# in the args.
run_rch_cargo_logged() {
    local output_file="$1"
    shift
    run_rch_cargo_logged_with_timeout "${RCH_STEP_TIMEOUT_SECS}" "${output_file}" "$@"
}

# Call once at harness start. Sets up internal variables.
# Usage: rch_init <log_dir> <run_id> <harness_name> [repo_root]
rch_init() {
    local log_dir="$1"
    local run_id="$2"
    local harness_name="$3"
    _RCH_REPO_ROOT="${4:-$(cd "$(dirname "${BASH_SOURCE[1]}")/../.." && pwd)}"

    _RCH_PROBE_LOG="${log_dir}/${harness_name}_${run_id}.rch_probe.log"
    _RCH_SMOKE_LOG="${log_dir}/${harness_name}_${run_id}.rch_smoke.log"
    _RCH_SMOKE_TARGET_DIR="target/rch-smoke/${harness_name}/${run_id}"
}

# Preflight check: ensure rch is available, workers reachable, and remote
# cargo execution works. Calls rch_fatal on any failure.
ensure_rch_ready() {
    if ! command -v rch >/dev/null 2>&1; then
        rch_fatal "rch is required for this E2E harness; refusing local cargo execution."
    fi
    resolve_timeout_bin
    if [[ -z "${TIMEOUT_BIN}" ]]; then
        rch_fatal "timeout or gtimeout is required to fail closed on stalled remote execution."
    fi

    set +e
    run_rch --json workers probe --all >"${_RCH_PROBE_LOG}" 2>&1
    local probe_rc=$?
    set -e
    rch_write_meta_json "${_RCH_PROBE_LOG}" "${probe_rc}"
    if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${_RCH_PROBE_LOG}"; then
        rch_fatal "rch workers are unavailable; refusing local cargo execution. See ${_RCH_PROBE_LOG}"
    fi

    if [[ "${RCH_SKIP_SMOKE_PREFLIGHT}" == "1" ]]; then
        printf '%s\n' "Smoke preflight skipped because RCH_SKIP_SMOKE_PREFLIGHT=1" >"${_RCH_SMOKE_LOG}"
        rch_write_meta_json "${_RCH_SMOKE_LOG}" "0"
        return 0
    fi

    set +e
    run_rch_cargo_logged_with_timeout "${RCH_SMOKE_TIMEOUT_SECS}" "${_RCH_SMOKE_LOG}" \
        env CARGO_TARGET_DIR="${_RCH_SMOKE_TARGET_DIR}" cargo check --help
    local smoke_rc=$?
    set -e
    if [[ ${smoke_rc} -ne 0 ]]; then
        rch_fatal "rch remote smoke preflight failed. See ${_RCH_SMOKE_LOG}"
    fi
}
