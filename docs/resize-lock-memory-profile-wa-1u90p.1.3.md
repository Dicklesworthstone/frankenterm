# Resize Lock/Memory Profiling Snapshot (`wa-1u90p.1.3`)

Date: 2026-02-14  
Author: `LavenderCastle`

## Scope

Produce lock-contention and memory-attribution evidence for resize/reflow paths, with reproducible artifact pointers feeding:

- `wa-1u90p.1.4` (SLO calibration)
- `wa-1u90p.1.5` (baseline bottleneck dossier)

## Artifact Index

- `evidence/wa-1u90p.1.3/summaries/simulate_cli_parse.log`
- `evidence/wa-1u90p.1.3/summaries/simulation_harness_build.log`
- `evidence/wa-1u90p.1.3/summaries/simulation_harness_build_error_summary.txt`
- `evidence/wa-1u90p.1.3/summaries/simulate_cli_parse_2026-02-14.log`
- `evidence/wa-1u90p.1.3/summaries/simulation_resize_suite_no_run_2026-02-14.log`
- `evidence/wa-1u90p.1.3/summaries/resize_single_pane_timeline.json`
- `evidence/wa-1u90p.1.3/summaries/resize_multi_tab_storm_timeline.json`
- `evidence/wa-1u90p.1.3/summaries/font_churn_multi_pane_timeline.json`
- `evidence/wa-1u90p.1.3/summaries/mixed_scale_soak_timeline.json`
- `evidence/wa-1u90p.1.3/summaries/resize_baseline_timeline_rollup_2026-02-14.json`
- `evidence/wa-1u90p.1.3/summaries/runtime_lock_memory_telemetry_refs.txt`
- `evidence/wa-1u90p.1.3/summaries/localpane_resize_telemetry_refs.txt`
- `evidence/wa-1u90p.1.3/summaries/docs_cross_refs.txt`

## Commands Executed

```bash
target/debug/ft simulate run fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml --json

CARGO_TARGET_DIR=target-lavender-wa1u90p13 \
  cargo test -p frankenterm-core simulation_resize_suite -- --nocapture
```

## Current Measurement Blockers

### 1. `ft simulate run` replay status (updated 2026-02-14, `VioletDune`)

The prior `generate_scrollback` parse failure was caused by invoking a stale prebuilt `target/debug/ft` binary (dated 2026-02-11), not by current source.

Fresh replay verification from source:

- `CARGO_TARGET_DIR=target-violetdune cargo run -p frankenterm -- simulate run fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml --json`
- Result: scenario parses and executes all 9 events (including `GenerateScrollback`) and all 3 expectations pass.

### 2. `frankenterm-core` compile-harness status (updated 2026-02-14)

Historical compile blockers captured in `simulation_harness_build_error_summary.txt` have been resolved by subsequent compile-unblock slices.

Fresh verification:

- `CARGO_TARGET_DIR=target-violetdune cargo check --all-targets` (workspace) ✅
- `CARGO_TARGET_DIR=target-violetdune cargo check -p frankenterm-core --all-targets` ✅
- `CARGO_TARGET_DIR=target-violetdune cargo test -p frankenterm-core --test simulation_resize_suite -- --nocapture` ✅ (`4/4`)

Timeline capture verification:

- `CARGO_TARGET_DIR=target-violetdune cargo run -p frankenterm -- simulate run fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml --json --resize-timeline-json > evidence/wa-1u90p.1.3/summaries/resize_single_pane_timeline.json`
- Artifact summary: `executed_resize_events=8`, stage coverage includes all required stages (`input_intent`, `scheduler_queueing`, `logical_reflow`, `render_prep`, `presentation`) with `flame_samples=40`.

## Baseline Timeline Snapshot Summary (2026-02-14, `VioletDune`)

All canonical resize-baseline fixtures now execute via `cargo run -p frankenterm -- simulate run ... --json --resize-timeline-json` and emit timeline artifacts.

| Scenario | Executed resize events | `logical_reflow.max_duration_ns` | Flame samples |
|---|---:|---:|---:|
| `resize_single_pane_scrollback` | 8 | 3,766,917 | 40 |
| `resize_multi_tab_storm` | 24 | 325,917 | 120 |
| `font_churn_multi_pane` | 24 | 216,084 | 120 |
| `mixed_scale_soak` | 28 | 671,167 | 140 |

## Consolidated Rollup Artifact

Generated:

- `evidence/wa-1u90p.1.3/summaries/resize_baseline_timeline_rollup_2026-02-14.json`

This rollup consolidates per-scenario:

- stage `avg/p95/max` durations,
- scheduler queue-depth peaks (`depth_before`, `depth_after`),
- event/sample counts for direct ingestion by the baseline bottleneck dossier.

## Hotspot Findings (from rollup)

### Queueing pressure profile

| Scenario | `scheduler_queue_depth.max_before` | `scheduler_queue_depth.max_after` | `scheduler_queue_depth.min_after` |
|---|---:|---:|---:|
| `resize_single_pane_scrollback` | 8 | 7 | 0 |
| `resize_multi_tab_storm` | 24 | 23 | 0 |
| `font_churn_multi_pane` | 24 | 23 | 0 |
| `mixed_scale_soak` | 28 | 27 | 0 |

Interpretation:

- Queue depth scales with event fanout as expected and drains back to `0` in all scenarios (no persistent queue accumulation in this simulation path).
- `mixed_scale_soak` is the highest scheduler-pressure workload in this suite (`max_before=28`).

### Stage-latency profile

| Scenario | `logical_reflow.p95_duration_ns` | `logical_reflow.max_duration_ns` | `presentation.p95_duration_ns` | `presentation.max_duration_ns` |
|---|---:|---:|---:|---:|
| `resize_single_pane_scrollback` | 3,000 | 3,766,917 | 4,458 | 234,708 |
| `resize_multi_tab_storm` | 285,833 | 325,917 | 12,875 | 15,750 |
| `font_churn_multi_pane` | 190,834 | 216,084 | 8,792 | 10,708 |
| `mixed_scale_soak` | 530,000 | 671,167 | 38,875 | 118,250 |

Interpretation:

- Primary latency lane is `logical_reflow`; it dominates maxima in all four scenarios.
- `resize_single_pane_scrollback` shows the most extreme isolated spike (`3,766,917 ns`) with low p95, indicating a heavy outlier pattern rather than steady pressure.
- `mixed_scale_soak` shows the heaviest sustained tail pressure (`logical_reflow.p95=530,000 ns`, highest `presentation` p95/max among multi-pane scenarios).

## Lock/Memory Telemetry Anchors Confirmed

From `crates/frankenterm-core/src/runtime.rs` and reference extract:

- lock wait metrics:
  - `max_storage_lock_wait_ms`
  - `avg_storage_lock_wait_ms`
  - `storage_lock_contention_events`
- lock hold metrics:
  - `max_storage_lock_hold_ms`
  - `avg_storage_lock_hold_ms`
- cursor snapshot memory metrics:
  - `cursor_snapshot_bytes_last`
  - `cursor_snapshot_bytes_max`
  - `avg_cursor_snapshot_bytes`

Warning thresholds encoded in runtime:

- storage lock wait: `15.0 ms`
- storage lock hold: `75.0 ms`
- cursor snapshot memory: `64 MiB`

## Resize Path Observability Anchors Confirmed

From `frankenterm/mux/src/localpane.rs` and reference extract:

- queue/coalescing signals:
  - `replaced_seq`
  - `worker_spawned`
  - `queue_wait_us`
  - `completion_us`
- lock/resize phase timing:
  - `probe_lock_wait_us`
  - `pty_lock_wait_us`
  - `pty_resize_us`
  - `terminal_apply_lock_wait_us`
  - `terminal_resize_us`

This instrumentation is sufficient for per-phase lock attribution once harness execution is unblocked.

## Immediate Next Steps

1. Feed rollup metrics from `resize_baseline_timeline_rollup_2026-02-14.json` into `docs/resize-baseline-bottleneck-dossier.md` to update intervention ranking inputs for `wa-1u90p.1.5`.
2. Add lock/memory percentile captures from live runtime telemetry surfaces (using existing runtime metrics fields) to complete the final lock/memory growth curve requirement for this bead.
3. Keep `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` issues tracked separately as repo-wide hygiene debt (not specific blockers for this profiling run).
