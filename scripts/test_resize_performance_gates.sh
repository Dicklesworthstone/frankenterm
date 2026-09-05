#!/usr/bin/env bash
# =============================================================================
# Unit tests for scripts/check_resize_performance_gates.sh
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

CHECKER="$SCRIPT_DIR/check_resize_performance_gates.sh"
PASS=0
FAIL=0
TEST_ROOT="$(mktemp -d)"

new_tmpdir() {
    mktemp -d "$TEST_ROOT/case.XXXXXX"
}

cleanup() {
    printf 'Retained resize gate fixtures: %s\n' "$TEST_ROOT"
}
trap cleanup EXIT

assert_pass() {
    local name="$1"
    shift
    if "$@"; then
        echo "  PASS: $name"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $name"
        FAIL=$((FAIL + 1))
    fi
}

assert_fail() {
    local name="$1"
    shift
    if "$@"; then
        echo "  FAIL: $name (expected failure)"
        FAIL=$((FAIL + 1))
    else
        echo "  PASS: $name"
        PASS=$((PASS + 1))
    fi
}

assert_json_eq() {
    local name="$1"
    local file="$2"
    local query="$3"
    local expected="$4"
    local actual
    actual="$(jq -r "$query" "$file" || echo "__ERR__")"
    if [[ "$actual" == "$expected" ]]; then
        echo "  PASS: $name"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $name (expected '$expected', got '$actual')"
        FAIL=$((FAIL + 1))
    fi
}

write_envelope() {
    local file="$1"
    local scenario="$2"
    local p50="$3"
    local p95="$4"
    local p99="$5"
    local logical_reflow_p95="$6"
    local expectations_failed="$7"

    cat >"$file" <<EOF
{
  "mode": "resize_timeline_json",
  "completed": true,
  "expectations_failed": $expectations_failed,
  "timeline": {
    "executed_resize_events": 8,
    "events": [
      {
        "event_index": 0,
        "pane_id": 0,
        "action": "resize",
        "scheduled_at_ns": 10,
        "dispatch_offset_ns": 1,
        "total_duration_ns": $p95,
        "stages": [
          {"stage":"input_intent","start_offset_ns":0,"duration_ns":1000},
          {"stage":"scheduler_queueing","start_offset_ns":1000,"duration_ns":2000,"queue_metrics":{"depth_before":3,"depth_after":2}},
          {"stage":"logical_reflow","start_offset_ns":3000,"duration_ns":4000},
          {"stage":"render_prep","start_offset_ns":7000,"duration_ns":5000},
          {"stage":"presentation","start_offset_ns":12000,"duration_ns":6000}
        ]
      }
    ]
  },
  "stage_summary": [
    {"stage":"input_intent","samples":8,"p95_duration_ns":500000},
    {"stage":"scheduler_queueing","samples":8,"p95_duration_ns":2000000},
    {"stage":"logical_reflow","samples":8,"p95_duration_ns":$logical_reflow_p95},
    {"stage":"render_prep","samples":8,"p95_duration_ns":3000000},
    {"stage":"presentation","samples":8,"p95_duration_ns":3000000}
  ],
  "aggregate_event_duration_ns": {
    "p50": $p50,
    "p95": $p95,
    "p99": $p99
  },
  "scenario": {
    "name": "$scenario",
    "reproducibility_key": "resize_baseline:test:$scenario:1"
  }
}
EOF
}

write_suite() {
    local dir="$1"
    local p50="$2"
    local p95="$3"
    local p99="$4"
    local logical_reflow_p95="$5"
    local expectations_failed="$6"
    mkdir -p "$dir"
    write_envelope "$dir/resize_single_pane_scrollback.json" "resize_single_pane_scrollback" "$p50" "$p95" "$p99" "$logical_reflow_p95" "$expectations_failed"
    write_envelope "$dir/resize_multi_tab_storm.json" "resize_multi_tab_storm" "$p50" "$p95" "$p99" "$logical_reflow_p95" "$expectations_failed"
    write_envelope "$dir/font_churn_multi_pane.json" "font_churn_multi_pane" "$p50" "$p95" "$p99" "$logical_reflow_p95" "$expectations_failed"
    write_envelope "$dir/mixed_scale_soak.json" "mixed_scale_soak" "$p50" "$p95" "$p99" "$logical_reflow_p95" "$expectations_failed"
    write_envelope "$dir/mixed_workload_interactive_streaming.json" "mixed_workload_interactive_streaming" "$p50" "$p95" "$p99" "$logical_reflow_p95" "$expectations_failed"
}

echo "Test 1: green run passes"
DIR1="$(new_tmpdir)"
INPUT1="$DIR1/input"
ART1="$DIR1/artifacts"
write_suite "$INPUT1" 8000000 12000000 18000000 4000000 0
assert_pass "checker green run exits 0" \
    bash "$CHECKER" --check-only "$INPUT1" --artifacts-dir "$ART1" --baseline-file "$DIR1/missing.json" --skip-test-lanes
assert_json_eq "green summary status" "$ART1/resize-performance-report.json" '.summary.overall_status' "pass"
assert_json_eq "green hard_fail count" "$ART1/resize-performance-report.json" '.summary.scenario_hard_fail_count' "0"

echo
echo "Test 2: near-threshold becomes warning, not hard fail"
DIR2="$(new_tmpdir)"
INPUT2="$DIR2/input"
ART2="$DIR2/artifacts"
write_suite "$INPUT2" 10000000 18000000 28000000 7600000 0
assert_pass "checker warning run exits 0" \
    bash "$CHECKER" --check-only "$INPUT2" --artifacts-dir "$ART2" --baseline-file "$DIR2/missing.json" --skip-test-lanes
assert_json_eq "warning summary status" "$ART2/resize-performance-report.json" '.summary.overall_status' "warning"
assert_json_eq "warning count > 0 mapped" "$ART2/resize-performance-report.json" '.summary.scenario_warning_count' "5"

echo
echo "Test 3: threshold breach hard-fails"
DIR3="$(new_tmpdir)"
INPUT3="$DIR3/input"
ART3="$DIR3/artifacts"
write_suite "$INPUT3" 12000000 20000000 40000000 4000000 0
assert_fail "checker hard fail exits non-zero" \
    bash "$CHECKER" --check-only "$INPUT3" --artifacts-dir "$ART3" --baseline-file "$DIR3/missing.json" --skip-test-lanes
assert_json_eq "hard fail summary status" "$ART3/resize-performance-report.json" '.summary.overall_status' "hard_fail"

echo
echo "Test 4: baseline drift hard-fails"
DIR4="$(new_tmpdir)"
INPUT4="$DIR4/input"
ART4="$DIR4/artifacts"
BASE4="$DIR4/baseline.json"
write_suite "$INPUT4" 10000000 12100000 20000000 4000000 0
cat >"$BASE4" <<EOF
{
  "version": "1",
  "scenarios": {
    "resize_single_pane_scrollback": {"m1": {"p95_ns": 9000000, "p99_ns": 15000000}},
    "resize_multi_tab_storm": {"m1": {"p95_ns": 9000000, "p99_ns": 15000000}},
    "font_churn_multi_pane": {"m1": {"p95_ns": 9000000, "p99_ns": 15000000}},
    "mixed_scale_soak": {"m1": {"p95_ns": 9000000, "p99_ns": 15000000}},
    "mixed_workload_interactive_streaming": {"m1": {"p95_ns": 9000000, "p99_ns": 15000000}}
  }
}
EOF
assert_fail "baseline drift breach exits non-zero" \
    bash "$CHECKER" --check-only "$INPUT4" --artifacts-dir "$ART4" --baseline-file "$BASE4" --skip-test-lanes
assert_json_eq "baseline drift classified hard_fail" "$ART4/resize-performance-report.json" '.summary.overall_status' "hard_fail"

echo
echo "Test 5: real checker admission and RCH guards with command-only fixtures"
GUARD_DIR="$(new_tmpdir)"
mkdir -p "$GUARD_DIR/bin" "$GUARD_DIR/no-rch-bin"
# A closed PATH prevents either fixture from reaching real Cargo, RCH, SSH,
# or other service tools. These are ordinary shell utilities only.
for tool in dirname basename date jq mkdir awk sed grep cat tee head tail tr wc sort env sleep; do
    resolved="$(command -v "$tool")"
    ln -s "$resolved" "$GUARD_DIR/bin/$tool"
    ln -s "$resolved" "$GUARD_DIR/no-rch-bin/$tool"
done
ln -s "$BASH" "$GUARD_DIR/bin/bash"
ln -s "$BASH" "$GUARD_DIR/no-rch-bin/bash"
if command -v timeout >/dev/null 2>&1; then
    timeout_path="$(command -v timeout)"
else
    timeout_path="$(command -v gtimeout)"
fi
ln -s "$timeout_path" "$GUARD_DIR/bin/timeout"
cat >"$GUARD_DIR/bin/cargo" <<'EOF'
#!/usr/bin/env bash
printf 'LOCAL_CARGO_EXECUTED\n' >>"$FT_RESIZE_FIXTURE_CALLS"
printf 'forbidden local Cargo fixture invoked\n' >&2
exit 97
EOF
chmod +x "$GUARD_DIR/bin/cargo"
ln -s "$GUARD_DIR/bin/cargo" "$GUARD_DIR/no-rch-bin/cargo"
cat >"$GUARD_DIR/bin/rch" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'required=%s self_healing=%s argv=' "${RCH_REQUIRE_REMOTE:-}" "${RCH_NO_SELF_HEALING:-}" >>"$FT_RESIZE_FIXTURE_CALLS"
printf '%q ' "$@" >>"$FT_RESIZE_FIXTURE_CALLS"
printf '\n' >>"$FT_RESIZE_FIXTURE_CALLS"
case "$*" in
    '--json workers probe --all')
        printf '{"workers":[{"id":"vmi-fixture","status":"healthy"}]}\n'
        ;;
    '--json workers capabilities')
        printf '{"workers":[{"capabilities":{"rustc_version":"fixture-only"}}]}\n'
        ;;
    'exec -- '*)
        [[ "$RCH_REQUIRE_REMOTE" == 1 && "$RCH_NO_SELF_HEALING" == 1 ]] || exit 93
        if [[ "$FT_RESIZE_FIXTURE_MODE" == fallback ]]; then
            printf '[RCH] local: planted forbidden fallback\n' >&2
            exit 0
        fi
        # Never execute the payload. The last argument is the scenario path.
        scenario="${!#}"
        scenario="$(basename "$scenario" .yaml)"
        printf 'Selected worker: vmi-fixture\n'
        printf '__FT_RESIZE_GATE_JSON_BEGIN__\n'
        jq -c . "$FT_RESIZE_FIXTURE_INPUT/$scenario.json"
        printf '__FT_RESIZE_GATE_JSON_END__\n'
        printf 'Remote command finished: exit=0 in 1ms\n'
        ;;
    *)
        printf 'unexpected RCH fixture invocation: %s\n' "$*" >&2
        exit 94
        ;;
esac
EOF
chmod +x "$GUARD_DIR/bin/rch"

run_guard_case() {
    local mode="$1" required="$2" fixture_path="$3" case_dir="$GUARD_DIR/$1-$2"
    local rc
    mkdir -p "$case_dir"
    printf 'retain-this-summary\n' >"$case_dir/summary"
    : >"$case_dir/calls"
    if env PATH="$fixture_path" BASH_ENV= _LIB_RCH_GUARDS_LOADED= \
        FT_RESIZE_FIXTURE_MODE="$mode" FT_RESIZE_FIXTURE_INPUT="$INPUT1" \
        FT_RESIZE_FIXTURE_CALLS="$case_dir/calls" \
        RCH_REQUIRE_REMOTE="$required" RCH_SKIP_QUEUE_PREFLIGHT=1 \
        RCH_PROOF_LEDGER_FILE= RCH_MIRROR_REQUIRED_PATHS= \
        RCH_MIRROR_REQUIRE_WORKSPACE_MEMBER_ROOTS=0 RCH_SELECTED_WORKER_MIRROR_PREFLIGHT=0 \
        GITHUB_ACTIONS=true FT_RECORDER_VALIDATION_GITHUB_ACTIONS_LOCAL_CARGO=1 \
        GITHUB_STEP_SUMMARY="$case_dir/summary" \
        "$BASH" "$CHECKER" --skip-test-lanes --artifacts-dir "$case_dir/artifacts" \
        --baseline-file "$case_dir/missing.json" >"$case_dir/output.log" 2>&1; then
        rc=0
    else
        rc=$?
    fi
    cat "$case_dir/output.log"
    # The helper retains the actual stderr-bearing transcript separately.
    for log in "$case_dir/artifacts/scenarios/"*.log; do
        [[ ! -f "$log" ]] || cat "$log"
    done
    return "$rc"
}

assert_pass "strict remote transcript fixture completes all scenarios" \
    run_guard_case remote 1 "$GUARD_DIR/bin"
assert_json_eq "remote fixture reaches checker run mode" \
    "$GUARD_DIR/remote-1/artifacts/resize-performance-report.json" '.mode' "run"
assert_pass "all five scenario payloads routed through RCH fixture" \
    test "$(grep -c 'argv=exec' "$GUARD_DIR/remote-1/calls")" -eq 5
assert_fail "remote admission emits no obsolete command-not-found diagnostic" \
    grep -E 'command not found|rch_github_actions_local_cargo_enabled' "$GUARD_DIR/remote-1/output.log"
assert_pass "obsolete summary destination stays byte-exact" \
    test "$(cat "$GUARD_DIR/remote-1/summary")" = retain-this-summary
assert_fail "exit-zero local fallback transcript is refused" \
    run_guard_case fallback 1 "$GUARD_DIR/bin"
assert_pass "fallback refusal retains the explicit policy diagnostic" \
    grep -F 'refusing offload policy violation' "$GUARD_DIR/fallback-1/output.log"
assert_fail "RCH_REQUIRE_REMOTE=0 cannot enable local execution" \
    run_guard_case disabled 0 "$GUARD_DIR/bin"
assert_pass "disabled remote mode is refused before any runner call" \
    test ! -s "$GUARD_DIR/disabled-0/calls"
assert_fail "missing RCH fails even with obsolete hosted bypass variables" \
    run_guard_case missing 1 "$GUARD_DIR/no-rch-bin"
assert_pass "missing RCH refusal names the required runner" \
    grep -F 'rch is required for cargo execution' "$GUARD_DIR/missing-1/output.log"
for calls in "$GUARD_DIR/"*/calls; do
    assert_fail "local Cargo poison remains untouched: $calls" grep -F LOCAL_CARGO_EXECUTED "$calls"
done

echo
echo "========================================"
echo "resize gate checker tests: $PASS passed, $FAIL failed"

if (( FAIL > 0 )); then
    exit 1
fi

echo "All tests passed."
exit 0
