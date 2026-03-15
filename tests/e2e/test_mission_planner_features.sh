#!/usr/bin/env bash
# E2E smoke test: Planner Feature Extraction and Normalization (ft-1i2ge.2.3)
#
# Validates feature extraction pipeline, scoring, normalization bounds,
# and ranking using Rust unit tests as ground truth.
#
# Summary JSON: {"test":"mission_planner_features","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"

PASS_COUNT=0
FAIL_COUNT=0

# ── rch infrastructure ──────────────────────────────────────────────────────
RCH_TARGET_DIR="target/rch-e2e-planner-features-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/planner_features_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/planner_features_${RUN_ID}.smoke.log"

fatal() { echo "FATAL: $1" >&2; exit 1; }
run_rch() { TMPDIR=/tmp rch "$@"; }
run_rch_cargo() { run_rch exec -- env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo "$@"; }
probe_has_reachable_workers() { grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"; }

check_rch_fallback() {
    local output_file="$1"
    if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_file}" 2>/dev/null; then
        fatal "rch fell back to local execution; refusing offload policy violation. See ${output_file}"
    fi
}

run_rch_cargo_logged() {
    local output_file="$1"; shift
    set +e; ( cd "${REPO_ROOT}"; run_rch_cargo "$@" ) >"${output_file}" 2>&1; local rc=$?; set -e
    check_rch_fallback "${output_file}"; return "${rc}"
}

ensure_rch_ready() {
    if ! command -v rch >/dev/null 2>&1; then
        fatal "rch is required for this e2e harness; refusing local cargo execution."
    fi
    set +e; run_rch --json workers probe --all >"${RCH_PROBE_LOG}" 2>&1; local probe_rc=$?; set -e
    if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${RCH_PROBE_LOG}"; then
        fatal "rch workers unavailable; refusing local cargo execution. See ${RCH_PROBE_LOG}"
    fi
    set +e; run_rch_cargo check --help >"${RCH_SMOKE_LOG}" 2>&1; local smoke_rc=$?; set -e
    check_rch_fallback "${RCH_SMOKE_LOG}"
    if [[ ${smoke_rc} -ne 0 ]]; then
        fatal "rch remote smoke preflight failed. See ${RCH_SMOKE_LOG}"
    fi
}

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Planner Feature Extraction E2E Suite ==="

ensure_rch_ready

# ── Scenario 1: Feature extraction basics ──────────────────────────────
echo ""
echo "--- Scenario 1: Feature Extraction Basics ---"

step1_log="${LOG_DIR}/planner_features_${RUN_ID}.s1.log"
if run_rch_cargo_logged "${step1_log}" test -p frankenterm-core --lib --features subprocess-bridge -- planner_features::tests::extract_ && grep -q "test result: ok" "${step1_log}"; then
    pass "Empty, single, blocked, and all-candidate extraction"
    echo '{"test":"mission_planner_features","scenario":1,"check":"extraction","status":"pass"}'
else
    fail "Feature extraction basics"
fi

# ── Scenario 2: Impact scoring ─────────────────────────────────────────
echo ""
echo "--- Scenario 2: Impact Scoring ---"

step2_log="${LOG_DIR}/planner_features_${RUN_ID}.s2.log"
if run_rch_cargo_logged "${step2_log}" test -p frankenterm-core --lib --features subprocess-bridge -- planner_features::tests::impact_ && grep -q "test result: ok" "${step2_log}"; then
    pass "Leaf zero, unblock increase, max cap"
    echo '{"test":"mission_planner_features","scenario":2,"check":"impact","status":"pass"}'
else
    fail "Impact scoring"
fi

# ── Scenario 3: Urgency scoring ────────────────────────────────────────
echo ""
echo "--- Scenario 3: Urgency Scoring ---"

step3_log="${LOG_DIR}/planner_features_${RUN_ID}.s3.log"
if run_rch_cargo_logged "${step3_log}" test -p frankenterm-core --lib --features subprocess-bridge -- planner_features::tests::urgency_ && grep -q "test result: ok" "${step3_log}"; then
    pass "P0 highest, P4 lowest, staleness increase and cap"
    echo '{"test":"mission_planner_features","scenario":3,"check":"urgency","status":"pass"}'
else
    fail "Urgency scoring"
fi

# ── Scenario 4: Risk and confidence ────────────────────────────────────
echo ""
echo "--- Scenario 4: Risk and Confidence ---"

step4_log="${LOG_DIR}/planner_features_${RUN_ID}.s4.log"
if run_rch_cargo_logged "${step4_log}" test -p frankenterm-core --lib --features subprocess-bridge -- planner_features::tests::risk_ planner_features::tests::confidence_ && grep -q "test result: ok" "${step4_log}"; then
    pass "Risk: clean/blocked/degraded. Confidence: clean/missing/partial/in-progress"
    echo '{"test":"mission_planner_features","scenario":4,"check":"risk_confidence","status":"pass"}'
else
    fail "Risk and confidence"
fi

# ── Scenario 5: Fit scoring ────────────────────────────────────────────
echo ""
echo "--- Scenario 5: Fit Scoring ---"

step5_log="${LOG_DIR}/planner_features_${RUN_ID}.s5.log"
if run_rch_cargo_logged "${step5_log}" test -p frankenterm-core --lib --features subprocess-bridge -- planner_features::tests::fit_ && grep -q "test result: ok" "${step5_log}"; then
    pass "No agents, idle, degraded, offline, loaded, best-of"
    echo '{"test":"mission_planner_features","scenario":5,"check":"fit","status":"pass"}'
else
    fail "Fit scoring"
fi

# ── Scenario 6: Composite scoring and ranking ──────────────────────────
echo ""
echo "--- Scenario 6: Composite Scoring and Ranking ---"

step6_log="${LOG_DIR}/planner_features_${RUN_ID}.s6.log"
if run_rch_cargo_logged "${step6_log}" test -p frankenterm-core --lib --features subprocess-bridge -- planner_features::tests::composite_ planner_features::tests::ranking_ planner_features::tests::default_weights && grep -q "test result: ok" "${step6_log}"; then
    pass "Composite bounds, max, zero, custom weights, ranking order"
    echo '{"test":"mission_planner_features","scenario":6,"check":"composite","status":"pass"}'
else
    fail "Composite scoring and ranking"
fi

# ── Scenario 7: Full unit test suite ───────────────────────────────────
echo ""
echo "--- Scenario 7: Full Unit Test Suite ---"

step7_log="${LOG_DIR}/planner_features_${RUN_ID}.s7.log"
if run_rch_cargo_logged "${step7_log}" test -p frankenterm-core --lib --features subprocess-bridge -- planner_features && grep -q "test result: ok" "${step7_log}"; then
    pass "All planner feature extraction tests (37 tests)"
else
    fail "Full unit test suite"
fi

# ── Summary ──────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"mission_planner_features\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
