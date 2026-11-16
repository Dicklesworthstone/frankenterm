#!/bin/bash
# Dummy burst script for stress testing
# Emits COUNT lines as fast as possible with a marker.
#
# Usage: ./dummy_burst.sh [COUNT] [MARKER]

set -euo pipefail

COUNT="${1:-100000}"
MARKER="${2:-E2E_STRESS_MARKER}"

if [[ ! "$COUNT" =~ ^[0-9]+$ ]] || (( ${#COUNT} > 7 )); then
    echo "COUNT must be an integer from 1 through 1000000" >&2
    exit 2
fi
COUNT_NUM=$((10#$COUNT))
if (( COUNT_NUM < 1 || COUNT_NUM > 1000000 )); then
    echo "COUNT must be an integer from 1 through 1000000" >&2
    exit 2
fi
if (( ${#MARKER} > 128 )); then
    echo "MARKER must not exceed 128 bytes" >&2
    exit 2
fi

i=1
while (( i <= COUNT_NUM )); do
    printf "Line %d: %s\n" "$i" "$MARKER"
    i=$((i + 1))
done
