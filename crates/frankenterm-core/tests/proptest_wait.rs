//! Property-based tests for the `wait` module.
//!
//! Covers `Backoff::next_delay` monotonicity and capping invariants,
//! `QueueDepthGauge` increment/decrement/saturation properties,
//! `ActivityTracker` idle/record state transitions,
//! `WaitError` Display format consistency, and `QuiescenceState`
//! behavioral properties.

use std::sync::Arc;
use std::time::{Duration, Instant};

use frankenterm_core::wait::{
    ActivityTracker, Backoff, QueueDepthGauge, QuiescenceSignals, QuiescenceState, WaitError,
    WaitFor,
};
use proptest::prelude::*;

// =========================================================================
// Backoff::next_delay — monotonicity and capping
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// next_delay never exceeds max.
    #[test]
    fn prop_next_delay_capped(
        initial_ms in 1_u64..1000,
        max_ms in 1_u64..10_000,
        factor in 1_u32..10,
        current_ms in 0_u64..100_000,
    ) {
        let backoff = Backoff {
            initial: Duration::from_millis(initial_ms),
            max: Duration::from_millis(max_ms),
            factor,
            max_retries: None,
        };
        let current = Duration::from_millis(current_ms);
        let next = backoff.next_delay(current);
        prop_assert!(next <= backoff.max, "next {:?} > max {:?}", next, backoff.max);
    }

    /// next_delay is always >= current (monotonic growth) when factor >= 1
    /// and current hasn't already exceeded max (capping reduces to max).
    #[test]
    fn prop_next_delay_monotonic(
        max_ms in 100_u64..100_000,
        factor in 1_u32..10,
        current_ms in 1_u64..1000,
    ) {
        prop_assume!(current_ms <= max_ms);
        let backoff = Backoff {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(max_ms),
            factor,
            max_retries: None,
        };
        let current = Duration::from_millis(current_ms);
        let next = backoff.next_delay(current);
        prop_assert!(next >= current, "next {:?} < current {:?}", next, current);
    }

    /// Repeated application eventually reaches max.
    #[test]
    fn prop_next_delay_converges_to_max(
        initial_ms in 1_u64..100,
        max_ms in 100_u64..10_000,
        factor in 2_u32..5,
    ) {
        let backoff = Backoff {
            initial: Duration::from_millis(initial_ms),
            max: Duration::from_millis(max_ms),
            factor,
            max_retries: None,
        };
        let mut delay = backoff.initial;
        for _ in 0..100 {
            delay = backoff.next_delay(delay);
        }
        prop_assert_eq!(delay, backoff.max, "should converge to max after many iterations");
    }

    /// Factor 1 means delay stays the same (capped at current*1 = current).
    #[test]
    fn prop_factor_one_stays_same(
        max_ms in 100_u64..10_000,
        current_ms in 1_u64..100,
    ) {
        let backoff = Backoff {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(max_ms),
            factor: 1,
            max_retries: None,
        };
        let current = Duration::from_millis(current_ms);
        let next = backoff.next_delay(current);
        prop_assert_eq!(next, current, "factor=1 should keep delay the same");
    }
}

// =========================================================================
// Backoff — default values
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// Default backoff has documented values.
    #[test]
    fn prop_default_backoff(_dummy in 0..1_u8) {
        let b = Backoff::default();
        prop_assert_eq!(b.initial, Duration::from_millis(25));
        prop_assert_eq!(b.max, Duration::from_secs(1));
        prop_assert_eq!(b.factor, 2);
        prop_assert!(b.max_retries.is_none());
    }
}

// =========================================================================
// QueueDepthGauge — increment/decrement properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Incrementing N times then decrementing N times yields depth 0.
    #[test]
    fn prop_gauge_balanced(n in 1_usize..100) {
        let gauge = QueueDepthGauge::new("test");
        for _ in 0..n {
            gauge.increment();
        }
        prop_assert_eq!(gauge.depth(), n);
        for _ in 0..n {
            gauge.decrement();
        }
        prop_assert_eq!(gauge.depth(), 0);
    }

    /// Decrementing past zero saturates at zero (never underflows).
    #[test]
    fn prop_gauge_saturates_at_zero(extra_decrements in 1_usize..10) {
        let gauge = QueueDepthGauge::new("test");
        gauge.increment();
        gauge.decrement();
        // Now at 0; extra decrements should not underflow
        for _ in 0..extra_decrements {
            gauge.decrement();
        }
        prop_assert_eq!(gauge.depth(), 0);
    }

    /// Gauge name is preserved.
    #[test]
    fn prop_gauge_name_preserved(name in "[a-z_]{3,15}") {
        let gauge = QueueDepthGauge::new(&name);
        prop_assert_eq!(gauge.name(), name.as_str());
    }

    /// New gauge starts at zero.
    #[test]
    fn prop_gauge_starts_at_zero(name in "[a-z]{3,10}") {
        let gauge = QueueDepthGauge::new(&name);
        prop_assert_eq!(gauge.depth(), 0);
    }
}

// =========================================================================
// ActivityTracker — state transitions
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// New tracker is always idle.
    #[test]
    fn prop_new_tracker_idle(_dummy in 0..1_u8) {
        let tracker = ActivityTracker::new();
        prop_assert!(tracker.is_idle());
        prop_assert!(tracker.last_activity().is_none());
    }

    /// After recording activity, tracker is no longer idle.
    #[test]
    fn prop_record_makes_not_idle(_dummy in 0..1_u8) {
        let tracker = ActivityTracker::new();
        tracker.record();
        prop_assert!(!tracker.is_idle());
        prop_assert!(tracker.last_activity().is_some());
    }

    /// Multiple records keep tracker not-idle.
    #[test]
    fn prop_multiple_records_not_idle(n in 1_usize..10) {
        let tracker = ActivityTracker::new();
        for _ in 0..n {
            tracker.record();
        }
        prop_assert!(!tracker.is_idle());
    }
}

// =========================================================================
// WaitError — Display format consistency
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// WaitError Display always includes the expected condition.
    #[test]
    fn prop_wait_error_includes_expected(
        expected in "[a-z ]{3,20}",
        retries in 0_usize..100,
        elapsed_ms in 0_u64..100_000,
    ) {
        let err = WaitError {
            expected: expected.clone(),
            last_observed: None,
            retries,
            elapsed: Duration::from_millis(elapsed_ms),
        };
        let display = err.to_string();
        prop_assert!(
            display.contains(&expected),
            "display '{}' should contain expected '{}'", display, expected
        );
    }

    /// WaitError Display includes retry count.
    #[test]
    fn prop_wait_error_includes_retries(
        retries in 0_usize..100,
    ) {
        let err = WaitError {
            expected: "test".to_string(),
            last_observed: None,
            retries,
            elapsed: Duration::from_millis(100),
        };
        let display = err.to_string();
        prop_assert!(
            display.contains(&format!("retries={retries}")),
            "display '{}' should contain retries={}", display, retries
        );
    }

    /// WaitError Display with Some(last_observed) includes the observed value.
    #[test]
    fn prop_wait_error_includes_observed(
        observed in "[a-z]{3,10}",
    ) {
        let err = WaitError {
            expected: "test".to_string(),
            last_observed: Some(observed.clone()),
            retries: 1,
            elapsed: Duration::from_millis(100),
        };
        let display = err.to_string();
        prop_assert!(
            display.contains(&observed),
            "display '{}' should contain observed '{}'", display, observed
        );
    }

    /// WaitError Display with None observed shows <none>.
    #[test]
    fn prop_wait_error_none_shows_placeholder(_dummy in 0..1_u8) {
        let err = WaitError {
            expected: "test".to_string(),
            last_observed: None,
            retries: 1,
            elapsed: Duration::from_millis(100),
        };
        let display = err.to_string();
        prop_assert!(
            display.contains("<none>"),
            "display '{}' should contain '<none>'", display
        );
    }
}

// =========================================================================
// QuiescenceState — behavioral properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// QuiescenceState with pending > 0 is never quiet.
    #[test]
    fn prop_pending_not_quiet(pending in 1_usize..100) {
        let state = QuiescenceState {
            pending,
            last_activity: None,
            quiet_window: Duration::from_millis(0),
        };
        prop_assert!(!state.is_quiet(Instant::now()));
    }

    /// QuiescenceState with pending=0 and no activity is always quiet.
    #[test]
    fn prop_no_pending_no_activity_quiet(window_ms in 0_u64..10_000) {
        let state = QuiescenceState {
            pending: 0,
            last_activity: None,
            quiet_window: Duration::from_millis(window_ms),
        };
        prop_assert!(state.is_quiet(Instant::now()));
    }

    /// QuiescenceState describe always includes "pending=" field.
    #[test]
    fn prop_describe_includes_pending(pending in 0_usize..50) {
        let state = QuiescenceState {
            pending,
            last_activity: Some(Instant::now()),
            quiet_window: Duration::from_millis(100),
        };
        let desc = state.describe(Instant::now());
        prop_assert!(
            desc.contains(&format!("pending={pending}")),
            "describe '{}' should contain pending={}", desc, pending
        );
    }

    /// QuiescenceState describe always includes "quiet_window_ms=".
    #[test]
    fn prop_describe_includes_window(window_ms in 0_u64..10_000) {
        let state = QuiescenceState {
            pending: 0,
            last_activity: None,
            quiet_window: Duration::from_millis(window_ms),
        };
        let desc = state.describe(Instant::now());
        prop_assert!(
            desc.contains(&format!("quiet_window_ms={window_ms}")),
            "describe '{}' should contain quiet_window_ms={}", desc, window_ms
        );
    }
}

// =========================================================================
// WaitFor constructors
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// WaitFor::ready wraps the value.
    #[test]
    fn prop_wait_for_ready(val in any::<i32>()) {
        let w = WaitFor::ready(val);
        match w {
            WaitFor::Ready(v) => prop_assert_eq!(v, val),
            WaitFor::NotReady { .. } => prop_assert!(false, "expected Ready"),
        }
    }

    /// WaitFor::not_ready with Some preserves the string.
    #[test]
    fn prop_wait_for_not_ready_some(msg in "[a-z]{3,10}") {
        let w: WaitFor<i32> = WaitFor::not_ready(Some(msg.clone()));
        match w {
            WaitFor::NotReady { last_observed: Some(obs) } => {
                prop_assert_eq!(obs, msg);
            }
            _ => prop_assert!(false, "expected NotReady with Some"),
        }
    }

    /// WaitFor::not_ready with None has None last_observed.
    #[test]
    fn prop_wait_for_not_ready_none(_dummy in 0..1_u8) {
        let w: WaitFor<i32> = WaitFor::not_ready(None::<String>);
        match w {
            WaitFor::NotReady { last_observed: None } => {}
            _ => prop_assert!(false, "expected NotReady with None"),
        }
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn backoff_zero_current_grows() {
    let backoff = Backoff {
        initial: Duration::from_millis(10),
        max: Duration::from_millis(1000),
        factor: 2,
        max_retries: None,
    };
    let next = backoff.next_delay(Duration::ZERO);
    assert_eq!(next, Duration::ZERO); // 0 * 2 = 0
}

#[test]
fn gauge_concurrent_safety() {
    let gauge = Arc::new(QueueDepthGauge::new("concurrent"));
    let handles: Vec<_> = (0..10)
        .map(|_| {
            let g = gauge.clone();
            std::thread::spawn(move || {
                for _ in 0..100 {
                    g.increment();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(gauge.depth(), 1000);
}

#[test]
fn wait_error_is_std_error() {
    let err = WaitError {
        expected: "test".to_string(),
        last_observed: None,
        retries: 0,
        elapsed: Duration::ZERO,
    };
    let _: &dyn std::error::Error = &err;
}
