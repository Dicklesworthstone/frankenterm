# Durability overload: native receipt (ft-yccm0.2.1.6)

mac-studio (M5 Ultra, macOS, load about 45-60 from other agents), 2026-10-14.
Release builds of main (8ea8de7fc for the "changing" rows).

`stress.sh BIN_DIR BUDGET_MB [SECS]` starts a private mux server (own HOME,
XDG dirs and socket) with `scrollback_durability_queue_max_mb = BUDGET_MB`.
Its only pane runs `flood_seq.py`, which prints numbered 64-byte lines at full
speed. After SECS seconds the script reports:
- the ingest rate: the last sequence number the pane shows, divided by the
  elapsed time;
- the server's overload log lines;
- what `ft session list-durable` reports for the pane.

## Ingest is never throttled

Interleaved pairs (`ab.txt`), 10 s each, with the same binaries:

| pair | 1 MiB budget | 64 MiB budget (default) | host load |
|---|---|---|---|
| 1 | 0.41 M lines/s (25 MiB/s) | 0.44 M lines/s (27 MiB/s) | about 50 |
| 2 | 0.33 | 0.26 | about 55 |
| 3 | 0.11 | 0.10 | rising to 92 |

The rate follows host load, not the budget (pair ratios 0.93-1.27). With a
1 MiB budget the writer sheds nearly everything, and the parse rate is still
the default budget's: the PTY never waits on durability. A first pair, before
the interleaved runs, gave 0.44 vs 0.46 M lines/s over 15 s.

## Loss is explicit

- **Logs:**

  ```text
  WARN … scrollback durability for pane … is overloaded (524040 of 1048576 queued bytes): rows 0..331 (331 rows, …
  WARN … caught up below half its queue budget at row 7783; overload gaps stop
  ```

  With 64 MiB the pane alternates between the two warnings (49 lines in 15 s).
  The hysteresis drains to half the budget each time.
- **`ft session list-durable` after the writer stops:** the 1 MiB run shows
  `"state": "complete"` and
  `"durability_gaps": {"markers": 313, "rows": 4595604, "bytes": 7297819152}`.
  `retained_rows` is 313, so every retained row is a marker.
- **`ft session list-durable` during the flood:** before 8ea8de7fc it reported
  `"state": "corrupt"`, with "live scrollback manifest changed during
  discovery". That is a race with the writer, which republishes with every
  marker (about 30 a second here). With 8ea8de7fc, the six runs in `ab.txt`
  list `complete` five times, with live gap totals (for example 1 MiB:
  markers 465, rows 3,426,512; 64 MiB: markers 74, rows 2,493,458). Once a
  listing landed mid-publication and reported `changing`. None reported
  `corrupt`.

## Sizing finding

A queued row costs about 1.58 KB (524,040 bytes for 331 rows of 62-byte
text), because the queue holds uncompressed `Line`s. The default 64 MiB budget
therefore holds about 42k rows. At this flood rate that is about 0.1 s of
output, which is why even the default budget sheds rows on this machine.
Queueing compressed rows (`compress_for_scrollback`) would make the same
budget hold an order of magnitude more.

## Follow-up: charge rows at their real size

The queue now charges a row `Line::retained_bytes()`: text and attribute runs
for a clustered row, the usual spilled form. Before, every row was charged as
a `Cell` per column. Same new `ft` in both arms, 64 MiB budget, interleaved
(`charge-ab.txt`):

| arm | bytes per gap row | gap markers per 10 s run | rows durably written |
|---|---|---|---|
| old server (08:23 build) | ~1,588 | 55, 24, 124 | 2.4%, 0.4%, 0.1% |
| new server | ~252 | 20, 18, 27 | 8.9%, 4.9%, 18% |

The ingest rate is unchanged (0.55-0.68 vs 0.59-0.62 M lines/s). Fewer
markers leave more real rows in the retained window. The budget remains a
true bound on queued memory; before, it held about a sixth of what it allowed.
