# ft-yccm0.2.3 AC3: terminal lock histograms on the M4 Pro (mac-mini-max)

Native, 2026-10-14, main 1a11ac1f2 (release-interactive bundle). Host:
mac-mini-max (Apple M4 Pro, macOS 26.2, 1920x1080 at 2x, 30 Hz). Each run of
`hold_ab.py` has one GUI and one 240x30 window whose pane runs
`while :; do cat CORPUS; done`. It flushes for 20 s, then reads the GUI's
resource snapshot `terminal_locks`. The histograms are cumulative from launch,
and their percentiles are bucket bounds. The corpora are the first 100 MB of
`~/color-random.bin` (T1) and of `~/color-emoji-random.bin` (T0). Neither has
a newline: each is one logical line that wraps.

The window is placed below the full-screen ChatGPT window, with its top strip
on screen (`FT_CV_Y=1960`). At the default position it opened under another
app's window, AppKit reported it occluded, and it parked: 0 frames presented.

| run | parser holds | hold p50 / p95 / p99 / max (ms) | paint wait p99 / max (ms) | presented fps |
|---|---|---|---|---|
| T1 r1 | 47,160 | 0.005 / 0.655 / 0.918 / 185.0 | 1.835 / 10.0 | 29.6 |
| T1 r2 | 39,322 | 0.010 / 0.786 / 1.049 / 200.0 | 3.670 / 10.0 | 28.6 |
| T1 (poll) | 50,760 | 0.006 / 0.786 / 0.918 / 65.6 | 1.311 / 5.5 | 29.3 |
| T1 (sampled) | 45,054 | 0.010 / 0.655 / 0.786 / 95.1 | 3.670 / 10.0 | 28.1 |
| T0 r1 | 52,235 | 0.014 / 0.918 / 1.049 / 5.1 | 1.049 / 10.0 | 29.6 |
| T0 r2 | 48,535 | 0.029 / 0.918 / 1.049 / 5.3 | 3.670 / 10.0 | 29.6 |

mac-mini-old (M4, macOS 26.5, 60 Hz) with T0, 1a11ac1: parser hold p99 0.786
ms (max 4.2-4.8 ms) in 80x12 and in 240x30 windows.

## Against AC3
- **Parser hold p99 <= 2 ms: met** on the M4 Pro under T1 (0.92-1.05 ms) and
  T0 (1.05 ms). The 2.1-2.6 ms of the 2026-10-12 T0 profile runs was on
  mac-studio with build 07f454fb4.
- **Snapshot (paint) wait p99 <= 1 ms: not met**: 1.05-3.67 ms. The max is
  ~10.0 ms in 5 of 6 runs.
- **T1 outlier.** Single parser holds of 65-200 ms, once per run. Polling the
  snapshot every 0.1 s placed it about 7 s into the flood, not at startup. T0
  stays under 5.3 ms. `sample` over ssh got no stacks, because the hardened
  runtime of the signed bundle blocks it, so the operation is not identified
  yet.

A slice check that yields before one more stretch would cross the 2 ms budget
was written and then dropped. Holds here rarely reach the budget, so it had
nothing to fix.
