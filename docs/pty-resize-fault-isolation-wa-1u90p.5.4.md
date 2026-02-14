# PTY Resize Fault Isolation (`wa-1u90p.5.4`)

Date: 2026-02-13  
Author: `VioletFinch`

## Objective

Harden async pane resize so transient PTY failures are retried and persistent failures are isolated to the affected pane without stalling global resize progress.

## Implementation

File changed: `frankenterm/mux/src/localpane.rs`

### Retry policy + helper

Added reusable retry primitives for PTY resize:

- `ResizeRetryPolicy { max_attempts, base_backoff, max_backoff }`
- `ResizeRetryStats { attempts, backoff_elapsed }`
- `retry_backoff_for_attempt(...)` (exponential, capped)
- `retry_with_backoff(...)` generic helper

Default PTY policy:

- attempts: `3`
- backoff: `2ms` base, capped at `25ms`

### PTY resize application path

`apply_resize_sync(...)` now:

- executes PTY resize via `retry_with_backoff(...)`
- logs each retry attempt with pane/attempt/target context
- wraps terminal failure with attempt and target metadata
- tracks retry stats in completion metrics:
  - `pty_resize_attempts`
  - `pty_retry_backoff_elapsed`

### Fault isolation behavior

In worker loop:

- persistent PTY failure logs error for that pane/seq and continues processing queue loop.
- failures do not panic or block unrelated panes.

### Observability updates

Completion log now includes:

- queue latency and completion timing
- PTY lock/resize timing
- retry-specific fields (`pty_resize_attempts`, `pty_retry_backoff_us`)

## Test Coverage Added

In `localpane.rs` test module:

- `retry_with_backoff_succeeds_after_transient_failures`
- `retry_with_backoff_reports_terminal_failure_after_budget`
- `retry_backoff_is_monotonic_and_capped`

These complement prior async replay tests and queue stress tests.

## Validation

- `cargo fmt --check` ✅
- `CARGO_TARGET_DIR=target-violetfinch-wa1u90p54 cargo check -p mux --lib` ✅
- `CARGO_TARGET_DIR=target-violetfinch-wa1u90p54 cargo check -p mux --all-targets` ❌ blocked by existing unrelated `tab.rs` test import issue (`SerdeUrl::try_from` scope)
- `CARGO_TARGET_DIR=target-violetfinch-wa1u90p54 cargo clippy -p mux --lib -- -D warnings` ❌ blocked by pre-existing lint debt in other workspace crates

## Notes

This change isolates PTY resize faults to the pane-level worker path and preserves system responsiveness under transient/persistent PTY errors.
