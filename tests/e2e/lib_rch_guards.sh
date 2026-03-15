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
#   check_rch_fallback()       - Fatal if rch fell back to local execution
#   run_rch()                  - TMPDIR-safe rch wrapper
#   resolve_timeout_bin()      - Find timeout or gtimeout

# Guard against double-sourcing.
[[ -n "${_LIB_RCH_GUARDS_LOADED:-}" ]] && return 0
_LIB_RCH_GUARDS_LOADED=1

RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"

# Populated by rch_init().
_RCH_PROBE_LOG=""
_RCH_SMOKE_LOG=""
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

probe_has_reachable_workers() {
    grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"
}

check_rch_fallback() {
    local output_file="$1"
    if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_file}" 2>/dev/null; then
        rch_fatal "rch fell back to local execution; refusing offload policy violation. See ${output_file}"
    fi
}

# Usage: run_rch_cargo_logged <output_file> <args passed to rch exec -- ...>
# The caller is responsible for including `env CARGO_TARGET_DIR=... cargo ...`
# in the args.
run_rch_cargo_logged() {
    local output_file="$1"
    shift

    set +e
    (
        cd "${_RCH_REPO_ROOT}"
        env TMPDIR=/tmp "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${RCH_STEP_TIMEOUT_SECS}" \
            rch exec -- "$@"
    ) >"${output_file}" 2>&1
    local rc=$?
    set -e

    check_rch_fallback "${output_file}"
    if [[ ${rc} -eq 124 || ${rc} -eq 137 ]]; then
        local queue_log="${output_file%.log}.rch_queue_timeout.log"
        if ! run_rch queue >"${queue_log}" 2>&1; then
            queue_log="${output_file}"
        fi
        rch_fatal "RCH-REMOTE-STALL: rch remote command timed out after ${RCH_STEP_TIMEOUT_SECS}s. See ${queue_log}"
    fi
    return "${rc}"
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
    if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${_RCH_PROBE_LOG}"; then
        rch_fatal "rch workers are unavailable; refusing local cargo execution. See ${_RCH_PROBE_LOG}"
    fi

    set +e
    run_rch_cargo_logged "${_RCH_SMOKE_LOG}" env CARGO_TARGET_DIR=target cargo check --help
    local smoke_rc=$?
    set -e
    if [[ ${smoke_rc} -ne 0 ]]; then
        rch_fatal "rch remote smoke preflight failed. See ${_RCH_SMOKE_LOG}"
    fi
}
