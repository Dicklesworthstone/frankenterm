#!/usr/bin/env bash
# =============================================================================
# CI gate: interactive input-latency under resize/font storms (ft-1u90p.7.8)
#
# Validates typing/paste/mouse interaction latency envelopes from simulation
# timeline artifacts, emits per-event causality JSONL, and enforces
# p50/p95/p99 interaction lag budgets with failure artifacts for regressions.
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

ARTIFACT_DIR="${FT_INPUT_LATENCY_ARTIFACT_DIR:-target/input-latency-gates}"
TARGET_DIR="${FT_INPUT_LATENCY_TARGET_DIR:-target-input-latency-gates}"
CHECK_ONLY_DIR=""
RCH_MODE="${FT_INPUT_LATENCY_RCH_MODE:-auto}"
WARN_NEAR_RATIO="${FT_INPUT_LATENCY_WARN_NEAR_RATIO:-0.90}"

# Global stage budget (queueing is the most direct pressure signal).
QUEUE_WAIT_P95_MAX_MS="${FT_INPUT_LATENCY_QUEUE_WAIT_P95_MAX_MS:-120}"

# scenario|fixture|p50_max_ms|p95_max_ms|p99_max_ms
SCENARIOS=(
    "input_latency_happy_path|fixtures/simulations/input_latency/input_latency_happy_path.yaml|20|60|120"
    "input_latency_adversarial_storm|fixtures/simulations/input_latency/input_latency_adversarial_storm.yaml|30|90|180"
    "input_latency_regression_resize_wrap_jitter_2026_02|fixtures/simulations/input_latency/input_latency_regression_resize_wrap_jitter_2026_02.yaml|35|100|200"
)

usage() {
    cat <<EOF
Usage: $0 [OPTIONS]

Options:
  --check-only DIR     Evaluate pre-generated scenario envelopes in DIR.
                       Expected files: <DIR>/<scenario-name>.json
  --artifacts-dir DIR  Override artifacts directory (default: $ARTIFACT_DIR)
  --target-dir DIR     Override cargo target dir (default: $TARGET_DIR)
  --no-rch             Do not use rch even if available
  -h, --help           Show help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --check-only)
            CHECK_ONLY_DIR="$2"
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
        --no-rch)
            RCH_MODE="never"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "[input-latency-gates] Unknown arg: $1" >&2
            usage
            exit 3
            ;;
    esac
done

if ! command -v jq >/dev/null 2>&1; then
    echo "[input-latency-gates] ERROR: jq is required" >&2
    exit 5
fi

if [[ -z "$CHECK_ONLY_DIR" ]] && ! command -v cargo >/dev/null 2>&1; then
    echo "[input-latency-gates] ERROR: cargo is required" >&2
    exit 5
fi

mkdir -p "$ARTIFACT_DIR"
SCENARIO_ARTIFACT_DIR="$ARTIFACT_DIR/scenarios"
mkdir -p "$SCENARIO_ARTIFACT_DIR"
REPORT_FILE="$ARTIFACT_DIR/input-latency-report.json"

now_iso() {
    date -u +"%Y-%m-%dT%H:%M:%SZ"
}

float_gt() {
    awk "BEGIN { exit !($1 > $2) }"
}

run_simulate() {
    local fixture="$1"
    local out_json="$2"
    local out_log="$3"

    local -a cmd=(cargo run -p frankenterm -- simulate run "$fixture" --json --resize-timeline-json)

    if [[ "$RCH_MODE" != "never" ]] && command -v rch >/dev/null 2>&1; then
        rch exec -- env CARGO_TARGET_DIR="$TARGET_DIR" "${cmd[@]}" >"$out_json" 2>"$out_log"
    else
        env CARGO_TARGET_DIR="$TARGET_DIR" "${cmd[@]}" >"$out_json" 2>"$out_log"
    fi
}

build_correlation_jsonl() {
    local scenario_name="$1"
    local envelope_json="$2"
    local out_jsonl="$3"

    jq -cr --arg scenario "$scenario_name" '
        (.timeline.events // [])[] as $event
        | (($event.stages // [])
            | map(select(.stage == "scheduler_queueing") | (.queue_metrics // {}))
            | .[0] // {}) as $queue
        | {
            test_case_id: ($event.test_case_id // $scenario),
            resize_transaction_id: $event.resize_transaction_id,
            pane_id: $event.pane_id,
            tab_id: $event.tab_id,
            sequence_no: $event.sequence_no,
            scheduler_decision: $event.scheduler_decision,
            frame_id: $event.frame_id,
            action: ($event.action // "unknown"),
            queue_wait_ms: ($event.queue_wait_ms // 0),
            reflow_ms: ($event.reflow_ms // 0),
            render_ms: ($event.render_ms // 0),
            present_ms: ($event.present_ms // 0),
            total_lag_ms: (((($event.total_duration_ns // 0) / 1000000.0) * 1000) | round / 1000),
            scheduled_at_ms: (((($event.scheduled_at_ns // 0) / 1000000.0) * 1000) | round / 1000),
            dispatch_offset_ms: (((($event.dispatch_offset_ns // 0) / 1000000.0) * 1000) | round / 1000),
            queue_depth_before: ($queue.depth_before // null),
            queue_depth_after: ($queue.depth_after // null),
            causality_id: ($scenario + ":" + (($event.event_index // 0) | tostring)),
            causality_parent_id: (
                if (($event.event_index // 0) > 0)
                then ($scenario + ":" + (((($event.event_index // 0) - 1) | tostring)))
                else null
                end
            )
        }
    ' "$envelope_json" >"$out_jsonl"
}

build_metrics_json() {
    local correlation_jsonl="$1"
    local out_metrics="$2"

    jq -s '
        def percentile($arr; $pct):
            if ($arr | length) == 0 then 0
            else ($arr | sort | .[((((length - 1) * $pct) / 100) | floor)])
            end;

        . as $rows
        | ($rows | map(.total_lag_ms // 0)) as $lag
        | ($rows | map((.queue_wait_ms // 0) | tonumber)) as $queue
        | ($rows | map((.reflow_ms // 0) | tonumber)) as $reflow
        | ($rows | map((.render_ms // 0) | tonumber)) as $render
        | ($rows | map((.present_ms // 0) | tonumber)) as $present
        | {
            event_count: ($rows | length),
            interaction_lag_ms: {
                p50: percentile($lag; 50),
                p95: percentile($lag; 95),
                p99: percentile($lag; 99)
            },
            stage_lag_ms: {
                queue_wait: {
                    p50: percentile($queue; 50),
                    p95: percentile($queue; 95),
                    p99: percentile($queue; 99)
                },
                reflow: {
                    p50: percentile($reflow; 50),
                    p95: percentile($reflow; 95),
                    p99: percentile($reflow; 99)
                },
                render: {
                    p50: percentile($render; 50),
                    p95: percentile($render; 95),
                    p99: percentile($render; 99)
                },
                present: {
                    p50: percentile($present; 50),
                    p95: percentile($present; 95),
                    p99: percentile($present; 99)
                }
            },
            scheduler: {
                decision_counts: (
                    $rows
                    | map(.scheduler_decision // "unknown")
                    | group_by(.)
                    | map({decision: .[0], count: length})
                ),
                queue_depth_before_max: ($rows | map(.queue_depth_before // 0) | max // 0),
                queue_depth_after_max: ($rows | map(.queue_depth_after // 0) | max // 0)
            },
            actions: (
                $rows
                | map(.action // "unknown")
                | group_by(.)
                | map({action: .[0], count: length})
            )
        }
    ' "$correlation_jsonl" >"$out_metrics"
}

emit_failure_artifacts() {
    local scenario_name="$1"
    local scenario_dir="$2"
    local hard_reasons_json="$3"
    local metrics_json="$4"

    local correlation_jsonl="$scenario_dir/correlation.jsonl"
    local trace_bundle="$scenario_dir/trace_bundle.json"
    local frame_histogram="$scenario_dir/frame_histogram.json"
    local failure_signature="$scenario_dir/failure_signature.json"

    local top_events_json
    top_events_json="$(jq -s 'sort_by(-(.total_lag_ms // 0)) | .[0:20]' "$correlation_jsonl")"

    jq -n \
        --arg schema_version "wa.trace_bundle.v1" \
        --arg generated_at "$(now_iso)" \
        --arg test_case_id "$scenario_name" \
        --argjson reasons "$hard_reasons_json" \
        --argjson metrics "$(cat "$metrics_json")" \
        --argjson top_events "$top_events_json" \
        '{
            schema_version: $schema_version,
            generated_at: $generated_at,
            test_case_id: $test_case_id,
            failure_signature: "input_latency_budget_violation",
            reasons: $reasons,
            metrics: $metrics,
            top_events: $top_events
        }' >"$trace_bundle"

    local frame_count dropped_count buckets_json
    frame_count="$(jq -s 'length' "$correlation_jsonl")"
    dropped_count="$(jq -s '[.[] | select((.total_lag_ms // 0) >= 33)] | length' "$correlation_jsonl")"
    buckets_json="$(jq -s '
        map({ms: ((.total_lag_ms // 0) | floor), count: 1})
        | group_by(.ms)
        | map({ms: .[0].ms, count: length})
    ' "$correlation_jsonl")"

    jq -n \
        --arg schema_version "wa.frame_histogram.v1" \
        --arg generated_at "$(now_iso)" \
        --arg test_case_id "$scenario_name" \
        --argjson frame_count "$frame_count" \
        --argjson dropped_frame_count "$dropped_count" \
        --argjson bucket_ms "$buckets_json" \
        '{
            schema_version: $schema_version,
            generated_at: $generated_at,
            test_case_id: $test_case_id,
            histogram: {
                frame_count: $frame_count,
                dropped_frame_count: $dropped_frame_count,
                bucket_ms: $bucket_ms
            }
        }' >"$frame_histogram"

    jq -n \
        --arg schema_version "wa.failure_signature.v1" \
        --arg generated_at "$(now_iso)" \
        --arg test_case_id "$scenario_name" \
        --arg signature "input_latency_budget_violation" \
        --argjson reasons "$hard_reasons_json" \
        '{
            schema_version: $schema_version,
            generated_at: $generated_at,
            test_case_id: $test_case_id,
            signature: $signature,
            reasons: $reasons
        }' >"$failure_signature"
}

scenario_rows_json='[]'
scenario_pass_count=0
scenario_warning_count=0
scenario_hard_fail_count=0

for entry in "${SCENARIOS[@]}"; do
    scenario_name="${entry%%|*}"
    rest="${entry#*|}"
    fixture_path="${rest%%|*}"
    rest="${rest#*|}"
    p50_budget_ms="${rest%%|*}"
    rest="${rest#*|}"
    p95_budget_ms="${rest%%|*}"
    rest="${rest#*|}"
    p99_budget_ms="$rest"

    scenario_dir="$SCENARIO_ARTIFACT_DIR/$scenario_name"
    mkdir -p "$scenario_dir"

    envelope_json="$scenario_dir/${scenario_name}.json"
    run_log="$scenario_dir/${scenario_name}.log"

    if [[ -n "$CHECK_ONLY_DIR" ]]; then
        envelope_json="$CHECK_ONLY_DIR/${scenario_name}.json"
        run_log="$CHECK_ONLY_DIR/${scenario_name}.log"
        if [[ ! -f "$envelope_json" ]]; then
            scenario_rows_json="$(jq -c --arg scenario "$scenario_name" --arg fixture "check-only" '
                . + [{
                    scenario: $scenario,
                    fixture: $fixture,
                    status: "hard_fail",
                    reasons: ["missing envelope JSON in check-only directory"],
                    warnings: [],
                    metrics: null
                }]
            ' <<<"$scenario_rows_json")"
            scenario_hard_fail_count=$((scenario_hard_fail_count + 1))
            continue
        fi
    else
        if ! run_simulate "$fixture_path" "$envelope_json" "$run_log"; then
            scenario_rows_json="$(jq -c --arg scenario "$scenario_name" --arg fixture "$fixture_path" '
                . + [{
                    scenario: $scenario,
                    fixture: $fixture,
                    status: "hard_fail",
                    reasons: ["simulate run failed"],
                    warnings: [],
                    metrics: null
                }]
            ' <<<"$scenario_rows_json")"
            scenario_hard_fail_count=$((scenario_hard_fail_count + 1))
            continue
        fi
    fi

    if ! jq -e '.mode == "resize_timeline_json"' "$envelope_json" >/dev/null 2>&1; then
        scenario_rows_json="$(jq -c --arg scenario "$scenario_name" --arg fixture "$fixture_path" '
            . + [{
                scenario: $scenario,
                fixture: $fixture,
                status: "hard_fail",
                reasons: ["artifact mode must be resize_timeline_json"],
                warnings: [],
                metrics: null
            }]
        ' <<<"$scenario_rows_json")"
        scenario_hard_fail_count=$((scenario_hard_fail_count + 1))
        continue
    fi

    local_envelope="$scenario_dir/envelope.json"
    if [[ "$envelope_json" != "$local_envelope" ]]; then
        cp "$envelope_json" "$local_envelope"
    fi

    correlation_jsonl="$scenario_dir/correlation.jsonl"
    metrics_json="$scenario_dir/latency_metrics.json"

    build_correlation_jsonl "$scenario_name" "$local_envelope" "$correlation_jsonl"
    build_metrics_json "$correlation_jsonl" "$metrics_json"

    event_count="$(jq -r '.event_count // 0' "$metrics_json")"
    expectations_failed="$(jq -r '.expectations_failed // 0' "$local_envelope")"
    p50_ms="$(jq -r '.interaction_lag_ms.p50 // 0' "$metrics_json")"
    p95_ms="$(jq -r '.interaction_lag_ms.p95 // 0' "$metrics_json")"
    p99_ms="$(jq -r '.interaction_lag_ms.p99 // 0' "$metrics_json")"
    queue_wait_p95_ms="$(jq -r '.stage_lag_ms.queue_wait.p95 // 0' "$metrics_json")"

    hard_reasons_json='[]'
    warning_reasons_json='[]'

    if (( event_count == 0 )); then
        hard_reasons_json="$(jq -c '. + ["no timeline events captured"]' <<<"$hard_reasons_json")"
    fi

    if (( expectations_failed > 0 )); then
        hard_reasons_json="$(jq -c --arg reason "expectations_failed > 0" '. + [$reason]' <<<"$hard_reasons_json")"
    fi

    if float_gt "$p50_ms" "$p50_budget_ms"; then
        hard_reasons_json="$(jq -c --arg reason "interaction lag p50 exceeds budget" '. + [$reason]' <<<"$hard_reasons_json")"
    elif float_gt "$p50_ms" "$(awk "BEGIN { print $p50_budget_ms * $WARN_NEAR_RATIO }")"; then
        warning_reasons_json="$(jq -c --arg reason "interaction lag p50 near budget" '. + [$reason]' <<<"$warning_reasons_json")"
    fi

    if float_gt "$p95_ms" "$p95_budget_ms"; then
        hard_reasons_json="$(jq -c --arg reason "interaction lag p95 exceeds budget" '. + [$reason]' <<<"$hard_reasons_json")"
    elif float_gt "$p95_ms" "$(awk "BEGIN { print $p95_budget_ms * $WARN_NEAR_RATIO }")"; then
        warning_reasons_json="$(jq -c --arg reason "interaction lag p95 near budget" '. + [$reason]' <<<"$warning_reasons_json")"
    fi

    if float_gt "$p99_ms" "$p99_budget_ms"; then
        hard_reasons_json="$(jq -c --arg reason "interaction lag p99 exceeds budget" '. + [$reason]' <<<"$hard_reasons_json")"
    elif float_gt "$p99_ms" "$(awk "BEGIN { print $p99_budget_ms * $WARN_NEAR_RATIO }")"; then
        warning_reasons_json="$(jq -c --arg reason "interaction lag p99 near budget" '. + [$reason]' <<<"$warning_reasons_json")"
    fi

    if float_gt "$queue_wait_p95_ms" "$QUEUE_WAIT_P95_MAX_MS"; then
        hard_reasons_json="$(jq -c --arg reason "queue_wait p95 exceeds budget" '. + [$reason]' <<<"$hard_reasons_json")"
    elif float_gt "$queue_wait_p95_ms" "$(awk "BEGIN { print $QUEUE_WAIT_P95_MAX_MS * $WARN_NEAR_RATIO }")"; then
        warning_reasons_json="$(jq -c --arg reason "queue_wait p95 near budget" '. + [$reason]' <<<"$warning_reasons_json")"
    fi

    status="pass"
    if (( $(jq -r 'length' <<<"$hard_reasons_json") > 0 )); then
        status="hard_fail"
        scenario_hard_fail_count=$((scenario_hard_fail_count + 1))
        emit_failure_artifacts "$scenario_name" "$scenario_dir" "$hard_reasons_json" "$metrics_json"
    elif (( $(jq -r 'length' <<<"$warning_reasons_json") > 0 )); then
        status="warning"
        scenario_warning_count=$((scenario_warning_count + 1))
    else
        scenario_pass_count=$((scenario_pass_count + 1))
    fi

    scenario_rows_json="$(jq -c \
        --arg scenario "$scenario_name" \
        --arg fixture "$fixture_path" \
        --arg status "$status" \
        --argjson reasons "$hard_reasons_json" \
        --argjson warnings "$warning_reasons_json" \
        --argjson metrics "$(cat "$metrics_json")" \
        --argjson thresholds "$(jq -n --argjson p50 "$p50_budget_ms" --argjson p95 "$p95_budget_ms" --argjson p99 "$p99_budget_ms" --argjson queue_p95 "$QUEUE_WAIT_P95_MAX_MS" '{interaction_lag_ms:{p50_max:$p50,p95_max:$p95,p99_max:$p99},queue_wait_p95_max_ms:$queue_p95}')" \
        '. + [{
            scenario: $scenario,
            fixture: $fixture,
            status: $status,
            reasons: $reasons,
            warnings: $warnings,
            thresholds: $thresholds,
            metrics: $metrics,
            artifacts: {
                envelope_json: ("scenarios/" + $scenario + "/envelope.json"),
                correlation_jsonl: ("scenarios/" + $scenario + "/correlation.jsonl"),
                latency_metrics_json: ("scenarios/" + $scenario + "/latency_metrics.json"),
                trace_bundle: ("scenarios/" + $scenario + "/trace_bundle.json"),
                frame_histogram: ("scenarios/" + $scenario + "/frame_histogram.json"),
                failure_signature: ("scenarios/" + $scenario + "/failure_signature.json")
            }
        }]' <<<"$scenario_rows_json")"

done

overall_status="pass"
if (( scenario_hard_fail_count > 0 )); then
    overall_status="hard_fail"
elif (( scenario_warning_count > 0 )); then
    overall_status="warning"
fi

cat >"$REPORT_FILE" <<EOF
{
  "version": "1",
  "format": "ft.input_latency.gates.v1",
  "generated_at": "$(now_iso)",
  "mode": $([[ -n "$CHECK_ONLY_DIR" ]] && echo "\"check_only\"" || echo "\"run\""),
  "artifacts_dir": "$ARTIFACT_DIR",
  "thresholds": {
    "warn_near_ratio": $WARN_NEAR_RATIO,
    "queue_wait_p95_max_ms": $QUEUE_WAIT_P95_MAX_MS
  },
  "scenarios": $scenario_rows_json,
  "summary": {
    "scenario_pass_count": $scenario_pass_count,
    "scenario_warning_count": $scenario_warning_count,
    "scenario_hard_fail_count": $scenario_hard_fail_count,
    "overall_status": "$overall_status"
  }
}
EOF

echo "[input-latency-gates] report: $REPORT_FILE"
echo "[input-latency-gates] overall status: $overall_status"

if [[ "$overall_status" == "hard_fail" ]]; then
    exit 1
fi

exit 0
