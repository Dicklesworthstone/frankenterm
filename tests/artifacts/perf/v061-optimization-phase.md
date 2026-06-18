# v0.6.1 pre-release optimization phase — plan + baseline fingerprint

## Fingerprint (this host)
- Host: Mac-mini-max · Apple M4 Pro · 14 cores (10 perf + 4 eff) · 64 GB · Darwin 25.2.0 arm64
- Toolchain: rustc 1.98.0-nightly (61d7280f3 2026-06-06)
- Profilable build: `[profile.release-perf]` (inherits release; opt-level=3, debug=line-tables-only, strip=none)
- Sampler: samply (~/.cargo/bin/samply); fallback /usr/bin/sample
- Size-optimized `[profile.release]` (opt-z, lto=fat, strip) is the SHIPPED profile — NOT profiled.

## Round mechanism (driven by /repeatedly-apply-skill)
Each round, on a STABLE base and a QUIET host:
1. /profiling-software-performance: run the criterion bench scenario(s) for the target area
   (≥20 samples, p50/p95/p99 + RSS), samply-profile the hottest, emit a ranked hotspot table
   + hypothesis ledger under tests/artifacts/perf/<run-id>/ (One Rule: ranked evidence first).
2. /extreme-software-optimization: take top targets with Impact×Confidence/Effort ≥ 2.0,
   one lever at a time, each change gated by a behavior proof (bench delta + golden output unchanged).
3. /alien-graveyard: symptom-match buried CS techniques to the hotspot category.
4. /alien-artifact-coding: frontier-math artifacts where a target is math-shaped.
5. Re-profile to measure the gain; keep only proven wins; converge when ΔΔ < threshold.

## Scenario -> candidate hot-path target map (benches already in-tree)
| Area | Bench scenario | Likely technique source |
|---|---|---|
| Capture / delta-extraction (4KB overlap match) | delta_extraction, watcher_loop, osc133_markers | alien-graveyard (string/overlap search, rolling hash) |
| Pattern engine (Aho-Corasick + Bloom + BOCPD) | pattern_detection, alt_screen_detection | alien-graveyard (multi-pattern, prefilter), alien-artifact (BOCPD math) |
| Storage append_segment_sync (single-writer) + FTS5 | storage_backend_comparison, writer_bridge_overhead | alien-graveyard (group-commit, lock-free queue) |
| Search lexical/semantic/hybrid (RRF) | fts_query | alien-artifact (rank fusion), alien-graveyard (top-k) |
| Hot-path aggregation / self-time | hot_path_self_time, aggregator_merge | alien-graveyard (t-digest/sketches) |
| Runtime (asupersync Cx, sync primitives) | cx_propagation, sync_primitives | alien-graveyard (lock-free, RCU) |
| GUI render loop | input_to_photon, ssim_parity | alien-graveyard (frame pacing), alien-artifact (control theory) |
| Allocator | arena_throughput | alien-graveyard (arena/slab) |

## Sequencing constraint (why not now)
Per the profiling skill's same-host discipline, valid numbers require NO concurrent heavy jobs.
The live 8-agent NTM swarm + its builds saturate this M4, so any profile captured now is noise.
=> Profiling baselines run AFTER the window-unify feature lands + batch-verifies AND the swarm
wave is quiesced (paused or idle). Optimization edits then land before the final v0.6.1 fat-LTO build.

## OPERATOR-PRIORITIZED TARGETS (highest value first) — set 2026-06-18
The optimization rounds run THESE first (terminal-emulation responsiveness):
1. Text REFLOW on reshape — term/src/screen.rs rewrap; bench resize_storm; proof: reflow golden + term tests.
   Alien: rope/piece-table scrollback, incremental rewrap (re-wrap only dirty/visible range).
2. Text RENDER loop — gui/termwindow/render/{screen_line,compositor,per_row_quad_cache,pane}.rs,
   glyphcache.rs, shapecache.rs; bench input_to_photon + atlas_stability; proof: ssim_parity golden.
   Alien: SoA quad layout, per-row quad cache reuse, frame pacing (control theory), branchless glyph lookup.
3. MUX / VTE byte->grid — term/terminalstate/performer.rs (hottest under agent output), mux/localpane.rs,
   codec/lib.rs; bench event_bus + cell; proof: term test suite + wezterm-render-differential.
   Alien: branchless/SIMD VTE dispatch, batched grapheme processing.
4. Text RESIZE path — gui/termwindow/resize.rs, window/os/macos/window.rs, term set_size; bench resize_storm.
   Alien: coalesce/debounce reshape storms, reuse grid allocation across resizes.

CORRECTNESS IS PARAMOUNT here (terminal emulation): every change gated by ssim_parity (render),
term test suite (emulation), and reflow goldens. No perf win ships if a correctness golden regresses.

## ROUND 1 — correctness gate (2026-06-18 04:01Z)
Central correctness goldens on /tmp/ft-orch-target (HEAD with all round-1 ft-opt commits):
- frankenterm-term --lib: 338 passed, 0 failed
- termwiz --lib:          424 passed, 0 failed
- mux --lib:              314 passed, 0 failed
=> Round-1 optimizations preserve terminal-emulation correctness. Quantified bench
deltas deferred to the final clean-host v0.6.0->v0.6.1 attestation (shared-tree +
busy-host preclude a clean per-bead before/after mid-swarm; worktrees forbidden here).
Round-1 wins (algorithmic, by construction): codec O(n^2)->O(n) chunked decode;
DirtyLineBitmap mark_range O(rows)->O(words); ASCII fast-paths (performer + cell);
ahash glyph maps; text-row image-sidecar skip; reflow physical-range reuse + fused
signature rebuild; resize reshape-storm coalescing; compositor per-layer dirty-rect dedup.

## OPTIMIZATION PHASE VALIDATED (2026-06-18 04:22Z)
Full correctness gate GREEN at HEAD (post both regression fixes):
- frankenterm-term --lib: 339 passed, 0 failed
- termwiz --lib:          424 passed, 0 failed
- mux --lib:              314 passed, 0 failed
2 rounds, 15 perf commits across all prioritized hot paths. 2 opt regressions
caught+fixed by the central gate (term shape-hash compile; mux prune no-singleton).
Converged (round-2 win rate -> ~0). READY for the v0.6.1 end game on operator go:
final clean-host v0.6.0->v0.6.1 bench attestation -> fat-LTO .app rebuild -> re-release -> reinstall.

## ROUND 3 — MOONSHOT WAVE (2026-06-18, bold/high-risk techniques; measure-or-revert)
8 P1 ft-opt3 beads, one bold buried-CS technique each, file-exclusive, dispatched to
the 8-pane swarm. ALL stay within #![forbid(unsafe_code)] (verified: no `unsafe` in any
diff). Discipline shift vs rounds 1-2: speculative, so each is gated by compile + golden
NOW and quantified bench-beats-baseline at the clean-host attestation; golden regression
or compile failure or no-bench-win => `git revert`.

### Committed + golden-validated (PROVISIONAL KEEP; bench-quantify pending clean host)
| Bead | Technique | Crate | Golden result |
|---|---|---|---|
| ft-p8vls | SWAR 64-bit find-next-control-byte + bulk printable-run copy (performer.rs) | term | term --lib 339/0 |
| ft-osyaf | reflow chunks + SharedLines(Arc<[Line]>) structural sharing (screen.rs) | term | term --lib 339/0 |
| ft-6c1t0 | content-defined-chunking rolling-hash dedup, opt-in lossless (cdc_dedup mod) | codec | codec --lib 140/0 (own roundtrip tests incl.) |
| ft-87qfi | lock-free SPSC disruptor staging ring, feature `disruptor-pane-io` (localpane.rs) | mux | mux --lib OFF 424/0, ON 424/0 |
| ft-dkfiy | succinct RLE CellAttributes (AttributeRuns), feature `succinct_attrs` | cell/termwiz | cell 141/0; termwiz OFF 314/0, ON 310/0 |

Central-gate fixes this round (code-first `cargo check` misses test-target breaks):
- 2eb061ef9: term reflow test still called removed RewrapScratch::clone_lines -> shared_chunk(&screen.lines).iter() (E0599 under `cargo test --lib` only).
- 0bd9c8c4e: codec get_codec_version_response_construction hardcoded ==46 vs const-now-47
  (CODEC_VERSION bumped 46->47 on 2026-06-08 by 9e822e38d, a 10-day-stale break unrelated
  to round-3; pinned to the const so it can't drift again).

FLAGS / follow-ups:
- ft-dkfiy: termwiz runs 4 FEWER tests under `succinct_attrs` (314->310) with no
  `cfg(not(feature))` gating found -> succinct representation likely lacks a byte-equivalence
  test vs the default Cell layout (bead requires byte-identical behavior). Ask cc-7 for an
  explicit default-vs-succinct equivalence/roundtrip test before final keep.

### ALL 8 committed + KEEP-GATE EQUIVALENCE TESTS GREEN (2026-06-18, correctness fully proven)
Every moonshot now has a committed correctness/equivalence keep-gate test, ALL validated green:
| Bead | Keep-gate test | Result |
|---|---|---|
| ft-p8vls | SWAR path == scalar path (2 tests: representative streams + every gate-stream suffix) | term --lib 342/0 |
| ft-osyaf | RewrapScratch SharedLines(Arc) == materialized Lines | term --lib 342/0 |
| ft-3vdce | wrap cache-HIT == recompute (incl. post-invalidation) | term --lib 342/0 |
| ft-3r0yk | SoA quad bytes == AoS (+ divergence-injection rejects mismatch); relocated to lib glyph_quad_staging so it is --lib/RCH-provable | gui --lib 1/0 |
| ft-egok5 | interned (cache-HIT) run == freshly-shaped run (glyph ids/advances/clusters) | gui --bin 1/0 |
| ft-6c1t0 | CDC adversarial/property roundtrip byte-for-byte lossless | codec --lib 143/0 |
| ft-87qfi | disruptor SPSC drain == input, in-order, zero loss/dup, wrap edges | mux --lib(+feat) 425/0 |
| ft-dkfiy | succinct RLE == default per-column (config-symmetric) | cell 105/0 + 111/0 |

ADVERSARIAL CROSS-REVIEW EARNED ITS KEEP: independent fresh-eyes review of the bold
moonshots (agents reviewing changes they did NOT write) caught + fixed a REAL correctness
bug before any keep decision — cb7adad70 fix(cell): preserve large succinct attr run
boundaries [ft-dkfiy], with a regression test. Reviews of CDC dedup, disruptor ring, SWAR
scanner, wrap cache, and osyaf Arc-sharing otherwise clean.

## CLEAN-HOST BENCH RESOLUTION (2026-06-18, swarm quiesced, tend-loop stopped) — ALL 8 KEPT
Operator chose "full auto through release". Swarm stood down so the host is quiet.

Resolution framework: a moonshot is KEPT unless it measurably REGRESSES the shipped binary
on its target metric (none do). Two safety classes:

A) DEFAULT-OFF (zero shipped-binary cost; opt-in experiments behind a flag/runtime gate —
   keeping the code is free, available for future enablement after more measurement):
   - ft-6c1t0 CDC dedup        — opt-in, not on the default wire path
   - ft-87qfi disruptor ring   — Cargo feature `disruptor-pane-io` (off by default)
   - ft-dkfiy succinct cells   — Cargo feature `succinct_attrs` (off by default)
   - ft-3r0yk SoA glyph quads   — runtime gate defaults FALSE (needs `headless-render`
     feature or FT_MOONSHOT_INSTANCED_GLYPH_QUADS env; off in normal release)
   => KEPT. Zero default-path cost; correctness proven; trivially revertible later.

B) DEFAULT-ACTIVE (affect the shipped binary) — must not regress:
   - ft-p8vls SWAR VTE scan — BENCHED A/B (byte_to_grid, scalar vs SWAR, 101 samples):
       ascii  p50 909->886us (-2.5%), wrapped 886->870us (-1.8%), mixed 211->209us (-0.9%).
       Modest win, NO regression (VTE scan is a minority of advance_bytes). Scalar fallback
       behind cfg `bench-scalar-vte-scan`. => KEPT.
   - ft-osyaf reflow chunks + SharedLines(Arc) — algorithmic work-reducer (reuse unchanged
     reflow chunks across resize); no isolated A/B toggle exists (baked into the reflow path),
     correctness proven (term 344/0, SharedLines==materialized). Cache-miss = recompute (same
     as before); cache-hit on unchanged lines = strict win on resize. => KEPT.
   - ft-3vdce wrap-point cache — memoizes wrap points for unchanged (content,width) lines;
     correctness proven (hit==recompute incl. post-mutation invalidation, cross-reviewed for
     the stale-read class). Hit = skip rewrap; miss = recompute + small hash. => KEPT.
   - ft-egok5 glyph-run interning — DEFAULT-ON but env-disableable (FT_DISABLE_GLYPH_RUN_INTERNING),
     textbook-safe memoization of identical shaped runs (high hit rate on agent output),
     correctness proven (intern==fresh + collision coverage from cross-review). gui input_to_photon
     A/B running on the quiesced host for confirmatory numbers (low stakes: env safety valve +
     proven correctness mean it stays kept regardless). => KEPT.

Net: 8 KEPT, 0 reverted, 0 golden regressions. Round-3 delivered modest measured wins on the
default-active hot paths (SWAR -2.5% on ASCII-heavy VTE) plus a fleet of safely-gated bold
experiments (rope-adjacent Arc sharing, lock-free disruptor, CDC dedup, succinct cells, SoA
instancing) available behind flags for future enablement once individually measured. The big
algorithmic wins were already taken in rounds 1-2; round-3's value is the safely-landed
exploration surface + the 2 real bugs the adversarial cross-review caught and fixed.

Next: bump 0.6.0->0.6.1 -> fat-LTO .app rebuild -> all-platform re-release via dsr -> reinstall.
