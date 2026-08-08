#!/bin/bash
# Dummy agent script for E2E testing
# Simulates an AI agent that triggers compaction and echoes received input
#
# Usage: ./dummy_agent.sh [DELAY_BEFORE_COMPACTION] [REPEAT_COUNT] [REPEAT_INTERVAL] [READ_TIMEOUT]
#
# Arguments:
#   DELAY_BEFORE_COMPACTION - Seconds to wait before emitting compaction marker (default: 1)
#   REPEAT_COUNT - How many times to emit the compaction marker (default: 1)
#   REPEAT_INTERVAL - Seconds between repeated markers (default: 1)
#   READ_TIMEOUT - Seconds of input inactivity before clean exit (default: 30)

set -euo pipefail

DELAY="${1:-1}"
REPEAT_COUNT="${2:-1}"
REPEAT_INTERVAL="${3:-1}"
READ_TIMEOUT="${4:-30}"

parse_bounded_integer() {
    local name="$1"
    local value="$2"
    local minimum="$3"
    local maximum="$4"
    if [[ ! "$value" =~ ^[0-9]+$ ]] || (( ${#value} > 7 )); then
        echo "$name must be an integer from $minimum through $maximum" >&2
        exit 2
    fi
    REPLY=$((10#$value))
    if (( REPLY < minimum || REPLY > maximum )); then
        echo "$name must be an integer from $minimum through $maximum" >&2
        exit 2
    fi
}

parse_bounded_integer DELAY_BEFORE_COMPACTION "$DELAY" 0 60
DELAY="$REPLY"
parse_bounded_integer REPEAT_COUNT "$REPEAT_COUNT" 1 100
REPEAT_COUNT="$REPLY"
parse_bounded_integer REPEAT_INTERVAL "$REPEAT_INTERVAL" 0 60
REPEAT_INTERVAL="$REPLY"
parse_bounded_integer READ_TIMEOUT "$READ_TIMEOUT" 1 7200
READ_TIMEOUT="$REPLY"

LIFETIME=$((DELAY + (REPEAT_COUNT - 1) * REPEAT_INTERVAL + READ_TIMEOUT))
if (( LIFETIME > 7200 )); then
    echo "combined dummy-agent lifetime must not exceed 7200 seconds" >&2
    exit 2
fi

echo "[CODEX] Session started"
echo "[CODEX] Agent ready for work"

sleep "$DELAY"

echo "[CODEX] Compaction required: context window 95% full"
echo "[CODEX] Waiting for refresh prompt..."

if [[ "$REPEAT_COUNT" -gt 1 ]]; then
    for ((i=2; i<=REPEAT_COUNT; i++)); do
        sleep "$REPEAT_INTERVAL"
        echo "[CODEX] Compaction required: context window 95% full"
        echo "[CODEX] Waiting for refresh prompt..."
    done
fi

# Wait for input and echo it back
# This simulates the agent receiving and processing user input
while IFS= read -r -t "$READ_TIMEOUT" line; do
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
