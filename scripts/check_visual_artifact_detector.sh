#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/check_visual_artifact_detector.sh --run-dir <path> [options]

Options:
  --run-dir <path>         E2E run directory containing scenarios/*/visual_artifact_report.json
  --output <path>          Output summary path (default: <run-dir>/visual_artifact_summary.json)
  --warn-threshold <num>   Warning score threshold (default: VISUAL_ARTIFACT_WARN_THRESHOLD or 0.35)
  --fail-threshold <num>   Failure score threshold (default: VISUAL_ARTIFACT_FAIL_THRESHOLD or 0.65)
  --strict-warn            Exit non-zero when warnings are present (failures still use exit 2)
  --help                   Show this message
EOF
}

RUN_DIR=""
OUTPUT=""
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

SCENARIOS_DIR="$RUN_DIR/scenarios"
mkdir -p "$(dirname "$OUTPUT")"

if [[ ! -d "$SCENARIOS_DIR" ]]; then
    jq -n \
        --arg schema_version "wa.visual_artifact_summary.v1" \
        --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg run_dir "$RUN_DIR" \
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
SCENARIOS_JSON="[]"

for report in "${REPORTS[@]}"; do
    scenario_id=$(jq -r '.test_case_id // empty' "$report" 2>/dev/null || echo "")
    if [[ -z "$scenario_id" ]]; then
        scenario_id="$(basename "$(dirname "$report")")"
    fi

    score=$(jq -r '.severity.score // .metrics.score // 0' "$report" 2>/dev/null || echo "0")
    klass=$(jq -r '.severity.class // empty' "$report" 2>/dev/null || echo "")
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

    report_rel="${report#$RUN_DIR/}"
    SCENARIOS_JSON=$(jq -c \
        --arg scenario_id "$scenario_id" \
        --arg score "$score" \
        --arg class "$klass" \
        --arg report "$report_rel" \
        '. + [{
            test_case_id: $scenario_id,
            score: ($score | tonumber? // 0),
            class: $class,
            report: $report
        }]' <<< "$SCENARIOS_JSON")
done

SCENARIOS_JSON=$(jq -c 'sort_by(.score) | reverse' <<< "$SCENARIOS_JSON")
SCENARIO_COUNT=$(jq -r 'length' <<< "$SCENARIOS_JSON")

jq -n \
    --arg schema_version "wa.visual_artifact_summary.v1" \
    --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg run_dir "$RUN_DIR" \
    --arg warn_threshold "$WARN_THRESHOLD" \
    --arg fail_threshold "$FAIL_THRESHOLD" \
    --argjson scenario_count "$SCENARIO_COUNT" \
    --argjson pass_count "$PASS_COUNT" \
    --argjson warn_count "$WARN_COUNT" \
    --argjson fail_count "$FAIL_COUNT" \
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
            fail: $fail_count
        },
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
