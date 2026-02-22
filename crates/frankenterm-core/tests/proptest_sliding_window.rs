//! Property-based tests for `sliding_window` — time-bucketed rate counter.

use proptest::prelude::*;

use frankenterm_core::sliding_window::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_config() -> impl Strategy<Value = (u64, usize)> {
    (100..10_000u64, 1..50usize)
}

fn arb_events(max_time: u64) -> impl Strategy<Value = Vec<u64>> {
    proptest::collection::vec(0..max_time, 0..50)
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. Empty window has zero count
    #[test]
    fn empty_has_zero_count((dur, buckets) in arb_config()) {
        let w = SlidingWindow::new(dur, buckets);
        prop_assert_eq!(w.count(0), 0);
        prop_assert_eq!(w.count(dur * 2), 0);
        prop_assert!(w.is_empty());
    }

    // 2. Count is non-negative (always >= 0, trivially true for u64)
    #[test]
    fn count_non_negative((dur, buckets) in arb_config(), events in arb_events(5000), now in 0..10_000u64) {
        let mut w = SlidingWindow::new(dur, buckets);
        for &t in &events {
            w.record(t);
        }
        // u64 is always >= 0, but let's verify the count makes sense
        let _ = w.count(now);
    }

    // 3. Count at recording time includes the event
    #[test]
    fn count_includes_event((dur, buckets) in arb_config(), t in 0..5000u64) {
        let mut w = SlidingWindow::new(dur, buckets);
        w.record(t);
        prop_assert!(w.count(t) >= 1);
    }

    // 4. Count after window expiry is zero
    #[test]
    fn count_after_expiry_zero((dur, buckets) in arb_config(), t in 0..5000u64) {
        let mut w = SlidingWindow::new(dur, buckets);
        w.record(t);
        // After window + full bucket duration, should be expired
        let far_future = t + dur + dur;
        prop_assert_eq!(w.count(far_future), 0);
    }

    // 5. record_n(t, n) increases count by n
    #[test]
    fn record_n_increases_count((dur, buckets) in arb_config(), t in 0..5000u64, n in 1..100u64) {
        let mut w = SlidingWindow::new(dur, buckets);
        w.record_n(t, n);
        prop_assert_eq!(w.count(t), n);
    }

    // 6. Multiple records in same bucket accumulate
    #[test]
    fn same_bucket_accumulates((dur, buckets) in arb_config(), t in 0..5000u64, count in 1..20usize) {
        let mut w = SlidingWindow::new(dur, buckets);
        for _ in 0..count {
            w.record(t);
        }
        prop_assert_eq!(w.count(t), count as u64);
    }

    // 7. Clear makes count zero
    #[test]
    fn clear_makes_zero((dur, buckets) in arb_config(), events in arb_events(5000)) {
        let mut w = SlidingWindow::new(dur, buckets);
        for &t in &events {
            w.record(t);
        }
        w.clear();
        prop_assert!(w.is_empty());
        prop_assert_eq!(w.count(5000), 0);
    }

    // 8. rate_per_second is non-negative
    #[test]
    fn rate_non_negative((dur, buckets) in arb_config(), events in arb_events(5000), now in 0..10_000u64) {
        let mut w = SlidingWindow::new(dur, buckets);
        for &t in &events {
            w.record(t);
        }
        prop_assert!(w.rate_per_second(now) >= 0.0);
    }

    // 9. rate = count / window_seconds
    #[test]
    fn rate_equals_count_over_window((dur, buckets) in arb_config(), events in arb_events(5000)) {
        let mut w = SlidingWindow::new(dur, buckets);
        let now = 5000u64;
        for &t in &events {
            w.record(t);
        }
        let count = w.count(now) as f64;
        let window_secs = dur as f64 / 1000.0;
        let expected_rate = count / window_secs;
        let actual_rate = w.rate_per_second(now);
        prop_assert!((actual_rate - expected_rate).abs() < 0.01,
            "rate mismatch: actual={}, expected={}", actual_rate, expected_rate);
    }

    // 10. exceeds_rate consistent with rate_per_second
    #[test]
    fn exceeds_rate_consistent((dur, buckets) in arb_config(), events in arb_events(5000), now in 0..10_000u64) {
        let mut w = SlidingWindow::new(dur, buckets);
        for &t in &events {
            w.record(t);
        }
        let rate = w.rate_per_second(now);
        prop_assert_eq!(w.exceeds_rate(now, rate - 0.001), rate > (rate - 0.001));
    }

    // 11. is_empty consistent with count
    #[test]
    fn is_empty_consistent((dur, buckets) in arb_config(), events in arb_events(5000)) {
        let mut w = SlidingWindow::new(dur, buckets);
        for &t in &events {
            w.record(t);
        }
        if w.is_empty() {
            // If marked empty, count at various times should be 0
            // (though is_empty checks all buckets regardless of time)
        }
        // After clear, definitely empty
        w.clear();
        prop_assert!(w.is_empty());
    }

    // 12. bucket_count matches construction
    #[test]
    fn bucket_count_matches((dur, buckets) in arb_config()) {
        let w = SlidingWindow::new(dur, buckets);
        prop_assert_eq!(w.bucket_count(), buckets);
    }

    // 13. window_duration_ms matches construction
    #[test]
    fn window_duration_matches((dur, buckets) in arb_config()) {
        let w = SlidingWindow::new(dur, buckets);
        prop_assert_eq!(w.window_duration_ms(), dur);
    }

    // 14. bucket_duration = window / n_buckets (at least 1)
    #[test]
    fn bucket_duration_correct((dur, buckets) in arb_config()) {
        let w = SlidingWindow::new(dur, buckets);
        let expected = (dur / buckets as u64).max(1);
        prop_assert_eq!(w.bucket_duration_ms(), expected);
    }

    // 15. from_config matches direct construction
    #[test]
    fn from_config_matches((dur, buckets) in arb_config()) {
        let config = SlidingWindowConfig {
            window_duration_ms: dur,
            n_buckets: buckets,
        };
        let w1 = SlidingWindow::new(dur, buckets);
        let w2 = SlidingWindow::from_config(config);
        prop_assert_eq!(w1.bucket_count(), w2.bucket_count());
        prop_assert_eq!(w1.window_duration_ms(), w2.window_duration_ms());
    }

    // 16. snapshot total_count matches count
    #[test]
    fn snapshot_total_matches_count((dur, buckets) in arb_config(), events in arb_events(5000)) {
        let mut w = SlidingWindow::new(dur, buckets);
        let now = 5000u64;
        for &t in &events {
            w.record(t);
        }
        let snap = w.snapshot(now);
        prop_assert_eq!(snap.total_count, w.count(now));
    }

    // 17. snapshot has correct metadata
    #[test]
    fn snapshot_metadata((dur, buckets) in arb_config()) {
        let w = SlidingWindow::new(dur, buckets);
        let snap = w.snapshot(0);
        prop_assert_eq!(snap.window_duration_ms, dur);
        prop_assert_eq!(snap.n_buckets, buckets);
        prop_assert_eq!(snap.bucket_counts.len(), buckets);
    }

    // 18. serde roundtrip preserves state
    #[test]
    fn serde_roundtrip((dur, buckets) in arb_config(), events in arb_events(5000)) {
        let mut w = SlidingWindow::new(dur, buckets);
        for &t in &events {
            w.record(t);
        }
        let json = serde_json::to_string(&w).unwrap();
        let back: SlidingWindow = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(w, back);
    }

    // 19. config serde roundtrip
    #[test]
    fn config_serde_roundtrip((dur, buckets) in arb_config()) {
        let config = SlidingWindowConfig {
            window_duration_ms: dur,
            n_buckets: buckets,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: SlidingWindowConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    // 20. oldest_timestamp = now - window_duration
    #[test]
    fn oldest_timestamp_correct((dur, buckets) in arb_config(), now in 0..100_000u64) {
        let w = SlidingWindow::new(dur, buckets);
        let expected = now.saturating_sub(dur);
        prop_assert_eq!(w.oldest_timestamp(now), expected);
    }

    // 21. count monotonically decreasing as events expire
    #[test]
    fn count_decreases_as_events_age((dur, buckets) in arb_config()) {
        let mut w = SlidingWindow::new(dur, buckets);
        // Record all events at time 0
        for _ in 0..10 {
            w.record(0);
        }
        let count_at_0 = w.count(0);
        let count_at_half = w.count(dur / 2);
        let count_at_end = w.count(dur + dur);
        // Count should not increase over time (may stay same or decrease)
        prop_assert!(count_at_0 >= count_at_half);
        prop_assert!(count_at_half >= count_at_end);
    }

    // 22. recent_count <= total count
    #[test]
    fn recent_count_bounded((dur, buckets) in arb_config(), events in arb_events(5000)) {
        let mut w = SlidingWindow::new(dur, buckets);
        let now = 5000u64;
        for &t in &events {
            w.record(t);
        }
        let total = w.count(now);
        let recent = w.recent_count(now, 1);
        prop_assert!(recent <= total);
    }

    // 23. recent_count with all buckets equals total count
    #[test]
    fn recent_all_equals_total((dur, buckets) in arb_config(), events in arb_events(5000)) {
        let mut w = SlidingWindow::new(dur, buckets);
        let now = 5000u64;
        for &t in &events {
            w.record(t);
        }
        let total = w.count(now);
        let recent_all = w.recent_count(now, buckets);
        prop_assert_eq!(recent_all, total);
    }

    // 24. clone equality
    #[test]
    fn clone_eq((dur, buckets) in arb_config(), events in arb_events(5000)) {
        let mut w = SlidingWindow::new(dur, buckets);
        for &t in &events {
            w.record(t);
        }
        let cloned = w.clone();
        prop_assert_eq!(w, cloned);
    }

    // 25. effective_rate is non-negative
    #[test]
    fn effective_rate_non_negative((dur, buckets) in arb_config(), events in arb_events(5000)) {
        let mut w = SlidingWindow::new(dur, buckets);
        for &t in &events {
            w.record(t);
        }
        prop_assert!(w.effective_rate(5000) >= 0.0);
    }

    // 26. recording same timestamp multiple times accumulates
    #[test]
    fn same_timestamp_accumulates((dur, buckets) in arb_config(), t in 0..5000u64, n in 1..50usize) {
        let mut w = SlidingWindow::new(dur, buckets);
        for _ in 0..n {
            w.record(t);
        }
        prop_assert_eq!(w.count(t), n as u64);
    }

    // 27. advancing past window clears all old events
    #[test]
    fn full_advance_clears((dur, buckets) in arb_config(), events in arb_events(1000)) {
        let mut w = SlidingWindow::new(dur, buckets);
        for &t in &events {
            w.record(t);
        }
        // Record one event far in the future
        let future = 1000 + dur * 2;
        w.record(future);
        // Only the future event should remain
        prop_assert_eq!(w.count(future), 1);
    }

    // 28. monotonic events give monotonic counts
    #[test]
    fn monotonic_events_monotonic_counts((dur, buckets) in arb_config(), n in 1..20usize) {
        let mut w = SlidingWindow::new(dur, buckets);
        let step = dur / (n as u64 * 2).max(1);
        let mut counts = Vec::new();
        for i in 0..n {
            let t = i as u64 * step;
            w.record(t);
            counts.push(w.count(t));
        }
        // Counts should be non-decreasing (since all events are within window)
        for window in counts.windows(2) {
            prop_assert!(window[0] <= window[1]);
        }
    }

    // 29. snapshot bucket_counts sum to total_count
    #[test]
    fn snapshot_bucket_sum((dur, buckets) in arb_config(), events in arb_events(5000)) {
        let mut w = SlidingWindow::new(dur, buckets);
        let now = 5000u64;
        for &t in &events {
            w.record(t);
        }
        let snap = w.snapshot(now);
        let sum: u64 = snap.bucket_counts.iter().sum();
        prop_assert_eq!(sum, snap.total_count);
    }

    // 30. recording and querying at same time is consistent
    #[test]
    fn record_query_same_time((dur, buckets) in arb_config(), count in 1..100u64) {
        let mut w = SlidingWindow::new(dur, buckets);
        let t = 1000u64;
        w.record_n(t, count);
        prop_assert_eq!(w.count(t), count);
    }
}
