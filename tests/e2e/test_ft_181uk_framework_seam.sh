#!/usr/bin/env bash
# E2E: Validate ft-181uk framework seam centralization contract.
#
# Scenarios:
#   1. Guard test target compiles and passes via rch-offloaded cargo test
#   2. Shared seam modules exist and are registered in frankenterm-core/lib.rs
#   3. Direct fastmcp/fastapi imports remain centralized across core+CLI surfaces
#   4. Seam modules continue to be the only allowed direct framework import sites
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/ft_181uk_framework_seam"
mkdir -p "${LOG_DIR}" "${ARTIFACT_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_181uk_framework_seam"
CORRELATION_ID="ft-181uk.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/framework_seam_contract_${RUN_ID}.jsonl"
SUMMARY_FILE="${ARTIFACT_DIR}/summary_${RUN_ID}.json"
CARGO_TARGET_DIR="${ROOT_DIR}/.target-ft-181uk-framework-seam"
export CARGO_TARGET_DIR

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
    --arg component "framework_seam_contract.e2e" \
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
  TOTAL=$((TOTAL + 1))
  if [ "${ok}" = "true" ]; then
    PASS=$((PASS + 1))
    emit_log "passed" "${name}" "scenario_end" "completed" "none" "${LOG_FILE}" ""
    echo "  PASS: ${name}"
  else
    FAIL=$((FAIL + 1))
    emit_log "failed" "${name}" "scenario_end" "${3:-assertion_failed}" "${4:-assertion_failed}" "${LOG_FILE}" "${5:-}"
    echo "  FAIL: ${name}"
  fi
}

allowed_framework_file() {
  local path="$1"
  case "${path}" in
    "crates/frankenterm-core/src/mcp_framework.rs"|\
    "crates/frankenterm-core/src/web_framework.rs"|\
    "crates/frankenterm-core/tests/framework_seam_guard.rs")
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

scan_framework_import_violations() {
  local output_file="$1"
  local search_dirs=()
  local raw
  local filtered
  local line
  local path

  : > "${output_file}"

  for dir in \
    "${ROOT_DIR}/crates/frankenterm-core/src" \
    "${ROOT_DIR}/crates/frankenterm-core/tests" \
    "${ROOT_DIR}/crates/frankenterm-core/benches" \
    "${ROOT_DIR}/crates/frankenterm/src" \
    "${ROOT_DIR}/crates/frankenterm/tests" \
    "${ROOT_DIR}/crates/frankenterm/benches"
  do
    if [ -d "${dir}" ]; then
      search_dirs+=("${dir}")
    fi
  done

  if [ "${#search_dirs[@]}" -eq 0 ]; then
    echo "no search directories found" > "${output_file}"
    return 1
  fi

  raw="$(rg -n 'fastmcp::|fastapi::' "${search_dirs[@]}" -g '*.rs' || true)"
  filtered=""
  while IFS= read -r line; do
    [ -z "${line}" ] && continue
    path="${line%%:*}"
    path="${path#"${ROOT_DIR}/"}"
    if ! allowed_framework_file "${path}"; then
      filtered+="${path}:${line#*:}"$'\n'
    fi
  done <<< "${raw}"

  printf "%s" "${filtered}" > "${output_file}"
  [ -z "${filtered}" ]
}

echo "=== Framework Seam Contract E2E (ft-181uk.2) ==="
emit_log "started" "e2e_suite" "script_init" "none" "none" "${LOG_FILE}" "RUN_ID=${RUN_ID}"

# -----------------------------------------------------------------------
# Scenario 1: Guard test target passes through rch
# -----------------------------------------------------------------------
echo ""
echo "--- Scenario 1: framework_seam_guard passes via rch ---"

if rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test -p frankenterm-core --test framework_seam_guard -- --nocapture \
    > "${ARTIFACT_DIR}/framework_seam_guard_stdout.log" \
    2> "${ARTIFACT_DIR}/framework_seam_guard_stderr.log"; then
  test_count=$(grep -c '^test ' "${ARTIFACT_DIR}/framework_seam_guard_stdout.log" || echo "0")
  record_result "framework_seam_guard_pass" "true"
  echo "    ${test_count} tests observed"
else
  record_result "framework_seam_guard_pass" "false" "command_failed" "cargo_test_failed" "see framework_seam_guard_stderr.log"
fi

# -----------------------------------------------------------------------
# Scenario 2: Shared seam modules exist and are registered
# -----------------------------------------------------------------------
echo ""
echo "--- Scenario 2: seam modules and lib wiring ---"

MODULE_OK="true"
: > "${ARTIFACT_DIR}/module_presence.log"
for file in \
  "${ROOT_DIR}/crates/frankenterm-core/src/mcp_framework.rs" \
  "${ROOT_DIR}/crates/frankenterm-core/src/web_framework.rs" \
  "${ROOT_DIR}/crates/frankenterm-core/tests/framework_seam_guard.rs"
do
  if [ ! -f "${file}" ]; then
    echo "missing file: ${file#${ROOT_DIR}/}" >> "${ARTIFACT_DIR}/module_presence.log"
    MODULE_OK="false"
  fi
done

LIB_FILE="${ROOT_DIR}/crates/frankenterm-core/src/lib.rs"
if ! grep -q 'pub mod mcp_framework;' "${LIB_FILE}"; then
  echo "missing lib.rs registration: pub mod mcp_framework;" >> "${ARTIFACT_DIR}/module_presence.log"
  MODULE_OK="false"
fi
if ! grep -q 'pub mod web_framework;' "${LIB_FILE}"; then
  echo "missing lib.rs registration: pub mod web_framework;" >> "${ARTIFACT_DIR}/module_presence.log"
  MODULE_OK="false"
fi

if [ "${MODULE_OK}" = "true" ]; then
  record_result "seam_module_registration" "true"
else
  record_result "seam_module_registration" "false" "precondition_failed" "missing_module" "see module_presence.log"
fi

# -----------------------------------------------------------------------
# Scenario 3: Import scan across core + CLI surfaces
# -----------------------------------------------------------------------
echo ""
echo "--- Scenario 3: direct framework imports stay centralized ---"

if scan_framework_import_violations "${ARTIFACT_DIR}/framework_import_violations.log"; then
  record_result "framework_import_scan" "true"
else
  record_result "framework_import_scan" "false" "invariant_violation" "framework_import_leak" "see framework_import_violations.log"
fi

# -----------------------------------------------------------------------
# Scenario 4: Seam modules still carry the direct framework references
# -----------------------------------------------------------------------
echo ""
echo "--- Scenario 4: seam modules remain the direct import choke points ---"

SEAM_OK="true"
: > "${ARTIFACT_DIR}/seam_module_contract.log"
if ! rg -q 'fastmcp::' "${ROOT_DIR}/crates/frankenterm-core/src/mcp_framework.rs"; then
  echo "mcp_framework.rs no longer references fastmcp::" >> "${ARTIFACT_DIR}/seam_module_contract.log"
  SEAM_OK="false"
fi
if ! rg -q 'fastapi::' "${ROOT_DIR}/crates/frankenterm-core/src/web_framework.rs"; then
  echo "web_framework.rs no longer references fastapi::" >> "${ARTIFACT_DIR}/seam_module_contract.log"
  SEAM_OK="false"
fi
if ! rg -q 'fastmcp::|fastapi::' "${ROOT_DIR}/crates/frankenterm-core/tests/framework_seam_guard.rs"; then
  echo "framework_seam_guard.rs no longer checks fastmcp::/fastapi:: patterns" >> "${ARTIFACT_DIR}/seam_module_contract.log"
  SEAM_OK="false"
fi

if [ "${SEAM_OK}" = "true" ]; then
  record_result "seam_modules_are_choke_points" "true"
else
  record_result "seam_modules_are_choke_points" "false" "precondition_failed" "seam_contract_drift" "see seam_module_contract.log"
fi

echo ""
echo "=== Summary ==="
echo "  Total: ${TOTAL}  Pass: ${PASS}  Fail: ${FAIL}"
echo "  Log: ${LOG_FILE}"

emit_log "$([ "${FAIL}" -eq 0 ] && echo passed || echo failed)" \
  "e2e_suite" "script_end" "completed" "none" "${LOG_FILE}" \
  "total=${TOTAL},pass=${PASS},fail=${FAIL}"

jq -cn \
  --arg test "ft_181uk_framework_seam" \
  --arg run_id "${RUN_ID}" \
  --arg log_file "${LOG_FILE}" \
  --argjson pass "${PASS}" \
  --argjson fail "${FAIL}" \
  --argjson total "${TOTAL}" \
  '{
    test: $test,
    run_id: $run_id,
    scenarios_pass: $pass,
    scenarios_fail: $fail,
    total: $total,
    log_file: $log_file
  }' > "${SUMMARY_FILE}"

[ "${FAIL}" -eq 0 ]
