#!/usr/bin/env bash
# Advisory: warn when the last WezTerm upstream backport batch recorded in
# frankenterm/PROVENANCE.md ("### Batch YYYY-MM-DD: ...") is older than the
# weekly cadence allows. Exit 0 unless --strict and the batch is overdue.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MAX_AGE_DAYS=14
STRICT=0
for arg in "$@"; do
  case "$arg" in
    --strict) STRICT=1 ;;
    --max-age-days=*) MAX_AGE_DAYS="${arg#*=}" ;;
    -h|--help)
      echo "Usage: scripts/check-upstream-backport-due.sh [--strict] [--max-age-days=N]"
      exit 0
      ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

last=$(grep -oE '^### Batch [0-9]{4}-[0-9]{2}-[0-9]{2}' "$REPO_ROOT/frankenterm/PROVENANCE.md" \
  | awk '{print $3}' | sort | tail -1)
if [ -z "$last" ]; then
  echo "upstream backport: no batch recorded in frankenterm/PROVENANCE.md"
  [ "$STRICT" -eq 1 ] && exit 1
  exit 0
fi
age_days=$(python3 -c "import datetime,sys; print((datetime.date.today() - datetime.date.fromisoformat(sys.argv[1])).days)" "$last")
if [ "$age_days" -gt "$MAX_AGE_DAYS" ]; then
  echo "upstream backport: last batch $last is ${age_days} days old (> ${MAX_AGE_DAYS}); run the weekly workflow in AGENTS.md"
  [ "$STRICT" -eq 1 ] && exit 1
else
  echo "upstream backport: last batch $last (${age_days} days ago)"
fi
exit 0
