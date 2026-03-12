#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"
RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_5_4_contract_verification"
CORRELATION_ID="ft-e34d9.10.5.4-${RUN_ID}"
LOG_FILE="${LOG_DIR}/contract_verification_${RUN_ID}.jsonl"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

emit_log() {
  local outcome="$1" decision_path="$2" reason_code="$3" error_code="$4" input_summary="$5"
  local ts; ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  jq -cn \
    --arg ts "${ts}" --arg component "contract_verification.e2e" \
    --arg sid "${SCENARIO_ID}" --arg cid "${CORRELATION_ID}" \
    --arg dp "${decision_path}" --arg is "${input_summary}" \
    --arg oc "${outcome}" --arg rc "${reason_code}" --arg ec "${error_code}" \
    '{timestamp:$ts,component:$component,scenario_id:$sid,correlation_id:$cid,
      decision_path:$dp,input_summary:$is,outcome:$oc,reason_code:$rc,error_code:$ec}' \
    >> "${LOG_FILE}"
}

echo "=== Core↔vendored async contract verification (ft-e34d9.10.5.4) ==="
echo "Run ID: ${RUN_ID}"
echo "Log:    ${LOG_FILE_REL}"
echo ""

PASS=0; FAIL=0
TEST_FILE="${ROOT_DIR}/crates/frankenterm-core/tests/vendored_async_contract_verification.rs"
CONTRACT_FILE="${ROOT_DIR}/crates/frankenterm-core/src/vendored_async_contracts.rs"

# S1: Verification test file exists
echo -n "S1: Contract verification test file exists... "
if [ -f "${TEST_FILE}" ]; then
  echo "PASS"; emit_log "pass" "test_file" "exists" "" ""; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "test_file" "missing" "E_FILE" ""; FAIL=$((FAIL+1))
fi

# S2: Contract definition file exists
echo -n "S2: Contract definition file exists... "
if [ -f "${CONTRACT_FILE}" ]; then
  echo "PASS"; emit_log "pass" "contract_file" "exists" "" ""; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "contract_file" "missing" "E_FILE" ""; FAIL=$((FAIL+1))
fi

# S3: Test count >= 25
echo -n "S3: Verification test count... "
TEST_COUNT=$(grep -c '#\[test\]' "${TEST_FILE}" || true)
if [ "${TEST_COUNT}" -ge 25 ]; then
  echo "PASS (${TEST_COUNT} tests)"; emit_log "pass" "test_count" "sufficient" "" "count=${TEST_COUNT}"; PASS=$((PASS+1))
else
  echo "FAIL (${TEST_COUNT})"; emit_log "fail" "test_count" "insufficient" "E_TESTS" "count=${TEST_COUNT}"; FAIL=$((FAIL+1))
fi

# S4: All 7 contract categories tested
echo -n "S4: All contract categories tested... "
CATS=$(grep -c 'ContractCategory::' "${TEST_FILE}" || true)
if [ "${CATS}" -ge 14 ]; then
  echo "PASS (${CATS} category refs)"; emit_log "pass" "categories" "all_covered" "" "refs=${CATS}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "categories" "incomplete" "E_CAT" "refs=${CATS}"; FAIL=$((FAIL+1))
fi

# S5: Static analysis tests present
echo -n "S5: Static analysis drift detection... "
STATIC=$(grep -c 'read_to_string\|env!("CARGO_MANIFEST_DIR")' "${TEST_FILE}" || true)
if [ "${STATIC}" -ge 10 ]; then
  echo "PASS (${STATIC} static analysis refs)"; emit_log "pass" "static_analysis" "present" "" "refs=${STATIC}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "static_analysis" "insufficient" "E_STATIC" "refs=${STATIC}"; FAIL=$((FAIL+1))
fi

# S6: Serde roundtrip tests present
echo -n "S6: Serde roundtrip tests... "
SERDE=$(grep -c 'serde_roundtrip\|serde_json::to_string\|serde_json::from_str' "${TEST_FILE}" || true)
if [ "${SERDE}" -ge 6 ]; then
  echo "PASS (${SERDE} serde refs)"; emit_log "pass" "serde_tests" "present" "" "refs=${SERDE}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "serde_tests" "insufficient" "E_SERDE" "refs=${SERDE}"; FAIL=$((FAIL+1))
fi

# S7: Contract evidence infrastructure tested
echo -n "S7: Contract evidence infrastructure... "
EVIDENCE=$(grep -c 'ContractCompliance::from_evidence\|ContractEvidence' "${TEST_FILE}" || true)
if [ "${EVIDENCE}" -ge 8 ]; then
  echo "PASS (${EVIDENCE} evidence refs)"; emit_log "pass" "evidence_infra" "present" "" "refs=${EVIDENCE}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "evidence_infra" "insufficient" "E_EVIDENCE" "refs=${EVIDENCE}"; FAIL=$((FAIL+1))
fi

# S8: Regression anchor tests present
echo -n "S8: Regression anchors... "
REGR=$(grep -c 'regression_contract_count\|regression_category_distribution' "${TEST_FILE}" || true)
if [ "${REGR}" -ge 2 ]; then
  echo "PASS (${REGR} regression tests)"; emit_log "pass" "regression_anchors" "present" "" "refs=${REGR}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "regression_anchors" "missing" "E_REGR" "refs=${REGR}"; FAIL=$((FAIL+1))
fi

# S9: Structured logging present
echo -n "S9: Structured logging... "
LOGS=$(grep -c 'emit_contract_log' "${TEST_FILE}" || true)
if [ "${LOGS}" -ge 20 ]; then
  echo "PASS (${LOGS} log emits)"; emit_log "pass" "structured_logging" "present" "" "refs=${LOGS}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "structured_logging" "insufficient" "E_LOG" "refs=${LOGS}"; FAIL=$((FAIL+1))
fi

# S10: Contract definition has >= 12 standard contracts
echo -n "S10: Standard contracts defined... "
STD=$(grep -c 'contract_id:' "${CONTRACT_FILE}" || true)
if [ "${STD}" -ge 12 ]; then
  echo "PASS (${STD} contracts)"; emit_log "pass" "standard_contracts" "sufficient" "" "count=${STD}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "standard_contracts" "insufficient" "E_CONTRACTS" "count=${STD}"; FAIL=$((FAIL+1))
fi

echo ""
echo "=== Results: ${PASS} passed, ${FAIL} failed ==="
echo "Log: ${LOG_FILE_REL}"

[ "${FAIL}" -gt 0 ] && exit 1 || exit 0
