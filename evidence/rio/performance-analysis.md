# Rio Performance Analysis

Bead: `ft-34sko.6`  
Scope: `legacy_rio/rio`

## Method

This analysis is source-driven (implementation evidence), not a fresh runtime benchmark run in this bead.

## Throughput and Latency Characteristics (Parser + Runtime + Render)

### Ingest/runtime layer

- PTY ingest uses a large read buffer (`READ_BUFFER_SIZE = 1 MiB`) and bounded lock work (`MAX_LOCKED_READ = 65,535`) (`legacy_rio/rio/rio-backend/src/performer/mod.rs:32`, `legacy_rio/rio/rio-backend/src/performer/mod.rs:34`, `legacy_rio/rio/rio-backend/src/performer/mod.rs:213`).
- Lock acquisition uses `try_lock_unfair` first, with blocking fallback only after pressure increases (`legacy_rio/rio/rio-backend/src/performer/mod.rs:196`, `legacy_rio/rio/rio-backend/src/performer/mod.rs:199`).
- Parsed output triggers `Wakeup` events instead of direct rendering, enabling coalescing (`legacy_rio/rio/rio-backend/src/performer/mod.rs:221`, `legacy_rio/rio/rio-backend/src/performer/mod.rs:228`).

### Parser layer

- Parser path uses `BatchedParser<1024>`, immediate for small chunks and thresholded batching for larger chunks (`legacy_rio/rio/rio-backend/src/performer/handler.rs:487`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:515`, `legacy_rio/rio/rio-backend/src/batched_parser.rs:34`).
- Synchronized-update safety envelope: 150 ms timeout + 2 MiB sync buffer cap (`legacy_rio/rio/rio-backend/src/performer/handler.rs:30`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:33`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:593`).

### Render layer

- Non-macOS pacing is frame-timed through `vblank_interval` and `wait_until`; renders are scheduled rather than unbounded immediate redraws (`legacy_rio/rio/frontends/rioterm/src/router/mod.rs:88`, `legacy_rio/rio/frontends/rioterm/src/router/mod.rs:511`, `legacy_rio/rio/frontends/rioterm/src/router/mod.rs:569`).
- `vblank_interval` derives from monitor refresh where available (`legacy_rio/rio/frontends/rioterm/src/router/mod.rs:579`, `legacy_rio/rio/frontends/rioterm/src/router/mod.rs:585`).
- WGPU present mode is FIFO with max frame latency target 2 (`legacy_rio/rio/sugarloaf/src/context/mod.rs:236`, `legacy_rio/rio/sugarloaf/src/context/mod.rs:237`, `legacy_rio/rio/sugarloaf/src/context/mod.rs:290`, `legacy_rio/rio/sugarloaf/src/context/mod.rs:291`).

## Memory Characteristics

### Terminal and parser memory

- Scrollback baseline is large: primary grid history depth `10_000`, alt grid `0`; history can be cleared (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:448`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:449`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:2380`).
- Sync parser memory is hard-capped by `SYNC_BUFFER_SIZE`; batched parser includes shrink behavior for oversized transient buffers (`legacy_rio/rio/rio-backend/src/performer/handler.rs:33`, `legacy_rio/rio/rio-backend/src/batched_parser.rs:92`).

### Render/cache memory

- Char cache is bounded (`MAX_UNICODE_CACHE_SIZE = 4096`) and font cache is bounded (`MAX_FONT_CACHE_SIZE = 8192`) (`legacy_rio/rio/frontends/rioterm/src/renderer/char_cache.rs:9`, `legacy_rio/rio/frontends/rioterm/src/renderer/font_cache.rs:10`).
- Image atlas max texture size is clamped with dirty-upload gating (`legacy_rio/rio/sugarloaf/src/components/rich_text/image_cache/cache.rs:78`, `legacy_rio/rio/sugarloaf/src/components/rich_text/image_cache/cache.rs:292`, `legacy_rio/rio/sugarloaf/src/components/rich_text/image_cache/cache.rs:324`).

## Scaling Characteristics (Pane Churn / Concurrency)

- Per-context PTY performer model can scale thread count roughly with active contexts (`legacy_rio/rio/frontends/rioterm/src/context/mod.rs:287`, `legacy_rio/rio/frontends/rioterm/src/context/mod.rs:296`).
- Event poll queue capacity is fixed at 1024 in performer loop (`legacy_rio/rio/rio-backend/src/performer/mod.rs:332`).
- Combined behavior under heavy pane churn suggests good isolation but potential contention/cadence limits from:
  - per-context threads,
  - lock contention on terminal mutation,
  - vblank-timed redraw gating.

## Existing Performance Test Coverage and Gaps

### Existing coverage

- Renderer microbench: `frontends/rioterm/benches/renderer_bench.rs` (`legacy_rio/rio/frontends/rioterm/benches/renderer_bench.rs:147`).
- Parser criterion suites: `copa/benches/parser_benchmark.rs` (`legacy_rio/rio/copa/benches/parser_benchmark.rs:116`, `legacy_rio/rio/copa/benches/parser_benchmark.rs:248`).
- Poll benchmark: `corcovado/benches/bench_poll.rs` (`legacy_rio/rio/corcovado/benches/bench_poll.rs:9`).
- Bench declarations exist in crate manifests (`legacy_rio/rio/frontends/rioterm/Cargo.toml:109`, `legacy_rio/rio/copa/Cargo.toml:27`, `legacy_rio/rio/corcovado/Cargo.toml:57`).

### Gaps

1. No end-to-end benchmark covering PTY ingest + parser + wakeup + redraw across many panes.
2. No explicit pane-churn load benchmark with p50/p95/p99 latency outputs.
3. No long-run memory budget test combining scrollback + render caches + parser sync safeguards.

## FrankenTerm Optimization Recommendations

### 1) Add end-to-end ingest-to-present telemetry and SLO gates

- Expected impact: high. Enables real bottleneck localization and operational SLO enforcement under swarm load.
- Risk: low-to-medium. Instrumentation overhead and schema drift if not standardized.
- Downstream validation requirements:
  - Criterion: `bench_ingest_to_present_latency` and `bench_wakeup_coalescing_efficiency`.
  - Unit/property: latency accumulator correctness and monotonic ordering properties.
  - Integration: verify latency stage accounting across parser, scheduler, and renderer boundaries.
  - E2E load: deterministic burst + mixed interactive scenarios in `fixtures/rio/performance`.

### 2) Introduce adaptive batching + sync safety policy tiers

- Expected impact: medium-to-high. Better tail latency for interactive panes while preserving throughput for bulk output panes.
- Risk: medium. Policy complexity can regress determinism if not constrained.
- Downstream validation requirements:
  - Criterion: `bench_adaptive_batch_thresholds` across small/medium/large payload mixes.
  - Unit/property: policy transition correctness and no-unbounded-buffer invariant.
  - Integration: verify sync-timeout fallback ordering and no missed wakeups.
  - E2E load: deterministic sync-stall and high-output fixtures with expected fallback signatures.

### 3) Implement pane-churn scalability harness with budgets

- Expected impact: high. Directly targets the swarm operating envelope and catches regressions early.
- Risk: medium. Harness complexity and infrastructure cost.
- Downstream validation requirements:
  - Criterion: `bench_pane_churn_create_close_resize` and `bench_multi_pane_throughput`.
  - Unit/property: scheduler queue budget invariants and fairness constraints.
  - Integration: multi-pane event ordering and no-deadlock assertions.
  - E2E load: deterministic 1/10/50/100 pane scenarios with latency and memory budget checks.

### 4) Add cache-pressure budgeting + eviction telemetry

- Expected impact: medium. Improves memory predictability and tuning under heterogeneous glyph/image workloads.
- Risk: low-to-medium. Over-aggressive evictions can hurt steady-state render cost.
- Downstream validation requirements:
  - Criterion: cache hit/miss/eviction throughput under glyph diversity sweeps.
  - Unit/property: eviction correctness, bounded cache size, and no stale-handle invariants.
  - Integration: font/theme changes clear/rebuild caches without render corruption.
  - E2E load: long-run memory soak with deterministic mixed-content fixtures.

## Required Structured Telemetry and Artifacts (Downstream)

All downstream performance validation must emit structured JSONL with at least:

- `run_id`
- `scenario_id`
- `pane_count`
- `queue_depth`
- `throughput`
- `cpu_ms`
- `gpu_ms`
- `p50_ms`
- `p95_ms`
- `p99_ms`
- `failure_signatures` (array)
- `outcome`
- `error_code`

Example:

```json
{"run_id":"run-20260218-004","scenario_id":"pane-churn-50","pane_count":50,"queue_depth":14,"throughput":18234.4,"cpu_ms":122.7,"gpu_ms":41.2,"p50_ms":8.4,"p95_ms":19.7,"p99_ms":31.5,"failure_signatures":[],"outcome":"ok","error_code":null}
```

## Mandatory Downstream E2E Contract

- Script path: `tests/e2e/rio/test_performance_scaling.sh`
- Fixture path: `fixtures/rio/performance`
- Artifact path: `e2e-artifacts/rio/performance/<run_id>.jsonl`
- Required row fields:
  - `run_id`
  - `scenario_id`
  - `pane_count`
  - `throughput`
  - `cpu_ms`
  - `gpu_ms`
  - `p50_ms`
  - `p95_ms`
  - `p99_ms`
  - `outcome`
  - `error_code`

## Cross-References

- `ft-1u90p` (zero-hitch resize/reflow epic)
- `ft-283h4` (advanced algorithm/performance tracks)
- `ft-34sko.7` (Rio analysis synthesis roadmap)
