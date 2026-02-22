#!/usr/bin/env bash
# Install git hooks for frankenterm.
# Run this on every machine that has a clone of this repo.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOOK_SRC="${REPO_ROOT}/scripts/pre-commit-guard.sh"
HOOK_DST="${REPO_ROOT}/.git/hooks/pre-commit"

chmod +x "$HOOK_SRC"

if [ -f "$HOOK_DST" ] && [ ! -L "$HOOK_DST" ]; then
    echo "Backing up existing pre-commit hook to pre-commit.old"
    mv "$HOOK_DST" "${HOOK_DST}.old"
fi

ln -sf "../../scripts/pre-commit-guard.sh" "$HOOK_DST"
echo "Installed pre-commit guard: $HOOK_DST -> $HOOK_SRC"
