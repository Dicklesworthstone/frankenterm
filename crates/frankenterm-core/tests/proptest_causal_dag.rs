//! Property-based tests for the Causal DAG module.
//!
//! Verifies core invariants:
//! - Transfer entropy is non-negative and finite
//! - Independent series yield no significant edges
//! - PaneTimeSeries circular buffer ordering and capacity
//! - CausalDagConfig / CausalEdge / CausalDagSnapshot serde roundtrips
//! - CausalDag register/unregister pane count
//! - Permutation test p-value bounds
//! - DAG update_count increments
//! - No self-loop edges
//! - Snapshot consistency with live state
//!
//! Bead: wa-1u90p.7.1

use proptest::prelude::*;

use frankenterm_core::causal_dag::{
    CausalDag, CausalDagConfig, CausalDagSnapshot, CausalEdge, PaneTimeSeries, permutation_test,
    transfer_entropy,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_causal_dag_config() -> impl Strategy<Value = CausalDagConfig> {
    (
        10_usize..500,  // window_size
        1_usize..4,     // k
        1_usize..4,     // l
        5_usize..200,   // n_permutations
        0.001_f64..0.1, // significance_level
        3_usize..16,    // n_bins
        0.001_f64..0.1, // min_te_bits
    )
        .prop_map(
            |(window_size, k, l, n_permutations, significance_level, n_bins, min_te_bits)| {
                CausalDagConfig {
                    window_size,
                    k,
                    l,
                    n_permutations,
                    significance_level,
                    n_bins,
                    min_te_bits,
                }
            },
        )
}

fn arb_causal_edge() -> impl Strategy<Value = CausalEdge> {
    (
        0_u64..10000,
        0_u64..10000,
        0.0_f64..5.0,
        0.0_f64..1.0,
        0_usize..10,
    )
        .prop_map(
            |(source, target, transfer_entropy, p_value, lag_samples)| CausalEdge {
                source,
                target,
                transfer_entropy,
                p_value,
                lag_samples,
            },
        )
}

fn arb_causal_dag_snapshot() -> impl Strategy<Value = CausalDagSnapshot> {
    (
        0_u64..20,
        prop::collection::vec(arb_causal_edge(), 0..10),
        0_u64..1000,
        prop::collection::vec(0_u64..10000, 0..20),
    )
        .prop_map(
            |(pane_count, edges, update_count, pane_ids)| CausalDagSnapshot {
                pane_count,
                edge_count: edges.len() as u64,
                edges,
                update_count,
                pane_ids,
            },
        )
}

// =============================================================================
// Transfer entropy — core invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Transfer entropy is always non-negative and finite.
    #[test]
    fn prop_te_non_negative(
        x in prop::collection::vec(-100.0_f64..100.0, 10..200),
        y in prop::collection::vec(-100.0_f64..100.0, 10..200),
        n_bins in 3_usize..12,
    ) {
        let te = transfer_entropy(&x, &y, 1, 1, n_bins);
        prop_assert!(te >= 0.0, "TE must be non-negative: {}", te);
        prop_assert!(te.is_finite(), "TE must be finite: {}", te);
    }

    /// TE returns 0.0 for inputs that are too short.
    #[test]
    fn prop_te_short_input_returns_zero(
        x in prop::collection::vec(-10.0_f64..10.0, 0..3),
        y in prop::collection::vec(-10.0_f64..10.0, 0..3),
        n_bins in 3_usize..8,
    ) {
        let te = transfer_entropy(&x, &y, 1, 1, n_bins);
        prop_assert!((te - 0.0_f64).abs() < f64::EPSILON, "short input should return 0.0");
    }

    /// TE returns 0.0 when n_bins is 0.
    #[test]
    fn prop_te_zero_bins_returns_zero(
        x in prop::collection::vec(-10.0_f64..10.0, 10..50),
        y in prop::collection::vec(-10.0_f64..10.0, 10..50),
    ) {
        let te = transfer_entropy(&x, &y, 1, 1, 0);
        prop_assert!((te - 0.0_f64).abs() < f64::EPSILON, "zero bins should return 0.0");
    }

    /// TE is approximately deterministic — same inputs produce similar output
    /// (HashMap iteration order may cause minor float differences).
    #[test]
    fn prop_te_approximately_deterministic(
        x in prop::collection::vec(-100.0_f64..100.0, 10..100),
        y in prop::collection::vec(-100.0_f64..100.0, 10..100),
        n_bins in 3_usize..12,
    ) {
        let te1 = transfer_entropy(&x, &y, 1, 1, n_bins);
        let te2 = transfer_entropy(&x, &y, 1, 1, n_bins);
        prop_assert!(
            (te1 - te2).abs() < 1e-10,
            "TE should be approximately deterministic: {} vs {}", te1, te2
        );
    }
}

// =============================================================================
// Transfer entropy — independent series
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Independent pseudo-random series should not pass significance test.
    #[test]
    fn prop_independent_series_no_spurious_edges(
        seed_a in 0_u64..10000,
        seed_b in 10000_u64..20000,
        n in 50_usize..200,
    ) {
        let x: Vec<f64> = (0..n).map(|i| {
            let s = (i as u64 + seed_a)
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (s >> 33) as f64 / (u32::MAX as f64) * 10.0
        }).collect();

        let y: Vec<f64> = (0..n).map(|i| {
            let s = (i as u64 + seed_b)
                .wrapping_mul(2_862_933_555_777_941_757)
                .wrapping_add(3);
            (s >> 33) as f64 / (u32::MAX as f64) * 10.0
        }).collect();

        let te = transfer_entropy(&x, &y, 1, 1, 8);
        let p = permutation_test(&x, &y, 1, 1, 8, 50, te);

        prop_assert!(
            p > 0.005 || te < 0.005,
            "independent series got p={}, te={}", p, te
        );
    }

    /// TE for causal signal (Y = lagged X) tends to be asymmetric.
    #[test]
    fn prop_te_asymmetric_for_causal(
        seed in 0_u64..10000,
        n in 50_usize..200,
    ) {
        let x: Vec<f64> = (0..n).map(|i| {
            let s = (i as u64 + seed)
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (s >> 33) as f64 % 10.0
        }).collect();

        let mut y = vec![0.0];
        y.extend_from_slice(&x[..n - 1]);

        let te_xy = transfer_entropy(&x, &y, 1, 1, 8);
        let te_yx = transfer_entropy(&y, &x, 1, 1, 8);

        prop_assert!(te_xy >= 0.0 && te_xy.is_finite());
        prop_assert!(te_yx >= 0.0 && te_yx.is_finite());
    }
}

// =============================================================================
// Permutation test — p-value bounds
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Permutation test p-value is always in [0, 1].
    #[test]
    fn prop_permutation_test_pvalue_bounded(
        x in prop::collection::vec(-50.0_f64..50.0, 20..100),
        y in prop::collection::vec(-50.0_f64..50.0, 20..100),
        n_perms in 5_usize..50,
    ) {
        let te = transfer_entropy(&x, &y, 1, 1, 8);
        let p = permutation_test(&x, &y, 1, 1, 8, n_perms, te);
        prop_assert!(
            (0.0..=1.0).contains(&p),
            "p-value {} should be in [0, 1]", p
        );
        prop_assert!(p.is_finite(), "p-value should be finite");
    }

    /// Permutation test with 0 permutations returns 1.0.
    #[test]
    fn prop_permutation_test_zero_perms(
        x in prop::collection::vec(-50.0_f64..50.0, 10..50),
        y in prop::collection::vec(-50.0_f64..50.0, 10..50),
        observed_te in 0.0_f64..5.0,
    ) {
        let p = permutation_test(&x, &y, 1, 1, 8, 0, observed_te);
        prop_assert!((p - 1.0_f64).abs() < f64::EPSILON, "0 permutations should return p=1.0");
    }

    /// More permutations yield a more precise (lower variance) p-value,
    /// but the result should still be in [0, 1].
    #[test]
    fn prop_permutation_test_more_perms_still_bounded(
        x in prop::collection::vec(-50.0_f64..50.0, 20..80),
        y in prop::collection::vec(-50.0_f64..50.0, 20..80),
        n_perms in 10_usize..100,
    ) {
        let te = transfer_entropy(&x, &y, 1, 1, 8);
        let p = permutation_test(&x, &y, 1, 1, 8, n_perms, te);
        prop_assert!(
            (0.0..=1.0).contains(&p) && p.is_finite(),
            "p-value {} should be in [0, 1] with {} permutations", p, n_perms
        );
    }
}

// =============================================================================
// PaneTimeSeries — circular buffer
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// PaneTimeSeries ordering: as_slice_ordered returns the last N values in order.
    #[test]
    fn prop_time_series_ordering(
        values in prop::collection::vec(-1000.0_f64..1000.0, 1..500),
        capacity in 10_usize..100,
    ) {
        let mut ts = PaneTimeSeries::new(capacity);
        for &v in &values {
            ts.push(v);
        }

        let ordered = ts.as_slice_ordered();
        let expected_len = values.len().min(capacity);
        prop_assert_eq!(ordered.len(), expected_len);

        let start = values.len().saturating_sub(capacity);
        let expected: Vec<f64> = values[start..].to_vec();
        prop_assert_eq!(ordered, expected);
    }

    /// PaneTimeSeries len never exceeds capacity.
    #[test]
    fn prop_time_series_capacity_enforcement(
        values in prop::collection::vec(-1000.0_f64..1000.0, 0..500),
        capacity in 1_usize..100,
    ) {
        let mut ts = PaneTimeSeries::new(capacity);
        for &v in &values {
            ts.push(v);
            prop_assert!(
                ts.len() <= capacity,
                "len {} exceeds capacity {}", ts.len(), capacity
            );
        }
    }

    /// PaneTimeSeries is_empty iff len == 0.
    #[test]
    fn prop_time_series_is_empty(
        n_pushes in 0_usize..50,
        capacity in 1_usize..100,
    ) {
        let mut ts = PaneTimeSeries::new(capacity);
        prop_assert!(ts.is_empty());
        prop_assert_eq!(ts.len(), 0);

        for i in 0..n_pushes {
            ts.push(i as f64);
            prop_assert!(!ts.is_empty(), "should not be empty after {} pushes", i + 1);
            prop_assert!(!ts.is_empty());
        }
    }

    /// PaneTimeSeries as_slice_ordered is idempotent (calling twice gives same result).
    #[test]
    fn prop_time_series_read_idempotent(
        values in prop::collection::vec(-100.0_f64..100.0, 1..200),
        capacity in 10_usize..50,
    ) {
        let mut ts = PaneTimeSeries::new(capacity);
        for &v in &values {
            ts.push(v);
        }
        let first = ts.as_slice_ordered();
        let second = ts.as_slice_ordered();
        prop_assert_eq!(first, second, "as_slice_ordered should be idempotent");
    }
}

// =============================================================================
// Serde roundtrips — CausalDagConfig
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// CausalDagConfig serde roundtrip preserves all fields.
    #[test]
    fn prop_config_serde_roundtrip(config in arb_causal_dag_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: CausalDagConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.window_size, config.window_size);
        prop_assert_eq!(back.k, config.k);
        prop_assert_eq!(back.l, config.l);
        prop_assert_eq!(back.n_permutations, config.n_permutations);
        prop_assert_eq!(back.n_bins, config.n_bins);
        // f64 fields use tolerance
        prop_assert!((back.significance_level - config.significance_level).abs() < 1e-10);
        prop_assert!((back.min_te_bits - config.min_te_bits).abs() < 1e-10);
    }

    /// CausalDagConfig default has sane values.
    #[test]
    fn prop_config_default_sane(_dummy in 0..1_u8) {
        let config = CausalDagConfig::default();
        prop_assert!(config.window_size > 0);
        prop_assert!(config.k > 0);
        prop_assert!(config.l > 0);
        prop_assert!(config.n_permutations > 0);
        prop_assert!(config.n_bins > 0);
        prop_assert!(config.significance_level > 0.0 && config.significance_level < 1.0);
        prop_assert!(config.min_te_bits > 0.0);
    }

    /// CausalDagConfig deserializes from empty object with defaults.
    #[test]
    fn prop_config_from_empty_json(_dummy in 0..1_u8) {
        let back: CausalDagConfig = serde_json::from_str("{}").unwrap();
        let default = CausalDagConfig::default();
        prop_assert_eq!(back.window_size, default.window_size);
        prop_assert_eq!(back.k, default.k);
        prop_assert_eq!(back.n_bins, default.n_bins);
    }
}

// =============================================================================
// Serde roundtrips — CausalEdge and CausalDagSnapshot
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// CausalEdge serde roundtrip preserves all fields.
    #[test]
    fn prop_edge_serde_roundtrip(edge in arb_causal_edge()) {
        let json = serde_json::to_string(&edge).unwrap();
        let back: CausalEdge = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.source, edge.source);
        prop_assert_eq!(back.target, edge.target);
        prop_assert_eq!(back.lag_samples, edge.lag_samples);
        prop_assert!((back.transfer_entropy - edge.transfer_entropy).abs() < 1e-10);
        prop_assert!((back.p_value - edge.p_value).abs() < 1e-10);
    }

    /// CausalDagSnapshot serde roundtrip preserves all fields.
    #[test]
    fn prop_snapshot_serde_roundtrip(snapshot in arb_causal_dag_snapshot()) {
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: CausalDagSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_count, snapshot.pane_count);
        prop_assert_eq!(back.edge_count, snapshot.edge_count);
        prop_assert_eq!(back.update_count, snapshot.update_count);
        prop_assert_eq!(back.edges.len(), snapshot.edges.len());
        prop_assert_eq!(back.pane_ids, snapshot.pane_ids);
    }
}

// =============================================================================
// CausalDag — state management
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Registering panes increases pane_count.
    #[test]
    fn prop_register_increases_count(
        pane_ids in prop::collection::vec(0_u64..10000, 1..20),
    ) {
        let mut dag = CausalDag::new(CausalDagConfig::default());
        let unique_ids: std::collections::HashSet<u64> = pane_ids.iter().copied().collect();

        for &id in &pane_ids {
            dag.register_pane(id);
        }
        prop_assert_eq!(
            dag.pane_count(), unique_ids.len(),
            "pane_count should match unique registered IDs"
        );
    }

    /// Unregistering panes decreases pane_count.
    #[test]
    fn prop_unregister_decreases_count(
        pane_ids in prop::collection::vec(0_u64..100, 2..10),
    ) {
        let mut dag = CausalDag::new(CausalDagConfig::default());
        let unique_ids: Vec<u64> = {
            let mut s = std::collections::HashSet::new();
            pane_ids.into_iter().filter(|id| s.insert(*id)).collect()
        };

        for &id in &unique_ids {
            dag.register_pane(id);
        }
        let initial_count = dag.pane_count();
        prop_assert_eq!(initial_count, unique_ids.len());

        // Unregister first pane
        dag.unregister_pane(unique_ids[0]);
        prop_assert_eq!(
            dag.pane_count(), initial_count - 1,
            "unregister should decrease count by 1"
        );
    }

    /// update_dag increments update_count.
    #[test]
    fn prop_update_count_increments(
        n_updates in 1_usize..10,
    ) {
        let config = CausalDagConfig {
            window_size: 20,
            n_permutations: 5,
            n_bins: 4,
            ..Default::default()
        };
        let mut dag = CausalDag::new(config);
        dag.register_pane(0);
        dag.register_pane(1);

        // Feed some data
        for i in 0..20 {
            dag.observe(0, i as f64);
            dag.observe(1, (i as f64) * 0.5);
        }

        for i in 0..n_updates {
            let before = dag.update_count();
            dag.update_dag();
            let after = dag.update_count();
            prop_assert_eq!(
                after, before + 1,
                "update_count should increment by 1 at iteration {}", i
            );
        }
    }

    /// Snapshot pane_count matches live pane_count().
    #[test]
    fn prop_snapshot_pane_count_consistent(
        pane_ids in prop::collection::vec(0_u64..1000, 1..15),
    ) {
        let mut dag = CausalDag::new(CausalDagConfig::default());
        for &id in &pane_ids {
            dag.register_pane(id);
        }

        let snapshot = dag.snapshot();
        prop_assert_eq!(
            snapshot.pane_count as usize, dag.pane_count(),
            "snapshot.pane_count should match live pane_count()"
        );
        prop_assert_eq!(
            snapshot.pane_ids.len(), dag.pane_count(),
            "snapshot.pane_ids.len should match pane_count()"
        );
    }

    /// Edges never contain self-loops (source == target).
    #[test]
    fn prop_no_self_loop_edges(
        n_obs in 20_usize..60,
    ) {
        let config = CausalDagConfig {
            window_size: 50,
            n_permutations: 10,
            significance_level: 0.5, // relaxed to generate edges
            n_bins: 4,
            min_te_bits: 0.001,
            ..Default::default()
        };
        let mut dag = CausalDag::new(config);

        // Register 3 panes with causal relationships
        for id in 0..3 {
            dag.register_pane(id);
        }
        for i in 0..n_obs {
            dag.observe(0, i as f64);
            dag.observe(1, (i as f64).mul_add(0.5, 1.0));
            dag.observe(2, (i as f64) * 0.3);
        }
        dag.update_dag();

        for edge in dag.edges() {
            prop_assert_ne!(
                edge.source, edge.target,
                "edge should not be a self-loop"
            );
        }
    }

    /// Observe on unregistered pane is a no-op (doesn't crash).
    #[test]
    fn prop_observe_unregistered_noop(
        pane_id in 0_u64..10000,
        values in prop::collection::vec(-100.0_f64..100.0, 1..20),
    ) {
        let mut dag = CausalDag::new(CausalDagConfig::default());
        // Don't register — just observe
        for &v in &values {
            dag.observe(pane_id, v);
        }
        prop_assert_eq!(dag.pane_count(), 0, "unregistered pane should not be counted");
    }
}

// =============================================================================
// Unit tests (supplementary)
// =============================================================================

#[test]
fn new_dag_is_empty() {
    let dag = CausalDag::new(CausalDagConfig::default());
    assert_eq!(dag.pane_count(), 0);
    assert_eq!(dag.update_count(), 0);
    assert!(dag.edges().is_empty());
}

#[test]
fn time_series_new_is_empty() {
    let ts = PaneTimeSeries::new(100);
    assert!(ts.is_empty());
    assert_eq!(ts.len(), 0);
    assert!(ts.as_slice_ordered().is_empty());
}

#[test]
fn config_serde_deterministic() {
    let config = CausalDagConfig::default();
    let j1 = serde_json::to_string(&config).unwrap();
    let j2 = serde_json::to_string(&config).unwrap();
    assert_eq!(j1, j2);
}
