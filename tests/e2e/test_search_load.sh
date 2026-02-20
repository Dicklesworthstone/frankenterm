#!/bin/bash
# E2E Test: Search Load
# Concurrent load profile (defaults: 50 qps for 30s) with per-query JSONL artifacts.
# Spec: ft-dr6zv.1.7

set -uo pipefail

TEST_NAME="test_search_load"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
FT_BIN="${FT_BIN:-}"
WATCH_PID=""

ASSERT_FAILS=0
INFRA_FAILS=0

DURATION_SECS="${DURATION_SECS:-30}"
TARGET_QPS="${TARGET_QPS:-50}"
PARALLELISM="${PARALLELISM:-8}"
MEMORY_DELTA_LIMIT_KB="${MEMORY_DELTA_LIMIT_KB:-50000}"
LOAD_QUERY="${LOAD_QUERY:-stress test}"

resolve_ft_bin() {
    if [[ -n "$FT_BIN" && -x "$FT_BIN" ]]; then
        return 0
    fi
    if [[ -x "$PROJECT_ROOT/target/release/ft" ]]; then
        FT_BIN="$PROJECT_ROOT/target/release/ft"
        return 0
    fi
    if [[ -x "$PROJECT_ROOT/target/debug/ft" ]]; then
        FT_BIN="$PROJECT_ROOT/target/debug/ft"
        return 0
    fi
    if command -v ft >/dev/null 2>&1; then
        FT_BIN="$(command -v ft)"
        return 0
    fi
    return 1
}

timestamp_ms() {
    python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
}

log_phase() {
    local phase="$1"
    local result="$2"
    local detail="$3"
    local duration_ms="${4:-0}"
    local metrics_json="${5:-}"
    local ts
    ts=$(timestamp_ms)
    if [[ -n "$metrics_json" ]]; then
        jq -nc \
            --arg test_name "$TEST_NAME" \
            --arg phase "$phase" \
            --arg result "$result" \
            --arg detail "$detail" \
            --argjson timestamp_ms "$ts" \
            --argjson duration_ms "$duration_ms" \
            --argjson metrics "$metrics_json" \
            '{test_name:$test_name,phase:$phase,timestamp_ms:$timestamp_ms,duration_ms:$duration_ms,result:$result,detail:$detail,metrics:$metrics}'
    else
        jq -nc \
            --arg test_name "$TEST_NAME" \
            --arg phase "$phase" \
            --arg result "$result" \
            --arg detail "$detail" \
            --argjson timestamp_ms "$ts" \
            --argjson duration_ms "$duration_ms" \
            '{test_name:$test_name,phase:$phase,timestamp_ms:$timestamp_ms,duration_ms:$duration_ms,result:$result,detail:$detail}'
    fi | tee -a "$LOG_FILE"
}

mark_assert_fail() {
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
    log_phase "assert" "fail" "$1" "${2:-0}" "${3:-}"
}

mark_infra_fail() {
    INFRA_FAILS=$((INFRA_FAILS + 1))
    log_phase "setup" "error" "$1" "${2:-0}" "${3:-}"
}

run_robot_json() {
    local output_path="$1"
    shift
    local started ended duration
    started=$(timestamp_ms)
    if "$FT_BIN" robot --format json "$@" >"$output_path" 2>"$output_path.stderr"; then
        ended=$(timestamp_ms)
        duration=$((ended - started))
        if jq -e '.ok == true' "$output_path" >/dev/null 2>&1; then
            log_phase "execute" "pass" "robot $* succeeded" "$duration"
            return 0
        fi
        mark_infra_fail "robot $* returned non-ok payload" "$duration"
        return 1
    fi
    ended=$(timestamp_ms)
    duration=$((ended - started))
    mark_infra_fail "robot $* failed to execute" "$duration"
    return 1
}

TEST_WORKSPACE="$(mktemp -d -t ft-e2e-load.XXXXXX)"
ARTIFACT_DIR="${ARTIFACT_DIR:-$TEST_WORKSPACE/artifacts}"
mkdir -p "$ARTIFACT_DIR"
LOG_FILE="$ARTIFACT_DIR/${TEST_NAME}.jsonl"
SUMMARY_FILE="$ARTIFACT_DIR/${TEST_NAME}_summary.json"
RESULTS_JSONL="$ARTIFACT_DIR/search_load_results.jsonl"

export FT_WORKSPACE="$TEST_WORKSPACE"
export FT_CONFIG_PATH="$TEST_WORKSPACE/ft.toml"
export FT_BIN
export RESULTS_JSONL
export LOAD_QUERY

finish() {
    trap - EXIT
    local prior_exit="$?"
    local teardown_start teardown_end teardown_duration
    teardown_start=$(timestamp_ms)

    if [[ -n "$WATCH_PID" ]] && kill -0 "$WATCH_PID" >/dev/null 2>&1; then
        "$FT_BIN" stop --force >/dev/null 2>&1 || true
        kill "$WATCH_PID" >/dev/null 2>&1 || true
        wait "$WATCH_PID" >/dev/null 2>&1 || true
    fi

    teardown_end=$(timestamp_ms)
    teardown_duration=$((teardown_end - teardown_start))
    log_phase "teardown" "pass" "Watcher stop attempted; workspace retained for artifacts" "$teardown_duration" \
        "{\"workspace\":\"$TEST_WORKSPACE\",\"artifacts\":\"$ARTIFACT_DIR\"}"

    local final_code=0
    if (( INFRA_FAILS > 0 )); then
        final_code=2
    elif (( ASSERT_FAILS > 0 )); then
        final_code=1
    elif (( prior_exit != 0 )); then
        final_code=2
    fi

    jq -nc \
        --arg test_name "$TEST_NAME" \
        --arg workspace "$TEST_WORKSPACE" \
        --arg artifacts "$ARTIFACT_DIR" \
        --arg results_jsonl "$RESULTS_JSONL" \
        --argjson assert_failures "$ASSERT_FAILS" \
        --argjson infra_failures "$INFRA_FAILS" \
        --argjson exit_code "$final_code" \
        '{
            test_name: $test_name,
            workspace: $workspace,
            artifacts: $artifacts,
            results_jsonl: $results_jsonl,
            assert_failures: $assert_failures,
            infra_failures: $infra_failures,
            exit_code: $exit_code
        }' >"$SUMMARY_FILE"

    exit "$final_code"
}
trap finish EXIT

if ! resolve_ft_bin; then
    mark_infra_fail "Unable to locate ft binary (set FT_BIN or build target/{debug,release}/ft)."
    exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
    mark_infra_fail "Missing required command: jq"
    exit 2
fi
if ! command -v python3 >/dev/null 2>&1; then
    mark_infra_fail "Missing required command: python3"
    exit 2
fi
if ! command -v xargs >/dev/null 2>&1; then
    mark_infra_fail "Missing required command: xargs"
    exit 2
fi

log_phase "setup" "pass" "Prerequisites satisfied" 0 "{\"ft_bin\":\"$FT_BIN\",\"duration_secs\":$DURATION_SECS,\"target_qps\":$TARGET_QPS,\"parallelism\":$PARALLELISM}"

cat >"$FT_CONFIG_PATH" <<EOF
[general]
log_level = "warn"
log_format = "json"

[search]
enabled = true
mode = "hybrid"
fast_only = false
EOF

"$FT_BIN" watch --foreground >"$ARTIFACT_DIR/watch.log" 2>&1 &
WATCH_PID=$!
log_phase "setup" "pass" "Started watcher in foreground background-job mode" 0 "{\"watch_pid\":$WATCH_PID}"

ready=0
for _ in $(seq 1 60); do
    if "$FT_BIN" robot --format json state >"$ARTIFACT_DIR/robot_state_probe.json" 2>"$ARTIFACT_DIR/robot_state_probe.stderr"; then
        if jq -e '.ok == true' "$ARTIFACT_DIR/robot_state_probe.json" >/dev/null 2>&1; then
            ready=1
            break
        fi
    fi
    sleep 0.5
done
if (( ready == 0 )); then
    mark_infra_fail "Watcher never became ready for robot state probes"
    exit 2
fi

PANE_ID="$(jq -r '.data.panes[0].pane_id // empty' "$ARTIFACT_DIR/robot_state_probe.json")"
if [[ -z "$PANE_ID" ]]; then
    mark_infra_fail "No active pane available for load-test seed data"
    exit 2
fi

run_robot_json "$ARTIFACT_DIR/seed_send.json" send "$PANE_ID" "echo 'stress test data for search load'" || true
sleep 2

if ! kill -0 "$WATCH_PID" >/dev/null 2>&1; then
    mark_infra_fail "Watcher process is not alive before load run"
    exit 2
fi

RSS_START="$(ps -o rss= -p "$WATCH_PID" | awk '{print $1}')"
if [[ -z "$RSS_START" ]]; then
    mark_infra_fail "Failed to read watcher RSS before load run"
    exit 2
fi
log_phase "setup" "pass" "Captured starting RSS" 0 "{\"rss_kb\":$RSS_START}"

TOTAL_REQS=$((DURATION_SECS * TARGET_QPS))
: >"$RESULTS_JSONL"
load_start="$(timestamp_ms)"

seq 1 "$TOTAL_REQS" | xargs -P "$PARALLELISM" -I {} bash -c '
req_id="$1"
start_ms=$(python3 - <<'"'"'PY'"'"'
import time
print(int(time.time() * 1000))
PY
)
if out=$("$FT_BIN" robot --format json search "$LOAD_QUERY" --limit 1 2>/dev/null); then
    ok=$(printf "%s" "$out" | jq -r ".ok // false" 2>/dev/null || echo "false")
    hits=$(printf "%s" "$out" | jq -r ".data.total_hits // 0" 2>/dev/null || echo "0")
    status="pass"
    if [[ "$ok" != "true" ]]; then
        status="fail"
    fi
else
    ok="false"
    hits=0
    status="error"
fi
end_ms=$(python3 - <<'"'"'PY'"'"'
import time
print(int(time.time() * 1000))
PY
)
latency_ms=$((end_ms - start_ms))
jq -nc \
    --argjson request_id "$req_id" \
    --arg status "$status" \
    --argjson timestamp_ms "$end_ms" \
    --argjson latency_ms "$latency_ms" \
    --argjson hits "$hits" \
    --arg query "$LOAD_QUERY" \
    '{
        request_id: $request_id,
        timestamp_ms: $timestamp_ms,
        latency_ms: $latency_ms,
        status: $status,
        hits: $hits,
        query: $query
    }' >> "$RESULTS_JSONL"
exit 0
' _ {}

load_end="$(timestamp_ms)"
load_duration_ms=$((load_end - load_start))

RSS_END="$(ps -o rss= -p "$WATCH_PID" | awk '{print $1}')"
if [[ -z "$RSS_END" ]]; then
    mark_infra_fail "Failed to read watcher RSS after load run"
    exit 2
fi
RSS_DELTA=$((RSS_END - RSS_START))

stats_json="$(python3 - "$RESULTS_JSONL" <<'PY'
import json
import math
import sys

path = sys.argv[1]
latencies = []
failures = 0
errors = 0
total = 0

with open(path, "r", encoding="utf-8") as f:
    for raw in f:
        raw = raw.strip()
        if not raw:
            continue
        row = json.loads(raw)
        total += 1
        latencies.append(int(row.get("latency_ms", 0)))
        status = row.get("status", "")
        if status != "pass":
            failures += 1
        if status == "error":
            errors += 1

latencies.sort()

def percentile(values, pct):
    if not values:
        return 0
    rank = max(0, min(len(values) - 1, math.ceil((pct / 100.0) * len(values)) - 1))
    return values[rank]

result = {
    "total_requests": total,
    "failure_count": failures,
    "error_count": errors,
    "p50_ms": percentile(latencies, 50),
    "p95_ms": percentile(latencies, 95),
    "p99_ms": percentile(latencies, 99),
}
print(json.dumps(result))
PY
)"

echo "$stats_json" >"$ARTIFACT_DIR/search_load_metrics.json"

failure_count="$(echo "$stats_json" | jq -r '.failure_count')"
p99_ms="$(echo "$stats_json" | jq -r '.p99_ms')"

log_phase "assert" "pass" "Load run completed" "$load_duration_ms" \
    "{\"total_requests\":$TOTAL_REQS,\"failure_count\":$failure_count,\"p99_ms\":$p99_ms,\"rss_delta_kb\":$RSS_DELTA}"

if (( failure_count > 0 )); then
    mark_assert_fail "Load run had failing/error query responses" 0 "{\"failure_count\":$failure_count}"
fi
if (( p99_ms > 500 )); then
    mark_assert_fail "p99 latency exceeded 500ms budget" 0 "{\"p99_ms\":$p99_ms}"
else
    log_phase "assert" "pass" "p99 latency within 500ms budget" 0 "{\"p99_ms\":$p99_ms}"
fi
if (( RSS_DELTA > MEMORY_DELTA_LIMIT_KB )); then
    mark_assert_fail "RSS delta exceeded configured memory budget" 0 "{\"rss_delta_kb\":$RSS_DELTA,\"limit_kb\":$MEMORY_DELTA_LIMIT_KB}"
else
    log_phase "assert" "pass" "RSS delta within configured memory budget" 0 "{\"rss_delta_kb\":$RSS_DELTA,\"limit_kb\":$MEMORY_DELTA_LIMIT_KB}"
fi
