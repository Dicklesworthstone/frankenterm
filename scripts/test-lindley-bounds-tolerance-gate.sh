#!/usr/bin/env bash
# scripts/test-lindley-bounds-tolerance-gate.sh — synthetic exercise of
# the Lindley-bounds tolerance gate (br-ft-jfyz7.1).
#
# Drives `scripts/lindley-bounds-build.sh` through two cases that
# verify the gate's exit-code semantics:
#
#   1. **Within tolerance** (FT_LINDLEY_EMPIRICAL_P99_MS=8.5, the
#      documented reference): the script must exit 0. PR-CI does not
#      file a regression bead; release does not abort.
#   2. **Outside tolerance** (FT_LINDLEY_EMPIRICAL_P99_MS=999.0, ~120×
#      the analytical bound): the script must exit 1. PR-CI fires the
#      auto-file-regression-bead step; release aborts before bundle
#      signing.
#
# This is the substrate-level test that backs the bead's
# "Synthetic test with deviation > 20% triggers / ≤ 20% does not"
# acceptance. The PR-CI workflow at .github/workflows/lindley-bounds-check.yml
# inherits the same exit-code contract — passing this test on any
# host is sufficient evidence that the gate semantics hold.
#
# Usage:
#   bash scripts/test-lindley-bounds-tolerance-gate.sh
#
# Exits 0 on both cases passing; 1 on either case failing.
#
# Bead: br-ft-jfyz7.1 (PR-CI regression bead automation).
# Substrate: scripts/lindley-bounds-build.sh +
#   crates/frankenterm-core/examples/lindley_bounds_build.rs (br-ft-43x69).
# Workflow: .github/workflows/lindley-bounds-check.yml (cc_1, br-ft-jfyz7.1).

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

BUILDER="$REPO_ROOT/scripts/lindley-bounds-build.sh"
[[ -x "$BUILDER" ]] || {
  echo "test-lindley-bounds-tolerance-gate: $BUILDER not executable" >&2
  exit 1
}

failures=0

run_case() {
  local label="$1" empirical="$2" expected_exit="$3"
  echo "::group::case: $label (FT_LINDLEY_EMPIRICAL_P99_MS=$empirical, expected exit $expected_exit)"
  local actual_exit
  FT_LINDLEY_EMPIRICAL_P99_MS="$empirical" \
    bash "$BUILDER" --no-write \
    >/dev/null 2>&1
  actual_exit=$?
  if [[ "$actual_exit" -eq "$expected_exit" ]]; then
    echo "PASS: $label exited $actual_exit (matches expected $expected_exit)"
  else
    echo "FAIL: $label exited $actual_exit (expected $expected_exit)"
    failures=$((failures + 1))
  fi
  echo "::endgroup::"
}

# Case 1: within tolerance — uses the documented reference value.
# The substrate's pipeline_delay_bound returns ~8.1 ms; 8.5 ms is
# within ±20% so the gate must pass.
run_case "within_tolerance" "8.5" 0

# Case 2: outside tolerance — 999 ms empirical against ~8.1 ms
# analytical is a ~12_000% deviation, far above the 20% TOLERANCE_PCT.
# The gate must fail with exit 1.
run_case "outside_tolerance" "999.0" 1

if [[ "$failures" -eq 0 ]]; then
  echo
  echo "test-lindley-bounds-tolerance-gate: all cases passed"
  exit 0
else
  echo
  echo "test-lindley-bounds-tolerance-gate: $failures case(s) failed" >&2
  exit 1
fi
