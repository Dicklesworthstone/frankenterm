#!/usr/bin/env bash
# =============================================================================
# CI gate: resize/reflow performance regression enforcement (ft-1u90p.7.4)
#
# Default mode:
#   1) Runs required deterministic validation tests.
#   2) Runs resize baseline scenarios with `--resize-timeline-json`.
#   3) Enforces mid-tier M1/M2 thresholds from docs/resize-performance-slos.md.
#   4) Emits machine-readable report with hard_fail vs warning classification.
#
# Check-only mode:
#   --check-only <dir>
#   Evaluate pre-generated scenario envelope JSON files at:
#     <dir>/<scenario-name>.json
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

ARTIFACT_DIR="${FT_RESIZE_GATE_ARTIFACT_DIR:-target/resize-performance-gates}"
TARGET_DIR="${FT_RESIZE_GATE_TARGET_DIR:-target-resize-performance-gates}"
BASELINE_FILE="${FT_RESIZE_GATE_BASELINE_FILE:-evidence/wa-1u90p.7.4/resize_perf_mid_baseline.json}"
BASELINE_WARN_MULTIPLIER="${FT_RESIZE_GATE_BASELINE_WARN_MULTIPLIER:-1.10}"
BASELINE_FAIL_MULTIPLIER="${FT_RESIZE_GATE_BASELINE_FAIL_MULTIPLIER:-1.20}"
BASELINE_REASON="${FT_RESIZE_GATE_BASELINE_REASON:-}"
CHECK_ONLY_DIR=""
WRITE_BASELINE=false
SKIP_TEST_LANES=false

# Mid-tier thresholds from docs/resize-performance-slos.md
M1_P50_MAX_NS=12000000
M1_P95_MAX_NS=20000000
M1_P99_MAX_NS=33000000

declare -A M2_P95_MAX_NS=(
    [input_intent]=1000000
    [scheduler_queueing]=3000000
    [logical_reflow]=8000000
    [render_prep]=6000000
    [presentation]=4000000
)

declare -a SCENARIOS=(
    "resize_single_pane_scrollback|fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml"
    "resize_multi_tab_storm|fixtures/simulations/resize_baseline/resize_multi_tab_storm.yaml"
    "font_churn_multi_pane|fixtures/simulations/resize_baseline/font_churn_multi_pane.yaml"
    "mixed_scale_soak|fixtures/simulations/resize_baseline/mixed_scale_soak.yaml"
    "mixed_workload_interactive_streaming|fixtures/simulations/resize_baseline/mixed_workload_interactive_streaming.yaml"
)

usage() {
    cat <<EOF
Usage: $0 [OPTIONS]

Options:
  --check-only DIR     Skip cargo execution; evaluate scenario JSON envelopes in DIR.
  --write-baseline     Write baseline file from current run metrics (requires FT_RESIZE_GATE_BASELINE_REASON).
  --baseline-file PATH Override baseline file path.
  --artifacts-dir DIR  Override artifacts directory (default: $ARTIFACT_DIR).
  --target-dir DIR     Override cargo target dir (default: $TARGET_DIR).
  --skip-test-lanes    Skip cargo test validation lanes (not recommended in CI).
  -h, --help           Show help.

Environment:
  FT_RESIZE_GATE_BASELINE_REASON  Required with --write-baseline for auditability.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --check-only)
            CHECK_ONLY_DIR="$2"
            shift 2
            ;;
        --write-baseline)
            WRITE_BASELINE=true
            shift
            ;;
        --baseline-file)
            BASELINE_FILE="$2"
            shift 2
            ;;
        --artifacts-dir)
            ARTIFACT_DIR="$2"
            shift 2
            ;;
        --target-dir)
            TARGET_DIR="$2"
            shift 2
            ;;
        --skip-test-lanes)
            SKIP_TEST_LANES=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            usage
            exit 3
            ;;
    esac
done

if ! command -v jq >/dev/null 2>&1; then
    echo "[resize-gates] ERROR: jq is required" >&2
    exit 5
fi

if [[ -z "$CHECK_ONLY_DIR" ]] && ! command -v cargo >/dev/null 2>&1; then
    echo "[resize-gates] ERROR: cargo is required" >&2
    exit 5
fi

mkdir -p "$ARTIFACT_DIR"
SCENARIO_ARTIFACT_DIR="$ARTIFACT_DIR/scenarios"
mkdir -p "$SCENARIO_ARTIFACT_DIR"
REPORT_FILE="$ARTIFACT_DIR/resize-performance-report.json"
BASELINE_AUDIT_LOG="${BASELINE_FILE%.json}.audit.jsonl"

now_iso() {
    date -u +"%Y-%m-%dT%H:%M:%SZ"
}

ns_to_ms() {
    local ns="$1"
    awk "BEGIN { printf \"%.3f\", (${ns} / 1000000.0) }"
}

float_gt() {
    awk "BEGIN { exit !($1 > $2) }"
}

float_ge() {
    awk "BEGIN { exit !($1 >= $2) }"
}

run_step() {
    local name="$1"
    shift
    local log_file="$ARTIFACT_DIR/${name}.log"
    echo "[resize-gates] === $name ==="
    echo "[resize-gates] cmd: $*" >"$log_file"
    if "$@" 2>&1 | tee -a "$log_file"; then
        echo "[resize-gates] step $name: pass"
        return 0
    fi
    echo "[resize-gates] step $name: fail"
    return 1
}

build_scenario_envelope() {
    local scenario_name="$1"
    local scenario_file="$2"
    local out_file="$3"
    local log_file="$4"

    env CARGO_TARGET_DIR="$TARGET_DIR" \
        cargo run -p frankenterm -- \
            simulate run "$scenario_file" --json --resize-timeline-json \
        >"$out_file" 2>"$log_file"
}

read_metric_or_zero() {
    local file="$1"
    local query="$2"
    jq -r "$query // 0" "$file" 2>/dev/null || echo 0
}

compute_m1_percentiles() {
    local file="$1"
    local p50 p95 p99
    p50="$(read_metric_or_zero "$file" '
        if .aggregate_event_duration_ns.p50 != null then .aggregate_event_duration_ns.p50
        else ([.timeline.events[].total_duration_ns] | sort | if length == 0 then 0 else .[((length - 1) * 50 / 100 | floor)] end)
        end
    ')"
    p95="$(read_metric_or_zero "$file" '
        if .aggregate_event_duration_ns.p95 != null then .aggregate_event_duration_ns.p95
        else ([.timeline.events[].total_duration_ns] | sort | if length == 0 then 0 else .[((length - 1) * 95 / 100 | floor)] end)
        end
    ')"
    p99="$(read_metric_or_zero "$file" '
        if .aggregate_event_duration_ns.p99 != null then .aggregate_event_duration_ns.p99
        else ([.timeline.events[].total_duration_ns] | sort | if length == 0 then 0 else .[((length - 1) * 99 / 100 | floor)] end)
        end
    ')"
    echo "$p50|$p95|$p99"
}

read_stage_p95() {
    local file="$1"
    local stage="$2"
    jq -r --arg stage "$stage" '[.stage_summary[]? | select(.stage == $stage) | .p95_duration_ns][0] // -1' "$file" 2>/dev/null || echo -1
}

read_queue_depth_peak() {
    local file="$1"
    local field="$2"
    jq -r --arg field "$field" '
        [.timeline.events[]?.stages[]?
            | select(.stage == "scheduler_queueing")
            | .queue_metrics[$field]?]
        | if length == 0 then 0 else max end
    ' "$file" 2>/dev/null || echo 0
}

overall_step_failures=0
step_rows_json='[]'

if [[ -z "$CHECK_ONLY_DIR" ]] && [[ "$SKIP_TEST_LANES" == "false" ]]; then
    if run_step "simulation_resize_suite" \
        env CARGO_TARGET_DIR="$TARGET_DIR" \
        cargo test -p frankenterm-core --test simulation_resize_suite -- --nocapture; then
        step_rows_json="$(jq -c '. + [{"name":"simulation_resize_suite","status":"pass"}]' <<<"$step_rows_json")"
    else
        step_rows_json="$(jq -c '. + [{"name":"simulation_resize_suite","status":"hard_fail"}]' <<<"$step_rows_json")"
        overall_step_failures=$((overall_step_failures + 1))
    fi

    if run_step "timeline_probe_integrity" \
        env CARGO_TARGET_DIR="$TARGET_DIR" \
        cargo test -p frankenterm-core resize_timeline_summary_and_flame_samples_cover_all_stages -- --nocapture; then
        step_rows_json="$(jq -c '. + [{"name":"timeline_probe_integrity","status":"pass"}]' <<<"$step_rows_json")"
    else
        step_rows_json="$(jq -c '. + [{"name":"timeline_probe_integrity","status":"hard_fail"}]' <<<"$step_rows_json")"
        overall_step_failures=$((overall_step_failures + 1))
    fi

    if run_step "runtime_warning_thresholds" \
        env CARGO_TARGET_DIR="$TARGET_DIR" \
        cargo test -p frankenterm-core warning_threshold_fires -- --nocapture; then
        step_rows_json="$(jq -c '. + [{"name":"runtime_warning_thresholds","status":"pass"}]' <<<"$step_rows_json")"
    else
        step_rows_json="$(jq -c '. + [{"name":"runtime_warning_thresholds","status":"hard_fail"}]' <<<"$step_rows_json")"
        overall_step_failures=$((overall_step_failures + 1))
    fi
elif [[ "$SKIP_TEST_LANES" == "true" ]]; then
    step_rows_json='[{"name":"test_lanes","status":"skipped"}]'
fi

scenario_results_json='[]'
scenario_hard_fail_count=0
scenario_warning_count=0
scenario_pass_count=0

for entry in "${SCENARIOS[@]}"; do
    scenario_name="${entry%%|*}"
    scenario_path="${entry#*|}"
    scenario_json="$SCENARIO_ARTIFACT_DIR/${scenario_name}.json"
    scenario_log="$SCENARIO_ARTIFACT_DIR/${scenario_name}.log"

    if [[ -n "$CHECK_ONLY_DIR" ]]; then
        scenario_json="$CHECK_ONLY_DIR/${scenario_name}.json"
        scenario_log="$CHECK_ONLY_DIR/${scenario_name}.log"
        if [[ ! -f "$scenario_json" ]]; then
            scenario_results_json="$(jq -c --arg scenario "$scenario_name" '
                . + [{
                    scenario: $scenario,
                    fixture: "check-only",
                    status: "hard_fail",
                    reasons: ["missing envelope JSON in check-only directory"],
                    warnings: [],
                    metrics: null
                }]
            ' <<<"$scenario_results_json")"
            scenario_hard_fail_count=$((scenario_hard_fail_count + 1))
            continue
        fi
    else
        mkdir -p "$(dirname "$scenario_json")"
        if ! build_scenario_envelope "$scenario_name" "$scenario_path" "$scenario_json" "$scenario_log"; then
            scenario_results_json="$(jq -c --arg scenario "$scenario_name" --arg fixture "$scenario_path" '
                . + [{
                    scenario: $scenario,
                    fixture: $fixture,
                    status: "hard_fail",
                    reasons: ["ft simulate run failed for scenario"],
                    warnings: [],
                    metrics: null
                }]
            ' <<<"$scenario_results_json")"
            scenario_hard_fail_count=$((scenario_hard_fail_count + 1))
            continue
        fi
    fi

    if ! jq -e '.mode == "resize_timeline_json"' "$scenario_json" >/dev/null 2>&1; then
        scenario_results_json="$(jq -c --arg scenario "$scenario_name" --arg fixture "$scenario_path" '
            . + [{
                scenario: $scenario,
                fixture: $fixture,
                status: "hard_fail",
                reasons: ["artifact mode must be resize_timeline_json"],
                warnings: [],
                metrics: null
            }]
        ' <<<"$scenario_results_json")"
        scenario_hard_fail_count=$((scenario_hard_fail_count + 1))
        continue
    fi

    IFS='|' read -r p50_ns p95_ns p99_ns <<<"$(compute_m1_percentiles "$scenario_json")"
    expectations_failed="$(read_metric_or_zero "$scenario_json" '.expectations_failed')"
    executed_events="$(read_metric_or_zero "$scenario_json" '.timeline.executed_resize_events')"
    queue_depth_before_max="$(read_queue_depth_peak "$scenario_json" 'depth_before')"
    queue_depth_after_max="$(read_queue_depth_peak "$scenario_json" 'depth_after')"

    scenario_status="pass"
    hard_reasons_json='[]'
    warning_reasons_json='[]'
    stage_metrics_json='{}'

    if (( expectations_failed > 0 )); then
        hard_reasons_json="$(jq -c --arg reason "expectations_failed > 0 (critical artifact budget breached)" '. + [$reason]' <<<"$hard_reasons_json")"
    fi

    if (( p50_ns > M1_P50_MAX_NS )); then
        hard_reasons_json="$(jq -c --arg reason "M1 p50 exceeds mid-tier threshold" '. + [$reason]' <<<"$hard_reasons_json")"
    fi
    if (( p95_ns > M1_P95_MAX_NS )); then
        hard_reasons_json="$(jq -c --arg reason "M1 p95 exceeds mid-tier threshold" '. + [$reason]' <<<"$hard_reasons_json")"
    fi
    if (( p99_ns > M1_P99_MAX_NS )); then
        hard_reasons_json="$(jq -c --arg reason "M1 p99 exceeds mid-tier threshold" '. + [$reason]' <<<"$hard_reasons_json")"
    fi

    # Warning classification for near-threshold metrics.
    if (( p50_ns > (M1_P50_MAX_NS * 85 / 100) )); then
        warning_reasons_json="$(jq -c --arg reason "M1 p50 within 15% of threshold" '. + [$reason]' <<<"$warning_reasons_json")"
    fi
    if (( p95_ns > (M1_P95_MAX_NS * 85 / 100) )); then
        warning_reasons_json="$(jq -c --arg reason "M1 p95 within 15% of threshold" '. + [$reason]' <<<"$warning_reasons_json")"
    fi
    if (( p99_ns > (M1_P99_MAX_NS * 85 / 100) )); then
        warning_reasons_json="$(jq -c --arg reason "M1 p99 within 15% of threshold" '. + [$reason]' <<<"$warning_reasons_json")"
    fi

    for stage in input_intent scheduler_queueing logical_reflow render_prep presentation; do
        stage_p95="$(read_stage_p95 "$scenario_json" "$stage")"
        stage_metrics_json="$(jq -c --arg stage "$stage" --argjson value "$stage_p95" '. + {($stage): $value}' <<<"$stage_metrics_json")"

        if (( stage_p95 < 0 )); then
            hard_reasons_json="$(jq -c --arg reason "stage_summary missing stage ${stage}" '. + [$reason]' <<<"$hard_reasons_json")"
            continue
        fi

        stage_threshold="${M2_P95_MAX_NS[$stage]}"
        if (( stage_p95 > stage_threshold )); then
            hard_reasons_json="$(jq -c --arg reason "M2 ${stage} p95 exceeds mid-tier threshold" '. + [$reason]' <<<"$hard_reasons_json")"
        elif (( stage_p95 > (stage_threshold * 85 / 100) )); then
            warning_reasons_json="$(jq -c --arg reason "M2 ${stage} p95 within 15% of threshold" '. + [$reason]' <<<"$warning_reasons_json")"
        fi
    done

    # Optional baseline drift checks.
    if [[ -f "$BASELINE_FILE" ]]; then
        baseline_p95="$(jq -r --arg s "$scenario_name" '.scenarios[$s].m1.p95_ns // 0' "$BASELINE_FILE" 2>/dev/null || echo 0)"
        baseline_p99="$(jq -r --arg s "$scenario_name" '.scenarios[$s].m1.p99_ns // 0' "$BASELINE_FILE" 2>/dev/null || echo 0)"

        if (( baseline_p95 > 0 )); then
            if float_gt "$p95_ns" "$(awk "BEGIN { print $baseline_p95 * $BASELINE_FAIL_MULTIPLIER }")"; then
                hard_reasons_json="$(jq -c --arg reason "baseline drift hard_fail: m1.p95_ns exceeds ${BASELINE_FAIL_MULTIPLIER}x baseline" '. + [$reason]' <<<"$hard_reasons_json")"
            elif float_gt "$p95_ns" "$(awk "BEGIN { print $baseline_p95 * $BASELINE_WARN_MULTIPLIER }")"; then
                warning_reasons_json="$(jq -c --arg reason "baseline drift warning: m1.p95_ns exceeds ${BASELINE_WARN_MULTIPLIER}x baseline" '. + [$reason]' <<<"$warning_reasons_json")"
            fi
        fi

        if (( baseline_p99 > 0 )); then
            if float_gt "$p99_ns" "$(awk "BEGIN { print $baseline_p99 * $BASELINE_FAIL_MULTIPLIER }")"; then
                hard_reasons_json="$(jq -c --arg reason "baseline drift hard_fail: m1.p99_ns exceeds ${BASELINE_FAIL_MULTIPLIER}x baseline" '. + [$reason]' <<<"$hard_reasons_json")"
            elif float_gt "$p99_ns" "$(awk "BEGIN { print $baseline_p99 * $BASELINE_WARN_MULTIPLIER }")"; then
                warning_reasons_json="$(jq -c --arg reason "baseline drift warning: m1.p99_ns exceeds ${BASELINE_WARN_MULTIPLIER}x baseline" '. + [$reason]' <<<"$warning_reasons_json")"
            fi
        fi
    fi

    hard_reason_count="$(jq -r 'length' <<<"$hard_reasons_json")"
    warning_reason_count="$(jq -r 'length' <<<"$warning_reasons_json")"

    if (( hard_reason_count > 0 )); then
        scenario_status="hard_fail"
        scenario_hard_fail_count=$((scenario_hard_fail_count + 1))
    elif (( warning_reason_count > 0 )); then
        scenario_status="warning"
        scenario_warning_count=$((scenario_warning_count + 1))
    else
        scenario_status="pass"
        scenario_pass_count=$((scenario_pass_count + 1))
    fi

    scenario_results_json="$(jq -c \
        --arg scenario "$scenario_name" \
        --arg fixture "$scenario_path" \
        --arg status "$scenario_status" \
        --argjson reasons "$hard_reasons_json" \
        --argjson warnings "$warning_reasons_json" \
        --argjson p50 "$p50_ns" \
        --argjson p95 "$p95_ns" \
        --argjson p99 "$p99_ns" \
        --argjson events "$executed_events" \
        --argjson expectations_failed "$expectations_failed" \
        --argjson queue_before "$queue_depth_before_max" \
        --argjson queue_after "$queue_depth_after_max" \
        --argjson stage_p95 "$stage_metrics_json" '
        . + [{
            scenario: $scenario,
            fixture: $fixture,
            status: $status,
            reasons: $reasons,
            warnings: $warnings,
            metrics: {
                m1: {
                    p50_ns: $p50,
                    p95_ns: $p95,
                    p99_ns: $p99,
                    p50_ms: ($p50 / 1000000.0),
                    p95_ms: ($p95 / 1000000.0),
                    p99_ms: ($p99 / 1000000.0)
                },
                m2: {
                    stage_p95_ns: $stage_p95
                },
                m3: {
                    critical_artifact_count: $expectations_failed,
                    minor_artifact_ratio: 0.0
                },
                resource_usage: {
                    scheduler_queue_depth_before_max: $queue_before,
                    scheduler_queue_depth_after_max: $queue_after,
                    executed_resize_events: $events
                }
            }
        }]
    ' <<<"$scenario_results_json")"
done

overall_status="pass"
if (( overall_step_failures > 0 || scenario_hard_fail_count > 0 )); then
    overall_status="hard_fail"
elif (( scenario_warning_count > 0 )); then
    overall_status="warning"
fi

generated_at="$(now_iso)"
cat >"$REPORT_FILE" <<EOF
{
  "version": "1",
  "format": "ft.resize.performance.gates.v1",
  "generated_at": "$generated_at",
  "mode": $([[ -n "$CHECK_ONLY_DIR" ]] && echo "\"check_only\"" || echo "\"run\""),
  "artifacts_dir": "$ARTIFACT_DIR",
  "baseline_file": "$BASELINE_FILE",
  "thresholds": {
    "tier": "mid",
    "m1_max_ns": {
      "p50": $M1_P50_MAX_NS,
      "p95": $M1_P95_MAX_NS,
      "p99": $M1_P99_MAX_NS
    },
    "m2_stage_p95_max_ns": {
      "input_intent": ${M2_P95_MAX_NS[input_intent]},
      "scheduler_queueing": ${M2_P95_MAX_NS[scheduler_queueing]},
      "logical_reflow": ${M2_P95_MAX_NS[logical_reflow]},
      "render_prep": ${M2_P95_MAX_NS[render_prep]},
      "presentation": ${M2_P95_MAX_NS[presentation]}
    },
    "m3": {
      "critical_artifact_count_max": 0,
      "minor_artifact_ratio_max": 0.001
    },
    "baseline_multipliers": {
      "warning": $BASELINE_WARN_MULTIPLIER,
      "hard_fail": $BASELINE_FAIL_MULTIPLIER
    }
  },
  "steps": $step_rows_json,
  "scenarios": $scenario_results_json,
  "summary": {
    "step_hard_fail_count": $overall_step_failures,
    "scenario_pass_count": $scenario_pass_count,
    "scenario_warning_count": $scenario_warning_count,
    "scenario_hard_fail_count": $scenario_hard_fail_count,
    "overall_status": "$overall_status"
  }
}
EOF

if [[ "$WRITE_BASELINE" == "true" ]]; then
    if [[ -z "$BASELINE_REASON" ]]; then
        echo "[resize-gates] ERROR: FT_RESIZE_GATE_BASELINE_REASON is required with --write-baseline" >&2
        exit 2
    fi

    baseline_dir="$(dirname "$BASELINE_FILE")"
    mkdir -p "$baseline_dir"
    commit_sha="$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")"

    jq -c '
        .scenarios
        | map({
            key: .scenario,
            value: {
                m1: .metrics.m1,
                m2_stage_p95_ns: .metrics.m2.stage_p95_ns,
                resource_usage: .metrics.resource_usage
            }
        })
        | from_entries
    ' "$REPORT_FILE" >"$ARTIFACT_DIR/baseline_scenarios.json"

    cat >"$BASELINE_FILE" <<EOF
{
  "version": "1",
  "format": "ft.resize.performance.baseline.v1",
  "tier": "mid",
  "updated_at": "$(now_iso)",
  "updated_by": "${USER:-unknown}",
  "updated_from_commit": "$commit_sha",
  "reason": "$(printf '%s' "$BASELINE_REASON" | sed 's/"/\\"/g')",
  "source_report": "$REPORT_FILE",
  "scenarios": $(cat "$ARTIFACT_DIR/baseline_scenarios.json")
}
EOF

    mkdir -p "$(dirname "$BASELINE_AUDIT_LOG")"
    jq -nc \
        --arg ts "$(now_iso)" \
        --arg user "${USER:-unknown}" \
        --arg commit "$commit_sha" \
        --arg reason "$BASELINE_REASON" \
        --arg baseline "$BASELINE_FILE" \
        --arg report "$REPORT_FILE" '
        {
          ts: $ts,
          action: "baseline_refresh",
          user: $user,
          commit: $commit,
          reason: $reason,
          baseline_file: $baseline,
          source_report: $report
        }
    ' >>"$BASELINE_AUDIT_LOG"

    echo "[resize-gates] baseline refreshed: $BASELINE_FILE"
    echo "[resize-gates] baseline audit log: $BASELINE_AUDIT_LOG"
fi

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    {
        echo "## Resize Performance Gates"
        echo ""
        echo "| Scenario | Status | M1 p95 (ms) | M1 p99 (ms) |"
        echo "|----------|--------|-------------|-------------|"
        jq -r '.scenarios[] | "| \(.scenario) | \(.status) | \(.metrics.m1.p95_ms) | \(.metrics.m1.p99_ms) |"' "$REPORT_FILE"
        echo ""
        echo "**Overall status:** \`$(jq -r '.summary.overall_status' "$REPORT_FILE")\`"
        echo ""
        echo "Report: \`$REPORT_FILE\`"
    } >>"$GITHUB_STEP_SUMMARY"
fi

echo "[resize-gates] report: $REPORT_FILE"
echo "[resize-gates] overall status: $overall_status"

if [[ "$overall_status" == "hard_fail" ]]; then
    exit 1
fi

exit 0
