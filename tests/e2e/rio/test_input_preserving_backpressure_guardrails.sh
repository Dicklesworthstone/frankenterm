#!/usr/bin/env bash
# Input-preserving backpressure guardrail validation.
# Bead: ft-1u90p.9.1.2
#
# Validates:
#   - Reserve-floor and surge-reserve config/logic anchors exist.
#   - Targeted unit/integration tests pass under rch-offloaded cargo execution.
#   - Structured JSONL guardrail reason-code traces are emitted and complete.
#
# Artifacts:
#   e2e-artifacts/rio/input_preserving_backpressure_guardrails/<run_id>/guardrail_decisions.jsonl
#   e2e-artifacts/rio/input_preserving_backpressure_guardrails/<run_id>/guardrail_trace.jsonl
#   e2e-artifacts/rio/input_preserving_backpressure_guardrails/<run_id>/summary.json

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "${SCRIPT_DIR}/harness.sh"
parse_harness_args "$@"

SCENARIO="input_preserving_backpressure_guardrails"
ARTIFACT_DIR=$(setup_artifact_dir "$SCENARIO")
DECISIONS_JSONL="${ARTIFACT_DIR}/guardrail_decisions.jsonl"
TRACE_JSONL="${ARTIFACT_DIR}/guardrail_trace.jsonl"

scenario_header "Input-Preserving Backpressure Guardrails"

RESIZE_SCHED="${SCRIPT_DIR}/../../../crates/frankenterm-core/src/resize_scheduler.rs"

# ── Phase 1: Anchor verification ───────────────────────────────
echo "[Phase 1] Verifying reserve-floor/surge anchors in resize scheduler..."

anchor_hits=0
anchor_total=4

if [[ -f "$RESIZE_SCHED" ]] && grep -q "input_resize_floor_units" "$RESIZE_SCHED"; then
  anchor_hits=$((anchor_hits + 1))
  echo "  PASS: input_resize_floor_units anchor found"
else
  echo "  FAIL: input_resize_floor_units anchor missing"
fi

if [[ -f "$RESIZE_SCHED" ]] && grep -q "input_surge_backlog_threshold" "$RESIZE_SCHED"; then
  anchor_hits=$((anchor_hits + 1))
  echo "  PASS: input_surge_backlog_threshold anchor found"
else
  echo "  FAIL: input_surge_backlog_threshold anchor missing"
fi

if [[ -f "$RESIZE_SCHED" ]] && grep -q "input_surge_reserve_units" "$RESIZE_SCHED"; then
  anchor_hits=$((anchor_hits + 1))
  echo "  PASS: input_surge_reserve_units anchor found"
else
  echo "  FAIL: input_surge_reserve_units anchor missing"
fi

if [[ -f "$RESIZE_SCHED" ]] && grep -q "resize_guardrail_budget_decision" "$RESIZE_SCHED"; then
  anchor_hits=$((anchor_hits + 1))
  echo "  PASS: structured guardrail decision log anchor found"
else
  echo "  FAIL: structured guardrail decision log anchor missing"
fi

if [[ $anchor_hits -eq $anchor_total ]]; then
  PASS_COUNT=$((PASS_COUNT + 1))
  phase1_outcome="pass"
else
  FAIL_COUNT=$((FAIL_COUNT + 1))
  phase1_outcome="fail"
fi

log_jsonl "$DECISIONS_JSONL" "$SCENARIO" "anchor_verification" "$phase1_outcome" \
  "anchors_found=${anchor_hits}" "anchors_expected=${anchor_total}" \
  "reason_codes_expected=3" "floor_enforced=true" "surge_enabled=true"

# ── Phase 2: Targeted test execution ───────────────────────────
echo "[Phase 2] Running targeted guardrail tests (rch-offloaded via harness)..."

run_targeted_guardrail_test() {
  local label="$1"
  shift
  local tmp_log="${ARTIFACT_DIR}/.${label}.tmp.log"
  local rc=0

  if command -v rch &>/dev/null; then
    if ! TMPDIR=/tmp rch exec -- cargo test -p frankenterm-core "$@" >"$tmp_log" 2>&1; then
      rc=$?
    fi
  else
    if ! env CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/ft-target}" \
      cargo test -p frankenterm-core "$@" >"$tmp_log" 2>&1; then
      rc=$?
    fi
  fi

  cat "$tmp_log" >> "${ARTIFACT_DIR}/guardrail_test_output.txt"
  if rg -q "\\[RCH\\] local" "${ARTIFACT_DIR}/guardrail_test_output.txt"; then
    echo "  FAIL: local fallback detected during ${label}"
    return 97
  fi
  return "$rc"
}

: > "${ARTIFACT_DIR}/guardrail_test_output.txt"
phase2_failed=0

echo "  Running unit test: input_guardrail_surge_reserve_activates_and_reports_reason_code"
run_targeted_guardrail_test \
  "unit_guardrail_surge" \
  --lib \
  resize_scheduler::tests::input_guardrail_surge_reserve_activates_and_reports_reason_code \
  -- --nocapture || phase2_failed=1

echo "  Running unit test: input_guardrail_floor_clamps_surge_reserve"
run_targeted_guardrail_test \
  "unit_guardrail_floor_clamp" \
  --lib \
  resize_scheduler::tests::input_guardrail_floor_clamps_surge_reserve \
  -- --nocapture || phase2_failed=1

echo "  Running unit test: input_guardrail_saturates_cleanly_near_u32_max"
run_targeted_guardrail_test \
  "unit_guardrail_saturation" \
  --lib \
  resize_scheduler::tests::input_guardrail_saturates_cleanly_near_u32_max \
  -- --nocapture || phase2_failed=1

echo "  Running integration test: e2e_guardrail_reason_code_trace"
run_targeted_guardrail_test \
  "integration_guardrail_trace" \
  --test resize_scheduler_input_guardrail_integration \
  e2e_guardrail_reason_code_trace \
  -- --nocapture || phase2_failed=1

if [[ "$phase2_failed" -eq 0 ]]; then
  PASS_COUNT=$((PASS_COUNT + 1))
  echo "  PASS: targeted guardrail tests"
  log_jsonl "$DECISIONS_JSONL" "$SCENARIO" "targeted_tests" "pass" \
    "tests=unit_surge,unit_floor_clamp,unit_saturation,integration_trace" \
    "rch_expected=true" "test_output=guardrail_test_output.txt"
else
  FAIL_COUNT=$((FAIL_COUNT + 1))
  echo "  FAIL: targeted guardrail tests"
  log_jsonl "$DECISIONS_JSONL" "$SCENARIO" "targeted_tests" "fail" \
    "tests=unit_surge,unit_floor_clamp,unit_saturation,integration_trace" \
    "rch_expected=true" "test_output=guardrail_test_output.txt"
fi

# ── Phase 3: JSONL trace extraction + reason-code gates ────────
echo "[Phase 3] Extracting guardrail JSONL trace and validating reason-code coverage..."

grep -E '^\{.*"test_name":"e2e_guardrail_reason_code_trace".*\}$' \
  "${ARTIFACT_DIR}/guardrail_test_output.txt" > "$TRACE_JSONL" || true

trace_count=$(wc -l < "$TRACE_JSONL" | tr -d ' ')

if [[ "${trace_count}" -ge 3 ]]; then
  PASS_COUNT=$((PASS_COUNT + 1))
  trace_outcome="pass"
  echo "  PASS: extracted ${trace_count} guardrail trace records"
else
  FAIL_COUNT=$((FAIL_COUNT + 1))
  trace_outcome="fail"
  echo "  FAIL: expected >=3 guardrail trace records, got ${trace_count}"
fi

for reason in backlog_below_threshold base_reserve base_plus_surge; do
  if grep -q "\"input_guardrail_reason_code\":\"${reason}\"" "$TRACE_JSONL"; then
    PASS_COUNT=$((PASS_COUNT + 1))
    echo "  PASS: reason code present: ${reason}"
  else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    echo "  FAIL: reason code missing: ${reason}"
  fi
done

log_jsonl "$DECISIONS_JSONL" "$SCENARIO" "trace_validation" "$trace_outcome" \
  "trace_count=${trace_count}" "required_reason_codes=3" \
  "reason_code_backlog_below_threshold=$(grep -c '\"input_guardrail_reason_code\":\"backlog_below_threshold\"' "$TRACE_JSONL" || true)" \
  "reason_code_base_reserve=$(grep -c '\"input_guardrail_reason_code\":\"base_reserve\"' "$TRACE_JSONL" || true)" \
  "reason_code_base_plus_surge=$(grep -c '\"input_guardrail_reason_code\":\"base_plus_surge\"' "$TRACE_JSONL" || true)"

# ── Summary artifact ────────────────────────────────────────────
cat > "${ARTIFACT_DIR}/guardrail_validation_summary.json" <<EOF
{
  "run_id": "${RUN_ID}",
  "scenario": "${SCENARIO}",
  "anchors_found": ${anchor_hits},
  "anchors_expected": ${anchor_total},
  "trace_records": ${trace_count},
  "required_reason_codes": [
    "backlog_below_threshold",
    "base_reserve",
    "base_plus_surge"
  ],
  "artifacts": {
    "decision_log": "guardrail_decisions.jsonl",
    "trace_log": "guardrail_trace.jsonl",
    "test_output": "guardrail_test_output.txt"
  }
}
EOF

write_summary "${ARTIFACT_DIR}/summary.json" "$SCENARIO"
scenario_footer "Input-Preserving Backpressure Guardrails"
