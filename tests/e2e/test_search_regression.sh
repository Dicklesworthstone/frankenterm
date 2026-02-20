#!/bin/bash
# E2E Test: Search Regression
# Ensures baseline regression query set remains searchable.
# Spec: ft-dr6zv.1.7

set -uo pipefail

TEST_NAME="test_search_regression"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
FT_BIN="${FT_BIN:-}"
WATCH_PID=""

ASSERT_FAILS=0
INFRA_FAILS=0

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

TEST_WORKSPACE="$(mktemp -d -t ft-e2e-regression.XXXXXX)"
ARTIFACT_DIR="${ARTIFACT_DIR:-$TEST_WORKSPACE/artifacts}"
mkdir -p "$ARTIFACT_DIR"
LOG_FILE="$ARTIFACT_DIR/${TEST_NAME}.jsonl"
SUMMARY_FILE="$ARTIFACT_DIR/${TEST_NAME}_summary.json"

export FT_WORKSPACE="$TEST_WORKSPACE"
export FT_CONFIG_PATH="$TEST_WORKSPACE/ft.toml"

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
        --argjson assert_failures "$ASSERT_FAILS" \
        --argjson infra_failures "$INFRA_FAILS" \
        --argjson exit_code "$final_code" \
        '{
            test_name: $test_name,
            workspace: $workspace,
            artifacts: $artifacts,
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
log_phase "setup" "pass" "Prerequisites satisfied" 0 "{\"ft_bin\":\"$FT_BIN\"}"

cat >"$FT_CONFIG_PATH" <<EOF
[general]
log_level = "info"
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
    mark_infra_fail "No active pane available for regression corpus population"
    exit 2
fi
log_phase "setup" "pass" "Selected pane for corpus population" 0 "{\"pane_id\":$PANE_ID}"

run_robot_json "$ARTIFACT_DIR/send_1.json" send "$PANE_ID" "echo 'compiler error: E0308'" || true
run_robot_json "$ARTIFACT_DIR/send_2.json" send "$PANE_ID" "echo 'warning: unused variable'" || true
run_robot_json "$ARTIFACT_DIR/send_3.json" send "$PANE_ID" "echo 'test result: ok. 5 passed; 0 failed'" || true
sleep 2

QUERIES=(
    "compiler error"
    "E0308"
    "warning"
    "test result"
    "failed"
)

for query in "${QUERIES[@]}"; do
    query_key="$(echo "$query" | tr ' ' '_' | tr -cd '[:alnum:]_')"
    output_path="$ARTIFACT_DIR/query_${query_key}.json"
    run_robot_json "$output_path" search "$query" --limit 1 --mode lexical || true
    hits="$(jq -r '.data.total_hits // (.data.results | length) // 0' "$output_path" 2>/dev/null || echo 0)"
    if (( hits > 0 )); then
        log_phase "assert" "pass" "Regression query returned at least one hit" 0 "{\"query\":\"$query\",\"hits\":$hits}"
    else
        mark_assert_fail "Regression query returned zero hits" 0 "{\"query\":\"$query\"}"
    fi
done
