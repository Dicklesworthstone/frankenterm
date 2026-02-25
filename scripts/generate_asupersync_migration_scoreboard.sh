#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_JSON="${1:-${ROOT_DIR}/docs/asupersync-migration-scoreboard.json}"
OUTPUT_MD="${2:-${ROOT_DIR}/docs/asupersync-migration-scoreboard.md}"

GENERATED_AT="${FT_ASUPERSYNC_GENERATED_AT:-$(date -u +"%Y-%m-%dT%H:%M:%SZ")}"
ISSUES_SOURCE="${FT_ASUPERSYNC_ISSUES_JSON:-}"
READY_SOURCE="${FT_ASUPERSYNC_READY_JSON:-}"
ISSUE_SCOPE_PREFIX="ft-e34d9.10"

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required" >&2
  exit 1
fi

if [[ -z "${ISSUES_SOURCE}" ]]; then
  if ! command -v br >/dev/null 2>&1; then
    echo "br is required when FT_ASUPERSYNC_ISSUES_JSON is not set" >&2
    exit 1
  fi
  ISSUES_JSON="$(br list --json)"
else
  ISSUES_JSON="$(cat "${ISSUES_SOURCE}")"
fi

if [[ -z "${READY_SOURCE}" ]]; then
  if ! command -v br >/dev/null 2>&1; then
    echo "br is required when FT_ASUPERSYNC_READY_JSON is not set" >&2
    exit 1
  fi
  READY_JSON="$(br ready --json)"
else
  READY_JSON="$(cat "${READY_SOURCE}")"
fi

RISK_JSON='[
  {
    "id": "R1",
    "hazard": "Cx threading breaks function signatures across async modules",
    "probability": 5,
    "impact": 4,
    "score": 20,
    "mitigation": "Thread Cx through scope boundaries first, then leaf functions.",
    "owner": "ft-e34d9.10.3"
  },
  {
    "id": "R2",
    "hazard": "select/join migration introduces cancellation bugs",
    "probability": 4,
    "impact": 5,
    "score": 20,
    "mitigation": "Catalog race sites and add cancellation-focused tests before/after migration.",
    "owner": "ft-e34d9.10.3"
  },
  {
    "id": "R3",
    "hazard": "Broadcast migration breaks event bus fanout",
    "probability": 4,
    "impact": 4,
    "score": 16,
    "mitigation": "Prototype asupersync broadcast replacement in isolation before wide rollout.",
    "owner": "ft-e34d9.10.2"
  },
  {
    "id": "R4",
    "hazard": "Signal handling gap blocks graceful shutdown",
    "probability": 3,
    "impact": 5,
    "score": 15,
    "mitigation": "Implement runtime_compat signal bridge with explicit shutdown assertions.",
    "owner": "ft-e34d9.10.4"
  },
  {
    "id": "R5",
    "hazard": "Vendored smol surfaces delay runtime convergence",
    "probability": 4,
    "impact": 3,
    "score": 12,
    "mitigation": "Isolate vendored harmonization wave and lock interface contracts early.",
    "owner": "ft-e34d9.10.5"
  }
]'

mkdir -p "$(dirname "${OUTPUT_JSON}")" "$(dirname "${OUTPUT_MD}")"

jq -n \
  --arg generated_at "${GENERATED_AT}" \
  --arg issue_scope_prefix "${ISSUE_SCOPE_PREFIX}" \
  --argjson issues "${ISSUES_JSON}" \
  --argjson ready "${READY_JSON}" \
  --argjson risk_ledger "${RISK_JSON}" '
  def issue_map:
    reduce (($issues // [])[] | select(.id | startswith($issue_scope_prefix))) as $i ({}; .[$i.id] = $i);
  def critical_ids:
    [
      "ft-e34d9.10.1",
      "ft-e34d9.10.2",
      "ft-e34d9.10.3",
      "ft-e34d9.10.4",
      "ft-e34d9.10.5",
      "ft-e34d9.10.6",
      "ft-e34d9.10.7",
      "ft-e34d9.10.8"
    ];
  def lookup($m; $id):
    ($m[$id] // {id: $id, title: "missing", status: "missing", priority: null, assignee: null});
  def status_counts($arr):
    reduce $arr[] as $item ({}; .[$item.status] = ((.[$item.status] // 0) + 1));
  ($issues // []) as $issues_all
  | ($issues_all | map(select(.id | startswith($issue_scope_prefix)))) as $scoped
  | issue_map as $imap
  | {
      schema_version: 1,
      generated_at: $generated_at,
      bead_id: "ft-e34d9.10.1.3",
      issue_scope_prefix: $issue_scope_prefix,
      source: {
        issues: (if $issues == null then "none" else "br list --json or fixture" end),
        ready: (if $ready == null then "none" else "br ready --json or fixture" end)
      },
      counts: {
        total: ($scoped | length),
        by_status: (status_counts($scoped))
      },
      critical_path: (
        critical_ids
        | map(lookup($imap; .) | {
            id: .id,
            title: .title,
            status: .status,
            priority: .priority,
            assignee: (.assignee // null)
          })
      ),
      ready_candidates: (
        ($ready // [])
        | map(select(.id | startswith($issue_scope_prefix)) | {
            id: .id,
            title: .title,
            status: .status,
            priority: .priority,
            assignee: (.assignee // null)
          })
      ),
      issue_progress: (
        $scoped
        | sort_by(.id)
        | map({
            id: .id,
            title: .title,
            status: .status,
            priority: .priority,
            assignee: (.assignee // null),
            completion_signal: (
              if .status == "closed" then 1.0
              elif .status == "in_progress" then 0.5
              elif .status == "blocked" then 0.1
              else 0.0
              end
            ),
            evidence_hint: ("br show " + .id + " --json and evidence/" + .id + "/")
          })
      ),
      blocked_count: (
        $scoped
        | map(select(.status == "blocked"))
        | length
      ),
      risk_ledger: $risk_ledger,
      highest_risk: ($risk_ledger | max_by(.score)),
      rollback_impact_tiers: [
        {
          tier: "red",
          threshold_score: 20,
          action: "freeze merges touching runtime bootstrap, prioritize mitigation owner bead"
        },
        {
          tier: "amber",
          threshold_score: 15,
          action: "allow scoped merges with explicit risk evidence and rollback notes"
        },
        {
          tier: "green",
          threshold_score: 0,
          action: "normal execution with standard quality gates"
        }
      ],
      evidence_contract: {
        required_artifacts: [
          "docs/asupersync-migration-scoreboard.json",
          "docs/asupersync-migration-scoreboard.md",
          "tests/e2e/test_asupersync_migration_scoreboard.sh"
        ],
        structured_log_fields: [
          "timestamp",
          "component",
          "scenario_id",
          "correlation_id",
          "decision_path",
          "input_summary",
          "outcome",
          "reason_code",
          "error_code",
          "artifact_path"
        ],
        heavy_compute_policy: "rch exec -- <command>"
      }
    }
' > "${OUTPUT_JSON}"

jq -r '
  "# asupersync Migration Scoreboard (ft-e34d9.10.1.3)\n",
  "Generated at: \(.generated_at)\n",
  "## Summary",
  "",
  "- Scoped issues: \(.counts.total)",
  "- Blocked issues: \(.blocked_count)",
  "- Highest risk: \(.highest_risk.id) (score=\(.highest_risk.score), owner=\(.highest_risk.owner))",
  "",
  "## Status Counts",
  "",
  "| Status | Count |",
  "|---|---:|",
  (
    .counts.by_status
    | to_entries
    | sort_by(.key)
    | .[]
    | "| \(.key) | \(.value) |"
  ),
  "",
  "## Issue Progress (All Scoped Beads)",
  "",
  "| ID | Status | Completion Signal | Evidence Link Hint |",
  "|---|---|---:|---|",
  (
    .issue_progress[]
    | "| \(.id) | \(.status) | \(.completion_signal) | \(.evidence_hint) |"
  ),
  "",
  "## Critical Path",
  "",
  "| ID | Status | Priority | Assignee | Title |",
  "|---|---|---:|---|---|",
  (
    .critical_path[]
    | "| \(.id) | \(.status) | \(.priority // "-") | \(.assignee // "-") | \(.title) |"
  ),
  "",
  "## Ready Candidates",
  "",
  "| ID | Priority | Assignee | Title |",
  "|---|---:|---|---|",
  (
    if (.ready_candidates | length) == 0
    then "| (none) | - | - | - |"
    else (
      .ready_candidates[]
      | "| \(.id) | \(.priority // "-") | \(.assignee // "-") | \(.title) |"
    )
    end
  ),
  "",
  "## Top Risks",
  "",
  "| Risk ID | Score | Owner | Hazard | Mitigation |",
  "|---|---:|---|---|---|",
  (
    .risk_ledger
    | sort_by(-.score)
    | .[]
    | "| \(.id) | \(.score) | \(.owner) | \(.hazard) | \(.mitigation) |"
  ),
  "",
  "## Rollback Tiers",
  "",
  "| Tier | Threshold Score | Action |",
  "|---|---:|---|",
  (
    .rollback_impact_tiers[]
    | "| \(.tier) | \(.threshold_score) | \(.action) |"
  ),
  "",
  "## Validation Commands",
  "",
  "```bash",
  "bash scripts/generate_asupersync_migration_scoreboard.sh",
  "bash tests/e2e/test_asupersync_migration_scoreboard.sh",
  "```",
  "",
  "Heavy compile/test/clippy validation policy: `rch exec -- <command>`."
' "${OUTPUT_JSON}" > "${OUTPUT_MD}"

echo "Generated asupersync migration scoreboard:"
echo "  JSON: ${OUTPUT_JSON}"
echo "  MD:   ${OUTPUT_MD}"
