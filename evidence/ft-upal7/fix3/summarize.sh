#!/bin/bash
# ft-upal7 A/B: for one finished multi.sh sandbox, every recovery publication and the guardian
# checkpoint worker's time per operation, over the whole run and from the mux restart on.
#   summarize.sh <sandbox>
# The windows compare UTC times of day, so a run must not cross 00:00 UTC (none of the A/B runs
# did: they ran 01:55 to 03:16 UTC on 2026-11-07).
set -u
S=${1:?sandbox}
ST=$S/h/.local/state/frankenterm-guardian-e2e/logs
ML=$ST/mux-server.log
GL=$ST/pty-guardian.log
echo "== $S"
sed 's/\x1b\[[0-9;]*m//g' "$ML" | grep -a 'published local generation\|restored [0-9]* guardian panes' \
    | sed -E 's/^\[([^ ]*) [A-Z]+ +[^]]*\] /\1 /' | cut -c1-150
# The restart: the first "restored" line's time, as HH:MM:SS.
restart=$(sed 's/\x1b\[[0-9;]*m//g' "$ML" | grep -a -m1 'restored [0-9]* guardian panes' | cut -c13-20)
for window in "all 00:00:00" "after-restart ${restart:-99:99:99}"; do
    set -- $window
    grep -a 'checkpoint_timing' "$GL" | awk -v name="$1" -v from="$2" '
        { t = substr($1, 13, 8); if (t < from) next; op = $4; us = $6; n[op]++; s[op] += us; tot += us }
        END {
            printf "  %s (from %s): busy %.1f s\n", name, from, tot / 1e6
            for (o in n) printf "    %-16s n=%6d total=%8.1f s mean=%8.0f us\n", o, n[o], s[o] / 1e6, s[o] / n[o]
        }'
done
