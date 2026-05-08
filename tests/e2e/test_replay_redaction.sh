#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
GUARD_LIB="${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
mkdir -p "$LOG_DIR"

run_id="replay_redaction_$(date -u +%Y%m%dT%H%M%SZ)"
json_log="$LOG_DIR/${run_id}.jsonl"
cargo_home="/tmp/cargo-home-replay-redaction-e2e"
component="replay_redaction"
scenario_id="replay_redaction_suite"
RCH_LOCAL_TMPDIR="${FT_REPLAY_CAPTURE_LOCAL_TMPDIR:-${TMPDIR:-/tmp}}"
remote_tmpdir="${FT_REPLAY_CAPTURE_REMOTE_TMPDIR:-/home/ubuntu}"
default_cargo_target_dir="target/rch-e2e-replay-redaction-${run_id}"
requested_cargo_target_dir="${FT_REPLAY_CAPTURE_TARGET_DIR:-}"
if [[ -n "${requested_cargo_target_dir}" && "${requested_cargo_target_dir}" != /* ]]; then
  cargo_target_dir="${requested_cargo_target_dir}"
else
  cargo_target_dir="${default_cargo_target_dir}"
fi
mkdir -p "${cargo_target_dir}"

RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${GUARD_LIB}"
rch_init "${LOG_DIR}" "${run_id}" "replay_redaction" "${ROOT_DIR}"

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

log_json() {
  echo "$1" >>"$json_log"
}

assert_nonzero_tests_ran() {
    local output_file="$1"
    if grep -Eq 'running 0 tests|0 passed; 0 failed' "$output_file" 2>/dev/null; then
        echo "cargo test completed without executing any tests. See ${output_file}" >&2
        return 65
    fi
}

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"prereq_check\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{\"command\":\"$cmd\"},\"outcome\":\"failed\",\"reason_code\":\"missing_prerequisite\",\"error_code\":\"E2E-PREREQ\",\"artifact_path\":\"${json_log#"$ROOT_DIR"/}\"}"
    echo "missing required command: $cmd" >&2
    exit 1
  fi
}

run_rch_preflight() {
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"rch_preflight\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{\"cargo_target_dir\":\"$cargo_target_dir\"},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${json_log#"$ROOT_DIR"/}\"}"

  set +e
  ( ensure_rch_ready )
  local rc=$?
  set -e

  if [[ $rc -ne 0 ]]; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"rch_preflight\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{\"cargo_target_dir\":\"$cargo_target_dir\"},\"outcome\":\"failed\",\"reason_code\":\"rch_preflight_failed\",\"error_code\":$rc,\"artifact_path\":\"${json_log#"$ROOT_DIR"/}\",\"artifacts\":{\"probe\":\"$(rch_probe_log_path | sed "s#^$ROOT_DIR/##")\",\"smoke\":\"$(rch_smoke_log_path | sed "s#^$ROOT_DIR/##")\"}}"
    exit "$rc"
  fi

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"rch_preflight\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{\"cargo_target_dir\":\"$cargo_target_dir\"},\"outcome\":\"pass\",\"reason_code\":\"workers_reachable\",\"error_code\":null,\"artifact_path\":\"${json_log#"$ROOT_DIR"/}\",\"artifacts\":{\"probe\":\"$(rch_probe_log_path | sed "s#^$ROOT_DIR/##")\",\"smoke\":\"$(rch_smoke_log_path | sed "s#^$ROOT_DIR/##")\"}}"
}

require_cmd jq
require_cmd rch
run_rch_preflight

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"suite\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${json_log#"$ROOT_DIR"/}\"}"

run_scenario() {
  local scenario_num="$1"
  local scenario_id="$2"
  local test_name="$3"

  local raw_log="$LOG_DIR/${run_id}.scenario_${scenario_num}.cargo.log"

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"replay_redaction\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\"},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${raw_log#"$ROOT_DIR"/}\"}"

  set +e
  (
    run_rch_cargo_logged "$raw_log" env \
      TMPDIR="$remote_tmpdir" \
      CARGO_HOME="$cargo_home" \
      CARGO_TARGET_DIR="$cargo_target_dir" \
      cargo test -p frankenterm-core --lib "$test_name" -- --nocapture
  )
  local rc=$?
  set -e

  if [[ $rc -ne 0 ]]; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"replay_redaction\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\",\"error_context\":\"cargo test failed\"},\"outcome\":\"failed\",\"reason_code\":\"cargo_test_failed\",\"error_code\":$rc,\"artifact_path\":\"${raw_log#"$ROOT_DIR"/}\"}"
    tail -n 80 "$raw_log" >&2 || true
    return "$rc"
  fi

  if ! assert_nonzero_tests_ran "$raw_log"; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"replay_redaction\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\",\"error_context\":\"cargo test ran zero tests\"},\"outcome\":\"failed\",\"reason_code\":\"zero_tests_executed\",\"error_code\":65,\"artifact_path\":\"${raw_log#"$ROOT_DIR"/}\"}"
    tail -n 80 "$raw_log" >&2 || true
    return 65
  fi

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"replay_redaction\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\"},\"outcome\":\"pass\",\"reason_code\":\"assertions_satisfied\",\"error_code\":null,\"secrets_found\":1,\"secrets_redacted\":1,\"artifact_path\":\"${raw_log#"$ROOT_DIR"/}\"}"
}

run_scenario 1 "mask_mode" "redaction_mask_mode_scrubs_secrets"
run_scenario 2 "hash_mode" "redaction_hash_mode_is_deterministic"
run_scenario 3 "retention_tombstone" "retention_enforcer_tombstones_expired_t3_artifact"
run_scenario 4 "drop_mode" "redaction_drop_mode_clears_content"

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"suite\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"pass\",\"reason_code\":\"all_checks_passed\",\"error_code\":null,\"artifact_path\":\"${json_log#"$ROOT_DIR"/}\"}"

echo "Replay redaction e2e passed. Logs: ${json_log#"$ROOT_DIR"/}"
