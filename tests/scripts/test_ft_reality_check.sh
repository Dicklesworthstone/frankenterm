#!/usr/bin/env bash
# G57 / ft-tf6g3.45: test harness for scripts/ft-reality-check.sh.
#
# Exercises each subcommand (status / next / silent-close-audit /
# structure-audit / is-due / epic / --help) and asserts shape +
# exit-code behavior. Does NOT verify the substantive correctness
# of bv triage / br queries — those are covered by their own
# upstream tests; this harness checks the wrapper contract.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
SUT="${REPO_ROOT}/scripts/ft-reality-check.sh"

command -v jq >/dev/null || { echo "jq required" >&2; exit 2; }
[[ -x "$SUT" ]] || { echo "$SUT not executable" >&2; exit 2; }

PASS=0
FAIL=0
pass() { printf 'PASS: %s\n' "$1"; PASS=$((PASS+1)); }
fail() { printf 'FAIL: %s\n  %s\n' "$1" "$2" >&2; FAIL=$((FAIL+1)); }

assert_jq() {
  local label="$1" json="$2" path="$3" expected="$4"
  local actual
  actual=$(printf '%s' "$json" | jq -r "$path") || { fail "$label" "jq failed"; return; }
  if [[ "$actual" == "$expected" ]]; then
    pass "$label ($path == $expected)"
  else
    fail "$label" "$path expected=$expected got=$actual"
  fi
}

assert_has_key() {
  local label="$1" json="$2" path="$3"
  local present
  present=$(printf '%s' "$json" | jq -r "$path | type") || { fail "$label" "jq failed"; return; }
  if [[ "$present" != "null" ]]; then
    pass "$label ($path present, type=$present)"
  else
    fail "$label" "$path missing"
  fi
}

assert_clean_cli_error() {
  local label="$1" expected_rc="$2" expected_text="$3"
  shift 3
  local out
  set +e
  out=$("$SUT" "$@" 2>&1 >/dev/null)
  local rc=$?
  set -e
  if [[ $rc -ne $expected_rc ]]; then
    fail "$label" "expected rc=$expected_rc, got rc=$rc; output=$out"
    return
  fi
  if [[ "$out" != *"$expected_text"* ]]; then
    fail "$label" "expected error text ${expected_text}; output=$out"
    return
  fi
  if [[ "$out" == *"unbound variable"* ]]; then
    fail "$label" "leaked raw shell error: $out"
    return
  fi
  pass "$label"
}

# ---------------------------------------------------------------------------
# 1: --help exits 0
# ---------------------------------------------------------------------------
t_help_exits_zero() {
  set +e
  "$SUT" --help >/dev/null 2>&1
  local rc=$?
  set -e
  if [[ $rc -eq 0 ]]; then pass "--help exits 0"; else fail "--help exits 0" "got rc=$rc"; fi
}

# ---------------------------------------------------------------------------
# 2: no subcommand -> usage + exit 2
# ---------------------------------------------------------------------------
t_no_subcommand_exits_two() {
  set +e
  "$SUT" >/dev/null 2>&1
  local rc=$?
  set -e
  if [[ $rc -eq 2 ]]; then pass "no-subcommand exits 2"; else fail "no-subcommand exits 2" "got rc=$rc"; fi
}

# ---------------------------------------------------------------------------
# 3: unknown subcommand -> exit 2
# ---------------------------------------------------------------------------
t_unknown_subcommand_exits_two() {
  set +e
  "$SUT" not-a-real-subcommand >/dev/null 2>&1
  local rc=$?
  set -e
  if [[ $rc -eq 2 ]]; then pass "unknown subcommand exits 2"; else fail "unknown subcommand exits 2" "got rc=$rc"; fi
}

t_missing_epic_value_exits_two_cleanly() {
  assert_clean_cli_error "missing --epic value exits 2 cleanly" 2 "error: --epic requires a value" status --epic
}

t_unknown_option_exits_two_cleanly() {
  assert_clean_cli_error "unknown option exits 2 cleanly" 2 "unknown option: --not-real" status --not-real
}

t_unexpected_subcommand_argument_exits_two() {
  assert_clean_cli_error "unexpected status argument exits 2 cleanly" 2 "unexpected argument for status: extra" status extra
}

t_epic_extra_argument_exits_two() {
  assert_clean_cli_error "unexpected epic extra argument exits 2 cleanly" 2 "unexpected extra argument for epic: beta" epic alpha beta
}

# ---------------------------------------------------------------------------
# 4: status --json emits required fields
# ---------------------------------------------------------------------------
t_status_json_shape() {
  set +e
  local out
  out=$("$SUT" status --json 2>/dev/null)
  set -e
  for key in '.epic' '.open' '.blocked' '.in_progress' '.closed' '.project_health'; do
    assert_has_key "status JSON has $key" "$out" "$key"
  done
}

# ---------------------------------------------------------------------------
# 5: next --json returns .next field
# ---------------------------------------------------------------------------
t_next_json_shape() {
  set +e
  local out
  out=$("$SUT" next --json 2>/dev/null)
  set -e
  # .next is either null OR an object with id+title+score
  local next_type
  next_type=$(printf '%s' "$out" | jq -r '.next | type')
  if [[ "$next_type" == "object" || "$next_type" == "null" ]]; then
    pass "next JSON has .next field (type=$next_type)"
  else
    fail "next JSON has .next field" "type=$next_type"
  fi
}

# ---------------------------------------------------------------------------
# 6: epic --json shows default_epic
# ---------------------------------------------------------------------------
t_epic_json_default() {
  set +e
  local out
  out=$("$SUT" epic --json 2>/dev/null)
  set -e
  assert_jq "epic default" "$out" '.default_epic' 'ft-tf6g3'
}

# ---------------------------------------------------------------------------
# 7: --epic override changes scope reported by epic subcommand
# ---------------------------------------------------------------------------
t_epic_override() {
  set +e
  local out
  out=$("$SUT" epic --json --epic alt-test-epic 2>/dev/null)
  set -e
  assert_jq "epic --epic override" "$out" '.default_epic' 'alt-test-epic'
}

# ---------------------------------------------------------------------------
# 8: is-due --json has signals block
# ---------------------------------------------------------------------------
t_is_due_json_shape() {
  set +e
  local out
  out=$("$SUT" is-due --json 2>/dev/null)
  set -e
  assert_has_key "is-due JSON has .signals" "$out" '.signals'
}

# ---------------------------------------------------------------------------
# 9: silent-close-audit --json reports phantom-close count
# ---------------------------------------------------------------------------
t_silent_close_audit_json_shape() {
  set +e
  local out
  out=$("$SUT" silent-close-audit --json 2>/dev/null)
  set -e
  for key in '.epic' '.total_closed' '.phantom_close_count' '.phantom_close_ids'; do
    assert_has_key "silent-close-audit JSON has $key" "$out" "$key"
  done
}

# ---------------------------------------------------------------------------
# 10: $FT_REALITY_CHECK_EPIC env var overrides default
# ---------------------------------------------------------------------------
t_env_var_override() {
  set +e
  local out
  out=$(FT_REALITY_CHECK_EPIC=env-override-epic "$SUT" epic --json 2>/dev/null)
  set -e
  assert_jq "env var override" "$out" '.default_epic' 'env-override-epic'
}

t_help_exits_zero
t_no_subcommand_exits_two
t_unknown_subcommand_exits_two
t_missing_epic_value_exits_two_cleanly
t_unknown_option_exits_two_cleanly
t_unexpected_subcommand_argument_exits_two
t_epic_extra_argument_exits_two
t_status_json_shape
t_next_json_shape
t_epic_json_default
t_epic_override
t_is_due_json_shape
t_silent_close_audit_json_shape
t_env_var_override

printf '\n---\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]] || exit 1
