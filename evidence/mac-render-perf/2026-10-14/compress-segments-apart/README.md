# Compression vouches for cell boundaries by local segmentation (native)

**Change.** `ClusteredLine::from_cell_slice_checked` (the default `compress_for_scrollback` builder)
returns whether every cell boundary is known to break. Before, only boundaries whose two edge chars
are both in the `is_cluster_inert` table counted, and any other boundary sent the whole row through
`reproduces`: re-segment the clustered text and compare every cell. Now such a boundary is checked by
segmenting just the two neighbouring cells (`segments_apart`, finl_unicode `Graphemes`, the
segmenter clustered storage reads back with), which is exact once the earlier boundaries break.

**Why it matters.** Counting with temporary counters on the 128 MiB `unicode_mix` corpus (CJK,
combining marks, ZWJ emoji, flags, RTL): of 1,000,000 compressed rows, 1 was inert by the char table
and 999,918 reproduced, so the whole-row check ran on essentially every row and almost never
changed the outcome.

**Profile** (`sample`, term lane, unicode_mix 128 MiB; samples by caller, on-CPU):
finl `Graphemes` called from `compress_for_scrollback` 164 -> 54; `ClusterGraphemes::next`
(the whole-row read-back) 174 -> 29.

**Throughput** (`ingest_throughput --lane term`, release-perf, before = 6d6299078, after = this
change, interleaved, mac-studio at load 80-120, identical `state_fingerprint` in every pair), MiB/s:

| corpus | before | after | median |
|--------|--------|-------|--------|
| unicode_mix (4 pairs) | 27.9 26.2 25.9 24.4 | 29.0 29.5 25.5 26.2 | 26.0 -> 27.6 (+6%) |
| color_emoji_random (4) | 249.7 272.4 249.9 269.4 | 225.1 262.5 262.2 260.1 | 259.6 -> 261.2 |
| tui_repaint (4) | 152.4 144.6 148.2 140.2 | 145.6 147.8 148.1 152.1 | 146.4 -> 148.0 |
| long_lines (8, order reversed) | 471 479 477 455 484 460 479 453 | 477 467 486 456 504 455 489 434 | 474.3 -> 472.2 (5/8 pairs after) |

ASCII and inert rows never reach the new check; their numbers are noise at this load. A first
version validated the concatenated pair with `str::from_utf8` (5.6% of samples); the shipped one
concatenates into a reused `Zeroizing<String>` (no validation, no `unsafe`; the workspace forbids it).
