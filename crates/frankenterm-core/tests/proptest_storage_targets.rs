//! Property-based tests for the `storage_targets` module.
//!
//! Verifies health-tier classification invariants (monotonicity, boundary
//! correctness, lattice properties), serde roundtrips, and composite
//! assessment consistency.

use std::time::Duration;

use frankenterm_core::storage_targets::{
    FtsConsistencyThresholds, HealthTier, IndexingLagThresholds, LatencyBudgets, ScaleTargets,
    StorageHealthSnapshot, StorageHealthThresholds, StorageMetrics, StoragePerfProfile,
    ThroughputBudgets, WalThresholds, WriterQueueThresholds,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_health_tier() -> impl Strategy<Value = HealthTier> {
    prop_oneof![
        Just(HealthTier::Green),
        Just(HealthTier::Yellow),
        Just(HealthTier::Red),
    ]
}

fn arb_writer_queue_thresholds() -> impl Strategy<Value = WriterQueueThresholds> {
    // yellow_ratio < red_ratio, both in (0, 1)
    (0.01f64..0.99f64).prop_flat_map(|yellow| {
        (yellow + 0.01..1.0f64).prop_map(move |red| WriterQueueThresholds {
            yellow_ratio: yellow,
            red_ratio: red,
        })
    })
}

fn arb_wal_thresholds() -> impl Strategy<Value = WalThresholds> {
    // yellow_frames < red_frames
    (1u64..100_000u64).prop_flat_map(|yellow| {
        (yellow + 1..200_000u64).prop_map(move |red| WalThresholds {
            yellow_frames: yellow,
            red_frames: red,
        })
    })
}

fn arb_fts_consistency_thresholds() -> impl Strategy<Value = FtsConsistencyThresholds> {
    // red_ratio < yellow_ratio, both in (0, 1)
    (0.01f64..0.99f64).prop_flat_map(|red| {
        (red + 0.01..1.0f64).prop_map(move |yellow| FtsConsistencyThresholds {
            yellow_ratio: yellow,
            red_ratio: red,
        })
    })
}

fn arb_indexing_lag_thresholds() -> impl Strategy<Value = IndexingLagThresholds> {
    // yellow < red
    (1u64..5000u64).prop_flat_map(|yellow_ms| {
        (yellow_ms + 1..10_000u64).prop_map(move |red_ms| IndexingLagThresholds {
            yellow: Duration::from_millis(yellow_ms),
            red: Duration::from_millis(red_ms),
        })
    })
}

fn arb_storage_metrics() -> impl Strategy<Value = StorageMetrics> {
    (
        0usize..10_000,  // writer_queue_depth
        1usize..10_000,  // writer_queue_capacity (nonzero)
        0u64..100_000,   // wal_frames
        0.0f64..1.5,     // fts_consistency_ratio
        0u64..10_000u64, // indexing_lag_ms
    )
        .prop_map(
            |(depth, capacity, wal_frames, fts_ratio, lag_ms)| StorageMetrics {
                writer_queue_depth: depth,
                writer_queue_capacity: capacity,
                wal_frames,
                fts_consistency_ratio: fts_ratio,
                indexing_lag: Duration::from_millis(lag_ms),
            },
        )
}

// =========================================================================
// HealthTier properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_health_tier_serde_roundtrip(tier in arb_health_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let parsed: HealthTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tier, parsed);
    }
}

#[test]
fn health_tier_display_is_lowercase() {
    for tier in [HealthTier::Green, HealthTier::Yellow, HealthTier::Red] {
        let display = tier.to_string();
        let lower = display.to_lowercase();
        assert_eq!(display, lower, "HealthTier::Display should be lowercase");
        assert!(!display.is_empty());
    }
}

#[test]
fn health_tier_display_is_distinct() {
    let displays: Vec<String> = [HealthTier::Green, HealthTier::Yellow, HealthTier::Red]
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert_ne!(displays[0], displays[1]);
    assert_ne!(displays[1], displays[2]);
    assert_ne!(displays[0], displays[2]);
}

// =========================================================================
// WriterQueueThresholds classification
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Monotonicity: higher depth with same capacity → same or worse tier.
    #[test]
    fn prop_writer_queue_monotone_depth(
        th in arb_writer_queue_thresholds(),
        d1 in 0usize..5000,
        delta in 0usize..5000,
        cap in 1usize..10000,
    ) {
        let d2 = d1.saturating_add(delta);
        let t1 = th.classify(d1, cap);
        let t2 = th.classify(d2, cap);
        // tier ordering: Green < Yellow < Red
        let ord = |t: HealthTier| match t {
            HealthTier::Green => 0,
            HealthTier::Yellow => 1,
            HealthTier::Red => 2,
        };
        prop_assert!(ord(t2) >= ord(t1),
            "depth {} → {:?}, depth {} → {:?} (cap={})",
            d1, t1, d2, t2, cap);
    }

    /// Zero capacity is always Red regardless of depth.
    #[test]
    fn prop_writer_queue_zero_capacity_red(depth in 0usize..10000) {
        let th = WriterQueueThresholds::default();
        prop_assert_eq!(th.classify(depth, 0), HealthTier::Red);
    }

    /// Zero depth with nonzero capacity is always Green.
    #[test]
    fn prop_writer_queue_zero_depth_green(
        th in arb_writer_queue_thresholds(),
        cap in 1usize..10000,
    ) {
        prop_assert_eq!(th.classify(0, cap), HealthTier::Green);
    }

    /// Full capacity is always Red.
    #[test]
    fn prop_writer_queue_full_is_red(
        th in arb_writer_queue_thresholds(),
        cap in 1usize..10000,
    ) {
        prop_assert_eq!(th.classify(cap, cap), HealthTier::Red);
    }
}

// =========================================================================
// WalThresholds classification
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Monotonicity: more frames → same or worse tier.
    #[test]
    fn prop_wal_monotone(
        th in arb_wal_thresholds(),
        f1 in 0u64..50_000,
        delta in 0u64..50_000,
    ) {
        let f2 = f1.saturating_add(delta);
        let t1 = th.classify(f1);
        let t2 = th.classify(f2);
        let ord = |t: HealthTier| match t {
            HealthTier::Green => 0,
            HealthTier::Yellow => 1,
            HealthTier::Red => 2,
        };
        prop_assert!(ord(t2) >= ord(t1),
            "frames {} → {:?}, frames {} → {:?}", f1, t1, f2, t2);
    }

    /// Zero frames is always Green.
    #[test]
    fn prop_wal_zero_green(th in arb_wal_thresholds()) {
        prop_assert_eq!(th.classify(0), HealthTier::Green);
    }

    /// At red threshold, tier is Red.
    #[test]
    fn prop_wal_at_red_is_red(th in arb_wal_thresholds()) {
        prop_assert_eq!(th.classify(th.red_frames), HealthTier::Red);
    }

    /// At yellow threshold, tier is Yellow.
    #[test]
    fn prop_wal_at_yellow_is_yellow(th in arb_wal_thresholds()) {
        prop_assert_eq!(th.classify(th.yellow_frames), HealthTier::Yellow);
    }
}

// =========================================================================
// FtsConsistencyThresholds classification
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Monotonicity: lower ratio → same or worse tier (inverted).
    #[test]
    fn prop_fts_monotone(
        th in arb_fts_consistency_thresholds(),
        r1 in 0.0f64..1.5,
        delta in 0.0f64..1.0,
    ) {
        let r2 = (r1 - delta).max(0.0);
        // r2 <= r1, so r2 should be same or worse
        let t1 = th.classify(r1);
        let t2 = th.classify(r2);
        let ord = |t: HealthTier| match t {
            HealthTier::Green => 0,
            HealthTier::Yellow => 1,
            HealthTier::Red => 2,
        };
        prop_assert!(ord(t2) >= ord(t1),
            "ratio {} → {:?}, ratio {} → {:?}", r1, t1, r2, t2);
    }

    /// Perfect consistency (1.0) is always Green when thresholds are valid.
    #[test]
    fn prop_fts_perfect_green(th in arb_fts_consistency_thresholds()) {
        prop_assert_eq!(th.classify(1.0), HealthTier::Green);
    }

    /// Zero ratio is always Red.
    #[test]
    fn prop_fts_zero_red(th in arb_fts_consistency_thresholds()) {
        prop_assert_eq!(th.classify(0.0), HealthTier::Red);
    }
}

// =========================================================================
// IndexingLagThresholds classification
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Monotonicity: longer lag → same or worse tier.
    #[test]
    fn prop_lag_monotone(
        th in arb_indexing_lag_thresholds(),
        ms1 in 0u64..10_000,
        delta in 0u64..10_000,
    ) {
        let ms2 = ms1.saturating_add(delta);
        let t1 = th.classify(Duration::from_millis(ms1));
        let t2 = th.classify(Duration::from_millis(ms2));
        let ord = |t: HealthTier| match t {
            HealthTier::Green => 0,
            HealthTier::Yellow => 1,
            HealthTier::Red => 2,
        };
        prop_assert!(ord(t2) >= ord(t1),
            "lag {}ms → {:?}, lag {}ms → {:?}", ms1, t1, ms2, t2);
    }

    /// Zero lag is always Green.
    #[test]
    fn prop_lag_zero_green(th in arb_indexing_lag_thresholds()) {
        prop_assert_eq!(th.classify(Duration::ZERO), HealthTier::Green);
    }

    /// At red threshold, tier is Red.
    #[test]
    fn prop_lag_at_red_is_red(th in arb_indexing_lag_thresholds()) {
        prop_assert_eq!(th.classify(th.red), HealthTier::Red);
    }

    /// At yellow threshold, tier is Yellow.
    #[test]
    fn prop_lag_at_yellow_is_yellow(th in arb_indexing_lag_thresholds()) {
        prop_assert_eq!(th.classify(th.yellow), HealthTier::Yellow);
    }
}

// =========================================================================
// StorageHealthSnapshot composite assessment
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// overall tier == worst individual tier.
    #[test]
    fn prop_overall_is_worst(
        metrics in arb_storage_metrics(),
    ) {
        let thresholds = StorageHealthThresholds::default();
        let snap = StorageHealthSnapshot::assess(&metrics, &thresholds);
        let ord = |t: HealthTier| match t {
            HealthTier::Green => 0,
            HealthTier::Yellow => 1,
            HealthTier::Red => 2,
        };
        let expected_worst = [
            snap.writer_queue,
            snap.wal,
            snap.fts_consistency,
            snap.indexing_lag,
        ]
        .iter()
        .map(|t| ord(*t))
        .max()
        .unwrap_or(0);
        prop_assert_eq!(ord(snap.overall), expected_worst,
            "overall {:?} doesn't match worst of {:?}/{:?}/{:?}/{:?}",
            snap.overall, snap.writer_queue, snap.wal, snap.fts_consistency, snap.indexing_lag);
    }

    /// All Green inputs → Green overall.
    #[test]
    fn prop_all_green_means_green_overall(cap in 1usize..10000) {
        let metrics = StorageMetrics {
            writer_queue_depth: 0,
            writer_queue_capacity: cap,
            wal_frames: 0,
            fts_consistency_ratio: 1.0,
            indexing_lag: Duration::ZERO,
        };
        let thresholds = StorageHealthThresholds::default();
        let snap = StorageHealthSnapshot::assess(&metrics, &thresholds);
        prop_assert_eq!(snap.overall, HealthTier::Green);
        prop_assert_eq!(snap.writer_queue, HealthTier::Green);
        prop_assert_eq!(snap.wal, HealthTier::Green);
        prop_assert_eq!(snap.fts_consistency, HealthTier::Green);
        prop_assert_eq!(snap.indexing_lag, HealthTier::Green);
    }

    /// Assess is deterministic: same inputs → same result.
    #[test]
    fn prop_assess_deterministic(metrics in arb_storage_metrics()) {
        let thresholds = StorageHealthThresholds::default();
        let snap1 = StorageHealthSnapshot::assess(&metrics, &thresholds);
        let snap2 = StorageHealthSnapshot::assess(&metrics, &thresholds);
        prop_assert_eq!(snap1.writer_queue, snap2.writer_queue);
        prop_assert_eq!(snap1.wal, snap2.wal);
        prop_assert_eq!(snap1.fts_consistency, snap2.fts_consistency);
        prop_assert_eq!(snap1.indexing_lag, snap2.indexing_lag);
        prop_assert_eq!(snap1.overall, snap2.overall);
    }
}

// =========================================================================
// Serde roundtrips
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_scale_targets_serde_roundtrip(
        panes in 1usize..1000,
        bytes in 0u64..10_000_000_000,
        rate in 0u64..100_000_000,
        segments in 0u64..10_000_000,
        db_size in 0u64..20_000_000_000u64,
    ) {
        let t = ScaleTargets {
            min_concurrent_panes: panes,
            min_transcript_bytes: bytes,
            min_ingest_bytes_per_sec: rate,
            min_segments_before_degradation: segments,
            max_db_size_bytes: db_size,
        };
        let json = serde_json::to_string(&t).unwrap();
        let parsed: ScaleTargets = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(t, parsed);
    }

    #[test]
    fn prop_wal_thresholds_serde_roundtrip(th in arb_wal_thresholds()) {
        let json = serde_json::to_string(&th).unwrap();
        let parsed: WalThresholds = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(th, parsed);
    }

    #[test]
    fn prop_indexing_lag_thresholds_serde_roundtrip(th in arb_indexing_lag_thresholds()) {
        let json = serde_json::to_string(&th).unwrap();
        let parsed: IndexingLagThresholds = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(th, parsed);
    }

    #[test]
    fn prop_throughput_budgets_serde_roundtrip(
        seg_per_sec in 0u64..100_000,
        batch_cap in 1usize..1000,
        sync_batch in 1usize..1000,
        sync_bytes in 1usize..10_000_000,
    ) {
        let t = ThroughputBudgets {
            min_segments_per_sec: seg_per_sec,
            writer_batch_cap: batch_cap,
            fts_sync_batch_size: sync_batch,
            fts_sync_max_batch_bytes: sync_bytes,
        };
        let json = serde_json::to_string(&t).unwrap();
        let parsed: ThroughputBudgets = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(t, parsed);
    }
}

// =========================================================================
// Performance profile ordering invariants
// =========================================================================

#[test]
fn constrained_profile_latencies_more_relaxed_than_default() {
    let def = StoragePerfProfile::default().latency;
    let con = StoragePerfProfile::constrained().latency;
    assert!(con.append_segment_p95 >= def.append_segment_p95);
    assert!(con.fts_query_common_p95 >= def.fts_query_common_p95);
    assert!(con.fts_query_complex_p95 >= def.fts_query_complex_p95);
    assert!(con.indexing_lag_ceiling >= def.indexing_lag_ceiling);
}

#[test]
fn high_perf_profile_latencies_tighter_than_default() {
    let def = StoragePerfProfile::default().latency;
    let hp = StoragePerfProfile::high_performance().latency;
    assert!(hp.append_segment_p95 <= def.append_segment_p95);
    assert!(hp.fts_query_common_p95 <= def.fts_query_common_p95);
    assert!(hp.fts_query_complex_p95 <= def.fts_query_complex_p95);
    assert!(hp.indexing_lag_ceiling <= def.indexing_lag_ceiling);
}

#[test]
fn profile_ordering_constrained_default_highperf() {
    let con = StoragePerfProfile::constrained();
    let def = StoragePerfProfile::default();
    let hp = StoragePerfProfile::high_performance();

    // Scale: con < def < hp
    assert!(con.scale.min_concurrent_panes < def.scale.min_concurrent_panes);
    assert!(def.scale.min_concurrent_panes < hp.scale.min_concurrent_panes);

    // Throughput: con < def < hp
    assert!(con.throughput.min_segments_per_sec < def.throughput.min_segments_per_sec);
    assert!(def.throughput.min_segments_per_sec < hp.throughput.min_segments_per_sec);
}

// =========================================================================
// Latency budgets consistency
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn prop_latency_budgets_serde_roundtrip(
        append_ms in 1u64..100,
        batch_ms in 10u64..500,
        batch_size in 1usize..512,
        fts_common_ms in 1u64..200,
        fts_complex_ms in 10u64..500,
        pane_ms in 1u64..50,
        lag_ms in 100u64..5000,
        ckpt_ms in 10u64..500,
    ) {
        let b = LatencyBudgets {
            append_segment_p95: Duration::from_millis(append_ms),
            batch_append_p95: Duration::from_millis(batch_ms),
            batch_size,
            fts_query_common_p95: Duration::from_millis(fts_common_ms),
            fts_query_complex_p95: Duration::from_millis(fts_complex_ms),
            pane_upsert_p95: Duration::from_millis(pane_ms),
            indexing_lag_ceiling: Duration::from_millis(lag_ms),
            checkpoint_passive_p95: Duration::from_millis(ckpt_ms),
        };
        let json = serde_json::to_string(&b).unwrap();
        let parsed: LatencyBudgets = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(b, parsed);
    }
}
