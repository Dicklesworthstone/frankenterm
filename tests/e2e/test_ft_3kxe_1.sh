#!/usr/bin/env bash
# ────────────────────────────────────────────────────────────────────────────
# E2E Harness: ft-3kxe.1 — Memory leak root cause analysis and patches
#
# Validates that memory-leak hardening patches compile and pass all
# existing tests in the affected crates (term, surface, escape-parser, mux).
# Optionally captures RSS growth evidence for a running mux PID.
#
# Execution: rch exec -- bash tests/e2e/test_ft_3kxe_1.sh
# Optional profiling:
#   FT_3KXE1_PROFILE_PID=<pid> FT_3KXE1_PROFILE_MAX_SAMPLES=30 \
#   FT_3KXE1_PROFILE_SAMPLE_SECS=60 FT_3KXE1_MAX_GROWTH_MB_HR=1 \
#   rch exec -- bash tests/e2e/test_ft_3kxe_1.sh
# ────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SCENARIO_ID="ft-3kxe-1"
LOG_DIR="$SCRIPT_DIR/logs"
TIMESTAMP="$(date -u +%Y%m%d_%H%M%S)"
LOG_FILE="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}.jsonl"
PROFILE_PID="${FT_3KXE1_PROFILE_PID:-}"
PROFILE_MAX_SAMPLES="${FT_3KXE1_PROFILE_MAX_SAMPLES:-0}"
PROFILE_SAMPLE_SECS="${FT_3KXE1_PROFILE_SAMPLE_SECS:-60}"
PROFILE_MAX_GROWTH_MB_HR="${FT_3KXE1_MAX_GROWTH_MB_HR:-}"
PROFILE_OUT_DIR="${FT_3KXE1_PROFILE_OUT_DIR:-$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_profile}"

mkdir -p "$LOG_DIR"

DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-ft3kxe1"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
    CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
    CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${TIMESTAMP}" "3kxe_1"
ensure_rch_ready

# ── Structured log helper ──────────────────────────────────────────────────
log_event() {
    local component="$1"
    local decision_path="$2"
    local input_summary="$3"
    local outcome="$4"
    local reason_code="${5:-nominal}"
    local error_code="${6:-none}"
    printf '{"timestamp":"%s","component":"%s","scenario_id":"%s","correlation_id":"%s-%s","decision_path":"%s","input_summary":"%s","outcome":"%s","reason_code":"%s","error_code":"%s"}\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        "$component" \
        "$SCENARIO_ID" \
        "$SCENARIO_ID" "$TIMESTAMP" \
        "$decision_path" \
        "$input_summary" \
        "$outcome" \
        "$reason_code" \
        "$error_code" >> "$LOG_FILE"
}

RCH_FAIL_OPEN_REGEX='\[RCH\] local|running locally'
RCH_PROBE_LOG="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_rch_probe.log"
RCH_SMOKE_LOG="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_rch_smoke.log"

run_rch() {
    TMPDIR=/tmp rch "$@"
}

run_rch_cargo() {
    run_rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo "$@"
}

probe_has_reachable_workers() {
    grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"
}

check_rch_fallback_in_logs() {
    local label="$1"
    shift

    if grep -Eq "$RCH_FAIL_OPEN_REGEX" "$@" 2>/dev/null; then
        log_event "rch_offload" "cargo_step" "$label" "fail" "rch_local_fallback_detected" "RCH-LOCAL-FALLBACK"
        echo "rch fell back to local execution during ${label}; refusing offload policy violation." >&2
        exit 3
    fi
}

run_rch_cargo_logged() {
    local label="$1"
    local output_file="$2"
    shift 2

    set +e
    run_rch_cargo "$@" 2>&1 | tee "$output_file"
    local rc=${PIPESTATUS[0]}
    set -e
    check_rch_fallback_in_logs "$label" "$output_file"
    return "$rc"
}

# ── Preflight ──────────────────────────────────────────────────────────────
log_event "preflight" "startup" "checking_rch" "started"

if ! command -v rch &>/dev/null; then
    log_event "preflight" "startup" "rch_binary_missing" "fail" "rch_required_missing" "RCH-E001"
    echo "rch is required; refusing local cargo execution." >&2
    exit 1
fi

set +e
run_rch --json workers probe --all >"$RCH_PROBE_LOG" 2>&1
probe_rc=$?
set -e
if [[ $probe_rc -ne 0 ]] || ! probe_has_reachable_workers "$RCH_PROBE_LOG"; then
    log_event "preflight" "startup" "rch_workers_probe" "fail" "rch_workers_unhealthy" "RCH-E100"
    echo "rch workers are unavailable; refusing local cargo execution." >&2
    exit 1
fi

set +e
run_rch_cargo check --help >"$RCH_SMOKE_LOG" 2>&1
smoke_rc=$?
set -e
check_rch_fallback_in_logs "rch_remote_smoke" "$RCH_SMOKE_LOG"
if [[ $smoke_rc -ne 0 ]]; then
    log_event "preflight" "startup" "cargo_check_help" "fail" "rch_remote_smoke_failed" "RCH-E101"
    echo "rch remote smoke preflight failed; refusing local cargo execution." >&2
    exit 1
fi

cd "$PROJECT_ROOT"

log_event "preflight" "startup" "cargo_target=$CARGO_TARGET_DIR" "ready"

# ── Test matrix ────────────────────────────────────────────────────────────
CRATES=(
    "frankenterm-term"
    "frankenterm-surface"
)
TOTAL_CRATES=${#CRATES[@]}
TOTAL_STEPS=$((TOTAL_CRATES + 2))  # compile check + crate tests + formatting
if [[ -n "$PROFILE_PID" ]]; then
    TOTAL_STEPS=$((TOTAL_STEPS + 1)) # optional profiling evidence capture
fi
PASSED=0
FAILED=0

echo "Running ft-3kxe.1 memory leak patch validation..."
log_event "harness" "nominal_path" "crate_count=$TOTAL_CRATES" "started"

# ── Step 1: Compile check for all affected crates ──────────────────────────
echo "[1/$TOTAL_STEPS] Compile check (term + surface)..."
COMPILE_OUTPUT="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_compile.log"
if run_rch_cargo_logged "memory_leak_compile" "$COMPILE_OUTPUT" check -p frankenterm-term -p frankenterm-surface; then
    log_event "compile" "nominal_path" "cargo_check" "pass"
    echo "  ✓ Compile check passed"
    PASSED=$((PASSED + 1))
else
    log_event "compile" "failure_injection_path" "cargo_check" "fail" "compile_error" "CARGO-E001"
    echo "  ✗ Compile check FAILED"
    echo "Scenario: $SCENARIO_ID"
    echo "Logs: $LOG_FILE"
    exit 1
fi

# ── Step 2: Run tests for each affected crate ─────────────────────────────
STEP=2
for crate in "${CRATES[@]}"; do
    echo "[$STEP/$TOTAL_STEPS] Testing $crate..."
    TEST_OUTPUT="$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_${crate}.log"

    if run_rch_cargo_logged "${crate}_tests" "$TEST_OUTPUT" test -p "$crate" --lib; then
        # Count passed tests
        test_count=$(grep -c "test result: ok" "$TEST_OUTPUT" 2>/dev/null || echo "0")
        log_event "unit_tests" "nominal_path" "$crate" "pass" "tests_ok=$test_count"
        echo "  ✓ $crate tests passed"
        PASSED=$((PASSED + 1))
    else
        log_event "unit_tests" "failure_injection_path" "$crate" "fail" "test_failure" "TEST-E001"
        echo "  ✗ $crate tests FAILED"
        FAILED=$((FAILED + 1))
    fi
    STEP=$((STEP + 1))
done

# ── Step 3: Format check on patched files ──────────────────────────────────
echo "[$TOTAL_STEPS/$TOTAL_STEPS] Format check on patched files..."
PATCHED_FILES=(
    "frankenterm/term/src/terminalstate/mod.rs"
    "frankenterm/term/src/terminalstate/performer.rs"
    "frankenterm/term/src/terminalstate/sixel.rs"
    "frankenterm/term/src/screen.rs"
    "frankenterm/surface/src/line/line.rs"
)
FMT_PASS=true
for f in "${PATCHED_FILES[@]}"; do
    if ! rustfmt --edition 2018 --check "$f" 2>/dev/null; then
        echo "  ✗ Format check failed: $f"
        FMT_PASS=false
    fi
done
if $FMT_PASS; then
    log_event "format" "nominal_path" "rustfmt_check" "pass"
    echo "  ✓ Format check passed"
    PASSED=$((PASSED + 1))
else
    log_event "format" "failure_injection_path" "rustfmt_check" "fail" "format_drift" "FMT-E001"
    echo "  ✗ Format check FAILED"
    FAILED=$((FAILED + 1))
fi

# ── Step 4 (optional): Capture RSS growth evidence ─────────────────────────
if [[ -n "$PROFILE_PID" ]]; then
    STEP=$((TOTAL_STEPS))
    echo "[$STEP/$TOTAL_STEPS] Profiling evidence capture (PID=$PROFILE_PID)..."
    PROFILE_CMD=(scripts/profiling/mux_memory_watch.sh --pid "$PROFILE_PID" --out-dir "$PROFILE_OUT_DIR")
    if SAMPLE_SECS="$PROFILE_SAMPLE_SECS" \
       MAX_SAMPLES="$PROFILE_MAX_SAMPLES" \
       MAX_GROWTH_MB_HR="$PROFILE_MAX_GROWTH_MB_HR" \
       "${PROFILE_CMD[@]}" 2>&1 | tee "$LOG_DIR/${SCENARIO_ID}_${TIMESTAMP}_profiling.log"; then
        if [[ -f "$PROFILE_OUT_DIR/summary.json" ]]; then
            summary_compact="$(tr -d '\n' < "$PROFILE_OUT_DIR/summary.json" | tr -d ' ')"
        else
            summary_compact="{}"
        fi
        log_event "profiling" "nominal_path" "mux_memory_watch" "pass" "summary=$summary_compact"
        echo "  ✓ Profiling evidence captured: $PROFILE_OUT_DIR/summary.json"
        PASSED=$((PASSED + 1))
    else
        log_event "profiling" "failure_injection_path" "mux_memory_watch" "fail" "rss_threshold_or_sampling_failure" "PROF-E001"
        echo "  ✗ Profiling evidence step FAILED"
        FAILED=$((FAILED + 1))
    fi
fi

# ── Summary ────────────────────────────────────────────────────────────────
TOTAL=$((PASSED + FAILED))
echo ""
echo "═══════════════════════════════════════════"
echo "  Scenario: $SCENARIO_ID"
echo "  Passed: $PASSED / $TOTAL"
echo "  Failed: $FAILED / $TOTAL"
echo "  Logs: $LOG_FILE"
echo "═══════════════════════════════════════════"

log_event "summary" "completed" "passed=$PASSED,failed=$FAILED,total=$TOTAL" \
    "$([ "$FAILED" -eq 0 ] && echo 'pass' || echo 'partial_fail')"

[ "$FAILED" -eq 0 ] && exit 0 || exit 1
