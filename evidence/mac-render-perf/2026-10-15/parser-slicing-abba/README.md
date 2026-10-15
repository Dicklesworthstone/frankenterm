# ft-yccm0.2.3 AC3 throughput: sliced vs unsliced fused feed (M.1 bench, ABBA)

AC3 of ft-yccm0.2.3 asks for "no throughput regression > 3% on M.1 mux lane
(ABBA)". The M.1 bench (`frankenterm/term/benches/ingest`) had no lane that
goes through the slicing. Its `mux_fused` lane calls `Terminal::feed` once per
128 KiB chunk, as the mux did before 8d977b591. This commit adds
`mux_fused_sliced`. It feeds the same chunks through `SlicedFeed` the way
`LocalPane::run_parser_slices` does: 1 KiB slices that end before a control
byte, with a new output batch whenever a hold reaches the 2 ms
`PARSER_HOLD_BUDGET`. Both lanes run in the same binary, so one invocation
compares them.

The lane does not include what the mux does at each handoff: one fair unlock
and relock of the terminal mutex, and a render-facts publish. With nothing
waiting, that happens at most once per 2 ms (500 times a second).

- Host: mac-studio (Apple M5 Ultra, macOS 27.0.1), load average 14-28 (shared
  with other agents' builds).
- Build: `cargo build -p frankenterm-term --profile release-perf --example
  ingest_throughput`, rustc 1.100.0-nightly 908501772, tree ddf302373 plus this
  lane.
- Corpora: the bench's generated 64 MiB corpora at the default seed.
- Driver: `abba_sliced.py BIN runs.jsonl ROUNDS CORPORA`. Round r runs A then
  B when r is even and B then A when r is odd, each as its own process. Raw
  records are in `runs.jsonl`.

Ratio is mux_fused_sliced / mux_fused, taken per round. Below 1 means slicing
is slower.

| corpus | rounds | mux_fused median MiB/s | sliced median MiB/s | per-round ratio median (min-max) |
|---|---|---|---|---|
| color_random (T1) | 10 | 246.3 | 243.7 | 0.9857 (0.9658-1.0232) |
| color_emoji_random (T0) | 10 | 308.1 | 300.3 | 0.9692 (0.9080-1.0471) |
| seq_lines | 10 | 63.3 | 64.0 | 1.0083 (0.9897-1.1026) |
| tui_repaint | 10 | 153.5 | 151.2 | 0.9892 (0.9476-1.0165) |
| color_emoji_random (T0) | 30 | 284.4 | 283.7 | 1.0054 (0.8843-1.1315) |
| color_random (T1) | 30 | 229.7 | 224.2 | 0.9848 (0.9148-1.0438) |

Each corpus's final state fingerprint was identical across both lanes and all
rounds. In this bench too, slicing leaves the same grid as one unsliced feed.

## Reading

- Slicing costs about 1.5% on T1 (0.9857 and 0.9848 in two independent
  series) and 1% on tui_repaint. It costs nothing measurable on seq_lines.
- On T0 the first 10-round series gave 0.9692, just past the 3% budget. A
  30-round rerun gave 1.0054. The per-round spread on this shared host is
  ±10-13%, so neither series alone resolves a few percent. Pooled over 40
  rounds each, the per-round ratio median is 0.9854 for T0 (mean 0.9876) and
  0.9852 for T1 (mean 0.9851).
- AC3's throughput clause is therefore met on this machine. Slicing costs
  about 1.5% on the two scoreboard corpora and at most about 1% on the others,
  against a 3% budget. A quieter host would narrow the T0 interval.
