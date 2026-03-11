#!/usr/bin/env bash
# pull-legacy-repos.sh — Pull latest code from all legacy terminal project repos.
# Intended to run daily via cron or launchd.
# Usage: bash scripts/pull-legacy-repos.sh

set -euo pipefail

REPOS=(
  "/dp/ghostty"
  "/dp/wezterm"
  "/dp/zellij"
  "/dp/rio"
)

LOG_FILE="${HOME}/.local/share/ft/legacy-repo-pull.log"
mkdir -p "$(dirname "$LOG_FILE")"

log() {
  echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] $*" | tee -a "$LOG_FILE"
}

log "=== Starting legacy repo pull ==="

for repo in "${REPOS[@]}"; do
  if [ ! -d "$repo/.git" ]; then
    log "SKIP $repo — not a git repo or doesn't exist"
    continue
  fi
  log "Pulling $repo ..."
  if git -C "$repo" pull --ff-only 2>&1 | tee -a "$LOG_FILE"; then
    log "OK $repo"
  else
    log "WARN $repo — pull failed (possibly dirty or diverged), trying fetch-only"
    git -C "$repo" fetch --all 2>&1 | tee -a "$LOG_FILE" || true
  fi
done

log "=== Done ==="
