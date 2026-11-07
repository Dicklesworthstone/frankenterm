#!/bin/bash
# Aggregate the ft-upal7 fix 3 profiles into profile/ with sample_agg.py and sample_callers.py.
# Kept as run, from the scratch directory this evidence was assembled in: the raw `sample` files
# it reads ($P) are not committed. The mux capture threads are named by their sample headers'
# thread ids. Writes only new files; refuses to replace one.
set -euo pipefail
cd /private/tmp/claude-501/-Users-jemanuel-projects-frankenterm/e42e584b-203c-42fb-b81d-e913119edb0c/scratchpad/retain/ev-upal7-fix3b
set -o noclobber
P=/private/tmp/claude-501/-Users-jemanuel-projects-frankenterm/e42e584b-203c-42fb-b81d-e913119edb0c/scratchpad/upal7/prof
python3 -I sample_agg.py "$P/gsample-1.txt" ft-guardian-checkpoint 45 > profile/guardian-first-root-checkpoint-worker.txt
python3 -I sample_agg.py "$P/gsample-2.txt" ft-guardian-checkpoint 45 > profile/guardian-full-size-checkpoint-worker.txt
python3 -I sample_callers.py "$P/gsample-2.txt" ft-guardian-checkpoint checkpoint_catalog_reread_published_candidate 3 6 > profile/guardian-full-size-reread-callers.txt
python3 -I sample_agg.py "$P/m-mux-1.txt" Thread_1501280399 45 > profile/mux-first-root-capture-thread.txt
python3 -I sample_agg.py "$P/m-mux-1.txt" mux-parse-pane- 60 > profile/mux-first-root-parse-threads.txt
python3 -I sample_agg.py "$P/m-mux-2.txt" Thread_1501336904 45 > profile/mux-full-size-capture-thread.txt
/bin/ls -la profile
