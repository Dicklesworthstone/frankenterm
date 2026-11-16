# ft-2760r: a remote GUI painting a flooding pane

Native, 2026-10-14, mac-mini-old (Apple M4, macOS 26.5, 60 Hz).

`push_rate.py` runs a private `frankenterm-mux-server` whose only pane prints
numbered 100-column lines at full speed. A GUI attaches to it with `connect`
over the unix domain, and the script counts the frames that GUI presents
(resource snapshot `frames.presented_total`, 12-15 s after 4-5 s warm-up). The
control is the same GUI running the same producer in a local pane. The window
sits in the visible strip at the bottom of the screen (`FT_PR_Y=1120`); the
app stays inactive. Base bundle: main 421fe2028 (release-interactive).
`FT_PR_MUX` swaps in a server built from the working tree with the same
build identity.

## 1. Pushes never got the terminal (server)

Background pushes (`prepare_legacy_render`) took the pane's terminal with a
bare `try_lock`. A flooding parser hands the lock over only to waiters that
register demand (ft-yccm0.2.3), so a push almost never got it. The GUI only
saw a new frame when a push won the race.

| server | remote pane fps (range over runs) |
|---|---|
| main 421fe2028 (push only tries the lock) | 25.9-27.6 (local control 58.7-60.8) |
| push waits 1 ms (`RPC_READ_TERMINAL_WAIT`), no budget | 55.7-57.1 |
| push waits 1 ms within a 2 ms per 16 ms budget (committed) | 54.5-58.3; 56.0-59.8 in the A runs below |

Pushes run on the server's main thread, which also dispatches every client's
input. So a waiting push may use at most 2 ms of each 16 ms window
(`push_wait_budget`, one eighth of the main thread). Past the budget a push
only tries the lock, as before. One flooding pane gets the full rate.

## 2. Read-ahead was refused ~50 times a second (client)

With pushes fixed, the remote GUI still logged ERROR `get_lines failed ...
get_lines_at_layout code=backend_failure` about 50 times a second (the fetch
rate limit), and the server logged `owned line read failed stage=capture
reason=other` for each.

Temporary server logging traced every refused read to the client's read-ahead
(`prefetch_ranges` in `client/src/pane/renderable.rs`). Each paint prefetched
the viewport-sized page above the visible rows, so a page scroll would find it
cached. Every push already carries the visible rows as bonus lines. While
output streams, though:
- the rows above the viewport changed on every push;
- they were cold history by the time the request arrived (15,000 rows had
  scrolled past between one push and the next);
- the request named a superseded layout, so the server refused it (cold
  eviction under an installed cold visual layout moves the layout floor).

Even an accepted page would have scrolled away at once. The prefetches also
spent the shared line-fetch rate limit (`ratelimit_mux_line_prefetches_per_second`,
50/s) that on-screen rows need.

The fix: read-ahead waits for a settled screen. It is skipped while the latest
render delta moved `physical_top`. When output stops, the next delta that does
not scroll re-enables it. Without a later paint the first page scroll fetches
on demand, as it did before read-ahead existed.

Old GUI (421fe20) against new GUI, both against the committed server,
alternating, 12 s each:

| run | remote fps | local fps | refused line reads (GUI ERROR lines) |
|---|---|---|---|
| A old GUI r1 | 56.0 | 60.6 | 840 |
| B new GUI r1 | 60.4 | 60.5 | 0 |
| A old GUI r2 | 58.4 | 60.8 | 836 |
| B new GUI r2 | 58.0 | 60.6 | 0 |
| A old GUI r3 | 59.8 | 60.6 | 842 |
| B new GUI r3 | 59.8 | 60.7 | 0 |

The frame rate is unchanged within noise: the refused reads cost round trips
and log lines, not frames.

## Not changed
- A server-side "resident floor": resident reads fenced at a layout that only
  cold eviction superseded would still publish. It did not change the refusal
  count, because the refused reads were cold, not resident. Dropped.
- The scrollback durability overload warnings under the flood are by design
  (shared 64 MiB budget).
- Rarely (1 of 18 runs) the remote GUI gave up hydrating user variables: a
  flooding pane's user-vars snapshot is busy. Filed as ft-ypblq.

## Harness notes
- Processes are stopped by exact pid, and run directories under `/tmp/ftpr.*`
  are kept.
- One A/B attempt on mini-old stalled in process start: five servers wrote
  their first log line in the same 4 s, 3 minutes after launch, and the same
  binaries listen in 0.2 s otherwise (`startprobe`). A server that never
  listens is now stopped, not left running.
