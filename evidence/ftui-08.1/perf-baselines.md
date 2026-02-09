# FTUI-08.1 Performance Baselines

**Date:** 2026-02-09
**Benchmark:** `cargo bench -p wa-core --features ftui --bench tui_rendering`
**Environment:** Linux 6.17.0-8-generic, rustc nightly, bench profile (optimized)

---

## 1  Per-View Rendering (80x24, small dataset)

| View | p50 (µs) | p99 est (µs) | Budget |
|------|----------|---------------|--------|
| Home | 18.4 | ~19.2 | PASS (< 1ms) |
| Panes | 17.6 | ~18.0 | PASS |
| Events | 22.8 | ~23.5 | PASS |
| Triage | 19.2 | ~19.8 | PASS |
| History | 14.7 | ~14.9 | PASS |
| Search | 13.9 | ~14.1 | PASS |
| Help | 15.3 | ~15.6 | PASS |

All views render in < 25 µs at standard terminal size. Well within the 1ms p50 / 5ms p99 budget.

## 2  Terminal Size Scaling

| Size | Home (µs) | Events (µs) | Budget |
|------|-----------|-------------|--------|
| 40x10 | 4.5 | 8.9 | PASS (< 500µs p50) |
| 80x24 | 15.0 | 23.3 | PASS (< 1ms p50) |
| 120x50 | 39.7 | 44.5 | PASS (< 2ms p50) |
| 200x60 | 74.8 | 85.9 | PASS |

Rendering scales linearly with terminal area. Even at 200x60 (12,000 cells), frames complete in < 90 µs.

## 3  Data Scale Impact

| Benchmark | Small (3p/5e) | Large (20p/100e) | Ratio |
|-----------|---------------|------------------|-------|
| Events @80x24 | 19.9 µs | 27.5 µs | 1.38x |
| Panes @80x24 | — | 25.8 µs | — |

Data scale has modest impact (~38% increase for 20x more data). The visible-row clamp limits rendering work regardless of total dataset size.

## 4  Key Event Processing

| Key | Latency | Budget |
|-----|---------|--------|
| Tab (view switch) | 9.0 ns | PASS (< 100µs p50) |
| Down (list scroll) | 131.8 ns | PASS |

Key processing is sub-microsecond. The state machine update path is pure computation with no allocations.

## 5  Data Refresh

| Dataset | Latency | Budget |
|---------|---------|--------|
| Small (3 panes, 5 events) | 17.5 µs | PASS (< 500µs p50) |
| Large (20 panes, 100 events) | 179.8 µs | PASS |

Data refresh includes QueryClient calls + view model adaptation. Even with 100 events, well within the 500µs budget.

## 6  60fps Budget Analysis

Target: < 16.67ms per frame (60 fps).

**Worst case measured:** 85.9 µs (Events @200x60) = **0.52% of frame budget**.

The rendering pipeline has > 190x headroom against the 60fps target at the largest tested terminal size.

## 7  Benchmark Infrastructure

- **File:** `crates/wa-core/benches/tui_rendering.rs`
- **Groups:** 5 (per_view, sizes, data_scale, update_key, refresh_data)
- **Total benchmarks:** 22
- **Framework:** Criterion 0.7 with 100-sample collection
- **Mock:** `BenchQuery` implementing `QueryClient` (no DB overhead)

## 8  Verdict

**ALL BUDGETS PASS.** The ftui rendering pipeline meets all performance targets with substantial headroom. No performance regressions detected. These baselines can be used for CI regression gating.
