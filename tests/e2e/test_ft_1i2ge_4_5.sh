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

run_cargo_step() {
    local label="$1"
    shift

    local step_log="$LOG_DIR/${label}.log"
    local test_cmd=(env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo "$@")

    set +e
    run_rch_cargo_logged "$step_log" "${test_cmd[@]}"
    local rc=$?
    set -e

    cat "$step_log" >>"$LOG_DIR/test_stdout.log"
    return "$rc"
}

# ── Preflight ────────────────────────────────────────────────────────────────

if ! command -v jq &>/dev/null; then
    log_structured "SKIP" "jq_missing" "jq_not_found" ',"input_summary":"jq binary not in PATH"'
    echo "SKIP: jq not found — install jq to run structured assertions"
    exit 0
fi

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${CORRELATION_ID}" "1i2ge_4_5"
ensure_rch_ready

: >"$LOG_DIR/test_stdout.log"

echo "=== E2E: ${SCENARIO_ID} — Conflict Detection & Deconfliction ==="
echo "    cargo_cmd=run_rch_cargo_logged"
echo "    log_dir=${LOG_DIR}"

# ── Test 1: Unit tests pass ──────────────────────────────────────────────────

echo "[1/3] Running conflict detection unit tests..."
if run_cargo_step "conflict_detection_tests" test --lib -p frankenterm-core --features subprocess-bridge \
    -- mission_loop::tests::conflict_detection; then
    PASS_COUNT=$(count_matches "test mission_loop::tests::conflict_detection.*ok" "$LOG_DIR/test_stdout.log")
    log_structured "PASS" "unit_tests_pass" "" "$(json_field "input_summary" "conflict_detection tests")$(json_field "decision_path" "cargo test")$(json_field "artifact_path" "$LOG_DIR/test_stdout.log")$(json_field "pass_count" "$PASS_COUNT")"
    echo "    ✓ ${PASS_COUNT} conflict detection tests passed"
else
    log_structured "FAIL" "unit_tests_fail" "E2E001" "$(json_field "input_summary" "conflict_detection tests")$(json_field "artifact_path" "$LOG_DIR/conflict_detection_tests.log")"
    echo "    ✗ Unit tests failed — see $LOG_DIR/conflict_detection_tests.log"
    exit 1
fi

# ── Test 2: Path overlap and wildcard tests ──────────────────────────────────

echo "[2/3] Running path overlap tests..."
if run_cargo_step "paths_overlap_tests" test --lib -p frankenterm-core --features subprocess-bridge \
    -- mission_loop::tests::paths_overlap; then
    PASS_COUNT=$(count_matches "test mission_loop::tests::paths_overlap.*ok" "$LOG_DIR/test_stdout.log")
    log_structured "PASS" "path_overlap_tests_pass" "" "$(json_field "input_summary" "paths_overlap + wildcard")$(json_field "pass_count" "$PASS_COUNT")"
    echo "    ✓ Path overlap tests passed"
else
    log_structured "FAIL" "path_overlap_tests_fail" "E2E002"
    exit 1
fi

# ── Test 3: Serde roundtrip tests ────────────────────────────────────────────

echo "[3/3] Running serde roundtrip tests..."
if run_cargo_step "conflict_detection_report_serde" test --lib -p frankenterm-core --features subprocess-bridge \
    -- mission_loop::tests::conflict_detection_report_serde \
    && run_cargo_step "conflict_type_serde" test --lib -p frankenterm-core --features subprocess-bridge \
        -- mission_loop::tests::conflict_type_serde \
    && run_cargo_step "conflict_resolution_serde" test --lib -p frankenterm-core --features subprocess-bridge \
        -- mission_loop::tests::conflict_resolution_serde \
    && run_cargo_step "deconfliction_strategy_serde" test --lib -p frankenterm-core --features subprocess-bridge \
        -- mission_loop::tests::deconfliction_strategy_serde; then
    log_structured "PASS" "serde_roundtrip_pass" "" ',"input_summary":"conflict types serde roundtrip"'
    echo "    ✓ Serde roundtrip tests passed"
else
    log_structured "FAIL" "serde_roundtrip_fail" "E2E003"
    exit 1
fi

echo ""
echo "=== E2E: ${SCENARIO_ID} — ALL PASSED ==="
echo "    Logs: ${LOG_DIR}/results.jsonl"
log_structured "PASS" "e2e_suite_complete" "" ',"input_summary":"all 3 test groups passed"'
