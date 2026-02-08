#!/usr/bin/env bash
# =============================================================================
# Unit tests for scripts/check_bench_budgets.sh
#
# Creates synthetic Criterion output and verifies the budget checker:
#   1. Passes when all benchmarks are within budget
#   2. Fails when a benchmark exceeds its budget
#   3. Produces a valid JSON summary report
#   4. Handles missing data gracefully
#   5. Recognizes lazy init budget entries
#
# Usage:
#   scripts/test_bench_budgets.sh
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

CHECKER="$SCRIPT_DIR/check_bench_budgets.sh"
PASS=0
FAIL=0
TMPDIRS=()

# --- Helpers ------------------------------------------------------------------

new_tmpdir() {
    local d
    d=$(mktemp -d)
    TMPDIRS+=("$d")
    echo "$d"
}

cleanup() {
    for d in "${TMPDIRS[@]}"; do
        rm -rf "$d"
    done
}
trap cleanup EXIT

create_criterion_estimate() {
    local dir="$1"
    local group="$2"
    local bench="$3"
    local median_ns="$4"

    local target="$dir/$group/$bench/new"
    mkdir -p "$target"
    cat > "$target/estimates.json" <<EOF
{
  "mean": {"point_estimate": $median_ns, "confidence_interval": {"lower_bound": $median_ns, "upper_bound": $median_ns}},
  "median": {"point_estimate": $median_ns, "confidence_interval": {"lower_bound": $median_ns, "upper_bound": $median_ns}},
  "std_dev": {"point_estimate": 100, "confidence_interval": {"lower_bound": 50, "upper_bound": 150}}
}
EOF
}

make_checker_with_dir() {
    local criterion_dir="$1"
    local tmp
    tmp=$(mktemp)
    sed "s|CRITERION_DIR=\"target/criterion\"|CRITERION_DIR=\"$criterion_dir\"|" "$CHECKER" > "$tmp"
    chmod +x "$tmp"
    echo "$tmp"
}

assert_pass() {
    local test_name="$1"
    shift
    if "$@" > /dev/null 2>&1; then
        echo "  PASS: $test_name"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $test_name (expected pass, got failure)"
        FAIL=$((FAIL + 1))
    fi
}

assert_fail() {
    local test_name="$1"
    shift
    if "$@" > /dev/null 2>&1; then
        echo "  FAIL: $test_name (expected failure, got pass)"
        FAIL=$((FAIL + 1))
    else
        echo "  PASS: $test_name"
        PASS=$((PASS + 1))
    fi
}

assert_json_field() {
    local test_name="$1"
    local file="$2"
    local query="$3"
    local expected="$4"

    local actual
    actual=$(jq -r "$query" "$file" 2>/dev/null || echo "PARSE_ERROR")
    if [[ "$actual" == "$expected" ]]; then
        echo "  PASS: $test_name"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $test_name (expected '$expected', got '$actual')"
        FAIL=$((FAIL + 1))
    fi
}

# --- Test 1: All benchmarks within budget ------------------------------------

echo "Test 1: All benchmarks within budget"
DIR1=$(new_tmpdir)

create_criterion_estimate "$DIR1" "pattern_quick_reject" "typical" 100000        # 100us << 5ms
create_criterion_estimate "$DIR1" "pattern_detection" "codex" 300000             # 300us << 5ms
create_criterion_estimate "$DIR1" "delta_extraction" "typical" 1000000           # 1ms << 5ms
create_criterion_estimate "$DIR1" "backpressure_tier" "classify" 5000            # 5us << 10us

CHK1=$(make_checker_with_dir "$DIR1")
assert_pass "all within budget" bash "$CHK1" --check
rm -f "$CHK1"

# --- Test 2: Budget violation detected ---------------------------------------

echo ""
echo "Test 2: Budget violation detected"
DIR2=$(new_tmpdir)

create_criterion_estimate "$DIR2" "pattern_quick_reject" "regression" 50000000   # 50ms >> 5ms budget
create_criterion_estimate "$DIR2" "pattern_detection" "codex" 300000             # within budget

CHK2=$(make_checker_with_dir "$DIR2")
assert_fail "budget violation fails" bash "$CHK2" --check
rm -f "$CHK2"

# --- Test 3: JSON report structure -------------------------------------------

echo ""
echo "Test 3: JSON report structure"
DIR3=$(new_tmpdir)

create_criterion_estimate "$DIR3" "pattern_quick_reject" "typical" 100000
create_criterion_estimate "$DIR3" "pattern_detection" "codex" 300000

CHK3=$(make_checker_with_dir "$DIR3")
bash "$CHK3" --check > /dev/null 2>&1 || true
rm -f "$CHK3"

REPORT3="$DIR3/wa-budget-report.json"
assert_json_field "report has version field" "$REPORT3" '.version' "1"
assert_json_field "report has format field" "$REPORT3" '.format' "wa-budget-report"
assert_json_field "report has pass count" "$REPORT3" '.pass' "2"
assert_json_field "report has zero failures" "$REPORT3" '.fail' "0"
assert_json_field "results is array" "$REPORT3" '.results | type' "array"
assert_json_field "results has correct count" "$REPORT3" '.results | length' "2"

# --- Test 4: Budget violation appears in report ------------------------------

echo ""
echo "Test 4: Budget violation appears in report"
DIR4=$(new_tmpdir)

create_criterion_estimate "$DIR4" "pattern_quick_reject" "fast" 100000           # pass
create_criterion_estimate "$DIR4" "backpressure_tier" "slow" 500000              # 500us >> 10us budget

CHK4=$(make_checker_with_dir "$DIR4")
bash "$CHK4" --check > /dev/null 2>&1 || true
rm -f "$CHK4"

REPORT4="$DIR4/wa-budget-report.json"
assert_json_field "report shows 1 failure" "$REPORT4" '.fail' "1"
assert_json_field "report shows 1 pass" "$REPORT4" '.pass' "1"
assert_json_field "failed result has FAIL status" "$REPORT4" \
    '.results[] | select(.status == "FAIL") | .bench' "backpressure_tier/slow"

# --- Test 5: No benchmark data produces error exit ---------------------------

echo ""
echo "Test 5: No benchmark data produces error exit"
DIR5=$(new_tmpdir)

CHK5=$(make_checker_with_dir "$DIR5")
assert_fail "empty dir fails" bash "$CHK5" --check
rm -f "$CHK5"

# --- Test 6: Lazy init budgets recognized ------------------------------------

echo ""
echo "Test 6: Lazy init budgets recognized"
DIR6=$(new_tmpdir)

create_criterion_estimate "$DIR6" "pattern_lazy_init" "construction_only" 12000000   # 12ms << 50ms
create_criterion_estimate "$DIR6" "pattern_lazy_init" "first_detect_cold" 25000000   # 25ms << 200ms
create_criterion_estimate "$DIR6" "pattern_lazy_init" "subsequent_detect_warm" 40000  # 40us << 5ms

CHK6=$(make_checker_with_dir "$DIR6")
assert_pass "lazy init budgets pass" bash "$CHK6" --check
rm -f "$CHK6"

REPORT6="$DIR6/wa-budget-report.json"
assert_json_field "all 3 lazy init pass" "$REPORT6" '.pass' "3"
assert_json_field "no failures" "$REPORT6" '.fail' "0"

# --- Summary ------------------------------------------------------------------

echo ""
echo "========================================"
echo "Budget checker tests: $PASS passed, $FAIL failed"

if (( FAIL > 0 )); then
    exit 1
fi
echo "All tests passed."
exit 0
