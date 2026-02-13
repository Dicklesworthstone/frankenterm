//! Property-based tests for VOI capture scheduler invariants.
//!
//! Bead: wa-cn72
//!
//! Validates:
//! 1. VOI non-negativity: VOI(i,t) ≥ 0 for all panes and times
//! 2. VOI monotonic in staleness: staler panes have higher VOI
//! 3. VOI proportional to importance: higher W(i) → higher VOI
//! 4. VOI inversely proportional to cost: higher C(i) → lower VOI
//! 5. Schedule descending: schedule is sorted by VOI descending
//! 6. Entropy bounded: H ∈ [0, max_entropy] after any operation
//! 7. Probabilities sum to 1: belief probabilities are normalized
//! 8. Drift increases entropy: entropy grows with staleness
//! 9. Drift capped: entropy never exceeds max_entropy
//! 10. Interval clamped: suggested interval ∈ [min, max]
//! 11. Backpressure reduces VOI: Red < Green for same pane
//! 12. Register/unregister: pane_count tracks correctly
//! 13. Observations tracked: total_observations increments

use proptest::prelude::*;

use frankenterm_core::bayesian_ledger::PaneState;
use frankenterm_core::voi::{BackpressureTierInput, VoiConfig, VoiScheduler};

// =============================================================================
// Strategies
// =============================================================================

fn arb_pane_ids(count: usize) -> impl Strategy<Value = Vec<u64>> {
    // Unique pane IDs.
    proptest::collection::hash_set(1_u64..1000, count)
        .prop_map(|s| s.into_iter().collect())
}

fn arb_time_ms() -> impl Strategy<Value = u64> {
    1000_u64..100_000
}

fn arb_log_likelihoods() -> impl Strategy<Value = [f64; PaneState::COUNT]> {
    proptest::array::uniform7(-5.0_f64..5.0).prop_map(|arr| arr)
}

fn arb_config() -> impl Strategy<Value = VoiConfig> {
    (
        0.001_f64..1.0,   // min_voi_threshold
        0.01_f64..1.0,    // entropy_drift_rate
        10_u64..200,       // min_poll_interval_ms
        5000_u64..60_000,  // max_poll_interval_ms
        0.5_f64..10.0,     // default_cost_ms
        0.5_f64..5.0,      // default_importance
    )
        .prop_map(
            |(threshold, drift, min_poll, max_poll, cost, importance)| VoiConfig {
                min_voi_threshold: threshold,
                entropy_drift_rate: drift,
                min_poll_interval_ms: min_poll,
                max_poll_interval_ms: max_poll.max(min_poll + 1000),
                default_cost_ms: cost,
                default_importance: importance,
                ..VoiConfig::default()
            },
        )
}

// =============================================================================
// Property: VOI is non-negative
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn voi_non_negative(
        pane_ids in arb_pane_ids(5),
        start_ms in arb_time_ms(),
        delta_ms in 0_u64..30_000,
    ) {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        for &id in &pane_ids {
            sched.register_pane(id, start_ms);
        }

        let result = sched.schedule(start_ms + delta_ms);
        for decision in &result.schedule {
            prop_assert!(decision.voi >= 0.0,
                "VOI for pane {} should be >= 0, got {}",
                decision.pane_id, decision.voi);
        }
    }
}

// =============================================================================
// Property: VOI monotonic in staleness
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn voi_increases_with_staleness(
        start_ms in arb_time_ms(),
    ) {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, start_ms);

        // Give a strong observation to reduce entropy first.
        let mut lls = [0.0; PaneState::COUNT];
        lls[PaneState::Active.index()] = 10.0;
        sched.update_belief(1, &lls, start_ms);

        let mut prev_voi = 0.0;
        for dt in [0, 1000, 5000, 10_000, 30_000] {
            let result = sched.schedule(start_ms + dt);
            if let Some(d) = result.schedule.first() {
                prop_assert!(d.voi >= prev_voi - 0.001,
                    "VOI should not decrease with staleness: dt={}, voi={}, prev={}",
                    dt, d.voi, prev_voi);
                prev_voi = d.voi;
            }
        }
    }
}

// =============================================================================
// Property: Higher importance → higher VOI
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn higher_importance_higher_voi(
        start_ms in arb_time_ms(),
        delta_ms in 1000_u64..10_000,
        importance_lo in 0.1_f64..5.0,
        importance_delta in 0.1_f64..5.0,
    ) {
        let importance_hi = importance_lo + importance_delta;
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, start_ms);
        sched.register_pane(2, start_ms);
        sched.set_importance(1, importance_lo);
        sched.set_importance(2, importance_hi);

        let result = sched.schedule(start_ms + delta_ms);
        let voi_lo = result.schedule.iter().find(|d| d.pane_id == 1).unwrap().voi;
        let voi_hi = result.schedule.iter().find(|d| d.pane_id == 2).unwrap().voi;

        prop_assert!(voi_hi >= voi_lo - 0.001,
            "higher importance ({}) should have >= VOI than lower ({}): {} vs {}",
            importance_hi, importance_lo, voi_hi, voi_lo);
    }
}

// =============================================================================
// Property: Higher cost → lower VOI
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn higher_cost_lower_voi(
        start_ms in arb_time_ms(),
        delta_ms in 1000_u64..10_000,
        cost_lo in 0.5_f64..5.0,
        cost_delta in 0.5_f64..15.0,
    ) {
        let cost_hi = cost_lo + cost_delta;
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, start_ms);
        sched.register_pane(2, start_ms);
        sched.set_cost(1, cost_lo);
        sched.set_cost(2, cost_hi);

        let result = sched.schedule(start_ms + delta_ms);
        let voi_cheap = result.schedule.iter().find(|d| d.pane_id == 1).unwrap().voi;
        let voi_expensive = result.schedule.iter().find(|d| d.pane_id == 2).unwrap().voi;

        prop_assert!(voi_cheap >= voi_expensive - 0.001,
            "cheaper pane (cost={}) should have >= VOI than expensive (cost={}): {} vs {}",
            cost_lo, cost_hi, voi_cheap, voi_expensive);
    }
}

// =============================================================================
// Property: Schedule sorted by VOI descending
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn schedule_sorted_descending(
        pane_ids in arb_pane_ids(8),
        start_ms in arb_time_ms(),
        delta_ms in 0_u64..30_000,
    ) {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        for &id in &pane_ids {
            sched.register_pane(id, start_ms);
        }

        let result = sched.schedule(start_ms + delta_ms);

        for window in result.schedule.windows(2) {
            prop_assert!(window[0].voi >= window[1].voi - 0.001,
                "schedule should be sorted by VOI descending: {} (voi={}) before {} (voi={})",
                window[0].pane_id, window[0].voi,
                window[1].pane_id, window[1].voi);
        }
    }
}

// =============================================================================
// Property: Entropy bounded [0, max_entropy]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn entropy_bounded(
        start_ms in arb_time_ms(),
        delta_ms in 0_u64..60_000,
        lls in arb_log_likelihoods(),
    ) {
        let config = VoiConfig::default();
        let max_h = config.max_entropy;
        let mut sched = VoiScheduler::new(config);
        sched.register_pane(1, start_ms);

        // Update belief with random likelihoods.
        sched.update_belief(1, &lls, start_ms);

        // Apply drift.
        sched.apply_drift(start_ms + delta_ms);

        let result = sched.schedule(start_ms + delta_ms);
        for d in &result.schedule {
            prop_assert!(d.entropy >= 0.0,
                "entropy should be >= 0, got {}", d.entropy);
            prop_assert!(d.entropy <= max_h + 0.01,
                "entropy should be <= max ({}), got {}", max_h, d.entropy);
        }
    }
}

// =============================================================================
// Property: Probabilities sum to 1
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn probabilities_normalized(
        start_ms in arb_time_ms(),
        lls_sequence in proptest::collection::vec(arb_log_likelihoods(), 0..5),
    ) {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, start_ms);

        for (i, lls) in lls_sequence.iter().enumerate() {
            sched.update_belief(1, lls, start_ms + (i as u64 + 1) * 1000);
        }

        let probs = sched.pane_probabilities(1).unwrap();
        let sum: f64 = probs.iter().sum();
        prop_assert!((sum - 1.0).abs() < 1e-6,
            "probabilities should sum to 1.0, got {}", sum);

        // All probabilities should be non-negative.
        for (i, &p) in probs.iter().enumerate() {
            prop_assert!(p >= 0.0,
                "probability {} should be >= 0, got {}", i, p);
        }
    }
}

// =============================================================================
// Property: Drift increases entropy (from non-max)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn drift_increases_entropy(
        start_ms in arb_time_ms(),
        drift_secs in 1_u64..60,
    ) {
        let mut sched = VoiScheduler::new(VoiConfig {
            entropy_drift_rate: 0.5,
            ..VoiConfig::default()
        });
        sched.register_pane(1, start_ms);

        // Give strong evidence to reduce entropy below max.
        let mut lls = [0.0; PaneState::COUNT];
        lls[PaneState::Active.index()] = 10.0;
        sched.update_belief(1, &lls, start_ms);

        // Capture entropy before drift.
        let result_before = sched.schedule(start_ms);
        let h_before = result_before.schedule[0].entropy;

        // Apply drift.
        let drift_ms = drift_secs * 1000;
        sched.apply_drift(start_ms + drift_ms);

        // Capture entropy after drift.
        let result_after = sched.schedule(start_ms + drift_ms);
        let h_after = result_after.schedule[0].entropy;

        // Entropy should increase (unless already at max).
        if h_before < sched.snapshot(start_ms).config.max_entropy - 0.01 {
            prop_assert!(h_after >= h_before - 0.001,
                "drift should increase entropy: {} -> {}", h_before, h_after);
        }
    }
}

// =============================================================================
// Property: Drift capped at max_entropy
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn drift_capped(
        start_ms in arb_time_ms(),
        drift_secs in 100_u64..10_000,
        drift_rate in 0.1_f64..10.0,
    ) {
        let config = VoiConfig {
            entropy_drift_rate: drift_rate,
            ..VoiConfig::default()
        };
        let max_h = config.max_entropy;
        let mut sched = VoiScheduler::new(config);
        sched.register_pane(1, start_ms);

        sched.apply_drift(start_ms + drift_secs * 1000);

        let result = sched.schedule(start_ms + drift_secs * 1000);
        let h = result.schedule[0].entropy;
        prop_assert!(h <= max_h + 0.01,
            "entropy after drift should be <= max ({}), got {}", max_h, h);
    }
}

// =============================================================================
// Property: Interval clamped to [min, max]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn interval_clamped(
        config in arb_config(),
        start_ms in arb_time_ms(),
        delta_ms in 0_u64..60_000,
    ) {
        let min_interval = config.min_poll_interval_ms;
        let max_interval = config.max_poll_interval_ms;
        let mut sched = VoiScheduler::new(config);
        sched.register_pane(1, start_ms);

        let interval = sched.suggested_interval_ms(1, start_ms + delta_ms);
        prop_assert!(interval >= min_interval,
            "interval ({}) should be >= min ({})", interval, min_interval);
        prop_assert!(interval <= max_interval,
            "interval ({}) should be <= max ({})", interval, max_interval);
    }
}

// =============================================================================
// Property: Backpressure reduces VOI
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn backpressure_reduces_voi(
        start_ms in arb_time_ms(),
        delta_ms in 1000_u64..10_000,
    ) {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, start_ms);

        sched.set_backpressure(BackpressureTierInput::Green);
        let result_green = sched.schedule(start_ms + delta_ms);
        let voi_green = result_green.schedule[0].voi;

        sched.set_backpressure(BackpressureTierInput::Yellow);
        let result_yellow = sched.schedule(start_ms + delta_ms);
        let voi_yellow = result_yellow.schedule[0].voi;

        sched.set_backpressure(BackpressureTierInput::Red);
        let result_red = sched.schedule(start_ms + delta_ms);
        let voi_red = result_red.schedule[0].voi;

        prop_assert!(voi_green >= voi_yellow - 0.001,
            "green VOI ({}) should be >= yellow ({})", voi_green, voi_yellow);
        prop_assert!(voi_yellow >= voi_red - 0.001,
            "yellow VOI ({}) should be >= red ({})", voi_yellow, voi_red);
    }
}

// =============================================================================
// Property: Register/unregister tracking
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn register_unregister_count(
        pane_ids in arb_pane_ids(10),
        start_ms in arb_time_ms(),
    ) {
        let mut sched = VoiScheduler::new(VoiConfig::default());

        for &id in &pane_ids {
            sched.register_pane(id, start_ms);
        }
        prop_assert_eq!(sched.pane_count(), pane_ids.len());

        // Unregister half.
        let half = pane_ids.len() / 2;
        for &id in &pane_ids[..half] {
            sched.unregister_pane(id);
        }
        prop_assert_eq!(sched.pane_count(), pane_ids.len() - half);

        // Unregistered panes should not appear in schedule.
        let result = sched.schedule(start_ms + 5000);
        for d in &result.schedule {
            prop_assert!(!pane_ids[..half].contains(&d.pane_id),
                "unregistered pane {} should not appear in schedule", d.pane_id);
        }
    }
}

// =============================================================================
// Property: Observations tracked
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn observations_increment(
        n_updates in 1_usize..20,
        start_ms in arb_time_ms(),
    ) {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, start_ms);

        let lls = [0.0; PaneState::COUNT];
        for i in 0..n_updates {
            sched.update_belief(1, &lls, start_ms + (i as u64 + 1) * 1000);
        }

        prop_assert_eq!(sched.total_observations(), n_updates as u64,
            "total observations should be {}", n_updates);
    }
}

// =============================================================================
// Property: Schedule completeness — all registered panes appear
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn schedule_contains_all_panes(
        pane_ids in arb_pane_ids(8),
        start_ms in arb_time_ms(),
        delta_ms in 0_u64..10_000,
    ) {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        for &id in &pane_ids {
            sched.register_pane(id, start_ms);
        }

        let result = sched.schedule(start_ms + delta_ms);
        prop_assert_eq!(result.schedule.len(), pane_ids.len(),
            "schedule should contain all {} panes", pane_ids.len());

        let scheduled_ids: std::collections::HashSet<u64> =
            result.schedule.iter().map(|d| d.pane_id).collect();
        for &id in &pane_ids {
            prop_assert!(scheduled_ids.contains(&id),
                "pane {} should appear in schedule", id);
        }
    }
}

// =============================================================================
// Property: Snapshot consistency
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn snapshot_consistent(
        pane_ids in arb_pane_ids(5),
        start_ms in arb_time_ms(),
        n_updates in 0_usize..5,
    ) {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        for &id in &pane_ids {
            sched.register_pane(id, start_ms);
        }

        let lls = [0.0; PaneState::COUNT];
        for i in 0..n_updates {
            // Update first pane.
            sched.update_belief(pane_ids[0], &lls, start_ms + (i as u64 + 1) * 1000);
        }

        let snap = sched.snapshot(start_ms + 5000);
        prop_assert_eq!(snap.pane_count, pane_ids.len());
        prop_assert_eq!(snap.pane_states.len(), pane_ids.len());
        prop_assert_eq!(snap.total_observations, n_updates as u64);

        // All entropies in snapshot should be bounded.
        for entry in &snap.pane_states {
            prop_assert!(entry.entropy >= 0.0);
            prop_assert!(entry.entropy <= snap.config.max_entropy + 0.01);
        }
    }
}
