#!/usr/bin/env bash
# scripts/dogfood-status.sh — is the observe loop actually running anywhere on
# this host?
#
# Why: on 2026-09-01 the only ft.db on the maintainer's machine had last
# captured on 2026-02-14 while FrankenTerm.app ran daily. A product whose
# author does not run its core loop cannot make honest first-run claims, so
# this is a release-checklist item (ft-xxfwy.11): the newest capture on the
# release host must be less than 24 h old, or the release notes say why not.
#
# Usage:
#   scripts/dogfood-status.sh                 # human table; exit 1 if newest capture > MAX_AGE_HOURS
#   scripts/dogfood-status.sh --json          # machine-readable
#   scripts/dogfood-status.sh --root DIR ...  # extra directories to scan (default: $HOME, $FT_WORKSPACE, $PWD)
#   MAX_AGE_HOURS=48 scripts/dogfood-status.sh
#
# Read-only: opens every database with sqlite3 in read-only mode and never
# touches locks. Requires sqlite3 and jq.
set -u
JSON=0
declare -a ROOTS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --json) JSON=1 ;;
    --root) shift; ROOTS+=("${1:?--root needs a directory}") ;;
    -h|--help) sed -n '2,19p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done
command -v sqlite3 >/dev/null 2>&1 || { echo "sqlite3 is required" >&2; exit 2; }
command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 2; }
MAX_AGE_HOURS="${MAX_AGE_HOURS:-24}"
[[ ${#ROOTS[@]} -gt 0 ]] || ROOTS=("${HOME:-/nonexistent}" "${FT_WORKSPACE:-}" "$PWD")

declare -A seen=()
declare -a dbs=()
for root in "${ROOTS[@]}"; do
  [[ -n "$root" && -d "$root" ]] || continue
  while IFS= read -r db; do
    real="$(cd "$(dirname "$db")" && pwd -P)/$(basename "$db")"
    [[ -n "${seen[$real]:-}" ]] && continue
    seen["$real"]=1
    dbs+=("$real")
  done < <(find "$root" -maxdepth 5 -type f -name 'ft.db' -not -path '*/target/*' -not -path '*/.git/*' 2>/dev/null)
done

now_ms=$(( $(date +%s) * 1000 ))
newest_age_hours=""
rows="[]"
for db in "${dbs[@]}"; do
  q() { sqlite3 -readonly "file:$db?mode=ro" "$1" 2>/dev/null || echo ""; }
  has_segments=$(q "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='output_segments';")
  if [[ "$has_segments" != "1" ]]; then
    rows=$(jq -c --arg db "$db" '. + [{db: $db, state: "not_an_ft_db"}]' <<<"$rows")
    continue
  fi
  last_capture=$(q "SELECT COALESCE(MAX(captured_at), 0) FROM output_segments;")
  segments=$(q "SELECT count(*) FROM output_segments;")
  events=$(q "SELECT count(*) FROM events;")
  workflows=$(q "SELECT count(*) FROM workflow_executions;")
  audits=$(q "SELECT count(*) FROM audit_actions;")
  schema=$(q "SELECT schema_version FROM ft_meta LIMIT 1;")
  age_hours=""
  if [[ "${last_capture:-0}" =~ ^[0-9]+$ && "$last_capture" -gt 0 ]]; then
    age_hours=$(( (now_ms - last_capture) / 3600000 ))
    if [[ -z "$newest_age_hours" || "$age_hours" -lt "$newest_age_hours" ]]; then
      newest_age_hours="$age_hours"
    fi
  fi
  lock="$(dirname "$db")/watch.lock"
  watcher="absent"
  if [[ -e "$lock" ]]; then
    watcher="stale"
    pid=$(head -c 64 "$lock" 2>/dev/null | tr -dc '0-9' | head -c 10)
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then watcher="running(pid $pid)"; fi
  fi
  rows=$(jq -c --arg db "$db" --arg schema "${schema:-?}" --argjson segments "${segments:-0}" \
    --argjson events "${events:-0}" --argjson workflows "${workflows:-0}" --argjson audits "${audits:-0}" \
    --arg age "${age_hours}" --arg watcher "$watcher" \
    '. + [{db: $db, schema_version: $schema, segments: $segments, events: $events,
           workflow_executions: $workflows, audit_actions: $audits,
           last_capture_age_hours: (if $age == "" then null else ($age|tonumber) end),
           watcher: $watcher}]' <<<"$rows")
done

if [[ -z "$newest_age_hours" ]]; then
  verdict="no_capture_found"; ok=false
elif [[ "$newest_age_hours" -le "$MAX_AGE_HOURS" ]]; then
  verdict="fresh"; ok=true
else
  verdict="stale"; ok=false
fi

if [[ $JSON -eq 1 ]]; then
  jq -n --arg host "$(hostname)" --arg ts "$(date -u +%FT%TZ)" --arg verdict "$verdict" \
    --argjson ok "$ok" --arg newest "${newest_age_hours:-}" --argjson max "$MAX_AGE_HOURS" --argjson dbs "$rows" \
    '{schema_version: "frankenterm.dogfood-status.v1", host: $host, checked_at: $ts, verdict: $verdict, ok: $ok,
      newest_capture_age_hours: (if $newest == "" then null else ($newest|tonumber) end),
      max_age_hours: $max, databases: $dbs}'
else
  printf 'dogfood status on %s (%s): %s' "$(hostname)" "$(date -u +%FT%TZ)" "$verdict"
  [[ -n "$newest_age_hours" ]] && printf ' (newest capture %sh ago, limit %sh)' "$newest_age_hours" "$MAX_AGE_HOURS"
  printf '\n'
  jq -r '.[] | if .state then "  \(.db): \(.state)" else
    "  \(.db): schema v\(.schema_version) segments=\(.segments) events=\(.events) workflows=\(.workflow_executions) audits=\(.audit_actions) last_capture_age_hours=\(.last_capture_age_hours // "never") watcher=\(.watcher)" end' <<<"$rows"
fi
$ok && exit 0 || exit 1
