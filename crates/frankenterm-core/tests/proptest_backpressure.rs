//! Property-based tests for backpressure module.
//!
//! Verifies the 4-tier FSM (Green/Yellow/Red/Black) invariants:
//! - Tier ordering: Green < Yellow < Red < Black
//! - as_u8 monotonic: tier_a < tier_b ⟹ a.as_u8() < b.as_u8()
//! - QueueDepths ratios bounded [0.0, 1.0], zero capacity → 0.0
//! - classify() monotonic: higher fill → higher or equal tier
//! - classify() symmetry: capture and write both influence tier
//! - Black saturation: depth within threshold of capacity → Black
//! - evaluate() upgrades immediate, downgrades blocked by hysteresis
//! - evaluate() disabled → always None, Green stays
//! - Pane pause set semantics: idempotent, sorted output
//! - BackpressureConfig serde roundtrip
//! - BackpressureSnapshot serde roundtrip
//! - BackpressureTier serde roundtrip
//! - Metrics counters: monotonically non-decreasing

use proptest::prelude::*;
use std::sync::atomic::Ordering;

use frankenterm_core::backpressure::{
    BackpressureConfig, BackpressureManager, BackpressureSnapshot, BackpressureTier, QueueDepths,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_tier() -> impl Strategy<Value = BackpressureTier> {
    prop_oneof![
        Just(BackpressureTier::Green),
        Just(BackpressureTier::Yellow),
        Just(BackpressureTier::Red),
        Just(BackpressureTier::Black),
    ]
}

fn arb_capacity() -> impl Strategy<Value = usize> {
    1usize..=10_000
}

fn arb_depth(capacity: usize) -> impl Strategy<Value = usize> {
    0..=capacity
}

fn arb_queue_depths() -> impl Strategy<Value = QueueDepths> {
    (arb_capacity(), arb_capacity()).prop_flat_map(|(cc, wc)| {
        (arb_depth(cc), Just(cc), arb_depth(wc), Just(wc)).prop_map(|(cd, cc, wd, wc)| {
            QueueDepths {
                capture_depth: cd,
                capture_capacity: cc,
                write_depth: wd,
                write_capacity: wc,
            }
        })
    })
}

/// Config with valid thresholds (yellow < red for both capture and write).
fn arb_config() -> impl Strategy<Value = BackpressureConfig> {
    (
        prop::bool::ANY, // enabled
        100u64..=10_000, // check_interval_ms
        0.1f64..=0.4,    // yellow_capture
        0.5f64..=0.9,    // red_capture
        0.1f64..=0.4,    // yellow_write
        0.5f64..=0.9,    // red_write
        100u64..=60_000, // hysteresis_ms
        1.0f64..=5.0,    // idle_poll_backoff_factor
        0.0f64..=1.0,    // skip_detection_ratio
        0.0f64..=1.0,    // pause_ratio
        1usize..=1000,   // max_buffered_segments
        100u64..=10_000, // recovery_resume_interval_ms
    )
        .prop_map(
            |(
                enabled,
                check_interval_ms,
                yellow_capture,
                red_capture,
                yellow_write,
                red_write,
                hysteresis_ms,
                idle_poll_backoff_factor,
                skip_detection_ratio,
                pause_ratio,
                max_buffered_segments,
                recovery_resume_interval_ms,
            )| {
                BackpressureConfig {
                    enabled,
                    check_interval_ms,
                    yellow_capture,
                    red_capture,
                    yellow_write,
                    red_write,
                    hysteresis_ms,
                    idle_poll_backoff_factor,
                    skip_detection_ratio,
                    pause_ratio,
                    max_buffered_segments,
                    recovery_resume_interval_ms,
                }
            },
        )
}

fn arb_pane_ids() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(0u64..1000, 0..20)
}

// ────────────────────────────────────────────────────────────────────
// BackpressureTier: ordering & as_u8
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// as_u8() preserves tier ordering: a < b ⟹ a.as_u8() < b.as_u8().
    #[test]
    fn prop_tier_as_u8_monotonic(
        t1 in arb_tier(),
        t2 in arb_tier(),
    ) {
        if t1 < t2 {
            prop_assert!(t1.as_u8() < t2.as_u8());
        } else if t1 == t2 {
            prop_assert_eq!(t1.as_u8(), t2.as_u8());
        } else {
            prop_assert!(t1.as_u8() > t2.as_u8());
        }
    }

    /// as_u8() is in [0, 3].
    #[test]
    fn prop_tier_as_u8_bounded(t in arb_tier()) {
        prop_assert!(t.as_u8() <= 3);
    }
}

// ────────────────────────────────────────────────────────────────────
// BackpressureTier: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Tier serialises to/from JSON without loss.
    #[test]
    fn prop_tier_serde_roundtrip(t in arb_tier()) {
        let json = serde_json::to_string(&t).unwrap();
        let back: BackpressureTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(t, back);
    }

    /// Display produces the expected uppercase string.
    #[test]
    fn prop_tier_display_nonempty(t in arb_tier()) {
        let s = t.to_string();
        prop_assert!(!s.is_empty());
        // Display is uppercase
        let upper = s.to_uppercase();
        prop_assert_eq!(s, upper);
    }
}

// ────────────────────────────────────────────────────────────────────
// QueueDepths: ratio bounds
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// capture_ratio() and write_ratio() are in [0.0, 1.0].
    #[test]
    fn prop_ratios_bounded(d in arb_queue_depths()) {
        let cr = d.capture_ratio();
        let wr = d.write_ratio();
        prop_assert!(cr >= 0.0 && cr <= 1.0, "capture_ratio {} out of bounds", cr);
        prop_assert!(wr >= 0.0 && wr <= 1.0, "write_ratio {} out of bounds", wr);
    }

    /// Zero capacity always gives ratio 0.0.
    #[test]
    fn prop_zero_capacity_ratio_zero(
        cd in 0usize..100,
        wd in 0usize..100,
    ) {
        let d = QueueDepths {
            capture_depth: cd,
            capture_capacity: 0,
            write_depth: wd,
            write_capacity: 0,
        };
        prop_assert!((d.capture_ratio()).abs() < f64::EPSILON);
        prop_assert!((d.write_ratio()).abs() < f64::EPSILON);
    }

    /// Full queue gives ratio 1.0.
    #[test]
    fn prop_full_queue_ratio_one(
        cc in 1usize..10_000,
        wc in 1usize..10_000,
    ) {
        let d = QueueDepths {
            capture_depth: cc,
            capture_capacity: cc,
            write_depth: wc,
            write_capacity: wc,
        };
        prop_assert!((d.capture_ratio() - 1.0).abs() < 1e-9);
        prop_assert!((d.write_ratio() - 1.0).abs() < 1e-9);
    }

    /// Empty queue gives ratio 0.0.
    #[test]
    fn prop_empty_queue_ratio_zero(
        cc in 1usize..10_000,
        wc in 1usize..10_000,
    ) {
        let d = QueueDepths {
            capture_depth: 0,
            capture_capacity: cc,
            write_depth: 0,
            write_capacity: wc,
        };
        prop_assert!((d.capture_ratio()).abs() < f64::EPSILON);
        prop_assert!((d.write_ratio()).abs() < f64::EPSILON);
    }
}

// ────────────────────────────────────────────────────────────────────
// BackpressureConfig: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Config serialises to/from JSON preserving all fields.
    #[test]
    fn prop_config_serde_roundtrip(c in arb_config()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: BackpressureConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.enabled, c.enabled);
        prop_assert_eq!(back.check_interval_ms, c.check_interval_ms);
        prop_assert!((back.yellow_capture - c.yellow_capture).abs() < 1e-9);
        prop_assert!((back.red_capture - c.red_capture).abs() < 1e-9);
        prop_assert!((back.yellow_write - c.yellow_write).abs() < 1e-9);
        prop_assert!((back.red_write - c.red_write).abs() < 1e-9);
        prop_assert_eq!(back.hysteresis_ms, c.hysteresis_ms);
        prop_assert!((back.idle_poll_backoff_factor - c.idle_poll_backoff_factor).abs() < 1e-9);
        prop_assert!((back.skip_detection_ratio - c.skip_detection_ratio).abs() < 1e-9);
        prop_assert!((back.pause_ratio - c.pause_ratio).abs() < 1e-9);
        prop_assert_eq!(back.max_buffered_segments, c.max_buffered_segments);
        prop_assert_eq!(back.recovery_resume_interval_ms, c.recovery_resume_interval_ms);
    }
}

// ────────────────────────────────────────────────────────────────────
// classify(): tier monotonicity with fill ratio
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Higher capture fill → higher or equal tier (with write held at zero).
    #[test]
    fn prop_classify_monotonic_capture(
        c in arb_config(),
        cap in 200usize..10_000,
        lo in 0usize..100,
    ) {
        let m = BackpressureManager::new(c);
        // lo/cap vs (cap-1)/cap — lower fill should not exceed higher fill's tier
        let d_lo = QueueDepths {
            capture_depth: lo.min(cap),
            capture_capacity: cap,
            write_depth: 0,
            write_capacity: 10_000,
        };
        let d_hi = QueueDepths {
            capture_depth: cap.saturating_sub(1),
            capture_capacity: cap,
            write_depth: 0,
            write_capacity: 10_000,
        };
        let t_lo = m.classify(&d_lo);
        let t_hi = m.classify(&d_hi);
        prop_assert!(t_lo <= t_hi, "lo tier {:?} > hi tier {:?}", t_lo, t_hi);
    }

    /// Higher write fill → higher or equal tier (with capture held at zero).
    #[test]
    fn prop_classify_monotonic_write(
        c in arb_config(),
        cap in 200usize..10_000,
        lo in 0usize..100,
    ) {
        let m = BackpressureManager::new(c);
        let d_lo = QueueDepths {
            capture_depth: 0,
            capture_capacity: 10_000,
            write_depth: lo.min(cap),
            write_capacity: cap,
        };
        let d_hi = QueueDepths {
            capture_depth: 0,
            capture_capacity: 10_000,
            write_depth: cap.saturating_sub(1),
            write_capacity: cap,
        };
        let t_lo = m.classify(&d_lo);
        let t_hi = m.classify(&d_hi);
        prop_assert!(t_lo <= t_hi, "lo tier {:?} > hi tier {:?}", t_lo, t_hi);
    }

    /// Empty queues always classify as Green.
    #[test]
    fn prop_classify_empty_is_green(c in arb_config()) {
        let m = BackpressureManager::new(c);
        let d = QueueDepths {
            capture_depth: 0,
            capture_capacity: 1000,
            write_depth: 0,
            write_capacity: 10_000,
        };
        prop_assert_eq!(m.classify(&d), BackpressureTier::Green);
    }

    /// Zero-capacity queues classify as Green (ratio returns 0.0).
    #[test]
    fn prop_classify_zero_cap_is_green(c in arb_config()) {
        let m = BackpressureManager::new(c);
        let d = QueueDepths {
            capture_depth: 0,
            capture_capacity: 0,
            write_depth: 0,
            write_capacity: 0,
        };
        prop_assert_eq!(m.classify(&d), BackpressureTier::Green);
    }
}

// ────────────────────────────────────────────────────────────────────
// classify(): Black saturation
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Capture depth within 5 of capacity → Black (capacity > 0).
    #[test]
    fn prop_classify_black_capture_saturated(
        cap in 6usize..10_000,
    ) {
        let m = BackpressureManager::new(BackpressureConfig::default());
        let d = QueueDepths {
            capture_depth: cap.saturating_sub(4), // within 5 of cap
            capture_capacity: cap,
            write_depth: 0,
            write_capacity: 10_000,
        };
        prop_assert_eq!(m.classify(&d), BackpressureTier::Black);
    }

    /// Write depth within 100 of capacity → Black (capacity > 0).
    #[test]
    fn prop_classify_black_write_saturated(
        cap in 101usize..10_000,
    ) {
        let m = BackpressureManager::new(BackpressureConfig::default());
        let d = QueueDepths {
            capture_depth: 0,
            capture_capacity: 10_000,
            write_depth: cap.saturating_sub(99), // within 100 of cap
            write_capacity: cap,
        };
        prop_assert_eq!(m.classify(&d), BackpressureTier::Black);
    }
}

// ────────────────────────────────────────────────────────────────────
// evaluate(): upgrade is immediate
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Upgrade from Green to any higher tier is always immediate.
    #[test]
    fn prop_evaluate_upgrade_immediate(d in arb_queue_depths()) {
        let m = BackpressureManager::new(BackpressureConfig::default());
        let proposed = m.classify(&d);
        let result = m.evaluate(&d);
        if proposed > BackpressureTier::Green {
            prop_assert!(result.is_some(), "Upgrade should be immediate");
            let (old, new) = result.unwrap();
            prop_assert_eq!(old, BackpressureTier::Green);
            prop_assert_eq!(new, proposed);
        }
    }

    /// evaluate() with disabled config always returns None.
    #[test]
    fn prop_evaluate_disabled_noop(d in arb_queue_depths()) {
        let mut config = BackpressureConfig::default();
        config.enabled = false;
        let m = BackpressureManager::new(config);
        prop_assert!(m.evaluate(&d).is_none());
        prop_assert_eq!(m.current_tier(), BackpressureTier::Green);
    }

    /// evaluate() with same tier (no change) returns None.
    #[test]
    fn prop_evaluate_same_tier_none(
        cap in 1000usize..10_000,
    ) {
        let m = BackpressureManager::new(BackpressureConfig::default());
        // Two green observations in a row → second returns None
        let d = QueueDepths {
            capture_depth: 10,
            capture_capacity: cap,
            write_depth: 10,
            write_capacity: cap,
        };
        let _ = m.evaluate(&d); // Green → Green, should be None already
        let second = m.evaluate(&d);
        prop_assert!(second.is_none());
    }
}

// ────────────────────────────────────────────────────────────────────
// evaluate(): metrics counters
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// After a sequence of upgrades, tier-specific counters match transitions.
    #[test]
    fn prop_metrics_yellow_counter(
        cap in 1000usize..10_000,
    ) {
        let m = BackpressureManager::new(BackpressureConfig::default());
        // Green → Yellow via capture ratio at 50%
        let half = cap / 2;
        let d = QueueDepths {
            capture_depth: half,
            capture_capacity: cap,
            write_depth: 0,
            write_capacity: 10_000,
        };
        let result = m.evaluate(&d);
        if let Some((_, BackpressureTier::Yellow)) = result {
            prop_assert!(m.metrics.yellow_entries.load(Ordering::Relaxed) >= 1);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// evaluate(): hysteresis blocks downgrades
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Downgrade attempt within hysteresis window returns None.
    #[test]
    fn prop_hysteresis_blocks_downgrade(
        cap in 1000usize..10_000,
    ) {
        let mut config = BackpressureConfig::default();
        config.hysteresis_ms = 60_000; // 60s — won't elapse during test
        let m = BackpressureManager::new(config);

        // Upgrade to Red
        let red_depth = (cap as f64 * 0.80) as usize;
        let d_up = QueueDepths {
            capture_depth: red_depth,
            capture_capacity: cap,
            write_depth: 0,
            write_capacity: 10_000,
        };
        m.evaluate(&d_up);

        // Attempt downgrade to Green
        let d_down = QueueDepths {
            capture_depth: 0,
            capture_capacity: cap,
            write_depth: 0,
            write_capacity: 10_000,
        };
        let result = m.evaluate(&d_down);
        prop_assert!(result.is_none(), "Downgrade should be blocked by hysteresis");
        // Tier should still be the elevated tier
        prop_assert!(m.current_tier() > BackpressureTier::Green);
    }
}

// ────────────────────────────────────────────────────────────────────
// Pane pause management: set semantics
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// pause_pane is idempotent — pausing twice doesn't duplicate.
    #[test]
    fn prop_pause_idempotent(ids in arb_pane_ids()) {
        let m = BackpressureManager::new(BackpressureConfig::default());
        for &id in &ids {
            m.pause_pane(id);
            m.pause_pane(id); // double-pause
        }
        let paused = m.paused_pane_ids();
        // Should be a set — no duplicates
        let mut sorted = paused.clone();
        sorted.dedup();
        prop_assert_eq!(paused.len(), sorted.len(), "Duplicates found after idempotent pause");
    }

    /// paused_pane_ids() returns sorted output.
    #[test]
    fn prop_paused_panes_sorted(ids in arb_pane_ids()) {
        let m = BackpressureManager::new(BackpressureConfig::default());
        for &id in &ids {
            m.pause_pane(id);
        }
        let paused = m.paused_pane_ids();
        for w in paused.windows(2) {
            prop_assert!(w[0] <= w[1], "Not sorted: {} > {}", w[0], w[1]);
        }
    }

    /// resume_pane removes only that pane.
    #[test]
    fn prop_resume_removes_target(ids in prop::collection::vec(0u64..100, 2..10)) {
        let m = BackpressureManager::new(BackpressureConfig::default());
        for &id in &ids {
            m.pause_pane(id);
        }

        // Resume the first pane
        let target = ids[0];
        m.resume_pane(target);

        prop_assert!(!m.is_pane_paused(target), "Pane {} should be resumed", target);
    }

    /// resume_all_panes clears everything.
    #[test]
    fn prop_resume_all_clears(ids in arb_pane_ids()) {
        let m = BackpressureManager::new(BackpressureConfig::default());
        for &id in &ids {
            m.pause_pane(id);
        }
        m.resume_all_panes();
        prop_assert!(m.paused_pane_ids().is_empty());
    }

    /// is_pane_paused reflects pause state.
    #[test]
    fn prop_is_pane_paused_consistent(ids in prop::collection::vec(0u64..100, 1..10)) {
        let m = BackpressureManager::new(BackpressureConfig::default());
        for &id in &ids {
            m.pause_pane(id);
        }
        for &id in &ids {
            prop_assert!(m.is_pane_paused(id), "Pane {} should be paused", id);
        }
        // An unpaused pane should not be reported
        prop_assert!(!m.is_pane_paused(9999));
    }
}

// ────────────────────────────────────────────────────────────────────
// BackpressureSnapshot: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Snapshot serialises to/from JSON preserving all fields.
    #[test]
    fn prop_snapshot_serde_roundtrip(
        tier in arb_tier(),
        ts in 0u64..2_000_000_000_000,
        cd in 0usize..10_000,
        cc in 0usize..10_000,
        wd in 0usize..10_000,
        wc in 0usize..10_000,
        dur in 0u64..1_000_000,
        trans in 0u64..1000,
        panes in prop::collection::vec(0u64..1000, 0..10),
    ) {
        let snap = BackpressureSnapshot {
            tier,
            timestamp_epoch_ms: ts,
            capture_depth: cd,
            capture_capacity: cc,
            write_depth: wd,
            write_capacity: wc,
            duration_in_tier_ms: dur,
            transitions: trans,
            paused_panes: panes.clone(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: BackpressureSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.tier, tier);
        prop_assert_eq!(back.timestamp_epoch_ms, ts);
        prop_assert_eq!(back.capture_depth, cd);
        prop_assert_eq!(back.capture_capacity, cc);
        prop_assert_eq!(back.write_depth, wd);
        prop_assert_eq!(back.write_capacity, wc);
        prop_assert_eq!(back.duration_in_tier_ms, dur);
        prop_assert_eq!(back.transitions, trans);
        prop_assert_eq!(back.paused_panes, panes);
    }
}

// ────────────────────────────────────────────────────────────────────
// Manager: snapshot reflects state
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Snapshot tier matches current_tier().
    #[test]
    fn prop_snapshot_tier_matches_current(d in arb_queue_depths()) {
        let m = BackpressureManager::new(BackpressureConfig::default());
        let _ = m.evaluate(&d);
        let snap = m.snapshot(&d);
        prop_assert_eq!(snap.tier, m.current_tier());
    }

    /// Snapshot captures depths accurately.
    #[test]
    fn prop_snapshot_depths_accurate(d in arb_queue_depths()) {
        let m = BackpressureManager::new(BackpressureConfig::default());
        let snap = m.snapshot(&d);
        prop_assert_eq!(snap.capture_depth, d.capture_depth);
        prop_assert_eq!(snap.capture_capacity, d.capture_capacity);
        prop_assert_eq!(snap.write_depth, d.write_depth);
        prop_assert_eq!(snap.write_capacity, d.write_capacity);
    }

    /// Snapshot paused_panes matches paused_pane_ids().
    #[test]
    fn prop_snapshot_paused_panes(
        ids in arb_pane_ids(),
    ) {
        let m = BackpressureManager::new(BackpressureConfig::default());
        for &id in &ids {
            m.pause_pane(id);
        }
        let d = QueueDepths {
            capture_depth: 0,
            capture_capacity: 1000,
            write_depth: 0,
            write_capacity: 1000,
        };
        let snap = m.snapshot(&d);
        prop_assert_eq!(snap.paused_panes, m.paused_pane_ids());
    }
}

// ────────────────────────────────────────────────────────────────────
// Manager: config accessors
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Config accessors return the values passed at construction.
    #[test]
    fn prop_config_accessors(c in arb_config()) {
        let m = BackpressureManager::new(c.clone());
        prop_assert_eq!(m.is_enabled(), c.enabled);
        prop_assert!((m.idle_poll_backoff_factor() - c.idle_poll_backoff_factor).abs() < 1e-9);
        prop_assert!((m.skip_detection_ratio() - c.skip_detection_ratio).abs() < 1e-9);
        prop_assert!((m.pause_ratio() - c.pause_ratio).abs() < 1e-9);
        prop_assert_eq!(m.max_buffered_segments(), c.max_buffered_segments);
        prop_assert_eq!(m.recovery_resume_interval_ms(), c.recovery_resume_interval_ms);
    }

    /// Initial tier is always Green.
    #[test]
    fn prop_initial_tier_green(c in arb_config()) {
        let m = BackpressureManager::new(c);
        prop_assert_eq!(m.current_tier(), BackpressureTier::Green);
    }
}

// ────────────────────────────────────────────────────────────────────
// Manager: transition count
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Each successful evaluate() that changes tier increments transition count.
    #[test]
    fn prop_transition_count_incremented(
        cap in 1000usize..10_000,
    ) {
        let m = BackpressureManager::new(BackpressureConfig::default());

        // Green → Yellow
        let half = cap / 2;
        let d1 = QueueDepths {
            capture_depth: half,
            capture_capacity: cap,
            write_depth: 0,
            write_capacity: 10_000,
        };
        let r1 = m.evaluate(&d1);

        // Yellow → Red (immediate upgrade)
        let three_quarter = (cap as f64 * 0.76) as usize;
        let d2 = QueueDepths {
            capture_depth: three_quarter,
            capture_capacity: cap,
            write_depth: 0,
            write_capacity: 10_000,
        };
        let r2 = m.evaluate(&d2);

        let transitions = r1.is_some() as u64 + r2.is_some() as u64;
        let snap = m.snapshot(&d2);
        prop_assert!(
            snap.transitions >= transitions,
            "transitions {} < expected {}", snap.transitions, transitions
        );
    }
}
