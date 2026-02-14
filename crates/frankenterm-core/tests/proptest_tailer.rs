//! Property-based tests for the tailer module.
//!
//! Covers: SchedulerSnapshot serde roundtrip, CaptureScheduler budget
//! enforcement invariants, TailerConfig structural invariants,
//! TailerMode display, StreamingBridge counter monotonicity.

use proptest::prelude::*;

use frankenterm_core::config::CaptureBudgetConfig;
use frankenterm_core::tailer::{
    CaptureScheduler, SchedulerSnapshot, StreamingBridge, TailerConfig, TailerMode,
};
use std::time::Duration;

// ─── Strategies ──────────────────────────────────────────────────────

fn arb_capture_budget() -> impl Strategy<Value = CaptureBudgetConfig> {
    (0u32..1000, 0u64..100_000).prop_map(|(caps, bytes)| CaptureBudgetConfig {
        max_captures_per_sec: caps,
        max_bytes_per_sec: bytes,
    })
}

fn arb_scheduler_snapshot() -> impl Strategy<Value = SchedulerSnapshot> {
    (
        any::<bool>(),
        0u32..1000,
        0u64..100_000,
        0u32..1000,
        0u64..100_000,
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
        0usize..500,
    )
        .prop_map(
            |(
                budget_active,
                max_captures_per_sec,
                max_bytes_per_sec,
                captures_remaining,
                bytes_remaining,
                total_rate_limited,
                total_byte_budget_exceeded,
                total_throttle_events,
                tracked_panes,
            )| {
                SchedulerSnapshot {
                    budget_active,
                    max_captures_per_sec,
                    max_bytes_per_sec,
                    captures_remaining,
                    bytes_remaining,
                    total_rate_limited,
                    total_byte_budget_exceeded,
                    total_throttle_events,
                    tracked_panes,
                }
            },
        )
}

fn arb_tailer_config() -> impl Strategy<Value = TailerConfig> {
    (
        1u64..5000,   // min_interval_ms
        1u64..10_000, // max_interval_ms
        prop::num::f64::POSITIVE.prop_filter("finite and >= 1.0", |v| v.is_finite() && *v >= 1.0),
        1usize..100,  // max_concurrent
        0usize..4096, // overlap_size
        1u64..5000,   // send_timeout_ms
    )
        .prop_map(
            |(min_ms, max_ms_delta, backoff, max_concurrent, overlap_size, send_ms)| {
                // Ensure min <= max by adding delta
                let max_ms = min_ms + max_ms_delta;
                TailerConfig {
                    min_interval: Duration::from_millis(min_ms),
                    max_interval: Duration::from_millis(max_ms),
                    backoff_multiplier: backoff,
                    max_concurrent,
                    overlap_size,
                    send_timeout: Duration::from_millis(send_ms),
                }
            },
        )
}

/// Generate a vector of (pane_id, priority) pairs, pre-sorted by (priority, pane_id).
fn arb_ready_panes() -> impl Strategy<Value = Vec<(u64, u32)>> {
    prop::collection::vec((1u64..1000, 0u32..1000), 0..50).prop_map(|mut panes| {
        panes.sort_by_key(|&(id, prio)| (prio, id));
        // Deduplicate pane IDs
        panes.dedup_by_key(|p| p.0);
        panes
    })
}

// ─── SchedulerSnapshot serde roundtrip ──────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn scheduler_snapshot_serde_roundtrip(snap in arb_scheduler_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let decoded: SchedulerSnapshot = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(snap.budget_active, decoded.budget_active);
        prop_assert_eq!(snap.max_captures_per_sec, decoded.max_captures_per_sec);
        prop_assert_eq!(snap.max_bytes_per_sec, decoded.max_bytes_per_sec);
        prop_assert_eq!(snap.captures_remaining, decoded.captures_remaining);
        prop_assert_eq!(snap.bytes_remaining, decoded.bytes_remaining);
        prop_assert_eq!(snap.total_rate_limited, decoded.total_rate_limited);
        prop_assert_eq!(
            snap.total_byte_budget_exceeded,
            decoded.total_byte_budget_exceeded
        );
        prop_assert_eq!(snap.total_throttle_events, decoded.total_throttle_events);
        prop_assert_eq!(snap.tracked_panes, decoded.tracked_panes);
    }
}

#[cfg(feature = "semantic-search")]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn scheduler_snapshot_serde_msgpack_roundtrip(snap in arb_scheduler_snapshot()) {
        let packed = rmp_serde::to_vec(&snap).unwrap();
        let decoded: SchedulerSnapshot = rmp_serde::from_slice(&packed).unwrap();

        prop_assert_eq!(snap.budget_active, decoded.budget_active);
        prop_assert_eq!(snap.max_captures_per_sec, decoded.max_captures_per_sec);
        prop_assert_eq!(snap.max_bytes_per_sec, decoded.max_bytes_per_sec);
        prop_assert_eq!(snap.captures_remaining, decoded.captures_remaining);
        prop_assert_eq!(snap.bytes_remaining, decoded.bytes_remaining);
        prop_assert_eq!(snap.total_rate_limited, decoded.total_rate_limited);
        prop_assert_eq!(
            snap.total_byte_budget_exceeded,
            decoded.total_byte_budget_exceeded
        );
        prop_assert_eq!(snap.total_throttle_events, decoded.total_throttle_events);
        prop_assert_eq!(snap.tracked_panes, decoded.tracked_panes);
    }
}

// ─── CaptureScheduler: unlimited budget allows everything ───────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn unlimited_budget_allows_all_panes(
        panes in arb_ready_panes(),
        permits in 0usize..100,
    ) {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 0,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        let selected = sched.select_panes(&panes, permits);

        // With unlimited budget, only permits limit the count
        let expected_count = panes.len().min(permits);
        prop_assert_eq!(
            selected.len(),
            expected_count,
            "unlimited budget should select min(panes, permits)"
        );
    }

    #[test]
    fn unlimited_budget_check_global_always_true(n in 1u32..100) {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 0,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        for _ in 0..n {
            prop_assert!(
                sched.check_global_budget(),
                "unlimited budget should always allow"
            );
        }
    }

    #[test]
    fn unlimited_byte_budget_never_exhausted(
        captures in prop::collection::vec((1u64..1000, 1u64..100_000), 1..50),
    ) {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 0,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        for (pane_id, bytes) in captures {
            sched.record_capture(pane_id, bytes);
            prop_assert!(
                !sched.is_byte_budget_exhausted(),
                "unlimited byte budget should never be exhausted"
            );
        }
    }
}

// ─── CaptureScheduler: select_panes invariants ──────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn select_panes_never_exceeds_permits(
        budget in arb_capture_budget(),
        panes in arb_ready_panes(),
        permits in 0usize..50,
    ) {
        let mut sched = CaptureScheduler::new(budget);
        let selected = sched.select_panes(&panes, permits);

        prop_assert!(
            selected.len() <= permits,
            "selected {} panes but only {} permits available",
            selected.len(),
            permits
        );
    }

    #[test]
    fn select_panes_never_exceeds_input_count(
        budget in arb_capture_budget(),
        panes in arb_ready_panes(),
        permits in 0usize..100,
    ) {
        let mut sched = CaptureScheduler::new(budget);
        let selected = sched.select_panes(&panes, permits);

        prop_assert!(
            selected.len() <= panes.len(),
            "selected {} but only {} panes offered",
            selected.len(),
            panes.len()
        );
    }

    #[test]
    fn select_panes_never_exceeds_capture_budget(
        cap_budget in 1u32..100,
        panes in arb_ready_panes(),
        permits in 0usize..200,
    ) {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: cap_budget,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);
        let selected = sched.select_panes(&panes, permits);

        prop_assert!(
            selected.len() <= cap_budget as usize,
            "selected {} but capture budget is {}",
            selected.len(),
            cap_budget
        );
    }

    #[test]
    fn select_panes_preserves_input_order(
        budget in arb_capture_budget(),
        panes in arb_ready_panes(),
        permits in 1usize..50,
    ) {
        let mut sched = CaptureScheduler::new(budget);
        let selected = sched.select_panes(&panes, permits);

        // Selected panes should be a prefix of the input (since input is sorted
        // by priority and select_panes takes from the front).
        let expected_prefix: Vec<u64> = panes.iter().take(selected.len()).map(|(id, _)| *id).collect();
        prop_assert_eq!(
            &selected,
            &expected_prefix,
            "selected panes should be a prefix of the sorted input"
        );
    }

    #[test]
    fn select_panes_all_returned_ids_from_input(
        budget in arb_capture_budget(),
        panes in arb_ready_panes(),
        permits in 0usize..50,
    ) {
        let mut sched = CaptureScheduler::new(budget);
        let selected = sched.select_panes(&panes, permits);

        let input_ids: std::collections::HashSet<u64> = panes.iter().map(|(id, _)| *id).collect();
        for id in &selected {
            prop_assert!(
                input_ids.contains(id),
                "selected pane {} not in input set",
                id
            );
        }
    }

    #[test]
    fn select_panes_empty_input_always_empty(
        budget in arb_capture_budget(),
        permits in 0usize..100,
    ) {
        let mut sched = CaptureScheduler::new(budget);
        let selected = sched.select_panes(&[], permits);
        prop_assert!(selected.is_empty(), "empty input should yield empty output");
    }

    #[test]
    fn select_panes_zero_permits_always_empty(
        budget in arb_capture_budget(),
        panes in arb_ready_panes(),
    ) {
        let mut sched = CaptureScheduler::new(budget);
        let selected = sched.select_panes(&panes, 0);
        prop_assert!(selected.is_empty(), "zero permits should yield empty output");
    }
}

// ─── CaptureScheduler: budget depletion monotonicity ────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn capture_budget_depletes_monotonically(
        cap_budget in 1u32..50,
        call_count in 1u32..20,
    ) {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: cap_budget,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        let mut prev_allowed = true;
        for _ in 0..call_count {
            let allowed = sched.check_global_budget();
            // Once denied, must stay denied (within same window).
            if !prev_allowed {
                prop_assert!(
                    !allowed,
                    "once budget exhausted, subsequent calls should also be denied"
                );
            }
            prev_allowed = allowed;
        }
    }

    #[test]
    fn byte_budget_depletes_monotonically(
        byte_budget in 1u64..10_000,
        captures in prop::collection::vec((1u64..100, 1u64..1000), 1..20),
    ) {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 0,
            max_bytes_per_sec: byte_budget,
        };
        let mut sched = CaptureScheduler::new(budget);

        let mut was_exhausted = false;
        for (pane_id, bytes) in captures {
            sched.record_capture(pane_id, bytes);
            let exhausted = sched.is_byte_budget_exhausted();
            // Once exhausted, should stay exhausted (within same window).
            if was_exhausted {
                prop_assert!(
                    exhausted,
                    "byte budget should stay exhausted once depleted"
                );
            }
            was_exhausted = exhausted;
        }
    }

    #[test]
    fn byte_budget_saturating_never_underflows(
        byte_budget in 1u64..100,
        oversized_capture in 100u64..100_000,
    ) {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 0,
            max_bytes_per_sec: byte_budget,
        };
        let mut sched = CaptureScheduler::new(budget);

        // Record more bytes than the budget allows
        sched.record_capture(1, oversized_capture);

        // Should be exhausted, never underflowed
        prop_assert!(sched.is_byte_budget_exhausted());

        // Snapshot should show 0 bytes remaining, not a wrapped value
        let snap = sched.snapshot();
        prop_assert_eq!(snap.bytes_remaining, 0, "should saturate at 0, not underflow");
    }
}

// ─── CaptureScheduler: metrics invariants ───────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn throttle_events_geq_rate_limited_plus_byte_exceeded(
        cap_budget in 0u32..10,
        byte_budget in 0u64..500,
        ops in prop::collection::vec(
            prop::bool::ANY, // true = check_global, false = record+check_byte
            1..30
        ),
    ) {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: cap_budget,
            max_bytes_per_sec: byte_budget,
        };
        let mut sched = CaptureScheduler::new(budget);

        for (i, do_check_global) in ops.iter().enumerate() {
            if *do_check_global {
                sched.check_global_budget();
            } else {
                sched.record_capture(i as u64, 100);
                sched.is_byte_budget_exhausted();
            }
        }

        let m = sched.metrics();
        // throttle_events counts both global rate limits and byte budget exceeded
        prop_assert!(
            m.throttle_events >= m.global_rate_limited,
            "throttle_events ({}) should be >= global_rate_limited ({})",
            m.throttle_events,
            m.global_rate_limited
        );
        prop_assert!(
            m.throttle_events >= m.pane_byte_budget_exceeded,
            "throttle_events ({}) should be >= pane_byte_budget_exceeded ({})",
            m.throttle_events,
            m.pane_byte_budget_exceeded
        );
    }

    #[test]
    fn scheduler_snapshot_reflects_budget_config(budget in arb_capture_budget()) {
        let sched = CaptureScheduler::new(budget.clone());
        let snap = sched.snapshot();

        prop_assert_eq!(snap.max_captures_per_sec, budget.max_captures_per_sec);
        prop_assert_eq!(snap.max_bytes_per_sec, budget.max_bytes_per_sec);

        // budget_active should be true iff at least one limit is non-zero
        let expected_active = budget.max_captures_per_sec > 0 || budget.max_bytes_per_sec > 0;
        prop_assert_eq!(
            snap.budget_active,
            expected_active,
            "budget_active mismatch for caps={}, bytes={}",
            budget.max_captures_per_sec,
            budget.max_bytes_per_sec
        );
    }

    #[test]
    fn fresh_scheduler_snapshot_has_full_budget(budget in arb_capture_budget()) {
        let sched = CaptureScheduler::new(budget.clone());
        let snap = sched.snapshot();

        prop_assert_eq!(
            snap.captures_remaining,
            budget.max_captures_per_sec,
            "fresh scheduler should have full capture budget"
        );
        prop_assert_eq!(
            snap.bytes_remaining,
            budget.max_bytes_per_sec,
            "fresh scheduler should have full byte budget"
        );
        prop_assert_eq!(snap.total_rate_limited, 0);
        prop_assert_eq!(snap.total_byte_budget_exceeded, 0);
        prop_assert_eq!(snap.total_throttle_events, 0);
        prop_assert_eq!(snap.tracked_panes, 0);
    }
}

// ─── CaptureScheduler: per-pane tracking ────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn record_capture_tracks_all_panes(
        captures in prop::collection::vec((1u64..100, 1u64..10_000), 1..30),
    ) {
        let budget = CaptureBudgetConfig::default();
        let mut sched = CaptureScheduler::new(budget);

        let mut expected_panes = std::collections::HashSet::new();
        for (pane_id, bytes) in &captures {
            sched.record_capture(*pane_id, *bytes);
            expected_panes.insert(*pane_id);
        }

        let snap = sched.snapshot();
        prop_assert_eq!(
            snap.tracked_panes,
            expected_panes.len(),
            "tracked_panes should match unique pane IDs"
        );
    }

    #[test]
    fn remove_pane_decrements_tracked_count(
        pane_ids in prop::collection::hash_set(1u64..100, 1..20),
    ) {
        let budget = CaptureBudgetConfig::default();
        let mut sched = CaptureScheduler::new(budget);

        for &pane_id in &pane_ids {
            sched.record_capture(pane_id, 100);
        }

        let initial_count = sched.snapshot().tracked_panes;
        prop_assert_eq!(initial_count, pane_ids.len());

        // Remove one pane at a time
        for (i, &pane_id) in pane_ids.iter().enumerate() {
            sched.remove_pane(pane_id);
            let remaining = sched.snapshot().tracked_panes;
            prop_assert_eq!(
                remaining,
                initial_count - i - 1,
                "removing pane {} should decrement tracked count",
                pane_id
            );
        }
    }

    #[test]
    fn remove_nonexistent_pane_is_noop(pane_id in 1u64..1000) {
        let budget = CaptureBudgetConfig::default();
        let mut sched = CaptureScheduler::new(budget);

        let before = sched.snapshot().tracked_panes;
        sched.remove_pane(pane_id);
        let after = sched.snapshot().tracked_panes;

        prop_assert_eq!(before, after, "removing nonexistent pane should be noop");
    }
}

// ─── CaptureScheduler: update_budget preserves window ───────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn update_budget_preserves_metrics(
        initial in arb_capture_budget(),
        updated in arb_capture_budget(),
        ops_before in 0u32..10,
    ) {
        let mut sched = CaptureScheduler::new(initial);

        // Do some operations to accumulate metrics
        for _ in 0..ops_before {
            sched.check_global_budget();
        }

        let metrics_before_rate = sched.metrics().global_rate_limited;
        let metrics_before_throttle = sched.metrics().throttle_events;

        sched.update_budget(updated.clone());

        // Metrics should be preserved through update
        prop_assert!(
            sched.metrics().global_rate_limited >= metrics_before_rate,
            "metrics should not decrease after update"
        );
        prop_assert!(
            sched.metrics().throttle_events >= metrics_before_throttle,
            "throttle events should not decrease after update"
        );

        // New config should be reflected
        let snap = sched.snapshot();
        prop_assert_eq!(snap.max_captures_per_sec, updated.max_captures_per_sec);
        prop_assert_eq!(snap.max_bytes_per_sec, updated.max_bytes_per_sec);
    }
}

// ─── TailerConfig structural invariants ─────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn tailer_config_min_leq_max(config in arb_tailer_config()) {
        prop_assert!(
            config.min_interval <= config.max_interval,
            "min_interval ({:?}) should be <= max_interval ({:?})",
            config.min_interval,
            config.max_interval
        );
    }

    #[test]
    fn tailer_config_backoff_geq_one(config in arb_tailer_config()) {
        prop_assert!(
            config.backoff_multiplier >= 1.0,
            "backoff_multiplier ({}) should be >= 1.0",
            config.backoff_multiplier
        );
    }

    #[test]
    fn tailer_config_max_concurrent_positive(config in arb_tailer_config()) {
        prop_assert!(
            config.max_concurrent >= 1,
            "max_concurrent ({}) should be >= 1",
            config.max_concurrent
        );
    }
}

#[test]
fn tailer_config_default_invariants() {
    let config = TailerConfig::default();
    assert!(config.min_interval <= config.max_interval);
    assert!(config.backoff_multiplier >= 1.0);
    assert!(config.max_concurrent >= 1);
    assert!(config.send_timeout > Duration::ZERO);
}

// ─── TailerMode display ─────────────────────────────────────────────

#[test]
fn tailer_mode_display_polling() {
    assert_eq!(TailerMode::Polling.to_string(), "polling");
}

#[test]
fn tailer_mode_display_streaming() {
    assert_eq!(TailerMode::Streaming.to_string(), "streaming");
}

#[test]
fn tailer_mode_display_all_variants_non_empty() {
    for mode in [TailerMode::Polling, TailerMode::Streaming] {
        let display = mode.to_string();
        assert!(
            !display.is_empty(),
            "display for {:?} should be non-empty",
            mode
        );
    }
}

#[test]
fn tailer_mode_equality() {
    assert_eq!(TailerMode::Polling, TailerMode::Polling);
    assert_eq!(TailerMode::Streaming, TailerMode::Streaming);
    assert_ne!(TailerMode::Polling, TailerMode::Streaming);
}

// ─── StreamingBridge counter monotonicity ────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn streaming_bridge_fallback_count_monotonic(n in 1u32..100) {
        let mut bridge = StreamingBridge::new();
        let mut prev = bridge.fallback_count();

        for _ in 0..n {
            bridge.record_fallback();
            let current = bridge.fallback_count();
            prop_assert!(
                current > prev,
                "fallback_count should monotonically increase: {} -> {}",
                prev,
                current
            );
            prev = current;
        }
    }

    #[test]
    fn streaming_bridge_fallback_count_equals_calls(n in 0u32..200) {
        let mut bridge = StreamingBridge::new();
        for _ in 0..n {
            bridge.record_fallback();
        }
        prop_assert_eq!(
            bridge.fallback_count(),
            n as u64,
            "fallback_count should equal number of record_fallback calls"
        );
    }
}

#[test]
fn streaming_bridge_default_counters_zero() {
    let bridge = StreamingBridge::new();
    assert_eq!(bridge.events_processed(), 0);
    assert_eq!(bridge.fallback_count(), 0);
    assert_eq!(bridge.dirty_range_total(), 0);
    assert_eq!(bridge.dirty_row_total(), 0);
}

#[test]
fn streaming_bridge_default_eq_new() {
    let a = StreamingBridge::new();
    let b = StreamingBridge::default();
    assert_eq!(a.events_processed(), b.events_processed());
    assert_eq!(a.fallback_count(), b.fallback_count());
    assert_eq!(a.dirty_range_total(), b.dirty_range_total());
    assert_eq!(a.dirty_row_total(), b.dirty_row_total());
}

// ─── CaptureScheduler: combined stress test ─────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Interleaved select_panes + record_capture + check_global_budget
    /// should never panic and metrics should remain consistent.
    #[test]
    fn scheduler_interleaved_ops_no_panic(
        budget in arb_capture_budget(),
        ops in prop::collection::vec(
            prop::sample::select(vec![0u8, 1, 2, 3]),
            1..50
        ),
    ) {
        let mut sched = CaptureScheduler::new(budget);

        for (i, op) in ops.iter().enumerate() {
            let pane_id = (i as u64) % 10 + 1;
            match op {
                0 => {
                    // select_panes
                    let panes = vec![(pane_id, i as u32 % 100)];
                    let _ = sched.select_panes(&panes, 5);
                }
                1 => {
                    // record_capture
                    sched.record_capture(pane_id, (i as u64 + 1) * 100);
                }
                2 => {
                    // check_global_budget
                    let _ = sched.check_global_budget();
                }
                3 => {
                    // is_byte_budget_exhausted
                    let _ = sched.is_byte_budget_exhausted();
                }
                _ => unreachable!(),
            }
        }

        // After all operations, snapshot should be consistent
        let snap = sched.snapshot();
        let m = sched.metrics();

        // Invariant: throttle_events >= individual throttle counts
        prop_assert!(
            m.throttle_events >= m.global_rate_limited,
            "throttle >= rate_limited"
        );
        prop_assert!(
            m.throttle_events >= m.pane_byte_budget_exceeded,
            "throttle >= byte_exceeded"
        );

        // Snapshot config should match
        prop_assert_eq!(snap.total_rate_limited, m.global_rate_limited);
        prop_assert_eq!(snap.total_byte_budget_exceeded, m.pane_byte_budget_exceeded);
        prop_assert_eq!(snap.total_throttle_events, m.throttle_events);
    }
}

// ─── SchedulerSnapshot: edge cases ──────────────────────────────────

#[test]
fn scheduler_snapshot_default_all_zeros() {
    let snap = SchedulerSnapshot::default();
    assert!(!snap.budget_active);
    assert_eq!(snap.max_captures_per_sec, 0);
    assert_eq!(snap.max_bytes_per_sec, 0);
    assert_eq!(snap.captures_remaining, 0);
    assert_eq!(snap.bytes_remaining, 0);
    assert_eq!(snap.total_rate_limited, 0);
    assert_eq!(snap.total_byte_budget_exceeded, 0);
    assert_eq!(snap.total_throttle_events, 0);
    assert_eq!(snap.tracked_panes, 0);
}

#[test]
fn scheduler_snapshot_json_includes_all_fields() {
    let snap = SchedulerSnapshot {
        budget_active: true,
        max_captures_per_sec: 42,
        max_bytes_per_sec: 9999,
        captures_remaining: 10,
        bytes_remaining: 5000,
        total_rate_limited: 3,
        total_byte_budget_exceeded: 1,
        total_throttle_events: 4,
        tracked_panes: 7,
    };
    let json = serde_json::to_string(&snap).unwrap();
    assert!(json.contains("budget_active"));
    assert!(json.contains("max_captures_per_sec"));
    assert!(json.contains("max_bytes_per_sec"));
    assert!(json.contains("captures_remaining"));
    assert!(json.contains("bytes_remaining"));
    assert!(json.contains("total_rate_limited"));
    assert!(json.contains("total_byte_budget_exceeded"));
    assert!(json.contains("total_throttle_events"));
    assert!(json.contains("tracked_panes"));
}
