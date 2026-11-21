#!/usr/bin/env bash
# scripts/reality-check-status.sh — weekly reality-check progress drumbeat.
#
# Bead: ft-at08r (BR-RC-WEEKLY-DRUMBEAT).
#
# Counts open / in_progress / closed across every bead carrying the
# `reality-check` label, sectioned by epic-prefix (BR-RC-FOUNDATION,
# BR-RC-CUTOVERS, BR-RC-RUNTIME-SEMANTICS, BR-RC-ROBOT-CONTRACT, etc.),
# and renders a markdown report linking to the live attestation manifest
# at docs/attestations/manifest.json.
#
# Usage:
#   scripts/reality-check-status.sh                    # write report
#                                                      # to docs/reports/
#                                                      # reality-check-<date>.md
#   scripts/reality-check-status.sh --stdout           # stream to stdout
#                                                      # (no file written)
#   scripts/reality-check-status.sh --json             # machine-readable
#                                                      # rollup (used by
#                                                      # the weekly cron
#                                                      # workflow that
#                                                      # also updates
#                                                      # README to point
#                                                      # at the latest
#                                                      # report)
#
# Cadence: run manually (or from a local cron) when
# scripts/check-reality-check-due.sh reports a due trigger; releases are
# DSR-only and no GitHub workflow drives this script.
# README link cadence: the cron job rewrites the dated link in the
# README's "Trust & Attestation" section after each run.
#
# Requires: br (beads CLI), jq.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

OUTPUT_MODE="file"     # file | stdout | json
DATE_STAMP="$(date -u +%Y-%m-%d)"
TIMESTAMP_UTC="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
EPIC_FILTER=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --stdout) OUTPUT_MODE="stdout"; shift ;;
    --json)   OUTPUT_MODE="json";   shift ;;
    --date)   DATE_STAMP="$2";      shift 2 ;;
    --epic)   EPIC_FILTER="$2";     shift 2 ;;
    -h|--help)
      sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown flag: $1" >&2
      exit 2
      ;;
  esac
done

command -v br >/dev/null 2>&1 || { echo "error: br (beads CLI) required" >&2; exit 1; }
command -v jq >/dev/null 2>&1 || { echo "error: jq required" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Pull the issue list once. We query each status independently since `br
# list --label reality-check` returns only open issues by default.
# ---------------------------------------------------------------------------

cd "$REPO_ROOT"

OPEN_JSON="$(br list --label reality-check --status=open --json 2>/dev/null || echo '{"issues":[]}')"
IP_JSON="$(br list --label reality-check --status=in_progress --json 2>/dev/null || echo '{"issues":[]}')"
CLOSED_JSON="$(br list --label reality-check --status=closed --json 2>/dev/null || echo '{"issues":[]}')"

OPEN_COUNT="$(jq '.issues | length' <<<"$OPEN_JSON")"
IP_COUNT="$(jq '.issues | length' <<<"$IP_JSON")"
CLOSED_COUNT="$(jq '.issues | length' <<<"$CLOSED_JSON")"
TOTAL_COUNT=$((OPEN_COUNT + IP_COUNT + CLOSED_COUNT))

# ---------------------------------------------------------------------------
# Bucket issues by round/epic prefix (RC1 BR-RC-*, RC2 ft-tf6g3, RC3 ft-xxfwy,
# RC4 ft-mgy5t2, plus explicit [RC<N>.G<M>] title markers). Only beads without
# any recognized round or BR-RC-* marker fall into "uncategorized" (ft-mgy5t2.9).
# ---------------------------------------------------------------------------

bucket_issues() {
  # arg 1: status JSON.   prints "<epic>\t<id>\t<title>" lines.
  jq -r --arg filter "$EPIC_FILTER" '
    (.issues // .)[]
    | select($filter == "" or .id == $filter or (.id | startswith($filter + ".")))
    | [.id, .title] | @tsv
  ' <<<"$1" | while IFS=$'\t' read -r id title; do
    epic=""
    case "$id" in
      ft-mgy5t2|ft-mgy5t2.*) epic="RC4-ft-mgy5t2" ;;
      ft-xxfwy|ft-xxfwy.*)   epic="RC3-ft-xxfwy" ;;
      ft-tf6g3|ft-tf6g3.*)   epic="RC2-ft-tf6g3" ;;
      *)
        epic="$(printf '%s' "$title" | grep -oE 'BR-RC-[A-Z][A-Z0-9-]*' | head -n1 || true)"
        if [[ -z "$epic" ]]; then
          if printf '%s' "$title" | grep -qE '^\[RC4[.\]]|reality-check-2026-11-20'; then
            epic="RC4-ft-mgy5t2"
          elif printf '%s' "$title" | grep -qE '^\[RC3[.\]]|reality-check-2026-09-01'; then
            epic="RC3-ft-xxfwy"
          elif printf '%s' "$title" | grep -qE 'reality-check-2026-05-12'; then
            epic="RC2-ft-tf6g3"
          else
            epic="uncategorized"
          fi
        fi
        ;;
    esac
    printf '%s\t%s\t%s\n' "$epic" "$id" "$title"
  done
}

# Optional closeout-audit hook (owned by ft-mgy5t2.13; degrades gracefully when absent).
CLOSEOUT_HEALTH_JSON="null"
if [[ -x "$REPO_ROOT/scripts/beads-closeout-audit.sh" ]]; then
  if audit_out="$("$REPO_ROOT/scripts/beads-closeout-audit.sh" --json 2>/dev/null)"; then
    if jq -e . >/dev/null 2>&1 <<<"$audit_out"; then
      CLOSEOUT_HEALTH_JSON="$audit_out"
    fi
  fi
fi

ALL_TSV="$(
  {
    bucket_issues "$OPEN_JSON"   | awk -F'\t' '{print $1"\topen\t"$2"\t"$3}'
    bucket_issues "$IP_JSON"     | awk -F'\t' '{print $1"\tin_progress\t"$2"\t"$3}'
    bucket_issues "$CLOSED_JSON" | awk -F'\t' '{print $1"\tclosed\t"$2"\t"$3}'
  } | sort
)"

OPEN_COUNT="$(printf '%s\n' "$ALL_TSV" | awk -F'\t' '$2=="open"' | wc -l | tr -d ' ')"
IP_COUNT="$(printf '%s\n' "$ALL_TSV" | awk -F'\t' '$2=="in_progress"' | wc -l | tr -d ' ')"
CLOSED_COUNT="$(printf '%s\n' "$ALL_TSV" | awk -F'\t' '$2=="closed"' | wc -l | tr -d ' ')"
TOTAL_COUNT=$((OPEN_COUNT + IP_COUNT + CLOSED_COUNT))

EPICS="$(printf '%s\n' "$ALL_TSV" | awk -F'\t' '{print $1}' | sort -u | grep -v '^$' || true)"

# ---------------------------------------------------------------------------
# Pull canonical attestation categories from
# docs/attestations/manifest.json so the report can link claim → bead.
# ---------------------------------------------------------------------------

MANIFEST_PATH="$REPO_ROOT/docs/attestations/manifest.json"
ATTESTATION_TABLE=""
if [[ -f "$MANIFEST_PATH" ]]; then
  ATTESTATION_TABLE="$(
    jq -r '.slots[] | "| \(.category) | \(.produced_by_bead // "—") | \(if .path then "[present](" + .path + ")" else "_pending_" end) |"' \
      "$MANIFEST_PATH"
  )"
fi

# ---------------------------------------------------------------------------
# Renderers.
# ---------------------------------------------------------------------------

render_json() {
  jq -n \
    --arg date "$DATE_STAMP" \
    --arg ts "$TIMESTAMP_UTC" \
    --arg epic_filter "$EPIC_FILTER" \
    --argjson totals "{\"open\":$OPEN_COUNT,\"in_progress\":$IP_COUNT,\"closed\":$CLOSED_COUNT,\"total\":$TOTAL_COUNT}" \
    --arg epics "$EPICS" \
    --arg rows "$ALL_TSV" \
    --argjson closeout_health "$CLOSEOUT_HEALTH_JSON" \
    '{
      report_date: $date,
      generated_at: $ts,
      epic_filter: (if $epic_filter == "" then null else $epic_filter end),
      totals: $totals,
      closeout_health: $closeout_health,
      epics: ($epics | split("\n") | map(select(. != ""))),
      issues_by_epic: (
        $rows | split("\n") | map(select(. != "")) | map(
          split("\t") | { epic: .[0], status: .[1], id: .[2], title: .[3] }
        ) | group_by(.epic) | map({
          epic: .[0].epic,
          counts: { open: ([.[] | select(.status=="open")] | length),
                    in_progress: ([.[] | select(.status=="in_progress")] | length),
                    closed: ([.[] | select(.status=="closed")] | length) },
          issues: .
        })
      )
    }'
}

render_markdown() {
  cat <<EOF
# Reality-Check Drumbeat — $DATE_STAMP

_Generated $TIMESTAMP_UTC by [\`scripts/reality-check-status.sh\`](../../scripts/reality-check-status.sh)._
_Bead: \`ft-at08r\` (BR-RC-WEEKLY-DRUMBEAT)._

## Headline rollup

| Status | Count |
| ------ | ----- |
| open | $OPEN_COUNT |
| in_progress | $IP_COUNT |
| closed | $CLOSED_COUNT |
| **total** | **$TOTAL_COUNT** |

EOF

  if [[ -n "$EPICS" ]]; then
    printf '## By epic\n\n'
    while read -r epic; do
      [[ -z "$epic" ]] && continue
      epic_open=$(printf '%s\n' "$ALL_TSV" | awk -F'\t' -v e="$epic" '$1==e && $2=="open"' | wc -l | tr -d ' ')
      epic_ip=$(printf '%s\n' "$ALL_TSV" | awk -F'\t' -v e="$epic" '$1==e && $2=="in_progress"' | wc -l | tr -d ' ')
      epic_closed=$(printf '%s\n' "$ALL_TSV" | awk -F'\t' -v e="$epic" '$1==e && $2=="closed"' | wc -l | tr -d ' ')
      epic_total=$((epic_open + epic_ip + epic_closed))
      printf '### %s — %d open / %d in_progress / %d closed (%d total)\n\n' \
        "$epic" "$epic_open" "$epic_ip" "$epic_closed" "$epic_total"
      printf '| Status | ID | Title |\n| ------ | -- | ----- |\n'
      printf '%s\n' "$ALL_TSV" | awk -F'\t' -v e="$epic" '$1==e {printf "| %s | %s | %s |\n", $2, $3, $4}'
      printf '\n'
    done <<<"$EPICS"
  fi

  if [[ -n "$ATTESTATION_TABLE" ]]; then
    cat <<EOF
## Attestation status

The release attestation manifest at
[\`docs/attestations/manifest.json\`](../attestations/manifest.json)
declares one slot per reality-check claim. A slot with a non-null
\`path\` means the producing bead has shipped its artifact and the
release-bundle build picks it up; \`_pending_\` means the bead is
still open.

| Category | Producing bead | Artifact |
| -------- | -------------- | -------- |
$ATTESTATION_TABLE

EOF
  fi

  cat <<EOF
## Cross-references

- Bead board (open RC work): \`br list --label reality-check --status=open\`
- Bridge plans: [\`docs/reality-check-bridge-plan-2026-11-20.md\`](../reality-check-bridge-plan-2026-11-20.md) (RC4), [\`docs/reality-check-bridge-plan-2026-09-01.md\`](../reality-check-bridge-plan-2026-09-01.md) (RC3), [\`docs/reality-check-bridge-plan-2026-05-12.md\`](../reality-check-bridge-plan-2026-05-12.md) (RC2), [\`docs/reality-check-bridge-plan.md\`](../reality-check-bridge-plan.md) (RC1)
- Attestation schema + verifier: [\`docs/attestations/schema.json\`](../attestations/schema.json), [\`scripts/attestation-verify.sh\`](../../scripts/attestation-verify.sh)
- Cadence: manual, when \`scripts/check-reality-check-due.sh\` reports a due trigger
EOF
}

# ---------------------------------------------------------------------------
# Dispatch.
# ---------------------------------------------------------------------------

case "$OUTPUT_MODE" in
  stdout)
    render_markdown
    ;;
  json)
    render_json
    ;;
  file)
    OUT="$REPO_ROOT/docs/reports/reality-check-$DATE_STAMP.md"
    render_markdown > "$OUT"
    echo "wrote $OUT" >&2
    echo "$OUT"
    ;;
esac
