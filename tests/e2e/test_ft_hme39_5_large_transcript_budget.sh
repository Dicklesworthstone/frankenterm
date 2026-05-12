#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-hme39.5"
SCENARIO_ID="ft_hme39_5_large_transcript_budget"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/logs/terminal-conformance/${BEAD_ID}/${RUN_ID}"
EVENT_LOG="${ARTIFACT_DIR}/events.jsonl"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
BUDGET_EVENTS="${ARTIFACT_DIR}/budget_events.jsonl"
TEST_LOG="${ARTIFACT_DIR}/large_transcript_budget.log"
REMOTE_TARGET_DIR="${RCH_REMOTE_TARGET_DIR:-target/rch-e2e-ft-hme39-5-${RUN_ID}}"
SUMMARY_WRITTEN=0

RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-3600}"
RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
RCH_WORKER_SELECTION_WAIT_SECS="${RCH_WORKER_SELECTION_WAIT_SECS:-1800}"
RCH_WORKER_SELECTION_POLL_SECS="${RCH_WORKER_SELECTION_POLL_SECS:-15}"
RCH_REMOTE_PREFLIGHT_WAIT_SECS="${RCH_REMOTE_PREFLIGHT_WAIT_SECS:-${RCH_WORKER_SELECTION_WAIT_SECS}}"
export RCH_QUEUE_WHEN_BUSY="${RCH_QUEUE_WHEN_BUSY:-1}"
export RCH_TEST_SLOTS="${RCH_TEST_SLOTS:-2}"
DEFAULT_RCH_MIRROR_REQUIRED_PATHS="Cargo.toml,crates/frankenterm-core/src/lib.rs,crates/frankenterm-core/src/large_swarm_replay.rs,crates/frankenterm-core/tests/large_transcript_budget.rs,crates/frankenterm-core-replay-types/Cargo.toml,crates/frankenterm-core-replay-types/src/lib.rs,crates/frankenterm-core-replay-types/src/replay_decision_graph.rs,crates/frankenterm-core-replay-types/src/recorder_metadata.rs"
RCH_MIRROR_REQUIRED_PATHS="${RCH_MIRROR_REQUIRED_PATHS:-${DEFAULT_RCH_MIRROR_REQUIRED_PATHS}}"

mkdir -p "${ARTIFACT_DIR}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "${SCENARIO_ID}" "${ROOT_DIR}"

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    jq -cn \
      --arg timestamp "$(now_ts)" \
      --arg component "terminal_conformance.e2e" \
      --arg bead_id "${BEAD_ID}" \
      --arg scenario_id "${SCENARIO_ID}" \
      --arg correlation_id "${RUN_ID}" \
      --arg decision_path "preflight.${cmd}" \
      --arg outcome "failed" \
      --arg reason_code "missing_prerequisite" \
      --arg artifact_path "${EVENT_LOG#"${ROOT_DIR}"/}" \
      '{timestamp:$timestamp,component:$component,bead_id:$bead_id,scenario_id:$scenario_id,correlation_id:$correlation_id,decision_path:$decision_path,outcome:$outcome,reason_code:$reason_code,artifact_path:$artifact_path}' \
      >>"${EVENT_LOG}"
    exit 1
  fi
}

emit_event() {
  local decision_path="$1"
  local outcome="$2"
  local reason_code="$3"
  local artifact_path="$4"
  local input_summary="$5"
  local worker="${6:-}"

  jq -cn \
    --arg timestamp "$(now_ts)" \
    --arg component "terminal_conformance.e2e" \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${RUN_ID}" \
    --arg decision_path "${decision_path}" \
    --arg input_summary "${input_summary}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg artifact_path "${artifact_path#"${ROOT_DIR}"/}" \
    --arg worker "${worker}" \
    '{
      timestamp: $timestamp,
      component: $component,
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      decision_path: $decision_path,
      input_summary: $input_summary,
      outcome: $outcome,
      reason_code: $reason_code,
      artifact_path: $artifact_path
    } + (if $worker == "" then {} else {rch_worker_id: $worker} end)' \
    >>"${EVENT_LOG}"
}

worker_for_log() {
  local meta_file
  meta_file="$(rch_log_meta_path "$1")"
  if [[ -f "${meta_file}" ]]; then
    jq -r '.selected_worker // ""' "${meta_file}" 2>/dev/null || true
  fi
}

failure_reason_for_log() {
  local log_file="$1"
  local meta_file code
  if rch_log_has_worker_selection_all_busy "${log_file}"; then
    printf '%s\n' "rch_infrastructure_worker_selection_all_busy"
    return
  fi
  if grep -Eq 'no admissible workers:|active_project_exclusion' "${log_file}" 2>/dev/null; then
    printf '%s\n' "rch_infrastructure_worker_selection_blocked"
    return
  fi
  if grep -Fq '[RCH] local (no workers with Rust installed)' "${log_file}" 2>/dev/null; then
    printf '%s\n' "rch_infrastructure_no_rust_worker_available"
    return
  fi
  if ! rch_log_has_remote_execution_marker "${log_file}"; then
    printf '%s\n' "rch_infrastructure_remote_execution_missing"
    return
  fi
  meta_file="$(rch_log_meta_path "${log_file}")"
  if [[ ! -f "${meta_file}" ]]; then
    printf '%s\n' "source_or_test_failure"
    return
  fi
  if jq -e '.fail_open_detected == true' "${meta_file}" >/dev/null 2>&1; then
    printf '%s\n' "rch_local_fallback"
    return
  fi
  if jq -e '.timed_out == true' "${meta_file}" >/dev/null 2>&1; then
    printf '%s\n' "rch_remote_timeout"
    return
  fi
  code="$(jq -r '.failure_reason_code // ""' "${meta_file}" 2>/dev/null || true)"
  if [[ -n "${code}" ]]; then
    printf 'rch_infrastructure_%s\n' "${code}"
    return
  fi
  printf '%s\n' "source_or_test_failure"
}

# shellcheck disable=SC2329 # Called from the EXIT trap helper.
pre_summary_failure_reason() {
  local mirror_preflight
  local remote_preflight
  local remote_reason
  local worker_selection
  local probe_log

  if [[ -s "${TEST_LOG}" ]]; then
    failure_reason_for_log "${TEST_LOG}"
    return
  fi

  mirror_preflight="$(rch_mirror_preflight_log_path)"
  if [[ -s "${mirror_preflight}" ]] \
    && jq -e '(.status // "") == "failed" or (.status // "") == "blocked" or (.reason_code // "") != ""' "${mirror_preflight}" >/dev/null 2>&1; then
    printf '%s\n' "rch_infrastructure_mirror_preflight_failed"
    return
  fi

  remote_preflight="$(rch_remote_preflight_log_path)"
  if [[ -s "${remote_preflight}" ]]; then
    remote_reason="$(jq -r '
      if ((.status // "") == "failed" or (.status // "") == "blocked") then
        (.reason_code // "remote_preflight_failed")
      else
        ""
      end
    ' "${remote_preflight}" 2>/dev/null || true)"
    if [[ -n "${remote_reason}" ]]; then
      printf 'rch_infrastructure_%s\n' "${remote_reason}"
      return
    fi
  fi

  worker_selection="$(rch_worker_selection_log_path)"
  if [[ -s "${worker_selection}" ]]; then
    if ! jq -e . "${worker_selection}" >/dev/null 2>&1; then
      printf '%s\n' "rch_infrastructure_worker_selection_failed"
      return
    fi
    if jq -e '(.success // false) != true' "${worker_selection}" >/dev/null 2>&1; then
      printf '%s\n' "rch_infrastructure_worker_selection_failed"
      return
    fi
    if jq -e '(.data.worker_selection.worker // "") == ""' "${worker_selection}" >/dev/null 2>&1; then
      printf '%s\n' "rch_infrastructure_worker_selection_blocked"
      return
    fi
  fi

  probe_log="$(rch_probe_log_path)"
  if [[ -s "${probe_log}" ]] \
    && ! grep -Fq '"success": true' "${probe_log}"; then
    printf '%s\n' "rch_infrastructure_probe_failed"
    return
  fi

  printf '%s\n' "harness_aborted_before_summary"
}

extract_budget_events() {
  : >"${BUDGET_EVENTS}"
  grep -E '^\{' "${TEST_LOG}" 2>/dev/null \
    | jq -c 'select(.component? == "terminal_conformance.large_transcript_budget")' \
    >"${BUDGET_EVENTS}" || true
}

write_summary() {
  local outcome="$1"
  local failed_reason="${2:-}"
  local worker
  local test_meta
  local budget_summary

  worker="$(worker_for_log "${TEST_LOG}")"
  test_meta="$(rch_log_meta_path "${TEST_LOG}" | sed "s#^${ROOT_DIR}/##")"
  if [[ -s "${BUDGET_EVENTS}" ]]; then
    budget_summary="$(jq -s '{
      row_count: length,
      scale_point_count: (map(select(.pane_count? != null)) | length),
      failed_count: (map(select(.outcome == "failed")) | length),
      max_wall_time_ms: (map(.wall_time_ms // 0) | max // 0),
      max_artifact_bytes: (map(.artifact_bytes // 0) | max // 0),
      max_memory_proxy_bytes: (map(.memory_proxy_bytes // 0) | max // 0)
    }' "${BUDGET_EVENTS}")"
  else
    budget_summary='{"row_count":0,"scale_point_count":0,"failed_count":0,"max_wall_time_ms":0,"max_artifact_bytes":0,"max_memory_proxy_bytes":0}'
  fi

  jq -cn \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg outcome "${outcome}" \
    --arg artifact_dir "${ARTIFACT_DIR#"${ROOT_DIR}"/}" \
    --arg events_jsonl "${EVENT_LOG#"${ROOT_DIR}"/}" \
    --arg summary_json "${SUMMARY_FILE#"${ROOT_DIR}"/}" \
    --arg test_log "${TEST_LOG#"${ROOT_DIR}"/}" \
    --arg test_meta "${test_meta}" \
    --arg budget_events "${BUDGET_EVENTS#"${ROOT_DIR}"/}" \
    --arg rch_probe "$(rch_probe_log_path | sed "s#^${ROOT_DIR}/##")" \
    --arg rch_remote_preflight "$(rch_remote_preflight_log_path | sed "s#^${ROOT_DIR}/##")" \
    --arg rch_mirror_preflight "$(rch_mirror_preflight_log_path | sed "s#^${ROOT_DIR}/##")" \
    --arg rch_capabilities "$(rch_capabilities_log_path | sed "s#^${ROOT_DIR}/##")" \
    --arg rch_scheduler_workers "$(rch_scheduler_workers_log_path | sed "s#^${ROOT_DIR}/##")" \
    --arg rch_worker_selection "$(rch_worker_selection_log_path | sed "s#^${ROOT_DIR}/##")" \
    --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
    --arg failed_reason "${failed_reason}" \
    --arg worker "${worker}" \
    --argjson budget_summary "${budget_summary}" \
    '{
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      run_id: $run_id,
      outcome: $outcome,
      artifact_dir: $artifact_dir,
      remote_target_dir: $remote_target_dir,
      artifacts: {
        events_jsonl: $events_jsonl,
        summary_json: $summary_json,
        large_transcript_budget_log: $test_log,
        large_transcript_budget_meta: $test_meta,
        budget_events_jsonl: $budget_events,
        rch_probe: $rch_probe,
        rch_remote_preflight: $rch_remote_preflight,
        rch_mirror_preflight: $rch_mirror_preflight,
        rch_capabilities: $rch_capabilities,
        rch_scheduler_workers: $rch_scheduler_workers,
        rch_worker_selection: $rch_worker_selection
      },
      budget_summary: $budget_summary
    }
    + (if $worker == "" then {} else {rch_worker_id: $worker} end)
    + (if $failed_reason == "" then {} else {final_failure:{reason_code:$failed_reason, log:$test_log, meta:$test_meta}} end)' \
    >"${SUMMARY_FILE}"
  SUMMARY_WRITTEN=1
}

require_cmd jq
require_cmd rch

# shellcheck disable=SC2329 # Invoked by the EXIT trap below.
write_abort_summary_on_exit() {
  local status="$?"
  if [[ "${status}" -ne 0 && "${SUMMARY_WRITTEN}" -eq 0 ]]; then
    extract_budget_events
    write_summary "failed" "$(pre_summary_failure_reason)"
  fi
}

trap write_abort_summary_on_exit EXIT

emit_event "suite.start" "running" "suite_started" "${EVENT_LOG}" \
  "artifact_dir=${ARTIFACT_DIR#"${ROOT_DIR}"/}; remote_target_dir=${REMOTE_TARGET_DIR}"
ensure_rch_ready
emit_event "preflight.rch" "passed" "rch_ready" "$(rch_probe_log_path)" \
  "RCH remote workers reachable; smoke preflight skipped=${RCH_SKIP_SMOKE_PREFLIGHT}"

emit_event "budget.large_transcript" "running" "remote_rch_started" "${TEST_LOG}" \
  "cargo test -p frankenterm-core --test large_transcript_budget terminal_conformance_large_transcript_budget target_dir=${REMOTE_TARGET_DIR}"
set +e
run_rch_cargo_logged "${TEST_LOG}" \
  env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
  cargo test --config net.git-fetch-with-cli=true \
    -p frankenterm-core --test large_transcript_budget --no-default-features \
    terminal_conformance_large_transcript_budget -- --nocapture
status=$?
set -e

extract_budget_events
worker="$(worker_for_log "${TEST_LOG}")"
if [[ "${status}" -eq 0 ]]; then
  emit_event "budget.large_transcript" "passed" "remote_rch_passed" "${TEST_LOG}" \
    "large transcript scale-point budgets passed" "${worker}"
  emit_event "suite.complete" "passed" "all_assertions_satisfied" "${SUMMARY_FILE}" \
    "large transcript performance/resource budget passed" "${worker}"
  write_summary "passed"
else
  failed_reason="$(failure_reason_for_log "${TEST_LOG}")"
  emit_event "budget.large_transcript" "failed" "${failed_reason}" "${TEST_LOG}" \
    "large transcript budget failed; see RCH metadata and budget events" "${worker}"
  emit_event "suite.complete" "failed" "large_transcript_budget_failed" "${SUMMARY_FILE}" \
    "see step logs and rch metadata" "${worker}"
  write_summary "failed" "${failed_reason}"
fi

printf 'summary=%s\n' "${SUMMARY_FILE#"${ROOT_DIR}"/}"
exit "${status}"
