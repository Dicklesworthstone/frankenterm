#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/check_soak_anomaly_reports.sh --run-dir <path> [options]

Options:
  --run-dir <path>              E2E run directory (expects soak/* telemetry)
  --output <path>               Incident report output path (default: <run-dir>/soak/incident_report.json)
  --warn-latency-count <n>      Warn threshold for latency_budget_pressure markers (default: SOAK_INCIDENT_WARN_LATENCY_COUNT or 2)
  --fail-latency-count <n>      Fail threshold for latency_budget_pressure markers (default: SOAK_INCIDENT_FAIL_LATENCY_COUNT or 5)
  --warn-anomaly-count <n>      Warn threshold for scenario_failure markers (default: SOAK_INCIDENT_WARN_ANOMALY_COUNT or 1)
  --fail-anomaly-count <n>      Fail threshold for scenario_failure markers (default: SOAK_INCIDENT_FAIL_ANOMALY_COUNT or 3)
  --strict-warn                 Exit 1 when report status is warn (fail remains exit 2)
  --help                        Show this message
EOF
}

RUN_DIR=""
OUTPUT=""
WARN_LATENCY_COUNT="${SOAK_INCIDENT_WARN_LATENCY_COUNT:-2}"
FAIL_LATENCY_COUNT="${SOAK_INCIDENT_FAIL_LATENCY_COUNT:-5}"
WARN_ANOMALY_COUNT="${SOAK_INCIDENT_WARN_ANOMALY_COUNT:-1}"
FAIL_ANOMALY_COUNT="${SOAK_INCIDENT_FAIL_ANOMALY_COUNT:-3}"
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
        --warn-latency-count)
            WARN_LATENCY_COUNT="${2:-}"
            shift 2
            ;;
        --fail-latency-count)
            FAIL_LATENCY_COUNT="${2:-}"
            shift 2
            ;;
        --warn-anomaly-count)
            WARN_ANOMALY_COUNT="${2:-}"
            shift 2
            ;;
        --fail-anomaly-count)
            FAIL_ANOMALY_COUNT="${2:-}"
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
            echo "[soak_incident] unknown argument: $1" >&2
            usage >&2
            exit 64
            ;;
    esac
done

require_int_ge_zero() {
    local value="$1"
    local name="$2"
    if ! [[ "$value" =~ ^[0-9]+$ ]]; then
        echo "[soak_incident] invalid $name: $value (expected integer >= 0)" >&2
        exit 64
    fi
}

if [[ -z "$RUN_DIR" ]]; then
    echo "[soak_incident] --run-dir is required" >&2
    usage >&2
    exit 64
fi

if [[ ! -d "$RUN_DIR" ]]; then
    echo "[soak_incident] run directory does not exist: $RUN_DIR" >&2
    exit 66
fi

require_int_ge_zero "$WARN_LATENCY_COUNT" "warn-latency-count"
require_int_ge_zero "$FAIL_LATENCY_COUNT" "fail-latency-count"
require_int_ge_zero "$WARN_ANOMALY_COUNT" "warn-anomaly-count"
require_int_ge_zero "$FAIL_ANOMALY_COUNT" "fail-anomaly-count"

if [[ -z "$OUTPUT" ]]; then
    OUTPUT="$RUN_DIR/soak/incident_report.json"
fi
mkdir -p "$(dirname "$OUTPUT")"

SOAK_DIR="$RUN_DIR/soak"
ANOMALY_FILE="$SOAK_DIR/anomaly_markers.jsonl"
CHECKPOINT_FILE="$SOAK_DIR/checkpoint_telemetry.jsonl"
FAULT_EVENTS_FILE="$SOAK_DIR/fault_matrix_events.jsonl"
FAULT_SUMMARY_FILE="$SOAK_DIR/fault_matrix_summary.json"

count_jsonl_select() {
    local file="$1"
    local select_expr="$2"
    if [[ ! -s "$file" ]]; then
        echo 0
        return 0
    fi
    jq -s "$select_expr | length" "$file" 2>/dev/null || echo 0
}

add_unique_string() {
    local json_array="$1"
    local value="$2"
    jq -c --arg value "$value" '
        if index($value) then . else . + [$value] end
    ' <<< "$json_array"
}

add_artifact_if_exists() {
    local artifact_json="$1"
    local abs_path="$2"
    if [[ -f "$abs_path" ]]; then
        local rel_path="${abs_path#$RUN_DIR/}"
        add_unique_string "$artifact_json" "$rel_path"
    else
        echo "$artifact_json"
    fi
}

LATENCY_MARKERS=$(count_jsonl_select "$ANOMALY_FILE" 'map(select(.marker_type == "latency_budget_pressure"))')
SCENARIO_FAILURE_MARKERS=$(count_jsonl_select "$ANOMALY_FILE" 'map(select(.marker_type == "scenario_failure"))')
STARVATION_MARKERS=$(count_jsonl_select "$ANOMALY_FILE" 'map(select((.marker_type // "" | test("starvation|queue"; "i")) or (.detail // "" | test("starvation|queue_stall|queue_wait"; "i"))))')

FAULT_EVENT_COUNT=$(count_jsonl_select "$FAULT_EVENTS_FILE" 'map(.)')
RESPONSIVENESS_BREACHES=$(count_jsonl_select "$FAULT_EVENTS_FILE" 'map(select(.classification == "responsiveness_budget_exceeded"))')
CONTAINED_FAILURES=$(count_jsonl_select "$FAULT_EVENTS_FILE" 'map(select(.classification == "contained_failure"))')
UNEXPECTED_FAILURES=$(count_jsonl_select "$FAULT_EVENTS_FILE" 'map(select(.classification == "unexpected_failure_without_injection"))')

CRASH_SIGNATURES_JSON="[]"
while IFS= read -r signature_file; do
    signature=$(jq -r '.signature // .failure_signature // "unknown"' "$signature_file" 2>/dev/null || echo "unknown")
    rel_path="${signature_file#$RUN_DIR/}"
    CRASH_SIGNATURES_JSON=$(jq -c \
        --arg signature "$signature" \
        --arg path "$rel_path" \
        '. + [{signature: $signature, path: $path}]' <<< "$CRASH_SIGNATURES_JSON")
done < <(find "$RUN_DIR" -maxdepth 2 -type f -name 'failure_signature.json' | LC_ALL=C sort)
CRASH_SIGNATURE_COUNT=$(jq -r 'length' <<< "$CRASH_SIGNATURES_JSON")

VISUAL_SUMMARY_FILE=""
if [[ -f "$RUN_DIR/visual_artifact_summary.json" ]]; then
    VISUAL_SUMMARY_FILE="$RUN_DIR/visual_artifact_summary.json"
elif [[ -f "$SOAK_DIR/visual_artifact_summary.json" ]]; then
    VISUAL_SUMMARY_FILE="$SOAK_DIR/visual_artifact_summary.json"
fi

VISUAL_STATUS="not_available"
VISUAL_WARN_COUNT=0
VISUAL_FAIL_COUNT=0
if [[ -n "$VISUAL_SUMMARY_FILE" ]]; then
    VISUAL_STATUS=$(jq -r '.status // "unknown"' "$VISUAL_SUMMARY_FILE" 2>/dev/null || echo "unknown")
    VISUAL_WARN_COUNT=$(jq -r '.counts.warn // 0' "$VISUAL_SUMMARY_FILE" 2>/dev/null || echo 0)
    VISUAL_FAIL_COUNT=$(jq -r '.counts.fail // 0' "$VISUAL_SUMMARY_FILE" 2>/dev/null || echo 0)
fi

HAS_DATA=false
if [[ -s "$ANOMALY_FILE" || -s "$FAULT_EVENTS_FILE" || "$CRASH_SIGNATURE_COUNT" -gt 0 || -n "$VISUAL_SUMMARY_FILE" ]]; then
    HAS_DATA=true
fi

ROOT_CAUSES_JSON="[]"
INCIDENTS_JSON="[]"
ARTIFACTS_JSON="[]"

ARTIFACTS_JSON=$(add_artifact_if_exists "$ARTIFACTS_JSON" "$SOAK_DIR/config.json")
ARTIFACTS_JSON=$(add_artifact_if_exists "$ARTIFACTS_JSON" "$SOAK_DIR/last_checkpoint.json")
ARTIFACTS_JSON=$(add_artifact_if_exists "$ARTIFACTS_JSON" "$CHECKPOINT_FILE")
ARTIFACTS_JSON=$(add_artifact_if_exists "$ARTIFACTS_JSON" "$ANOMALY_FILE")
ARTIFACTS_JSON=$(add_artifact_if_exists "$ARTIFACTS_JSON" "$FAULT_EVENTS_FILE")
ARTIFACTS_JSON=$(add_artifact_if_exists "$ARTIFACTS_JSON" "$FAULT_SUMMARY_FILE")
if [[ -n "$VISUAL_SUMMARY_FILE" ]]; then
    ARTIFACTS_JSON=$(add_artifact_if_exists "$ARTIFACTS_JSON" "$VISUAL_SUMMARY_FILE")
fi

while IFS= read -r crash_path; do
    ARTIFACTS_JSON=$(add_unique_string "$ARTIFACTS_JSON" "$crash_path")
done < <(jq -r '.[].path' <<< "$CRASH_SIGNATURES_JSON")

add_incident() {
    local incident_type="$1"
    local severity="$2"
    local count="$3"
    local description="$4"
    local root_hint="$5"
    local evidence_json="$6"
    INCIDENTS_JSON=$(jq -c \
        --arg incident_type "$incident_type" \
        --arg severity "$severity" \
        --argjson count "$count" \
        --arg description "$description" \
        --arg root_hint "$root_hint" \
        --argjson evidence "$evidence_json" \
        '. + [{
            incident_type: $incident_type,
            severity: $severity,
            count: $count,
            description: $description,
            probable_root_cause: $root_hint,
            evidence: $evidence
        }]' <<< "$INCIDENTS_JSON")
    ROOT_CAUSES_JSON=$(add_unique_string "$ROOT_CAUSES_JSON" "$root_hint")
}

if [[ "$LATENCY_MARKERS" -ge "$WARN_LATENCY_COUNT" ]]; then
    latency_severity="warn"
    if [[ "$LATENCY_MARKERS" -ge "$FAIL_LATENCY_COUNT" ]]; then
        latency_severity="fail"
    fi
    add_incident \
        "latency_regression" \
        "$latency_severity" \
        "$LATENCY_MARKERS" \
        "latency_budget_pressure markers exceeded threshold" \
        "Resize/reflow queue pressure is growing under soak load." \
        '["soak/anomaly_markers.jsonl","soak/checkpoint_telemetry.jsonl"]'
fi

if [[ "$SCENARIO_FAILURE_MARKERS" -ge "$WARN_ANOMALY_COUNT" ]]; then
    failure_marker_severity="warn"
    if [[ "$SCENARIO_FAILURE_MARKERS" -ge "$FAIL_ANOMALY_COUNT" ]]; then
        failure_marker_severity="fail"
    fi
    add_incident \
        "scenario_failure_spike" \
        "$failure_marker_severity" \
        "$SCENARIO_FAILURE_MARKERS" \
        "scenario_failure anomaly markers exceeded threshold" \
        "One or more scenario lanes are failing repeatedly under soak pressure." \
        '["soak/anomaly_markers.jsonl"]'
fi

if [[ "$STARVATION_MARKERS" -gt 0 || "$RESPONSIVENESS_BREACHES" -gt 0 ]]; then
    starvation_count=$((STARVATION_MARKERS + RESPONSIVENESS_BREACHES))
    starvation_severity="warn"
    if [[ "$RESPONSIVENESS_BREACHES" -gt 0 ]]; then
        starvation_severity="fail"
    fi
    add_incident \
        "starvation_or_responsiveness_breach" \
        "$starvation_severity" \
        "$starvation_count" \
        "starvation-like markers or responsiveness budget breaches detected" \
        "Scheduler starvation / queue backlog is likely violating degradation policy." \
        '["soak/anomaly_markers.jsonl","soak/fault_matrix_events.jsonl"]'
fi

if [[ "$CONTAINED_FAILURES" -gt 0 ]]; then
    add_incident \
        "contained_failures" \
        "warn" \
        "$CONTAINED_FAILURES" \
        "fault-matrix contained failures recorded" \
        "Fault injection is triggering expected contained failure paths; verify recovery quality." \
        '["soak/fault_matrix_events.jsonl","soak/fault_matrix_summary.json"]'
fi

if [[ "$UNEXPECTED_FAILURES" -gt 0 ]]; then
    add_incident \
        "unexpected_failures_without_injection" \
        "fail" \
        "$UNEXPECTED_FAILURES" \
        "failures occurred outside active fault-injection windows" \
        "Unexpected non-injected failures indicate instability unrelated to planned chaos lanes." \
        '["soak/fault_matrix_events.jsonl"]'
fi

if [[ "$VISUAL_WARN_COUNT" -gt 0 || "$VISUAL_FAIL_COUNT" -gt 0 ]]; then
    visual_count=$((VISUAL_WARN_COUNT + VISUAL_FAIL_COUNT))
    visual_severity="warn"
    if [[ "$VISUAL_FAIL_COUNT" -gt 0 ]]; then
        visual_severity="fail"
    fi
    evidence_path="visual_artifact_summary.json"
    if [[ "$VISUAL_SUMMARY_FILE" == "$SOAK_DIR/"* ]]; then
        evidence_path="soak/visual_artifact_summary.json"
    fi
    add_incident \
        "visual_artifact_spike" \
        "$visual_severity" \
        "$visual_count" \
        "visual artifact detector reports warn/fail scenarios" \
        "Render pipeline commit timing and frame pacing are likely unstable." \
        "[\"$evidence_path\"]"
fi

if [[ "$CRASH_SIGNATURE_COUNT" -gt 0 ]]; then
    CRASH_EVIDENCE_JSON=$(jq -c 'map(.path)' <<< "$CRASH_SIGNATURES_JSON")
    add_incident \
        "crash_signatures" \
        "fail" \
        "$CRASH_SIGNATURE_COUNT" \
        "failure signatures found in scenario artifacts" \
        "Crash/failure signatures indicate unstable recovery under soak load." \
        "$CRASH_EVIDENCE_JSON"
fi

FAIL_INCIDENTS=$(jq -r '[.[] | select(.severity == "fail")] | length' <<< "$INCIDENTS_JSON")
WARN_INCIDENTS=$(jq -r '[.[] | select(.severity == "warn")] | length' <<< "$INCIDENTS_JSON")

STATUS="pass"
if [[ "$HAS_DATA" != "true" ]]; then
    STATUS="no_data"
elif [[ "$FAIL_INCIDENTS" -gt 0 ]]; then
    STATUS="fail"
elif [[ "$WARN_INCIDENTS" -gt 0 ]]; then
    STATUS="warn"
fi

jq -n \
    --arg schema_version "wa.soak_incident_report.v1" \
    --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg run_dir "$RUN_DIR" \
    --arg status "$STATUS" \
    --argjson strict_warn "$STRICT_WARN" \
    --argjson warn_latency_count "$WARN_LATENCY_COUNT" \
    --argjson fail_latency_count "$FAIL_LATENCY_COUNT" \
    --argjson warn_anomaly_count "$WARN_ANOMALY_COUNT" \
    --argjson fail_anomaly_count "$FAIL_ANOMALY_COUNT" \
    --argjson latency_markers "$LATENCY_MARKERS" \
    --argjson scenario_failure_markers "$SCENARIO_FAILURE_MARKERS" \
    --argjson starvation_markers "$STARVATION_MARKERS" \
    --argjson fault_event_count "$FAULT_EVENT_COUNT" \
    --argjson responsiveness_breaches "$RESPONSIVENESS_BREACHES" \
    --argjson contained_failures "$CONTAINED_FAILURES" \
    --argjson unexpected_failures "$UNEXPECTED_FAILURES" \
    --argjson crash_signature_count "$CRASH_SIGNATURE_COUNT" \
    --argjson visual_warn_count "$VISUAL_WARN_COUNT" \
    --argjson visual_fail_count "$VISUAL_FAIL_COUNT" \
    --arg visual_status "$VISUAL_STATUS" \
    --argjson incidents "$INCIDENTS_JSON" \
    --argjson probable_root_causes "$ROOT_CAUSES_JSON" \
    --argjson crash_signatures "$CRASH_SIGNATURES_JSON" \
    --argjson artifacts "$ARTIFACTS_JSON" \
    '{
        schema_version: $schema_version,
        generated_at: $generated_at,
        run_dir: $run_dir,
        status: $status,
        thresholds: {
            latency_markers: {
                warn: $warn_latency_count,
                fail: $fail_latency_count
            },
            scenario_failure_markers: {
                warn: $warn_anomaly_count,
                fail: $fail_anomaly_count
            }
        },
        metrics: {
            latency_markers: $latency_markers,
            scenario_failure_markers: $scenario_failure_markers,
            starvation_markers: $starvation_markers,
            fault_event_count: $fault_event_count,
            responsiveness_breaches: $responsiveness_breaches,
            contained_failures: $contained_failures,
            unexpected_failures: $unexpected_failures,
            crash_signature_count: $crash_signature_count,
            visual_artifacts: {
                status: $visual_status,
                warn: $visual_warn_count,
                fail: $visual_fail_count
            }
        },
        incidents: $incidents,
        probable_root_causes: $probable_root_causes,
        evidence: {
            artifacts: $artifacts,
            crash_signatures: $crash_signatures
        }
    }' > "$OUTPUT"

echo "[soak_incident] report written: $OUTPUT" >&2
echo "[soak_incident] status=$STATUS incidents=$(jq -r 'length' <<< "$INCIDENTS_JSON") fail_incidents=$FAIL_INCIDENTS warn_incidents=$WARN_INCIDENTS" >&2

if [[ "$STATUS" == "fail" ]]; then
    exit 2
fi

if [[ "$STRICT_WARN" == "true" && "$STATUS" == "warn" ]]; then
    exit 1
fi

exit 0
