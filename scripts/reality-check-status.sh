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
# Cron cadence: see .github/workflows/reality-check-drumbeat.yml.
# README link cadence: the cron job rewrites the dated link in the
# README's "Trust & Attestation" section after each run.
#
# Requires: br (beads CLI), jq.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

OUTPUT_MODE="file"     # file | stdout | json
DATE_STAMP="$(date -u +%Y-%m-%d)"
TIMESTAMP_UTC="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --stdout) OUTPUT_MODE="stdout"; shift ;;
    --json)   OUTPUT_MODE="json";   shift ;;
    --date)   DATE_STAMP="$2";      shift 2 ;;
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
# Bucket issues by epic-prefix. Epic prefix = the first three dot-separated
# tokens of the BR-RC-* tag in the title (e.g.
# "BR-RC-FOUNDATION.G3.1" → "BR-RC-FOUNDATION"). Issues without a BR-RC-
# prefix in their title fall into "uncategorized".
# ---------------------------------------------------------------------------

bucket_issues() {
  # arg 1: status JSON.   prints "<epic>\t<id>\t<title>" lines.
  jq -r '.issues[] | [.id, .title] | @tsv' <<<"$1" | while IFS=$'\t' read -r id title; do
    epic="$(printf '%s' "$title" | grep -oE 'BR-RC-[A-Z][A-Z0-9-]*' | head -n1)"
    [[ -z "$epic" ]] && epic="uncategorized"
    printf '%s\t%s\t%s\n' "$epic" "$id" "$title"
  done
}

ALL_TSV="$(
  {
    bucket_issues "$OPEN_JSON"   | awk -F'\t' '{print $1"\topen\t"$2"\t"$3}'
    bucket_issues "$IP_JSON"     | awk -F'\t' '{print $1"\tin_progress\t"$2"\t"$3}'
    bucket_issues "$CLOSED_JSON" | awk -F'\t' '{print $1"\tclosed\t"$2"\t"$3}'
  } | sort
)"

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
    --argjson totals "{\"open\":$OPEN_COUNT,\"in_progress\":$IP_COUNT,\"closed\":$CLOSED_COUNT,\"total\":$TOTAL_COUNT}" \
    --arg epics "$EPICS" \
    --arg rows "$ALL_TSV" \
    '{
      report_date: $date,
      generated_at: $ts,
      totals: $totals,
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
- Bridge plan: [\`docs/reality-check-bridge-plan.md\`](../reality-check-bridge-plan.md)
- Attestation schema + verifier: [\`docs/attestations/schema.json\`](../attestations/schema.json), [\`scripts/attestation-verify.sh\`](../../scripts/attestation-verify.sh)
- Cadence: weekly via \`.github/workflows/reality-check-drumbeat.yml\`
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
