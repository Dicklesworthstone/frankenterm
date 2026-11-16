#!/bin/bash
# E4.F1.T5: Operator migration journey — full walkthrough with narrative logging
#
# This script simulates the operator experience of migrating from
# AppendLog to FrankenSqlite backend, including pre-checks, execution,
# post-validation, and rollback drill.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

SCRIPT_NAME=$(basename "$0")
RUN_ID="${FT_OPERATOR_MIGRATION_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")-$$}"
LOG_DIR="test_results"
LOG_FILE="${LOG_DIR}/${SCRIPT_NAME%.sh}_$(date +%Y%m%d_%H%M%S).log"
DEFAULT_RCH_TARGET_DIR="target/rch-operator-migration-journey-${RUN_ID}"
REQUESTED_RCH_TARGET_DIR="${FT_OPERATOR_MIGRATION_TARGET_DIR:-${CARGO_TARGET_DIR:-}}"
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
RCH_STEP_TIMEOUT_SECS="${FT_OPERATOR_MIGRATION_RCH_TIMEOUT_SECS:-${RCH_STEP_TIMEOUT_SECS:-900}}"
# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$REPO_ROOT/tests/e2e/lib_rch_guards.sh"
rch_init "$LOG_DIR" "$RUN_ID" "operator_migration_journey" "$REPO_ROOT"
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
    printf '{"timestamp":"%s","journey":"migration","step":%s,"description":"%s"}\n' \
        "$(timestamp_utc)" "$1" "$(json_escape "$2")"
}

pass() {
    echo "  ✓ $1"
    printf '{"timestamp":"%s","result":"pass","detail":"%s"}\n' \
        "$(timestamp_utc)" "$(json_escape "$1")"
    PASS=$((PASS + 1))
}

fail() {
    echo "  ✗ $1"
    printf '{"timestamp":"%s","result":"fail","detail":"%s"}\n' \
        "$(timestamp_utc)" "$(json_escape "$1")"
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

if run_remote_cargo_tail health_append_log test -p frankenterm-core --test frankensqlite_contract_tests -- test_health_append_log; then
    pass "Source AppendLog backend health verified"
else
    fail "AppendLog health check failed" "Check disk space and permissions on data_path"
fi

# ──────────────────────────────────────────────────────────────────────
step 2 "Run contract suite — verify recorder seam contracts hold"
# ──────────────────────────────────────────────────────────────────────
# Before migration, ensure all contract invariants are passing.

if run_remote_cargo_tail contract_suite test -p frankenterm-core --test frankensqlite_contract_tests; then
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

if run_remote_cargo_tail full_migration_happy_path test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_full_migration_happy_path; then
    pass "Full M0-M5 migration pipeline completed successfully"
else
    fail "Migration pipeline failed" "Check error logs; consider M2 import failure or digest mismatch"
fi

# ──────────────────────────────────────────────────────────────────────
step 4 "Post-migration validation — verify data integrity"
# ──────────────────────────────────────────────────────────────────────
# After migration, the operator verifies digest match and event counts.

if run_remote_cargo_tail manifest_digest_matches_re_export test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_manifest_digest_matches_re_export; then
    pass "Export digest is deterministic and reproducible"
else
    fail "Digest reproducibility check failed" "Data may be corrupted; initiate immediate rollback"
fi

# ──────────────────────────────────────────────────────────────────────
step 5 "Checkpoint monotonicity — verify no regression across cutover"
# ──────────────────────────────────────────────────────────────────────

if run_remote_cargo_tail checkpoint_monotonicity test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_checkpoint_monotonicity; then
    pass "Checkpoint monotonicity preserved across cutover"
else
    fail "Checkpoint regression detected" "Consumer checkpoints went backwards; check M3 sync"
fi

# ──────────────────────────────────────────────────────────────────────
step 6 "Rollback drill — verify rollback playbook executes correctly"
# ──────────────────────────────────────────────────────────────────────
# Every migration should include a rollback drill to verify the
# operator can safely revert if needed.

if run_remote_cargo_tail immediate_rollback_playbook test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_immediate_rollback_playbook; then
    pass "Immediate rollback playbook executes successfully"
else
    fail "Rollback playbook failed" "Manual intervention required; check rollback state"
fi

if run_remote_cargo_tail postcutover_rollback_playbook test -p frankenterm-core --test frankensqlite_e2e_tests -- test_e2e_postcutover_rollback_playbook; then
    pass "Post-cutover rollback playbook executes successfully"
else
    fail "Post-cutover rollback failed" "Projection rebuild may be needed"
fi

# ──────────────────────────────────────────────────────────────────────
step 7 "SLO gates — verify performance meets budgets"
# ──────────────────────────────────────────────────────────────────────

if run_remote_cargo_tail perf_slo test -p frankenterm-core --test frankensqlite_perf_tests -- test_slo; then
    pass "All SLO gate tests passing"
else
    fail "SLO gate check failed" "Performance below budget; check system load"
fi

# ──────────────────────────────────────────────────────────────────────
step 8 "Observability — verify structured logging fields present"
# ──────────────────────────────────────────────────────────────────────

if run_remote_cargo_tail full_pipeline_logs test -p frankenterm-core --test frankensqlite_logging_tests -- test_full_pipeline_emits_all_stage_logs; then
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
