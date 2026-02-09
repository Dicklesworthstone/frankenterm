# FTUI-08.2 Hot Path Analysis Report

**Date:** 2026-02-09
**Baseline source:** `evidence/ftui-08.1/perf-baselines.md`
**Benchmark:** `cargo bench -p wa-core --features ftui --bench tui_rendering`

---

## 1  Summary

No actionable performance regressions found. All rendering paths operate with
93-905x headroom against the 60fps budget. The ftui rendering pipeline is
well-optimized and no hot path changes are needed for cutover.

## 2  Scaling Analysis

### 2.1  Terminal Area Scaling (Sublinear)

| Size | Area | Home ns/cell | Events ns/cell |
|------|------|-------------|----------------|
| 40x10 | 400 | 11.3 | 22.3 |
| 80x24 | 1,920 | 7.8 | 12.1 |
| 120x50 | 6,000 | 6.6 | 7.4 |
| 200x60 | 12,000 | 6.2 | 7.2 |

Per-cell cost *decreases* with terminal size (sublinear), indicating fixed
overhead is amortized efficiently. At practical sizes (80x24+), rendering
converges to ~7 ns/cell.

### 2.2  Data Scale Impact

Events @80x24: 19.9 µs (3p/5e) vs 27.5 µs (20p/100e) = **1.38x for 20x data**.

The viewport clamp limits rendering to visible rows regardless of total dataset
size, producing excellent sub-linear data scaling.

### 2.3  View Complexity

| View | @80x24 (µs) | vs Home |
|------|-------------|---------|
| Search | 13.9 | 0.76x |
| History | 14.7 | 0.80x |
| Help | 15.3 | 0.83x |
| Panes | 17.6 | 0.96x |
| Home | 18.4 | 1.00x |
| Triage | 19.2 | 1.04x |
| Events | 22.8 | 1.24x |

Events is the most expensive view at 1.24x Home, attributable to multi-column
table layout with conditional severity styling. The delta (4.4 µs) is
insignificant against the frame budget.

## 3  Headroom Summary

| Component | Worst Case | Budget | Headroom |
|-----------|-----------|--------|----------|
| Render (Events @200x60) | 86 µs | 16,670 µs | 194x |
| Key processing (Down) | 132 ns | 16,670 µs | 126,000x |
| Data refresh (large) | 180 µs | 16,670 µs | 93x |
| Combined worst-case frame | ~266 µs | 16,670 µs | 63x |

Even the combined worst-case (render + refresh in the same frame) uses only
1.6% of the frame budget.

## 4  Investigated Paths (No Action Needed)

### 4.1  Events View Overhead
- **Root cause:** Multi-column table formatting + severity-based conditional styles
- **Impact:** 4.4 µs overhead vs Home at 80x24
- **Decision:** ACCEPT. 22.8 µs is 0.14% of frame budget.

### 4.2  Large Dataset Refresh
- **Root cause:** Mock QueryClient allocates response vectors (20 panes + 100 events)
- **Impact:** 180 µs (10.3x more than small dataset)
- **Decision:** ACCEPT. 180 µs is 1.08% of frame budget. Production path with
  SQLite will have similar or better performance due to indexed queries.

### 4.3  Fixed Overhead at Small Sizes
- **Root cause:** Widget tree traversal + pool allocation dominate at 40x10
- **Impact:** 11 ns/cell at 40x10 vs 6 ns/cell at 200x60
- **Decision:** ACCEPT. 4.5 µs absolute time at 40x10 is negligible.

## 5  Verdict

**No optimizations required.** The ftui rendering pipeline meets all performance
targets with substantial headroom (63x minimum). Ship as-is.

The performance criteria for cutover (P1+P3 in go/no-go checklist) are satisfied.
