# Native FT-vs-FT A/Bs of two runtime kill switches (ft-yg1lt, ft-951ev)

**Build**: `ingest_throughput --lane term`, `--profile release-perf`, at 6d6299078, mac-studio (M5 Ultra,
macOS 27). One binary; the arms differ only in an environment variable. Final terminal state
(`state_fingerprint`) is identical in every arm.

**Corpora**: the operator's T0 file `~/color-emoji-random.bin` (715 MB, 95% wide emoji with 256-color SGR),
in full and as its first 64 MiB (sha256 6dd3160a…), and the generated 64 MiB `seq_lines` (sha256
d07e1bf9…). 80x120 grid, 3500 scrollback rows.

## ft-yg1lt: `Presentation::for_grapheme` skips the variation map (`FT_PRESENTATION_VS_SKIP=0` = old path)

Full T0 file, arms interleaved, load 45-50 (`vs-*.json`), MiB/s:

| pair | skip (default) | old |
|------|----------------|-----|
| 1 | 247.6 | 256.0 |
| 2 | 287.5 | 269.1 |
| 3 | 292.3 | 263.5 |
| 4 | 286.9 | 254.3 |

Median 287.2 vs 259.8 MiB/s (+10.5%), 3 of 4 pairs. Profile (`sample` 2 s into a full run,
top-of-stack share): `for_grapheme` + variation-map lookups 1.0% (default) vs 5.9% (old path).
The 64 MiB slice at load ~100 (`ks-ab.log`) agreed in median (+6%) but was noisy.

## ft-951ev: compress_for_scrollback through the slice builder (`FT_LINE_COMPRESS_SLICE=0` = legacy)

Full T0 file, interleaved, load ~45 (`full-*.json`), MiB/s:

| pair | slice (default) | legacy |
|------|-----------------|--------|
| 1 | 283.8 | 258.6 |
| 2 | 259.8 | 265.0 |
| 3 | 264.7 | 263.8 |
| 4 | 257.1 | 260.0 |

Median ~262 for both: no measurable end-to-end difference. The 64 MiB T0 slice and the seq corpus
(`ks-ab.log`) also show none (T0 ~250, seq ~57 MiB/s in both arms). Compression is 5-8% of T0
samples in both arms (the slice builder is inlined into `Line::compress_for_scrollback`; the legacy
path shows as `ClusteredLine::build`), so the 1.30x per-row gain measured on x86 (ts1) does not
show up end to end on Apple Silicon. The slice path stays (correct, no regression).
