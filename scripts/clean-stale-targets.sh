#!/usr/bin/env bash
# Clean stale build artifact dirs older than N hours.
#
# Usage:
#   clean-stale-targets.sh [hours]                                  (default: 12)
#   clean-stale-targets.sh --dry-run [hours]                        (no deletions; reports would-remove)
#   clean-stale-targets.sh --dry-run --threshold-hours <hours>      (documented runbook form)
#   clean-stale-targets.sh --dry-run --threshold-hours=<hours>      (equals form)
#   clean-stale-targets.sh --inventory --threshold-hours <hours>    (read-only size/age inventory)
#   clean-stale-targets.sh --inventory --format json [hours]        (machine-readable inventory)
#   DRY_RUN=1 clean-stale-targets.sh [hours]                        (env-var form of --dry-run)
#
# Override target glob for tests:
#   TARGET_GLOB='/tmp/clean-stale-test-XXXX/ft-*-target' clean-stale-targets.sh ...
#   FT_OPERATOR_LOCK_DIR=/tmp/test-lock clean-stale-targets.sh ...
#
# Concurrency safety (ft-v5lz3.2.8):
#   Before rm-ing a candidate dir, the script checks whether any process
#   has it open (cargo, rustc, ld) via `lsof +D <dir>`. If usage is
#   detected, the dir is SKIPPED with a "skipped (active usage)" log
#   line and the rest of the cleanup continues. If lsof is unavailable,
#   the script falls back to mtime: any file under the dir touched in
#   the last 5 minutes is treated as active work.
#
#   Test override: FT_TEST_FAKE_ACTIVE_DIRS — colon-separated list of
#   dir paths to treat as active (used by tests to deterministically
#   exercise the skip path without holding real FDs).
#
# Exit codes:
#   0  ran to completion (removed >=0 dirs, or dry-ran successfully)
#   2  invalid arguments
#
# AGENTS.md Rule 1 (no file deletion without permission) exception: this
# script ONLY deletes per-agent build cache directories matching the
# TARGET_GLOB pattern (default /tmp/ft-*-target). It must never touch
# the project repo. Tests use a hermetic temp directory via TARGET_GLOB.
#
# Platform: macOS + Linux. mtime read uses BSD `stat -f %m` on Darwin
# and GNU `stat -c %Y` elsewhere (see read_mtime_seconds below). All
# other syscalls in this script are POSIX-clean.

set -u

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
  printf '%s\n' "clean-stale-targets.sh" > "$lock_dir/name"
}

release_operator_lock() {
  local lock_dir="$1"
  if [ -f "$lock_dir/pid" ] && [ "$(cat "$lock_dir/pid" 2>/dev/null || true)" = "$$" ]; then
    rm -f "$lock_dir/pid" "$lock_dir/name" 2>/dev/null || true
    rmdir "$lock_dir" 2>/dev/null || true
  fi
}

# Read the mtime of a path as Unix-seconds. macOS ships BSD stat
# (`-f %m`); most Linux distros ship GNU coreutils stat (`-c %Y`).
# Branching on `uname` keeps both paths in a single script with no
# extra runtime dependency.
read_mtime_seconds() {
  local path="$1"
  case "$(uname -s)" in
    Darwin)
      stat -f %m "$path" 2>/dev/null || echo 0
      ;;
    *)
      stat -c %Y "$path" 2>/dev/null || echo 0
      ;;
  esac
}

# Returns 0 if the dir is in active use by another process; 1 otherwise.
# Concurrency safety check used before deleting a stale candidate.
active_usage() {
  local d="$1"

  # Test-mode override (deterministic, no FD juggling required).
  if [ -n "${FT_TEST_FAKE_ACTIVE_DIRS:-}" ]; then
    local item
    IFS=':' read -ra _fake_active <<< "$FT_TEST_FAKE_ACTIVE_DIRS"
    for item in "${_fake_active[@]}"; do
      if [ "$item" = "$d" ]; then
        return 0
      fi
    done
  fi

  if command -v lsof >/dev/null 2>&1; then
    # lsof exits non-zero when there is no output; redirect stderr to
    # silence "no file descriptors found" noise. We only care about
    # whether ANY line of stdout came back.
    if lsof +D "$d" 2>/dev/null | head -n 1 | grep -q .; then
      return 0
    fi
    return 1
  fi

  # lsof unavailable: fall back to mtime check.
  # Any file under the dir touched in the last 5 minutes counts as
  # active work. -mmin -5 returns matching paths; we just need one.
  if find "$d" -mmin -5 -type f -print -quit 2>/dev/null | grep -q .; then
    return 0
  fi
  return 1
}

path_size_kb() {
  local path="$1"
  /usr/bin/du -sk "$path" 2>/dev/null | awk 'NR == 1 {print $1 + 0} END {if (NR == 0) print 0}'
}

json_escape() {
  local s="$1"
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  s=${s//$'\n'/\\n}
  s=${s//$'\r'/\\r}
  s=${s//$'\t'/\\t}
  printf '%s' "$s"
}

emit_inventory() {
  local now="$1"
  local total=0
  local fresh=0
  local stale=0
  local active=0
  local reclaimable_kb=0
  local first=1

  if [ "$inventory_format" = "json" ]; then
    printf '{\n'
    printf '  "ok": true,\n'
    printf '  "mode": "inventory",\n'
    printf '  "target_glob": "%s",\n' "$(json_escape "$target_glob")"
    printf '  "threshold_hours": %s,\n' "$hours"
    printf '  "threshold_min": %s,\n' "$threshold_min"
    printf '  "candidates": [\n'
  else
    echo "inventory target_glob=$target_glob threshold_hours=$hours threshold_min=$threshold_min"
  fi

  for d in "${candidates[@]+"${candidates[@]}"}"; do
    [ -d "$d" ] || continue

    local mtime
    local age_min
    local size_kb
    local status
    local reason
    local candidate_reclaimable_kb=0

    total=$((total + 1))
    mtime=$(read_mtime_seconds "$d")
    age_min=$(( (now - mtime) / 60 ))
    size_kb=$(path_size_kb "$d")
    status="fresh"
    reason="below_threshold"

    if [ "$age_min" -gt "$threshold_min" ]; then
      if active_usage "$d"; then
        active=$((active + 1))
        status="active"
        reason="active_usage"
      else
        stale=$((stale + 1))
        status="stale"
        reason="older_than_threshold"
        candidate_reclaimable_kb="$size_kb"
        reclaimable_kb=$((reclaimable_kb + candidate_reclaimable_kb))
      fi
    else
      fresh=$((fresh + 1))
    fi

    if [ "$inventory_format" = "json" ]; then
      if [ "$first" = "1" ]; then
        first=0
      else
        printf ',\n'
      fi
      printf '    {"path":"%s","status":"%s","reason":"%s","age_min":%s,"size_kb":%s,"reclaimable_kb":%s}' \
        "$(json_escape "$d")" "$status" "$reason" "$age_min" "$size_kb" "$candidate_reclaimable_kb"
    else
      echo "candidate status=$status reason=$reason age_min=$age_min size_kb=$size_kb reclaimable_kb=$candidate_reclaimable_kb path=$d"
    fi
  done

  if [ "$inventory_format" = "json" ]; then
    printf '\n  ],\n'
    printf '  "summary": {"candidates":%s,"fresh":%s,"stale":%s,"active":%s,"reclaimable_kb":%s,"reclaimable_bytes":%s}\n' \
      "$total" "$fresh" "$stale" "$active" "$reclaimable_kb" "$((reclaimable_kb * 1024))"
    printf '}\n'
  else
    echo "inventory summary candidates=$total fresh=$fresh stale=$stale active=$active reclaimable_kb=$reclaimable_kb reclaimable_bytes=$((reclaimable_kb * 1024))"
  fi
}

dry_run=0
inventory=0
inventory_format="text"
hours=""
set_hours() {
  local value="$1"
  local source="$2"
  if [ -n "$hours" ]; then
    echo "threshold hours specified more than once: $source" >&2
    exit 2
  fi
  hours="$value"
}

set_inventory_format() {
  local value="$1"
  case "$value" in
    text|json)
      inventory_format="$value"
      ;;
    *)
      echo "inventory format must be text or json (got: $value)" >&2
      exit 2
      ;;
  esac
}

while [ "$#" -gt 0 ]; do
  arg="$1"
  shift
  case "$arg" in
    --dry-run) dry_run=1 ;;
    --inventory) inventory=1 ;;
    --inventory-json)
      inventory=1
      inventory_format="json"
      ;;
    --format)
      if [ "$#" -eq 0 ] || [[ "$1" == --* ]]; then
        echo "missing value for --format" >&2
        exit 2
      fi
      inventory=1
      set_inventory_format "$1"
      shift
      ;;
    --format=*)
      inventory=1
      set_inventory_format "${arg#--format=}"
      ;;
    --threshold-hours)
      if [ "$#" -eq 0 ] || [[ "$1" == --* ]]; then
        echo "missing value for --threshold-hours" >&2
        exit 2
      fi
      set_hours "$1" "--threshold-hours"
      shift
      ;;
    --threshold-hours=*)
      set_hours "${arg#--threshold-hours=}" "--threshold-hours"
      ;;
    -h|--help)
      sed -n '2,/^$/p' "$0"
      exit 0
      ;;
    --*)
      echo "unknown flag: $arg" >&2
      exit 2
      ;;
    *)
      set_hours "$arg" "positional hours"
      ;;
  esac
done

if [ "${DRY_RUN:-0}" = "1" ]; then
  dry_run=1
fi

hours="${hours:-12}"
case "$hours" in
  ''|*[!0-9]*)
    echo "hours must be a non-negative integer (got: $hours)" >&2
    exit 2
    ;;
esac
threshold_min=$((hours * 60))

target_glob="${TARGET_GLOB:-/tmp/ft-*-target}"

acquire_operator_lock "$operator_lock_dir" || exit $?
trap 'release_operator_lock "$operator_lock_dir"' EXIT
trap 'release_operator_lock "$operator_lock_dir"; exit 130' INT
trap 'release_operator_lock "$operator_lock_dir"; exit 143' TERM

# Expand glob in current shell. If nothing matches under nullglob, the array
# stays empty so the for-loop does nothing.
shopt -s nullglob
declare -a candidates=()
# shellcheck disable=SC2206
candidates=( $target_glob )
shopt -u nullglob

if [ "$inventory" = "1" ]; then
  emit_inventory "$(date +%s)"
  exit 0
fi

if [ "${#candidates[@]}" -gt 0 ]; then
  before=$(/usr/bin/du -sk "${candidates[@]}" 2>/dev/null | awk '{s+=$1} END{print s}')
else
  before=0
fi

killed=0
would_kill=0
skipped=0
prefix=""
if [ "$dry_run" = "1" ]; then
  prefix="[dry-run] "
fi

now=$(date +%s)
for d in "${candidates[@]+"${candidates[@]}"}"; do
  [ -d "$d" ] || continue
  mtime=$(read_mtime_seconds "$d")
  age_min=$(( (now - mtime) / 60 ))
  if [ "$age_min" -gt "$threshold_min" ]; then
    if active_usage "$d"; then
      skipped=$((skipped + 1))
      echo "${prefix}skipped $d (active usage)"
      continue
    fi
    if [ "$dry_run" = "1" ]; then
      would_kill=$((would_kill + 1))
      echo "${prefix}would-remove $d (age=${age_min}m)"
    else
      if rm -rf "$d"; then
        killed=$((killed + 1))
        echo "removed $d (age=${age_min}m)"
      fi
    fi
  fi
done

if [ "${#candidates[@]}" -gt 0 ]; then
  # Some candidates may have just been removed; du still returns 0 for missing
  # paths so this is fine.
  after=$(/usr/bin/du -sk "${candidates[@]}" 2>/dev/null | awk '{s+=$1} END{print s}')
else
  after=0
fi

if [ "$dry_run" = "1" ]; then
  echo "cleaned 0 dirs (would have cleaned ${would_kill}, skipped ${skipped}); KB before=${before:-0} after=${after:-0}"
else
  echo "cleaned $killed dirs (skipped ${skipped}); KB before=${before:-0} after=${after:-0}"
fi
