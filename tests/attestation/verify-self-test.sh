#!/usr/bin/env bash
# Run the attestation verifier against known-good and known-bad bundle fixtures.
set -euo pipefail
umask 077

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
EXPECTED_DIR="${FT_ATTESTATION_SELF_TEST_EXPECTED:-}"
VERIFY_SCRIPT="${ROOT_DIR}/scripts/attestation-verify.sh"
RUN_ID="${FT_ATTESTATION_SELF_TEST_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
LOG_ROOT="${FT_ATTESTATION_SELF_TEST_LOG_DIR:-${ROOT_DIR}/target/test-logs/attestation-verify-self-test}"
unset FT_ATTESTATION_RELEASE_POLICY

resolve_repo_path() {
  local path="$1" part
  local -a parts=()
  [[ -n "$path" && "$path" != /* ]] || return 1
  IFS=/ read -r -a parts <<< "$path"
  for part in "${parts[@]}"; do
    [[ -n "$part" && "$part" != . && "$part" != .. ]] || return 1
  done
  printf '%s/%s\n' "$ROOT_DIR" "$path"
}

verdict_agrees_with_status() {
  local actual_ok="$1" rc="$2"
  [[ "$actual_ok" == true && "$rc" == 0 ]] || [[ "$actual_ok" == false && "$rc" != 0 ]]
}

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
[[ "$RUN_ID" =~ ^[A-Za-z0-9._-]+$ ]] || { echo "invalid self-test run id" >&2; exit 2; }
if [[ -z "$EXPECTED_DIR" ]]; then
  corpus_dir="${FT_ATTESTATION_SELF_TEST_DIR:-$ROOT_DIR/target/test-artifacts/attestation-verifier-$RUN_ID}"
  FT_ATTESTATION_SELF_TEST_DIR="$corpus_dir" bash "$ROOT_DIR/scripts/rotate-attestation-self-test.sh"
  [[ "$corpus_dir" == /* ]] || corpus_dir="$ROOT_DIR/$corpus_dir"
  EXPECTED_DIR="$corpus_dir/expected"
fi
[[ -d "$EXPECTED_DIR" ]] || { echo "expected fixture directory missing: $EXPECTED_DIR" >&2; exit 2; }
[[ -f "$VERIFY_SCRIPT" ]] || { echo "verifier script missing: $VERIFY_SCRIPT" >&2; exit 2; }

total=0
passed=0
failed=0

for expected_file in "${EXPECTED_DIR}"/*.json; do
  [[ -e "$expected_file" ]] || { echo "no expected fixture files under $EXPECTED_DIR" >&2; exit 2; }
  total=$((total + 1))
  fixture_rel="$(jq -r '.fixture' "$expected_file")"
  fixture="$(resolve_repo_path "$fixture_rel")" || { echo "unsafe expected fixture path" >&2; exit 2; }
  manifest="$(resolve_repo_path "$(jq -er '.manifest' "$expected_file")")" || { echo "expected record requires a repo-relative fixture manifest" >&2; exit 2; }
  verifier_tools="$(resolve_repo_path "$(jq -er '.tools' "$expected_file")")" || { echo "expected record requires an offline tools directory" >&2; exit 2; }
  retractions="$(resolve_repo_path "$(jq -er '.retractions' "$expected_file")")" || { echo "expected record requires a private retractions directory" >&2; exit 2; }
  [[ -f "$manifest" && -d "$verifier_tools" && -d "$retractions" ]] || { echo "fixture environment missing" >&2; exit 2; }
  if PATH="$verifier_tools" command -v cosign >/dev/null 2>&1; then
    echo "fixture tool path must exclude network-capable cosign" >&2
    exit 2
  fi
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

  fixture_log_dir="${LOG_ROOT}/${fixture_name}"
  mkdir -p "$fixture_log_dir"
  stdout_path="$(mktemp "$fixture_log_dir/$RUN_ID.stdout.XXXXXX")"
  stderr_path="$(mktemp "$fixture_log_dir/$RUN_ID.stderr.XXXXXX")"
  rc=0
  FT_ATTESTATION_MANIFEST="$manifest" FT_ATTESTATION_RETRACTIONS_ROOT="$retractions" \
    PATH="$verifier_tools" "$VERIFY_SCRIPT" "$fixture" "${flags[@]}" --json \
    > "$stdout_path" 2> "$stderr_path" || rc=$?
  output="$(cat "$stdout_path")"
  stderr_output="$(cat "$stderr_path")"
  [[ -z "$stderr_output" ]] || printf '%s\n' "$stderr_output" >&2

  actual_ok="false"
  errors_text="$output"
  valid_json=false
  if parsed_json="$(jq -ce 'select(type == "object" and (.ok | type == "boolean") and (.errors | type == "array"))' "$stdout_path")"; then
    valid_json=true
    actual_ok="$(jq -r '.ok' <<<"$parsed_json")"
    errors_text="$(jq -r '.errors[]?' <<<"$parsed_json")"
  fi

  outcome="passed"
  reason="matched_expected_verdict"
  if [[ "$valid_json" != true ]]; then
    outcome="failed"
    reason="invalid_verifier_json"
  elif ! verdict_agrees_with_status "$actual_ok" "$rc"; then
    outcome="failed"
    reason="verdict_exit_status_disagreement"
  elif [[ "$actual_ok" != "$expected_ok" ]]; then
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
    --arg expected_file "${expected_file#"${ROOT_DIR}"/}" \
    --arg expected_ok "$expected_ok" \
    --arg actual_ok "$actual_ok" \
    --arg rc "$rc" \
    --arg outcome "$outcome" \
    --arg reason "$reason" \
    --arg regex "$expected_regex" \
    --arg raw_output "$output" \
    --arg raw_stderr "$stderr_output" \
    --arg stdout_path "$stdout_path" \
    --arg stderr_path "$stderr_path" \
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
      fixture_only: true,
      raw_output: $raw_output,
      raw_stderr: $raw_stderr,
      stdout_path: $stdout_path,
      stderr_path: $stderr_path
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
    fixture_only: true,
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
