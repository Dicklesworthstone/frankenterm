#!/usr/bin/env bash
# =============================================================================
# CI: Run Criterion benchmarks and enforce coarse performance budgets.
#
# Budget thresholds are embedded below (~10x observed current performance).
# The script compares each benchmark group's Criterion median against
# its ceiling and fails on gross regressions (order-of-magnitude slowdowns).
#
# Usage:
#   scripts/check_bench_budgets.sh          # run benchmarks then check
#   scripts/check_bench_budgets.sh --check  # check only (assume bench already ran)
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

CRITERION_DIR="target/criterion"
SUMMARY_FILE="$CRITERION_DIR/wa-budget-report.json"

RUN_BENCH=true
if [[ "${1:-}" == "--check" ]]; then
    RUN_BENCH=false
fi

# =============================================================================
# Budget table: group_prefix → max median nanoseconds
#
# Ceilings are ~10x observed current performance.  The goal is to catch gross
# regressions (10x+ slowdown) without false positives from CI noise.
#
# To update: run benchmarks locally, observe median values, set ceiling at
# roughly 10x the observed value.
# =============================================================================

declare -A BUDGETS=(
    # Pattern engine
    ["pattern_quick_reject"]=5000000           # observed ~100us-2.7ms (varies by size), ceiling 5ms
    ["pattern_detection/"]=5000000            # observed ~300-400us, ceiling 5ms
    ["pattern_detection_context"]=5000000     # observed ~350us, ceiling 5ms
    ["pattern_throughput"]=200000000          # observed ~20ms at 64KB, ceiling 200ms
    ["pattern_lazy_init/construction_only"]=50000000  # observed ~12ms, ceiling 50ms
    ["pattern_lazy_init/first_detect_cold"]=200000000 # observed ~25ms, ceiling 200ms
    ["pattern_lazy_init/subsequent_detect_warm"]=5000000 # observed ~40us, ceiling 5ms

    # Delta extraction
    ["delta_extraction"]=5000000              # ceiling 5ms

    # FTS queries (DB-bound, slow in CI)
    ["fts_query"]=500000000                   # ceiling 500ms

    # Storage writes (DB-bound)
    ["storage_single_append"]=50000000        # ceiling 50ms
    ["storage_batch_append"]=500000000        # ceiling 500ms
    ["storage_fts_regression"]=500000000      # ceiling 500ms
    ["storage_upsert_pane"]=20000000          # ceiling 20ms

    # Watcher loop overhead
    ["watcher_loop"]=5000000                  # ceiling 5ms

    # Backpressure (fast in-memory ops)
    ["backpressure_tier"]=10000               # ceiling 10us
    ["backpressure_scheduler"]=500000         # ceiling 500us

    # Sizing benchmarks (DB-bound, large data)
    ["sizing_insert"]=2000000000              # ceiling 2s
    ["sizing_query"]=1000000000               # ceiling 1s
)

# --- Step 1: Run benchmarks (unless --check) ---------------------------------

if $RUN_BENCH; then
    mkdir -p "$CRITERION_DIR"
    echo "[bench-budgets] Running benchmarks..."
    cargo bench -p wa-core 2>&1 | tee "$CRITERION_DIR/bench-output.log"
    echo "[bench-budgets] Benchmarks complete."
fi

# --- Step 2: Walk Criterion estimates and check budgets -----------------------

pass=0
fail=0
skip=0
total=0
results=()

# Find matching budget for a benchmark path.
# Tries longest-prefix match against the BUDGETS keys.
find_budget() {
    local bench_path="$1"
    local best_match=""
    local best_len=0

    for prefix in "${!BUDGETS[@]}"; do
        if [[ "$bench_path" == "$prefix"* ]]; then
            local plen=${#prefix}
            if (( plen > best_len )); then
                best_match="$prefix"
                best_len=$plen
            fi
        fi
    done

    if [[ -n "$best_match" ]]; then
        echo "${BUDGETS[$best_match]}"
    fi
}

# Find all estimates.json files under target/criterion/
while IFS= read -r estimates_file; do
    # Extract group/bench from path:
    #   target/criterion/<group>/<bench>/new/estimates.json
    rel="${estimates_file#$CRITERION_DIR/}"
    # Strip /new/estimates.json
    bench_path="${rel%/new/estimates.json}"

    # Extract group name (first path component)
    group="${bench_path%%/*}"

    # Skip non-benchmark directories
    if [[ "$group" == "report" ]]; then
        continue
    fi

    # Read median point estimate (nanoseconds) from Criterion JSON
    median_ns=$(jq -r '.median.point_estimate // empty' "$estimates_file" 2>/dev/null || true)
    if [[ -z "$median_ns" ]]; then
        ((skip++)) || true
        continue
    fi

    # Convert to integer (Criterion emits floats)
    median_int=$(printf '%.0f' "$median_ns")

    total=$((total + 1))

    # Find matching budget via longest-prefix match
    budget_ns=$(find_budget "$bench_path")

    if [[ -z "$budget_ns" ]]; then
        # No budget defined for this group; skip
        ((skip++)) || true
        continue
    fi

    status="PASS"
    if (( median_int > budget_ns )); then
        status="FAIL"
        ((fail++)) || true
    else
        ((pass++)) || true
    fi

    results+=("{\"bench\":\"$bench_path\",\"median_ns\":$median_int,\"budget_ns\":$budget_ns,\"status\":\"$status\"}")

    if [[ "$status" == "FAIL" ]]; then
        echo "[bench-budgets] FAIL: $bench_path  median=${median_int}ns > budget=${budget_ns}ns"
    else
        echo "[bench-budgets]  ok:  $bench_path  median=${median_int}ns <= budget=${budget_ns}ns"
    fi
done < <(find "$CRITERION_DIR" -name "estimates.json" -path "*/new/*" 2>/dev/null | sort)

# --- Step 3: Generate summary report -----------------------------------------

if (( ${#results[@]} > 0 )); then
    results_json=$(printf '%s\n' "${results[@]}" | jq -s '.')
else
    results_json="[]"
fi

mkdir -p "$(dirname "$SUMMARY_FILE")"
cat > "$SUMMARY_FILE" <<EOF
{
  "version": "1",
  "format": "wa-budget-report",
  "generated_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "total": $total,
  "pass": $pass,
  "fail": $fail,
  "skip": $skip,
  "results": $results_json
}
EOF

echo ""
echo "[bench-budgets] ========================================"
echo "[bench-budgets] Budget check: $pass passed, $fail failed, $skip skipped ($total checked)"
echo "[bench-budgets] Report: $SUMMARY_FILE"

# --- Step 4: Write GitHub step summary (if in CI) ----------------------------

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    {
        echo "## Benchmark Budget Report"
        echo ""
        echo "| Benchmark | Median | Budget | Status |"
        echo "|-----------|--------|--------|--------|"
        echo "$results_json" | jq -r '.[] | "| \(.bench) | \(.median_ns)ns | \(.budget_ns)ns | \(.status) |"'
        echo ""
        echo "**Total:** $pass passed, $fail failed, $skip skipped"
    } >> "$GITHUB_STEP_SUMMARY"
fi

# --- Step 5: Exit code -------------------------------------------------------

if (( total == 0 )); then
    echo "[bench-budgets] WARNING: No benchmark results found. Run benchmarks first."
    exit 1
fi

if (( fail > 0 )); then
    echo "[bench-budgets] FAILED: $fail benchmark(s) exceeded budget."
    exit 1
fi

echo "[bench-budgets] All benchmarks within budget."
exit 0
