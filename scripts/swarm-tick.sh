#!/usr/bin/env bash
# Operator-tick helper for the frankenterm swarm.
# Emits a compact JSON snapshot the orchestrator agent can consume to decide
# which panes need fresh marching orders. Where the installed ntm binary lacks
# `ntm coordinator ...`, this script exposes the closest read-only robot-mode
# equivalents under `.coordinator`.
#
# Usage:
#   swarm-tick.sh [session]
#   swarm-tick.sh --agent-mail-fallback [session]
#   swarm-tick.sh --agent-mail-handoff --bead <id> [--touched-path PATH ...]
#                 [--avoided-path PATH ...] [--proof-command CMD ...] [session]
#
# `--agent-mail-fallback` emits a read-only Beads/git coordination snapshot
# for the AGENTS.md rule where Agent Mail is unavailable after one retry.
# `--agent-mail-handoff` formats that snapshot as a Markdown Beads comment
# block for human review; it never writes comments or mutates Beads itself.
# Env overrides (mainly for tests):
#   REPO_ROOT  — repo path to cd into (default: /Users/jemanuel/projects/frankenterm)
#   DISK_VOL   — `df -h` target volume. Default branches on uname:
#                /System/Volumes/Data on Darwin, / elsewhere.
#   FT_OPERATOR_LOCK_DIR — shared operator-script lock dir (default: /tmp/ft-operator-scripts.lock)
#   FT_OPERATOR_NOW_ISO / FT_OPERATOR_NOW_EPOCH — deterministic test clock.
#   FT_AGENT_MAIL_FAILURE_CLASS — classifier input for fallback snapshots:
#                available, database_recovery_notice, database_error,
#                api_unreachable, timeout, registration_failed,
#                contact_failed, or unknown.
#
# Platform: macOS + Linux (ft-v5lz3.2.7). All external commands used here
# (df -h, du -sk, find -maxdepth -mmin, ls -d) accept identical flags on
# BSD and GNU coreutils.
set -uo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "${script_dir}/agent-mail-failover-classifier.sh"

mode="tick"
session="frankenterm"
handoff_bead=""
handoff_touched_paths=()
handoff_avoided_paths=()
handoff_proof_commands=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --agent-mail-fallback)
      mode="agent_mail_fallback"
      shift
      ;;
    --agent-mail-handoff)
      mode="agent_mail_handoff"
      shift
      ;;
    --bead)
      if [ "$#" -lt 2 ]; then
        echo "--bead requires an issue id" >&2
        exit 64
      fi
      handoff_bead="$2"
      shift 2
      ;;
    --touched-path)
      if [ "$#" -lt 2 ]; then
        echo "--touched-path requires a path" >&2
        exit 64
      fi
      handoff_touched_paths+=("$2")
      shift 2
      ;;
    --avoided-path)
      if [ "$#" -lt 2 ]; then
        echo "--avoided-path requires a path" >&2
        exit 64
      fi
      handoff_avoided_paths+=("$2")
      shift 2
      ;;
    --proof-command)
      if [ "$#" -lt 2 ]; then
        echo "--proof-command requires a command string" >&2
        exit 64
      fi
      handoff_proof_commands+=("$2")
      shift 2
      ;;
    --help|-h)
      cat <<'EOF'
Usage:
  swarm-tick.sh [session]
  swarm-tick.sh --agent-mail-fallback [session]
  swarm-tick.sh --agent-mail-handoff --bead <id> [--touched-path PATH ...]
                [--avoided-path PATH ...] [--proof-command CMD ...] [session]

Emit a read-only operator snapshot for the frankenterm swarm.

`--agent-mail-handoff` prints a Markdown Beads comment block for review. It
does not post the comment, repair Agent Mail, or mutate Beads.
EOF
      exit 0
      ;;
    --)
      shift
      if [ "$#" -gt 0 ]; then
        session="$1"
        shift
      fi
      ;;
    -*)
      echo "unknown option: $1" >&2
      exit 64
      ;;
    *)
      session="$1"
      shift
      ;;
  esac
done

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

now="${FT_OPERATOR_NOW_ISO:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"

json_array_count_or_zero() {
  local output
  output="$("$@" 2>/dev/null || true)"
  if [ -n "$output" ]; then
    printf '%s\n' "$output" | jq 'if type == "array" then length else 0 end' 2>/dev/null || echo 0
  else
    echo 0
  fi
}

beads_issue_array() {
  local output
  output=$(cd "$repo_root" && "$@" 2>/dev/null || true)
  if [ -n "$output" ]; then
    printf '%s\n' "$output" | jq '
      if type == "object" and (.issues | type) == "array" then
        .issues
      elif type == "array" then
        .
      else
        []
      end
    ' 2>/dev/null || printf '[]\n'
  else
    printf '[]\n'
  fi
}

git_dirty_paths_json() {
  cd "$repo_root" && git status --short --untracked-files=all 2>/dev/null | jq -Rcs '
    split("\n")
    | map(select(length > 0)
      | {
          raw: .,
          status: .[0:2],
          path: (.[3:] | sub(" -> .*"; ""))
        })
  ' 2>/dev/null || printf '[]\n'
}

json_array_from_args() {
  if [ "$#" -eq 0 ]; then
    printf '[]\n'
  else
    printf '%s\n' "$@" | jq -R . | jq -s .
  fi
}

emit_agent_mail_fallback_snapshot() {
  local now_epoch
  now_epoch="${FT_OPERATOR_NOW_EPOCH:-$(date -u +%s)}"

  local in_progress_json ready_json dirty_json agent_mail_json
  in_progress_json=$(beads_issue_array br list --status in_progress --json)
  ready_json=$(beads_issue_array br ready --json)
  dirty_json=$(git_dirty_paths_json)
  agent_mail_json=$(agent_mail_failover_classify_json "${FT_AGENT_MAIL_FAILURE_CLASS:-api_unreachable}")

  jq -cn \
    --arg ts "$now" \
    --arg session "$session" \
    --argjson now_epoch "$now_epoch" \
    --argjson in_progress "$in_progress_json" \
    --argjson ready "$ready_json" \
    --argjson dirty "$dirty_json" \
    --argjson agent_mail "$agent_mail_json" \
    '
    def parse_bead_ts:
      if . == null then
        null
      else
        (sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601? // null)
      end;

    def enriched_issue:
      . as $issue
      | (($issue.updated_at // $issue.created_at // null) | parse_bead_ts) as $updated_epoch
      | {
          id: $issue.id,
          title: $issue.title,
          status: ($issue.status // null),
          priority: ($issue.priority // null),
          assignee: ($issue.assignee // "unassigned"),
          updated_at: ($issue.updated_at // null),
          age_seconds: (if $updated_epoch == null then null else (($now_epoch - $updated_epoch) | floor) end),
          stale_over_2h: (if $updated_epoch == null then false else (($now_epoch - $updated_epoch) >= 7200) end)
        };

    def dirty_category:
      if (.path == ".beads/issues.jsonl" or (.path | startswith(".beads/"))) then
        "shared_tracker"
      elif (.path | startswith(".stash_janitor_workspace/")) then
        "janitor_untracked"
      elif .status == "??" then
        "untracked_review_required"
      else
        "tracked_overlap_risk"
      end;

    def dirty_severity:
      dirty_category as $category
      | if $category == "shared_tracker" then
          "high"
        elif $category == "tracked_overlap_risk" then
          "high"
        elif $category == "untracked_review_required" then
          "medium"
        else
          "low"
        end;

    def dirty_guidance:
      dirty_category as $category
      | if $category == "shared_tracker" then
          "Shared Beads tracker is dirty; coordinate before staging .beads and avoid bundling unrelated issue updates."
        elif $category == "tracked_overlap_risk" then
          "Tracked file already has local changes; treat as another active pane work item until ownership is known."
        elif $category == "janitor_untracked" then
          "Untracked janitor artifact; leave untouched unless you own the cleanup lane."
        else
          "Untracked path needs ownership review before editing or staging."
        end;

    def enriched_dirty_path:
      . + {
        category: dirty_category,
        severity: dirty_severity
      };

    def active_agents:
      map(enriched_issue)
      | sort_by(.assignee, .id)
      | group_by(.assignee)
      | map({
          assignee: .[0].assignee,
          active_count: length,
          beads: map({
            id,
            title,
            updated_at,
            age_seconds,
            stale_over_2h
          })
        });

    def stale_reopen_guidance($dirty_enriched; $risk_level; $high_risk_count):
      map(enriched_issue) as $issues
      | {
          default_action: "do_not_reopen",
          threshold_seconds: 7200,
          dirty_risk_level: $risk_level,
          high_risk_dirty_count: $high_risk_count,
          dirty_tree_guard: "Do not reopen a bead when dirty tracked/shared files may belong to that assignee or overlap the bead; comment for status first.",
          manual_checks: [
            "br show <id> --json: confirm no recent comments or ownership handoff",
            "scripts/swarm-tick.sh --agent-mail-fallback frankenterm: confirm the bead remains stale in the latest snapshot",
            "git status --short --untracked-files=all: confirm no dirty paths overlap expected files for the bead",
            "If Agent Mail recovers, ask or acknowledge the assignee before reopening"
          ],
          active_not_stale: (
            $issues
            | map(select(.stale_over_2h | not)
              | {
                  id,
                  title,
                  assignee,
                  updated_at,
                  age_seconds,
                  recommendation: "do_not_reopen",
                  reason: "Updated inside the stale threshold; treat as active while Agent Mail is unavailable."
                })
          ),
          candidates: (
            $issues
            | map(select(.stale_over_2h)
              | {
                  id,
                  title,
                  assignee,
                  updated_at,
                  age_seconds,
                  recommendation: "status_check_before_reopen",
                  reason: "Stale threshold exceeded, but red-mail mode cannot prove abandonment from age alone.",
                  required_evidence: [
                    "No recent br comments or handoff",
                    "Latest fallback snapshot still marks the bead stale",
                    "Dirty paths do not overlap expected files for the bead",
                    "Assignee is unreachable or explicitly inactive"
                  ],
                  status_check_command: ("br comments add " + .id + " --author <agent> --message \"status check: still active? Agent Mail is unavailable; please comment if this bead is still owned.\""),
                  reopen_command: ("br update " + .id + " --status open --assignee \"\" --actor <agent>")
                })
          ),
          dirty_overlap_unknown: (
            $dirty_enriched
            | map(select(.severity == "high" or .severity == "medium")
              | {
                  path,
                  status,
                  category,
                  severity,
                  recommendation: "do_not_reopen_related_beads_until_owner_clear"
                })
          )
        };

    ($dirty | map(enriched_dirty_path)) as $dirty_enriched
    | ($dirty_enriched | map(select(.severity == "high")) | length) as $high_risk_count
    | ($dirty_enriched | map(select(.severity == "medium")) | length) as $medium_risk_count
    | ($dirty_enriched | map(select(.status != "??")) | length) as $tracked_dirty_count
    | ($dirty_enriched | map(select(.status == "??")) | length) as $untracked_dirty_count
    | (if $high_risk_count > 0 then
         "high"
       elif $medium_risk_count > 0 then
         "medium"
       elif ($dirty_enriched | length) > 0 then
         "low"
       else
         "clean"
       end) as $risk_level
    | (if $high_risk_count > 0 then
         "tracked or shared coordination files are already dirty"
       elif $medium_risk_count > 0 then
         "only untracked review-required paths are dirty"
       elif ($dirty_enriched | length) > 0 then
         "only low-risk janitor artifacts are dirty"
       else
         "worktree is clean"
       end) as $risk_reason
    |
    {
      ts: $ts,
      session: $session,
      mode: (if $agent_mail.status == "available" then "agent_mail_available" else "agent_mail_unavailable_beads_only" end),
      agent_mail: $agent_mail,
      beads: {
        in_progress_count: ($in_progress | length),
        ready_count: ($ready | length),
        active_agents: ($in_progress | active_agents),
        in_progress: ($in_progress | map(enriched_issue)),
        stale_reopen: ($in_progress | stale_reopen_guidance($dirty_enriched; $risk_level; $high_risk_count)),
        ready: ($ready | map({
          id,
          title,
          status: (.status // "ready"),
          priority: (.priority // null),
          assignee: (.assignee // "unassigned")
        }))
      },
      git: {
        dirty_count: ($dirty_enriched | length),
        tracked_dirty_count: $tracked_dirty_count,
        untracked_dirty_count: $untracked_dirty_count,
        high_risk_count: $high_risk_count,
        risk_level: $risk_level,
        risk_reason: $risk_reason,
        dirty_domains: (
          $dirty_enriched
          | sort_by(.category)
          | group_by(.category)
          | map({
              category: .[0].category,
              severity: .[0].severity,
              count: length,
              paths: map(.path)
            })
        ),
        dirty_paths: $dirty_enriched,
        conflict_hints: ($dirty_enriched | map({
          path,
          status,
          category,
          severity,
          guidance: dirty_guidance
        }))
      },
      next_actions: [
        "Use Beads status as the coordination source of truth until Agent Mail recovers.",
        "Before editing, compare dirty_paths and in_progress assignees with your intended files.",
        "Use beads.stale_reopen before reopening any in-progress bead; default to do_not_reopen.",
        "Record this snapshot in the Beads comment when closing or handing off work."
      ],
      proof_doctor: "not applicable; coordination snapshot only; no Cargo/RCH proof lane claimed."
    }'
}

emit_agent_mail_handoff_block() {
  if [ -z "$handoff_bead" ]; then
    echo "--agent-mail-handoff requires --bead <id>" >&2
    return 64
  fi

  local snapshot_json touched_json avoided_json proof_json
  snapshot_json="$(emit_agent_mail_fallback_snapshot)"
  touched_json="$(json_array_from_args "${handoff_touched_paths[@]}")"
  avoided_json="$(json_array_from_args "${handoff_avoided_paths[@]}")"
  proof_json="$(json_array_from_args "${handoff_proof_commands[@]}")"

  jq -r \
    --arg bead "$handoff_bead" \
    --argjson touched "$touched_json" \
    --argjson avoided "$avoided_json" \
    --argjson proof "$proof_json" \
    '
    def bullet_list($items):
      if ($items | length) == 0 then
        "- not provided"
      else
        ($items | map("- " + .) | join("\n"))
      end;

    def active_agent_lines:
      if (.beads.active_agents | length) == 0 then
        "- none"
      else
        .beads.active_agents
        | map(
            "- " + .assignee + ": " +
            (.beads
             | map(.id + " (" + (if .stale_over_2h then "stale_over_2h" else "active_not_stale" end) + ", age_seconds=" + (.age_seconds | tostring) + ")")
             | join(", "))
          )
        | join("\n")
      end;

    def active_not_stale_lines:
      if (.beads.stale_reopen.active_not_stale | length) == 0 then
        "- none"
      else
        .beads.stale_reopen.active_not_stale
        | map("- " + .id + ": " + .recommendation + " for " + .assignee + " (age_seconds=" + (.age_seconds | tostring) + ")")
        | join("\n")
      end;

    def stale_candidate_lines:
      if (.beads.stale_reopen.candidates | length) == 0 then
        "- none"
      else
        .beads.stale_reopen.candidates
        | map("- " + .id + ": " + .recommendation + " for " + .assignee + " (age_seconds=" + (.age_seconds | tostring) + ")")
        | join("\n")
      end;

    def dirty_overlap_lines:
      if (.beads.stale_reopen.dirty_overlap_unknown | length) == 0 then
        "- none"
      else
        .beads.stale_reopen.dirty_overlap_unknown
        | map("- " + .path + " [" + .category + "/" + .severity + "]: " + .recommendation)
        | join("\n")
      end;

    [
      "Red-mail Beads handoff for " + $bead,
      "",
      "Agent Mail: " + .agent_mail.status + " - " + (.agent_mail.error_summary // (.agent_mail.reason_codes | join(","))),
      "Snapshot: " + .ts + " session=" + .session + " ready_count=" + (.beads.ready_count | tostring),
      "",
      "Active assignees:",
      active_agent_lines,
      "",
      "Stale/non-stale classification:",
      "- default_action: " + .beads.stale_reopen.default_action,
      "- threshold_seconds: " + (.beads.stale_reopen.threshold_seconds | tostring),
      "",
      "Active, do not reopen:",
      active_not_stale_lines,
      "",
      "Stale candidates requiring status check:",
      stale_candidate_lines,
      "",
      "Dirty risk:",
      "- risk_level: " + .git.risk_level,
      "- risk_reason: " + .git.risk_reason,
      "- tracked_dirty_count: " + (.git.tracked_dirty_count | tostring),
      "- untracked_dirty_count: " + (.git.untracked_dirty_count | tostring),
      "- high_risk_count: " + (.git.high_risk_count | tostring),
      "",
      "Dirty overlap unknown:",
      dirty_overlap_lines,
      "",
      "Touched paths:",
      bullet_list($touched),
      "",
      "Avoided paths:",
      bullet_list($avoided),
      "",
      "Proof commands actually run:",
      bullet_list($proof),
      "",
      "Closure basis: use only the proof commands above plus Beads state. Sync chatter, transfer logs, and code presence alone are not proof.",
      "",
      "Review this block before posting. Suggested command:",
      "br comments add " + $bead + " --author <agent> --file <reviewed-handoff.md>"
    ]
    | join("\n")
    ' <<< "$snapshot_json"
}

if [ "$mode" = "agent_mail_fallback" ]; then
  emit_agent_mail_fallback_snapshot
  exit 0
fi

if [ "$mode" = "agent_mail_handoff" ]; then
  emit_agent_mail_handoff_block
  exit $?
fi

git_commits_1h=$(cd "$repo_root" && git log --since="1 hour ago" --oneline 2>/dev/null | wc -l | tr -d ' ')
git_commits_4m=$(cd "$repo_root" && git log --since="4 minutes ago" --oneline 2>/dev/null | wc -l | tr -d ' ')

beads_open=$(cd "$repo_root" && br list --status open --json 2>/dev/null | jq '.issues|length' 2>/dev/null || echo 0)
beads_in_progress=$(cd "$repo_root" && br list --status in_progress --json 2>/dev/null | jq '.issues|length' 2>/dev/null || echo 0)
beads_blocked=$(cd "$repo_root" && br list --status blocked --json 2>/dev/null | jq '.issues|length' 2>/dev/null || echo 0)
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
