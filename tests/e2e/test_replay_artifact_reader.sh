#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="replay_artifact_reader_$(date -u +%Y%m%dT%H%M%SZ)"
scenario_id="replay_artifact_reader"
json_log="$LOG_DIR/${run_id}.jsonl"
raw_dir="$LOG_DIR/${run_id}_raw"
mkdir -p "$raw_dir"

cargo_home="/tmp/cargo-home-replay-artifact-reader"
cargo_target_dir="$ROOT_DIR/target-replay-artifact-reader"

RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/replay_artifact_reader_${run_id}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/replay_artifact_reader_${run_id}.smoke.log"
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
TIMEOUT_BIN=""

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

log_json() {
  local payload="$1"
  echo "$payload" >>"$json_log"
}

fatal() { echo "FATAL: $1" >&2; exit 1; }

run_rch() {
    TMPDIR=/tmp rch "$@"
}

capture_rch_queue_timeout_log() {
    local output_file="$1"
    local queue_log="${output_file%.log}.rch_queue_timeout.log"
    if ! run_rch queue >"${queue_log}" 2>&1; then
        queue_log="${output_file}"
    fi
    printf '%s\n' "${queue_log}"
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
        fatal "rch fell back to local execution; refusing offload policy violation. See ${output_file}"
    fi
}

run_rch_cargo_logged() {
    local output_file="$1"
    shift
    set +e
    (
        cd "${ROOT_DIR}"
        env TMPDIR=/tmp "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${RCH_STEP_TIMEOUT_SECS}" \
            rch exec -- "$@"
    ) >"${output_file}" 2>&1
    local rc=$?
    set -e
    check_rch_fallback "${output_file}"
    if [[ ${rc} -eq 124 || ${rc} -eq 137 ]]; then
        local queue_log
        queue_log="$(capture_rch_queue_timeout_log "${output_file}")"
        fatal "RCH-REMOTE-STALL: rch remote command timed out after ${RCH_STEP_TIMEOUT_SECS}s. See ${queue_log}"
    fi
    return "${rc}"
}

ensure_rch_ready() {
    if ! command -v rch >/dev/null 2>&1; then
        fatal "rch is required for this E2E harness; refusing local cargo execution."
    fi
    resolve_timeout_bin
    if [[ -z "${TIMEOUT_BIN}" ]]; then
        fatal "timeout or gtimeout is required to fail closed on stalled remote execution."
    fi
    set +e
    run_rch --json workers probe --all >"${RCH_PROBE_LOG}" 2>&1
    local probe_rc=$?
    set -e
    if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${RCH_PROBE_LOG}"; then
        fatal "rch workers are unavailable; refusing local cargo execution. See ${RCH_PROBE_LOG}"
    fi
    set +e
    run_rch_cargo_logged "${RCH_SMOKE_LOG}" env CARGO_TARGET_DIR="${cargo_target_dir}" cargo check --help
    local smoke_rc=$?
    set -e
    if [[ ${smoke_rc} -ne 0 ]]; then
        fatal "rch remote smoke preflight failed. See ${RCH_SMOKE_LOG}"
    fi
}

run_reader_test() {
  local scenario="$1"
  local test_filter="$2"
  local combined_file="$raw_dir/scenario${scenario}.combined.log"

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_reader\",\"scenario_id\":\"${scenario}\",\"step\":\"run_test\",\"status\":\"running\",\"run_id\":\"$run_id\",\"inputs\":{\"test_filter\":\"$test_filter\"}}"

  if run_rch_cargo_logged "$combined_file" env \
    CARGO_HOME="$cargo_home" \
    CARGO_TARGET_DIR="$cargo_target_dir" \
    cargo test -p frankenterm-core --lib "$test_filter" -- --nocapture; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_reader\",\"scenario_id\":\"${scenario}\",\"step\":\"run_test\",\"status\":\"pass\",\"run_id\":\"$run_id\",\"test\":\"artifact_reader\",\"version\":\"ftreplay.v1\",\"integrity\":\"pass\",\"compression\":\"none|gzip|zstd\",\"artifact_path\":\"${combined_file#$ROOT_DIR/}\"}"
  else
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_reader\",\"scenario_id\":\"${scenario}\",\"step\":\"run_test\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"test\":\"artifact_reader\",\"version\":\"ftreplay.v1\",\"integrity\":\"fail\",\"reason_code\":\"test_failure\",\"artifact_path\":\"${combined_file#$ROOT_DIR/}\"}"
    tail -n 120 "$combined_file" >&2 || true
    exit 1
  fi
}

ensure_rch_ready

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_reader\",\"scenario_id\":\"$scenario_id\",\"step\":\"start\",\"status\":\"running\",\"run_id\":\"$run_id\"}"

# Scenario 1: write/read and parseable baseline
run_reader_test "1" "replay_fixture_harvest::tests::artifact_reader_open_reads_v1_artifact"

# Scenario 2: corruption/integrity mismatch
run_reader_test "2" "replay_fixture_harvest::tests::artifact_reader_reports_integrity_mismatch"

# Scenario 3: migration path (v0 -> v1)
run_reader_test "3" "replay_fixture_harvest::tests::artifact_reader_open_applies_v0_to_v1_migration"

# Scenario 4: future version incompatible path
run_reader_test "4" "replay_fixture_harvest::tests::artifact_reader_open_rejects_unknown_future_schema"

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_reader\",\"scenario_id\":\"$scenario_id\",\"step\":\"complete\",\"status\":\"pass\",\"run_id\":\"$run_id\"}"

echo "Replay artifact reader e2e passed. Logs: ${json_log#$ROOT_DIR/}"
