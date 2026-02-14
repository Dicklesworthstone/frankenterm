# Resize Lock Graph and Contention Notes (`wa-1u90p.5.7`)

Date: 2026-02-13  
Author: `VioletFinch`  
Scope: LocalPane resize path and immediate tab fanout interactions.

## Objective

Map lock ordering in the resize path, identify high-contention sections, and
apply an initial low-risk refactor that reduces avoidable lock work.

## Lock Graph (Current)

### A. Tab fanout path

`Tab::resize` (`frankenterm/mux/src/tab.rs`) acquires `Tab.inner` mutex and
drives split layout updates before calling pane-level resize operations.

Representative flow:

1. `Tab::resize` -> `self.inner.lock().resize(size)`  
   File: `frankenterm/mux/src/tab.rs:1011`
2. `TabInner::resize` recalculates splits and calls `apply_sizes_from_splits`  
   File: `frankenterm/mux/src/tab.rs:1782`
3. `apply_sizes_from_splits` calls `pane.resize(...)` for each leaf  
   File: `frankenterm/mux/src/tab.rs:819`

### B. Local pane path

`LocalPane::resize` (`frankenterm/mux/src/localpane.rs`) performs PTY and
terminal resizing.

Previous behavior:

- Always acquire PTY mutex and perform PTY resize.
- Then acquire terminal mutex and perform terminal resize.
- No no-op short-circuit for same-size requests.

## Contention Hotspot Identified

Under resize storms, duplicate size events are common. Repeated same-size
resizes were still taking both lock paths and issuing PTY resize calls, adding
avoidable lock wait and syscall overhead.

## Implemented Slice

File changed: `frankenterm/mux/src/localpane.rs`

1. Added same-size no-op fast path in `LocalPane::resize`:
- Probe current terminal size.
- If current size equals target size, return early without PTY or terminal resize.

2. Added per-resize lock/hold telemetry at `trace` level:
- `probe_lock_wait_us`
- `pty_lock_wait_us`
- `pty_resize_us`
- `terminal_apply_lock_wait_us`
- `terminal_resize_us`

3. Kept lock scopes non-overlapping:
- PTY lock is dropped before terminal apply lock.

## Before/After Critical-Section Summary

Before:
- Every resize request (including duplicates) entered PTY + terminal paths.

After:
- Duplicate-size requests stop after a short terminal size probe.
- PTY and terminal lock durations are visible via trace metrics for
  before/after contention analysis.

### Deterministic Path Metrics (Duplicate-Size Events)

| Metric (per duplicate resize request) | Before | After |
|---|---:|---:|
| PTY lock acquisitions | 1 | 0 |
| PTY resize syscalls attempted | 1 | 0 |
| Terminal lock acquisitions | 1 | 1 (probe only) |
| Total lock acquisitions in `LocalPane::resize` | 2 | 1 |
| PTY lock hold duration | >0 (always exercised) | 0 (bypassed) |

These counts are invariant for same-size events and come directly from control
flow in `LocalPane::resize`.

## How to Collect Metrics

1. Enable verbose logs while reproducing resize churn:

```bash
RUST_LOG=trace ft -vv watch --foreground
```

2. Filter for resize telemetry lines:

```bash
rg "LocalPane::resize pane_id=" /path/to/log.txt
```

3. Compare:
- frequency of `noop` lines
- p95 of `pty_lock_wait_us` and `terminal_apply_lock_wait_us`
- p95 of `pty_resize_us` and `terminal_resize_us`

## Follow-on Work (Not in this slice)

1. Move pane resize execution outside long-lived `Tab.inner` lock scopes where safe.
2. Add explicit lock-order assertions/instrumentation for tab -> pane traversal.
3. Build a resize-storm benchmark that reports lock-wait percentiles per phase.
