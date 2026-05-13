#!/usr/bin/env bash
# G61 / ft-tf6g3.49: trigger-validation tests for the G31 reality-check
# discipline cron script (scripts/check-reality-check-due.sh).
#
# Each test drives the script via its --as-of / --open-threshold /
# --contract-diff-threshold / --claim-growth-threshold / --calendar-days
# flags to make a target trigger fire (or not), then asserts on the JSON
# output. Strict-mode exit code is checked separately.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
SUT="${REPO_ROOT}/scripts/check-reality-check-due.sh"

command -v jq >/dev/null || { echo "jq required" >&2; exit 2; }
[[ -x "$SUT" ]] || { echo "$SUT not executable" >&2; exit 2; }

PASS=0
FAIL=0

# ----------------------------------------------------------------------------
# Test helpers
# ----------------------------------------------------------------------------

# pretty-pass / pretty-fail emit one line per assertion so the test harness
# output is easily greppable from CI logs.
pass() { printf 'PASS: %s\n' "$1"; PASS=$((PASS+1)); }
fail() { printf 'FAIL: %s\n  %s\n' "$1" "$2" >&2; FAIL=$((FAIL+1)); }

assert_json() {
  local label="$1" json="$2" jq_path="$3" expected="$4"
  local actual
  actual=$(printf '%s' "$json" | jq -r "$jq_path") || {
    fail "$label" "jq path $jq_path failed against output"; return
  }
  if [[ "$actual" == "$expected" ]]; then
    pass "$label ($jq_path == $expected)"
  else
    fail "$label" "$jq_path expected $expected, got $actual"
  fi
}

# ----------------------------------------------------------------------------
# Trigger 1: calendar (quarterly minimum, default 90 days)
# ----------------------------------------------------------------------------

t_calendar_fires_past_threshold() {
  # The script reads the LATEST reality-check artifact date from the repo.
  # Forcing a calendar-days threshold of 1 means any age past today's date
  # fires the trigger as long as a prior reality-check artifact exists.
  local out
  out=$("$SUT" --json --as-of "2027-01-01" --calendar-days 90)
  assert_json "calendar fires when 230 days > 90 days" "$out" '.signals.calendar.triggered' 'true'
}

t_calendar_does_not_fire_under_threshold() {
  local out
  out=$("$SUT" --json --as-of "2026-05-12" --calendar-days 90)
  assert_json "calendar does NOT fire when 0 days < 90 days" "$out" '.signals.calendar.triggered' 'false'
}

t_calendar_off_by_one() {
  # 89 days < 90 days threshold -> no fire
  local out
  out=$("$SUT" --json --as-of "2026-08-09" --calendar-days 90)
  assert_json "calendar does NOT fire at 89 days" "$out" '.signals.calendar.triggered' 'false'
  # 91 days > 90 days threshold -> fire
  local out2
  out2=$("$SUT" --json --as-of "2026-08-11" --calendar-days 90)
  assert_json "calendar fires at 91 days" "$out2" '.signals.calendar.triggered' 'true'
}

# ----------------------------------------------------------------------------
# Trigger 3: open beads
# ----------------------------------------------------------------------------

t_open_beads_fires_below_threshold() {
  # Setting threshold to 1 guarantees fire since open beads >= 1 in any
  # non-empty repo.
  local out
  out=$("$SUT" --json --open-threshold 1)
  assert_json "open-beads fires at threshold 1" "$out" '.signals.open_beads.triggered' 'true'
}

t_open_beads_does_not_fire_at_high_threshold() {
  # Setting threshold to 10_000 guarantees no fire (the project has
  # far less than that open).
  local out
  out=$("$SUT" --json --open-threshold 10000)
  assert_json "open-beads does NOT fire at threshold 10_000" "$out" '.signals.open_beads.triggered' 'false'
}

# ----------------------------------------------------------------------------
# Trigger 4: contract-doc churn
# ----------------------------------------------------------------------------

t_contract_doc_churn_threshold_respected() {
  # With a threshold of 1, any change to a contract doc since the latest
  # reality-check would fire. With 1_000_000, no churn would.
  local out_high
  out_high=$("$SUT" --json --contract-diff-threshold 1000000)
  assert_json "contract-doc churn does NOT fire at threshold 1M" "$out_high" '.signals.contract_doc_churn.triggered' 'false'
}

# ----------------------------------------------------------------------------
# Trigger 5: README headline-claim growth
# ----------------------------------------------------------------------------

t_headline_growth_threshold_respected() {
  local out_high
  out_high=$("$SUT" --json --claim-growth-threshold 1000)
  assert_json "headline growth does NOT fire at threshold 1000" "$out_high" '.signals.readme_headline_claims.triggered' 'false'
}

# ----------------------------------------------------------------------------
# Composite: 'due' is true when ANY trigger fires
# ----------------------------------------------------------------------------

t_due_composite_any_trigger() {
  # Make only the open-beads signal fire; assert .due is true.
  local out
  out=$("$SUT" --json --open-threshold 1 --calendar-days 100000 --contract-diff-threshold 1000000 --claim-growth-threshold 1000)
  assert_json "due is true when only open-beads fires" "$out" '.due' 'true'
}

t_due_false_when_no_trigger_fires() {
  # Push every threshold high enough to silence all triggers + an as-of
  # date matching the latest reality-check.
  local out
  out=$("$SUT" --json --as-of "2026-05-12" --calendar-days 100000 --open-threshold 1000000 --contract-diff-threshold 1000000 --claim-growth-threshold 1000)
  assert_json "due is false when no trigger fires" "$out" '.due' 'false'
}

# ----------------------------------------------------------------------------
# Strict-mode exit code
# ----------------------------------------------------------------------------

t_strict_exits_nonzero_when_due() {
  set +e
  "$SUT" --strict --open-threshold 1 --calendar-days 100000 --contract-diff-threshold 1000000 --claim-growth-threshold 1000 >/dev/null 2>&1
  local rc=$?
  set -e
  if [[ $rc -eq 1 ]]; then
    pass "strict mode exits 1 when due=true"
  else
    fail "strict mode exits 1 when due=true" "got exit code $rc"
  fi
}

t_strict_exits_zero_when_not_due() {
  set +e
  "$SUT" --strict --as-of "2026-05-12" --calendar-days 100000 --open-threshold 1000000 --contract-diff-threshold 1000000 --claim-growth-threshold 1000 >/dev/null 2>&1
  local rc=$?
  set -e
  if [[ $rc -eq 0 ]]; then
    pass "strict mode exits 0 when due=false"
  else
    fail "strict mode exits 0 when due=false" "got exit code $rc"
  fi
}

# ----------------------------------------------------------------------------
# JSON shape conformance
# ----------------------------------------------------------------------------

t_json_top_level_keys_present() {
  local out
  out=$("$SUT" --json)
  for key in as_of latest_reality_check_date due signals; do
    local present
    present=$(printf '%s' "$out" | jq -r --arg k "$key" 'has($k)')
    if [[ "$present" == "true" ]]; then
      pass "JSON has top-level key $key"
    else
      fail "JSON top-level key $key" "key missing"
    fi
  done
}

t_json_all_five_signals_present() {
  local out
  out=$("$SUT" --json)
  for sig in calendar minor_version open_beads contract_doc_churn readme_headline_claims; do
    local present
    present=$(printf '%s' "$out" | jq -r --arg k "$sig" '.signals | has($k)')
    if [[ "$present" == "true" ]]; then
      pass "JSON has signal $sig"
    else
      fail "JSON signal $sig" "signal missing"
    fi
  done
}

# ----------------------------------------------------------------------------
# Help mode
# ----------------------------------------------------------------------------

t_help_mode_exits_zero() {
  set +e
  "$SUT" --help >/dev/null 2>&1
  local rc=$?
  set -e
  if [[ $rc -eq 0 ]]; then
    pass "--help exits 0"
  else
    fail "--help exits 0" "got exit code $rc"
  fi
}

# ----------------------------------------------------------------------------
# Run all tests
# ----------------------------------------------------------------------------

t_calendar_fires_past_threshold
t_calendar_does_not_fire_under_threshold
t_calendar_off_by_one
t_open_beads_fires_below_threshold
t_open_beads_does_not_fire_at_high_threshold
t_contract_doc_churn_threshold_respected
t_headline_growth_threshold_respected
t_due_composite_any_trigger
t_due_false_when_no_trigger_fires
t_strict_exits_nonzero_when_due
t_strict_exits_zero_when_not_due
t_json_top_level_keys_present
t_json_all_five_signals_present
t_help_mode_exits_zero

printf '\n---\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]] || exit 1
