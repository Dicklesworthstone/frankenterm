#!/bin/bash
# E4.F1.T5: Operator incident triage — simulated failure diagnosis and response
#
# This script simulates an operator responding to a migration incident:
# detecting degradation, diagnosing the rollback tier, executing the
# playbook, and verifying recovery.
set -euo pipefail

SCRIPT_NAME=$(basename "$0")
LOG_DIR="test_results"
LOG_FILE="${LOG_DIR}/${SCRIPT_NAME%.sh}_$(date +%Y%m%d_%H%M%S).log"
mkdir -p "$LOG_DIR"

exec > >(tee -a "$LOG_FILE") 2>&1

PASS=0
FAIL=0

step() {
    echo ""
    echo "═══════════════════════════════════════════════════════════════"
    echo "  Step $1: $2"
    echo "═══════════════════════════════════════════════════════════════"
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","journey":"incident","step":'"$1"',"description":"'"$2"'"}'
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

if cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_m5_degraded_target_reports_unhealthy 2>&1 | tail -3; then
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
if cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_m2_digest_mismatch 2>&1 | tail -3; then
    pass "Tier 1 digest mismatch classifier working"
else
    fail "Tier 1 classifier broken" "Rollback automation may not trigger correctly"
fi

echo ""
echo "  Checking Tier 2 (PostCutover) classifier..."
if cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_m5_health_failure 2>&1 | tail -3; then
    pass "Tier 2 health failure classifier working"
else
    fail "Tier 2 classifier broken" "Check consecutive_slo_breach_windows threshold"
fi

echo ""
echo "  Checking Tier 3 (DataIntegrityEmergency) classifier..."
if cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_data_loss 2>&1 | tail -3; then
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
if cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_immediate_rollback_playbook 2>&1 | tail -3; then
    pass "Tier 1 rollback: backend reverted to AppendLog, target cleared"
else
    fail "Tier 1 rollback execution failed" "Manual backend switch required"
fi

echo ""
echo "  Testing Tier 2 (PostCutover) rollback execution..."
if cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_postcutover_rollback 2>&1 | tail -3; then
    pass "Tier 2 rollback: projection rebuild triggered, backend reverted"
else
    fail "Tier 2 rollback execution failed" "Manual projection rebuild needed"
fi

echo ""
echo "  Testing Tier 3 (DataIntegrity) write freeze..."
if cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_data_integrity_freeze 2>&1 | tail -3; then
    pass "Tier 3 freeze: recorder writes blocked, forensic bundle captured"
else
    fail "Tier 3 freeze failed" "Manual intervention required; writes may continue to corrupt data"
fi

# ──────────────────────────────────────────────────────────────────────
step 4 "VERIFY RECOVERY — Confirm source data intact after rollback"
# ──────────────────────────────────────────────────────────────────────

if cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_rollback_preserves_source 2>&1 | tail -3; then
    pass "Source AppendLog data preserved after rollback"
else
    fail "Source data integrity check failed" "Backup restoration may be needed"
fi

# ──────────────────────────────────────────────────────────────────────
step 5 "VERIFY OBSERVABILITY — Confirm logs captured incident details"
# ──────────────────────────────────────────────────────────────────────

if cargo test -p frankenterm-core --test frankensqlite_logging_tests -- test_rollback_trigger_logs_warn 2>&1 | tail -3; then
    pass "Rollback trigger logged at WARN level with structured fields"
else
    fail "Rollback logging missing" "Incident audit trail may be incomplete"
fi

if cargo test -p frankenterm-core --test frankensqlite_logging_tests -- test_rollback_classifier_logs_stage 2>&1 | tail -3; then
    pass "Rollback classifier logs include stage and tier information"
else
    fail "Classifier logging incomplete" "Triage will lack context"
fi

# ──────────────────────────────────────────────────────────────────────
step 6 "POST-INCIDENT — Verify write freeze state is detectable"
# ──────────────────────────────────────────────────────────────────────

if cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_rollback_execution_state 2>&1 | tail -3; then
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
