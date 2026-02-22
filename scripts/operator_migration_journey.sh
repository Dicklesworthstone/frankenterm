#!/bin/bash
# E4.F1.T5: Operator migration journey — full walkthrough with narrative logging
#
# This script simulates the operator experience of migrating from
# AppendLog to FrankenSqlite backend, including pre-checks, execution,
# post-validation, and rollback drill.
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
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","journey":"migration","step":'"$1"',"description":"'"$2"'"}'
}

pass() {
    echo "  ✓ $1"
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","result":"pass","detail":"'"$1"'"}'
    PASS=$((PASS + 1))
}

fail() {
    echo "  ✗ $1"
    echo '{"timestamp":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","result":"fail","detail":"'"$1"'"}'
    echo "  → Recommended action: $2"
    FAIL=$((FAIL + 1))
}

echo "=== [$SCRIPT_NAME] Operator Migration Journey ==="
echo "=== Starting at $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
echo ""
echo "  This walkthrough simulates a FrankenSqlite migration from the"
echo "  operator's perspective, exercising each verification step."
echo ""

# ──────────────────────────────────────────────────────────────────────
step 1 "Pre-migration health check — verify source AppendLog is healthy"
# ──────────────────────────────────────────────────────────────────────
# The operator runs the contract tests to verify the storage layer is
# functioning correctly before attempting any migration.

if cargo test -p frankenterm-core --test frankensqlite_contract_tests -- test_health_append_log 2>&1 | tail -3; then
    pass "Source AppendLog backend health verified"
else
    fail "AppendLog health check failed" "Check disk space and permissions on data_path"
fi

# ──────────────────────────────────────────────────────────────────────
step 2 "Run contract suite — verify recorder seam contracts hold"
# ──────────────────────────────────────────────────────────────────────
# Before migration, ensure all contract invariants are passing.

if cargo test -p frankenterm-core --test frankensqlite_contract_tests 2>&1 | tail -3; then
    pass "All 32 contract tests passing"
else
    fail "Contract tests have failures" "Fix contract violations before migrating"
fi

# ──────────────────────────────────────────────────────────────────────
step 3 "Execute migration pipeline — M0 through M5"
# ──────────────────────────────────────────────────────────────────────
# The operator runs the full E2E migration test which exercises
# M0 (preflight) → M1 (export) → M2 (import) → M3 (checkpoint sync)
# → M5 (cutover) in sequence.

if cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_full_migration_happy_path 2>&1 | tail -3; then
    pass "Full M0-M5 migration pipeline completed successfully"
else
    fail "Migration pipeline failed" "Check error logs; consider M2 import failure or digest mismatch"
fi

# ──────────────────────────────────────────────────────────────────────
step 4 "Post-migration validation — verify data integrity"
# ──────────────────────────────────────────────────────────────────────
# After migration, the operator verifies digest match and event counts.

if cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_manifest_digest_matches_re_export 2>&1 | tail -3; then
    pass "Export digest is deterministic and reproducible"
else
    fail "Digest reproducibility check failed" "Data may be corrupted; initiate immediate rollback"
fi

# ──────────────────────────────────────────────────────────────────────
step 5 "Checkpoint monotonicity — verify no regression across cutover"
# ──────────────────────────────────────────────────────────────────────

if cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_checkpoint_monotonicity 2>&1 | tail -3; then
    pass "Checkpoint monotonicity preserved across cutover"
else
    fail "Checkpoint regression detected" "Consumer checkpoints went backwards; check M3 sync"
fi

# ──────────────────────────────────────────────────────────────────────
step 6 "Rollback drill — verify rollback playbook executes correctly"
# ──────────────────────────────────────────────────────────────────────
# Every migration should include a rollback drill to verify the
# operator can safely revert if needed.

if cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_immediate_rollback_playbook 2>&1 | tail -3; then
    pass "Immediate rollback playbook executes successfully"
else
    fail "Rollback playbook failed" "Manual intervention required; check rollback state"
fi

if cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_postcutover_rollback_playbook 2>&1 | tail -3; then
    pass "Post-cutover rollback playbook executes successfully"
else
    fail "Post-cutover rollback failed" "Projection rebuild may be needed"
fi

# ──────────────────────────────────────────────────────────────────────
step 7 "SLO gates — verify performance meets budgets"
# ──────────────────────────────────────────────────────────────────────

if cargo test -p frankenterm-core --test frankensqlite_perf_tests -- test_slo 2>&1 | tail -3; then
    pass "All SLO gate tests passing"
else
    fail "SLO gate check failed" "Performance below budget; check system load"
fi

# ──────────────────────────────────────────────────────────────────────
step 8 "Observability — verify structured logging fields present"
# ──────────────────────────────────────────────────────────────────────

if cargo test -p frankenterm-core --test frankensqlite_logging_tests -- test_full_pipeline_emits_all_stage_logs 2>&1 | tail -3; then
    pass "Migration pipeline emits all required stage logs"
else
    fail "Stage logging incomplete" "Check tracing subscriber configuration"
fi

# ──────────────────────────────────────────────────────────────────────
echo ""
echo "═══════════════════════════════════════════════════════════════"
echo "  JOURNEY SUMMARY"
echo "═══════════════════════════════════════════════════════════════"
echo "  Passed: $PASS"
echo "  Failed: $FAIL"
echo ""

if [ "$FAIL" -eq 0 ]; then
    echo "  All checks passed. Migration is safe to proceed."
    echo "=== [$SCRIPT_NAME] RESULT: PASS ==="
    exit 0
else
    echo "  $FAIL check(s) failed. Review failures above before proceeding."
    echo "=== [$SCRIPT_NAME] RESULT: FAIL ==="
    exit 1
fi
