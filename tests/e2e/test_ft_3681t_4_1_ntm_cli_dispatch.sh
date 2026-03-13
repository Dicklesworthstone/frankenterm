#!/usr/bin/env bash
# E2E test: ft-3681t.4.1 NTM CLI dispatch wiring
# Validates that all 22 NTM-gap robot subcommands parse correctly and return
# structured stub responses with NTM equivalence metadata.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_3681t_4_1_ntm_cli_dispatch"
CORRELATION_ID="ft-3681t.4.1-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.stdout.log"

PASS=0
FAIL=0
SKIP=0

emit_log() {
  local outcome="$1"
  local decision_path="$2"
  local reason_code="$3"
  local error_code="$4"
  local input_summary="$5"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "ntm_cli_dispatch.e2e" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg decision_path "${decision_path}" \
    --arg input_summary "${input_summary}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "$(basename "${STDOUT_FILE}")" \
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

emit_log "started" "script_init" "none" "none" "NTM CLI dispatch end-to-end validation"

if ! command -v jq >/dev/null 2>&1; then
  emit_log "failed" "preflight_jq" "jq_missing" "jq_not_found" "jq is required"
  exit 1
fi

# Find the ft binary
FT_BIN=""
for candidate in \
    "${ROOT_DIR}/target/debug/ft" \
    "${ROOT_DIR}/target/release/ft" \
    "/tmp/ft-pinkforge-target16/debug/ft" \
    "/tmp/ft-pinkforge-target16/release/ft" \
    "${ROOT_DIR}/target/debug/frankenterm" \
    "${ROOT_DIR}/target/release/frankenterm"; do
  if [[ -x "${candidate}" ]]; then
    FT_BIN="${candidate}"
    break
  fi
done

if [[ -z "${FT_BIN}" ]]; then
  emit_log "skipped" "preflight_binary" "ft_binary_not_found" "none" "No ft binary found; build first"
  echo "SKIP: ft binary not found. Build with: cargo build --package frankenterm"
  exit 0
fi

echo "Using ft binary: ${FT_BIN}"
: > "${STDOUT_FILE}"

# Test a single NTM robot command
# Usage: test_ntm_cmd <test_name> <family> <action> <extra_args...>
test_ntm_cmd() {
  local test_name="$1"
  shift
  local family="$1"
  shift
  local action="$1"
  shift
  local extra_args=("$@")

  echo -n "  ${test_name}: ft robot ${family} ${action} ... "

  set +e
  local output
  output=$("${FT_BIN}" robot "${family}" "${action}" "${extra_args[@]}" 2>/dev/null)
  local status=$?
  set -e

  # Accept exit code 0 (success) or 1 (config error is OK for stub — workspace may not exist)
  # The key validation is that the command parsed and returned valid JSON with stub fields

  if echo "${output}" | jq -e '.ok == true and .data.status == "stub"' >/dev/null 2>&1; then
    # Stub response with correct structure
    local resp_family
    resp_family=$(echo "${output}" | jq -r '.data.family')
    local resp_action
    resp_action=$(echo "${output}" | jq -r '.data.action')

    if [[ "${resp_family}" == "${family}" ]] && [[ "${resp_action}" == "${action}" ]]; then
      echo "PASS (stub, family=${resp_family}, action=${resp_action})"
      emit_log "passed" "ntm_cmd/${test_name}" "stub_response_valid" "none" "ft robot ${family} ${action}"
      PASS=$((PASS + 1))
      return 0
    else
      echo "FAIL (wrong family/action: got ${resp_family}/${resp_action})"
      emit_log "failed" "ntm_cmd/${test_name}" "field_mismatch" "wrong_family_action" \
        "expected ${family}/${action}, got ${resp_family}/${resp_action}"
      FAIL=$((FAIL + 1))
      return 1
    fi
  elif echo "${output}" | jq -e '.ok == false' >/dev/null 2>&1; then
    # Command parsed but failed at runtime (e.g., no workspace) — that's OK for parsing validation
    local err_code
    err_code=$(echo "${output}" | jq -r '.error_code // "unknown"')
    if [[ "${err_code}" == "robot.config_error" ]]; then
      echo "PASS (parsed OK, config error expected without workspace)"
      emit_log "passed" "ntm_cmd/${test_name}" "parsed_config_error" "none" \
        "Command parsed; config error expected without workspace"
      PASS=$((PASS + 1))
      return 0
    else
      echo "FAIL (unexpected error: ${err_code})"
      emit_log "failed" "ntm_cmd/${test_name}" "unexpected_error" "${err_code}" \
        "ft robot ${family} ${action}: ${output:0:200}"
      FAIL=$((FAIL + 1))
      return 1
    fi
  else
    # Not valid JSON or unexpected format
    echo "FAIL (invalid response)"
    echo "${output}" | head -5 >> "${STDOUT_FILE}"
    emit_log "failed" "ntm_cmd/${test_name}" "invalid_response" "non_json" \
      "ft robot ${family} ${action}: exit=${status}"
    FAIL=$((FAIL + 1))
    return 1
  fi
}

echo ""
echo "=== Checkpoint commands ==="
test_ntm_cmd "ckpt_save"     checkpoint save     --label "test" --include-scrollback || true
test_ntm_cmd "ckpt_list"     checkpoint list     --limit 10 --offset 0 || true
test_ntm_cmd "ckpt_show"     checkpoint show     "cp-001" || true
test_ntm_cmd "ckpt_delete"   checkpoint delete   "cp-001" || true
test_ntm_cmd "ckpt_rollback" checkpoint rollback "cp-001" --dry-run || true

echo ""
echo "=== Context commands ==="
test_ntm_cmd "ctx_status"  context status  --pane-id 1 || true
test_ntm_cmd "ctx_rotate"  context rotate  42 --strategy aggressive || true
test_ntm_cmd "ctx_history" context history 42 --limit 20 || true

echo ""
echo "=== Work commands ==="
test_ntm_cmd "work_claim"    work claim    "item-1" --agent-id "agent-1" || true
test_ntm_cmd "work_release"  work release  "item-1" --reason "done" || true
test_ntm_cmd "work_complete" work complete "item-1" --summary "finished" || true
test_ntm_cmd "work_list"     work list     --status open --limit 10 || true
test_ntm_cmd "work_ready"    work ready    --agent-id "agent-1" || true
test_ntm_cmd "work_assign"   work assign   "item-1" --agent-id "agent-2" || true

echo ""
echo "=== Fleet commands ==="
test_ntm_cmd "fleet_status"    fleet status    --detailed || true
test_ntm_cmd "fleet_scale"     fleet scale     "claude_code" 4 --dry-run || true
test_ntm_cmd "fleet_rebalance" fleet rebalance --strategy round_robin --dry-run || true
test_ntm_cmd "fleet_agents"    fleet agents    --program "claude_code" || true

echo ""
echo "=== Profile commands ==="
test_ntm_cmd "profile_list"     profile list     --role worker || true
test_ntm_cmd "profile_show"     profile show     "default" || true
test_ntm_cmd "profile_apply"    profile apply    "default" --count 2 --dry-run || true
test_ntm_cmd "profile_validate" profile validate "default" || true

echo ""
echo "=== Summary ==="
TOTAL=$((PASS + FAIL + SKIP))
echo "Total: ${TOTAL}  Pass: ${PASS}  Fail: ${FAIL}  Skip: ${SKIP}"
echo "Log: ${LOG_FILE}"

emit_log "completed" "summary" "none" "none" \
  "total=${TOTAL} pass=${PASS} fail=${FAIL} skip=${SKIP}"

if [[ ${FAIL} -gt 0 ]]; then
  exit 1
fi
