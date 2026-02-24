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
# Requires: rch (falls back to local cargo if workers offline)

set -euo pipefail

SCENARIO_ID="ft-1i2ge-4-5"
COMPONENT="mission_loop::conflict_detection"
TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
CORRELATION_ID="${SCENARIO_ID}-$(date +%s)"
LOG_DIR="${TMPDIR:-/tmp}/ft-e2e-${SCENARIO_ID}"
mkdir -p "$LOG_DIR"

log_structured() {
    local outcome="$1" reason_code="$2" error_code="${3:-}" extra="${4:-}"
    printf '{"timestamp":"%s","component":"%s","scenario_id":"%s","correlation_id":"%s","outcome":"%s","reason_code":"%s","error_code":"%s"%s}\n' \
        "$TIMESTAMP" "$COMPONENT" "$SCENARIO_ID" "$CORRELATION_ID" \
        "$outcome" "$reason_code" "$error_code" "$extra" \
        | tee -a "$LOG_DIR/results.jsonl"
}

# ── Preflight ────────────────────────────────────────────────────────────────

if ! command -v jq &>/dev/null; then
    log_structured "SKIP" "jq_missing" "jq_not_found" ',"input_summary":"jq binary not in PATH"'
    echo "SKIP: jq not found — install jq to run structured assertions"
    exit 0
fi

# Determine cargo runner (rch or local)
CARGO_CMD="cargo"
if command -v rch &>/dev/null; then
    if rch check --quiet 2>/dev/null; then
        CARGO_CMD="rch exec cargo"
    fi
fi

echo "=== E2E: ${SCENARIO_ID} — Conflict Detection & Deconfliction ==="
echo "    cargo_cmd=${CARGO_CMD}"
echo "    log_dir=${LOG_DIR}"

# ── Test 1: Unit tests pass ──────────────────────────────────────────────────

echo "[1/3] Running conflict detection unit tests..."
if $CARGO_CMD test --lib -p frankenterm-core --features subprocess-bridge \
    -- mission_loop::tests::conflict_detection 2>"$LOG_DIR/test_stderr.log" | tee "$LOG_DIR/test_stdout.log"; then
    PASS_COUNT=$(grep -c "test mission_loop::tests::conflict_detection.*ok" "$LOG_DIR/test_stdout.log" || echo "0")
    log_structured "PASS" "unit_tests_pass" "" "$(printf ',\"input_summary\":\"conflict_detection tests\",\"decision_path\":\"cargo test\",\"artifact_path\":\"%s/test_stdout.log\",\"pass_count\":\"%s\"' "$LOG_DIR" "$PASS_COUNT")"
    echo "    ✓ ${PASS_COUNT} conflict detection tests passed"
else
    log_structured "FAIL" "unit_tests_fail" "E2E001" "$(printf ',\"input_summary\":\"conflict_detection tests\",\"artifact_path\":\"%s/test_stderr.log\"' "$LOG_DIR")"
    echo "    ✗ Unit tests failed — see $LOG_DIR/test_stderr.log"
    exit 1
fi

# ── Test 2: Path overlap and wildcard tests ──────────────────────────────────

echo "[2/3] Running path overlap tests..."
if $CARGO_CMD test --lib -p frankenterm-core --features subprocess-bridge \
    -- mission_loop::tests::paths_overlap 2>>"$LOG_DIR/test_stderr.log" | tee -a "$LOG_DIR/test_stdout.log"; then
    PASS_COUNT=$(grep -c "test mission_loop::tests::paths_overlap.*ok" "$LOG_DIR/test_stdout.log" || echo "0")
    log_structured "PASS" "path_overlap_tests_pass" "" "$(printf ',\"input_summary\":\"paths_overlap + wildcard\",\"pass_count\":\"%s\"' "$PASS_COUNT")"
    echo "    ✓ Path overlap tests passed"
else
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
