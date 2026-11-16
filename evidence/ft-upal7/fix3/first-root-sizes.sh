#!/bin/bash
# ft-upal7 fix B: the size of every pane's first checkpoint (its catalog generation 1 candidate)
# in one finished multi.sh sandbox, then their count and mean.
#   first-root-sizes.sh <sandbox>
set -euo pipefail
S=${1:?sandbox}
G=$S/h/.local/state/frankenterm-guardian-e2e/receiver/guardian-output-v3
echo "== $S"
/bin/ls -l "$G" | awk '/checkpoint-catalog-pane-.*generation-00000000000000000001\..*ftgccandidate/ {
        n++; s += $5; printf "  %10d %s\n", $5, $NF
    }
    END { printf "%d files, mean %.0f KiB\n", n, (n ? s / n / 1024 : 0) }'
