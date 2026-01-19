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

echo "Entering alternate screen mode for ${DURATION}s..."
echo "This simulates vim/less/htop style full-screen apps"

# ANSI escape to enter alternate screen buffer
printf '\033[?1049h'

# Clear alt screen and show message
printf '\033[2J\033[H'
echo "=== ALTERNATE SCREEN MODE ==="
echo ""
echo "This pane is in alternate screen buffer."
echo "wa policy should block send_text to this pane."
echo ""
echo "Press Ctrl+C or wait ${DURATION}s to exit."

# Sleep in alt screen
sleep "$DURATION" || true

# ANSI escape to exit alternate screen buffer
printf '\033[?1049l'

echo "Exited alternate screen mode."
