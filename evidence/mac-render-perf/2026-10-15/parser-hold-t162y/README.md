# ft-t162y rerun: T1 parser holds on the M4 Pro, before and after e5a53d0ed

Native, 2026-10-15 12:57-13:05 EDT, mac-mini-max (Apple M4 Pro, macOS 26.2,
1920x1080 at 2x, 30 Hz). The harness is
`../../2026-10-14/parser-hold-yccm023/hold_ab.py` (unchanged) with
`FT_CV_Y=1960 FT_HA_COLS=240 FT_HA_ROWS=30` and 20 s floods of
`while :; do cat t1-100m.bin; done`, where `t1-100m.bin` is the first 100 MB
of `~/color-random.bin` (T1: one logical line, no newline).

Two release bundles from `ft-mac-build`, run alternately in one session:

- **786c119** (main 786c11920, 2026-10-14 22:36). Newer than the 1a11ac1f2
  bundle of the original report, older than e5a53d0ed. Its shed scrollback rows
  are still freed on the parser, under the terminal lock.
- **0a4d33e** (main 0a4d33e6d). It includes e5a53d0ed, which frees shed rows on
  a release thread.

Host load during the runs: 1-minute average 5.4-10.0 on 14 cores. The original
runs were at 11-31.

| run | parser holds | hold p50 / p95 / p99 / max (ms) | paint wait p99 / max (ms) | main thread blocked (ms, 20 s) | fps |
|---|---|---|---|---|---|
| 786c119 r1 | 67,168 | 0.005 / 0.524 / 0.655 / 2.122 | 0.025 / 0.224 | 3.94 | 29.5 |
| 0a4d33e r2 | 64,113 | 0.012 / 0.655 / 0.655 / 6.625 | 0.115 / 6.442 | 12.32 | 29.6 |
| 786c119 r2 | 64,000 | 0.004 / 0.655 / 0.655 / 3.231 | 0.049 / 0.732 | 6.35 | 29.6 |
| 0a4d33e r3 | 66,141 | 0.004 / 0.655 / 0.655 / 4.305 | 0.041 / 0.969 | 5.76 | 29.6 |
| 786c119 r3 | 66,179 | 0.004 / 0.524 / 0.655 / 1.225 | 0.041 / 0.081 | 4.57 | 29.6 |
| 0a4d33e r4 | 63,090 | 0.004 / 0.655 / 0.655 / 1.657 | 0.033 / 0.169 | 4.24 | 29.5 |
| 786c119 r4 | 63,573 | 0.005 / 0.655 / 0.655 / 2.017 | 0.029 / 0.340 | 4.34 | 29.6 |

Percentiles are histogram bucket bounds. "Main thread blocked" is the
cumulative `terminal_locks.main_thread_blocked_ns` since launch, which also
counts bounded waits that gave up (a4d155502).

## Reading

- **No 65-200 ms parser hold in 7 runs, in either build.** The longest hold was
  6.6 ms. The run directories hold 5-57 MB each, so the 64 MiB durability queue
  never overflowed and the shed path that e5a53d0ed moved never ran. These runs
  therefore say nothing about e5a53d0ed. They show that the outlier does not
  reproduce on the same machine at lower load, even in a build that still frees
  shed rows on the parser.
- This matches the 2026-10-14 correction on ft-t162y. During the original
  report mini-max ran at load 11-31. A diagnostic run at load 326 then showed
  paints waiting exactly one 10 ms scheduling quantum while no holder kept the
  lock that long. The 65-200 ms holds were most likely the parser thread being
  preempted while it held the lock on an oversubscribed host. A preempted
  holder still cannot stall the UI: paint waits at most `RENDER_CAPTURE_WAIT`
  (1 ms since 51438789c), then draws the last frame.
- **ft-yccm0.2.3 AC3 on this machine.** Parser hold p99 is 0.655 ms in every
  run (budget 2 ms). Paint wait p99 is 0.025-0.115 ms (budget 1 ms). The
  2026-10-14 paint-wait p99 of 1.05-3.67 ms was CPU contention at load 11-31.
  A single 6.4 ms paint wait in 0a4d33e r2 is above the 1 ms bounded wait. That
  can only be the main thread itself being descheduled, since the capture gives
  up after 1 ms.

The run directories (`/tmp/ftha.*` on mac-mini-max) and the copied bundles
(`~/.cache/ftt162y`) were kept (AGENTS.md RULE 1).
