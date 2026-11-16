#!/usr/bin/env bash
# =============================================================================
# CI: Run FTUI tests with grouped reporting and minimum count enforcement.
#
# Groups tests into categories (snapshot, e2e, unit) and reports counts.
# Enforces minimum thresholds to catch accidental test deletion.
#
# Implements: wa-36xw (FTUI-07.4)
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$PROJECT_ROOT/tests/e2e/lib_rch_guards.sh"

ARTIFACT_DIR="${FTUI_ARTIFACT_DIR:-target/ftui-test-artifacts}"
RUN_ID="${FTUI_TEST_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
RCH_TARGET_DIR="${FTUI_TEST_RCH_TARGET_DIR:-target/rch-ftui-tests-${RUN_ID}}"
mkdir -p "$ARTIFACT_DIR"
rch_init "$ARTIFACT_DIR" "$RUN_ID" "ftui_tests" "$PROJECT_ROOT"

run_ftui_cargo_logged() {
    local log_file="$1"
    shift

    run_rch_cargo_logged "$log_file" \
        env CARGO_TARGET_DIR="$RCH_TARGET_DIR" \
        cargo "$@"
}

# =============================================================================
# Minimum test count thresholds
# Update these as new tests are added. The script fails if counts drop below.
# =============================================================================
MIN_SNAPSHOT_TESTS=25
MIN_E2E_TESTS=15
MIN_TOTAL_TESTS=50

# =============================================================================
# Step 1: List all ftui tests to compute counts before running
# =============================================================================
echo "[ftui-tests] Listing ftui tests..."
echo "[ftui-tests] RCH artifacts: $ARTIFACT_DIR"
echo "[ftui-tests] RCH cargo target: $RCH_TARGET_DIR"

TEST_LIST_FILE="$ARTIFACT_DIR/test-list.txt"
TEST_LIST_LOG="$ARTIFACT_DIR/test-list.log"
ensure_rch_ready

if ! run_ftui_cargo_logged "$TEST_LIST_LOG" \
    test -p frankenterm-core --features ftui --lib -- --list; then
    echo "[ftui-tests] FAIL: unable to list ftui tests through RCH (see $TEST_LIST_LOG)"
    exit 1
fi
grep ': test$' "$TEST_LIST_LOG" > "$TEST_LIST_FILE" || true

TOTAL_COUNT=$(wc -l < "$TEST_LIST_FILE" | tr -d ' ')
SNAPSHOT_COUNT=$(grep -c '::snapshot_' "$TEST_LIST_FILE" || true)
E2E_COUNT=$(grep -c '::e2e_' "$TEST_LIST_FILE" || true)
UNIT_COUNT=$((TOTAL_COUNT - SNAPSHOT_COUNT - E2E_COUNT))

echo "[ftui-tests] Test counts:"
echo "  Snapshot: $SNAPSHOT_COUNT (min: $MIN_SNAPSHOT_TESTS)"
echo "  E2E:      $E2E_COUNT (min: $MIN_E2E_TESTS)"
echo "  Unit:     $UNIT_COUNT"
echo "  Total:    $TOTAL_COUNT (min: $MIN_TOTAL_TESTS)"

# =============================================================================
# Step 2: Enforce minimum thresholds
# =============================================================================
THRESHOLD_FAIL=0

if (( SNAPSHOT_COUNT < MIN_SNAPSHOT_TESTS )); then
    echo "[ftui-tests] FAIL: Snapshot test count $SNAPSHOT_COUNT < minimum $MIN_SNAPSHOT_TESTS"
    THRESHOLD_FAIL=1
fi

if (( E2E_COUNT < MIN_E2E_TESTS )); then
    echo "[ftui-tests] FAIL: E2E test count $E2E_COUNT < minimum $MIN_E2E_TESTS"
    THRESHOLD_FAIL=1
fi

if (( TOTAL_COUNT < MIN_TOTAL_TESTS )); then
    echo "[ftui-tests] FAIL: Total test count $TOTAL_COUNT < minimum $MIN_TOTAL_TESTS"
    THRESHOLD_FAIL=1
fi

if (( THRESHOLD_FAIL )); then
    echo "[ftui-tests] ERROR: Test count thresholds not met. Update thresholds or restore deleted tests."
    exit 1
fi

echo "[ftui-tests] All test count thresholds met."
echo ""

# =============================================================================
# Step 3: Run snapshot tests
# =============================================================================
echo "[ftui-tests] === Running snapshot tests ==="

SNAPSHOT_LOG="$ARTIFACT_DIR/snapshot-tests.log"
SNAPSHOT_EXIT=0
run_ftui_cargo_logged "$SNAPSHOT_LOG" \
    test -p frankenterm-core --features ftui --lib -- snapshot_ --nocapture \
    || SNAPSHOT_EXIT=$?

SNAPSHOT_PASSED=$(grep -c '^test .* ok$' "$SNAPSHOT_LOG" || true)
SNAPSHOT_FAILED=$(grep -c '^test .* FAILED$' "$SNAPSHOT_LOG" || true)

echo "[ftui-tests] Snapshot results: $SNAPSHOT_PASSED passed, $SNAPSHOT_FAILED failed"
echo ""

# =============================================================================
# Step 4: Run E2E tests
# =============================================================================
echo "[ftui-tests] === Running E2E tests ==="

E2E_LOG="$ARTIFACT_DIR/e2e-tests.log"
E2E_EXIT=0
run_ftui_cargo_logged "$E2E_LOG" \
    test -p frankenterm-core --features ftui --lib -- e2e_ --nocapture \
    || E2E_EXIT=$?

E2E_PASSED=$(grep -c '^test .* ok$' "$E2E_LOG" || true)
E2E_FAILED=$(grep -c '^test .* FAILED$' "$E2E_LOG" || true)

echo "[ftui-tests] E2E results: $E2E_PASSED passed, $E2E_FAILED failed"
echo ""

# =============================================================================
# Step 5: Run remaining unit tests (everything not snapshot_ or e2e_)
# =============================================================================
echo "[ftui-tests] === Running unit tests ==="

UNIT_LOG="$ARTIFACT_DIR/unit-tests.log"
UNIT_EXIT=0
run_ftui_cargo_logged "$UNIT_LOG" \
    test -p frankenterm-core --features ftui --lib -- --skip snapshot_ --skip e2e_ \
    || UNIT_EXIT=$?

UNIT_PASSED=$(grep -c '^test .* ok$' "$UNIT_LOG" || true)
UNIT_FAILED=$(grep -c '^test .* FAILED$' "$UNIT_LOG" || true)

echo "[ftui-tests] Unit results: $UNIT_PASSED passed, $UNIT_FAILED failed"
echo ""

# =============================================================================
# Step 6: Generate summary report
# =============================================================================
REPORT_FILE="$ARTIFACT_DIR/ftui-test-report.json"
cat > "$REPORT_FILE" <<EOF
{
  "version": "1",
  "format": "ftui-test-report",
  "generated_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "thresholds": {
    "min_snapshot": $MIN_SNAPSHOT_TESTS,
    "min_e2e": $MIN_E2E_TESTS,
    "min_total": $MIN_TOTAL_TESTS
  },
  "counts": {
    "total": $TOTAL_COUNT,
    "snapshot": $SNAPSHOT_COUNT,
    "e2e": $E2E_COUNT,
    "unit": $UNIT_COUNT
  },
  "results": {
    "snapshot": {"passed": $SNAPSHOT_PASSED, "failed": $SNAPSHOT_FAILED, "exit_code": $SNAPSHOT_EXIT},
    "e2e": {"passed": $E2E_PASSED, "failed": $E2E_FAILED, "exit_code": $E2E_EXIT},
    "unit": {"passed": $UNIT_PASSED, "failed": $UNIT_FAILED, "exit_code": $UNIT_EXIT}
  }
}
EOF

echo "[ftui-tests] Report written to: $REPORT_FILE"

# =============================================================================
# Step 7: Write GitHub step summary (if in CI)
# =============================================================================
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    {
        echo "## FTUI Migration Test Report"
        echo ""
        echo "| Category | Registered | Passed | Failed | Status |"
        echo "|----------|-----------|--------|--------|--------|"
        S_STATUS=$( (( SNAPSHOT_EXIT == 0 )) && echo "pass" || echo "FAIL" )
        E_STATUS=$( (( E2E_EXIT == 0 )) && echo "pass" || echo "FAIL" )
        U_STATUS=$( (( UNIT_EXIT == 0 )) && echo "pass" || echo "FAIL" )
        echo "| Snapshot | $SNAPSHOT_COUNT | $SNAPSHOT_PASSED | $SNAPSHOT_FAILED | $S_STATUS |"
        echo "| E2E | $E2E_COUNT | $E2E_PASSED | $E2E_FAILED | $E_STATUS |"
        echo "| Unit | $UNIT_COUNT | $UNIT_PASSED | $UNIT_FAILED | $U_STATUS |"
        echo "| **Total** | **$TOTAL_COUNT** | **$((SNAPSHOT_PASSED + E2E_PASSED + UNIT_PASSED))** | **$((SNAPSHOT_FAILED + E2E_FAILED + UNIT_FAILED))** | |"
        echo ""
        echo "Thresholds: snapshot >= $MIN_SNAPSHOT_TESTS, e2e >= $MIN_E2E_TESTS, total >= $MIN_TOTAL_TESTS"
    } >> "$GITHUB_STEP_SUMMARY"
fi

# =============================================================================
# Step 8: Exit with failure if any group failed
# =============================================================================
OVERALL_EXIT=0
if (( SNAPSHOT_EXIT != 0 )); then
    echo "[ftui-tests] FAIL: Snapshot tests failed (exit $SNAPSHOT_EXIT)"
    OVERALL_EXIT=1
fi
if (( E2E_EXIT != 0 )); then
    echo "[ftui-tests] FAIL: E2E tests failed (exit $E2E_EXIT)"
    OVERALL_EXIT=1
fi
if (( UNIT_EXIT != 0 )); then
    echo "[ftui-tests] FAIL: Unit tests failed (exit $UNIT_EXIT)"
    OVERALL_EXIT=1
fi

if (( OVERALL_EXIT == 0 )); then
    echo "[ftui-tests] All FTUI tests passed."
else
    echo "[ftui-tests] FTUI test suite FAILED — see logs above."
fi

exit $OVERALL_EXIT
