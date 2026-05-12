#!/usr/bin/env bash
# Advisory full reality-check trigger detector.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

OUTPUT_MODE="text"
STRICT=0
AS_OF="$(date -u +%Y-%m-%d)"
OPEN_THRESHOLD=50
CONTRACT_DIFF_THRESHOLD=50
CLAIM_GROWTH_THRESHOLD=3
CALENDAR_DAYS=90

usage() {
  cat <<'USAGE'
Usage: scripts/check-reality-check-due.sh [options]

Options:
  --json                         Emit machine-readable JSON.
  --strict                       Exit 1 when any trigger fires.
  --as-of YYYY-MM-DD             Override today's date for reproducible checks.
  --open-threshold N             Open-bead trigger threshold (default: 50).
  --contract-diff-threshold N    Contract-doc churn threshold (default: 50).
  --claim-growth-threshold N     README headline-claim growth threshold (default: 3).
  --calendar-days N              Calendar trigger threshold (default: 90).
  -h, --help                     Show this help.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --json) OUTPUT_MODE="json"; shift ;;
    --strict) STRICT=1; shift ;;
    --as-of) AS_OF="$2"; shift 2 ;;
    --open-threshold) OPEN_THRESHOLD="$2"; shift 2 ;;
    --contract-diff-threshold) CONTRACT_DIFF_THRESHOLD="$2"; shift 2 ;;
    --claim-growth-threshold) CLAIM_GROWTH_THRESHOLD="$2"; shift 2 ;;
    --calendar-days) CALENDAR_DAYS="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

command -v git >/dev/null 2>&1 || { echo "error: git required" >&2; exit 2; }
command -v jq >/dev/null 2>&1 || { echo "error: jq required" >&2; exit 2; }

cd "$REPO_ROOT"

epoch_for_date() {
  local d="$1"
  if date -u -d "$d" +%s >/dev/null 2>&1; then
    date -u -d "$d" +%s
  elif date -u -j -f "%Y-%m-%d" "$d" +%s >/dev/null 2>&1; then
    date -u -j -f "%Y-%m-%d" "$d" +%s
  else
    return 1
  fi
}

extract_plan_date() {
  local path="$1"
  local base
  base="$(basename "$path")"
  if [[ "$base" =~ reality-check-bridge-plan-([0-9]{4}-[0-9]{2}-[0-9]{2})\.md ]]; then
    printf '%s\n' "${BASH_REMATCH[1]}"
    return
  fi
  grep -Eo 'invocation [0-9]{4}-[0-9]{2}-[0-9]{2}|[0-9]{4}-[0-9]{2}-[0-9]{2}' "$path" \
    | head -n1 \
    | grep -Eo '[0-9]{4}-[0-9]{2}-[0-9]{2}' \
    || true
}

latest_plan_date() {
  local latest=""
  local path
  while IFS= read -r path; do
    [[ -n "$path" ]] || continue
    local d
    d="$(extract_plan_date "$path")"
    if [[ -n "$d" && ( -z "$latest" || "$d" > "$latest" ) ]]; then
      latest="$d"
    fi
  done < <(
    git ls-files -- \
      docs/reality-check-bridge-plan.md \
      'docs/reality-check-bridge-plan-*.md' \
      | sort
  )
  printf '%s\n' "$latest"
}

latest_tracked_plan_commit() {
  git log -1 --format=%H -- \
    docs/reality-check-bridge-plan.md \
    'docs/reality-check-bridge-plan-*.md' \
    2>/dev/null || true
}

cargo_minor_version_from_stdin() {
  awk '
    /^version[[:space:]]*=/ {
      gsub(/"/, "", $3)
      split($3, parts, ".")
      if (parts[1] != "" && parts[2] != "") {
        print parts[1] "." parts[2]
        exit
      }
    }
  '
}

readme_headline_claim_count_from_stdin() {
  awk '
    /^## Quickstart/ { capture = 0 }
    /^# ft/ { capture = 1 }
    capture && /([0-9]+[[:space:]]*(ms|MB|GB|panes|agents|tests|crates|lines)|[0-9][0-9,]*\+|[0-9]+x|< ?[0-9]+)/ {
      count++
    }
    END { print count + 0 }
  '
}

open_bead_count() {
  local out count
  if command -v bv >/dev/null 2>&1; then
    out="$(bv --robot-triage 2>/dev/null || true)"
    count="$(jq -r '.triage.project_health.counts.open // .triage.quick_ref.open_count // empty' <<<"$out" 2>/dev/null || true)"
    if [[ "$count" =~ ^[0-9]+$ ]]; then
      printf '%s\n' "$count"
      return
    fi
  fi

  if command -v br >/dev/null 2>&1; then
    out="$(br list --status open --json 2>/dev/null || true)"
    count="$(jq -r 'if type=="array" then length else (.issues // []) | length end' <<<"$out" 2>/dev/null || true)"
    if [[ "$count" =~ ^[0-9]+$ ]]; then
      printf '%s\n' "$count"
      return
    fi
  fi

  printf '0\n'
}

contract_doc_delta_since() {
  local commit="$1"
  if [[ -z "$commit" ]]; then
    printf '0\n'
    return
  fi
  git diff --numstat "${commit}..HEAD" -- \
    ':(glob)docs/**/*contract*.md' \
    ':(glob)docs/robot-contracts/*.md' \
    2>/dev/null \
    | awk '{ added += $1; deleted += $2 } END { print added + deleted + 0 }'
}

latest_date="$(latest_plan_date)"
latest_commit="$(latest_tracked_plan_commit)"

if [[ -z "$latest_date" ]]; then
  echo "error: no reality-check bridge plan found" >&2
  exit 2
fi

as_of_epoch="$(epoch_for_date "$AS_OF")"
latest_epoch="$(epoch_for_date "$latest_date")"
days_since=$(( (as_of_epoch - latest_epoch) / 86400 ))
if [[ "$days_since" -lt 0 ]]; then
  days_since=0
fi

current_minor="$(cargo_minor_version_from_stdin < Cargo.toml)"
baseline_minor="$current_minor"
baseline_claim_count="$(readme_headline_claim_count_from_stdin < README.md)"
if [[ -n "$latest_commit" ]]; then
  baseline_minor="$(git show "${latest_commit}:Cargo.toml" 2>/dev/null | cargo_minor_version_from_stdin || printf '%s\n' "$current_minor")"
  baseline_claim_count="$(git show "${latest_commit}:README.md" 2>/dev/null | readme_headline_claim_count_from_stdin || printf '%s\n' "$baseline_claim_count")"
fi

current_claim_count="$(readme_headline_claim_count_from_stdin < README.md)"
claim_growth=$(( current_claim_count - baseline_claim_count ))
if [[ "$claim_growth" -lt 0 ]]; then
  claim_growth=0
fi

open_count="$(open_bead_count)"
contract_delta="$(contract_doc_delta_since "$latest_commit")"

calendar_due=false
minor_due=false
open_due=false
contract_due=false
claims_due=false

[[ "$days_since" -ge "$CALENDAR_DAYS" ]] && calendar_due=true
[[ "$current_minor" != "$baseline_minor" ]] && minor_due=true
[[ "$open_count" -ge "$OPEN_THRESHOLD" ]] && open_due=true
[[ "$contract_delta" -ge "$CONTRACT_DIFF_THRESHOLD" ]] && contract_due=true
[[ "$claim_growth" -ge "$CLAIM_GROWTH_THRESHOLD" ]] && claims_due=true

due=false
if [[ "$calendar_due" == true || "$minor_due" == true || "$open_due" == true || "$contract_due" == true || "$claims_due" == true ]]; then
  due=true
fi

if [[ "$OUTPUT_MODE" == "json" ]]; then
  jq -n \
    --arg as_of "$AS_OF" \
    --arg latest_date "$latest_date" \
    --arg latest_commit "$latest_commit" \
    --arg current_minor "$current_minor" \
    --arg baseline_minor "$baseline_minor" \
    --argjson days_since "$days_since" \
    --argjson calendar_days "$CALENDAR_DAYS" \
    --argjson open_count "$open_count" \
    --argjson open_threshold "$OPEN_THRESHOLD" \
    --argjson contract_delta "$contract_delta" \
    --argjson contract_threshold "$CONTRACT_DIFF_THRESHOLD" \
    --argjson current_claim_count "$current_claim_count" \
    --argjson baseline_claim_count "$baseline_claim_count" \
    --argjson claim_growth "$claim_growth" \
    --argjson claim_growth_threshold "$CLAIM_GROWTH_THRESHOLD" \
    --argjson due "$due" \
    --argjson calendar_due "$calendar_due" \
    --argjson minor_due "$minor_due" \
    --argjson open_due "$open_due" \
    --argjson contract_due "$contract_due" \
    --argjson claims_due "$claims_due" \
    '{
      as_of: $as_of,
      latest_reality_check_date: $latest_date,
      latest_tracked_plan_commit: $latest_commit,
      due: $due,
      signals: {
        calendar: {days_since: $days_since, threshold_days: $calendar_days, triggered: $calendar_due},
        minor_version: {current: $current_minor, baseline: $baseline_minor, triggered: $minor_due},
        open_beads: {count: $open_count, threshold: $open_threshold, triggered: $open_due},
        contract_doc_churn: {changed_lines: $contract_delta, threshold: $contract_threshold, triggered: $contract_due},
        readme_headline_claims: {current_count: $current_claim_count, baseline_count: $baseline_claim_count, growth: $claim_growth, threshold: $claim_growth_threshold, triggered: $claims_due}
      }
    }'
else
  printf 'Reality-check due check\n'
  printf '  as_of: %s\n' "$AS_OF"
  printf '  latest_plan_date: %s\n' "$latest_date"
  printf '  latest_tracked_plan_commit: %s\n' "${latest_commit:-none}"
  printf '  days_since_latest: %s / %s\n' "$days_since" "$CALENDAR_DAYS"
  printf '  minor_version: current=%s baseline=%s\n' "$current_minor" "$baseline_minor"
  printf '  open_beads: %s / %s\n' "$open_count" "$OPEN_THRESHOLD"
  printf '  contract_doc_changed_lines: %s / %s\n' "$contract_delta" "$CONTRACT_DIFF_THRESHOLD"
  printf '  readme_headline_claim_growth: %s / %s (current=%s baseline=%s)\n' \
    "$claim_growth" "$CLAIM_GROWTH_THRESHOLD" "$current_claim_count" "$baseline_claim_count"

  if [[ "$due" == true ]]; then
    echo "warning: full reality-check is due"
    [[ "$calendar_due" == true ]] && echo "warning: trigger=calendar"
    [[ "$minor_due" == true ]] && echo "warning: trigger=minor-version"
    [[ "$open_due" == true ]] && echo "warning: trigger=open-beads"
    [[ "$contract_due" == true ]] && echo "warning: trigger=contract-doc-churn"
    [[ "$claims_due" == true ]] && echo "warning: trigger=readme-headline-claims"
  else
    echo "ok: no full reality-check trigger fired"
  fi
fi

if [[ "$due" == true && "$STRICT" -eq 1 ]]; then
  exit 1
fi
