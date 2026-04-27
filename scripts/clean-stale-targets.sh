#!/usr/bin/env bash
# Clean stale /tmp/ft-*-target build artifact dirs older than N hours.
# Usage: clean-stale-targets.sh [hours] (default: 12)

hours="${1:-12}"
threshold_min=$((hours * 60))

before=$(/usr/bin/du -sk /tmp/ft-*-target 2>/dev/null | awk '{s+=$1} END{print s}')
killed=0
for d in /tmp/ft-*-target; do
  [ -d "$d" ] || continue
  age_min=$(( ($(date +%s) - $(stat -f %m "$d" 2>/dev/null || echo 0)) / 60 ))
  if [ "$age_min" -gt "$threshold_min" ]; then
    rm -rf "$d" && killed=$((killed + 1)) && echo "removed $d (age=${age_min}m)"
  fi
done
after=$(/usr/bin/du -sk /tmp/ft-*-target 2>/dev/null | awk '{s+=$1} END{print s}')
echo "cleaned $killed dirs; KB before=${before:-0} after=${after:-0}"
