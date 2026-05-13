#!/usr/bin/env bash
# G57 / ft-tf6g3.45: ft-reality-check operator dispatcher.
#
# Wraps the existing reality-check substrate (bv robot-triage,
# br, scripts/check-reality-check-due.sh,
# scripts/check-reality-check-bead-structure.sh) behind a single
# operator-friendly subcommand tree. A native Rust integration into
# the `ft` binary is a v2 enhancement once the
# crates/frankenterm-core build context is ergonomic again; the
# bash wrapper ships the operator surface today.
#
# Subcommands:
#   status             Epic readiness + open/closed/blocked counts
#   next               bv top recommendation scoped to the epic
#   silent-close-audit Apply the G55 phantom-deliverable forensic
#                      audit protocol to the active epic
#   structure-audit    Run the G56 bead-structural validator
#   epic <id>          Switch scope to a specific epic
#   is-due             Run scripts/check-reality-check-due.sh
#
# Output:
#   --json   Machine-readable JSON on stdout
#   default  Human-readable summary on stderr; structured data on stdout
#
# Exit codes:
#   0  Subcommand succeeded; epic green where applicable
#   1  Subcommand reported a non-green state (e.g., due=true, audit
#      findings, structure violations)
#   2  Usage error
#   3  Substrate tool missing (br, bv, jq)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

DEFAULT_EPIC="ft-tf6g3"

JSON_MODE=0
EPIC_ID="${FT_REALITY_CHECK_EPIC:-$DEFAULT_EPIC}"

usage() {
  cat <<USAGE >&2
Usage: scripts/ft-reality-check.sh <subcommand> [options]

Subcommands:
  status                 Epic readiness summary
  next                   Top actionable bead in the epic
  silent-close-audit     Apply G55 protocol to the epic
  structure-audit        Apply G56 validator to the epic
  is-due                 Run reality-check-due trigger detector
  epic <id>              Show the epic-id default + override info

Options:
  --json                 Emit machine-readable JSON
  --epic <id>            Override default epic (also honors
                         \$FT_REALITY_CHECK_EPIC env var)
  -h, --help             Show this help

Examples:
  scripts/ft-reality-check.sh status --json
  scripts/ft-reality-check.sh next --epic ft-tf6g3
  scripts/ft-reality-check.sh structure-audit
USAGE
}

need() {
  command -v "$1" >/dev/null || { echo "error: $1 required (install or PATH-fix)" >&2; exit 3; }
}

emit_human() {
  if [[ "$JSON_MODE" -eq 0 ]]; then
    printf '%s\n' "$1" >&2
  fi
}

emit_json() {
  printf '%s\n' "$1"
}

# ---------------------------------------------------------------------------
# Subcommand handlers
# ---------------------------------------------------------------------------

cmd_status() {
  need br
  need bv
  need jq

  emit_human "ft-reality-check status — epic=$EPIC_ID"

  local triage
  triage=$(bv --robot-triage --graph-root "$EPIC_ID" 2>/dev/null) || true
  local counts
  counts=$(printf '%s' "$triage" | jq '.triage.project_health.counts // {}') || counts='{}'

  local open_children blocked_children in_progress_children closed_children
  open_children=$(br list --json --status=open --limit 5000 2>/dev/null \
    | jq --arg pref "${EPIC_ID}." '[.issues[] | select(.id == $pref[:-1] or (.id | startswith($pref)))] | length')
  blocked_children=$(br list --json --status=blocked --limit 5000 2>/dev/null \
    | jq --arg pref "${EPIC_ID}." '[.issues[] | select(.id | startswith($pref))] | length' || echo 0)
  in_progress_children=$(br list --json --status=in_progress --limit 5000 2>/dev/null \
    | jq --arg pref "${EPIC_ID}." '[.issues[] | select(.id | startswith($pref))] | length' || echo 0)
  closed_children=$(br list --json --status=closed --limit 5000 2>/dev/null \
    | jq --arg pref "${EPIC_ID}." '[.issues[] | select(.id | startswith($pref))] | length' || echo 0)

  local result
  result=$(jq -n \
    --arg epic "$EPIC_ID" \
    --argjson open "${open_children:-0}" \
    --argjson blocked "${blocked_children:-0}" \
    --argjson in_progress "${in_progress_children:-0}" \
    --argjson closed "${closed_children:-0}" \
    --argjson counts "$counts" \
    '{epic: $epic, open: $open, blocked: $blocked, in_progress: $in_progress, closed: $closed, project_health: $counts}')

  emit_json "$result"
  emit_human "  open=$open_children blocked=$blocked_children in_progress=$in_progress_children closed=$closed_children"

  # Exit 1 if there are open or blocked children — epic is not done.
  [[ "${open_children:-0}" -eq 0 && "${blocked_children:-0}" -eq 0 ]] || exit 1
}

cmd_next() {
  need bv
  need jq
  local out
  out=$(bv --robot-triage --graph-root "$EPIC_ID" 2>/dev/null \
    | jq --arg pref "${EPIC_ID}." '.triage.recommendations | map(select(.id == $pref[:-1] or (.id | startswith($pref)))) | sort_by(-.score) | .[0] // null')
  if [[ "$out" == "null" || -z "$out" ]]; then
    emit_human "ft-reality-check next — no recommendations for epic=$EPIC_ID"
    emit_json '{"next": null}'
    return 0
  fi
  emit_json "$(jq -n --argjson next "$out" '{next: $next}')"
  emit_human "ft-reality-check next — top pick: $(printf '%s' "$out" | jq -r '.id') ($(printf '%s' "$out" | jq -r '.title'))"
}

cmd_silent_close_audit() {
  need br
  need jq
  emit_human "ft-reality-check silent-close-audit — epic=$EPIC_ID"

  # Find closed children with zero comments — candidate phantom deliverables.
  local closed_ids
  closed_ids=$(br list --json --status=closed --limit 5000 2>/dev/null \
    | jq -r --arg pref "${EPIC_ID}." '.issues | map(select(.id | startswith($pref))) | .[].id')

  local total=0 phantom_count=0
  local phantom_list=""
  for id in $closed_ids; do
    total=$((total + 1))
    # G55 protocol: a legitimate closure has at least one audit comment
    # or a 'closed' audit-log event with a non-empty comment.
    local comment_count
    comment_count=$(br audit log "$id" --json 2>/dev/null \
      | jq '[.events[] | select(.event_type == "closed" or .event_type == "commented") | .comment // empty | select(length > 0)] | length' || echo 0)
    if [[ "${comment_count:-0}" -eq 0 ]]; then
      phantom_count=$((phantom_count + 1))
      phantom_list="$phantom_list $id"
    fi
  done

  local result
  result=$(jq -n \
    --arg epic "$EPIC_ID" \
    --argjson total "$total" \
    --argjson phantom "$phantom_count" \
    --arg phantom_ids "$phantom_list" \
    '{epic: $epic, total_closed: $total, phantom_close_count: $phantom, phantom_close_ids: ($phantom_ids | split(" ") | map(select(length > 0)))}')

  emit_json "$result"
  emit_human "  total_closed=$total phantom_close_count=$phantom_count"
  [[ "$phantom_count" -eq 0 ]] || exit 1
}

cmd_structure_audit() {
  local validator="${REPO_ROOT}/scripts/check-reality-check-bead-structure.sh"
  if [[ ! -x "$validator" ]]; then
    emit_human "ft-reality-check structure-audit — validator not executable at $validator"
    emit_json '{"error": "validator-missing", "path": "scripts/check-reality-check-bead-structure.sh"}'
    exit 3
  fi
  emit_human "ft-reality-check structure-audit — epic=$EPIC_ID"
  if [[ "$JSON_MODE" -eq 1 ]]; then
    # Try to pass --json through to the validator; if unsupported, wrap.
    if "$validator" --json --epic "$EPIC_ID" 2>/dev/null; then
      return $?
    fi
  fi
  "$validator" --epic "$EPIC_ID"
}

cmd_is_due() {
  local due_script="${REPO_ROOT}/scripts/check-reality-check-due.sh"
  if [[ ! -x "$due_script" ]]; then
    emit_human "ft-reality-check is-due — script not executable at $due_script"
    emit_json '{"error": "due-script-missing"}'
    exit 3
  fi
  if [[ "$JSON_MODE" -eq 1 ]]; then
    "$due_script" --json
  else
    "$due_script"
  fi
}

cmd_epic() {
  local id="${1:-}"
  if [[ -z "$id" ]]; then
    emit_human "ft-reality-check epic — default=$EPIC_ID (override via --epic <id> or \$FT_REALITY_CHECK_EPIC)"
    emit_json "$(jq -n --arg epic "$EPIC_ID" '{default_epic: $epic}')"
    return 0
  fi
  EPIC_ID="$id"
  emit_human "ft-reality-check epic — scope set to $id"
  emit_json "$(jq -n --arg epic "$EPIC_ID" '{epic: $epic}')"
}

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

if [[ $# -eq 0 ]]; then
  usage
  exit 2
fi

SUBCMD="${1:-}"
shift || true

while [[ $# -gt 0 ]]; do
  case "$1" in
    --json) JSON_MODE=1; shift ;;
    --epic) EPIC_ID="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) break ;;
  esac
done

case "$SUBCMD" in
  status)              cmd_status ;;
  next)                cmd_next ;;
  silent-close-audit)  cmd_silent_close_audit ;;
  structure-audit)     cmd_structure_audit ;;
  is-due)              cmd_is_due ;;
  epic)                cmd_epic "${1:-}" ;;
  -h|--help)           usage; exit 0 ;;
  *)                   echo "unknown subcommand: $SUBCMD" >&2; usage; exit 2 ;;
esac
