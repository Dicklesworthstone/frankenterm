# ft-pjjeg: tail reads of a streaming pane refused with resource_busy

Native macOS (mac-studio, M5 Ultra, load 30-50 from other agents), 2026-10-14.

`repro.sh BIN_DIR [reads] [producer]` starts a private mux server (own HOME, XDG
dirs and socket) whose only pane runs `producer`, waits `WARM` seconds, then
runs `ft get-text <pane> --tail 20` `reads` times and counts refusals, plus the
server's `owned line read failed` log lines. `rate.py N` writes N 80-column
lines every 10 ms (`rate.py N T`: every T ms). In this first table `ft` is the
installed d79bd79 CLI in every run and only the server differs. base = installed d79bd79 server; fix = main d8dc89513 plus this
change, release build.

| producer | warm | base ok/total | fix ok/total |
|---|---|---|---|
| `cat /usr/share/dict/words` loop (max rate) | 3 s | 2/30 | 12/30 |
| rate.py 10 (1,000 lines/s, 80 KB/s) | 8 s | — | 20/20 |
| rate.py 100 (10,000 lines/s, 800 KB/s) | 6 s | 20/20 | 20/20 |
| rate.py 1000 (100,000 lines/s, 8 MB/s), run 1 | 6 s | 15/20 | 12/20 |
| rate.py 1000, run 2 | 6 s | 20/20 | 18/20 |

Server-side refusals at max rate:
- base: capture `metadata_busy` 19, plus the render-state fence (PDU114) refused
  for 23 of the 28 failed reads. The client prints those as
  `code=unknown_future`, because the project-level rejection codes have no
  resource_busy.
- fix: no `metadata_busy`. Every capture failure, 58 of 58, is "line read
  layout changed or unavailable".

## Cause (fixed here)
The server's RPC reads took the pane's terminal with a bare `try_lock`, which
registers no demand. A parser applying continuous output hands the lock over
only to registered demand (ft-yccm0.2.3), so the reads almost never got it. Line
reads (layout check, capture, publish) and the PDU114 render-state observation
now wait up to 1 ms with demand registered, the main-thread bound GUI paints
already use (ft-yccm0.2.2.3). Background pushes still only try.

## Remaining (ft-wpgjw, fixed below)
At max rate the pane's line-layout floor moves on almost every observation,
reaching the current seqno. A temporary log showed fence seqno 5565 against
floor 5571, then 5578 vs 5580, and so on. `scrollback_rows` stays at 3524: the
cold tier evicts its oldest rows continuously, and
`refresh_cold_source_observation` counts start movement as a source change.
Every fenced read issued even one or two seqnos earlier is then rejected, and
after three re-fences the client gives up with `mux_text_snapshot_changed`. At
≤800 KB/s per pane this does not happen.

## ft-wpgjw: layout-floor churn and the client's final fence

Two more changes:
- **Server (c6cc3a7b1).** Evicting the oldest cold rows no longer moves the
  line-layout floor unless a cold visual layout is installed.
- **Client.** A text read served by exactly one GetLinesAtLayout reply skips
  the final source fence. That reply was captured atomically and published
  against the fence's layout. Stitched chunks still require it.

With only the server change, capture refusals fell from 58 to 2. Reads still
failed on the client's final fence (`seqno` must be unchanged across fence,
read and fence, which a continuously printing pane never satisfies): 8/30 at
max rate, 7/20 at 8 MB/s.

Both changes (new `ft` and new server, release builds of main c6cc3a7b1 plus
the client change):

| producer | old ft + old server (d79bd79) | new ft + new server |
|---|---|---|
| max rate | 2/30 | 30/30 |
| rate.py 1000 (8 MB/s, bursts every 10 ms) | 15/20, 20/20 | 20/20 |
| rate.py 1 1 (one line per ms, continuous, 80 KB/s) | 18/20 | 20/20 |

Coherence: at 8 MB/s each returned `--tail 20` was 20 consecutive sequence
numbers, with no gaps or malformed rows.

## Follow-up: publication window and the newest-rows tail

ft-4p4uo's swarm-echo harness (4 unthrottled noise panes, Linux) still died:
every failure was `stage=publish reason=source_changed` on 44-row noise
tails. Publication requires every captured resident row unchanged, and a pane
at full rate rewrote its cursor row in the 1-20 ms between capture and
publication. Two changes:

- **Server (198fc06c7).** A read whose rows are all resident is captured,
  hydrated and published in one terminal-lock hold.
- **Client (28b15f103).** A one-chunk tail is one unfenced GetLines for the
  pane's newest rows, which the server clamps at capture time. They are hot
  even when the fenced rows would have spilled cold.

30 reads each, this server and ft:

| producer | reads | server-side line-read failures |
|---|---|---|
| max rate | 30/30 | 0 |
| rate.py 1000 (8 MB/s) | 30/30 | 0 |
| rate.py 1 1 (continuous) | 30/30 | 0 |

With the server change alone, max rate gave 29/30, with 10 publish refusals
on rows that had spilled cold. The run directories are kept (the script no
longer deletes them).
