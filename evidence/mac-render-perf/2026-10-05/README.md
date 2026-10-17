# mac-render-perf evidence: 2026-10-05 diagnosis session

Planning evidence for the `[mac-render]` bead epic, which aims to make FrankenTerm faster than Ghostty on Apple Silicon. Every number below is **provisional**: it was captured on a shared, heavily loaded host (load average 36-100, 20 of 21.5 GB swap in use). Re-measure under the quiet-window protocol (the M.2/M.3 beads) before citing anything as a claim.

## Host

- Apple M4 Pro: 10 performance + 4 efficiency cores, 20-core GPU, Metal 4
- 64 GB RAM, 6144x3456 display
- macOS 26.2 (25C56)
- rustc 1.100.0-nightly (2026-08-30)
- zig 0.16.0, used only to build ghostty-bench

Details are in `fingerprint.json`.

## Files

| File | What it is |
|---|---|
| `gui-47759-sample-0.15.2.txt` | `/usr/bin/sample` (8 s, 1 ms) of the frozen FrankenTerm.app 0.15.2 (319a56d, front_end=WebGpu) while `cat color-random.bin` was stalled. Binary is stripped, so frames are addresses. |
| `ftbench-seq-sample.txt` | Symbolized sample of the out-of-tree ftbench `term` lane on 64 MiB of `seq` output |
| `ftbench-emoji-term-sample.txt` | Symbolized sample of the ftbench `term` lane on 64 MiB of the operator's color-emoji-random corpus |
| `ftbench/` | Source of the out-of-tree headless driver over `frankenterm-term` at HEAD fc3fd2121. Lanes: parse, term, mux; `CONFIG=prodlike` mimics the GUI's mutex + Arc config reads. Build it from the repository root with `CARGO_TARGET_DIR=/tmp/ftbench-target RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --manifest-path evidence/mac-render-perf/2026-10-05/ftbench/Cargo.toml`, then run it as `ftbench <parse\|term\|mux> <corpus>`. It is not a workspace member, and its path dependencies are repository-relative. |
| `ftbench-emoji-term-after-fix-sample.txt` | Symbolized sample of the ftbench `term` lane on the emoji corpus after 2f7920f79 |

## Key observations

### 1. The 0.15.2 freeze

- **cat:** its TTY offset advanced in exact 524,288-byte steps (equal to the operator's `mux_output_parser_buffer_size`), with 20+ s stalls in between. Average drain was about 37 KB/s, versus about 80 MB/s for Ghostty on the same file.
- **Parse thread:** roughly 100% of its time was spent inside about 35 `fcntl` call sites per cycle (`File::sync_all`, which is F_FULLFSYNC on macOS). The rest was `rename`, `openat`, `opendir`, and `fstatat`. All of it is the durable encrypted scrollback store, running per row under the terminal mutex.
- **Main thread:** 100% of samples were in `pthread_cond_wait` under `-[NSWindow _handleMouseDownEvent:]`. It was waiting on the terminal mutex, which is what produced the beach ball.
- **Memory** (`vmmap --summary`):
  - footprint 6.2 GB (peak 8.4 GB);
  - jemalloc "Memory Tag 254": about 4.2 GB in 31,947 mappings;
  - "owned unmapped" GPU memory: about 3.0 GB;
  - IOSurface plus IOAccelerator: about 1.2 GB.

### 2. Headless throughput at HEAD fc3fd2121

ftbench at 80x120 with 3,500-line scrollback, compared with `ghostty-bench +terminal-stream` at Ghostty HEAD e500d414f. Times are for 64 MiB.

| Corpus | ftbench parse | ftbench term | ftbench term under load | ghostty-bench under the same load |
|---|---|---|---|---|
| color_emoji_random (operator's primary test) | 26.6 MiB/s | 6.2 MiB/s | 10.3 s | 1.6-2.0 s |
| color-random | 138 MiB/s | 73 MiB/s | 5.8 s | 0.8-1.9 s |
| seq | — | 10.6 MiB/s | 33-45 s | 1.5-2.2 s |

### 3. Hotspots

**Emoji corpus (term lane):** `Performer::recluster_at_cursor` accounts for 52% of samples, and malloc/free for about 20%. The cause is an O(cursor_x) allocating scan that runs for every multi-byte grapheme (`frankenterm/term/src/terminalstate/performer.rs:110`, called at `:449`).

This was fixed in 2f7920f79 (bead ft-yccm0.2.11). The after-fix sample is `ftbench-emoji-term-after-fix-sample.txt`; it shows `recluster_at_cursor` at 0.0% of samples. The residual fallback is tracked in ft-yccm0.2.14.

Quieter-window re-measurement on the same 64 MiB emoji slice (load avg 11-20):

| Engine | Time |
|---|---|
| ghostty-bench | 0.42-0.50 s (~140 MiB/s) |
| FrankenTerm term, before the fix (4 runs) | 2.61-2.87 s |
| FrankenTerm term, allocation-free scan only (10 runs) | mean 1.218 s |
| FrankenTerm term, fixed with the ZWJ gate (10 runs) | mean 0.862 s, CV ~2% |

Ghostty's headless rate swings from ~32 to ~140 MiB/s with host load. Ghostty's GUI time on this test (41.8 s, about 17.9 MB/s) was measured on a busy machine. Compare only numbers taken in the same window.

**seq corpus:** time goes to `Line::set_cell_impl` → `ClusteredLine::append_grapheme` / `guarded_reserve_text`, plus `Arc::make_mut` on every character, `CellAttributes::eq` on every character, and `Arc<ClusteredLine>::drop_slow` on every scroll (about 14%). Bead A4.6 addresses this.

### 4. Ghostty references from the operator

- `time cat color-random.bin` (1.1 GB): 14.2 s total.
- `time cat color-emoji-random.bin` (749,801,000 bytes) on a busy machine: `0.01s user 5.30s system 12% cpu 41.827 total`, about 17.9 MB/s.

The color-emoji-random corpus is the operator's primary acceptance test. The generator is reproduced in the M.1 bead and in the epic description.
