#!/usr/bin/env bash
set -euo pipefail

# ft-3681t.1.5.1 — Traceability matrix validation e2e harness.
#
# Validates:
# 1. Matrix artifact exists and passes structural jq checks.
# 2. Rust integration tests validate schema and anchor paths.
# 3. Failure injection path is exercised (unmapped high-gap matrix should fail).

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_3681t_1_5_1_traceability_matrix"
CORRELATION_ID="ft-3681t.1.5.1-${RUN_ID}"
LOG_FILE="${LOG_DIR}/traceability_matrix_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/traceability_matrix_${RUN_ID}.stdout.log"
REPORT_OK="${LOG_DIR}/traceability_matrix_${RUN_ID}.report.ok.json"
REPORT_FAIL="${LOG_DIR}/traceability_matrix_${RUN_ID}.report.fail.json"
REPORT_INCOMPLETE="${LOG_DIR}/traceability_matrix_${RUN_ID}.report.incomplete.json"
RCH_PROBE_FILE="${LOG_DIR}/traceability_matrix_${RUN_ID}.rch_probe.json"
BAD_MATRIX_FILE="${LOG_DIR}/traceability_matrix_${RUN_ID}.bad_matrix.json"
TARGET_DIR="${CARGO_TARGET_DIR:-${ROOT_DIR}/.cargo-ft-3681t-1-5-1/target}"
BUILD_JOBS="${CARGO_BUILD_JOBS:-1}"

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0
INCOMPLETE_COUNT=0
RCH_PREFLIGHT_DONE=0
RCH_REMOTE_READY=0

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "3681t_1_5_1_traceability_matrix"
ensure_rch_ready

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
    --arg component "traceability_matrix.e2e" \
    --arg scenario_id "${SCENARIO_ID}" \
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
    }' | tee -a "${LOG_FILE}" >/dev/null
}

pass() { PASS_COUNT=$((PASS_COUNT + 1)); }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); }
skip() { SKIP_COUNT=$((SKIP_COUNT + 1)); }
incomplete() { INCOMPLETE_COUNT=$((INCOMPLETE_COUNT + 1)); }

rch_preflight() {
  if [ "${RCH_PREFLIGHT_DONE}" -eq 1 ]; then
    [ "${RCH_REMOTE_READY}" -eq 1 ]
    return
  fi

  RCH_PREFLIGHT_DONE=1

  if ! command -v rch >/dev/null 2>&1; then
    emit_log "SKIP" "rch_preflight" "RCH_NOT_FOUND" "E-TM-005" "" "rch required for remote cargo offload"
    echo "  SKIP: rch not found; remote-only cargo scenarios cannot run"
    skip
    incomplete
    RCH_REMOTE_READY=0
    return 1
  fi

  if ! rch workers probe --json --all > "${RCH_PROBE_FILE}" 2>> "${STDOUT_FILE}"; then
    emit_log "SKIP" "rch_preflight" "RCH_PROBE_FAILED" "E-TM-006" "${RCH_PROBE_FILE}" "worker probe command failed"
    echo "  SKIP: rch worker probe failed; refusing cargo execution"
    skip
    incomplete
    RCH_REMOTE_READY=0
    return 1
  fi

  if jq -e '
      .success == true
      and (.data | type == "array")
      and any(.data[]?; (.status // "") | ascii_downcase | test("^(healthy|ok|ready|reachable|success)$"))
    ' "${RCH_PROBE_FILE}" >/dev/null; then
    emit_log "PASS" "rch_preflight" "RCH_WORKERS_READY" "" "${RCH_PROBE_FILE}" "at least one worker reachable"
    echo "  PASS: at least one rch worker is reachable"
    pass
    RCH_REMOTE_READY=1
    return 0
  fi

  emit_log "SKIP" "rch_preflight" "RCH_NO_REACHABLE_WORKERS" "E-TM-007" "${RCH_PROBE_FILE}" "no reachable workers in probe output"
  echo "  SKIP: no reachable rch workers; refusing cargo execution"
  skip
  incomplete
  RCH_REMOTE_READY=0
  return 1
}

echo "=== ft-3681t.1.5.1: Traceability Matrix E2E ==="
echo "Run ID:         ${RUN_ID}"
echo "Correlation ID: ${CORRELATION_ID}"
echo "Log file:       ${LOG_FILE}"
echo ""

MATRIX_FILE="${ROOT_DIR}/docs/design/ntm-fcp-traceability-matrix.json"
ISSUES_JSONL="${ROOT_DIR}/.beads/issues.jsonl"

# ---------------------------------------------------------------------------
# Scenario 1: Matrix artifact structure checks
# ---------------------------------------------------------------------------
echo "--- Scenario 1: Matrix structure ---"

if [ ! -f "${MATRIX_FILE}" ]; then
  emit_log "FAIL" "matrix_exists" "FILE_NOT_FOUND" "E-TM-001" "${MATRIX_FILE}" "matrix file missing"
  echo "  FAIL: ${MATRIX_FILE} not found"
  fail
else
  emit_log "PASS" "matrix_exists" "FILE_FOUND" "" "${MATRIX_FILE}" "matrix file exists"
  echo "  PASS: matrix file exists"
  pass

  if jq -e '.artifact == "ntm-fcp-traceability-matrix"' "${MATRIX_FILE}" >/dev/null; then
    emit_log "PASS" "artifact_tag" "ARTIFACT_TAG_OK" "" "${MATRIX_FILE}" "artifact tag"
    echo "  PASS: artifact tag matches"
    pass
  else
    emit_log "FAIL" "artifact_tag" "ARTIFACT_TAG_BAD" "E-TM-002" "${MATRIX_FILE}" "artifact tag"
    echo "  FAIL: artifact tag mismatch"
    fail
  fi

  if jq -e '.entries | type == "array" and length > 0' "${MATRIX_FILE}" >/dev/null; then
    emit_log "PASS" "entries_array" "ENTRIES_OK" "" "${MATRIX_FILE}" "entries array exists"
    echo "  PASS: entries array exists and is non-empty"
    pass
  else
    emit_log "FAIL" "entries_array" "ENTRIES_INVALID" "E-TM-003" "${MATRIX_FILE}" "entries array invalid"
    echo "  FAIL: entries array missing/empty"
    fail
  fi

  if jq -e '
      (.required_capability_ids | type == "array" and length > 0)
      and ((.required_capability_ids - [.entries[] | .capability_id]) | length == 0)
    ' "${MATRIX_FILE}" >/dev/null; then
    emit_log "PASS" "required_capability_ids" "REQUIRED_CAPABILITIES_OK" "" "${MATRIX_FILE}" "required capability ids are present and mapped"
    echo "  PASS: required capability ids are present and mapped"
    pass
  else
    emit_log "FAIL" "required_capability_ids" "REQUIRED_CAPABILITIES_INVALID" "E-TM-003A" "${MATRIX_FILE}" "required capability ids missing or unmapped"
    echo "  FAIL: required capability ids missing or unmapped"
    fail
  fi

  if jq -e '[.entries[] | select((.gap_severity=="high" or .gap_severity=="medium") and (.mapped_bead_ids | length == 0))] | length == 0' "${MATRIX_FILE}" >/dev/null; then
    emit_log "PASS" "gap_mapping" "NO_UNMAPPED_HIGH_MEDIUM" "" "${MATRIX_FILE}" "high/medium gaps mapped"
    echo "  PASS: no unmapped high/medium gaps"
    pass
  else
    emit_log "FAIL" "gap_mapping" "UNMAPPED_HIGH_MEDIUM" "E-TM-004" "${MATRIX_FILE}" "unmapped high/medium gaps"
    echo "  FAIL: found unmapped high/medium gaps"
    fail
  fi

  if [ ! -f "${ISSUES_JSONL}" ]; then
    emit_log "FAIL" "bead_reference_existence" "ISSUES_JSONL_MISSING" "E-TM-004A" "${ISSUES_JSONL}" "issues export missing"
    echo "  FAIL: ${ISSUES_JSONL} not found"
    fail
  elif jq -s -e '
      .[0:-1] as $issues
      | .[-1] as $matrix
      | ($issues | map(.id) | map({key: ., value: true}) | from_entries) as $issue_ids
      | [ $matrix.entries[] | .mapped_bead_ids[]? | select($issue_ids[.] != true) ]
      | length == 0
    ' "${ISSUES_JSONL}" "${MATRIX_FILE}" >/dev/null; then
    emit_log "PASS" "bead_reference_existence" "BEAD_REFERENCES_OK" "" "${MATRIX_FILE}" "mapped bead ids resolve in issues export"
    echo "  PASS: all mapped bead ids resolve in .beads/issues.jsonl"
    pass
  else
    emit_log "FAIL" "bead_reference_existence" "BEAD_REFERENCE_MISSING" "E-TM-004B" "${MATRIX_FILE}" "missing mapped bead ids in issues export"
    echo "  FAIL: traceability matrix references missing bead ids"
    fail
  fi
fi

# ---------------------------------------------------------------------------
# Scenario 2: Rust matrix validation tests
# ---------------------------------------------------------------------------
echo ""
echo "--- Scenario 2: Rust validation tests ---"

if ! rch_preflight; then
  emit_log "SKIP" "rust_tests" "RCH_REMOTE_UNAVAILABLE" "E-TM-008" "${RCH_PROBE_FILE}" "remote cargo validation blocked by preflight"
  echo "  SKIP: remote cargo validation blocked by rch preflight"
  skip
  incomplete
else
  echo "  Running: rch exec -- env CARGO_TARGET_DIR=${TARGET_DIR} CARGO_BUILD_JOBS=${BUILD_JOBS} cargo test -p frankenterm-core --test ntm_fcp_traceability_matrix --no-default-features -- --nocapture"
  if rch exec -- env CARGO_TARGET_DIR="${TARGET_DIR}" CARGO_BUILD_JOBS="${BUILD_JOBS}" cargo test -p frankenterm-core --test ntm_fcp_traceability_matrix --no-default-features -- --nocapture >> "${STDOUT_FILE}" 2>&1; then
    emit_log "PASS" "rust_tests" "TESTS_PASS" "" "${STDOUT_FILE}" "matrix integration tests"
    echo "  PASS: matrix integration tests succeeded"
    pass
  else
    exit_code=$?
    emit_log "FAIL" "rust_tests" "TESTS_FAIL" "E-TM-010" "${STDOUT_FILE}" "exit ${exit_code}"
    echo "  FAIL: matrix integration tests failed (exit ${exit_code})"
    fail
  fi
fi

# ---------------------------------------------------------------------------
# Scenario 3: Failure injection (invalid matrix should fail validation test)
# ---------------------------------------------------------------------------
echo ""
echo "--- Scenario 3: Failure injection path ---"

if [ ! -f "${MATRIX_FILE}" ]; then
  emit_log "FAIL" "failure_injection_setup" "MATRIX_MISSING" "E-TM-021" "${MATRIX_FILE}" "cannot synthesize invalid matrix"
  echo "  FAIL: matrix file missing; cannot execute failure-injection scenario"
  fail
elif ! rch_preflight; then
  emit_log "SKIP" "failure_injection" "RCH_REMOTE_UNAVAILABLE" "E-TM-022" "${RCH_PROBE_FILE}" "failure injection blocked by rch preflight"
  echo "  SKIP: remote failure-injection validation blocked by rch preflight"
  skip
  incomplete
else
  jq '(.entries[0].status = "gap") | (.entries[0].gap_severity = "high") | (.entries[0].mapped_bead_ids = [])' \
    "${MATRIX_FILE}" > "${BAD_MATRIX_FILE}"

  echo "  Running failure-injection command (expected failure): rch exec -- env FT_TRACEABILITY_MATRIX_PATH=${BAD_MATRIX_FILE} CARGO_TARGET_DIR=${TARGET_DIR} CARGO_BUILD_JOBS=${BUILD_JOBS} cargo test -p frankenterm-core --test ntm_fcp_traceability_matrix --no-default-features traceability_matrix_schema_is_valid -- --exact --nocapture"
  if rch exec -- env FT_TRACEABILITY_MATRIX_PATH="${BAD_MATRIX_FILE}" CARGO_TARGET_DIR="${TARGET_DIR}" CARGO_BUILD_JOBS="${BUILD_JOBS}" cargo test -p frankenterm-core --test ntm_fcp_traceability_matrix --no-default-features traceability_matrix_schema_is_valid -- --exact --nocapture >> "${STDOUT_FILE}" 2>&1; then
    emit_log "FAIL" "failure_injection" "UNEXPECTED_PASS" "E-TM-020" "${STDOUT_FILE}" "invalid matrix unexpectedly validated"
    echo "  FAIL: invalid matrix unexpectedly passed validation"
    fail
  else
    emit_log "PASS" "failure_injection" "EXPECTED_FAILURE_OBSERVED" "" "${STDOUT_FILE}" "invalid matrix rejected"
    echo "  PASS: invalid matrix correctly rejected"
    pass
  fi
fi

TOTAL_CHECKS=$((PASS_COUNT + FAIL_COUNT + SKIP_COUNT))

if [ "${FAIL_COUNT}" -eq 0 ] && [ "${INCOMPLETE_COUNT}" -eq 0 ]; then
  jq -n \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg log_file "${LOG_FILE}" \
    --arg stdout_file "${STDOUT_FILE}" \
    --arg matrix_file "${MATRIX_FILE}" \
    --arg rch_probe_file "${RCH_PROBE_FILE}" \
    --arg bad_matrix_file "${BAD_MATRIX_FILE}" \
    --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --argjson total "${TOTAL_CHECKS}" \
    --argjson pass "${PASS_COUNT}" \
    --argjson fail "${FAIL_COUNT}" \
    --argjson skip "${SKIP_COUNT}" \
    --argjson incomplete "${INCOMPLETE_COUNT}" \
    '{
      ok: true,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      generated_at: $generated_at,
      summary: {total_checks: $total, pass: $pass, fail: $fail, skip: $skip, incomplete: $incomplete},
      artifacts: {
        matrix_file: $matrix_file,
        jsonl_log: $log_file,
        stdout_log: $stdout_file,
        rch_probe_file: $rch_probe_file,
        bad_matrix_file: $bad_matrix_file
      }
    }' | tee "${REPORT_OK}" >/dev/null

  echo ""
  echo "PASS: ${PASS_COUNT}/${TOTAL_CHECKS} checks passed, ${FAIL_COUNT} failed, ${SKIP_COUNT} skipped"
  echo "Report: ${REPORT_OK}"
  exit 0
fi

if [ "${FAIL_COUNT}" -eq 0 ] && [ "${INCOMPLETE_COUNT}" -gt 0 ]; then
  jq -n \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg log_file "${LOG_FILE}" \
    --arg stdout_file "${STDOUT_FILE}" \
    --arg matrix_file "${MATRIX_FILE}" \
    --arg rch_probe_file "${RCH_PROBE_FILE}" \
    --arg bad_matrix_file "${BAD_MATRIX_FILE}" \
    --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --argjson total "${TOTAL_CHECKS}" \
    --argjson pass "${PASS_COUNT}" \
    --argjson fail "${FAIL_COUNT}" \
    --argjson skip "${SKIP_COUNT}" \
    --argjson incomplete "${INCOMPLETE_COUNT}" \
    '{
      ok: false,
      incomplete: true,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      generated_at: $generated_at,
      summary: {total_checks: $total, pass: $pass, fail: $fail, skip: $skip, incomplete: $incomplete},
      artifacts: {
        matrix_file: $matrix_file,
        jsonl_log: $log_file,
        stdout_log: $stdout_file,
        rch_probe_file: $rch_probe_file,
        bad_matrix_file: $bad_matrix_file
      }
    }' | tee "${REPORT_INCOMPLETE}" >/dev/null

  echo ""
  echo "INCOMPLETE: ${PASS_COUNT}/${TOTAL_CHECKS} checks passed, ${FAIL_COUNT} failed, ${SKIP_COUNT} skipped"
  echo "Report: ${REPORT_INCOMPLETE}"
  exit 2
fi

jq -n \
  --arg scenario_id "${SCENARIO_ID}" \
  --arg correlation_id "${CORRELATION_ID}" \
  --arg log_file "${LOG_FILE}" \
  --arg stdout_file "${STDOUT_FILE}" \
  --arg matrix_file "${MATRIX_FILE}" \
  --arg rch_probe_file "${RCH_PROBE_FILE}" \
  --arg bad_matrix_file "${BAD_MATRIX_FILE}" \
  --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
  --argjson total "${TOTAL_CHECKS}" \
  --argjson pass "${PASS_COUNT}" \
  --argjson fail "${FAIL_COUNT}" \
  --argjson skip "${SKIP_COUNT}" \
  --argjson incomplete "${INCOMPLETE_COUNT}" \
  '{
    ok: false,
    scenario_id: $scenario_id,
    correlation_id: $correlation_id,
    generated_at: $generated_at,
    summary: {total_checks: $total, pass: $pass, fail: $fail, skip: $skip, incomplete: $incomplete},
    artifacts: {
      matrix_file: $matrix_file,
      jsonl_log: $log_file,
      stdout_log: $stdout_file,
      rch_probe_file: $rch_probe_file,
      bad_matrix_file: $bad_matrix_file
    }
  }' | tee "${REPORT_FAIL}" >/dev/null

echo ""
echo "FAIL: ${PASS_COUNT}/${TOTAL_CHECKS} checks passed, ${FAIL_COUNT} failed, ${SKIP_COUNT} skipped"
echo "Report: ${REPORT_FAIL}"
exit 1
