#!/usr/bin/env bash
# Validate mission metric JSON does not cite ignored runtime logs as retained proof.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ $# -gt 0 ]]; then
  FILES=("$@")
else
  FILES=(
    "docs/metrics/mission_chaos_evidence.json"
    "docs/metrics/mission_soak_chaos_evidence.json"
    "docs/metrics/mission_tx_rollout_readiness.json"
  )
fi

require_tool() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: required tool not found: $1" >&2
    exit 2
  fi
}

is_tracked_file() {
  local rel="$1"
  [[ -f "$ROOT_DIR/$rel" ]] || return 1
  git -C "$ROOT_DIR" ls-files --error-unmatch "$rel" >/dev/null 2>&1
}

require_tool jq
require_tool git

failures=0

record_failure() {
  printf 'ERROR: %s\n' "$*" >&2
  failures=$((failures + 1))
}

for file in "${FILES[@]}"; do
  path="$ROOT_DIR/$file"
  if [[ ! -f "$path" ]]; then
    record_failure "missing metric file: $file"
    continue
  fi
  if ! jq empty "$path" >/dev/null; then
    record_failure "invalid JSON: $file"
    continue
  fi

  while IFS=$'\t' read -r field value_type status artifact_path details; do
    [[ -n "$field" ]] || continue
    case "$value_type" in
      string)
        if [[ "$artifact_path" == tests/e2e/logs/* ]]; then
          record_failure "$file:$field cites ignored runtime log as retained proof: $artifact_path"
        elif ! is_tracked_file "$artifact_path"; then
          record_failure "$file:$field cites missing or untracked retained artifact: $artifact_path"
        fi
        ;;
      object)
        case "$status" in
          runtime_log_unretained|raw_log_unavailable)
            if [[ "$artifact_path" != tests/e2e/logs/* ]]; then
              record_failure "$file:$field status=$status must name original_runtime_path under tests/e2e/logs"
            fi
            if [[ -z "$details" || "$details" == "null" ]]; then
              record_failure "$file:$field status=$status must include storage_details"
            fi
            ;;
          external_retained)
            if [[ -z "$details" || "$details" == "null" ]]; then
              record_failure "$file:$field status=external_retained must include storage_details"
            fi
            ;;
          tracked_retained)
            if ! is_tracked_file "$artifact_path"; then
              record_failure "$file:$field status=tracked_retained cites missing or untracked path: $artifact_path"
            fi
            ;;
          *)
            record_failure "$file:$field has unsupported artifact status: ${status:-<missing>}"
            ;;
        esac
        ;;
      *)
        record_failure "$file:$field must be string or object, got $value_type"
        ;;
    esac
  done < <(
    jq -r '
      def interesting:
        {
          artifact_log: true,
          chaos_log: true,
          tx_matrix_log: true,
          tx_observability_log: true,
          resume_run_a_log: true,
          resume_run_b_log: true,
          mission_soak_jsonl: true,
          mission_chaos_jsonl: true
        };
      paths as $p
      | select(($p[-1] | type) == "string")
      | select(interesting[$p[-1]] == true)
      | getpath($p) as $v
      | [
          ($p | map(tostring) | join(".")),
          ($v | type),
          (if ($v | type) == "object" then ($v.status // "") else "" end),
          (if ($v | type) == "object" then ($v.original_runtime_path // $v.path // "") else $v end),
          (if ($v | type) == "object" then ($v.storage_details // $v.external_storage // "") else "" end)
        ]
      | @tsv
    ' "$path"
  )
done

if [[ "$failures" -ne 0 ]]; then
  exit 1
fi

echo "mission metric artifact references are explicit and resolvable"
