#!/usr/bin/env bash
# Estimate bounded state-space coverage for docs/specs TLA+ runs.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

usage() {
  cat <<'USAGE'
Usage: scripts/measure-tla-coverage.sh [options] docs/specs/<spec>.tla [...]

Options:
  --summary <path>        Read one existing TLC summary/attestation JSON (single spec only).
  --run-tlc               Run scripts/run-tlc.sh before measuring each spec.
  --timeout-secs <secs>   TLC time budget when --run-tlc is used (default: 30).
  --workers <value>       TLC worker count when --run-tlc is used (default: 1).
  --out-dir <path>        TLC output root when --run-tlc is used (default: target/tlc).
  -h, --help              Show this help.

Each spec must contain a comment block like:

  \* coverage-metric:
  \*   subsystem: robot-work
  \*   declared-invariants: SafetyInvariants
  \*   max-depth: 8
  \*   branching-factor: 6
  \*   threshold-pct: 0.002

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

number_json_or_null() {
  local value="${1:-}"
  if [[ -n "$value" ]]; then
    printf '%s' "$value"
  else
    printf 'null'
  fi
}

list_json() {
  local value="$1"
  printf '%s' "$value" | jq -R 'split(",") | map(gsub("^\\s+|\\s+$"; "")) | map(select(length > 0))'
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

summary_bool_field() {
  local summary="$1"
  local expression="$2"
  jq -r "$expression" "$summary"
}

record_status() {
  local coverage="$1"
  local threshold="$2"
  local fail_threshold="$3"
  local timed_out="$4"
  local summary_ok="$5"

  if [[ "$timed_out" == "true" ]]; then
    printf 'space-explosion fail\n'
  elif [[ "$summary_ok" == "false" ]]; then
    printf 'tlc-run-failed fail\n'
  elif awk -v c="$coverage" -v f="$fail_threshold" 'BEGIN { exit !(c < f) }'; then
    printf 'under-ci-threshold fail\n'
  elif awk -v c="$coverage" -v t="$threshold" 'BEGIN { exit !(c < t) }'; then
    printf 'below-threshold warn\n'
  else
    printf 'complete pass\n'
  fi
}

summary_for_spec() {
  local spec="$1"
  local summary_override="$2"
  local run_tlc="$3"
  local timeout_secs="$4"
  local workers="$5"
  local out_root="$6"

  if [[ -n "$summary_override" ]]; then
    abs_path "$summary_override"
    return
  fi

  local base
  base="$(basename "$spec" .tla)"
  local summary="${PROJECT_ROOT}/${out_root}/${base}/summary.json"

  if [[ "$run_tlc" -eq 1 ]]; then
    mkdir -p "${PROJECT_ROOT}/${out_root}/${base}"
    local run_stdout="${PROJECT_ROOT}/${out_root}/${base}/coverage-run.stdout.json"
    set +e
    "${PROJECT_ROOT}/scripts/run-tlc.sh" \
      --workers "$workers" \
      --timeout-secs "$timeout_secs" \
      --out-dir "$out_root/$base" \
      "$spec" >"$run_stdout"
    local rc=$?
    set -e
    if [[ ! -f "$summary" ]]; then
      fail "TLC run failed before writing ${out_root}/${base}/summary.json (exit ${rc})"
    fi
  elif [[ ! -f "$summary" ]]; then
    fail "$(rel_path "$summary") does not exist; run scripts/run-tlc.sh first, pass --summary, or pass --run-tlc"
  fi

  printf '%s' "$summary"
}

require_jq

summary_override=""
run_tlc=0
timeout_secs=30
workers=1
out_root="target/tlc"
specs=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --summary)
      summary_override="$2"
      shift 2
      ;;
    --run-tlc)
      run_tlc=1
      shift
      ;;
    --timeout-secs)
      timeout_secs="$2"
      shift 2
      ;;
    --workers)
      workers="$2"
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
      specs+=("$1")
      shift
      ;;
  esac
done

if [[ "${#specs[@]}" -eq 0 ]]; then
  usage >&2
  exit 2
fi
if [[ -n "$summary_override" && "${#specs[@]}" -ne 1 ]]; then
  fail "--summary can only be used with one spec"
fi
if [[ "$out_root" == /* ]]; then
  fail "--out-dir must be project-relative so generated TLC artifacts stay under the repo target directory"
fi

records=()
overall_status="pass"

for spec_input in "${specs[@]}"; do
  spec="$(abs_path "$spec_input")"
  [[ -f "$spec" ]] || fail "missing spec: $spec_input"

  subsystem="$(required_metric "$spec" "subsystem")"
  declared_invariants="$(required_metric "$spec" "declared-invariants")"
  max_depth="$(required_metric "$spec" "max-depth")"
  branching_factor="$(required_metric "$spec" "branching-factor")"
  threshold="$(required_metric "$spec" "threshold-pct")"
  fail_threshold="$(metric_value "$spec" "ci-fail-under-pct")"
  if [[ -z "$fail_threshold" ]]; then
    fail_threshold="$(half_threshold "$threshold")"
  fi

  [[ "$max_depth" =~ ^[0-9]+$ ]] || fail "$(rel_path "$spec") max-depth must be an integer"
  awk -v b="$branching_factor" 'BEGIN { exit !(b > 0) }' || fail "$(rel_path "$spec") branching-factor must be > 0"
  awk -v t="$threshold" 'BEGIN { exit !(t >= 0) }' || fail "$(rel_path "$spec") threshold-pct must be >= 0"
  awk -v t="$fail_threshold" 'BEGIN { exit !(t >= 0) }' || fail "$(rel_path "$spec") ci-fail-under-pct must be >= 0"

  summary="$(summary_for_spec "$spec" "$summary_override" "$run_tlc" "$timeout_secs" "$workers" "$out_root")"
  [[ -f "$summary" ]] || fail "missing summary: $summary"

  state_count="$(summary_field "$summary" '.["state-count"] // .state_count // .tlc_run.state_count')"
  distinct_state_count="$(summary_field "$summary" '.["distinct-state-count"] // .distinct_state_count // .tlc_run.distinct_state_count')"
  time_budget_seconds="$(summary_field "$summary" '.["time-budget"].seconds // .time_budget_seconds // .tlc_run.time_budget_seconds')"
  timed_out="$(summary_bool_field "$summary" 'if (.["time-budget"]["timed-out"]? != null) then .["time-budget"]["timed-out"] elif (.timed_out? != null) then .timed_out elif (.tlc_run.timed_out? != null) then .tlc_run.timed_out else false end')"
  elapsed_seconds="$(summary_field "$summary" '.elapsed_seconds // .tlc_run.elapsed_seconds // .tlc_run.summary_remote_duration_seconds')"
  summary_ok="$(summary_bool_field "$summary" 'if (.ok? != null) then .ok elif (.tlc_run.status? != null) then (.tlc_run.status == "pass") else true end')"
  invariant_results="$(jq -c '.["invariant-results"] // .invariant_results // .tlc_run.invariant_results // []' "$summary")"

  [[ -n "$state_count" ]] || fail "$(rel_path "$summary") missing state count"
  [[ -n "$distinct_state_count" ]] || fail "$(rel_path "$summary") missing distinct state count"

  estimate="$(state_space_estimate "$max_depth" "$branching_factor")"
  coverage="$(coverage_pct "$distinct_state_count" "$estimate")"
  read -r state status < <(record_status "$coverage" "$threshold" "$fail_threshold" "$timed_out" "$summary_ok")

  if [[ "$status" == "fail" ]]; then
    overall_status="fail"
  elif [[ "$status" == "warn" && "$overall_status" == "pass" ]]; then
    overall_status="warn"
  fi

  declared_json="$(list_json "$declared_invariants")"
  time_budget_json="$(number_json_or_null "$time_budget_seconds")"
  elapsed_json="$(number_json_or_null "$elapsed_seconds")"

  record="$(jq -n \
    --arg subsystem "$subsystem" \
    --arg spec "$(rel_path "$spec")" \
    --arg summary "$(rel_path "$summary")" \
    --arg state "$state" \
    --arg status "$status" \
    --argjson declared_invariants "$declared_json" \
    --argjson max_depth "$max_depth" \
    --argjson branching_factor "$branching_factor" \
    --argjson state_count "$state_count" \
    --argjson distinct_state_count "$distinct_state_count" \
    --argjson estimate "$estimate" \
    --argjson coverage_pct "$coverage" \
    --argjson threshold_pct "$threshold" \
    --argjson ci_fail_under_pct "$fail_threshold" \
    --argjson time_budget_seconds "$time_budget_json" \
    --argjson elapsed_seconds "$elapsed_json" \
    --argjson timed_out "$timed_out" \
    --argjson invariant_results "$invariant_results" \
    '{
      subsystem: $subsystem,
      spec: $spec,
      declared_invariants: $declared_invariants,
      max_depth: $max_depth,
      branching_factor: $branching_factor,
      state_count: $state_count,
      distinct_state_count: $distinct_state_count,
      state_space_estimate: $estimate,
      coverage_pct: $coverage_pct,
      threshold_pct: $threshold_pct,
      ci_fail_under_pct: $ci_fail_under_pct,
      time_budget_seconds: $time_budget_seconds,
      elapsed_seconds: $elapsed_seconds,
      timed_out: $timed_out,
      state: $state,
      status: $status,
      summary: $summary,
      invariant_results: $invariant_results
    }')"
  records+=("$record")
done

records_json="$(printf '%s\n' "${records[@]}" | jq -s '.')"
generated_at="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

jq -n \
  --arg generated_at "$generated_at" \
  --arg status "$overall_status" \
  --argjson records "$records_json" \
  '{
    schema_version: "1.0.0",
    kind: "tla-state-space-coverage",
    generated_at: $generated_at,
    proof_category: [6, 2],
    status: $status,
    records: $records
  }'

if [[ "$overall_status" == "fail" ]]; then
  exit 1
fi
