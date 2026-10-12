# Headless T0: `unicode_version` 9 versus 14

d2be79b86 made 14 the default, so that VS16 emoji and keycaps take two
cells (ft-vylyj). At 14, `grapheme_column_width` asks
`Presentation::for_grapheme` about every multi-byte grapheme before it
consults the width tables; at 9 it skipped that. This checks that the new
default costs nothing on the operator's primary test.

- Bench: `frankenterm-term` example `ingest_throughput`, profile
  `release-perf`, `--lane term,prod_config --from-file
  ~/color-emoji-random.bin` (749,801,000 bytes, sha256 a442d895...43767),
  80x120, scrollback 3500, 128 KiB chunks.
- Arms, built from the same tree (07f454fb4): B is the tree as is (14); A
  has the two defaults set back to 9 (`frankenterm/config/src/config.rs`,
  `frankenterm/term/src/config.rs`).
- Order: 8 blocks of A B B A, one after another on mac-studio (Apple M5
  Ultra, macOS 27.0.1), 1-minute load 21-37 from other projects' jobs.
- Receipt: `headless-t0-unicode-9-vs-14.jsonl`, one line per lane run with
  its arm and block.

| Lane | 9 (n=16) mean / median | 14 (n=16) mean / median | 14 over 9, mean |
|---|---|---|---|
| term | 2.695 s / 2.631 s | 2.644 s / 2.582 s | 0.981 |
| prod_config | 3.489 s / 3.435 s | 3.486 s / 3.359 s | 0.999 |

Per-block ratios run from 0.93 to 1.16, so the arms are not separable: 14
is no slower than 9. Both arms end in the same terminal state on this file
(fingerprint 56cc3fe315ce...), because its emoji carry no variation
selectors.

Not measured here: the GUI drain (`time cat` in a window).
