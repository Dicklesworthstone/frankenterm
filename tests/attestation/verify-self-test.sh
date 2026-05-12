#!/usr/bin/env bash
# Run the attestation verifier against known-good and known-bad bundle fixtures.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
EXPECTED_DIR="${FT_ATTESTATION_SELF_TEST_EXPECTED:-${ROOT_DIR}/tests/attestation_verify_self_test/expected}"
VERIFY_SCRIPT="${ROOT_DIR}/scripts/attestation-verify.sh"
RUN_ID="${FT_ATTESTATION_SELF_TEST_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
LOG_ROOT="${FT_ATTESTATION_SELF_TEST_LOG_DIR:-${ROOT_DIR}/target/test-logs/attestation-verify-self-test}"

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "missing required command: $cmd" >&2
    exit 2
  fi
}

json_log() {
  local fixture_name="$1"
  local row="$2"
  local fixture_log_dir="${LOG_ROOT}/${fixture_name}"
  mkdir -p "$fixture_log_dir"
  printf '%s\n' "$row" >> "${fixture_log_dir}/${RUN_ID}.jsonl"
}

require_cmd jq
[[ -d "$EXPECTED_DIR" ]] || { echo "expected fixture directory missing: $EXPECTED_DIR" >&2; exit 2; }
[[ -f "$VERIFY_SCRIPT" ]] || { echo "verifier script missing: $VERIFY_SCRIPT" >&2; exit 2; }

total=0
passed=0
failed=0

for expected_file in "${EXPECTED_DIR}"/*.json; do
  [[ -e "$expected_file" ]] || { echo "no expected fixture files under $EXPECTED_DIR" >&2; exit 2; }
  total=$((total + 1))
  fixture_rel="$(jq -r '.fixture' "$expected_file")"
  fixture="${ROOT_DIR}/${fixture_rel}"
  fixture_name="$(basename "$fixture" .json)"
  expected_ok="$(jq -r '.expected_ok' "$expected_file")"
  expected_regex="$(jq -r '.expected_error_regex // ""' "$expected_file")"
  flags=()
  while IFS= read -r flag; do
    [[ -n "$flag" ]] && flags+=("$flag")
  done < <(jq -r '.flags[]?' "$expected_file")

  if [[ ! -f "$fixture" ]]; then
    row="$(jq -cn \
      --arg run_id "$RUN_ID" \
      --arg fixture "$fixture_rel" \
      '{ts: now | todateiso8601, run_id: $run_id, fixture: $fixture, outcome: "failed", reason: "fixture_missing"}')"
    json_log "$fixture_name" "$row"
    echo "FAIL ${fixture_name}: fixture missing: ${fixture_rel}" >&2
    failed=$((failed + 1))
    continue
  fi

  set +e
  output="$("$VERIFY_SCRIPT" "$fixture" "${flags[@]}" --json 2>&1)"
  rc=$?
  set -e

  actual_ok="false"
  errors_text="$output"
  if parsed_json="$(jq '.' <<<"$output" 2>/dev/null)"; then
    actual_ok="$(jq -r '.ok' <<<"$parsed_json")"
    errors_text="$(jq -r '.errors[]?' <<<"$parsed_json")"
  fi

  outcome="passed"
  reason="matched_expected_verdict"
  if [[ "$actual_ok" != "$expected_ok" ]]; then
    outcome="failed"
    reason="verdict_mismatch"
  elif [[ "$expected_ok" == "false" && -n "$expected_regex" ]]; then
    if ! grep -Eq "$expected_regex" <<<"$errors_text"; then
      outcome="failed"
      reason="expected_error_regex_not_matched"
    fi
  fi

  row="$(jq -cn \
    --arg run_id "$RUN_ID" \
    --arg fixture "$fixture_rel" \
    --arg expected_file "${expected_file#${ROOT_DIR}/}" \
    --arg expected_ok "$expected_ok" \
    --arg actual_ok "$actual_ok" \
    --arg rc "$rc" \
    --arg outcome "$outcome" \
    --arg reason "$reason" \
    --arg regex "$expected_regex" \
    --arg raw_output "$output" \
    '{
      ts: now | todateiso8601,
      run_id: $run_id,
      fixture: $fixture,
      expected_file: $expected_file,
      expected_ok: ($expected_ok == "true"),
      actual_ok: ($actual_ok == "true"),
      exit_code: ($rc | tonumber),
      outcome: $outcome,
      reason: $reason,
      expected_error_regex: $regex,
      raw_output: $raw_output
    }')"
  json_log "$fixture_name" "$row"

  if [[ "$outcome" == "passed" ]]; then
    passed=$((passed + 1))
    echo "PASS ${fixture_name}"
  else
    failed=$((failed + 1))
    echo "FAIL ${fixture_name}: ${reason}" >&2
    echo "$errors_text" >&2
  fi
done

summary="$(jq -cn \
  --arg run_id "$RUN_ID" \
  --arg total "$total" \
  --arg passed "$passed" \
  --arg failed "$failed" \
  --arg log_root "$LOG_ROOT" \
  '{
    schema_version: "attestation.verify.self_test.summary.v1",
    run_id: $run_id,
    total: ($total | tonumber),
    passed: ($passed | tonumber),
    failed: ($failed | tonumber),
    log_root: $log_root
  }')"
echo "$summary"

if [[ "$failed" -ne 0 ]]; then
  exit 1
fi
