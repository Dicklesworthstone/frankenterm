# Async Resize Race Replay Notes (`wa-1u90p.5.9`)

Date: 2026-02-13  
Author: `VioletFinch`

## Scope

Add deterministic race/regression test coverage for the asynchronous `LocalPane` resize queue model introduced in `wa-1u90p.5.8`.

## Test Harness

Location: `frankenterm/mux/src/localpane.rs` test module.

A lightweight deterministic harness (`ResizeReplayHarness`) models:

- intent enqueue (`intent`)
- worker pickup (`start`)
- completion (`complete`)
- coalescing replacement (`replaced_seq`)

The harness captures a causality chain of event strings for assertions.

## Added Scenarios

1. `replay_cancellation_race_coalesces_to_latest_intent`
- Reproduces cancellation/coalescing race while one resize is in-flight.
- Verifies intermediate intent is replaced and latest intent completes.

2. `replay_prevents_out_of_order_completion`
- Verifies single in-flight rule prevents starting another completion path prematurely.
- Asserts completion order remains deterministic (`[1, latest]`).

3. `replay_rapid_resizes_emit_intent_to_completion_causality_chain`
- Simulates rapid burst (200 enqueues) while worker is busy.
- Asserts causality log includes intent/start/complete links and replacement markers.

## Existing Queue Semantics Tests Retained

- `resize_queue_coalesces_latest_pending_when_worker_is_running`
- `resize_queue_marks_worker_idle_when_empty`
- `resize_queue_stress_preserves_latest_intent_only`

These cover queue mechanics; replay tests cover race narratives + causality chain assertions.

## Validation Status

- `cargo fmt --check` ✅
- `CARGO_TARGET_DIR=target-violetfinch-wa1u90p59 cargo check -p mux --lib` ✅
- `CARGO_TARGET_DIR=target-violetfinch-wa1u90p59 cargo check -p mux --all-targets` ❌ blocked by existing unrelated `frankenterm/mux/src/tab.rs` test import issue (`SerdeUrl::try_from` trait scope).
- `CARGO_TARGET_DIR=target-violetfinch-wa1u90p59 cargo clippy -p mux --lib -- -D warnings` ❌ blocked by pre-existing lint debt in other workspace crates.

## Notes

This deterministic replay coverage is designed to make prior async race classes reproducible without relying on scheduler timing nondeterminism.
