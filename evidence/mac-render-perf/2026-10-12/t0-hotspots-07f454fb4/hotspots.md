# T0 at HEAD: where the GUI spends its time during `cat color-emoji-random.bin`

The M.0 deliverable of ft-yccm0.1.1: what HEAD (07f454fb4) does under the
operator's primary test, measured rather than inferred. Machine, build and
corpus are in `fingerprint.json`.

## Method

- **Profile:** `frankenterm-gui` built with `release-perf` and frame pointers,
  run by `scripts/mac-gui-throughput.sh` (FrankenTerm against itself,
  `--reps 2`), so all four runs were floods.
- **Sampling:** each run's GUI process was sampled by `/usr/bin/sample` for 7 s
  of its drain (1 ms interval, `profile-run.log`). The four samples are
  `sample-run*.txt.gz`.
- **Aggregation:** `aggregate_samples.py` merges them, using
  `analyze_sample.py`. A thread is on CPU unless its leaf frame is a wait
  (`read`, `__select`, `__psynch_cvwait`, `mach_msg2_trap`, semaphore waits).
- **Comparison:** separately, the shipped-profile bundle of the same SHA ran
  against Ghostty 1.3.2 (`ft-vs-ghostty-receipt.json`), with every `cat`'s
  priority logged twice a second (`cat-priority.log`).
- **Caveat:** sampling and host load (1-minute load 28-31) stretched the profiled
  drains to 12.8-17.9 s, so these shares rank costs; they do not time them.

## Threads (4 samples, about 8,500 per thread)

| Thread | On CPU | What it waits on otherwise |
|---|---|---|
| main (event loop and render) | 88% | `mach_msg2_trap` (the run loop) 9% |
| scrollback-durability | 86% | `__psynch_cvwait` 11% |
| mux-parse-pane-0 | 34% | `__select` 59%: waiting for bytes from the reader |
| mux-read-pane-0 | 9% | PTY `read` 63%, `__select` on the PTY 27% |

**The drain is producer-bound.** The reader spends 90% of its time waiting for
the PTY, and the parser 59% waiting for the reader. FrankenTerm takes bytes as
fast as `cat` can push them through the kernel tty. A faster parser, store or
renderer would not shorten this drain on this host.

## Ranked self time (share of each thread's on-CPU samples)

### main (event loop and render)

| Share | Frame |
|---|---|
| 9.6% | `_platform_memmove` |
| 5.9% | `mach_absolute_time` |
| 4.4% | `HarfbuzzShaper::do_shape` |
| 4.7% | SipHash `write` and `hash_one` in `frankenterm_font` |
| 2.7% | `TermWindow::build_line_element_shape` |
| 2.5% | `TermWindow::render_screen_line` |
| 4.9% | jemalloc `malloc` and `sdallocx` |
| 2.0% | `HeapQuadAllocator::apply_to` |
| 2.2% | metrics `Recorder::register_histogram` and `HistogramFn::record` |
| 1.6% | `powf` |
| 1.4% | `ShapedInfo::process` |
| 1.2% | `_cairo_tor_scan_converter_generate` (CPU rasterization on the main thread) |
| 1.1% | `GlyphCache::cached_glyph_for_subpixel_bin_with_status` |

### scrollback-durability

| Share | Frame |
|---|---|
| 11.0% | SHA-256 `compress256` |
| 8.9% | `__fcntl` (the barrier syncs) |
| 12.9% | zstd (`compressBlock_fast`, `FSE_buildCTable_wksp`, `encodeSequences`) |
| 6.1% | XChaCha20 and Poly1305 |
| 5.7% | `_platform_memmove` |
| 5.3% | `mmap_store::validate_append_chunks` |
| 4.5% | `exact_varbincode::Serializer::unsigned` |
| 3.8% | `__open` and `__openat` (files opened per batch) |
| 2.7% | `writev` |
| 2.0% | base64 `encode_scalar_tail` |
| 2.0% | `fstat` and `fstatat` |

### mux-parse-pane-0

| Share | Frame |
|---|---|
| 10.3% | `SlicedFeed::run` with `FusedFeedGate` |
| 9.5% | `Line::compress_for_scrollback` |
| 6.6% | `Performer::flush_print` |
| 5.7% | `mach_absolute_time` |
| 5.0% | `LineWrapGeometry::capture` |
| 5.0% | `parse_buffered_data` |
| 3.6% | `grapheme_column_width` |
| 2.8% | `Presentation::for_grapheme` |
| 2.5% | `clusterline::is_cluster_inert` |

### mux-read-pane-0

| Share | Frame |
|---|---|
| 70.9% | `write` (handing bytes to the parser) |
| 12.9% | `mach_absolute_time` |

## Hypothesis ledger

| Hypothesis | Verdict at HEAD | Evidence |
|---|---|---|
| The parse thread is bound by synced store commits (the 0.15.2 sample: ~100% in F_FULLFSYNC) | **Rejected** | The parse thread is 34% busy, with no sync frames. Syncs (`__fcntl`) now run on `scrollback-durability`. |
| The main thread blocks on the terminal mutex (0.15.2) | **Rejected** | `main` is 88% on CPU, with no `__psynch_mutexwait`; its waits are the run loop. |
| The durable store costs T0 throughput | **Rejected** (2026-10-11 store ON/OFF, 6 runs per arm) | Its thread is busy but off the critical path. |
| A larger parser buffer helps | **Rejected** (2026-10-11, 4 runs per arm) | The parser is starved, not saturated. |
| `unicode_version` 14 costs parse time | **Rejected** (`../unicode-14-default`, 16 runs per arm) | `for_grapheme` is 2.8% of the parse thread, and parse is idle 59%. |
| `cat` runs at a lower priority under FrankenTerm | **Rejected** | Every `cat` sample is priority 31, nice 0, under both terminals. |
| FrankenTerm drains slower than Ghostty | **Not separable on this host** | At load 11-13: FrankenTerm 6.93 and 6.53 s, Ghostty 6.86 and 8.26 s. Earlier the same morning at load ~12: FrankenTerm 9.56, 9.08 and 8.52 s, Ghostty 8.23, 7.34 and 6.21 s. Both swing ±25% run to run with the host's load (11 to over 80 within minutes). |

## What this means for the campaign

- **Throughput:** nothing inside FrankenTerm is the bottleneck for T0 at HEAD;
  the kernel tty and `cat` are. A Ghostty comparison on this host needs a
  pinned, quiet machine (the harness's own verdict was NO_ADMISSIBLE_RATIO, CV
  above 5%) or many more interleaved runs.
- **CPU cost is real.** While draining, FrankenTerm keeps about two cores busy
  (render 0.9, durable store 0.9, parse 0.3, read 0.1). On a loaded host or on
  battery that matters. Targets, by measured share:
  1. **Store:** SHA-256 (11%), zstd (13%), syncs (9%), per-batch `open` and
     `fstat` (6%), base64 (2%). Hashing and encoding the same bytes twice
     would be the first thing to check.
  2. **Render:** `memmove` (10%), SipHash shape-key hashing (5%), allocation
     (5%), per-frame metrics histogram registration (2%), `powf` (2%), cairo
     on the main thread (1%).
  3. **`mach_absolute_time`:** 6% of `main`, 6% of the parser and 13% of the
     reader. Timestamps taken per item in hot loops.
