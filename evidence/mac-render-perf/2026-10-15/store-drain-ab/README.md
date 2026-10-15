# ft-yccm0.2.1.8: GUI drain time with the durable scrollback store on vs off

This is the A1.8 gate's measurement: drain time with the store on, divided by
drain time with it off, should be at most 1.05. It was taken before the gate's
remaining blockers closed, to see where A1 stands.

## GUI, M4 Pro (mac-mini-max), bundle 0a4d33e

`drain_ab.py APP CORPUS ROUNDS [label]` runs one GUI per run under its own
HOME and an absolute `--config-file`, launched with `open -g -n` and stopped
by its exact pid. Each run has one 240x30 window placed so it presents frames
(about 30 fps). The pane times a single `cat CORPUS`, or `seq 1 N` when the
corpus is `seq:N`. cat can only write as fast as the GUI reads the pty, so its
wall time is the GUI's drain time.

- Arm "on" is the default configuration.
- Arm "off" sets `scrollback_tiered_enabled = false`: no warm or cold tier,
  and nothing reaches the store.

Rounds alternate on/off and off/on. Load average was 9-17 on 14 cores; the
raw records are in `gui-runs.txt`.

| corpus | on median (s) | off median (s) | per-round on/off | median |
|---|---|---|---|---|
| T1 `~/color-random.bin` (858 MB, one logical line) | 5.125 | 5.261 | 1.000, 0.955, 0.425*, 1.024 | 0.978 |
| `seq 1 20000000` (169 MB of short lines) | 25.131 | 15.068 | 1.653, 1.569, 1.682, 1.486 | **1.611** |
| `seq 1 2000000` (the ft-y0gy9 reference) | 2.080 | 1.435 | 1.280, 1.467, 1.445, 1.476 | **1.456** |

\* One off run took 11.7 s while load jumped to 17.

**Line output fails the 1.05 gate by a wide margin (1.46-1.61).** T1 passes
because it is one logical line: very few rows ever scroll into the store.

## Headless, mac-studio, same bundle

`store-drain-probe.sh BIN on|on-nogeom|off N [sample-secs]` runs a private mux
server whose single pane times `seq 1 N`. With `seq 1 5000000` the store-on
arm took 9.6-10.0 s and the off arm 7.9-8.5 s, about 1.2x. The host was at
load 30-46, and the reader thread sat mostly in `read()`, so the pty and the
producer bound this lane, not the parser.

## Where the time goes: in-process bench

`crates/frankenterm-mux-server-impl/benches/scrollback_ingest.rs`
(c7560a181) feeds line output through `Terminal::advance_bytes` in three
arms, ABBA, in one process:

- the production deferred sink (`open_scrollback_spill_sink`, the global
  durability writer);
- tiers on with no sink;
- untiered.

Per scrolled row on the parse thread (release-perf, mac-studio):

| rows | store | no sink | untiered | store/untiered |
|---|---|---|---|---|
| 80 columns, before c7560a181 | 884 ns | 140 ns | 115 ns | 7.7x |
| 80 columns, after | 361 ns | 142 ns | 114 ns | 3.2x |
| seq-like, before | 307 ns | 116 ns | 90 ns | 3.4x |
| seq-like, after | 285 ns | 124 ns | 95 ns | 3.0x |

A `sample` of the 80-column store arm before the change put 69% of
`Screen::record_scrollback_spill` in `LineWrapGeometry::capture`. That
function walks every cell through clustered-line grapheme iteration for the
cold-geometry index, and c7560a181 gives plain printable-ASCII rows an exact
shortcut. On seq-like rows after the change, 55% of
`record_scrollback_spill` is the deferred sink's admission. Of that admission:

- 38% is the per-row `Instant::now()` (an `isb`-fenced `mach_absolute_time`,
  ~80 ns in a busy pipeline against 13 ns in a tight loop);
- the rest is the line clone into the queue, two mutexes and the queue push.

Another 12% of `record_scrollback_spill` frees cold-geometry pages.

The commit that adds this paragraph takes the clock reads out of per-row
admission:

- one `Instant::now()` per commit window instead of one per row;
- in overload, the wall time only for a new gap span.

After it, the bench gives seq-like rows 270 ns (285-299 before, separate
runs at load 32-46) and 80-column rows 352 ns (361-368). In a 20M-row
seq-like store run, which is in durability overload most of the time, the
total fell from 7.54 s to 6.47 s, and a profile shows no clock reads left in
admission. The sampled 38% share of the clock overstated its cost, because
an `isb`-fenced counter read attracts samples. What remains per row is the
line clone into the queue (25% of admission), its box (10%), the locks and
queue push, and freeing cold-geometry pages.
