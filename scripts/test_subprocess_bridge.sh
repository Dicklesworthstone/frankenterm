#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
RUN_ID="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
LOG_DIR="${LOG_DIR:-$PROJECT_ROOT/target/e2e/subprocess-bridge}"
DEFAULT_CARGO_TARGET_DIR="target/rch-subprocess-bridge-${RUN_ID}"
REQUESTED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "$REQUESTED_CARGO_TARGET_DIR" && "$REQUESTED_CARGO_TARGET_DIR" != /* ]]; then
    CARGO_TARGET_DIR="$REQUESTED_CARGO_TARGET_DIR"
else
    CARGO_TARGET_DIR="$DEFAULT_CARGO_TARGET_DIR"
fi
export CARGO_TARGET_DIR

mkdir -p "$LOG_DIR"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$PROJECT_ROOT/tests/e2e/lib_rch_guards.sh"
RCH_SKIP_SMOKE_PREFLIGHT=1
rch_init "$LOG_DIR" "$RUN_ID" "scripts_test_subprocess_bridge" "$PROJECT_ROOT"
ensure_rch_ready

test_log="$LOG_DIR/subprocess_bridge_${RUN_ID}.rch.log"
run_rch_cargo_logged "$test_log" \
    env CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
    cargo test -p frankenterm-core --features subprocess-bridge --lib subprocess_bridge -- --nocapture
cat "$test_log"
echo "PASS: SubprocessBridge tests complete"
