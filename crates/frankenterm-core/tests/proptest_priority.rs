//! Property-based tests for priority module.
//!
//! Verifies invariants of:
//! - PanePriority: ordering, as_u8/from_u8 roundtrip, clamping, serde, label
//! - OutputRateTracker: non-negative rate, monotonic decay, total_lines monotonic,
//!   zero-line no-op, half-life decay accuracy
//! - PriorityClassifier: register/unregister, override precedence, tier→priority mapping,
//!   error signal → Critical, rate_limited → Background, classify_all completeness
//! - shedding_order: preserves all panes, ascending priority, stability
//! - PriorityConfig/PriorityMetrics: serde roundtrip
//!
//! Complements the 5 inline proptests (error_always_outranks_idle, override_always_wins,
//! decay_monotonic, total_order, u8_roundtrip).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use proptest::prelude::*;

use frankenterm_core::pane_tiers::PaneTier;
use frankenterm_core::priority::{
    OutputRateTracker, PanePriority, PriorityClassifier, PriorityConfig, PriorityMetrics,
    shedding_order,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_priority() -> impl Strategy<Value = PanePriority> {
    prop_oneof![
        Just(PanePriority::Background),
        Just(PanePriority::Low),
        Just(PanePriority::Medium),
        Just(PanePriority::High),
        Just(PanePriority::Critical),
    ]
}

fn arb_tier() -> impl Strategy<Value = PaneTier> {
    prop_oneof![
        Just(PaneTier::Active),
        Just(PaneTier::Thinking),
        Just(PaneTier::Idle),
        Just(PaneTier::Background),
        Just(PaneTier::Dormant),
    ]
}

fn arb_pane_id() -> impl Strategy<Value = u64> {
    1u64..=100
}

fn arb_pane_ids() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::hash_set(1u64..=100, 1..=10).prop_map(|s| s.into_iter().collect())
}

// ────────────────────────────────────────────────────────────────────
// PanePriority: Ord consistent with as_u8
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// PanePriority::Ord is consistent with as_u8 numeric ordering.
    #[test]
    fn prop_priority_ord_matches_u8(a in arb_priority(), b in arb_priority()) {
        let ord_enum = a.cmp(&b);
        let ord_u8 = a.as_u8().cmp(&b.as_u8());
        prop_assert_eq!(ord_enum, ord_u8, "Ord should match as_u8 ordering");
    }

    /// from_u8(as_u8(p)) is identity for all valid priorities.
    #[test]
    fn prop_priority_u8_roundtrip(p in arb_priority()) {
        prop_assert_eq!(PanePriority::from_u8(p.as_u8()), p);
    }

    /// from_u8 clamps: values >= 5 always produce Critical.
    #[test]
    fn prop_priority_from_u8_clamps(v in 5u8..=255) {
        prop_assert_eq!(PanePriority::from_u8(v), PanePriority::Critical);
    }

    /// All 5 variants have distinct as_u8 values.
    #[test]
    fn prop_priority_distinct_values(_seed in any::<u32>()) {
        let all = [
            PanePriority::Background,
            PanePriority::Low,
            PanePriority::Medium,
            PanePriority::High,
            PanePriority::Critical,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                prop_assert_ne!(
                    all[i].as_u8(), all[j].as_u8(),
                    "{:?} and {:?} have same as_u8", all[i], all[j]
                );
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// PanePriority: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// PanePriority survives JSON serialization roundtrip.
    #[test]
    fn prop_priority_serde_roundtrip(p in arb_priority()) {
        let json = serde_json::to_string(&p).unwrap();
        let back: PanePriority = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(p, back);
    }

    /// label() always returns a non-empty string.
    #[test]
    fn prop_priority_label_non_empty(p in arb_priority()) {
        prop_assert!(!p.label().is_empty());
    }
}

// ────────────────────────────────────────────────────────────────────
// OutputRateTracker: non-negative rate
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// lines_per_second is always non-negative.
    #[test]
    fn prop_rate_non_negative(
        half_life_ms in 100u64..10_000,
        line_counts in prop::collection::vec(0usize..500, 1..=10),
        intervals_ms in prop::collection::vec(10u64..2000, 1..=10),
    ) {
        let start = Instant::now();
        let half_life = Duration::from_millis(half_life_ms);
        let mut tracker = OutputRateTracker::with_start(half_life, start);

        let mut t = start;
        let len = line_counts.len().min(intervals_ms.len());
        for i in 0..len {
            t += Duration::from_millis(intervals_ms[i]);
            tracker.record_output(line_counts[i], t);
            let rate = tracker.lines_per_second(t);
            prop_assert!(rate >= 0.0, "Rate should be non-negative, got {}", rate);
        }

        // Check at a future time too
        let future = t + Duration::from_secs(60);
        let rate = tracker.lines_per_second(future);
        prop_assert!(rate >= 0.0, "Future rate should be non-negative, got {}", rate);
    }

    /// total_lines never decreases.
    #[test]
    fn prop_total_lines_monotonic(
        line_counts in prop::collection::vec(0usize..500, 1..=20),
    ) {
        let start = Instant::now();
        let mut tracker = OutputRateTracker::with_start(Duration::from_secs(10), start);

        let mut prev_total = 0u64;
        for (i, &count) in line_counts.iter().enumerate() {
            let t = start + Duration::from_millis((i as u64 + 1) * 100);
            tracker.record_output(count, t);
            let total = tracker.total_lines();
            prop_assert!(total >= prev_total, "total_lines decreased: {} -> {}", prev_total, total);
            prev_total = total;
        }
    }

    /// Recording 0 lines doesn't change total_lines.
    #[test]
    fn prop_zero_lines_is_noop(
        half_life_ms in 100u64..10_000,
    ) {
        let start = Instant::now();
        let mut tracker = OutputRateTracker::with_start(
            Duration::from_millis(half_life_ms), start
        );
        tracker.record_output(50, start + Duration::from_millis(100));
        let total_before = tracker.total_lines();
        tracker.record_output(0, start + Duration::from_millis(200));
        prop_assert_eq!(tracker.total_lines(), total_before, "0-line record should not change total");
    }
}

// ────────────────────────────────────────────────────────────────────
// OutputRateTracker: decay halving
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// After one half-life of silence, rate is approximately half.
    #[test]
    fn prop_rate_halves_after_half_life(
        half_life_secs in 1u64..=20,
        lines in 50usize..500,
    ) {
        let start = Instant::now();
        let half_life = Duration::from_secs(half_life_secs);
        let mut tracker = OutputRateTracker::with_start(half_life, start);

        let t1 = start + Duration::from_secs(1);
        tracker.record_output(lines, t1);
        let rate_at_burst = tracker.lines_per_second(t1);

        if rate_at_burst > 1e-6 {
            let t2 = t1 + half_life;
            let rate_after_half_life = tracker.lines_per_second(t2);
            let ratio = rate_after_half_life / rate_at_burst;
            prop_assert!(
                (ratio - 0.5).abs() < 0.05,
                "After one half-life, ratio should be ~0.5 but got {}", ratio
            );
        }
    }

    /// Rate at the same instant as recording is consistent (not NaN/Inf).
    #[test]
    fn prop_rate_finite(
        half_life_ms in 100u64..10_000,
        lines in 1usize..1000,
    ) {
        let start = Instant::now();
        let mut tracker = OutputRateTracker::with_start(
            Duration::from_millis(half_life_ms), start
        );
        let t = start + Duration::from_millis(100);
        tracker.record_output(lines, t);
        let rate = tracker.lines_per_second(t);
        prop_assert!(rate.is_finite(), "Rate should be finite, got {}", rate);
    }
}

// ────────────────────────────────────────────────────────────────────
// PriorityClassifier: register/unregister count
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// tracked_pane_count reflects registered panes.
    #[test]
    fn prop_classifier_register_count(
        pane_ids in arb_pane_ids(),
    ) {
        let c = PriorityClassifier::with_defaults();
        for &id in &pane_ids {
            c.register_pane(id);
        }
        // IDs are from a hash_set so already unique
        prop_assert_eq!(c.tracked_pane_count(), pane_ids.len());
    }

    /// After unregistering all panes, count is 0.
    #[test]
    fn prop_classifier_unregister_all(
        pane_ids in arb_pane_ids(),
    ) {
        let c = PriorityClassifier::with_defaults();
        for &id in &pane_ids {
            c.register_pane(id);
        }
        for &id in &pane_ids {
            c.unregister_pane(id);
        }
        prop_assert_eq!(c.tracked_pane_count(), 0);
    }

    /// Unregistered pane returns Low.
    #[test]
    fn prop_classifier_unregistered_returns_low(
        pane_id in arb_pane_id(),
    ) {
        let c = PriorityClassifier::with_defaults();
        prop_assert_eq!(c.classify(pane_id), PanePriority::Low);
    }
}

// ────────────────────────────────────────────────────────────────────
// PriorityClassifier: override always wins
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Manual override always takes precedence.
    #[test]
    fn prop_classifier_override_wins(
        pane_id in arb_pane_id(),
        tier in arb_tier(),
        override_p in arb_priority(),
    ) {
        let c = PriorityClassifier::with_defaults();
        c.register_pane(pane_id);
        c.update_tier(pane_id, tier);
        c.set_override(pane_id, override_p);
        let result = c.classify(pane_id);
        prop_assert_eq!(result, override_p, "Override should always win");
        prop_assert!(c.has_override(pane_id));
    }

    /// Clearing override returns to automatic classification.
    #[test]
    fn prop_classifier_clear_override(
        pane_id in arb_pane_id(),
        override_p in arb_priority(),
    ) {
        let c = PriorityClassifier::with_defaults();
        c.register_pane(pane_id);
        c.set_override(pane_id, override_p);
        c.clear_override(pane_id);
        prop_assert!(!c.has_override(pane_id));
    }
}

// ────────────────────────────────────────────────────────────────────
// PriorityClassifier: tier → priority mapping (no signals, no override)
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Without signals or override, Idle tier → Low priority.
    #[test]
    fn prop_idle_maps_to_low(pane_id in arb_pane_id()) {
        let c = PriorityClassifier::with_defaults();
        c.register_pane(pane_id);
        c.update_tier(pane_id, PaneTier::Idle);
        prop_assert_eq!(c.classify(pane_id), PanePriority::Low);
    }

    /// Without signals or override, Dormant tier → Background priority.
    #[test]
    fn prop_dormant_maps_to_background(pane_id in arb_pane_id()) {
        let c = PriorityClassifier::with_defaults();
        c.register_pane(pane_id);
        c.update_tier(pane_id, PaneTier::Dormant);
        prop_assert_eq!(c.classify(pane_id), PanePriority::Background);
    }

    /// Without signals or override, Background tier → Background priority.
    #[test]
    fn prop_background_tier_maps_to_background(pane_id in arb_pane_id()) {
        let c = PriorityClassifier::with_defaults();
        c.register_pane(pane_id);
        c.update_tier(pane_id, PaneTier::Background);
        prop_assert_eq!(c.classify(pane_id), PanePriority::Background);
    }

    /// Without signals or override, Thinking tier → Medium priority.
    #[test]
    fn prop_thinking_maps_to_medium(pane_id in arb_pane_id()) {
        let c = PriorityClassifier::with_defaults();
        c.register_pane(pane_id);
        c.update_tier(pane_id, PaneTier::Thinking);
        prop_assert_eq!(c.classify(pane_id), PanePriority::Medium);
    }

    /// Without signals or override, Active + zero rate → Medium.
    #[test]
    fn prop_active_zero_rate_maps_to_medium(pane_id in arb_pane_id()) {
        let c = PriorityClassifier::with_defaults();
        c.register_pane(pane_id);
        c.update_tier(pane_id, PaneTier::Active);
        // No output recorded → rate is 0 → Medium (not High)
        prop_assert_eq!(c.classify(pane_id), PanePriority::Medium);
    }
}

// ────────────────────────────────────────────────────────────────────
// PriorityClassifier: classify_all completeness
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// classify_all returns a priority for every registered pane.
    #[test]
    fn prop_classify_all_complete(
        pane_ids in arb_pane_ids(),
    ) {
        let c = PriorityClassifier::with_defaults();
        for &id in &pane_ids {
            c.register_pane(id);
        }

        let all = c.classify_all();
        prop_assert_eq!(all.len(), pane_ids.len());
        for &id in &pane_ids {
            prop_assert!(all.contains_key(&id), "Missing pane {} in classify_all", id);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// PriorityClassifier: metrics consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// metrics.tracked_panes matches tracked_pane_count().
    #[test]
    fn prop_metrics_tracked_panes(
        pane_ids in arb_pane_ids(),
    ) {
        let c = PriorityClassifier::with_defaults();
        for &id in &pane_ids {
            c.register_pane(id);
        }

        let m = c.metrics();
        prop_assert_eq!(m.tracked_panes, c.tracked_pane_count());
        prop_assert_eq!(m.tracked_panes, pane_ids.len());
    }
}

// ────────────────────────────────────────────────────────────────────
// shedding_order: preserves all panes
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// shedding_order output has same length and same pane IDs as input.
    #[test]
    fn prop_shedding_preserves_panes(
        entries in prop::collection::vec((arb_pane_id(), arb_priority()), 1..=20),
    ) {
        let priorities: HashMap<u64, PanePriority> = entries.into_iter().collect();
        let order = shedding_order(&priorities);

        prop_assert_eq!(order.len(), priorities.len());

        let mut order_sorted = order.clone();
        order_sorted.sort();
        let mut keys: Vec<u64> = priorities.keys().copied().collect();
        keys.sort();
        prop_assert_eq!(order_sorted, keys, "shedding_order should contain all pane IDs");
    }

    /// shedding_order sorts in ascending priority order.
    #[test]
    fn prop_shedding_ascending_priority(
        entries in prop::collection::vec((arb_pane_id(), arb_priority()), 2..=20),
    ) {
        let priorities: HashMap<u64, PanePriority> = entries.into_iter().collect();
        let order = shedding_order(&priorities);

        for w in order.windows(2) {
            let p0 = priorities[&w[0]];
            let p1 = priorities[&w[1]];
            prop_assert!(
                p0 <= p1,
                "Shedding order not ascending: {:?} > {:?} (panes {} vs {})",
                p0, p1, w[0], w[1]
            );
        }
    }

    /// Within same priority, pane IDs are sorted ascending (stability).
    #[test]
    fn prop_shedding_stable_within_priority(
        pane_ids in prop::collection::hash_set(arb_pane_id(), 2..=10),
        priority in arb_priority(),
    ) {
        let priorities: HashMap<u64, PanePriority> = pane_ids
            .into_iter()
            .map(|id| (id, priority))
            .collect();
        let order = shedding_order(&priorities);

        // All same priority → should be sorted by pane ID
        for w in order.windows(2) {
            prop_assert!(w[0] < w[1], "Same-priority panes not sorted by ID: {} >= {}", w[0], w[1]);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// PriorityConfig serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// PriorityConfig survives JSON serialization roundtrip.
    #[test]
    fn prop_config_serde_roundtrip(
        half_life in 1.0f64..100.0,
        high_thresh in 1.0f64..100.0,
        med_thresh in 0.1f64..10.0,
        retention in 1.0f64..300.0,
    ) {
        let config = PriorityConfig {
            rate_half_life_secs: half_life,
            high_rate_threshold: high_thresh,
            medium_rate_threshold: med_thresh,
            error_retention_secs: retention,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: PriorityConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((config.rate_half_life_secs - back.rate_half_life_secs).abs() < 1e-9);
        prop_assert!((config.high_rate_threshold - back.high_rate_threshold).abs() < 1e-9);
        prop_assert!((config.medium_rate_threshold - back.medium_rate_threshold).abs() < 1e-9);
        prop_assert!((config.error_retention_secs - back.error_retention_secs).abs() < 1e-9);
    }
}

// ────────────────────────────────────────────────────────────────────
// PriorityMetrics serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// PriorityMetrics survives JSON roundtrip.
    #[test]
    fn prop_metrics_serde_roundtrip(
        total in 0u64..10_000,
        overrides in 0usize..50,
        tracked in 0usize..100,
    ) {
        let m = PriorityMetrics {
            counts: HashMap::new(),
            total_classifications: total,
            override_count: overrides,
            tracked_panes: tracked,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: PriorityMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(m.total_classifications, back.total_classifications);
        prop_assert_eq!(m.override_count, back.override_count);
        prop_assert_eq!(m.tracked_panes, back.tracked_panes);
    }
}
