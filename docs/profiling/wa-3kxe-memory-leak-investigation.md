# wa-3kxe.1 Memory Leak Investigation Notes

This document tracks reproducible steps and current findings for bead `wa-3kxe.1`:
"Memory leak root cause analysis and patches (ESO methodology)".

## Scope

Target process:
- WezTerm-derived mux server paths in `frankenterm/mux/*` under long-running agent swarm load.

Current hypothesis focus:
1. Subscriber callback accumulation in tmux integration.
2. Unbounded buffered state in parser/notification paths under adverse streams.

## Repro Harness

Use the profiling script added for this bead:

```bash
scripts/profiling/mux_memory_watch.sh --pid <MUX_PID> --out-dir tmp/profiling/run_001
```

Key knobs:
- `SAMPLE_SECS` (default `60`)
- `MAX_SAMPLES` (default `0` = until process exits)
- `VMMAP_EVERY` (macOS only, default `30`)
- `CAPTURE_LEAKS_END` (macOS only, default `1`)

Outputs:
- `rss.csv`: timeline (`timestamp_utc,epoch_s,rss_kb,vsz_kb`)
- `summary.txt`: computed growth rate in MB/hour
- `vmmap_*.txt`: periodic VM region summaries (macOS)
- `leaks.txt`: end-of-run leak report (macOS)

## Patch Set Implemented

### 1) Prevent duplicate tmux subscriber registration

File:
- `frankenterm/mux/src/tmux.rs`

Change:
- Added `mux_subscribed: AtomicBool` to `TmuxDomainState`.
- On `Event::SessionChanged`, subscribe only once per domain.

Why:
- Repeated `SessionChanged` events could register multiple subscriber closures.
- Each closure remained active and contributed persistent memory/CPU overhead.

### 2) Prune stale subscriber callbacks

File:
- `frankenterm/mux/src/tmux_commands.rs`

Change:
- In `subscribe_notification`, callback now returns `false` when the tmux domain
  no longer exists (or is not a `TmuxDomain`), so `Mux::notify` drops it.

Why:
- Previously callback always returned `true`, so dead-domain callbacks never
  self-pruned.

### 3) Bound short-DCS parser accumulation

File:
- `frankenterm/escape-parser/src/parser/mod.rs`

Change:
- Added `MAX_SHORT_DCS_BYTES` guard for short DCS payload accumulation.
- Added `discarding_dcs` parse-state flag so once the cap is exceeded, incoming
  DCS bytes are dropped until the parser receives the matching unhook.
- Added unit test `overlong_short_dcs_is_discarded`.

Why:
- Malformed or unterminated short DCS streams could otherwise append bytes
  indefinitely to in-memory parser state.

## Validation Status

Executed:
- `cargo check -p mux` ✅
- `cargo check --all-targets` ✅
- `cargo fmt --check` ✅
- `cargo test -p frankenterm-escape-parser --features std overlong_short_dcs_is_discarded -- --nocapture` ✅

Workspace gates currently failing due unrelated pre-existing issues:
- `cargo clippy --all-targets -- -D warnings` (existing clippy debt outside this patch scope)

## Next Steps

1. Run before/after profiler sessions with `mux_memory_watch.sh` on identical swarm load.
2. Compare `rss_growth_mb_per_hour` in `summary.txt`.
3. Continue with additional bounded-memory guards in non-conflicting code paths
   and keep per-patch behavior notes with invariants.
