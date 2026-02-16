#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CHECKER="$PROJECT_ROOT/scripts/check_soak_anomaly_reports.sh"

TMP_ROOT="$(mktemp -d)"
RUN_DIR="$TMP_ROOT/run"
SOAK_DIR="$RUN_DIR/soak"
REPORT="$SOAK_DIR/incident_report.json"

cleanup() {
    rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

assert_eq() {
    local actual="$1"
    local expected="$2"
    local message="$3"
    if [[ "$actual" != "$expected" ]]; then
        echo "[test_soak_anomaly_reports] assert failed: $message (actual=$actual expected=$expected)" >&2
        exit 1
    fi
}

reset_run_dir() {
    rm -rf "$RUN_DIR"
    mkdir -p "$SOAK_DIR"
}

run_checker_expect() {
    local expected_rc="$1"
    shift
    set +e
    "$CHECKER" --run-dir "$RUN_DIR" --output "$REPORT" "$@"
    local rc=$?
    set -e
    assert_eq "$rc" "$expected_rc" "checker exit code"
}

# Case 1: no data -> no_data, exit 0.
reset_run_dir
run_checker_expect 0
assert_eq "$(jq -r '.status' "$REPORT")" "no_data" "status for no data case"

# Case 2: warn-level anomalies -> warn, exit 0 by default, exit 1 with strict warn.
reset_run_dir
cat > "$SOAK_DIR/anomaly_markers.jsonl" <<'EOF'
{"marker_type":"latency_budget_pressure","detail":"duration_secs=4;threshold_secs=3","test_case_id":"case_warn"}
EOF
cat > "$SOAK_DIR/fault_matrix_events.jsonl" <<'EOF'
{"classification":"contained_failure","fault":{"active":true,"class":"pty_failure"},"test_case_id":"case_warn"}
EOF
run_checker_expect 0 --warn-latency-count 1 --fail-latency-count 5
assert_eq "$(jq -r '.status' "$REPORT")" "warn" "status for warn case"
assert_eq "$(jq -r '.metrics.latency_markers' "$REPORT")" "1" "latency marker count"
run_checker_expect 1 --warn-latency-count 1 --fail-latency-count 5 --strict-warn

# Case 3: fail-level signals -> fail, exit 2.
reset_run_dir
cat > "$SOAK_DIR/anomaly_markers.jsonl" <<'EOF'
{"marker_type":"scenario_failure","detail":"scenario failure under soak","test_case_id":"case_fail"}
EOF
cat > "$SOAK_DIR/fault_matrix_events.jsonl" <<'EOF'
{"classification":"responsiveness_budget_exceeded","fault":{"active":true,"class":"scheduler_stress"},"test_case_id":"case_fail"}
EOF
mkdir -p "$RUN_DIR/scenario_01_case_fail"
cat > "$RUN_DIR/scenario_01_case_fail/failure_signature.json" <<'EOF'
{"signature":"timeout"}
EOF
run_checker_expect 2
assert_eq "$(jq -r '.status' "$REPORT")" "fail" "status for fail case"
assert_eq "$(jq -r '.metrics.crash_signature_count' "$REPORT")" "1" "crash signature count"
assert_eq "$(jq -r '[.incidents[] | select(.incident_type == "crash_signatures")] | length' "$REPORT")" "1" "crash incident present"

echo "[test_soak_anomaly_reports] PASS"
