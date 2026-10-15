# Who makes the GUI main thread wait for a terminal (ft-yccm0.2.2.3, ft-t162y)

Earlier runs left one question open. A real T1 flood showed a 6.4 ms
main-thread terminal wait, but paint's capture waits at most
`RENDER_CAPTURE_WAIT` (1 ms). Either some unbounded locker still runs on the
main thread, or the main thread was descheduled.

To find out, I ran a symbolized GUI: a release-perf `frankenterm-gui` copied
into bundle fad26a2 and ad-hoc signed. It ran `hold_ab.py` (T1: the first
100 MB of `~/color-random.bin`, 240x30, 20 s) on mac-mini-max with
`RUST_LOG=warn,mux::localpane=debug`.

The build carries the diagnostics added in this commit:

- Main-thread terminal waits of 2 ms or more were already logged with a
  backtrace when the wait acquired the lock (4525dadbe). Bounded waits that
  gave up are now logged too.
- Any terminal hold of 20 ms or more is logged at release, with the release
  backtrace. The guard drops inside the function that held it, so the
  backtrace names that function.
- A slow parser stretch reports the thread's CPU time over the stretch.
- The per-batch "applied one batch in N lock holds" line moves from debug to
  trace. It ran for nearly every batch of a flood, still under the terminal
  lock. At debug, a slow stderr write turned it into a long parser hold of
  its own: in perf2/perf3, holds of 33 and 97 ms were released in
  `feed_chunk_to_mux` after the last stretch measurement.

Per-run figures for all 30 runs are in `runs.txt`.

## Main-thread waits

There were **586 main-thread waits of 2 ms or more across the 30 runs. All 586
have `LocalPane::try_capture_render_frame` as their caller**, the bounded
paint capture. Median 9.8 ms, max 20.0 ms: one or two macOS scheduling quanta.
No unbounded terminal locker runs on the main thread during a flood.

The capture's `try_lock_for(1 ms)` returned the lock late. The parser hands
the lock over fairly (`unlock_fair`), so a waiting main thread receives
ownership at the handoff, even if that thread is not running at the time.
The wait then lasts until the main thread is scheduled again. Meanwhile the
lock sits idle and the parser, relocking, waits too. This is the cost of the
fair handoff that ft-yccm0.2.3 chose so a waiter is served promptly. Under
CPU contention, both threads pay a scheduling quantum.

The waits follow host load, not the build:

| batch | host load | paint wait p99 | slow main-thread waits per run | parser hold max |
|---|---|---|---|---|
| perf5 | 6 -> 11 | 0.04-0.52 ms | 0, except 2 in one run | 1.9-5.7 ms |
| perf2/perf3 | 13-15 | 0.08-10.0 ms | 0-19 | 3.9-96.8 ms (two of these were the logging artifact above) |
| perf4 | 19 -> 46 | 10.0-10.5 ms | 43-88 | 5.1-66.9 ms |

## Long parser holds (ft-t162y)

In perf4, every hold of 20 ms or more was a parser stretch inside one slice
step of the fused feed: "Yielded, 0 handoffs so far", released in
`run_parser_slices`. A 1 KiB T1 slice is microseconds of work, so these
stretches look like the parse thread being descheduled while it held the
lock. The thread runs at `UserInitiated` (mux lib.rs), but load reached 46 on
14 cores. The CPU-time field of the stretch log will settle this on the next
loaded run. The perf5 runs with that field had no stretch of 20 ms or more.
