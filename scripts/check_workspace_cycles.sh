#!/usr/bin/env bash
# ft-94juo — CI guard preventing new sub-crate cycles in the workspace
# dependency graph.
#
# During the ft-y0loj.* sub-crate split (April 2026), three cargo cycles
# were caught only when a developer ran `cargo check` manually:
#
#   - ft-y0loj.3 (fleet): full extract failed; partial-extracted
#     fleet_dashboard only (commit dd3e98fa).
#   - ft-t2d70 (mcp/connector): 3-file slice triggered `cyclic package
#     dependency` from frankenterm-core-mcp's path-deps on
#     Config/Error/Policy. Reverted; PARK ADR shipped.
#   - ft-j1qjt (replay tier-1): blocked extraction reverted in baef663e
#     ("not a tier-1 leaf").
#
# Each cycle was a load-bearing learning, but each was caught LATE — only
# after a `cargo check` round-trip. This guard runs BEFORE compile in
# CI and surfaces the cycle with the full dep-edge trail, so the next
# extraction attempt fails fast in the PR check.
#
# Mechanics: `cargo metadata --format-version=1 --no-deps` emits the
# workspace dep graph as JSON. We parse it in Python, build an
# adjacency list scoped to workspace members (skipping crates.io and
# git deps), and run a DFS cycle detector. On hit: print every edge in
# the cycle and exit 1. On clean: print a one-line summary and exit 0.
#
# Why a separate script + CI step (vs. relying on `cargo check`'s
# cycle detector): cargo's error message names ONE pair of edges; this
# guard surfaces the FULL cycle, which is what you actually need to
# decide which edge to break.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

if ! command -v cargo >/dev/null 2>&1; then
    echo "ft-94juo guard: cargo not found on PATH; cannot inspect workspace metadata." >&2
    exit 2
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "ft-94juo guard: python3 not found on PATH; the cycle detector needs it." >&2
    exit 2
fi

# `--no-deps` keeps the metadata small (workspace members only, no
# transitive dep graph). The DFS only needs intra-workspace edges.
cargo metadata --format-version=1 --no-deps | python3 "${SCRIPT_DIR}/check_workspace_cycles.py"
