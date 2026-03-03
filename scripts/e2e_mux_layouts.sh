#!/usr/bin/env bash
set -euo pipefail

# End-to-end validation for mux layout, floating pane, collapse priority,
# constraint-based resize, and resize reflow scorecard features.
#
# Exit codes:
#   0 = pass
#   1 = fail
#   2 = all steps skipped

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

RCH_BIN="${RCH_BIN:-rch}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}"
LOG_DIR="${LOG_DIR:-$PROJECT_ROOT/target/e2e/mux-layouts}"
DRY_RUN=0

step_index=0
pass_count=0
fail_count=0
skip_count=0
LAST_SKIP_REASON=""

mkdir -p "$LOG_DIR"

usage() {
  cat <<'EOF'
Usage: scripts/e2e_mux_layouts.sh [options]

Validates mux layout, floating pane, collapse, constraint, and resize reflow
scorecard functionality via targeted Rust test suites.

Options:
  --dry-run       Print actions without executing them
  -h, --help      Show help

Environment:
  RCH_BIN              rch executable (default: rch)
  CARGO_TARGET_DIR     Cargo target dir
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage; exit 1 ;;
  esac
done

now_ms() {
  python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
}

log() {
  printf '%s\n' "$*"
}

emit_step_json() {
  local status="$1"
  local name="$2"
  local duration_ms="$3"
  local detail="$4"
  printf '{"step":%d,"name":"%s","status":"%s","duration_ms":%d,"detail":"%s"}\n' \
    "$step_index" "$name" "$status" "$duration_ms" "${detail//\"/\\\"}" >&2
}

mark_skip() {
  LAST_SKIP_REASON="$1"
  return 0
}

run_cmd() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "[DRY-RUN] $*"
    return 0
  fi
  "$@"
}

run_step() {
  local name="$1"
  shift
  step_index=$((step_index + 1))
  local start_ms end_ms duration_ms
  start_ms="$(now_ms)"

  if "$@"; then
    end_ms="$(now_ms)"
    duration_ms=$((end_ms - start_ms))
    pass_count=$((pass_count + 1))
    log "[PASS] ${step_index}. ${name} (${duration_ms}ms)"
    emit_step_json "pass" "$name" "$duration_ms" "ok"
    return 0
  else
    local rc=$?
    end_ms="$(now_ms)"
    duration_ms=$((end_ms - start_ms))

    if [[ "$rc" -eq 125 ]]; then
      skip_count=$((skip_count + 1))
      local detail="${LAST_SKIP_REASON:-skipped}"
      LAST_SKIP_REASON=""
      log "[SKIP] ${step_index}. ${name} (${detail})"
      emit_step_json "skip" "$name" "$duration_ms" "$detail"
      return 0
    fi

    fail_count=$((fail_count + 1))
    log "[FAIL] ${step_index}. ${name} (${duration_ms}ms)"
    emit_step_json "fail" "$name" "$duration_ms" "failed"
    return 1
  fi
}

# --- Helper: run cargo test via rch or local fallback ---
run_tests() {
  local package="$1"
  shift
  local filter="${1:-}"

  local test_args=("--lib" "-p" "$package")
  if [[ -n "$filter" ]]; then
    test_args+=("--" "$filter")
  fi

  if command -v "$RCH_BIN" >/dev/null 2>&1; then
    run_cmd "$RCH_BIN" exec -- cargo test "${test_args[@]}" 2>&1 | tee "$LOG_DIR/${package}-${filter:-all}.log"
  else
    run_cmd cargo test "${test_args[@]}" 2>&1 | tee "$LOG_DIR/${package}-${filter:-all}.log"
  fi
}

# ======================================================================
# Step 1: Swap Layout unit tests (layout.rs)
# ======================================================================
step_layout_unit_tests() {
  run_tests "mux" "tests::layout_cycle"
}

# ======================================================================
# Step 2: Swap Layout integration tests with real panes (tab.rs)
# ======================================================================
step_swap_layout_tab_tests() {
  run_tests "mux" "test::swap_layout"
}

# ======================================================================
# Step 3: Floating pane tests (tab.rs)
# ======================================================================
step_floating_pane_tests() {
  run_tests "mux" "test::floating_pane"
}

# ======================================================================
# Step 4: Collapse priority tests (tab.rs)
# ======================================================================
step_collapse_priority_tests() {
  run_tests "mux" "test::collapse"
}

# ======================================================================
# Step 5: Constraint-based resize tests (tab.rs)
# ======================================================================
step_constraint_resize_tests() {
  run_tests "mux" "test::resize"
}

# ======================================================================
# Step 6: Stack cycling tests (tab.rs)
# ======================================================================
step_cycle_stack_tests() {
  run_tests "mux" "test::cycle_stack"
}

# ======================================================================
# Step 7: Codec PDU roundtrip tests for FrankenTerm layout PDUs (lib.rs)
# ======================================================================
step_codec_pdu_roundtrip() {
  run_tests "codec" "swap_to_layout_pdu_roundtrip"
  run_tests "codec" "set_layout_cycle_pdu_roundtrip"
  run_tests "codec" "cycle_stack_pdu_roundtrip"
  run_tests "codec" "select_stack_pane_pdu_roundtrip"
  run_tests "codec" "update_pane_constraints_pdu_roundtrip"
  run_tests "codec" "create_floating_pane_pdu_roundtrip"
  run_tests "codec" "move_floating_pane_pdu_roundtrip"
  run_tests "codec" "set_floating_pane_z_pdu_roundtrip"
  run_tests "codec" "toggle_floating_pane_pdu_roundtrip"
  run_tests "codec" "remove_floating_pane_pdu_roundtrip"
}

# ======================================================================
# Step 8: Resize reflow scorecard tests (screen.rs)
# ======================================================================
step_scorecard_tests() {
  run_tests "frankenterm-term" "tests::scorecard"
}

# ======================================================================
# Step 9: Readability gate tests (screen.rs)
# ======================================================================
step_gate_tests() {
  run_tests "frankenterm-term" "tests::gate"
}

# ======================================================================
# Step 10: Session handler PDU dispatch tests (sessionhandler.rs)
# ======================================================================
step_session_handler_tests() {
  run_tests "frankenterm-mux-server-impl" "sessionhandler::tests"
}

# --- Run all steps ---
log "=== FrankenTerm Mux Layouts E2E Suite ==="
log "Project root: $PROJECT_ROOT"
log "Log dir:      $LOG_DIR"
log ""

run_step "Layout cycle unit tests"         step_layout_unit_tests      || true
run_step "Swap layout tab integration"     step_swap_layout_tab_tests  || true
run_step "Floating pane operations"        step_floating_pane_tests    || true
run_step "Collapse priority under pressure" step_collapse_priority_tests || true
run_step "Constraint-based resize"         step_constraint_resize_tests || true
run_step "Stack cycling (forward/backward)" step_cycle_stack_tests     || true
run_step "Codec PDU roundtrip (10 PDUs)"   step_codec_pdu_roundtrip    || true
run_step "Resize reflow scorecard"         step_scorecard_tests        || true
run_step "Readability gate thresholds"     step_gate_tests             || true
run_step "Session handler PDU dispatch"   step_session_handler_tests  || true

# --- Summary ---
log ""
log "=== Summary ==="
total=$((pass_count + fail_count + skip_count))
log "Total: $total  Pass: $pass_count  Fail: $fail_count  Skip: $skip_count"

if [[ "$fail_count" -gt 0 ]]; then
  log "RESULT: FAIL"
  exit 1
elif [[ "$pass_count" -eq 0 ]]; then
  log "RESULT: ALL SKIPPED"
  exit 2
else
  log "RESULT: PASS"
  exit 0
fi
