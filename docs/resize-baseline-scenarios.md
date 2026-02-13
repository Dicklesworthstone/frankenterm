# Resize Baseline Scenario Suite

Bead: `wa-1u90p.1.1`

This document defines the canonical deterministic scenario pack for worst-case resize/font-change reproduction across pane, tab, and scrollback scales.

Related contract:
- `docs/resize-performance-slos.md` (authoritative SLO thresholds + CI/soak/go-no-go gates)

## Location

- `fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml`
- `fixtures/simulations/resize_baseline/resize_multi_tab_storm.yaml`
- `fixtures/simulations/resize_baseline/font_churn_multi_pane.yaml`
- `fixtures/simulations/resize_baseline/mixed_scale_soak.yaml`

## Metadata Contract

Each scenario includes `metadata` keys used for reproducibility and longitudinal comparison:

- `suite`: fixed to `resize_baseline`
- `suite_version`: scenario-pack revision
- `seed`: deterministic generation seed
- `scale_profile`: scenario family
- `pane_count`, `tab_count`, `scrollback_lines`, `font_steps`: declared workload axes

`ft simulate run --json` and `ft simulate validate --json` now emit `metadata` and `reproducibility_key`.

## Event Contract

Additional simulation actions used by this suite:

- `set_font_size`: records deterministic font-size transition markers
- `generate_scrollback`: synthesizes deterministic scrollback (`LINES` or `LINESxWIDTH`)

The mock simulation runtime encodes these as append markers/content so expectations and timeline replay remain deterministic.

## Resize Timeline Instrumentation

`Scenario` now exposes stage-level resize timeline probes for baseline attribution:

- `execute_all_with_resize_timeline`
- `execute_until_with_resize_timeline`

Each resize-class event (`resize`, `set_font_size`, `generate_scrollback`) emits ordered stage samples:

1. `input_intent`
2. `scheduler_queueing` (includes queue depth before/after)
3. `logical_reflow`
4. `render_prep`
5. `presentation`

The timeline artifact includes nanosecond stage durations, per-event structured records, stage summaries (`p95`, `max`, `avg`), and flamegraph-ready rows via `flame_samples()`.

## Timeline Data Model (Authoritative Fields)

Probe artifacts for this suite use the following structures in `crates/frankenterm-core/src/simulation.rs`:

- `ResizeTimeline`
  - `scenario`
  - `reproducibility_key`
  - `captured_at_ms`
  - `executed_resize_events`
  - `events`
- `ResizeTimelineEvent`
  - `event_index`
  - `pane_id`
  - `action`
  - `scheduled_at_ns`
  - `dispatch_offset_ns`
  - `total_duration_ns`
  - `stages`
- `ResizeTimelineStageSample`
  - `stage`
  - `start_offset_ns`
  - `duration_ns`
  - `queue_metrics` (scheduler stage only)
- `ResizeQueueMetrics`
  - `depth_before`
  - `depth_after`
- `ResizeTimelineStageSummary` (from `stage_summary()`)
  - `samples`
  - `total_duration_ns`
  - `avg_duration_ns`
  - `p95_duration_ns`
  - `max_duration_ns`

These field names are the baseline schema contract for `wa-1u90p.1.4` and `wa-1u90p.1.5`.

## How To Run

```bash
ft simulate list

ft simulate validate fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml
ft simulate run fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml --json
ft simulate run fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml --json --resize-timeline-json

# Timeline API validation and stage/queue probe coverage
cargo test -p frankenterm-core simulation_resize_suite -- --nocapture
cargo test -p frankenterm-core resize_timeline_summary_and_flame_samples_cover_all_stages -- --nocapture
```

## Coverage

Automated integration coverage lives in:

- `crates/frankenterm-core/tests/simulation_resize_suite.rs`

That test loads every suite file, validates metadata/reproducibility keys, executes all events in `MockWezterm`, and asserts all `contains` expectations.

`ft simulate run --json --resize-timeline-json` now emits a single machine-readable artifact envelope containing `timeline`, `stage_summary`, and `flame_samples`. The simulation API (`execute_all_with_resize_timeline` / `execute_until_with_resize_timeline`) remains the canonical source and is still used directly by tests and baseline analysis code.

## Companion Runtime Telemetry (`wa-1u90p.1.3`)

Resize baseline interpretation should be paired with lock/memory telemetry from `crates/frankenterm-core/src/runtime.rs`:

- lock contention: `max_storage_lock_wait_ms`, `avg_storage_lock_wait_ms`, `storage_lock_contention_events`
- lock hold: `max_storage_lock_hold_ms`, `avg_storage_lock_hold_ms`
- cursor snapshot memory: `cursor_snapshot_bytes_last`, `cursor_snapshot_bytes_max`, `avg_cursor_snapshot_bytes`

Warning thresholds currently encoded in runtime:

- storage lock wait warning: `15.0 ms`
- storage lock hold warning: `75.0 ms`
- cursor snapshot memory warning: `64 MiB`
