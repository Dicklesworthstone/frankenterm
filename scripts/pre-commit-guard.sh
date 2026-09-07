#!/usr/bin/env bash
# pre-commit-guard.sh — Blocks commits that mass-delete files
#
# Install: ln -sf ../../scripts/pre-commit-guard.sh .git/hooks/pre-commit
# Or: Run scripts/install-hooks.sh
#
# This hook prevents the recurring disaster where agents delete
# crates/frankenterm-core by "refactoring."

set -euo pipefail

# Rule 1: crates/frankenterm-core is permanent. No commit may delete from it.
CORE_DELETIONS=$(git diff --cached --diff-filter=D --name-only -- 'crates/frankenterm-core/' 2>/dev/null | wc -l | tr -d ' ')
if [ "$CORE_DELETIONS" -gt 0 ]; then
    echo ""
    echo "=========================================="
    echo " BLOCKED: Deleting crates/frankenterm-core files"
    echo "=========================================="
    echo ""
    echo " This commit deletes $CORE_DELETIONS files from crates/frankenterm-core/."
    echo " This crate is permanent and must never be removed."
    echo ""
    echo " Restore the missing files before committing."
    echo ""
    exit 1
fi

# Rule 2: large deletion batches require explicit human review.
TOTAL_DELETIONS=$(git diff --cached --diff-filter=D --name-only 2>/dev/null | wc -l | tr -d ' ')
if [ "$TOTAL_DELETIONS" -gt 50 ]; then
    echo ""
    echo "=========================================="
    echo " BLOCKED: Mass deletion ($TOTAL_DELETIONS files)"
    echo "=========================================="
    echo ""
    echo " This commit deletes $TOTAL_DELETIONS files."
    echo " Commits deleting more than 50 files require explicit human approval."
    echo ""
    exit 1
fi

# br has no `hooks run` subcommand. Export through its supported CLI without
# staging anything (including when the caller uses a private Git index).
# Keep export failures visible and fail the commit rather than hide stale tasks.
if command -v br >/dev/null 2>&1; then
    br sync --flush-only
fi
