#!/bin/bash
# E2E Test: FrankenSearch Integration
# Tests indexing -> query -> metrics/explain behavior.
# Spec: ft-dr6zv.1.7

set -uo pipefail

TEST_NAME="test_frankensearch_integration"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
FT_BIN="${FT_BIN:-}"
WATCH_PID=""

ASSERT_FAILS=0
INFRA_FAILS=0
SKIP_COUNT=0

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

require_cmd() {
    local cmd="$1"
    command -v "$cmd" >/dev/null 2>&1
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
            '{
                test_name: $test_name,
                phase: $phase,
                timestamp_ms: $timestamp_ms,
                duration_ms: $duration_ms,
                result: $result,
                detail: $detail,
                metrics: $metrics
            }'
    else
        jq -nc \
            --arg test_name "$TEST_NAME" \
            --arg phase "$phase" \
            --arg result "$result" \
            --arg detail "$detail" \
            --argjson timestamp_ms "$ts" \
            --argjson duration_ms "$duration_ms" \
            '{
                test_name: $test_name,
                phase: $phase,
                timestamp_ms: $timestamp_ms,
                duration_ms: $duration_ms,
                result: $result,
                detail: $detail
            }'
    fi | tee -a "$LOG_FILE"
}

mark_assert_fail() {
    local detail="$1"
    local duration_ms="${2:-0}"
    local metrics_json="${3:-}"
    ASSERT_FAILS=$((ASSERT_FAILS + 1))
    log_phase "assert" "fail" "$detail" "$duration_ms" "$metrics_json"
}

mark_infra_fail() {
    local detail="$1"
    local duration_ms="${2:-0}"
    local metrics_json="${3:-}"
    INFRA_FAILS=$((INFRA_FAILS + 1))
    log_phase "setup" "error" "$detail" "$duration_ms" "$metrics_json"
}

mark_skip() {
    local detail="$1"
    local duration_ms="${2:-0}"
    SKIP_COUNT=$((SKIP_COUNT + 1))
    log_phase "assert" "skip" "$detail" "$duration_ms"
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

TEST_WORKSPACE="$(mktemp -d -t ft-e2e-search.XXXXXX)"
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
        --argjson skips "$SKIP_COUNT" \
        --argjson exit_code "$final_code" \
        '{
            test_name: $test_name,
            workspace: $workspace,
            artifacts: $artifacts,
            assert_failures: $assert_failures,
            infra_failures: $infra_failures,
            skips: $skips,
            exit_code: $exit_code
        }' >"$SUMMARY_FILE"

    exit "$final_code"
}
trap finish EXIT

if ! resolve_ft_bin; then
    mark_infra_fail "Unable to locate ft binary (set FT_BIN or build target/{debug,release}/ft)."
    exit 2
fi
log_phase "setup" "pass" "Resolved ft binary" 0 "{\"ft_bin\":\"$FT_BIN\"}"

if ! require_cmd jq; then
    mark_infra_fail "Missing required command: jq"
    exit 2
fi
if ! require_cmd python3; then
    mark_infra_fail "Missing required command: python3"
    exit 2
fi

cat >"$FT_CONFIG_PATH" <<EOF
[general]
log_level = "info"
log_format = "json"

[search]
enabled = true
mode = "hybrid"
fast_only = false

[search.indexing]
index_dir = "$TEST_WORKSPACE/search_index"
EOF
log_phase "setup" "pass" "Wrote test config" 0 "{\"config_path\":\"$FT_CONFIG_PATH\"}"

local_watch_start=$(timestamp_ms)
"$FT_BIN" watch --foreground >"$ARTIFACT_DIR/watch.log" 2>&1 &
WATCH_PID=$!
local_watch_end=$(timestamp_ms)
log_phase "setup" "pass" "Started watcher in foreground background-job mode" \
    "$((local_watch_end - local_watch_start))" "{\"watch_pid\":$WATCH_PID}"

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
    mark_infra_fail "Watcher never became ready for robot state probes" 0
    exit 2
fi
log_phase "setup" "pass" "Watcher readiness probe passed" 0

PANE_ID="$(jq -r '.data.panes[0].pane_id // empty' "$ARTIFACT_DIR/robot_state_probe.json")"
if [[ -z "$PANE_ID" ]]; then
    mark_infra_fail "No active pane available for E2E send/search flow" 0
    exit 2
fi
log_phase "setup" "pass" "Selected pane for test data generation" 0 "{\"pane_id\":$PANE_ID}"

run_robot_json "$ARTIFACT_DIR/send_1.json" send "$PANE_ID" "echo 'The quick brown fox jumps over the lazy dog'" || true
run_robot_json "$ARTIFACT_DIR/send_2.json" send "$PANE_ID" "echo 'unique_string_alpha_beta_gamma'" || true
run_robot_json "$ARTIFACT_DIR/send_3.json" send "$PANE_ID" "echo 'Another distinct line for searching'" || true
sleep 2

doc_count=0
for _ in $(seq 1 20); do
    run_robot_json "$ARTIFACT_DIR/search_index_stats.json" search-index stats || true
    doc_count="$(jq -r '.data.document_count // 0' "$ARTIFACT_DIR/search_index_stats.json" 2>/dev/null || echo 0)"
    if (( doc_count > 0 )); then
        break
    fi
    sleep 1
done

if (( doc_count < 1 )); then
    mark_assert_fail "Search index remained empty after data generation window" 0 "{\"document_count\":$doc_count}"
else
    log_phase "assert" "pass" "Search index contains documents" 0 "{\"document_count\":$doc_count}"
fi

run_robot_json "$ARTIFACT_DIR/lexical_unique.json" search "unique_string_alpha_beta_gamma" --limit 5 --mode lexical || true
lexical_hits="$(jq -r '.data.results | length' "$ARTIFACT_DIR/lexical_unique.json" 2>/dev/null || echo 0)"
if (( lexical_hits < 1 )); then
    mark_assert_fail "Lexical query returned no hits for unique token" 0
else
    first_text="$(jq -r '.data.results[0].content // .data.results[0].snippet // ""' "$ARTIFACT_DIR/lexical_unique.json" 2>/dev/null || echo "")"
    if [[ "$first_text" != *"unique_string_alpha_beta_gamma"* ]]; then
        mark_assert_fail "Top lexical hit did not include expected token" 0
    else
        log_phase "assert" "pass" "Lexical query returned expected token in top hit" 0 "{\"hits\":$lexical_hits}"
    fi
fi

run_robot_json "$ARTIFACT_DIR/lexical_quick_fox.json" search "quick fox" --limit 5 --mode lexical || true
quick_hits="$(jq -r '.data.results | length' "$ARTIFACT_DIR/lexical_quick_fox.json" 2>/dev/null || echo 0)"
if (( quick_hits < 1 )); then
    mark_assert_fail "Lexical query 'quick fox' returned no hits" 0
else
    log_phase "assert" "pass" "Lexical query 'quick fox' returned hits" 0 "{\"hits\":$quick_hits}"
fi

run_robot_json "$ARTIFACT_DIR/hybrid_metrics.json" search "brown" --limit 5 --mode hybrid || true
if jq -e '.data.metrics.effective_mode? != null' "$ARTIFACT_DIR/hybrid_metrics.json" >/dev/null 2>&1; then
    log_phase "assert" "pass" "Hybrid search returned metrics envelope" 0
else
    mark_assert_fail "Hybrid search metrics missing from response payload" 0
fi

mark_skip "JSONL phase markers are not currently available in robot output formats (json|toon only). Captured hybrid metrics as interim signal."

run_robot_json "$ARTIFACT_DIR/search_explain.json" search-explain "lazy dog" || true
if jq -e '.data.reasons | type == "array"' "$ARTIFACT_DIR/search_explain.json" >/dev/null 2>&1; then
    reason_count="$(jq -r '.data.reasons | length' "$ARTIFACT_DIR/search_explain.json" 2>/dev/null || echo 0)"
    log_phase "assert" "pass" "search-explain returned structured reasons array" 0 "{\"reasons\":$reason_count}"
else
    mark_assert_fail "search-explain response missing reasons array" 0
fi
