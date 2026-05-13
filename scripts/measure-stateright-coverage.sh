#!/usr/bin/env bash
# Estimate bounded state-space coverage for Stateright model summaries.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

usage() {
  cat <<'USAGE'
Usage: scripts/measure-stateright-coverage.sh [options] <model-source.rs>

Options:
  --summary <path>        Read an existing Stateright summary/attestation JSON.
  --command <command>     Run a command that prints one JSON summary object.
  --out-dir <path>        Output root for command summaries (default: target/stateright-coverage).
  -h, --help              Show this help.

The model source must contain a comment block like:

  // coverage-metric:
  //   model: robot-work-atomicity
  //   declared-invariants: single-holder, durable-completion
  //   max-depth: 14
  //   branching-factor: 4
  //   threshold-pct: 0.002

The threshold is a warning threshold in percentage points. CI failure is
threshold / 2 unless ci-fail-under-pct is explicitly provided.
USAGE
}

fail() {
  printf 'error: %s\n' "$1" >&2
  exit 2
}

require_jq() {
  command -v jq >/dev/null 2>&1 || fail "jq is required"
}

abs_path() {
  local path="$1"
  if [[ "$path" != /* ]]; then
    path="${PROJECT_ROOT}/${path}"
  fi
  printf '%s/%s' "$(cd "$(dirname "$path")" && pwd)" "$(basename "$path")"
}

rel_path() {
  local path="$1"
  if [[ "$path" == "${PROJECT_ROOT}/"* ]]; then
    printf '%s' "${path#"${PROJECT_ROOT}/"}"
  else
    printf '%s' "$path"
  fi
}

metric_value() {
  local file="$1"
  local key="$2"
  awk -v want="$key" '
    /coverage-metric:/ { in_block = 1; next }
    in_block {
      line = $0
      if (line ~ /^[[:space:]]*(\\\*|\/\/)/) {
        sub(/^[[:space:]]*(\\\*|\/\/)[[:space:]]*/, "", line)
        if (line ~ /^[A-Za-z0-9_-]+:[[:space:]]*/) {
          pos = index(line, ":")
          k = substr(line, 1, pos - 1)
          v = substr(line, pos + 1)
          gsub(/^[ \t]+|[ \t]+$/, "", k)
          gsub(/^[ \t]+|[ \t]+$/, "", v)
          if (k == want) {
            print v
            exit
          }
        }
        next
      }
      if (line ~ /^[[:space:]]*$/) {
        next
      }
      exit
    }
  ' "$file"
}

required_metric() {
  local file="$1"
  local key="$2"
  local value
  value="$(metric_value "$file" "$key")"
  [[ -n "$value" ]] || fail "$(rel_path "$file") missing coverage-metric key: $key"
  printf '%s' "$value"
}

list_json() {
  local value="$1"
  printf '%s' "$value" | jq -R 'split(",") | map(gsub("^\\s+|\\s+$"; "")) | map(select(length > 0))'
}

number_json_or_null() {
  local value="${1:-}"
  if [[ -n "$value" ]]; then
    printf '%s' "$value"
  else
    printf 'null'
  fi
}

state_space_estimate() {
  local depth="$1"
  local branching="$2"
  awk -v depth="$depth" -v branching="$branching" '
    BEGIN {
      estimate = 0
      power = 1
      for (i = 0; i <= depth; i++) {
        estimate += power
        power *= branching
      }
      printf "%.0f", estimate
    }
  '
}

coverage_pct() {
  local distinct="$1"
  local estimate="$2"
  awk -v distinct="$distinct" -v estimate="$estimate" '
    BEGIN {
      if (estimate <= 0) {
        printf "0.000000"
      } else {
        raw = (distinct / estimate) * 100
        if (raw > 100) {
          raw = 100
        }
        printf "%.6f", raw
      }
    }
  '
}

half_threshold() {
  local threshold="$1"
  awk -v threshold="$threshold" 'BEGIN { printf "%.6f", threshold / 2 }'
}

summary_field() {
  local summary="$1"
  local expression="$2"
  jq -r "$expression // empty" "$summary"
}

record_status() {
  local coverage="$1"
  local threshold="$2"
  local fail_threshold="$3"
  local summary_status="$4"

  if [[ -n "$summary_status" && "$summary_status" != "pass" ]]; then
    printf 'model-run-failed fail\n'
  elif awk -v c="$coverage" -v f="$fail_threshold" 'BEGIN { exit !(c < f) }'; then
    printf 'under-ci-threshold fail\n'
  elif awk -v c="$coverage" -v t="$threshold" 'BEGIN { exit !(c < t) }'; then
    printf 'below-threshold warn\n'
  else
    printf 'complete pass\n'
  fi
}

summary_from_command() {
  local model="$1"
  local command="$2"
  local out_root="$3"
  local model_name
  model_name="$(basename "$model" .rs)"
  local out_dir="${PROJECT_ROOT}/${out_root}/${model_name}"
  local raw_stdout="${out_dir}/summary.stdout.log"
  local summary="${out_dir}/summary.json"

  mkdir -p "$out_dir"
  set +e
  bash -lc "$command" >"$raw_stdout"
  local rc=$?
  set -e
  tail -n 1 "$raw_stdout" >"$summary"
  if ! jq empty "$summary" >/dev/null 2>&1; then
    fail "command did not emit a JSON summary on its final stdout line (exit ${rc})"
  fi
  printf '%s' "$summary"
}

require_jq

summary_override=""
run_command=""
out_root="target/stateright-coverage"
model_input=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --summary)
      summary_override="$2"
      shift 2
      ;;
    --command)
      run_command="$2"
      shift 2
      ;;
    --out-dir)
      out_root="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    -*)
      fail "unknown option: $1"
      ;;
    *)
      if [[ -n "$model_input" ]]; then
        fail "multiple model source paths supplied"
      fi
      model_input="$1"
      shift
      ;;
  esac
done

[[ -n "$model_input" ]] || fail "missing model source"
[[ -n "$summary_override" || -n "$run_command" ]] || fail "pass --summary or --command"
if [[ -n "$summary_override" && -n "$run_command" ]]; then
  fail "use either --summary or --command, not both"
fi
if [[ "$out_root" == /* ]]; then
  fail "--out-dir must be project-relative so generated summaries stay under the repo target directory"
fi

model="$(abs_path "$model_input")"
[[ -f "$model" ]] || fail "missing model source: $model_input"

model_name="$(required_metric "$model" "model")"
declared_invariants="$(required_metric "$model" "declared-invariants")"
max_depth="$(required_metric "$model" "max-depth")"
branching_factor="$(required_metric "$model" "branching-factor")"
threshold="$(required_metric "$model" "threshold-pct")"
fail_threshold="$(metric_value "$model" "ci-fail-under-pct")"
if [[ -z "$fail_threshold" ]]; then
  fail_threshold="$(half_threshold "$threshold")"
fi

[[ "$max_depth" =~ ^[0-9]+$ ]] || fail "$(rel_path "$model") max-depth must be an integer"
awk -v b="$branching_factor" 'BEGIN { exit !(b > 0) }' || fail "$(rel_path "$model") branching-factor must be > 0"
awk -v t="$threshold" 'BEGIN { exit !(t >= 0) }' || fail "$(rel_path "$model") threshold-pct must be >= 0"
awk -v t="$fail_threshold" 'BEGIN { exit !(t >= 0) }' || fail "$(rel_path "$model") ci-fail-under-pct must be >= 0"

if [[ -n "$summary_override" ]]; then
  summary="$(abs_path "$summary_override")"
else
  summary="$(summary_from_command "$model" "$run_command" "$out_root")"
fi
[[ -f "$summary" ]] || fail "missing summary: $summary"

state_count="$(summary_field "$summary" '.state_count // .stateright_run.summary_stdout.state_count')"
unique_state_count="$(summary_field "$summary" '.unique_state_count // .stateright_run.summary_stdout.unique_state_count')"
observed_max_depth="$(summary_field "$summary" '.max_depth // .stateright_run.summary_stdout.max_depth')"
summary_status="$(summary_field "$summary" '.status // .stateright_run.status')"
elapsed_seconds="$(summary_field "$summary" '.elapsed_seconds // .stateright_run.summary_remote_duration_seconds')"

[[ -n "$state_count" ]] || fail "$(rel_path "$summary") missing state_count"
[[ -n "$unique_state_count" ]] || fail "$(rel_path "$summary") missing unique_state_count"

estimate="$(state_space_estimate "$max_depth" "$branching_factor")"
coverage="$(coverage_pct "$unique_state_count" "$estimate")"
read -r state status < <(record_status "$coverage" "$threshold" "$fail_threshold" "$summary_status")

declared_json="$(list_json "$declared_invariants")"
elapsed_json="$(number_json_or_null "$elapsed_seconds")"
observed_depth_json="$(number_json_or_null "$observed_max_depth")"
generated_at="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

jq -n \
  --arg generated_at "$generated_at" \
  --arg model "$model_name" \
  --arg source "$(rel_path "$model")" \
  --arg summary "$(rel_path "$summary")" \
  --arg state "$state" \
  --arg status "$status" \
  --argjson declared_invariants "$declared_json" \
  --argjson max_depth "$max_depth" \
  --argjson branching_factor "$branching_factor" \
  --argjson state_count "$state_count" \
  --argjson unique_state_count "$unique_state_count" \
  --argjson observed_max_depth "$observed_depth_json" \
  --argjson estimate "$estimate" \
  --argjson coverage_pct "$coverage" \
  --argjson threshold_pct "$threshold" \
  --argjson ci_fail_under_pct "$fail_threshold" \
  --argjson elapsed_seconds "$elapsed_json" \
  '{
    schema_version: "1.0.0",
    kind: "stateright-state-space-coverage",
    generated_at: $generated_at,
    proof_category: [6, 2],
    status: $status,
    records: [
      {
        model: $model,
        source: $source,
        declared_invariants: $declared_invariants,
        declared_max_depth: $max_depth,
        observed_max_depth: $observed_max_depth,
        branching_factor: $branching_factor,
        state_count: $state_count,
        unique_state_count: $unique_state_count,
        state_space_estimate: $estimate,
        coverage_pct: $coverage_pct,
        threshold_pct: $threshold_pct,
        ci_fail_under_pct: $ci_fail_under_pct,
        elapsed_seconds: $elapsed_seconds,
        state: $state,
        status: $status,
        summary: $summary
      }
    ]
  }'

if [[ "$status" == "fail" ]]; then
  exit 1
fi
