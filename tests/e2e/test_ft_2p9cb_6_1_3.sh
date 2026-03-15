#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="ft_2p9cb_6_1_3_$(date -u +%Y%m%dT%H%M%SZ)"
SCENARIO_ID="ft_2p9cb_6_1_3_fault_isolation_verify"
CORRELATION_ID="ft-2p9cb.6.1.3-${RUN_ID}"
JSON_LOG="${LOG_DIR}/${RUN_ID}.jsonl"
RAW_DIR="${LOG_DIR}/${RUN_ID}_raw"
TARGET_DIR="target-rch-ft-2p9cb-6-1-3"
mkdir -p "${RAW_DIR}"

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "2p9cb_6_1_3"
ensure_rch_ready

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

now_ms() {
  echo $(( $(date +%s) * 1000 ))
}

emit_log() {
  local step="$1"
  local status="$2"
  local outcome="$3"
  local decision_path="$4"
  local reason_code="$5"
  local error_code="$6"
  local rch_mode="$7"
  local artifact_path="$8"
  local input_summary="$9"
  local duration_ms="${10}"

  jq -cn \
    --arg timestamp "$(now_ts)" \
    --arg component "latency_stages.fault_isolation.e2e" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg step "${step}" \
    --arg status "${status}" \
    --arg outcome "${outcome}" \
    --arg decision_path "${decision_path}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg rch_mode "${rch_mode}" \
    --arg artifact_path "${artifact_path}" \
    --arg input_summary "${input_summary}" \
    --argjson duration_ms "${duration_ms}" \
    '{
      timestamp: $timestamp,
      component: $component,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      run_id: $run_id,
      step: $step,
      status: $status,
      outcome: $outcome,
      decision_path: $decision_path,
      reason_code: $reason_code,
      error_code: $error_code,
      rch_mode: $rch_mode,
      artifact_path: $artifact_path,
      input_summary: $input_summary,
      duration_ms: $duration_ms
    }' >> "${JSON_LOG}"
}

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logs" >&2
  exit 1
fi

emit_log \
  "start" \
  "running" \
  "running" \
  "verify.init" \
  "none" \
  "none" \
  "unknown" \
  "${JSON_LOG#${ROOT_DIR}/}" \
  "ft-2p9cb.6.1.3 verification harness start" \
  0

if ! command -v rch >/dev/null 2>&1; then
  emit_log \
    "preflight.rch" \
    "fail" \
    "fail" \
    "verify.preflight" \
    "rch_missing" \
    "rch_not_found" \
    "none" \
    "${JSON_LOG#${ROOT_DIR}/}" \
    "rch binary not found" \
    0
  exit 1
fi

PROBE_LOG="${RAW_DIR}/rch_workers_probe.log"
set +e
(cd "${ROOT_DIR}" && rch workers probe --all) >"${PROBE_LOG}" 2>&1
probe_status=$?
set -e
if [[ ${probe_status} -eq 0 ]]; then
  emit_log \
    "preflight.rch_workers" \
    "pass" \
    "pass" \
    "verify.preflight" \
    "workers_probe_ok" \
    "none" \
    "remote_probe" \
    "${PROBE_LOG#${ROOT_DIR}/}" \
    "rch workers probe succeeded" \
    0
else
  emit_log \
    "preflight.rch_workers" \
    "warn" \
    "warn" \
    "verify.preflight" \
    "workers_probe_failed" \
    "remote_worker_probe_failed" \
    "unknown" \
    "${PROBE_LOG#${ROOT_DIR}/}" \
    "rch workers probe failed; proceeding with fallback-aware execution" \
    0
fi

declare -a TEST_NAMES=(
  "fault_isolation_unit"
  "blast_radius_unit"
  "transition_log_unit"
  "fault_isolation_property"
)

declare -a TEST_CMDS=(
  "rch exec -- env CARGO_NET_GIT_FETCH_WITH_CLI=true CARGO_TARGET_DIR=${TARGET_DIR} cargo test -p frankenterm-core --lib fault_isolation -- --nocapture"
  "rch exec -- env CARGO_NET_GIT_FETCH_WITH_CLI=true CARGO_TARGET_DIR=${TARGET_DIR} cargo test -p frankenterm-core --lib blast_radius -- --nocapture"
  "rch exec -- env CARGO_NET_GIT_FETCH_WITH_CLI=true CARGO_TARGET_DIR=${TARGET_DIR} cargo test -p frankenterm-core --lib transition_log -- --nocapture"
  "rch exec -- env CARGO_NET_GIT_FETCH_WITH_CLI=true CARGO_TARGET_DIR=${TARGET_DIR} cargo test -p frankenterm-core --test proptest_latency_stages fault_isolation -- --nocapture"
)

declare -a REQUIRED_MARKERS=(
  "test_fault_isolation_snapshot_serde ... ok"
  "test_blast_radius_report_serde ... ok"
  "test_fault_transition_log_serde ... ok"
  "fault_isolation_snapshot_serde ... ok"
)

pass_count=0
fail_count=0
local_fallback_count=0

for i in "${!TEST_NAMES[@]}"; do
  name="${TEST_NAMES[$i]}"
  cmd="${TEST_CMDS[$i]}"
  marker="${REQUIRED_MARKERS[$i]}"
  stdout_file="${RAW_DIR}/${name}.stdout.log"
  stderr_file="${RAW_DIR}/${name}.stderr.log"
  started_ms="$(now_ms)"

  emit_log \
    "run.${name}" \
    "running" \
    "running" \
    "verify.execute" \
    "none" \
    "none" \
    "unknown" \
    "${stdout_file#${ROOT_DIR}/}" \
    "Executing: ${cmd}" \
    0

  set +e
  (cd "${ROOT_DIR}" && eval "${cmd}") >"${stdout_file}" 2>"${stderr_file}"
  status=$?
  set -e

  ended_ms="$(now_ms)"
  duration_ms=$((ended_ms - started_ms))

  rch_mode="remote_offload"
  if grep -Fq "[RCH] local" "${stdout_file}" || grep -Fq "[RCH] local" "${stderr_file}"; then
    rch_mode="local_fallback"
    local_fallback_count=$((local_fallback_count + 1))
  fi

  if [[ ${status} -ne 0 ]]; then
    fail_count=$((fail_count + 1))
    emit_log \
      "run.${name}" \
      "fail" \
      "fail" \
      "verify.execute" \
      "cargo_test_failed" \
      "nonzero_exit" \
      "${rch_mode}" \
      "${stderr_file#${ROOT_DIR}/}" \
      "Command failed with exit=${status}" \
      "${duration_ms}"
    tail -n 120 "${stderr_file}" >&2 || true
    exit ${status}
  fi

  if ! grep -Fq "${marker}" "${stdout_file}"; then
    fail_count=$((fail_count + 1))
    emit_log \
      "run.${name}" \
      "fail" \
      "fail" \
      "verify.assertions" \
      "missing_success_marker" \
      "expected_test_marker_missing" \
      "${rch_mode}" \
      "${stdout_file#${ROOT_DIR}/}" \
      "Missing marker: ${marker}" \
      "${duration_ms}"
    tail -n 120 "${stdout_file}" >&2 || true
    exit 1
  fi

  pass_count=$((pass_count + 1))
  emit_log \
    "run.${name}" \
    "pass" \
    "pass" \
    "verify.execute" \
    "assertions_satisfied" \
    "none" \
    "${rch_mode}" \
    "${stdout_file#${ROOT_DIR}/}" \
    "Marker verified: ${marker}" \
    "${duration_ms}"
done

summary_status="pass"
if [[ ${fail_count} -gt 0 ]]; then
  summary_status="fail"
fi

emit_log \
  "complete" \
  "${summary_status}" \
  "${summary_status}" \
  "verify.complete" \
  "verification_complete" \
  "none" \
  "mixed" \
  "${JSON_LOG#${ROOT_DIR}/}" \
  "pass=${pass_count}, fail=${fail_count}, local_fallback=${local_fallback_count}" \
  0

jq -cn \
  --arg run_id "${RUN_ID}" \
  --arg status "${summary_status}" \
  --argjson pass "${pass_count}" \
  --argjson fail "${fail_count}" \
  --argjson local_fallback "${local_fallback_count}" \
  --arg log "${JSON_LOG#${ROOT_DIR}/}" \
  --arg raw "${RAW_DIR#${ROOT_DIR}/}" \
  '{
    test: "ft_2p9cb_6_1_3_fault_isolation_verify",
    run_id: $run_id,
    status: $status,
    pass: $pass,
    fail: $fail,
    local_fallback: $local_fallback,
    json_log: $log,
    raw_dir: $raw
  }'

if [[ "${summary_status}" != "pass" ]]; then
  exit 1
fi
