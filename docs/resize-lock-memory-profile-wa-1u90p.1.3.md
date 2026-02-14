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

### 1. `ft simulate run` schema mismatch

`target/debug/ft` rejects `generate_scrollback` in baseline scenarios:

- unknown variant `generate_scrollback`
- accepted variants: `append`, `clear`, `set_title`, `resize`, `marker`

This blocks direct CLI replay of canonical baseline scenarios defined in `docs/resize-baseline-scenarios.md`.

### 2. `frankenterm-core` compile failures block timeline harness

`simulation_resize_suite` build currently fails before execution.

Error categories from `simulation_harness_build_error_summary.txt`:

- `E0308` (20): `std::time::Instant` vs `tokio::time::Instant` mismatches in `crates/frankenterm-core/src/workflows.rs`
- `E0277` (6): `u64: FromSql` not implemented in `crates/frankenterm-core/src/search/chunk_vector_store.rs`
- `E0609` (1): missing `wezterm_handle` field on `RuntimeHandle` in `crates/frankenterm-core/src/runtime.rs`
- `E0658` (1): const-trait call in `crates/frankenterm-core/src/resize_scheduler.rs`

Consequence: no fresh runtime timeline/percentile run is currently executable in this workspace state.

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

## Immediate Next Steps (Unblock Plan)

1. Restore baseline scenario replay compatibility (`generate_scrollback` action path).
2. Resolve compile blockers listed above (workflow instant typing, chunk vector SQL types, runtime handle field mismatch, const fn restriction).
3. Re-run `simulation_resize_suite` and export fresh timeline + lock/memory rollups into `evidence/wa-1u90p.1.3/`.
4. Feed updated numeric attribution directly into `docs/resize-baseline-bottleneck-dossier.md`.

