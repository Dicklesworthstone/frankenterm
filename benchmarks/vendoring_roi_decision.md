# Vendoring ROI Decision Baseline (wa-nu4.4.1.6)

## Summary
This document captures the **local benchmark baseline** for the watch-loop hot path
and defines **decision thresholds** for whether WezTerm vendoring is worth the
maintenance cost. It does **not** include WezTerm CLI polling latency/gap rate
because this environment is headless.

**Current decision:** **NO‑GO (provisional)** until real WezTerm CLI measurements
are gathered on a desktop environment. The microbench results below suggest the
core in-process logic is already fast; the remaining risk/latency likely sits
in `wezterm cli` I/O, which vendoring might address.

## Environment
- Date: 2026-01-29
- OS: Linux x86_64 (headless)
- CPU: AMD EPYC 7282 16‑Core Processor
- Rust: 1.95.0-nightly
- Command: `cargo bench -p wa-core --bench watcher_loop -- --sample-size 10`

## Benchmarks (watcher_loop)
These measure **filtering + fingerprinting** cost inside the watch loop (no
WezTerm CLI I/O).

- `watcher_pane_filter/typical_filter_no_match`: ~258.35 µs
- `watcher_pane_filter/typical_filter_match`: ~252.63 µs
- `watcher_pane_filter/heavy_filter_no_match`: ~1.12 µs
- `watcher_pane_filter/heavy_filter_match`: ~1.53 µs
- `watcher_fingerprint/fingerprint_without_content`: ~79.8 ns
- `watcher_fingerprint/fingerprint_with_content_small`: ~376.6 ns
- `watcher_fingerprint/fingerprint_with_content_large`: ~4.57 µs
- `watcher_combined_check/filter_and_fingerprint`: ~255.7 µs
- `watcher_combined_check/check_10_panes`: ~2.63 ms
- `watcher_combined_check/check_many_panes/50`: ~12.69 ms

Notes:
- These numbers show the in-process **filter/fingerprint logic is not the bottleneck**.
- Vendoring should only proceed if WezTerm CLI I/O dominates and can be reduced.

## Missing Measurements (Required for final decision)
The task requires CLI polling baseline under realistic workloads:
- `wezterm cli list` + `wezterm cli get-text` latency per pane
- CPU usage of the polling loop at 1, 4, 10, 50 panes
- Gap rate under fast output (e.g., 10k lines/sec)

These require a real WezTerm GUI session and cannot be run in this headless env.

## Decision Thresholds (Go/No‑Go)
Proceed with vendoring only if **all** are met in real measurements:
- **CPU:** ≥5x reduction in polling CPU for 10 panes (e.g., 5% → 1%)
- **Latency:** average capture latency **< 50ms** (or ≥3x improvement vs CLI)
- **Gaps:** gap rate **< 0.1%** under sustained output
- **Complexity:** vendored API surface limited to streaming/buffer access only

If results do **not** meet thresholds, defer vendoring and keep CLI polling.

## Tentative Pain Points Vendoring Could Address
- Avoid per-poll `wezterm cli` process startup and JSON parsing
- Enable streaming/delta updates without full snapshot
- Reduce capture latency and improve high-throughput fidelity

## Next Steps
- Run measurements on a desktop WezTerm session:
  - 1 / 4 / 10 / 50 panes
  - Idle vs heavy output
  - Record CPU, latency, gap counts
- Update this document with real results and finalize go/no‑go.
