#!/usr/bin/env bash
# Exercise formal state-space coverage measurement gates without running TLC.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT}/target/test-logs/spec-coverage/release-gate"

mkdir -p "$LOG_DIR"

fixture="${LOG_DIR}/synthetic-ring-buffer.tla"
coverage_cfg="${LOG_DIR}/synthetic-ring-buffer.coverage.cfg"
pass_summary="${LOG_DIR}/tlc-pass-summary.json"
warn_summary="${LOG_DIR}/tlc-warn-summary.json"
fail_summary="${LOG_DIR}/tlc-fail-summary.json"
timeout_summary="${LOG_DIR}/tlc-timeout-summary.json"

cat >"$fixture" <<'TLA'
\* coverage-metric:
\*   subsystem: synthetic-ring-buffer
\*   declared-invariants: SafetyInvariants
\*   max-depth: 2
\*   branching-factor: 1
\*   threshold-pct: 50
\*   coverage-cfg: target/test-logs/spec-coverage/release-gate/synthetic-ring-buffer.coverage.cfg
TLA

cat >"$coverage_cfg" <<'CFG'
SPECIFICATION Spec

CONSTANTS
  StateCount = 3

INVARIANT SafetyInvariants
CFG

dry_run_json="$(bash "${ROOT}/scripts/run-tlc.sh" --dry-run --cfg "$coverage_cfg" "$fixture")"
jq -e --arg cfg "$coverage_cfg" '.cfg == $cfg' >/dev/null <<<"$dry_run_json"

cat >"$pass_summary" <<'JSON'
{
  "ok": true,
  "state-count": 3,
  "distinct-state-count": 3,
  "time-budget": {"seconds": 10, "enforced": true, "timed-out": false},
  "invariant-results": [{"name": "SafetyInvariants", "status": "pass"}]
}
JSON

cat >"$warn_summary" <<'JSON'
{
  "ok": true,
  "state-count": 3,
  "distinct-state-count": 1,
  "time-budget": {"seconds": 10, "enforced": true, "timed-out": false},
  "invariant-results": [{"name": "SafetyInvariants", "status": "pass"}]
}
JSON

cat >"$fail_summary" <<'JSON'
{
  "ok": true,
  "state-count": 3,
  "distinct-state-count": 0,
  "time-budget": {"seconds": 10, "enforced": true, "timed-out": false},
  "invariant-results": [{"name": "SafetyInvariants", "status": "pass"}]
}
JSON

cat >"$timeout_summary" <<'JSON'
{
  "ok": false,
  "state-count": 2,
  "distinct-state-count": 2,
  "time-budget": {"seconds": 10, "enforced": true, "timed-out": true},
  "invariant-results": []
}
JSON

pass_json="$(bash "${ROOT}/scripts/measure-tla-coverage.sh" --summary "$pass_summary" "$fixture")"
jq -e '
  .status == "pass"
  and .records[0].state == "complete"
  and .records[0].cfg == "target/test-logs/spec-coverage/release-gate/synthetic-ring-buffer.coverage.cfg"
  and .records[0].state_space_estimate == 3
  and .records[0].coverage_pct == 100
' >/dev/null <<<"$pass_json"

warn_json="$(bash "${ROOT}/scripts/measure-tla-coverage.sh" --summary "$warn_summary" "$fixture")"
jq -e '
  .status == "warn"
  and .records[0].state == "below-threshold"
  and .records[0].coverage_pct < .records[0].threshold_pct
  and .records[0].coverage_pct >= .records[0].ci_fail_under_pct
' >/dev/null <<<"$warn_json"

set +e
fail_json="$(bash "${ROOT}/scripts/measure-tla-coverage.sh" --summary "$fail_summary" "$fixture")"
fail_rc=$?
set -e
[[ "$fail_rc" -eq 1 ]]
jq -e '
  .status == "fail"
  and .records[0].state == "under-ci-threshold"
' >/dev/null <<<"$fail_json"

set +e
timeout_json="$(bash "${ROOT}/scripts/measure-tla-coverage.sh" --summary "$timeout_summary" "$fixture")"
timeout_rc=$?
set -e
[[ "$timeout_rc" -eq 1 ]]
jq -e '
  .status == "fail"
  and .records[0].state == "space-explosion"
' >/dev/null <<<"$timeout_json"

stateright_json="$(
  bash "${ROOT}/scripts/measure-stateright-coverage.sh" \
    --summary "${ROOT}/docs/attestations/proofs/robot-work-atomicity.json" \
    "${ROOT}/tests/robot_work_atomicity_model/src/main.rs"
)"
jq -e '
  .status == "pass"
  and .records[0].model == "robot-work-atomicity"
  and .records[0].unique_state_count == 24997
  and .records[0].observed_max_depth == 8
' >/dev/null <<<"$stateright_json"

printf '%s\n' "$pass_json" >>"${LOG_DIR}/coverage-runs.jsonl"
printf '%s\n' "$warn_json" >>"${LOG_DIR}/coverage-runs.jsonl"
printf '%s\n' "$fail_json" >>"${LOG_DIR}/coverage-runs.jsonl"
printf '%s\n' "$timeout_json" >>"${LOG_DIR}/coverage-runs.jsonl"
printf '%s\n' "$stateright_json" >>"${LOG_DIR}/coverage-runs.jsonl"
