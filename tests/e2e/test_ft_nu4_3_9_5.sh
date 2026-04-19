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
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/ft-$(whoami)-target}"

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "nu4_3_9_5"
ensure_rch_ready

resolve_wezterm_bin() {
  local candidate=""

  for candidate in "${FT_WEZTERM_CLI:-}" "${WEZTERM_BIN:-}"; do
    if [[ -z "${candidate}" ]]; then
      continue
    fi
    if [[ -x "${candidate}" ]]; then
      printf '%s\n' "${candidate}"
      return 0
    fi
    if command -v "${candidate}" >/dev/null 2>&1; then
      command -v "${candidate}"
      return 0
    fi
  done

  if command -v wezterm >/dev/null 2>&1; then
    command -v wezterm
    return 0
  fi

  for candidate in \
    "/Applications/WezTerm.app/Contents/MacOS/wezterm" \
    "${HOME}/Applications/WezTerm.app/Contents/MacOS/wezterm" \
    "${HOME}/.local/bin/wezterm"; do
    if [[ -x "${candidate}" ]]; then
      printf '%s\n' "${candidate}"
      return 0
    fi
  done

  return 1
}

run_wezterm_cli() {
  local wezterm_bin="$1"
  shift

  if [[ -z "${TIMEOUT_BIN:-}" ]]; then
    resolve_timeout_bin
  fi

  if [[ -n "${TIMEOUT_BIN:-}" ]]; then
    "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 \
      "${FT_DOGFOOD_WEZTERM_TIMEOUT_SECS:-15}" \
      "${wezterm_bin}" cli --no-auto-start "$@"
    return
  fi

  "${wezterm_bin}" cli --no-auto-start "$@"
}

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

if ! WEZTERM_BIN="$(resolve_wezterm_bin)"; then
  fail_now \
    "live_capture" \
    "preflight_wezterm" \
    "wezterm_binary_missing" \
    "wezterm_not_found" \
    "$(basename "${LOG_FILE}")" \
    "Install or expose wezterm, or set FT_WEZTERM_CLI/WEZTERM_BIN before running live dogfood capture"
fi

WEZTERM_LIST_LOG="${LOG_DIR}/ft_nu4_3_9_5_${RUN_ID}_wezterm_list.json"
if ! run_wezterm_cli "${WEZTERM_BIN}" list --format json \
  > "${WEZTERM_LIST_LOG}" 2>"${WEZTERM_LIST_LOG}.stderr"; then
  fail_now \
    "live_capture" \
    "wezterm_cli_list" \
    "wezterm_mux_unreachable" \
    "wezterm_cli_failed" \
    "$(basename "${WEZTERM_LIST_LOG}.stderr")" \
    "Ensure the target mux is running and reachable via FT_WEZTERM_CLI/WEZTERM_BIN or WEZTERM_UNIX_SOCKET"
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

CAPTURE_TEXT="$(jq -r '.data.text // ""' "${CAPTURE_JSON}")"
if [[ -z "${CAPTURE_TEXT}" ]]; then
  fail_now \
    "live_capture" \
    "capture_text_extract" \
    "captured_text_empty" \
    "capture_text_missing" \
    "$(basename "${CAPTURE_JSON}")" \
    "ft robot get-text returned an empty text payload"
fi

emit_log \
  "passed" \
  "live_capture" \
  "capture_only" \
  "live_capture_collected" \
  "none" \
  "$(basename "${CAPTURE_JSON}")" \
  "captured pane_id=${PANE_ID} for dogfood fixture extraction"

RULES_JSON="${LOG_DIR}/ft_nu4_3_9_5_${RUN_ID}_rules_test.json"
if ! ft robot --format json rules test "${CAPTURE_TEXT}" \
  > "${RULES_JSON}" 2>"${RULES_JSON}.stderr"; then
  fail_now \
    "live_detection" \
    "ft_robot_rules_test" \
    "rules_test_failed" \
    "robot_rules_test_failed" \
    "$(basename "${RULES_JSON}.stderr")" \
    "Failed to evaluate live capture against the detection rules"
fi

if ! jq -e '.ok == true' "${RULES_JSON}" >/dev/null; then
  fail_now \
    "live_detection" \
    "ft_robot_rules_test_parse" \
    "rules_test_not_ok" \
    "robot_rules_test_payload_invalid" \
    "$(basename "${RULES_JSON}")" \
    "ft robot rules test returned ok=false"
fi

MATCH_COUNT="$(jq -r '.data.match_count // 0' "${RULES_JSON}")"
if [[ "${MATCH_COUNT}" == "0" ]]; then
  fail_now \
    "live_detection" \
    "rule_match_filter" \
    "no_rules_matched_live_capture" \
    "no_detection_matches" \
    "$(basename "${RULES_JSON}")" \
    "No detection rules matched the captured pane tail"
fi

MATCHED_RULE_ID=""
MATCHED_WORKFLOW=""
MATCHED_AGENT_TYPE=""
MATCHED_EVENT_TYPE=""
MATCHED_SEVERITY=""
MATCHED_RULE_DETAIL_JSON="${LOG_DIR}/ft_nu4_3_9_5_${RUN_ID}_matched_rule.json"

while IFS= read -r rule_id; do
  [[ -z "${rule_id}" ]] && continue

  if ! ft robot --format json rules show "${rule_id}" \
    > "${MATCHED_RULE_DETAIL_JSON}" 2>"${MATCHED_RULE_DETAIL_JSON}.stderr"; then
    continue
  fi

  if ! jq -e '.ok == true' "${MATCHED_RULE_DETAIL_JSON}" >/dev/null 2>&1; then
    continue
  fi

  workflow="$(jq -r '.data.workflow // empty' "${MATCHED_RULE_DETAIL_JSON}")"
  case "${workflow}" in
    handle_compaction|handle_usage_limits|handle_claude_code_limits|handle_auth_required)
      MATCHED_RULE_ID="${rule_id}"
      MATCHED_WORKFLOW="${workflow}"
      MATCHED_AGENT_TYPE="$(jq -r '.data.agent_type // "unknown"' "${MATCHED_RULE_DETAIL_JSON}")"
      MATCHED_EVENT_TYPE="$(jq -r '.data.event_type // "unknown"' "${MATCHED_RULE_DETAIL_JSON}")"
      MATCHED_SEVERITY="$(jq -r '.data.severity // "unknown"' "${MATCHED_RULE_DETAIL_JSON}")"
      break
      ;;
  esac
done < <(jq -r '.data.matches[].rule_id' "${RULES_JSON}")

if [[ -z "${MATCHED_RULE_ID}" ]]; then
  fail_now \
    "live_detection" \
    "workflow_resolution" \
    "no_dogfood_workflow_match" \
    "workflow_not_verified" \
    "$(basename "${RULES_JSON}")" \
    "Rules matched, but none resolved to handle_compaction, handle_usage_limits, handle_claude_code_limits, or handle_auth_required"
fi

DETECTION_SUMMARY="pane_id=${PANE_ID} rule_id=${MATCHED_RULE_ID} workflow=${MATCHED_WORKFLOW} agent_type=${MATCHED_AGENT_TYPE} event_type=${MATCHED_EVENT_TYPE} severity=${MATCHED_SEVERITY}"

emit_log \
  "passed" \
  "live_detection" \
  "capture_detect_verify" \
  "live_detection_workflow_verified" \
  "none" \
  "$(basename "${MATCHED_RULE_DETAIL_JSON}")" \
  "${DETECTION_SUMMARY}"

jq -cn \
  --arg run_id "${RUN_ID}" \
  --arg outcome "passed" \
  --arg pane_id "${PANE_ID}" \
  --arg capture_artifact "$(basename "${CAPTURE_JSON}")" \
  --arg rules_artifact "$(basename "${RULES_JSON}")" \
  --arg matched_rule_artifact "$(basename "${MATCHED_RULE_DETAIL_JSON}")" \
  --arg matched_rule_id "${MATCHED_RULE_ID}" \
  --arg matched_workflow "${MATCHED_WORKFLOW}" \
  --arg matched_agent_type "${MATCHED_AGENT_TYPE}" \
  --arg matched_event_type "${MATCHED_EVENT_TYPE}" \
  '{
    run_id: $run_id,
    outcome: $outcome,
    pane_id: ($pane_id | tonumber),
    capture_artifact: $capture_artifact,
    rules_artifact: $rules_artifact,
    matched_rule_artifact: $matched_rule_artifact,
    matched_rule_id: $matched_rule_id,
    matched_workflow: $matched_workflow,
    matched_agent_type: $matched_agent_type,
    matched_event_type: $matched_event_type
  }' > "${SUMMARY_FILE}"

emit_log \
  "passed" \
  "suite_complete" \
  "suite_complete" \
  "all_checks_passed" \
  "none" \
  "$(basename "${SUMMARY_FILE}")" \
  "dogfood fixture capture gate completed"
