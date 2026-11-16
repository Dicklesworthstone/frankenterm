# Capture Backpressure and Overflow Policy (Recorder v1)

Date: 2026-02-12  
Bead: `wa-oegrb.2.6`  
Status: Accepted baseline policy and validation contract

## Purpose

Define deterministic capture-path behavior under overload so recorder capture degrades safely and observably without destabilizing watcher operations.

## Policy Summary

The current policy is:
- bounded capture admission (concurrency + per-second budgets)
- explicit backpressure detection
- overflow signaled by synthetic GAP insertion (not silent drop)
- deterministic recovery after congestion

This is a **slow-and-signal** policy, not an unbounded spool policy.

## Deterministic Overload Semantics

### 1. Admission controls

Capture work is bounded by:
- semaphore permits (`TailerConfig.max_concurrent`)
- capture rate budget (`CaptureBudgetConfig.max_captures_per_sec`)
- byte budget (`CaptureBudgetConfig.max_bytes_per_sec`)

Budget semantics:
- `0` means unlimited for the respective field.
- global windows refill every 1 second.

### 2. Backpressure detection

Backpressure is recognized when the capture channel cannot be reserved before `send_timeout`:
- emits `PollOutcome::Backpressure`
- increments per-pane `consecutive_backpressure`

### 3. Overflow escalation

Constant threshold:
- `OVERFLOW_BACKPRESSURE_THRESHOLD = 5`

When `consecutive_backpressure >= threshold`:
- set `overflow_gap_pending = true`
- next successful scheduling path emits synthetic gap instead of normal capture

### 4. Synthetic overflow GAP

Overflow emission behavior:
- call `cursor.emit_overflow_gap("backpressure_overflow")`
- send `CapturedSegmentKind::Gap { reason: "backpressure_overflow" }`
- empty content payload
- sequence advances monotonically (gap consumes a sequence slot)
- egress tap receives matching gap event when configured

### 5. Recovery behavior

After overflow gap emission:
- `overflow_gap_pending = false`
- `consecutive_backpressure = 0`
- `PollOutcome::OverflowGapEmitted` recorded

This provides explicit discontinuity markers and deterministic re-entry to normal capture.

## Chosen Queueing Policy

Evaluated options:
- Drop silently: rejected (forensic ambiguity)
- Slow only: rejected (hidden data loss risk under sustained congestion)
- Spool unbounded: rejected (stability risk)
- **Slow + explicit overflow gap**: accepted

Rationale:
- preserves core watcher stability
- makes loss observable and replay-aware
- avoids unbounded resource growth

## Observability and Telemetry Contract

Tailer-level metrics:
- `events_sent`
- `send_timeouts`
- `no_change_captures`
- `overflow_gaps_emitted`

Scheduler-level metrics:
- `global_rate_limited`
- `pane_byte_budget_exceeded`
- `throttle_events`
- snapshot fields (`captures_remaining`, `bytes_remaining`, etc.)

Operational requirement:
- alerting must key off non-zero growth in overflow/backpressure counters, not only raw throughput.

## Validation Evidence (Current)

Implemented behavior exists in:
- `crates/frankenterm-core/src/tailer.rs`
- `crates/frankenterm-core/src/ingest.rs`

Targeted test execution run:
- `CARGO_TARGET_DIR=/tmp/ft-cargo-orangebarn cargo test -p frankenterm-core overflow_gap -- --nocapture`
- `CARGO_TARGET_DIR=/tmp/ft-cargo-orangebarn cargo test -p frankenterm-core scheduler_ -- --nocapture`

Observed results:
- overflow slice: 8/8 pass
- scheduler slice: 30/30 pass

Representative validated behaviors:
- threshold-triggered overflow pending flag
- overflow gap emission and reason propagation
- sequence advancement during overflow gaps
- byte/rate budget throttling and metrics accounting
- deterministic priority-aware selection under constrained budgets

## Required Fault/Load Scenarios (Ongoing)

For swarm-scale validation (`wa-oegrb.7.1` / `wa-oegrb.7.2`):
- sustained channel saturation per pane and across panes
- mixed high/low priority panes under tight capture budgets
- byte budget exhaustion under large deltas
- repeated open/close window cycles (token bucket refill behavior)
- circuit-breaker-open periods + recovery overlap with backpressure

Expected invariant:
- overload must produce explicit and auditable throttling/gap signals; no silent divergence.

## Non-Goals

- Infinite-lossless buffering in v1
- Preserving every byte under unbounded overload
- Hidden retry loops that obscure capture discontinuities

## Exit Criteria for `wa-oegrb.2.6`

1. Overload behavior is deterministic and documented.
2. Overflow/discontinuity signaling is explicit and replay-visible.
3. Validation scenarios and measurable signals are defined for downstream reliability tracks.
