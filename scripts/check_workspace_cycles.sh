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

RUN_ID="${FT_WORKSPACE_CYCLES_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")-$$}"
ARTIFACT_DIR="${FT_WORKSPACE_CYCLES_ARTIFACT_DIR:-target/workspace-cycle-guard}"
DEFAULT_TARGET_DIR="target/rch-workspace-cycles-${RUN_ID}"
REQUESTED_TARGET_DIR="${FT_WORKSPACE_CYCLES_TARGET_DIR:-${CARGO_TARGET_DIR:-}}"
if [[ -n "${REQUESTED_TARGET_DIR}" && "${REQUESTED_TARGET_DIR}" != /* ]]; then
    RCH_CARGO_TARGET_DIR="${REQUESTED_TARGET_DIR}"
else
    RCH_CARGO_TARGET_DIR="${DEFAULT_TARGET_DIR}"
fi

if ! command -v jq >/dev/null 2>&1; then
    echo "ft-94juo guard: jq not found on PATH; RCH metadata artifacts require it." >&2
    exit 2
fi

mkdir -p "${ARTIFACT_DIR}"

RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
RCH_STEP_TIMEOUT_SECS="${FT_WORKSPACE_CYCLES_RCH_TIMEOUT_SECS:-${RCH_STEP_TIMEOUT_SECS:-600}}"
# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${REPO_ROOT}/tests/e2e/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "workspace_cycles" "${REPO_ROOT}"
ensure_rch_ready

# `--no-deps` keeps the metadata small (workspace members only, no
# transitive dep graph). The DFS only needs intra-workspace edges.
cycle_log="${ARTIFACT_DIR}/workspace-cycles-${RUN_ID}.rch.log"
set +e
run_rch_cargo_logged "${cycle_log}" \
    env CARGO_TARGET_DIR="${RCH_CARGO_TARGET_DIR}" \
    bash -lc 'cargo metadata --format-version=1 --no-deps | python3 scripts/check_workspace_cycles.py'
rc=$?
set -e

cat "${cycle_log}"
exit "${rc}"
