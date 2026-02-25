#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_nu4_3_9_5_dogfood_capture"
CORRELATION_ID="ft-nu4.3.9.5-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_nu4_3_9_5_${RUN_ID}.jsonl"
SUMMARY_FILE="${LOG_DIR}/ft_nu4_3_9_5_${RUN_ID}_summary.json"
TARGET_DIR="target-rch-ft-nu4-3-9-5-${RUN_ID}"

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
    --arg component "dogfood.e2e" \
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

fail_now() {
  local scenario="$1"
  local decision_path="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local input_summary="$6"
  emit_log \
    "failed" \
    "${scenario}" \
    "${decision_path}" \
    "${reason_code}" \
    "${error_code}" \
    "${artifact_path}" \
    "${input_summary}"
  jq -cn \
    --arg run_id "${RUN_ID}" \
    --arg outcome "failed" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact "${artifact_path}" \
    '{
      run_id: $run_id,
      outcome: $outcome,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact: $artifact
    }' > "${SUMMARY_FILE}"
  exit 1
}

emit_log \
  "started" \
  "suite_init" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ft-nu4.3.9.5 dogfood fixture capture validation"

if ! command -v jq >/dev/null 2>&1; then
  fail_now \
    "suite_init" \
    "preflight_jq" \
    "jq_missing" \
    "jq_not_found" \
    "$(basename "${LOG_FILE}")" \
    "jq is required for structured logging"
fi

if ! command -v rch >/dev/null 2>&1; then
  fail_now \
    "suite_init" \
    "preflight_rch" \
    "rch_missing" \
    "rch_not_found" \
    "$(basename "${LOG_FILE}")" \
    "rch is required; cargo must not run locally for this bead"
fi

RCH_PROBE_LOG="${LOG_DIR}/ft_nu4_3_9_5_${RUN_ID}_rch_workers_probe.json"
if ! rch workers probe --all --json > "${RCH_PROBE_LOG}" 2>"${RCH_PROBE_LOG}.stderr"; then
  fail_now \
    "suite_init" \
    "preflight_rch_workers_command" \
    "rch_workers_probe_failed" \
    "rch_probe_command_failed" \
    "$(basename "${RCH_PROBE_LOG}.stderr")" \
    "rch workers probe command failed"
fi

if ! jq -e '[.data[] | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length > 0' \
  "${RCH_PROBE_LOG}" >/dev/null; then
  fail_now \
    "suite_init" \
    "preflight_rch_workers" \
    "rch_workers_unreachable" \
    "remote_worker_unavailable" \
    "$(basename "${RCH_PROBE_LOG}")" \
    "No reachable rch workers; aborting before any cargo invocation"
fi

CORPUS_TEST_LOG="${LOG_DIR}/ft_nu4_3_9_5_${RUN_ID}_pattern_corpus.stdout.log"
set +e
(
  cd "${ROOT_DIR}"
  env TMPDIR=/tmp \
    rch exec -- \
    env CARGO_TARGET_DIR="${TARGET_DIR}" \
    cargo test -p frankenterm-core --test pattern_corpus -- --nocapture
) 2>&1 | tee "${CORPUS_TEST_LOG}"
CORPUS_STATUS=${PIPESTATUS[0]}
set -e

if grep -q "\\[RCH\\] local" "${CORPUS_TEST_LOG}"; then
  fail_now \
    "corpus_validation" \
    "offload_guard" \
    "rch_local_fallback" \
    "remote_offload_required" \
    "$(basename "${CORPUS_TEST_LOG}")" \
    "rch fell back to local execution; refusing local CPU-intensive run"
fi

if [[ ${CORPUS_STATUS} -ne 0 ]]; then
  fail_now \
    "corpus_validation" \
    "cargo_test_pattern_corpus" \
    "pattern_corpus_regression" \
    "cargo_test_failed" \
    "$(basename "${CORPUS_TEST_LOG}")" \
    "pattern_corpus test failed"
fi

emit_log \
  "passed" \
  "corpus_validation" \
  "cargo_test_pattern_corpus" \
  "dogfood_metadata_validated" \
  "none" \
  "$(basename "${CORPUS_TEST_LOG}")" \
  "pattern_corpus tests passed through remote offload"

if ! command -v ft >/dev/null 2>&1; then
  fail_now \
    "live_capture" \
    "preflight_ft" \
    "ft_binary_missing" \
    "ft_not_found" \
    "$(basename "${LOG_FILE}")" \
    "Install or expose ft in PATH before running live dogfood capture"
fi

if ! command -v wezterm >/dev/null 2>&1; then
  fail_now \
    "live_capture" \
    "preflight_wezterm" \
    "wezterm_binary_missing" \
    "wezterm_not_found" \
    "$(basename "${LOG_FILE}")" \
    "Install or expose wezterm in PATH before running live dogfood capture"
fi

WEZTERM_LIST_LOG="${LOG_DIR}/ft_nu4_3_9_5_${RUN_ID}_wezterm_list.json"
if ! wezterm cli list > "${WEZTERM_LIST_LOG}" 2>"${WEZTERM_LIST_LOG}.stderr"; then
  fail_now \
    "live_capture" \
    "wezterm_cli_list" \
    "wezterm_mux_unreachable" \
    "wezterm_cli_failed" \
    "$(basename "${WEZTERM_LIST_LOG}.stderr")" \
    "Ensure mux is running (example: wezterm start --mux)"
fi

STATE_JSON="${LOG_DIR}/ft_nu4_3_9_5_${RUN_ID}_robot_state.json"
if ! ft robot --format json state > "${STATE_JSON}" 2>"${STATE_JSON}.stderr"; then
  fail_now \
    "live_capture" \
    "ft_robot_state" \
    "robot_state_failed" \
    "ft_robot_command_failed" \
    "$(basename "${STATE_JSON}.stderr")" \
    "ft robot state failed"
fi

if ! jq -e '.ok == true' "${STATE_JSON}" >/dev/null; then
  fail_now \
    "live_capture" \
    "ft_robot_state_parse" \
    "robot_state_not_ok" \
    "robot_state_payload_invalid" \
    "$(basename "${STATE_JSON}")" \
    "ft robot state returned ok=false"
fi

PANE_ID="${FT_DOGFOOD_PANE_ID:-$(jq -r '.data.panes[0].pane_id // empty' "${STATE_JSON}")}"
if [[ -z "${PANE_ID}" ]]; then
  fail_now \
    "live_capture" \
    "pane_selection" \
    "no_active_pane" \
    "pane_id_unavailable" \
    "$(basename "${STATE_JSON}")" \
    "Set FT_DOGFOOD_PANE_ID or start an agent pane"
fi

CAPTURE_JSON="${LOG_DIR}/ft_nu4_3_9_5_${RUN_ID}_live_capture.json"
if ! ft robot --format json get-text "${PANE_ID}" --tail "${FT_DOGFOOD_TAIL:-400}" \
  > "${CAPTURE_JSON}" 2>"${CAPTURE_JSON}.stderr"; then
  fail_now \
    "live_capture" \
    "ft_robot_get_text" \
    "get_text_failed" \
    "robot_get_text_failed" \
    "$(basename "${CAPTURE_JSON}.stderr")" \
    "Failed to capture pane output"
fi

if ! jq -e '.ok == true' "${CAPTURE_JSON}" >/dev/null; then
  fail_now \
    "live_capture" \
    "ft_robot_get_text_parse" \
    "get_text_not_ok" \
    "robot_get_text_payload_invalid" \
    "$(basename "${CAPTURE_JSON}")" \
    "ft robot get-text returned ok=false"
fi

emit_log \
  "passed" \
  "live_capture" \
  "capture_detect_verify" \
  "live_capture_ready_for_fixture_extraction" \
  "none" \
  "$(basename "${CAPTURE_JSON}")" \
  "captured pane_id=${PANE_ID} for dogfood fixture extraction"

jq -cn \
  --arg run_id "${RUN_ID}" \
  --arg outcome "passed" \
  --arg pane_id "${PANE_ID}" \
  --arg capture_artifact "$(basename "${CAPTURE_JSON}")" \
  '{
    run_id: $run_id,
    outcome: $outcome,
    pane_id: ($pane_id | tonumber),
    capture_artifact: $capture_artifact
  }' > "${SUMMARY_FILE}"

emit_log \
  "passed" \
  "suite_complete" \
  "suite_complete" \
  "all_checks_passed" \
  "none" \
  "$(basename "${SUMMARY_FILE}")" \
  "dogfood fixture capture gate completed"
