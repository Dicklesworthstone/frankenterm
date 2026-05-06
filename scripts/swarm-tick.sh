#!/usr/bin/env bash
# Operator-tick helper for the frankenterm swarm.
# Emits a compact JSON snapshot the orchestrator agent can consume to decide
# which panes need fresh marching orders. Where the installed ntm binary lacks
# `ntm coordinator ...`, this script exposes the closest read-only robot-mode
# equivalents under `.coordinator`.
#
# Usage: swarm-tick.sh [session]
# Env overrides (mainly for tests):
#   REPO_ROOT  — repo path to cd into (default: /Users/jemanuel/projects/frankenterm)
#   DISK_VOL   — `df -h` target volume. Default branches on uname:
#                /System/Volumes/Data on Darwin, / elsewhere.
#   FT_OPERATOR_LOCK_DIR — shared operator-script lock dir (default: /tmp/ft-operator-scripts.lock)
#
# Platform: macOS + Linux (ft-v5lz3.2.7). All external commands used here
# (df -h, du -sk, find -maxdepth -mmin, ls -d) accept identical flags on
# BSD and GNU coreutils.
set -uo pipefail
session="${1:-frankenterm}"

# Pick a sensible default disk volume per platform. macOS volumes mount
# the user's data partition under /System/Volumes/Data; Linux puts it
# at /. Operators can always override via DISK_VOL.
default_disk_vol() {
  case "$(uname -s)" in
    Darwin) printf '%s\n' "/System/Volumes/Data" ;;
    *)      printf '%s\n' "/" ;;
  esac
}

repo_root="${REPO_ROOT:-/Users/jemanuel/projects/frankenterm}"
disk_vol="${DISK_VOL:-$(default_disk_vol)}"
operator_lock_dir="${FT_OPERATOR_LOCK_DIR:-/tmp/ft-operator-scripts.lock}"

acquire_operator_lock() {
  local lock_dir="$1"
  local deadline="${FT_OPERATOR_LOCK_TIMEOUT_SECS:-30}"
  local start
  start=$(date +%s)

  while ! mkdir "$lock_dir" 2>/dev/null; do
    local holder=""
    if [ -f "$lock_dir/pid" ]; then
      holder=$(cat "$lock_dir/pid" 2>/dev/null || true)
    fi
    if [ -n "$holder" ] && ! kill -0 "$holder" 2>/dev/null; then
      rm -f "$lock_dir/pid" "$lock_dir/name" 2>/dev/null || true
      rmdir "$lock_dir" 2>/dev/null || true
      continue
    fi

    local now_s
    now_s=$(date +%s)
    if [ $((now_s - start)) -ge "$deadline" ]; then
      echo "timed out waiting for operator lock: $lock_dir" >&2
      return 75
    fi
    sleep 0.1
  done

  printf '%s\n' "$$" > "$lock_dir/pid"
  printf '%s\n' "swarm-tick.sh" > "$lock_dir/name"
}

release_operator_lock() {
  local lock_dir="$1"
  if [ -f "$lock_dir/pid" ] && [ "$(cat "$lock_dir/pid" 2>/dev/null || true)" = "$$" ]; then
    rm -f "$lock_dir/pid" "$lock_dir/name" 2>/dev/null || true
    rmdir "$lock_dir" 2>/dev/null || true
  fi
}

acquire_operator_lock "$operator_lock_dir" || exit $?
trap 'release_operator_lock "$operator_lock_dir"' EXIT
trap 'release_operator_lock "$operator_lock_dir"; exit 130' INT
trap 'release_operator_lock "$operator_lock_dir"; exit 143' TERM

now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
git_commits_1h=$(cd "$repo_root" && git log --since="1 hour ago" --oneline 2>/dev/null | wc -l | tr -d ' ')
git_commits_4m=$(cd "$repo_root" && git log --since="4 minutes ago" --oneline 2>/dev/null | wc -l | tr -d ' ')

beads_open=$(cd "$repo_root" && br list --status open --json 2>/dev/null | jq '.issues|length' 2>/dev/null || echo 0)
beads_in_progress=$(cd "$repo_root" && br list --status in_progress --json 2>/dev/null | jq '.issues|length' 2>/dev/null || echo 0)
beads_blocked=$(cd "$repo_root" && br list --status blocked --json 2>/dev/null | jq '.issues|length' 2>/dev/null || echo 0)

json_array_count_or_zero() {
  local output
  output="$("$@" 2>/dev/null || true)"
  if [ -n "$output" ]; then
    printf '%s\n' "$output" | jq 'if type == "array" then length else 0 end' 2>/dev/null || echo 0
  else
    echo 0
  fi
}

ready=$(cd "$repo_root" && json_array_count_or_zero br ready --json || echo 0)

# Disk
data_avail=$(df -h "$disk_vol" | awk 'NR==2{print $4}')
data_pct=$(df -h "$disk_vol" | awk 'NR==2{print $5}')

# Stale build dirs (>12h)
stale_targets=$(find /tmp -maxdepth 1 -name "ft-*-target" -type d -mmin +720 2>/dev/null | wc -l | tr -d ' ')
# shellcheck disable=SC2012  # We only need a count of glob matches; ls is fine.
total_targets=$(ls -d /tmp/ft-*-target 2>/dev/null | wc -l | tr -d ' ')
target_size_mb=$(du -sk /tmp/ft-*-target 2>/dev/null | awk '{s+=$1} END{print int(s/1024)}')

# Per-pane state
panes_json=$(ntm --robot-status 2>/dev/null | jq --arg s "$session" '.sessions[] | select(.name==$s) | {panes_count: .panes, agents: [.agents[] | {idx: .pane_idx, type, pane}]}')
# Fallback so output remains valid JSON if the session isn't present.
if [ -z "$panes_json" ]; then
  panes_json='{ "panes_count": 0, "agents": [] }'
fi

json_or_null() {
  local output
  output="$("$@" 2>/dev/null || true)"
  if [ -n "$output" ] && printf '%s\n' "$output" | jq -e . >/dev/null 2>&1; then
    printf '%s\n' "$output"
  else
    printf 'null\n'
  fi
}

health_json=$(json_or_null ntm "--robot-health=$session" --json)
alerts_json=$(json_or_null ntm --robot-alerts --alerts-session "$session" --json)
assign_json=$(json_or_null ntm "--robot-assign=$session" --strategy=balanced --json)
conflicts_json=$(json_or_null ntm conflicts "$session" --since 6h --limit 10 --json)

coordinator_json=$(jq -cn \
  --argjson health "$health_json" \
  --argjson alerts "$alerts_json" \
  --argjson assign "$assign_json" \
  --argjson conflicts "$conflicts_json" \
  '
  def conflict_count:
    if ($conflicts | type) == "array" then
      ($conflicts | length)
    elif ($conflicts | type) == "object" then
      ($conflicts.count // (($conflicts.conflicts // []) | length) // 0)
    else
      0
    end;

  {
    "mode": "ntm_robot_equivalents",
    "native_coordinator_available": false,
    "status": {
      "total_agents": ($health.summary.total // 0),
      "healthy": ($health.summary.healthy // 0),
      "degraded": ($health.summary.degraded // 0),
      "unhealthy": ($health.summary.unhealthy // 0),
      "rate_limited": ($health.summary.rate_limited // 0)
    },
    "digest": {
      "active_alerts": ($alerts.count // (($alerts.alerts // []) | length) // 0),
      "critical_or_error_alerts": (($alerts.alerts // []) | map(select(.severity == "critical" or .severity == "error")) | length)
    },
    "conflicts": {
      "count": conflict_count
    },
    "auto_assign": {
      "idle_agents": ($assign.summary.idle_agents // (($assign.idle_agents // []) | length) // 0),
      "recommendations": ($assign.summary.recommendations // (($assign.recommendations // []) | length) // 0),
      "blocked_beads": (($assign.blocked_beads // []) | length)
    }
  }')

cat <<EOF
{
  "ts": "$now",
  "session": "$session",
  "git": { "commits_1h": $git_commits_1h, "commits_since_last_tick": $git_commits_4m },
  "beads": { "open": $beads_open, "in_progress": $beads_in_progress, "blocked": $beads_blocked, "ready": $ready },
  "disk": { "data_avail": "$data_avail", "data_used_pct": "$data_pct", "stale_targets_12h": $stale_targets, "total_targets": $total_targets, "targets_size_mb": ${target_size_mb:-0} },
  "swarm": $panes_json,
  "coordinator": $coordinator_json
}
EOF
