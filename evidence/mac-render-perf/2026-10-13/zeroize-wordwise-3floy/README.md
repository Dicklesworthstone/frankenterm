# ft-3floy: seq_lines term-lane throughput and profile, before and after a32ff4f42 (native)

**Change under test.** a32ff4f42 wipes a clustered row's text eight bytes per volatile store
(`wipe_text_allocation`: extend the byte vector to its capacity, zeroize it as `u64` words via
`bytemuck::pod_align_to_mut`) instead of `String::zeroize`, which stores one byte at a time with a
fence per byte. Every byte of the allocation, used or spare, still gets a volatile zero store.
The row's `Drop` and `guarded_reserve_text` (buffer growth) both use it; growth now wipes the
buffer it leaves once instead of twice.

**Setup** (mac-studio, M5 Ultra, macOS 27; `3floy-ab.sh`, following
`../../2026-10-12/seq-gapfill-70s4z/70s4z-ab.sh`):
- `ingest_throughput --lane term`, built with `--profile release-perf`, from one checkout at
  a32ff4f42 ("after") and with the two changed files and `Cargo.lock` reverted to its parent
  ("before"); nothing else differs.
- Identical bytes for every run: the 64 MiB seq_lines corpus, sha256 d07e1bf9… (the corpus of
  the 70s4z A/B), via `--from-file`. 80x120 grid, 3580 retained rows.
- Arms interleave before, after, three times. 1-minute load 40-41 (shared host), per run in
  `ab.txt` and the JSON lines.
- One `/usr/bin/sample` of each arm, starting 1 s into a run, 4 s long.

**Throughput** (MiB/s, `run-*.json`, every run sanity=ok):

| arm | run 1 | run 2 | run 3 |
|-----|-------|-------|-------|
| before (parent of a32ff4f42) | 30.6 | 31.2 | 31.7 |
| after (a32ff4f42) | 41.1 | 39.9 | 39.9 |

About 29% more throughput at the same load.

**Profile** (share of on-CPU top-of-stack samples, excluding waits; `sample-*.txt.gz`):

| symbol | before | after |
|--------|--------|-------|
| `Arc<ClusteredLine>::drop_slow` (the byte-wise wipe on drop) | 25.7% | not present |
| `guarded_reserve_text` | 9.3% | 2.2% |
| `wipe_text_allocation` | n/a | 4.2% |
| `_platform_memset` (`Vec::resize` over the spare capacity) | 0% | 7.8% |

Wipe-related work falls from about 35% of the lane to about 14%. The remaining memset is the
zero-fill that makes the spare capacity addressable without `unsafe` (the workspace forbids it);
the volatile word stores then cover the same bytes.

A microbenchmark of the wipe alone (release, 200k rows of 40-240 bytes, 36 MiB of capacity):
`String::zeroize` 8.2-8.9 ms, word wipe 1.6-2.3 ms.
