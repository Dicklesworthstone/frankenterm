#!/usr/bin/env bash
# pre-commit-guard.sh — Blocks commits that mass-delete files
#
# Install: ln -sf ../../scripts/pre-commit-guard.sh .git/hooks/pre-commit
# Or: Run scripts/install-hooks.sh
#
# This hook prevents the recurring disaster where agents delete
# crates/frankenterm-core (860+ files, 624K LOC) by "refactoring."

set -euo pipefail

# --- Rule 1: NEVER delete anything in crates/frankenterm-core/ ---
CORE_DELETIONS=$(git diff --cached --diff-filter=D --name-only -- 'crates/frankenterm-core/' 2>/dev/null | wc -l | tr -d ' ')
if [ "$CORE_DELETIONS" -gt 0 ]; then
    echo ""
    echo "=========================================="
    echo " BLOCKED: Deleting crates/frankenterm-core files"
    echo "=========================================="
    echo ""
    echo " This commit deletes $CORE_DELETIONS files from crates/frankenterm-core/."
    echo " This crate is PERMANENT and must NEVER be removed."
    echo ""
    echo " If files are missing from disk, restore them:"
    echo "   git checkout -- crates/frankenterm-core/"
    echo ""
    echo " To override (ONLY with explicit human approval):"
    echo "   git commit --no-verify"
    echo ""
    exit 1
fi

# --- Rule 2: Block commits deleting more than 50 files ---
TOTAL_DELETIONS=$(git diff --cached --diff-filter=D --name-only 2>/dev/null | wc -l | tr -d ' ')
if [ "$TOTAL_DELETIONS" -gt 50 ]; then
    echo ""
    echo "=========================================="
    echo " BLOCKED: Mass deletion ($TOTAL_DELETIONS files)"
    echo "=========================================="
    echo ""
    echo " This commit deletes $TOTAL_DELETIONS files."
    echo " Commits deleting >50 files require explicit human approval."
    echo ""
    echo " To override (ONLY with explicit human approval):"
    echo "   git commit --no-verify"
    echo ""
    exit 1
fi

# --- Chain to bd hook if present ---
if command -v bd >/dev/null 2>&1; then
    bd hooks run pre-commit "$@" 2>/dev/null || true
fi
