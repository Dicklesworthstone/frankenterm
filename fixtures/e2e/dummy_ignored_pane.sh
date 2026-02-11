#!/bin/bash
# Dummy script for IGNORED pane in E2E exclude filter test
# Sets pane title to "IGNORED_PANE" (matching exclude rule) and prints SECRET_TOKEN
#
# Usage: ./dummy_ignored_pane.sh [SECRET_TOKEN] [COUNT]
#
# Arguments:
#   SECRET_TOKEN - Secret string that should NEVER appear in ft artifacts (default: SECRET_TOKEN_<timestamp>)
#   COUNT        - Number of lines to print (default: 50)
#
# The pane title "IGNORED_PANE" matches the exclude rule in config_pane_exclude.toml,
# so ft should NOT capture any output from this pane.

set -euo pipefail

SECRET_TOKEN="${1:-SECRET_TOKEN_$(date +%s)}"
COUNT="${2:-50}"

# Set WezTerm pane title to match exclude rule
# Uses OSC 0 (icon + title) and OSC 2 (title only) escape sequences
printf '\033]0;IGNORED_PANE\007'
printf '\033]2;IGNORED_PANE\007'

echo "=== IGNORED Pane (Should NOT Be Captured) ==="
echo "Title: IGNORED_PANE"
echo "Secret Token: $SECRET_TOKEN"
echo ""

for i in $(seq 1 "$COUNT"); do
    echo "Ignored Line $i: $SECRET_TOKEN - $(date +%H:%M:%S.%N)"
    sleep 0.02
done

echo ""
echo "Done (ignored): $SECRET_TOKEN"
echo "Total lines: $COUNT"

# Keep the pane alive for testing
echo "Pane will stay open for inspection..."
sleep 300
