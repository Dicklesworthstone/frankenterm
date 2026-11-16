# First recovery root under load, native macOS A/B (ft-upal7)

fc1aa61b8 ("fix(recovery): reuse kept guardian checkpoints and cap first-root checkpoints at
1 MiB (ft-upal7)") was measured here against its parent's mux server.

**Setup**
- Host: yi-ap, macOS 27.0.1 arm64. A shared machine, at load average 10 to 14 during the runs.
- Harness: `multi.sh` here. It runs a guardian-backed mux in an isolated HOME, with 50 panes. At
  mux-startup each pane prints rows 1 to 40,000, at `scrollback_lines = 100000`.
- Every run used `KILL_AT_ROOT=1`: the mux is killed with SIGKILL as soon as its first recovery
  root is published. The restore therefore comes from that first root.
- The harness then reads every pane. It passes only if each pane holds its newest row. It also
  counts the panes that hold every row from 1 to 40,000.
- Families: release-interactive `frankenterm-mux-server` from one source tree and profile, with
  bundle-2bd019d's guardian and `ft`.
  - base: d24c96a24.
  - new: gate 7ec285956 for the default and queue runs; gate 309b3ceb1 for the stall runs. 309b3ceb1
    adds the mux log line "a recovery capture takes back the guardian checkpoint kept N ms ago".

**Results**

| run | first root after ready | capture | restore | after the restore |
|---|---|---|---|---|
| default, base | 96 s | 70.2 s | 64 s | 50 of 50 panes hold rows 1..40000 |
| default, new | 40 s | 22.0 s | 36 s | 50 of 50 panes hold rows 1..40000 |
| queue 4 MB, base | 82 s | 63.8 s | 64 s | newest row in all 50; 1,402,802 rows in all |
| queue 4 MB, new | 39 s | 20.2 s | 32 s | newest row in all 50; 1,389,383 rows in all |
| stalled guardian, base | none in 15 min | 4 attempts over budget | refused | "no committed recovery snapshot exists; refusing a fresh mux start" |
| stalled guardian, new | 570 s | 177.9 s (3rd attempt) | 38 s | 50 of 50 panes hold rows 1..40000 |

- **Queue 4 MB** is `QUEUE_MB=4`, which sets `scrollback_durability_queue_max_mb`. The durability
  writer stays saturated, and the store has overload gaps by design. Those gaps, not the checkpoint
  size, are why fewer rows are held.
- **Stalled guardian** is `STALL_GUARDIAN=0.2/3.8`. Once all 50 panes were up, the guardian was
  stopped with SIGSTOP for 3.8 s of every 4 s. Each stop stays under the guardian client's 5 s
  socket timeout. Each checkpoint then took 4 to 12 s, against a budget of 30 s plus 3 s a pane
  (180 s).
  - Base took 9, 18, 24 and 28 of 50 panes in its four attempts. Each attempt dropped its work.
  - New took 23 of 50 panes in attempt 1. Attempt 2 took all 50, then overran by 2.4 s after the
    loop.
  - New's attempt 3 refreshed the 45 oldest kept checkpoints and named the other 5 as kept.
  - The `stall-*-mux-log.txt` files hold each sandbox's recovery lines, including the reuse lines
    with their ages.

**Commands** (`runs/<run>.out` holds each transcript):

```
KILL_AT_ROOT=1 multi.sh <family app> 100000 40000 50 300
QUEUE_MB=4 KILL_AT_ROOT=1 multi.sh <family app> 100000 40000 50 300
STALL_GUARDIAN=0.2/3.8 KILL_AT_ROOT=1 multi.sh <family app> 100000 40000 50 900
```

**Not covered.** A crash before the first root still strands every guardian pane. These runs
show that window is shorter (96 s to 40 s at 50 panes) and that a slow guardian no longer keeps a
mux from committing a root at all. The bead's options (b) and (c) would close the window, by
adopting guardian children that no root names; they are not part of this change.
