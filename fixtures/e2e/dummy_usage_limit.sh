#!/bin/bash
# Dummy agent script for E2E usage-limit testing
# Emits a Codex usage-limit line, then waits for Ctrl-C to print a session summary.
#
# Usage: ./dummy_usage_limit.sh [DELAY_BEFORE_LIMIT] [RESET_TIME] [SESSION_ID]
#
# Arguments:
#   DELAY_BEFORE_LIMIT - Seconds to wait before emitting usage-limit marker (default: 1)
#   RESET_TIME - Human-readable reset time string (default: "2026-02-01 00:00 UTC")
#   SESSION_ID - Resume session ID (default: fixed UUID-like value)

set -euo pipefail

DELAY="${1:-1}"
RESET_TIME="${2:-2026-02-01 00:00 UTC}"
SESSION_ID="${3:-123e4567-e89b-12d3-a456-426614174000}"

emit_summary() {
    echo "Token usage: total=42 input=20 output=22"
    echo "codex resume ${SESSION_ID}"
    echo "You've hit your usage limit. try again at ${RESET_TIME}."
}

trap 'emit_summary; exit 0' INT

echo "[CODEX] Session started"
echo "[CODEX] Agent ready for work"

sleep "$DELAY"

# Usage-limit marker (matches codex.usage.reached anchor + regex)
echo "You've hit your usage limit. try again at ${RESET_TIME}."
echo "[CODEX] Waiting for operator action..."

# Keep pane alive for Ctrl-C
while true; do
    sleep 1
done
