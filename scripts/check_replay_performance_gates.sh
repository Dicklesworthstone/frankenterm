#!/usr/bin/env bash
# =============================================================================
# Replay performance regression gate (ft-og6q6.7.3)
#
# Validates capture/replay/diff/report/artifact-read metrics against:
# - Absolute performance budgets
# - Baseline regression thresholds (warning >10%, blocking >25%)
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

RUN_BENCH=true
WRITE_BASELINE=false
BASELINE_SOURCE="manual_refresh"

TARGET_DIR="${FT_REPLAY_PERF_TARGET_DIR:-target-replay-performance-gates}"
ARTIFACT_DIR="${FT_REPLAY_PERF_ARTIFACT_DIR:-target/replay-performance-gates}"
BASELINE_FILE="${FT_REPLAY_PERF_BASELINE_FILE:-evidence/ft-og6q6.7.3/replay_performance_baseline.json}"
CRITERION_DIR="${FT_REPLAY_PERF_CRITERION_DIR:-}"

WARN_FRACTION="${FT_REPLAY_PERF_WARN_FRACTION:-0.10}"
BLOCK_FRACTION="${FT_REPLAY_PERF_BLOCK_FRACTION:-0.25}"

CAPTURE_BUDGET_MS="1.0"
REPLAY_BUDGET_EPS="100000.0"
DIFF_BUDGET_MS="1000.0"
REPORT_BUDGET_MS="100.0"
ARTIFACT_READ_BUDGET_EPS="500000.0"

REPLAY_BATCH_EVENTS=20000
ARTIFACT_STREAM_EVENTS=250000

usage() {
    cat <<USAGE
Usage: $0 [OPTIONS]

Options:
  --check                 Check-only mode (do not run cargo bench)
  --write-baseline        Refresh baseline JSON with current measured metrics
  --baseline-source STR   Baseline source label when writing baseline (default: manual_refresh)
  --target-dir DIR        Override CARGO_TARGET_DIR for benchmark runs
  --criterion-dir DIR     Override Criterion output root (default: <target-dir>/criterion)
  --artifacts-dir DIR     Override report/log artifact directory
  --baseline-file FILE    Override baseline file path
  -h, --help              Show this help

Environment:
  FT_REPLAY_PERF_AUTO_WRITE_BASELINE=true
      Automatically refresh baseline after push to main in CI.
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --check)
            RUN_BENCH=false
            shift
            ;;
        --write-baseline)
            WRITE_BASELINE=true
            shift
            ;;
        --baseline-source)
            BASELINE_SOURCE="$2"
            shift 2
            ;;
        --target-dir)
            TARGET_DIR="$2"
            shift 2
            ;;
        --criterion-dir)
            CRITERION_DIR="$2"
            shift 2
            ;;
        --artifacts-dir)
            ARTIFACT_DIR="$2"
            shift 2
            ;;
        --baseline-file)
            BASELINE_FILE="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "[replay-perf] Unknown argument: $1" >&2
            usage
            exit 2
            ;;
    esac
done

if [[ -z "$CRITERION_DIR" ]]; then
    CRITERION_DIR="$TARGET_DIR/criterion"
fi

if ! command -v jq >/dev/null 2>&1; then
    echo "[replay-perf] ERROR: jq is required" >&2
    exit 3
fi

mkdir -p "$ARTIFACT_DIR"
BENCH_LOG="$ARTIFACT_DIR/replay-performance-bench.log"
REPORT_FILE="$ARTIFACT_DIR/replay-performance-report.json"

now_iso() {
    date -u +"%Y-%m-%dT%H:%M:%SZ"
}

ensure_estimate() {
    local path="$1"
    local label="$2"
    if [[ ! -f "$path" ]]; then
        echo "[replay-perf] ERROR: missing criterion estimate for $label: $path" >&2
        exit 4
    fi
}

read_median_ns() {
    local path="$1"
    jq -er '.median.point_estimate' "$path"
}

to_ms() {
    local ns="$1"
    awk "BEGIN { printf \"%.6f\", ($ns / 1000000.0) }"
}

to_eps() {
    local events="$1"
    local ns="$2"
    awk "BEGIN { if ($ns <= 0) print 0; else printf \"%.6f\", (($events * 1000000000.0) / $ns) }"
}

if [[ "${FT_REPLAY_PERF_AUTO_WRITE_BASELINE:-false}" == "true" ]] \
    && [[ "${GITHUB_EVENT_NAME:-}" == "push" ]] \
    && [[ "${GITHUB_REF:-}" == "refs/heads/main" ]]; then
    WRITE_BASELINE=true
    BASELINE_SOURCE="auto_main_refresh"
fi

if $RUN_BENCH; then
    : > "$BENCH_LOG"
    echo "[replay-perf] Running replay performance benches" | tee -a "$BENCH_LOG"

    env CARGO_TARGET_DIR="$TARGET_DIR" \
        cargo bench -p frankenterm-core --bench replay_capture \
        2>&1 | tee -a "$BENCH_LOG"

    env CARGO_TARGET_DIR="$TARGET_DIR" \
        cargo bench -p frankenterm-core --bench replay_kernel \
        2>&1 | tee -a "$BENCH_LOG"

    env CARGO_TARGET_DIR="$TARGET_DIR" \
        cargo bench -p frankenterm-core --bench replay_diff \
        2>&1 | tee -a "$BENCH_LOG"
fi

CAPTURE_ESTIMATE="$CRITERION_DIR/replay_capture/capture_overhead_per_event/new/estimates.json"
KERNEL_ESTIMATE="$CRITERION_DIR/replay_kernel/instant_mode_20000_events/new/estimates.json"
ARTIFACT_ESTIMATE="$CRITERION_DIR/replay_kernel/artifact_read_stream_250000_events/new/estimates.json"
DIFF_ESTIMATE="$CRITERION_DIR/replay_diff/diff_1000_divergences/new/estimates.json"
REPORT_ESTIMATE="$CRITERION_DIR/replay_diff/standard_report_generation/new/estimates.json"

ensure_estimate "$CAPTURE_ESTIMATE" "capture_overhead_per_event"
ensure_estimate "$KERNEL_ESTIMATE" "instant_mode_20000_events"
ensure_estimate "$ARTIFACT_ESTIMATE" "artifact_read_stream_250000_events"
ensure_estimate "$DIFF_ESTIMATE" "diff_1000_divergences"
ensure_estimate "$REPORT_ESTIMATE" "standard_report_generation"

capture_ns="$(read_median_ns "$CAPTURE_ESTIMATE")"
kernel_ns="$(read_median_ns "$KERNEL_ESTIMATE")"
artifact_ns="$(read_median_ns "$ARTIFACT_ESTIMATE")"
diff_ns="$(read_median_ns "$DIFF_ESTIMATE")"
report_ns="$(read_median_ns "$REPORT_ESTIMATE")"

capture_ms="$(to_ms "$capture_ns")"
replay_eps="$(to_eps "$REPLAY_BATCH_EVENTS" "$kernel_ns")"
artifact_eps="$(to_eps "$ARTIFACT_STREAM_EVENTS" "$artifact_ns")"
diff_ms="$(to_ms "$diff_ns")"
report_ms="$(to_ms "$report_ns")"

sample_json="$(jq -nc \
    --argjson capture "$capture_ms" \
    --argjson replay "$replay_eps" \
    --argjson diff "$diff_ms" \
    --argjson report "$report_ms" \
    --argjson artifact "$artifact_eps" \
    '{
      capture_overhead_ms: $capture,
      replay_throughput_eps: $replay,
      diff_latency_ms: $diff,
      report_generation_ms: $report,
      artifact_read_eps: $artifact
    }'
)"

budgets_json="$(jq -nc \
    --argjson capture "$CAPTURE_BUDGET_MS" \
    --argjson replay "$REPLAY_BUDGET_EPS" \
    --argjson diff "$DIFF_BUDGET_MS" \
    --argjson report "$REPORT_BUDGET_MS" \
    --argjson artifact "$ARTIFACT_READ_BUDGET_EPS" \
    '{
      capture_overhead_ms: $capture,
      replay_throughput_eps: $replay,
      diff_latency_ms: $diff,
      report_generation_ms: $report,
      artifact_read_eps: $artifact
    }'
)"

baseline_meta_json="null"
baseline_sample_json="{}"
if [[ -f "$BASELINE_FILE" ]]; then
    if jq -e '.sample' "$BASELINE_FILE" >/dev/null 2>&1; then
        baseline_meta_json="$(jq -c '.' "$BASELINE_FILE")"
        baseline_sample_json="$(jq -c '.sample' "$BASELINE_FILE")"
    fi
fi

report_json="$(jq -nc \
    --arg version "1" \
    --arg format "ft-replay-performance-report" \
    --arg generated_at "$(now_iso)" \
    --arg criterion_dir "$CRITERION_DIR" \
    --arg baseline_file "$BASELINE_FILE" \
    --argjson warn "$WARN_FRACTION" \
    --argjson block "$BLOCK_FRACTION" \
    --argjson sample "$sample_json" \
    --argjson budgets "$budgets_json" \
    --argjson baseline_meta "$baseline_meta_json" \
    --argjson baseline_sample "$baseline_sample_json" \
'
  def lower_is_better($m): ($m == "capture_overhead_ms" or $m == "diff_latency_ms" or $m == "report_generation_ms");
  def within_budget($m; $value; $budget): if lower_is_better($m) then ($value <= $budget) else ($value >= $budget) end;
  def regression($m; $baseline; $value):
    if ($baseline == null) or ($baseline <= 0) then null
    else if lower_is_better($m) then (($value - $baseline) / $baseline)
         else (($baseline - $value) / $baseline)
         end
    end;
  def classify($within; $reg; $warn; $block):
    if ($within | not) then {status:"blocking", reason_code:"budget_exceeded"}
    elif ($reg == null) then {status:"pass", reason_code:"baseline_missing_or_invalid"}
    elif ($reg > $block) then {status:"blocking", reason_code:"regression_blocking"}
    elif ($reg > $warn) then {status:"warning", reason_code:"regression_warning"}
    elif ($reg < 0) then {status:"improvement", reason_code:"regression_improvement"}
    else {status:"pass", reason_code:"regression_within_tolerance"}
    end;
  def metric_row($m):
    ( $sample[$m] ) as $value |
    ( $budgets[$m] ) as $budget |
    ( $baseline_sample[$m] // null ) as $base |
    (within_budget($m; $value; $budget)) as $within |
    (regression($m; $base; $value)) as $reg |
    (classify($within; $reg; $warn; $block)) as $class |
    {
      metric: $m,
      value: $value,
      budget: $budget,
      within_budget: $within,
      baseline: $base,
      regression_fraction: $reg,
      regression_percent: (if $reg == null then null else ($reg * 100.0) end),
      status: $class.status,
      reason_code: $class.reason_code
    };
  ([
    "capture_overhead_ms",
    "replay_throughput_eps",
    "diff_latency_ms",
    "report_generation_ms",
    "artifact_read_eps"
  ] | map(metric_row(.))) as $metrics |
  {
    version: $version,
    format: $format,
    generated_at: $generated_at,
    criterion_dir: $criterion_dir,
    baseline_file: $baseline_file,
    warning_regression_fraction: $warn,
    blocking_regression_fraction: $block,
    sample: $sample,
    budgets: $budgets,
    baseline: $baseline_meta,
    metrics: $metrics,
    summary: {
      warning_count: ($metrics | map(select(.status == "warning")) | length),
      blocking_count: ($metrics | map(select(.status == "blocking")) | length),
      overall_status: (
        if ($metrics | any(.status == "blocking")) then "blocking"
        elif ($metrics | any(.status == "warning")) then "warning"
        elif ($metrics | any(.status == "improvement")) then "improvement"
        else "pass"
        end
      )
    },
    capacity_guidance: {
      replay_seconds_for_1m_events: (if $sample.replay_throughput_eps > 0 then (1000000.0 / $sample.replay_throughput_eps) else null end),
      replay_seconds_for_10m_events: (if $sample.replay_throughput_eps > 0 then (10000000.0 / $sample.replay_throughput_eps) else null end),
      artifact_read_seconds_for_1m_events: (if $sample.artifact_read_eps > 0 then (1000000.0 / $sample.artifact_read_eps) else null end),
      artifact_read_seconds_for_10m_events: (if $sample.artifact_read_eps > 0 then (10000000.0 / $sample.artifact_read_eps) else null end)
    }
  }
')"

printf '%s\n' "$report_json" > "$REPORT_FILE"

overall_status="$(jq -r '.summary.overall_status' "$REPORT_FILE")"
warning_count="$(jq -r '.summary.warning_count' "$REPORT_FILE")"
blocking_count="$(jq -r '.summary.blocking_count' "$REPORT_FILE")"

echo "[replay-perf] report: $REPORT_FILE"
echo "[replay-perf] status=$overall_status warnings=$warning_count blocking=$blocking_count"

if $WRITE_BASELINE; then
    mkdir -p "$(dirname "$BASELINE_FILE")"
    new_baseline_json="$(jq -nc \
        --arg version "1" \
        --arg source "$BASELINE_SOURCE" \
        --arg generated_at "$(now_iso)" \
        --argjson sample "$sample_json" \
        '{
          version: $version,
          source: $source,
          generated_at: $generated_at,
          sample: {
            capture_overhead_ms_per_event: $sample.capture_overhead_ms,
            replay_throughput_events_per_sec: $sample.replay_throughput_eps,
            diff_latency_ms_per_1000_divergences: $sample.diff_latency_ms,
            report_generation_ms: $sample.report_generation_ms,
            artifact_read_events_per_sec: $sample.artifact_read_eps
          }
        }'
    )"
    printf '%s\n' "$new_baseline_json" > "$BASELINE_FILE"

    baseline_audit_log="${BASELINE_FILE%.json}.audit.jsonl"
    jq -nc \
      --arg timestamp "$(now_iso)" \
      --arg source "$BASELINE_SOURCE" \
      --arg baseline_file "$BASELINE_FILE" \
      --arg report_file "$REPORT_FILE" \
      '{
        timestamp: $timestamp,
        component: "replay_performance_gate",
        decision_path: "baseline_refresh",
        source: $source,
        baseline_file: $baseline_file,
        report_file: $report_file
      }' >> "$baseline_audit_log"

    echo "[replay-perf] baseline refreshed: $BASELINE_FILE"
fi

if [[ "$overall_status" == "blocking" ]]; then
    echo "[replay-perf] BLOCKING regression detected"
    exit 1
fi

echo "[replay-perf] gate passed"
