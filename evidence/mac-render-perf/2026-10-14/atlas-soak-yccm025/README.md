# ft-yccm0.2.5 AC4 / ft-yccm0.2.6: 70-minute GUI soaks with atlas rebuilds

Native, 2026-10-15 02:39-03:59Z, mac-mini-old (Apple M4, macOS 26.5,
1920x1080 at 60 Hz, load 1.9-2.9). GUI: main 421fe2028 (release-interactive
bundle). The atlas code is unchanged in main up to 786c11920.

`scripts/mac-gui-soak.sh --duration 70m` was run twice at once, each with its
own isolated GUI. Workload: the harness's corpora (T0-format 256-color emoji,
seq lines, scroll-region churn, ~32k-glyph CJK/Hangul atlas-growth corpus in
four faces, double-size lines), then a 60 s idle per pass. Runs used
`--env FRANKENTERM_NATIVE_E2E_NONACTIVATING=1`, so there was no focus steal.
Each run's Lua config (`soak-*.lua`) places its window in the screen strip
that no other window covers, so it never parks. The harness could not resize
the windows: osascript has no Accessibility access over ssh, and the receipts
record `resizes_ok: 0`.

| run | config | bundles / span | verdict | atlas at end | GPU-owned slope | footprint slope |
|---|---|---|---|---|---|---|
| default (`receipt-default-atlas.json`) | defaults, 1024 px first atlas, 80x24 | 14 / 1.08 h | pass, not advisory | 4 generations from 1 texture (4 MiB); 3 same-size clears in place | 0.37 MB/h (R² 0.005) | 22 MB/h (R² 0.15) |
| rebuilds (`receipt-128px-start.json`) | `glyph_atlas_initial_size = 128`, font 24, 140x45 | 14 / 1.08 h | pass, not advisory | 10 generations, 5 textures created, 4 released, 1 live (16 MiB), peak 20 MiB | 0.0 MB/h | 4.9 MB/h (R² 0.001) |

Both GUIs presented about 5,200 frames: ~13 fps during each pass, ~1 fps idle.

## Reading
- **ft-yccm0.2.5 (no atlas pinning).** Across 9 rebuilds, the live atlas
  textures never exceeded one, except for the moment a growth rebuild holds old
  and new together (peak 4 + 16 MiB). Every replaced texture was released.
  GPU-owned memory and the atlas ledger had zero slope over the hour. This is
  the >= 1 h non-advisory soak that AC4 asked for.
- **ft-yccm0.2.6 (clear in place).** Generations outnumber textures: a
  same-size rebuild cleared the existing texture instead of creating one (3 in
  the default run, 5 in the rebuild run).
- Neither run reproduces the operator-scale 6K T0 stream of the original
  0.15.2 leak. Here the evidence for the fix is the zero-slope ledger under
  real rebuilds, plus the unit test
  `thousand_atlas_rebuilds_keep_live_atlas_bytes_within_twice_the_atlas_size`.
