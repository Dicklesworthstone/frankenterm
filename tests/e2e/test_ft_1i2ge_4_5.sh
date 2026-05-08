#!/usr/bin/env bash
# test_ft_1i2ge_4_5.sh — E2E harness for ft-1i2ge.4.5
# Conflict detection and automated deconfliction messaging
#
# Validates:
#   1. Conflict detection types compile and serialize correctly
#   2. File reservation overlap detection works end-to-end
#   3. Concurrent bead claim detection works end-to-end
#   4. Active claim collision detection works end-to-end
#   5. Deconfliction message generation produces structured output
#   6. Conflict detection config round-trips through JSON
#
# Usage: bash tests/e2e/test_ft_1i2ge_4_5.sh
# Requires: rch with at least one reachable remote worker

set -euo pipefail

SCENARIO_ID="ft-1i2ge-4-5"
COMPONENT="mission_loop::conflict_detection"
TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
CORRELATION_ID="${SCENARIO_ID}-$(date +%s)"
LOG_DIR="${TMPDIR:-/tmp}/ft-e2e-${SCENARIO_ID}"
mkdir -p "$LOG_DIR"

DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-ft1i2ge-4-5"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
    CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
    CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${CORRELATION_ID}" "1i2ge_4_5"
ensure_rch_ready

json_escape() {
    local value="$1"
    value=${value//\\/\\\\}
    value=${value//\"/\\\"}
    value=${value//$'\n'/\\n}
    value=${value//$'\r'/\\r}
    value=${value//$'\t'/\\t}
    printf '%s' "$value"
}

json_field() {
    local key="$1"
    local value="$2"
    printf ',"%s":"%s"' "$key" "$(json_escape "$value")"
}

log_structured() {
    local outcome="$1" reason_code="$2" error_code="${3:-}" extra="${4:-}"
    printf '{"timestamp":"%s","component":"%s","scenario_id":"%s","correlation_id":"%s","outcome":"%s","reason_code":"%s","error_code":"%s"%s}\n' \
        "$(json_escape "$TIMESTAMP")" \
        "$(json_escape "$COMPONENT")" \
        "$(json_escape "$SCENARIO_ID")" \
        "$(json_escape "$CORRELATION_ID")" \
        "$(json_escape "$outcome")" \
        "$(json_escape "$reason_code")" \
        "$(json_escape "$error_code")" \
        "$extra" \
        | tee -a "$LOG_DIR/results.jsonl"
}

RCH_FAIL_OPEN_REGEX='\[RCH\] local|running locally'
RCH_PROBE_LOG="$LOG_DIR/rch_probe.log"
RCH_SMOKE_LOG="$LOG_DIR/rch_smoke.log"

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
        log_structured "FAIL" "rch_local_fallback_detected" "RCH-LOCAL-FALLBACK" \
            "$(json_field "input_summary" "$label")$(json_field "artifact_path" "$1")"
        echo "rch fell back to local execution during ${label}; refusing offload policy violation." >&2
        exit 3
    fi
}

count_matches() {
    local pattern="$1"
    local file="$2"
    local count
    count=$(grep -c -- "$pattern" "$file") || {
        local rc=$?
        if [[ ${rc} -eq 1 ]]; then
            count=0
        else
            return "${rc}"
        fi
    }
    printf '%s\n' "$count"
}

# ── Preflight ────────────────────────────────────────────────────────────────

if ! command -v jq &>/dev/null; then
    log_structured "SKIP" "jq_missing" "jq_not_found" ',"input_summary":"jq binary not in PATH"'
    echo "SKIP: jq not found — install jq to run structured assertions"
    exit 0
fi

if ! command -v rch &>/dev/null; then
    log_structured "FAIL" "rch_required_missing" "RCH-E001" ',"input_summary":"rch binary missing"'
    echo "rch is required; refusing local cargo execution." >&2
    exit 1
fi

set +e
run_rch --json workers probe --all >"$RCH_PROBE_LOG" 2>&1
probe_rc=$?
set -e
if [[ $probe_rc -ne 0 ]] || ! probe_has_reachable_workers "$RCH_PROBE_LOG"; then
    log_structured "FAIL" "rch_workers_unhealthy" "RCH-E100" \
        "$(json_field "input_summary" "rch workers probe")$(json_field "artifact_path" "$RCH_PROBE_LOG")"
    echo "rch workers are unavailable; refusing local cargo execution." >&2
    exit 1
fi

set +e
run_rch_cargo check --help >"$RCH_SMOKE_LOG" 2>&1
smoke_rc=$?
set -e
check_rch_fallback_in_logs "rch_remote_smoke" "$RCH_SMOKE_LOG"
if [[ $smoke_rc -ne 0 ]]; then
    log_structured "FAIL" "rch_remote_smoke_failed" "RCH-E101" \
        "$(json_field "input_summary" "cargo check --help")$(json_field "artifact_path" "$RCH_SMOKE_LOG")"
    echo "rch remote smoke preflight failed; refusing local cargo execution." >&2
    exit 1
fi

CARGO_CMD="run_rch_cargo"

echo "=== E2E: ${SCENARIO_ID} — Conflict Detection & Deconfliction ==="
echo "    cargo_cmd=${CARGO_CMD}"
echo "    log_dir=${LOG_DIR}"

# ── Test 1: Unit tests pass ──────────────────────────────────────────────────

echo "[1/3] Running conflict detection unit tests..."
if $CARGO_CMD test --lib -p frankenterm-core --features subprocess-bridge \
    -- mission_loop::tests::conflict_detection 2>"$LOG_DIR/test_stderr.log" | tee "$LOG_DIR/test_stdout.log"; then
    check_rch_fallback_in_logs "conflict_detection_tests" "$LOG_DIR/test_stdout.log" "$LOG_DIR/test_stderr.log"
    PASS_COUNT=$(count_matches "test mission_loop::tests::conflict_detection.*ok" "$LOG_DIR/test_stdout.log")
    log_structured "PASS" "unit_tests_pass" "" "$(json_field "input_summary" "conflict_detection tests")$(json_field "decision_path" "cargo test")$(json_field "artifact_path" "$LOG_DIR/test_stdout.log")$(json_field "pass_count" "$PASS_COUNT")"
    echo "    ✓ ${PASS_COUNT} conflict detection tests passed"
else
    check_rch_fallback_in_logs "conflict_detection_tests" "$LOG_DIR/test_stdout.log" "$LOG_DIR/test_stderr.log"
    log_structured "FAIL" "unit_tests_fail" "E2E001" "$(json_field "input_summary" "conflict_detection tests")$(json_field "artifact_path" "$LOG_DIR/test_stderr.log")"
    echo "    ✗ Unit tests failed — see $LOG_DIR/test_stderr.log"
    exit 1
fi

# ── Test 2: Path overlap and wildcard tests ──────────────────────────────────

echo "[2/3] Running path overlap tests..."
if $CARGO_CMD test --lib -p frankenterm-core --features subprocess-bridge \
    -- mission_loop::tests::paths_overlap 2>>"$LOG_DIR/test_stderr.log" | tee -a "$LOG_DIR/test_stdout.log"; then
    check_rch_fallback_in_logs "paths_overlap_tests" "$LOG_DIR/test_stdout.log" "$LOG_DIR/test_stderr.log"
    PASS_COUNT=$(count_matches "test mission_loop::tests::paths_overlap.*ok" "$LOG_DIR/test_stdout.log")
    log_structured "PASS" "path_overlap_tests_pass" "" "$(json_field "input_summary" "paths_overlap + wildcard")$(json_field "pass_count" "$PASS_COUNT")"
    echo "    ✓ Path overlap tests passed"
else
    check_rch_fallback_in_logs "paths_overlap_tests" "$LOG_DIR/test_stdout.log" "$LOG_DIR/test_stderr.log"
    log_structured "FAIL" "path_overlap_tests_fail" "E2E002"
    exit 1
fi

# ── Test 3: Serde roundtrip tests ────────────────────────────────────────────

echo "[3/3] Running serde roundtrip tests..."
if $CARGO_CMD test --lib -p frankenterm-core --features subprocess-bridge \
    -- mission_loop::tests::conflict_detection_report_serde 2>>"$LOG_DIR/test_stderr.log" | tee -a "$LOG_DIR/test_stdout.log" \
    && $CARGO_CMD test --lib -p frankenterm-core --features subprocess-bridge \
    -- mission_loop::tests::conflict_type_serde 2>>"$LOG_DIR/test_stderr.log" | tee -a "$LOG_DIR/test_stdout.log" \
    && $CARGO_CMD test --lib -p frankenterm-core --features subprocess-bridge \
    -- mission_loop::tests::conflict_resolution_serde 2>>"$LOG_DIR/test_stderr.log" | tee -a "$LOG_DIR/test_stdout.log" \
    && $CARGO_CMD test --lib -p frankenterm-core --features subprocess-bridge \
    -- mission_loop::tests::deconfliction_strategy_serde 2>>"$LOG_DIR/test_stderr.log" | tee -a "$LOG_DIR/test_stdout.log"; then
    check_rch_fallback_in_logs "serde_roundtrip_tests" "$LOG_DIR/test_stdout.log" "$LOG_DIR/test_stderr.log"
    log_structured "PASS" "serde_roundtrip_pass" "" ',"input_summary":"conflict types serde roundtrip"'
    echo "    ✓ Serde roundtrip tests passed"
else
    check_rch_fallback_in_logs "serde_roundtrip_tests" "$LOG_DIR/test_stdout.log" "$LOG_DIR/test_stderr.log"
    log_structured "FAIL" "serde_roundtrip_fail" "E2E003"
    exit 1
fi

echo ""
echo "=== E2E: ${SCENARIO_ID} — ALL PASSED ==="
echo "    Logs: ${LOG_DIR}/results.jsonl"
log_structured "PASS" "e2e_suite_complete" "" ',"input_summary":"all 3 test groups passed"'
