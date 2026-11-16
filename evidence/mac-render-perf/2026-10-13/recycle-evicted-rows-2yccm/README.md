# ft-yccm0.2.9: recycle evicted scrollback rows as new blank rows (native)

**Change under test.** When a newline scrolls a full screen, `Screen::scroll_up` moves the row
evicted from scrollback to the bottom as the new blank row. For the common default-attribute
blank it used to drop that row (wipe its text, free its `Arc<ClusteredLine>`, `String` and
cluster `Vec`) and allocate a fresh `Line::new`, whose text buffer then regrew as the row was
written. `Line::recycle_as_new` now resets a uniquely owned clustered row in place
(`ClusteredLine::reset_for_reuse`: the old text is wiped exactly as on drop, the allocations
are kept). Shared rows, vector-backed rows, and rows whose text buffer exceeds 4096 bytes still
get a new line.

**Setup** (mac-studio, M5 Ultra, macOS 27; `recycle-ab.sh`, the same harness as
`zeroize-wordwise-3floy`): `ingest_throughput --lane term`, `--profile release-perf`, before =
parent commit, after = this change; the 64 MiB seq_lines corpus (sha256 d07e1bf9…), 80x120 grid,
3580 retained rows; arms interleaved three times at 1-minute load 37-38.

**Throughput** (MiB/s, `run-*.json`, every run sanity=ok):

| arm | run 1 | run 2 | run 3 |
|-----|-------|-------|-------|
| before | 43.9 | 42.9 | 43.5 |
| after | 59.4 | 60.6 | 61.3 |

About 39% more throughput.

**Profile** (`sample-*.txt.gz`: 1 s samples starting 0.2 s into a run, since a run now lasts
about a second; share of on-CPU top-of-stack samples, excluding waits):

| symbol | before | after |
|--------|--------|-------|
| allocator free | 12.1% | not present |
| allocator malloc | 8.4% | not present |
| `guarded_reserve_text` (text buffer growth) | 2.8% | not present |
| `wipe_text_allocation` + memset (wiping the old row's text) | 11.7% | 11.3% |

The remaining wipe is the deliberate zeroing of every evicted row's text.

With ft-70s4z (gap fill) and ft-3floy (word-wise wipe), this lane went from 10.5 MiB/s
(2026-10-12, load ~250) to about 60 MiB/s.
