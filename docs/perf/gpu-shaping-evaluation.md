# GPU Shaping Evaluation

Bead: `ft-mpc9b.6.9`

Decision: defer compute-shader text shaping.

## Scope

This spike evaluated whether FrankenTerm should ship a GPU compute path for text
shaping during heavy output bursts. The proposed bar was a minimal Latin-range
kernel plus a benchmark decision: ship only if burst-mode shaping is at least
2x faster than CPU HarfBuzz while preserving glyph-run correctness.

## Current CPU Path

The current path is still HarfBuzz-backed:

- `frankenterm/font/src/shaper/harfbuzz.rs` owns shaping, fallback, features,
  presentation handling, direction, cluster mapping, and metrics.
- `crates/frankenterm-gui/src/termwindow/render/mod.rs` resolves cached cluster
  shapes through `cached_cluster_shape`.
- `crates/frankenterm-gui/src/shapecache.rs` converts HarfBuzz `GlyphInfo`
  into render-facing `ShapedInfo` and already has focused shaping fixtures plus
  a CPU shaping microbench.

That means a GPU path must be semantically identical across cluster indices,
fallback fonts, ligatures, presentation width, bidi direction, and complex
script fallback. An ASCII-only kernel would not be a drop-in replacement; it
would need a strict dispatch gate and a golden corpus proving exact equivalence
for every accepted run.

## Local Baseline

Command:

```bash
FT_LOCAL_TARGET=/tmp/ft-cod7-target scripts/cargo-local.sh test -p frankenterm-gui --bin frankenterm-gui shapecache::test::bench_shaping -- --nocapture
```

Result:

```text
100: 787.37us
1000: 3.8469ms
10000: 34.575454ms
test shapecache::test::bench_shaping ... ok
```

This is a debug-profile microbench, so it is not a product latency claim. It is
enough for this spike because the GPU proposal also lacks a correctness-complete
kernel, queue handoff, or benchmark harness. A compute prototype would first
need to beat these CPU numbers after GPU upload, dispatch, readback or buffer
handoff, cache lookup, and fallback-gate costs are included.

## Defer Rationale

Do not ship a compute-shader shaping path now.

- The existing CPU path has correctness responsibilities that are larger than
  "Latin glyph index plus advance": fallback font selection, HarfBuzz features,
  bidi direction, presentation width, clusters, and ligatures.
- The renderer already avoids repeated CPU work through shape caches; a GPU path
  would mostly target cold burst workloads, where dispatch and transfer overhead
  are hardest to amortize safely.
- The proposed 2x speedup bar is not proven. The only measured artifact in this
  spike is the current CPU baseline.
- A minimal ASCII kernel would create a second shaping implementation with a
  high risk of silently diverging from HarfBuzz. That is not acceptable without
  a golden equivalence suite.

## Required Future Gate

Re-open this only with all of the following:

1. A release-profile CPU baseline for cached and cold burst shaping.
2. A WGSL prototype that returns the same glyph ids, advances, offsets, and
   cluster indices as HarfBuzz for an explicit accepted subset.
3. A dispatch predicate that rejects any run outside that subset and falls back
   to CPU before shaping.
4. A golden corpus covering ASCII, Latin ligatures, fallback fonts, bidi, CJK,
   Arabic, Indic, emoji presentation, and combining marks.
5. End-to-end timing that includes upload, dispatch, synchronization, and
   render integration costs.

Until those gates exist, the correct implementation path is to keep CPU
HarfBuzz as the single shaping authority and optimize queueing/cache behavior
around it.
