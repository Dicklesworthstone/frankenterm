# ft-70s4z: seq_lines term-lane throughput and profile, before and after 5e4a9b3d7 (native)

**Change under test.** 5e4a9b3d7 fills the gap before a write past the end of a clustered row with
one detach and one bulk append (`ClusteredLine::append_blank_cells`), instead of one
`Arc::make_mut`, capacity check and attribute compare per blank. ASCII runs past the end stay
on the clustered fast path.

**Setup** (mac-studio, M5 Ultra, macOS 27, nightly-2026-08-31; `70s4z-ab.sh`):
- `ingest_throughput --lane term`, built with `--profile release-perf`, at three commits:
  - before = 93e9176b9, the parent of 5e4a9b3d7;
  - after = 5e4a9b3d7;
  - head = 15a2b1c46.
- All runs use identical bytes: the 64 MiB seq_lines corpus, sha256 d07e1bf9…
  (`corpus.json`), via `--from-file`.
- 80x120 grid, 3500 rows of scrollback.
- Arms interleave before, after, head, three times. The host was shared and heavily loaded
  (1-minute load 241-261, stated per run in `ab.txt` and the JSON lines).
- One `/usr/bin/sample` each of before and head, starting 1 s into a run, 4 s long.

**Throughput** (MiB/s, `run-*.json`):

| arm | run 1 | run 2 | run 3 |
|-----|-------|-------|-------|
| before (93e9176b9) | 10.5 | 10.5 | 10.0 |
| after (5e4a9b3d7) | 33.1 | 31.9 | 32.6 |
| head (15a2b1c46) | 31.1 | 31.0 | 32.2 |

- The fix makes the lane about 3.1x faster at the same load.
- Head is within run-to-run spread of the fix alone.
- Every run reported sanity=ok.

**Profile** (share of on-CPU top-of-stack samples, excluding waits; `sample-*.txt.gz`):

| symbol | before | head |
|--------|--------|------|
| `Arc<ClusteredLine>::make_mut` | 11.1% | 1.0% |
| `CellAttributes::eq` | 14.2% | 1.8% |
| `ClusteredLine::append_grapheme` | 12.3% | below 5 samples |
| `guarded_reserve_text` | 13.8% | 7.9% |
| `Arc<ClusteredLine>::drop_slow` | 9.6% | 30.4% |

- The per-blank cost is gone.
- `drop_slow` is now the largest entry. It is the final drop of each evicted row:
  `ClusteredLine`'s `Drop` runs `String::zeroize`, which writes zeros over the whole text
  capacity one volatile byte at a time.
- Its share grew because everything around it shrank. Per MiB of input it is about the same
  as before: one drop per row, not one per cell, as the fix commit predicted.
- Making that wipe cheaper is a separate question about the hardening contract (follow-up
  bead).
