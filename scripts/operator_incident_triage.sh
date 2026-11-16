#!/bin/bash
# E4.F1.T5: Operator incident triage — simulated failure diagnosis and response
#
# This script simulates an operator responding to a migration incident:
# detecting degradation, diagnosing the rollback tier, executing the
# playbook, and verifying recovery.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

SCRIPT_NAME=$(basename "$0")
RUN_ID="${FT_OPERATOR_INCIDENT_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")-$$}"
LOG_DIR="test_results"
LOG_FILE="${LOG_DIR}/${SCRIPT_NAME%.sh}_$(date +%Y%m%d_%H%M%S).log"
DEFAULT_RCH_TARGET_DIR="target/rch-operator-incident-triage-${RUN_ID}"
REQUESTED_RCH_TARGET_DIR="${FT_OPERATOR_INCIDENT_TARGET_DIR:-${CARGO_TARGET_DIR:-}}"
if [[ -n "$REQUESTED_RCH_TARGET_DIR" && "$REQUESTED_RCH_TARGET_DIR" != /* ]]; then
    RCH_TARGET_DIR="$REQUESTED_RCH_TARGET_DIR"
else
    RCH_TARGET_DIR="$DEFAULT_RCH_TARGET_DIR"
fi
mkdir -p "$LOG_DIR"

exec > >(tee -a "$LOG_FILE") 2>&1

if ! command -v jq >/dev/null 2>&1; then
    echo "[$SCRIPT_NAME] ERROR: jq is required for RCH metadata artifacts" >&2
    exit 2
fi

RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
RCH_STEP_TIMEOUT_SECS="${FT_OPERATOR_INCIDENT_RCH_TIMEOUT_SECS:-${RCH_STEP_TIMEOUT_SECS:-900}}"
# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$REPO_ROOT/tests/e2e/lib_rch_guards.sh"
rch_init "$LOG_DIR" "$RUN_ID" "operator_incident_triage" "$REPO_ROOT"
ensure_rch_ready

PASS=0
FAIL=0

json_escape() {
    local value="$1"
    value=${value//\\/\\\\}
    value=${value//\"/\\\"}
    value=${value//$'\n'/\\n}
    value=${value//$'\r'/\\r}
    value=${value//$'\t'/\\t}
    printf '%s' "$value"
}

timestamp_utc() {
    date -u +%Y-%m-%dT%H:%M:%SZ
}

step() {
    echo ""
    echo "═══════════════════════════════════════════════════════════════"
    echo "  Step $1: $2"
    echo "═══════════════════════════════════════════════════════════════"
    printf '{"timestamp":"%s","journey":"incident","step":%s,"description":"%s"}\n' \
        "$(timestamp_utc)" "$1" "$(json_escape "$2")"
}

pass() {
    echo "  ✓ $1"
    PASS=$((PASS + 1))
}

fail() {
    echo "  ✗ $1"
    echo "  → Recommended action: $2"
    FAIL=$((FAIL + 1))
}

run_remote_cargo_tail() {
    local label="$1"
    shift
    local step_slug rch_log rc
    step_slug="$(printf '%s' "$label" | tr -cs 'A-Za-z0-9_.-' '_')"
    rch_log="${LOG_DIR}/${step_slug}_${RUN_ID}.rch.log"

    set +e
    run_rch_cargo_logged "$rch_log" \
        env CARGO_TARGET_DIR="$RCH_TARGET_DIR" \
        cargo "$@"
    rc=$?
    set -e

    tail -3 "$rch_log" || true
    return "$rc"
}

echo "=== [$SCRIPT_NAME] Operator Incident Triage Journey ==="
echo "=== Starting at $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
echo ""
echo "  SCENARIO: Migration to FrankenSqlite has been activated, but"
echo "  the operator receives alerts indicating degraded health."
echo "  This walkthrough exercises the incident response procedure."
echo ""

# ──────────────────────────────────────────────────────────────────────
step 1 "ALERT RECEIVED — Target backend reports degraded health"
# ──────────────────────────────────────────────────────────────────────
# The operator's monitoring fires on target_healthy=false after cutover.
# First verify that the degraded target detection works.

if run_remote_cargo_tail degraded_target test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_m5_degraded_target_reports_unhealthy; then
    pass "Degraded target health detection verified"
else
    fail "Health detection broken" "Monitoring may be misconfigured"
fi

# ──────────────────────────────────────────────────────────────────────
step 2 "DIAGNOSE — Classify the rollback tier"
# ──────────────────────────────────────────────────────────────────────
# The operator runs the rollback classifier to determine the severity.
# Tier 1 (Immediate): digest mismatch, cardinality mismatch
# Tier 2 (PostCutover): sustained SLO breach, repeated write failures
# Tier 3 (DataIntegrityEmergency): confirmed data loss, corruption

echo "  Checking Tier 1 (Immediate) classifier..."
if run_remote_cargo_tail tier1_digest_mismatch test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_m2_digest_mismatch; then
    pass "Tier 1 digest mismatch classifier working"
else
    fail "Tier 1 classifier broken" "Rollback automation may not trigger correctly"
fi

echo ""
echo "  Checking Tier 2 (PostCutover) classifier..."
if run_remote_cargo_tail tier2_health_failure test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_m5_health_failure; then
    pass "Tier 2 health failure classifier working"
else
    fail "Tier 2 classifier broken" "Check consecutive_slo_breach_windows threshold"
fi

echo ""
echo "  Checking Tier 3 (DataIntegrityEmergency) classifier..."
if run_remote_cargo_tail tier3_data_loss test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_data_loss; then
    pass "Tier 3 data loss classifier working"
else
    fail "Tier 3 classifier broken" "Emergency freeze may not trigger"
fi

# ──────────────────────────────────────────────────────────────────────
step 3 "EXECUTE ROLLBACK — Run the appropriate playbook"
# ──────────────────────────────────────────────────────────────────────
# Based on diagnosis, the operator executes the rollback playbook.
# Each tier has different steps and guarantees.

echo "  Testing Tier 1 (Immediate) rollback execution..."
if run_remote_cargo_tail immediate_rollback test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_immediate_rollback_playbook; then
    pass "Tier 1 rollback: backend reverted to AppendLog, target cleared"
else
    fail "Tier 1 rollback execution failed" "Manual backend switch required"
fi

echo ""
echo "  Testing Tier 2 (PostCutover) rollback execution..."
if run_remote_cargo_tail postcutover_rollback test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_postcutover_rollback; then
    pass "Tier 2 rollback: projection rebuild triggered, backend reverted"
else
    fail "Tier 2 rollback execution failed" "Manual projection rebuild needed"
fi

echo ""
echo "  Testing Tier 3 (DataIntegrity) write freeze..."
if run_remote_cargo_tail data_integrity_freeze test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_data_integrity_freeze; then
    pass "Tier 3 freeze: recorder writes blocked, forensic bundle captured"
else
    fail "Tier 3 freeze failed" "Manual intervention required; writes may continue to corrupt data"
fi

# ──────────────────────────────────────────────────────────────────────
step 4 "VERIFY RECOVERY — Confirm source data intact after rollback"
# ──────────────────────────────────────────────────────────────────────

if run_remote_cargo_tail rollback_preserves_source test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_rollback_preserves_source; then
    pass "Source AppendLog data preserved after rollback"
else
    fail "Source data integrity check failed" "Backup restoration may be needed"
fi

# ──────────────────────────────────────────────────────────────────────
step 5 "VERIFY OBSERVABILITY — Confirm logs captured incident details"
# ──────────────────────────────────────────────────────────────────────

if run_remote_cargo_tail rollback_trigger_logs_warn test -p frankenterm-core --test frankensqlite_logging_tests -- test_rollback_trigger_logs_warn; then
    pass "Rollback trigger logged at WARN level with structured fields"
else
    fail "Rollback logging missing" "Incident audit trail may be incomplete"
fi

if run_remote_cargo_tail rollback_classifier_logs_stage test -p frankenterm-core --test frankensqlite_logging_tests -- test_rollback_classifier_logs_stage; then
    pass "Rollback classifier logs include stage and tier information"
else
    fail "Classifier logging incomplete" "Triage will lack context"
fi

# ──────────────────────────────────────────────────────────────────────
step 6 "POST-INCIDENT — Verify write freeze state is detectable"
# ──────────────────────────────────────────────────────────────────────

if run_remote_cargo_tail rollback_execution_state test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_rollback_execution_state; then
    pass "Write freeze state is queryable for operator verification"
else
    fail "State query failed" "Operator cannot verify freeze status"
fi

# ──────────────────────────────────────────────────────────────────────
echo ""
echo "═══════════════════════════════════════════════════════════════"
echo "  INCIDENT TRIAGE SUMMARY"
echo "═══════════════════════════════════════════════════════════════"
echo "  Passed: $PASS"
echo "  Failed: $FAIL"
echo ""
echo "  Failure Taxonomy:"
echo "    Tier 1 (Immediate):            Digest/cardinality mismatch"
echo "    Tier 2 (PostCutover):          Sustained SLO breach, write failures"
echo "    Tier 3 (DataIntegrityEmergency): Confirmed data loss/corruption"
echo ""

if [ "$FAIL" -eq 0 ]; then
    echo "  All incident response checks passed."
    echo "  Rollback automation is functional across all tiers."
    echo "=== [$SCRIPT_NAME] RESULT: PASS ==="
    exit 0
else
    echo "  $FAIL check(s) failed. Incident response may be impaired."
    echo "=== [$SCRIPT_NAME] RESULT: FAIL ==="
    exit 1
fi
