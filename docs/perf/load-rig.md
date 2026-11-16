# Replay-Corpus Load Rig (W9.3 / W9.3a)

The load rig promotes the test-gated `chaos_scale_harness` 200-pane substrate
into a **runnable, on-demand** surface so the W9.3 acceptance ("runnable on
demand; documented") is met and scale regressions are catchable thereafter.

The runner is the `load_rig` example binary
(`crates/frankenterm-core/examples/load_rig.rs`); it drives
`ChaosScaleHarness::run_replay_corpus_load_rig` over a **deterministic
large-swarm replay corpus** (generated, not live mux panes) and reports
per-capture-mode metrics plus the threshold verdict.

## Run it

```bash
# Default: 200-pane scale point, both capture modes, human-readable report.
cargo run --example load_rig -p frankenterm-core

# JSON report (the full ReplayCorpusLoadRigReport) for machine consumption.
cargo run --example load_rig -p frankenterm-core -- --panes 200 --json

# Narrow the human display to one mode (both are still exercised).
cargo run --example load_rig -p frankenterm-core -- --mode native-push
```

## Measured storage write path (`--storage`)

`--storage` additionally writes every corpus egress frame through the
production `StorageHandle::append_segment` path into a fresh temporary SQLite
database (FTS indexing included) and reports **measured** per-append latency
p50/p95/p99/max (microseconds), throughput (segments/s), and an FTS search for a
term taken from the corpus. The run exits `1` if nothing was written or the FTS
check finds no hits. With `--json` the output becomes
`{"load_rig": <report>, "storage_probe": <probe>}`.

```bash
cargo run --release --example load_rig -p frankenterm-core -- --panes 200 --storage
```

Storage numbers depend on the host disk; quote them only with the host, build
profile, and commit, and never from a debug build.

Valid `--panes` scale points are **10, 50, 200 (default), 1000**; any other value
exits 2 with an `unsupported scale point` message. Config overrides:
`--max-capture-lag-ms`, `--memory-limit-mb`, `--poll-interval-ms`,
`--dedup-window-ms`, `--queue-depth-limit`. See `--help` for the full list.

## Both capture modes are always exercised

The rig's value is comparing the two capture paths on **identical** replay input,
so every run exercises both:

- `poll` — the periodic mux-polling capture path.
- `native_push` — a deterministic replay model that applies the native-push
  dedup-window coalescing rule to the real replay-corpus egress timestamps and
  byte sizes. It does not call the production `native_events.rs` bridge.

`--mode` only narrows the human-readable display; the JSON report and the
overall verdict always cover both modes.

## Metrics and verdict

Per mode the rig reports capture-lag p50/p95/p99 (ms), max queue depth per pane,
whole-run memory, replay events/frames, output bytes, deduplicated events,
dropped events, and capture gaps, with a per-mode `threshold_passed`. A mode
passes when its p99 capture-lag, per-pane queue depth, and run memory stay under
the configured ceilings (defaults: 750 ms / 32 events-per-pane / 256 MiB).

Exit codes: `0` every exercised mode passed; `1` a mode failed its thresholds (a
real regression to fix — W9 is budgeted find-and-fix); `2` argument error or
unsupported scale point.

## Honest limitations

The rig reports these in every run:

- the replay corpus is deterministic; no live mux panes are launched;
- `native_push` applies dedup-window coalescing to replay events, but does not
  traverse the live bridge, mux, PTY, transport, or OS-event path;
- `poll` and `native_push` share identical replay input, so their lag and queue
  metrics are directly comparable.
- capture lag is derived from the corpus timeline; the storage write path is
  measured only by the separate `--storage` probe, not end-to-end through the
  capture loop.

This rig is regression infrastructure feeding W9.4 (the target-class run); it is
not itself the signed target-class hardware artifact. See
[`target-class-hardware.md`](target-class-hardware.md).
