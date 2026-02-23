#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_so7qh_1_repeated_failure_loop"
CORRELATION_ID="ft-so7qh.1-${RUN_ID}"
PANE_ID=1
COMMAND_INPUT="cargo test"
COMMAND_HASH="$(printf '%s' "${COMMAND_INPUT}" | cksum | awk '{print $1}')"
LOG_FILE="${LOG_DIR}/ft_so7qh_1_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_so7qh_1_${RUN_ID}.stdout.log"

emit_log() {
  local outcome="$1"
  local decision_path="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local input_summary="$6"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "trauma_guard.e2e" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg pane_id "${PANE_ID}" \
    --arg command_hash "${COMMAND_HASH}" \
    --arg decision_path "${decision_path}" \
    --arg input_summary "${input_summary}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg decision_reason "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path}" \
    '{
      timestamp: $timestamp,
      component: $component,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      pane_id: ($pane_id | tonumber),
      command_hash: ($command_hash | tonumber),
      decision_path: $decision_path,
      input_summary: $input_summary,
      outcome: $outcome,
      reason_code: $reason_code,
      decision_reason: $decision_reason,
      error_code: $error_code,
      artifact_path: $artifact_path
    }' >> "${LOG_FILE}"
}

emit_log \
  "started" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "command=${COMMAND_INPUT} threshold=3 signature=core.codex:error_loop"

if ! command -v rch >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "preflight_rch" \
    "rch_missing" \
    "rch_not_found" \
    "$(basename "${LOG_FILE}")" \
    "rch must be installed for offloaded cargo execution"
  exit 1
fi

TEST_CMD=(
  rch exec --
  env CARGO_TARGET_DIR=target-rch-ft-so7qh-1
  cargo test -p frankenterm-core e2e_repeated_failure_loop_decision_is_deterministic -- --nocapture
)

emit_log \
  "running" \
  "cargo_test" \
  "none" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Executing: ${TEST_CMD[*]}"

set +e
(
  cd "${ROOT_DIR}"
  "${TEST_CMD[@]}"
) 2>&1 | tee "${STDOUT_FILE}"
status=${PIPESTATUS[0]}
set -e

if [[ ${status} -ne 0 ]]; then
  emit_log \
    "failed" \
    "cargo_test" \
    "test_failure" \
    "cargo_test_failed" \
    "$(basename "${STDOUT_FILE}")" \
    "exit=${status}"
  exit "${status}"
fi

if ! grep -q "e2e_repeated_failure_loop_decision_is_deterministic ... ok" "${STDOUT_FILE}"; then
  emit_log \
    "failed" \
    "assertion_check" \
    "unexpected_test_output" \
    "missing_success_marker" \
    "$(basename "${STDOUT_FILE}")" \
    "Expected deterministic trauma-loop test success marker"
  exit 1
fi

emit_log \
  "passed" \
  "record_command_result->signature_window->trailing_repeat_count" \
  "recurring_failure_loop" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Deterministic intervention threshold validated"

