#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/swarm_stress_contract"
RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1memj_30_swarm_stress_contract"
CORRELATION_ID="ft-1memj.30-contract-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"

mkdir -p "${LOG_DIR}" "${ARTIFACT_DIR}"

PASS=0
FAIL=0
TOTAL=0

emit_log() {
  local outcome="$1"
  local scenario="$2"
  local decision_path="$3"
  local reason_code="$4"
  local error_code="$5"
  local artifact_path="$6"
  local input_summary="$7"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "swarm_stress_contract.e2e" \
    --arg scenario_id "${SCENARIO_ID}:${scenario}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg decision_path "${decision_path}" \
    --arg input_summary "${input_summary}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path}" \
    '{
      timestamp: $timestamp,
      component: $component,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      decision_path: $decision_path,
      input_summary: $input_summary,
      outcome: $outcome,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path
    }' >> "${LOG_FILE}"
}

record_result() {
  local name="$1"
  local ok="$2"
  local reason_code="${3:-completed}"
  local error_code="${4:-none}"
  local input_summary="${5:-}"

  TOTAL=$((TOTAL + 1))
  if [[ "${ok}" == "true" ]]; then
    PASS=$((PASS + 1))
    emit_log "passed" "${name}" "scenario_end" "${reason_code}" "none" "${LOG_FILE}" "${input_summary}"
    echo "  PASS: ${name}"
  else
    FAIL=$((FAIL + 1))
    emit_log "failed" "${name}" "scenario_end" "${reason_code}" "${error_code}" "${LOG_FILE}" "${input_summary}"
    echo "  FAIL: ${name}"
  fi
}

write_success_rch() {
  local mock_bin="$1"
  mkdir -p "${mock_bin}"
  cat > "${mock_bin}/rch" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

cmdline="$*"

if [[ "${cmdline}" == *"workers probe --all"* ]]; then
  printf '%s\n' '{"api_version":"1.0","data":[{"id":"mock-worker","host":"127.0.0.1","status":"ok"}]}'
  exit 0
fi

if [[ "${1:-}" == "exec" ]]; then
  shift
  target_dir=""
  for arg in "$@"; do
    if [[ "${arg}" == CARGO_TARGET_DIR=* ]]; then
      target_dir="${arg#CARGO_TARGET_DIR=}"
      break
    fi
  done
  if [[ -z "${target_dir}" || "${target_dir}" = /* || "${target_dir}" == ../* ]]; then
    echo "invalid target dir: ${target_dir}" >&2
    exit 64
  fi

  case "${cmdline}" in
    *"cargo test -p frankenterm-core --test e2e_swarm_stress_core -- --nocapture"*)
      cat <<'METRICS'
FT_SWARM_METRIC {"test":"stress_50_panes_idle","pane_count":50,"rss_mb":12.5,"cpu_percent":null,"frame_time_p50_ms":null,"frame_time_p99_ms":null,"events_dropped":0,"backpressure_tier":"Green","duration_s":0.11,"status":"pass","metric_source":"mock","notes":"ok"}
FT_SWARM_METRIC {"test":"stress_100_panes_idle","pane_count":100,"rss_mb":25.0,"cpu_percent":null,"frame_time_p50_ms":null,"frame_time_p99_ms":null,"events_dropped":0,"backpressure_tier":"Green","duration_s":0.22,"status":"pass","metric_source":"mock","notes":"ok"}
FT_SWARM_METRIC {"test":"stress_200_panes_idle","pane_count":200,"rss_mb":40.0,"cpu_percent":null,"frame_time_p50_ms":null,"frame_time_p99_ms":null,"events_dropped":0,"backpressure_tier":"Green","duration_s":0.44,"status":"pass","metric_source":"mock","notes":"ok"}
FT_SWARM_METRIC {"test":"stress_50_panes_active","pane_count":50,"rss_mb":96.0,"cpu_percent":null,"frame_time_p50_ms":null,"frame_time_p99_ms":null,"events_dropped":0,"backpressure_tier":"Yellow","duration_s":0.55,"status":"pass","metric_source":"mock","notes":"ok"}
FT_SWARM_METRIC {"test":"stress_200_panes_active","pane_count":200,"rss_mb":380.0,"cpu_percent":null,"frame_time_p50_ms":null,"frame_time_p99_ms":null,"events_dropped":0,"backpressure_tier":"Red","duration_s":1.25,"status":"pass","metric_source":"mock","notes":"ok"}
FT_SWARM_METRIC {"test":"stress_single_pane_10mb","pane_count":1,"rss_mb":18.0,"cpu_percent":null,"frame_time_p50_ms":null,"frame_time_p99_ms":null,"events_dropped":0,"backpressure_tier":"Green","duration_s":0.35,"status":"pass","metric_source":"mock","notes":"ok"}
FT_SWARM_METRIC {"test":"stress_rapid_pane_create_destroy","pane_count":100,"rss_mb":0.0,"cpu_percent":null,"frame_time_p50_ms":null,"frame_time_p99_ms":null,"events_dropped":0,"backpressure_tier":"Green","duration_s":0.41,"status":"pass","metric_source":"mock","notes":"ok"}
FT_SWARM_METRIC {"test":"stress_200_panes_backpressure","pane_count":200,"rss_mb":180.0,"cpu_percent":null,"frame_time_p50_ms":null,"frame_time_p99_ms":null,"events_dropped":0,"backpressure_tier":"Black","duration_s":0.61,"status":"pass","metric_source":"mock","notes":"ok"}
METRICS
      ;;
    *)
      echo "unexpected exec invocation: ${cmdline}" >&2
      exit 64
      ;;
  esac
  exit 0
fi

echo "unexpected invocation: ${cmdline}" >&2
exit 64
EOF
  chmod +x "${mock_bin}/rch"
}

write_fallback_rch() {
  local mock_bin="$1"
  mkdir -p "${mock_bin}"
  cat > "${mock_bin}/rch" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

cmdline="$*"
if [[ "${cmdline}" == *"workers probe --all"* ]]; then
  printf '%s\n' '{"api_version":"1.0","data":[{"id":"mock-worker","host":"127.0.0.1","status":"ok"}]}'
  exit 0
fi

if [[ "${1:-}" == "exec" ]]; then
  echo "[RCH] local fallback for testing"
  exit 0
fi

echo "unexpected invocation: ${cmdline}" >&2
exit 64
EOF
  chmod +x "${mock_bin}/rch"
}

write_depinfo_failure_rch() {
  local mock_bin="$1"
  mkdir -p "${mock_bin}"
  cat > "${mock_bin}/rch" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

cmdline="$*"
if [[ "${cmdline}" == *"workers probe --all"* ]]; then
  printf '%s\n' '{"api_version":"1.0","data":[{"id":"mock-worker","host":"127.0.0.1","status":"ok"}]}'
  exit 0
fi

if [[ "${1:-}" == "exec" ]]; then
  cat <<'ERR'
warning: `frankenterm-core` (lib) generated 1 warning
error: could not parse/generate dep info at: /data/projects/frankenterm/target/mock-swarm-depinfo/debug/deps/frankenterm_core-16de6415c3b736e3.d

Caused by:
  No such file or directory (os error 2)
[RCH] remote mock-worker failed (exit 101)
ERR
  exit 101
fi

echo "unexpected invocation: ${cmdline}" >&2
exit 64
EOF
  chmod +x "${mock_bin}/rch"
}

scenario_successful_collection() {
  local scenario_dir="${ARTIFACT_DIR}/successful_collection"
  local mock_bin="${scenario_dir}/mock-bin"
  local stdout_file="${scenario_dir}/stdout.log"
  local log_path="${ROOT_DIR}/tests/e2e/logs/ft_1memj_30_swarm_stress_contract-success.jsonl"
  local summary_path="${ROOT_DIR}/tests/e2e/artifacts/swarm_stress/contract-success/summary.json"

  mkdir -p "${scenario_dir}"
  rm -f "${log_path}"
  rm -f "${summary_path}"
  write_success_rch "${mock_bin}"

  emit_log "running" "successful_collection" "mock_success" "none" "none" "${stdout_file}" "scripts/e2e_swarm_stress.sh"
  if ! env \
    PATH="${mock_bin}:${PATH}" \
    RCH_SKIP_SMOKE_PREFLIGHT=1 \
    RUN_ID="contract-success" \
    TARGET_DIR_REL="target/mock-swarm-success" \
    "${ROOT_DIR}/scripts/e2e_swarm_stress.sh" >"${stdout_file}" 2>&1; then
    emit_log "failed" "successful_collection" "mock_success" "script_failed" "script_exit_nonzero" "${stdout_file}" "expected success path"
    return 1
  fi

  if [[ ! -f "${log_path}" ]]; then
    emit_log "failed" "successful_collection" "log_assert" "missing_log" "log_not_found" "${stdout_file}" "expected swarm stress log file"
    return 1
  fi

  if [[ ! -f "${summary_path}" ]]; then
    emit_log "failed" "successful_collection" "summary_assert" "missing_summary" "summary_not_found" "${stdout_file}" "expected swarm stress summary file"
    return 1
  fi

  if ! jq -s -e '
    ([.[] | select(.record_type == "swarm_metric")] | length) == 8 and
    ([.[] | select(.record_type == "suite_summary")] | length) == 1 and
    any(.[]; .record_type == "suite_summary" and .tests_run == 8 and .highest_backpressure_tier == "Black") and
    any(.[]; .record_type == "swarm_metric" and .test == "stress_200_panes_active") and
    any(.[]; .record_type == "suite_event" and .decision_path == "suite_complete" and .outcome == "passed")
  ' "${log_path}" >/dev/null; then
    emit_log "failed" "successful_collection" "jq_assert" "unexpected_log_shape" "log_validation_failed" "${log_path}" "expected 8 metrics and suite summary"
    return 1
  fi

  if ! jq -e '
    (.schema_version == "ft.swarm_stress.summary.v1") and
    (.profile == "smoke") and
    (.tests_run == 8) and
    (.pane_scales == [1,50,100,200]) and
    (.metric_names | length == 8) and
    (.artifacts.commands | endswith("/commands.txt")) and
    (.artifacts.env | endswith("/env.txt")) and
    (.artifacts.structured_log | endswith(".jsonl"))
  ' "${summary_path}" >/dev/null; then
    emit_log "failed" "successful_collection" "summary_assert" "unexpected_summary_shape" "summary_validation_failed" "${summary_path}" "expected smoke summary with artifact pointers"
    return 1
  fi
}

scenario_fail_closed_on_local_fallback() {
  local scenario_dir="${ARTIFACT_DIR}/fail_closed_on_local_fallback"
  local mock_bin="${scenario_dir}/mock-bin"
  local stdout_file="${scenario_dir}/stdout.log"

  mkdir -p "${scenario_dir}"
  write_fallback_rch "${mock_bin}"

  emit_log "running" "fail_closed_on_local_fallback" "mock_fallback" "none" "none" "${stdout_file}" "scripts/e2e_swarm_stress.sh"
  if env \
    PATH="${mock_bin}:${PATH}" \
    RCH_SKIP_SMOKE_PREFLIGHT=1 \
    RUN_ID="contract-fallback" \
    TARGET_DIR_REL="target/mock-swarm-fallback" \
    "${ROOT_DIR}/scripts/e2e_swarm_stress.sh" >"${stdout_file}" 2>&1; then
    emit_log "failed" "fail_closed_on_local_fallback" "mock_fallback" "unexpected_success" "offload_guard_missing" "${stdout_file}" "script should fail when rch falls back locally"
    return 1
  fi
}

scenario_dep_info_failure_classified() {
  local scenario_dir="${ARTIFACT_DIR}/dep_info_failure_classified"
  local mock_bin="${scenario_dir}/mock-bin"
  local stdout_file="${scenario_dir}/stdout.log"
  local log_path="${ROOT_DIR}/tests/e2e/logs/ft_1memj_30_swarm_stress_contract-depinfo.jsonl"

  mkdir -p "${scenario_dir}"
  rm -f "${log_path}"
  write_depinfo_failure_rch "${mock_bin}"

  emit_log "running" "dep_info_failure_classified" "mock_depinfo" "none" "none" "${stdout_file}" "scripts/e2e_swarm_stress.sh"
  if env \
    PATH="${mock_bin}:${PATH}" \
    RCH_SKIP_SMOKE_PREFLIGHT=1 \
    RUN_ID="contract-depinfo" \
    TARGET_DIR_REL="target/mock-swarm-depinfo" \
    "${ROOT_DIR}/scripts/e2e_swarm_stress.sh" >"${stdout_file}" 2>&1; then
    emit_log "failed" "dep_info_failure_classified" "mock_depinfo" "unexpected_success" "depinfo_not_classified" "${stdout_file}" "script should fail on mocked dep-info error"
    return 1
  fi

  if [[ ! -f "${log_path}" ]]; then
    emit_log "failed" "dep_info_failure_classified" "log_assert" "missing_log" "log_not_found" "${stdout_file}" "expected dep-info failure log"
    return 1
  fi

  if ! jq -s -e '
    ([.[] | select(.record_type == "swarm_metric")] | length) == 0 and
    any(.[]; .record_type == "suite_event" and .decision_path == "suite_rch_exec" and .outcome == "failed" and .reason_code == "cargo_dep_info_missing" and .error_code == "cargo_dep_info_missing" and (.details | contains("dep-info failure at /data/projects/frankenterm/target/mock-swarm-depinfo/debug/deps/frankenterm_core-16de6415c3b736e3.d")))
  ' "${log_path}" >/dev/null; then
    emit_log "failed" "dep_info_failure_classified" "jq_assert" "depinfo_reason_missing" "log_validation_failed" "${log_path}" "expected explicit dep-info classification"
    return 1
  fi
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
}

main() {
  require_cmd jq

  echo "Scenario: ${SCENARIO_ID}"

  if scenario_successful_collection; then
    record_result "successful_collection" "true" "mock_metrics_collected" "none" "mock success path produced 8 swarm metrics"
  else
    record_result "successful_collection" "false" "mock_metrics_missing" "script_contract_failed" "mock success path failed"
  fi

  if scenario_fail_closed_on_local_fallback; then
    record_result "fail_closed_on_local_fallback" "true" "offload_guard_enforced" "none" "local fallback rejected"
  else
    record_result "fail_closed_on_local_fallback" "false" "offload_guard_missing" "fallback_not_rejected" "local fallback was not rejected"
  fi

  if scenario_dep_info_failure_classified; then
    record_result "dep_info_failure_classified" "true" "depinfo_reason_classified" "none" "dep-info failure received explicit reason/error codes"
  else
    record_result "dep_info_failure_classified" "false" "depinfo_reason_missing" "depinfo_not_classified" "dep-info failure classification contract failed"
  fi

  echo "Results: pass=${PASS} fail=${FAIL} total=${TOTAL}"
  echo "Log: ${LOG_FILE}"

  if [[ "${FAIL}" -ne 0 ]]; then
    exit 1
  fi
}

main "$@"
