#!/bin/bash
# Dummy print script for E2E testing
# Emits N lines with a unique marker for search testing
#
# Usage: ./dummy_print.sh [MARKER] [COUNT]
#
# Arguments:
#   MARKER - Unique string to search for (default: E2E_MARKER_<timestamp>)
#   COUNT  - Number of lines to print (default: 100)

set -euo pipefail

MARKER="${1:-E2E_MARKER_$(date +%s)}"
COUNT="${2:-100}"

echo "=== E2E Dummy Print Script ==="
echo "Marker: $MARKER"
echo "Count: $COUNT"
echo ""

for i in $(seq 1 "$COUNT"); do
    echo "Line $i: $MARKER - $(date +%H:%M:%S.%N)"
    sleep 0.01
done

echo ""
echo "Done: $MARKER"
echo "Total lines: $COUNT"
