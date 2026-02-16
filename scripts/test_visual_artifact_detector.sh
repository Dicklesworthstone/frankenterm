#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

source "$PROJECT_ROOT/scripts/lib/e2e_artifacts.sh"

TMP_ROOT="$(mktemp -d)"
RUN_DIR="$TMP_ROOT/run"
SCENARIOS_DIR="$RUN_DIR/scenarios"
mkdir -p "$SCENARIOS_DIR"

cleanup() {
    rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

make_scenario() {
    local name="$1"
    local frame_count="$2"
    local dropped="$3"
    local long_frames="$4"
    local p50="$5"
    local p95="$6"
    local p99="$7"
    local keyword_count="$8"

    local scenario_dir="$SCENARIOS_DIR/$name"
    mkdir -p "$scenario_dir"

    cat > "$scenario_dir/frame_histogram.json" <<EOF
{
  "schema_version": "wa.frame_histogram.v1",
  "histogram": {
    "frame_count": $frame_count,
    "dropped_frame_count": $dropped,
    "bucket_ms": [
      {"ms": 16, "count": $((frame_count - long_frames))},
      {"ms": 40, "count": $long_frames}
    ]
  }
}
EOF

    cat > "$scenario_dir/correlation.jsonl" <<EOF
{"test_case_id":"$name","resize_transaction_id":"run:$name:1","pane_id":1,"tab_id":1,"sequence_no":1,"scheduler_decision":"dequeue_latest_intent","frame_id":1,"queue_wait_ms":1,"reflow_ms":$p95,"render_ms":$p95,"present_ms":$p95,"p50_ms":$p50,"p95_ms":$p95,"p99_ms":$p99}
EOF

    : > "$scenario_dir/combined.log"
    for _ in $(seq 1 "$keyword_count"); do
        echo "flicker artifact jitter" >> "$scenario_dir/combined.log"
    done

    _e2e_emit_visual_artifact_report "$name" "$scenario_dir" 1000
}

assert_eq() {
    local actual="$1"
    local expected="$2"
    local msg="$3"
    if [[ "$actual" != "$expected" ]]; then
        echo "[test_visual_artifact_detector] assert failed: $msg (actual=$actual expected=$expected)" >&2
        exit 1
    fi
}

run_detector_expect() {
    local expected_rc="$1"
    shift
    set +e
    "$PROJECT_ROOT/scripts/check_visual_artifact_detector.sh" --run-dir "$RUN_DIR" "$@"
    local rc=$?
    set -e
    assert_eq "$rc" "$expected_rc" "detector exit code"
}

# Seeded cases: one pass, one warn, one fail.
make_scenario "healthy_case" 200 1 2 10 14 18 0
make_scenario "warning_case" 200 8 20 10 24 34 2
make_scenario "failing_case" 200 32 60 10 28 60 8

run_detector_expect 2

SUMMARY="$RUN_DIR/visual_artifact_summary.json"
assert_eq "$(jq -r '.status' "$SUMMARY")" "fail" "summary status after fail case"
assert_eq "$(jq -r '.counts.fail' "$SUMMARY")" "1" "fail count"
assert_eq "$(jq -r '.counts.warn' "$SUMMARY")" "1" "warn count"
assert_eq "$(jq -r '.counts.pass' "$SUMMARY")" "1" "pass count"

# Remove failing scenario report; strict warn should now fail with exit code 1.
rm -f "$SCENARIOS_DIR/failing_case/visual_artifact_report.json"
run_detector_expect 1 --strict-warn
assert_eq "$(jq -r '.status' "$SUMMARY")" "warn" "summary status after removing fail case"

# Remove warning scenario report; strict warn should pass.
rm -f "$SCENARIOS_DIR/warning_case/visual_artifact_report.json"
run_detector_expect 0 --strict-warn
assert_eq "$(jq -r '.status' "$SUMMARY")" "pass" "summary status after removing warn case"

echo "[test_visual_artifact_detector] PASS"
