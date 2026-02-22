#!/bin/bash
# FrankenSqlite CI quality gates — T1-T6 tiers and BT1-BT5 wave readiness
#
# Exit codes:
#   0 — all blocking gates pass
#   1 — at least one blocking gate failed
#
# T6 (perf) is advisory: failure is logged but does not block.
set -euo pipefail

SCRIPT_NAME=$(basename "$0")
LOG_DIR="test_results"
LOG_FILE="${LOG_DIR}/${SCRIPT_NAME%.sh}_$(date +%Y%m%d_%H%M%S).log"
GATE_REPORT="${LOG_DIR}/frankensqlite_gate_report_$(date +%Y%m%d_%H%M%S).json"
mkdir -p "$LOG_DIR"

exec > >(tee -a "$LOG_FILE") 2>&1

PASS=0
FAIL=0
ADVISORY_FAIL=0
GATE_RESULTS="[]"

gate() {
    local tier="$1" name="$2" blocking="$3" timeout_s="$4"
    shift 4
    local cmd="$*"

    echo ""
    echo "═══════════════════════════════════════════════════════════════"
    echo "  Gate [$tier] $name (blocking=$blocking, timeout=${timeout_s}s)"
    echo "═══════════════════════════════════════════════════════════════"

    local start_s
    start_s=$(date +%s)
    local result="pass"
    local exit_code=0

    if timeout "${timeout_s}" bash -c "$cmd" 2>&1; then
        echo "  ✓ PASS: $name"
        PASS=$((PASS + 1))
    else
        exit_code=$?
        if [ "$blocking" = "true" ]; then
            echo "  ✗ FAIL (blocking): $name"
            FAIL=$((FAIL + 1))
            result="fail"
        else
            echo "  ⚠ FAIL (advisory): $name"
            ADVISORY_FAIL=$((ADVISORY_FAIL + 1))
            result="advisory_fail"
        fi
    fi

    local end_s
    end_s=$(date +%s)
    local duration=$((end_s - start_s))

    # Append to gate results
    GATE_RESULTS=$(echo "$GATE_RESULTS" | python3 -c "
import json, sys
data = json.load(sys.stdin)
data.append({
    'tier': '$tier',
    'name': '$name',
    'blocking': $( [ \"$blocking\" = \"true\" ] && echo true || echo false),
    'result': '$result',
    'exit_code': $exit_code,
    'duration_s': $duration
})
json.dump(data, sys.stdout)
" 2>/dev/null || echo "$GATE_RESULTS")
}

echo "=== [$SCRIPT_NAME] FrankenSqlite CI Quality Gates ==="
echo "=== Starting at $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
echo ""

# ══════════════════════════════════════════════════════════════════════
# T1: Unit / Contract tests (blocking, <60s)
# ══════════════════════════════════════════════════════════════════════
gate "T1" "Contract tests (frankensqlite_contract_tests)" "true" 60 \
    "cargo test -p frankenterm-core --test frankensqlite_contract_tests"

# ══════════════════════════════════════════════════════════════════════
# T2: Integration tests (blocking, <120s)
# ══════════════════════════════════════════════════════════════════════
gate "T2" "E2E migration tests (frankensqlite_e2e_tests)" "true" 120 \
    "cargo test -p frankenterm-core --test frankensqlite_e2e_tests"

# ══════════════════════════════════════════════════════════════════════
# T3: Rollback scenario tests (blocking, <120s)
# ══════════════════════════════════════════════════════════════════════
gate "T3" "Rollback scenarios" "true" 120 \
    "cargo test -p frankenterm-core --test frankensqlite_e2e_tests -- 'rollback|corruption|data_loss|data_integrity|checkpoint_regression|cardinality|digest_mismatch|suspected'"

# ══════════════════════════════════════════════════════════════════════
# T4: Logging assertion tests (blocking, <60s)
# ══════════════════════════════════════════════════════════════════════
gate "T4" "Logging field assertions (frankensqlite_logging_tests)" "true" 60 \
    "cargo test -p frankenterm-core --test frankensqlite_logging_tests"

# ══════════════════════════════════════════════════════════════════════
# T5: Fixture validation tests (blocking, <60s)
# ══════════════════════════════════════════════════════════════════════
gate "T5" "Fixture validation (frankensqlite_fixture_tests)" "true" 60 \
    "cargo test -p frankenterm-core --test frankensqlite_fixture_tests"

# ══════════════════════════════════════════════════════════════════════
# T6: Performance / SLO tests (advisory, <300s)
# ══════════════════════════════════════════════════════════════════════
gate "T6" "Perf SLO gates (frankensqlite_perf_tests)" "false" 300 \
    "cargo test -p frankenterm-core --test frankensqlite_perf_tests"

# ══════════════════════════════════════════════════════════════════════
# Summary
# ══════════════════════════════════════════════════════════════════════
echo ""
echo "═══════════════════════════════════════════════════════════════"
echo "  GATE SUMMARY"
echo "═══════════════════════════════════════════════════════════════"
echo "  Blocking pass:    $PASS"
echo "  Blocking fail:    $FAIL"
echo "  Advisory fail:    $ADVISORY_FAIL"
echo ""

# Write machine-readable report
cat > "$GATE_REPORT" <<REPORTEOF
{
  "timestamp": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "script": "$SCRIPT_NAME",
  "blocking_pass": $PASS,
  "blocking_fail": $FAIL,
  "advisory_fail": $ADVISORY_FAIL,
  "overall": "$( [ "$FAIL" -eq 0 ] && echo "pass" || echo "fail")",
  "gates": $GATE_RESULTS
}
REPORTEOF
echo "  Gate report: $GATE_REPORT"

# ══════════════════════════════════════════════════════════════════════
# BT (Bead Tier) wave readiness assessment
# ══════════════════════════════════════════════════════════════════════
echo ""
echo "  Wave Readiness (BT gates):"
if [ "$FAIL" -eq 0 ]; then
    echo "    BT1 (W0 Seam Hardening):  ✓ T1 green"
    echo "    BT2 (W1 Backend Impl):    ✓ T1+T2 green"
    echo "    BT3 (W2 Verification):    ✓ T1-T5 green"
    if [ "$ADVISORY_FAIL" -eq 0 ]; then
        echo "    BT4 (W3 Rollout):         ✓ T1-T6 all green (incl. perf)"
    else
        echo "    BT4 (W3 Rollout):         ⚠ T6 advisory fail — perf review needed"
    fi
    echo "    BT5 (W4 Cleanup):         ✓ No blocking regressions"
else
    echo "    BT1 (W0 Seam Hardening):  $( [ "$FAIL" -eq 0 ] && echo "✓" || echo "✗") Requires T1 green"
    echo "    BT2 (W1 Backend Impl):    ✗ Requires T1+T2 green"
    echo "    BT3 (W2 Verification):    ✗ Requires T1-T5 green"
    echo "    BT4 (W3 Rollout):         ✗ Blocked by failures"
    echo "    BT5 (W4 Cleanup):         ✗ Blocked by failures"
fi

echo ""
if [ "$FAIL" -eq 0 ]; then
    echo "=== [$SCRIPT_NAME] RESULT: PASS ==="
    exit 0
else
    echo "=== [$SCRIPT_NAME] RESULT: FAIL ==="
    exit 1
fi
