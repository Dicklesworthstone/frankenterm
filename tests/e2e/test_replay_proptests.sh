#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="replay_proptests_$(date -u +%Y%m%dT%H%M%SZ)"
scenario_id="replay_property_suite"
json_log="$LOG_DIR/${run_id}.jsonl"
raw_dir="$LOG_DIR/${run_id}_raw"
mkdir -p "$raw_dir"

cargo_home="${CARGO_HOME:-$HOME/.cargo}"
cargo_target_base="${CARGO_TARGET_DIR:-$ROOT_DIR/target-replay-proptests}"

with_run_id_suffix() {
  local path_base="$1"
  if [[ "$path_base" == *"${run_id}"* ]]; then
    echo "$path_base"
    return
  fi
  echo "${path_base}-${run_id}"
}

if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
  cargo_target_dir="$(with_run_id_suffix "$cargo_target_base")"
else
  cargo_target_dir="$cargo_target_base"
fi
mkdir -p "$cargo_home" "$cargo_target_dir"

prop_cases="${PROPTEST_CASES:-100}"
if [[ "$prop_cases" =~ ^[0-9]+$ ]] && (( prop_cases < 100 )); then
  prop_cases=100
fi
if ! [[ "$prop_cases" =~ ^[0-9]+$ ]]; then
  prop_cases=100
fi

determinism_properties=16
diff_properties=6
total_properties=$((determinism_properties + diff_properties))
pass_properties=0
fail_properties=0
total_cases=$((prop_cases * total_properties))
suite_status=0

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

now_ms() {
  echo $(( $(date +%s) * 1000 ))
}

log_json() {
  local payload="$1"
  echo "$payload" >>"$json_log"
}

classify_reason_code() {
  local stderr_file="$1"
  if grep -Fq "No space left on device" "$stderr_file"; then
    echo "disk_exhausted"
    return
  fi
  if grep -Fq "[RCH] local" "$stderr_file"; then
    echo "rch_local_fallback_test_failure"
    return
  fi
  echo "test_failure"
}

run_prop_suite() {
  local suite_label="$1"
  local test_target="$2"
  local property_count="$3"
  local decision_path="$4"

  local stdout_file="$raw_dir/${suite_label}.stdout.log"
  local stderr_file="$raw_dir/${suite_label}.stderr.log"
  local seed_file="$raw_dir/${suite_label}.seeds.log"
  local started_ms
  local ended_ms
  local duration_ms
  local reason_code
  local rch_mode

  started_ms="$(now_ms)"
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_proptests\",\"scenario_id\":\"${suite_label}\",\"correlation_id\":\"${run_id}\",\"decision_path\":\"${decision_path}\",\"inputs\":{\"test_target\":\"${test_target}\",\"properties\":${property_count},\"prop_cases\":${prop_cases},\"cargo_home\":\"${cargo_home}\",\"cargo_target_dir\":\"${cargo_target_dir}\"},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${stdout_file#$ROOT_DIR/}\"}"

  set +e
  rch exec -- env \
    PROPTEST_CASES="${prop_cases}" \
    PROPTEST_VERBOSE=1 \
    CARGO_HOME="${cargo_home}" \
    CARGO_TARGET_DIR="${cargo_target_dir}" \
    cargo test -p frankenterm-core --test "${test_target}" -- --nocapture \
    >"${stdout_file}" 2>"${stderr_file}"
  local rc=$?
  set -e

  ended_ms="$(now_ms)"
  duration_ms=$((ended_ms - started_ms))

  if grep -Fq "[RCH] local" "${stderr_file}"; then
    rch_mode="local_fallback"
  else
    rch_mode="remote_offload"
  fi

  if [[ $rc -eq 0 ]]; then
    pass_properties=$((pass_properties + property_count))
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_proptests\",\"scenario_id\":\"${suite_label}\",\"correlation_id\":\"${run_id}\",\"decision_path\":\"${decision_path}\",\"inputs\":{\"test_target\":\"${test_target}\",\"properties\":${property_count},\"prop_cases\":${prop_cases}},\"outcome\":\"pass\",\"reason_code\":\"assertions_satisfied\",\"error_code\":null,\"duration_ms\":${duration_ms},\"rch_mode\":\"${rch_mode}\",\"artifact_path\":\"${stdout_file#$ROOT_DIR/}\"}"
    return 0
  fi

  fail_properties=$((fail_properties + property_count))
  reason_code="$(classify_reason_code "${stderr_file}")"
  cat "${stdout_file}" "${stderr_file}" | grep -Ei 'seed|proptest-regressions|minimal failing input' >"${seed_file}" || true
  if [[ ! -s "${seed_file}" ]]; then
    echo "no_proptest_seed_detected" >"${seed_file}"
  fi
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_proptests\",\"scenario_id\":\"${suite_label}\",\"correlation_id\":\"${run_id}\",\"decision_path\":\"${decision_path}\",\"inputs\":{\"test_target\":\"${test_target}\",\"properties\":${property_count},\"prop_cases\":${prop_cases}},\"outcome\":\"fail\",\"reason_code\":\"${reason_code}\",\"error_code\":\"cargo_test_failed\",\"duration_ms\":${duration_ms},\"rch_mode\":\"${rch_mode}\",\"artifact_path\":\"${stderr_file#$ROOT_DIR/}\",\"seed_artifact_path\":\"${seed_file#$ROOT_DIR/}\"}"
  tail -n 120 "${stderr_file}" >&2 || true
  return "$rc"
}

suite_started_ms="$(now_ms)"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_proptests\",\"scenario_id\":\"${scenario_id}\",\"correlation_id\":\"${run_id}\",\"decision_path\":\"suite.start\",\"inputs\":{\"total_properties\":${total_properties},\"prop_cases\":${prop_cases}},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"

run_prop_suite "determinism" "proptest_replay_determinism" "${determinism_properties}" "suite.determinism" || suite_status=1
run_prop_suite "diff" "proptest_replay_diff" "${diff_properties}" "suite.diff" || suite_status=1

status="pass"
reason_code="all_scenarios_passed"
if [[ $suite_status -ne 0 ]]; then
  status="fail"
  reason_code="one_or_more_scenarios_failed"
fi

summary_json="{\"test\":\"proptests\",\"properties\":${total_properties},\"pass\":${pass_properties},\"fail\":${fail_properties},\"total_cases\":${total_cases},\"status\":\"${status}\"}"

suite_ended_ms="$(now_ms)"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_proptests\",\"scenario_id\":\"${scenario_id}\",\"correlation_id\":\"${run_id}\",\"decision_path\":\"suite.complete\",\"inputs\":{\"total_properties\":${total_properties},\"pass\":${pass_properties},\"fail\":${fail_properties},\"total_cases\":${total_cases}},\"outcome\":\"${status}\",\"reason_code\":\"${reason_code}\",\"error_code\":null,\"duration_ms\":$((suite_ended_ms - suite_started_ms)),\"artifact_path\":\"${json_log#$ROOT_DIR/}\",\"summary\":${summary_json}}"

echo "${summary_json}"

if [[ $suite_status -ne 0 ]]; then
  exit 1
fi
