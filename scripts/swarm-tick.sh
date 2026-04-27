#!/usr/bin/env bash
# Operator-tick helper for the frankenterm swarm.
# Emits a compact JSON snapshot the orchestrator agent can consume to decide
# which panes need fresh marching orders.
#
# Usage: swarm-tick.sh [session]
set -uo pipefail
session="${1:-frankenterm}"

now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
git_commits_1h=$(cd /Users/jemanuel/projects/frankenterm && git log --since="1 hour ago" --oneline 2>/dev/null | wc -l | tr -d ' ')
git_commits_4m=$(cd /Users/jemanuel/projects/frankenterm && git log --since="4 minutes ago" --oneline 2>/dev/null | wc -l | tr -d ' ')

beads_open=$(cd /Users/jemanuel/projects/frankenterm && br list --status open --json 2>/dev/null | jq '.issues|length' 2>/dev/null || echo 0)
beads_in_progress=$(cd /Users/jemanuel/projects/frankenterm && br list --status in_progress --json 2>/dev/null | jq '.issues|length' 2>/dev/null || echo 0)
beads_blocked=$(cd /Users/jemanuel/projects/frankenterm && br list --status blocked --json 2>/dev/null | jq '.issues|length' 2>/dev/null || echo 0)

ready=$(cd /Users/jemanuel/projects/frankenterm && br ready --json 2>/dev/null | jq 'length' 2>/dev/null || echo 0)

# Disk
data_avail=$(df -h /System/Volumes/Data | awk 'NR==2{print $4}')
data_pct=$(df -h /System/Volumes/Data | awk 'NR==2{print $5}')

# Stale build dirs (>12h)
stale_targets=$(find /tmp -maxdepth 1 -name "ft-*-target" -type d -mmin +720 2>/dev/null | wc -l | tr -d ' ')
total_targets=$(ls -d /tmp/ft-*-target 2>/dev/null | wc -l | tr -d ' ')
target_size_mb=$(/usr/bin/du -sk /tmp/ft-*-target 2>/dev/null | awk '{s+=$1} END{print int(s/1024)}')

# Per-pane state
panes_json=$(ntm --robot-status 2>/dev/null | jq --arg s "$session" '.sessions[] | select(.name==$s) | {panes_count: .panes, agents: [.agents[] | {idx: .pane_idx, type, pane}]}')

cat <<EOF
{
  "ts": "$now",
  "session": "$session",
  "git": { "commits_1h": $git_commits_1h, "commits_since_last_tick": $git_commits_4m },
  "beads": { "open": $beads_open, "in_progress": $beads_in_progress, "blocked": $beads_blocked, "ready": $ready },
  "disk": { "data_avail": "$data_avail", "data_used_pct": "$data_pct", "stale_targets_12h": $stale_targets, "total_targets": $total_targets, "targets_size_mb": ${target_size_mb:-0} },
  "swarm": $panes_json
}
EOF
