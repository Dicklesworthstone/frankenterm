#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/check_visual_artifact_detector.sh --run-dir <path> [options]

Options:
  --run-dir <path>         E2E run directory containing scenarios/*/visual_artifact_report.json
  --output <path>          Output summary path (default: <run-dir>/visual_artifact_summary.json)
  --history-file <path>    Append run metrics to history JSONL and compute trend block
                           (default: VISUAL_ARTIFACT_HISTORY_FILE or <run-dir>/visual_artifact_history.jsonl)
  --trend-window <num>     Number of most-recent history entries for trend stats
                           (default: VISUAL_ARTIFACT_TREND_WINDOW or 20)
  --warn-threshold <num>   Warning score threshold (default: VISUAL_ARTIFACT_WARN_THRESHOLD or 0.35)
  --fail-threshold <num>   Failure score threshold (default: VISUAL_ARTIFACT_FAIL_THRESHOLD or 0.65)
  --strict-warn            Exit non-zero when warnings are present (failures still use exit 2)
  --help                   Show this message
EOF
}

RUN_DIR=""
OUTPUT=""
HISTORY_FILE="${VISUAL_ARTIFACT_HISTORY_FILE:-}"
TREND_WINDOW="${VISUAL_ARTIFACT_TREND_WINDOW:-20}"
WARN_THRESHOLD="${VISUAL_ARTIFACT_WARN_THRESHOLD:-0.35}"
FAIL_THRESHOLD="${VISUAL_ARTIFACT_FAIL_THRESHOLD:-0.65}"
STRICT_WARN=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --run-dir)
            RUN_DIR="${2:-}"
            shift 2
            ;;
        --output)
            OUTPUT="${2:-}"
            shift 2
            ;;
        --history-file)
            HISTORY_FILE="${2:-}"
            shift 2
            ;;
        --trend-window)
            TREND_WINDOW="${2:-}"
            shift 2
            ;;
        --warn-threshold)
            WARN_THRESHOLD="${2:-}"
            shift 2
            ;;
        --fail-threshold)
            FAIL_THRESHOLD="${2:-}"
            shift 2
            ;;
        --strict-warn)
            STRICT_WARN=true
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "[visual_artifact_detector] unknown argument: $1" >&2
            usage >&2
            exit 64
            ;;
    esac
done

if [[ -z "$RUN_DIR" ]]; then
    echo "[visual_artifact_detector] --run-dir is required" >&2
    usage >&2
    exit 64
fi

if [[ ! -d "$RUN_DIR" ]]; then
    echo "[visual_artifact_detector] run directory does not exist: $RUN_DIR" >&2
    exit 66
fi

if [[ -z "$OUTPUT" ]]; then
    OUTPUT="$RUN_DIR/visual_artifact_summary.json"
fi

if [[ -z "$HISTORY_FILE" ]]; then
    HISTORY_FILE="$RUN_DIR/visual_artifact_history.jsonl"
fi

SCENARIOS_DIR="$RUN_DIR/scenarios"
mkdir -p "$(dirname "$OUTPUT")"
mkdir -p "$(dirname "$HISTORY_FILE")"

if [[ ! -d "$SCENARIOS_DIR" ]]; then
    jq -n \
        --arg schema_version "wa.visual_artifact_summary.v1" \
        --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg run_dir "$RUN_DIR" \
        --arg history_file "$HISTORY_FILE" \
        --arg trend_window "$TREND_WINDOW" \
        --arg warn_threshold "$WARN_THRESHOLD" \
        --arg fail_threshold "$FAIL_THRESHOLD" \
        '{
            schema_version: $schema_version,
            generated_at: $generated_at,
            run_dir: $run_dir,
            thresholds: {
                warn: ($warn_threshold | tonumber? // 0.35),
                fail: ($fail_threshold | tonumber? // 0.65)
            },
            status: "no_data",
            counts: {
                scenarios: 0,
                pass: 0,
                warn: 0,
                fail: 0
            },
            quality: {
                false_positive_estimate: {
                    numerator: 0,
                    denominator: 0,
                    rate: 0.0,
                    method: "alerts_without_failure_signature / alerts"
                },
                false_negative_estimate: {
                    numerator: 0,
                    denominator: 0,
                    rate: 0.0,
                    method: "pass_class_with_failure_signature / scenarios"
                }
            },
            trend: {
                history_file: $history_file,
                window_size: ($trend_window | tonumber? // 20),
                sample_count: 0,
                current_false_positive_rate: 0.0,
                previous_false_positive_rate: null,
                delta_from_previous: null,
                moving_average_false_positive_rate: null
            },
            scenarios: []
        }' > "$OUTPUT"
    echo "[visual_artifact_detector] no scenarios directory at $SCENARIOS_DIR" >&2
    exit 0
fi

REPORTS=()
while IFS= read -r report_path; do
    REPORTS+=("$report_path")
done < <(find "$SCENARIOS_DIR" -maxdepth 2 -type f -name 'visual_artifact_report.json' | LC_ALL=C sort)

PASS_COUNT=0
WARN_COUNT=0
FAIL_COUNT=0
ALERT_COUNT=0
FP_ESTIMATE_COUNT=0
FN_ESTIMATE_COUNT=0
SCENARIOS_JSON="[]"

for report in "${REPORTS[@]}"; do
    scenario_id=$(jq -r '.test_case_id // empty' "$report" 2>/dev/null || echo "")
    if [[ -z "$scenario_id" ]]; then
        scenario_id="$(basename "$(dirname "$report")")"
    fi

    score=$(jq -r '.severity.score // .metrics.score // 0' "$report" 2>/dev/null || echo "0")
    klass=$(jq -r '.severity.class // empty' "$report" 2>/dev/null || echo "")
    has_failure_signature=$(jq -r '.evidence.failure_signature != null' "$report" 2>/dev/null || echo "false")
    if [[ -z "$klass" || "$klass" == "null" ]]; then
        klass=$(awk -v s="$score" -v w="$WARN_THRESHOLD" -v f="$FAIL_THRESHOLD" '
            BEGIN {
                if (s >= f) print "fail";
                else if (s >= w) print "warn";
                else print "pass";
            }
        ')
    fi

    case "$klass" in
        fail) ((FAIL_COUNT++)) || true ;;
        warn) ((WARN_COUNT++)) || true ;;
        *) klass="pass"; ((PASS_COUNT++)) || true ;;
    esac

    if [[ "$klass" != "pass" ]]; then
        ((ALERT_COUNT++)) || true
        if [[ "$has_failure_signature" != "true" ]]; then
            ((FP_ESTIMATE_COUNT++)) || true
        fi
    elif [[ "$has_failure_signature" == "true" ]]; then
        ((FN_ESTIMATE_COUNT++)) || true
    fi

    report_rel="${report#"${RUN_DIR}"/}"
    SCENARIOS_JSON=$(jq -c \
        --arg scenario_id "$scenario_id" \
        --arg score "$score" \
        --arg class "$klass" \
        --arg has_failure_signature "$has_failure_signature" \
        --arg report "$report_rel" \
        '. + [{
            test_case_id: $scenario_id,
            score: ($score | tonumber? // 0),
            class: $class,
            has_failure_signature: ($has_failure_signature == "true"),
            report: $report
        }]' <<< "$SCENARIOS_JSON")
done

SCENARIOS_JSON=$(jq -c 'sort_by(.score) | reverse' <<< "$SCENARIOS_JSON")
SCENARIO_COUNT=$(jq -r 'length' <<< "$SCENARIOS_JSON")

FP_RATE=$(awk -v n="$FP_ESTIMATE_COUNT" -v d="$ALERT_COUNT" 'BEGIN { if (d <= 0) print "0"; else printf "%.6f", n / d }')
FN_RATE=$(awk -v n="$FN_ESTIMATE_COUNT" -v d="$SCENARIO_COUNT" 'BEGIN { if (d <= 0) print "0"; else printf "%.6f", n / d }')

CURRENT_ENTRY=$(jq -c -n \
    --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg run_dir "$RUN_DIR" \
    --argjson scenario_count "$SCENARIO_COUNT" \
    --argjson pass_count "$PASS_COUNT" \
    --argjson warn_count "$WARN_COUNT" \
    --argjson fail_count "$FAIL_COUNT" \
    --argjson alert_count "$ALERT_COUNT" \
    --argjson fp_count "$FP_ESTIMATE_COUNT" \
    --argjson fn_count "$FN_ESTIMATE_COUNT" \
    --arg fp_rate "$FP_RATE" \
    --arg fn_rate "$FN_RATE" \
    '{
        generated_at: $generated_at,
        run_dir: $run_dir,
        counts: {
            scenarios: $scenario_count,
            pass: $pass_count,
            warn: $warn_count,
            fail: $fail_count,
            alerts: $alert_count
        },
        quality: {
            false_positive_estimate: {
                numerator: $fp_count,
                denominator: $alert_count,
                rate: ($fp_rate | tonumber)
            },
            false_negative_estimate: {
                numerator: $fn_count,
                denominator: $scenario_count,
                rate: ($fn_rate | tonumber)
            }
        }
    }')

if [[ -n "$HISTORY_FILE" ]]; then
    printf '%s\n' "$CURRENT_ENTRY" >> "$HISTORY_FILE"
fi

TREND_JSON=$(jq -cn \
    --arg history_file "$HISTORY_FILE" \
    --arg trend_window "$TREND_WINDOW" \
    --arg fp_rate "$FP_RATE" \
    '{
        history_file: $history_file,
        window_size: ($trend_window | tonumber? // 20),
        sample_count: 0,
        current_false_positive_rate: ($fp_rate | tonumber),
        previous_false_positive_rate: null,
        delta_from_previous: null,
        moving_average_false_positive_rate: null
    }')

if [[ -f "$HISTORY_FILE" ]]; then
    TREND_JSON=$(jq -c \
        --arg history_file "$HISTORY_FILE" \
        --arg trend_window "$TREND_WINDOW" \
        --slurpfile history "$HISTORY_FILE" \
        '
        ($trend_window | tonumber? // 20) as $window
        | ($history // []) as $all
        | ($all | if length > $window then .[-$window:] else . end) as $window_entries
        | ($window_entries | length) as $samples
        | ($window_entries[-1].quality.false_positive_estimate.rate // 0) as $current
        | ($window_entries[-2].quality.false_positive_estimate.rate // null) as $previous
        | (if $samples > 0 then ($window_entries | map(.quality.false_positive_estimate.rate // 0) | add / $samples) else null end) as $avg
        | {
            history_file: $history_file,
            window_size: $window,
            sample_count: $samples,
            current_false_positive_rate: $current,
            previous_false_positive_rate: $previous,
            delta_from_previous: (if $previous == null then null else ($current - $previous) end),
            moving_average_false_positive_rate: $avg
          }
        ' <<< '{}' 2>/dev/null || echo "$TREND_JSON")
fi

jq -n \
    --arg schema_version "wa.visual_artifact_summary.v1" \
    --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg run_dir "$RUN_DIR" \
    --arg history_file "$HISTORY_FILE" \
    --arg trend_window "$TREND_WINDOW" \
    --arg warn_threshold "$WARN_THRESHOLD" \
    --arg fail_threshold "$FAIL_THRESHOLD" \
    --argjson scenario_count "$SCENARIO_COUNT" \
    --argjson pass_count "$PASS_COUNT" \
    --argjson warn_count "$WARN_COUNT" \
    --argjson fail_count "$FAIL_COUNT" \
    --argjson alert_count "$ALERT_COUNT" \
    --argjson fp_count "$FP_ESTIMATE_COUNT" \
    --argjson fn_count "$FN_ESTIMATE_COUNT" \
    --arg fp_rate "$FP_RATE" \
    --arg fn_rate "$FN_RATE" \
    --argjson trend "$TREND_JSON" \
    --argjson scenarios "$SCENARIOS_JSON" \
    '{
        schema_version: $schema_version,
        generated_at: $generated_at,
        run_dir: $run_dir,
        thresholds: {
            warn: ($warn_threshold | tonumber? // 0.35),
            fail: ($fail_threshold | tonumber? // 0.65)
        },
        status: (
            if $fail_count > 0 then "fail"
            elif $warn_count > 0 then "warn"
            elif $scenario_count == 0 then "no_data"
            else "pass"
            end
        ),
        counts: {
            scenarios: $scenario_count,
            pass: $pass_count,
            warn: $warn_count,
            fail: $fail_count,
            alerts: $alert_count
        },
        quality: {
            false_positive_estimate: {
                numerator: $fp_count,
                denominator: $alert_count,
                rate: ($fp_rate | tonumber),
                method: "alerts_without_failure_signature / alerts"
            },
            false_negative_estimate: {
                numerator: $fn_count,
                denominator: $scenario_count,
                rate: ($fn_rate | tonumber),
                method: "pass_class_with_failure_signature / scenarios"
            }
        },
        trend: $trend,
        scenarios: $scenarios
    }' > "$OUTPUT"

echo "[visual_artifact_detector] summary written: $OUTPUT" >&2
echo "[visual_artifact_detector] counts: pass=$PASS_COUNT warn=$WARN_COUNT fail=$FAIL_COUNT" >&2

if [[ "$FAIL_COUNT" -gt 0 ]]; then
    exit 2
fi

if [[ "$STRICT_WARN" == "true" && "$WARN_COUNT" -gt 0 ]]; then
    exit 1
fi

exit 0
