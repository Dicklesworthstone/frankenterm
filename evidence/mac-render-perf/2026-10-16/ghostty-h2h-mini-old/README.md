# Pinned Ghostty vs FrankenTerm, headless ingest (first admissible run)

`scripts/ghostty-headless-h2h.sh` (ft-yccm0.1.3), 2026-10-16 17:16-17:20 UTC.
The receipt is this directory; `shasum -a 256 -c SHA256SUMS` verifies it.

**Setup.**
- **Host:** mac-mini-old, Apple M4 (not the contract's M4 Pro), console idle. The 1-minute load stayed at or below the contract's 4.0 for admitted rows.
- **Ghostty:** commit e500d414 (pinned), built with the pinned zig 0.16.0; ghostty-bench `+terminal-stream`. Ghostty.app 8f0dd3709 was verified from a copy of studio's install.
- **FrankenTerm:** main 6a9c781eb; `ingest_throughput --lane term` (release-perf), scrollback 3500, 64 KiB chunks.
- **Method:** identical 64 MiB corpora; 10 ABBA rounds per row after 3 warmups; CV ceiling 5%.

FT speed relative to Ghostty is ghostty median / FT median; above 1 means FT is faster.

| corpus | 80x120 | 117x512 | Track B gate (ft-yccm0.3.5) |
|---|---|---|---|
| color_emoji_random (primary) | **1.32x** | **1.44x** | >= 2.0x |
| color_random | 0.70x (CV over 5%, not admitted) | 0.72x | >= 2.0x |
| seq_lines | 0.24x | 0.24x | >= 1.5x |
| long_lines | 0.42x | 0.46x | >= 1.25x |
| unicode_mix | 0.20x | 0.42x | >= 1.0x |
| tui_repaint | 0.61x | 1.14x | >= 1.25x |

Medians in seconds (80x120 / 117x512):

| corpus | Ghostty | FrankenTerm |
|---|---|---|
| color_emoji_random | 0.354 / 0.388 | 0.268 / 0.270 |
| color_random | 0.242 / 0.236 | 0.343 / 0.327 |
| seq_lines | 0.266 / 0.430 | 1.130 / 1.782 |
| long_lines | 0.074 / 0.068 | 0.178 / 0.147 |
| unicode_mix | 0.402 / 0.842 | 1.987 / 1.991 |
| tui_repaint | 0.292 / 0.552 | 0.481 / 0.482 |

The receipt's verdict on the primary corpus is `ft_faster`. This is not the Track B gate: the host is an M4, and FT trails on every other corpus.

**Getting it to run.** A fresh checkout of the pin used to fail. zig 0.16 fetches Ghostty's packages into `zig-pkg/` in the checkout, and the drift listing then died of SIGPIPE (exit 141). That is fixed in 6a9c781eb. An earlier run in the same session (receipt 20261016T171003Z) refused every row because the FT build in that invocation raised the load to 17.65.
