#!/bin/bash
# Dummy agent script for E2E testing
# Simulates an AI agent that triggers compaction and echoes received input
#
# Usage: ./dummy_agent.sh [DELAY_BEFORE_COMPACTION]
#
# Arguments:
#   DELAY_BEFORE_COMPACTION - Seconds to wait before emitting compaction marker (default: 1)

set -euo pipefail

DELAY="${1:-1}"

echo "[CODEX] Session started"
echo "[CODEX] Agent ready for work"

sleep "$DELAY"

echo "[CODEX] Compaction required: context window 95% full"
echo "[CODEX] Waiting for refresh prompt..."

# Wait for input and echo it back
# This simulates the agent receiving and processing user input
while IFS= read -r -t 30 line; do
    echo "Received: $line"
    if [[ "$line" == *"exit"* ]]; then
        echo "[CODEX] Exit requested, shutting down"
        break
    fi
    if [[ "$line" == *"refresh"* ]] || [[ "$line" == *"/compact"* ]]; then
        echo "[CODEX] Refresh acknowledged"
        echo "[CODEX] Context compacted successfully"
    fi
done

echo "[CODEX] Session ended"
