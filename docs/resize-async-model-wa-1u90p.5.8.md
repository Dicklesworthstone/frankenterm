# Async LocalPane Resize Model (`wa-1u90p.5.8`)

Date: 2026-02-13  
Author: `VioletFinch`

## Goal

Make `LocalPane::resize` non-blocking for callers while preserving deterministic per-pane ordering semantics under resize storms.

## Implementation Summary

File changed: `frankenterm/mux/src/localpane.rs`

### 1. Queue + coalescing state

Added per-pane queue state:

- `ResizeQueueState { pending, next_seq, worker_running }`
- `pending` stores only one request (`PendingResize`), so enqueues coalesce to latest intent.
- `next_seq` provides monotonic sequence IDs for logs.

### 2. Non-blocking API behavior

`LocalPane::resize` now:

1. Performs synchronous size conversion validation (`TerminalSize -> PtySize`) to keep invalid-size error semantics.
2. Enqueues/coalesces request and returns immediately.
3. Spawns a per-pane worker thread only when transitioning from idle to active.

### 3. Worker execution model

Worker loop (`spawn_resize_worker`) repeatedly drains `pending` until empty:

- `pending.take()` selects next intent.
- If queue empty, worker marks `worker_running = false` and exits.
- PTY and terminal resize happen in `apply_resize_sync` with lock scopes kept non-overlapping.
- Same-size requests are no-op short-circuited in worker path.

### 4. Ordering guarantees

Per pane:

- Requests are processed in enqueue sequence order, except intermediate pending intents may be superseded by newer ones before execution.
- At most one worker executes at a time (`worker_running` gate).
- Latest intent wins for queued-but-not-yet-applied work.

## Telemetry Contract

### Queueing log

`LocalPane::resize enqueue ...`

Fields:

- `pane_id`
- `seq`
- `target`
- `replaced_seq` (if coalesced)
- `queue_depth_hint`
- `worker_spawned`

### Completion log

`LocalPane::resize complete ...`

Fields:

- `pane_id`
- `seq`
- `queue_wait_us`
- `completion_us`
- `noop`
- `current`, `target`
- `probe_lock_wait_us`
- `pty_lock_wait_us`
- `pty_resize_us`
- `terminal_apply_lock_wait_us`
- `terminal_resize_us`

## Test Coverage Added

In-file tests under `localpane.rs`:

- `resize_queue_coalesces_latest_pending_when_worker_is_running`
- `resize_queue_marks_worker_idle_when_empty`
- `resize_queue_stress_preserves_latest_intent_only`

These validate:

- latest-intent coalescing
- worker idle/active transitions
- stress behavior (1000 rapid enqueues retaining only latest pending intent)

## Validation Status

- `cargo fmt --check` ✅
- `CARGO_TARGET_DIR=target-violetfinch-wa1u90p58 cargo check -p mux --lib` ✅
- `CARGO_TARGET_DIR=target-violetfinch-wa1u90p58 cargo check -p mux --all-targets` ❌ blocked by existing unrelated `SerdeUrl::try_from` trait-import issue in `frankenterm/mux/src/tab.rs` tests.
- `CARGO_TARGET_DIR=target-violetfinch-wa1u90p58 cargo clippy -p mux --lib -- -D warnings` ❌ blocked by pre-existing lint debt in other workspace crates (`blob-leases`, `rangeset`, `char-props`).
