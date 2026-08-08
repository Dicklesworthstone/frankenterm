#!/bin/bash
# Dummy alt-screen script for E2E testing
# Enters alternate screen mode to test policy blocking
#
# Usage: ./dummy_alt_screen.sh [DURATION]
#
# Arguments:
#   DURATION - Seconds to stay in alt screen (default: 30)

set -euo pipefail

DURATION="${1:-30}"
CLEANED_UP=0

if [[ ! "$DURATION" =~ ^[0-9]+$ ]] || (( ${#DURATION} > 4 )); then
    echo "DURATION must be an integer from 1 through 7200" >&2
    exit 2
fi
DURATION=$((10#$DURATION))
if (( DURATION < 1 || DURATION > 7200 )); then
    echo "DURATION must be an integer from 1 through 7200" >&2
    exit 2
fi

cleanup() {
    if [[ "$CLEANED_UP" -eq 0 ]]; then
        CLEANED_UP=1
        printf '\033[?1049l'
        echo "Exited alternate screen mode."
    fi
}

stop_cleanly() {
    trap - INT TERM
    cleanup
    exit 0
}

trap cleanup EXIT
trap stop_cleanly INT TERM

echo "Entering alternate screen mode for ${DURATION}s..."
echo "This simulates vim/less/htop style full-screen apps"

# ANSI escape to enter alternate screen buffer
printf '\033[?1049h'

# Clear alt screen and show message
printf '\033[2J\033[H'
echo "=== ALTERNATE SCREEN MODE ==="
echo ""
echo "This pane is in alternate screen buffer."
echo "ft policy should block send_text to this pane."
echo ""
echo "Press Ctrl+C or wait ${DURATION}s to exit."

# Use one-second timer slices so INT/TERM cleanup is bounded and no timer child
# can outlive this fixture by more than one second.
DEADLINE=$((SECONDS + DURATION))
while (( SECONDS < DEADLINE )); do
    sleep 1 || true
done

cleanup
