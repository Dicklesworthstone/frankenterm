#!/usr/bin/env bash
# Clean stale build artifact dirs older than N hours.
#
# Usage:
#   clean-stale-targets.sh [hours]            (default: 12)
#   clean-stale-targets.sh --dry-run [hours]  (no deletions; reports would-remove)
#   DRY_RUN=1 clean-stale-targets.sh [hours]  (env-var form of --dry-run)
#
# Override target glob for tests:
#   TARGET_GLOB='/tmp/clean-stale-test-XXXX/ft-*-target' clean-stale-targets.sh ...
#   FT_OPERATOR_LOCK_DIR=/tmp/test-lock clean-stale-targets.sh ...
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
# Platform: macOS (uses `stat -f %m`). Linux portability tracked in
# ft-v5lz3.2.6.

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

dry_run=0
hours=""
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=1 ;;
    -h|--help)
      sed -n '2,/^$/p' "$0"
      exit 0
      ;;
    --*)
      echo "unknown flag: $arg" >&2
      exit 2
      ;;
    *)
      if [ -n "$hours" ]; then
        echo "unexpected extra arg: $arg" >&2
        exit 2
      fi
      hours="$arg"
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
# shellcheck disable=SC2206
candidates=( $target_glob )
shopt -u nullglob

if [ "${#candidates[@]}" -gt 0 ]; then
  before=$(/usr/bin/du -sk "${candidates[@]}" 2>/dev/null | awk '{s+=$1} END{print s}')
else
  before=0
fi

killed=0
would_kill=0
prefix=""
if [ "$dry_run" = "1" ]; then
  prefix="[dry-run] "
fi

now=$(date +%s)
for d in "${candidates[@]}"; do
  [ -d "$d" ] || continue
  mtime=$(stat -f %m "$d" 2>/dev/null || echo 0)
  age_min=$(( (now - mtime) / 60 ))
  if [ "$age_min" -gt "$threshold_min" ]; then
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
  echo "cleaned 0 dirs (would have cleaned ${would_kill}); KB before=${before:-0} after=${after:-0}"
else
  echo "cleaned $killed dirs; KB before=${before:-0} after=${after:-0}"
fi
