//! Property tests for [`storage_workload_advisor`] (ft-1650n.15).
//!
//! Pins the classifier's documented thresholds + truth table over
//! arbitrary `WorkloadProfile`s. Complements the unit tests in
//! `storage_workload_advisor.rs::tests` (31 cases including
//! AdvisorMetrics) and the e2e replay harness at
//! `tests/storage_workload_advisor_replay.rs` (7 fixtures).
//!
//! Properties pinned here:
//!
//! 1. **classify is pure** — same input always produces the same
//!    `AdvisorReport`.
//! 2. **Sparse-sample gate** — `total_ops < 1_000` always returns
//!    `DataNeeded` (the documented `MIN_SAMPLE_OPS` threshold).
//! 3. **Above-threshold returns Recommendation** — `total_ops >=
//!    1_000` always returns `Recommendation` (never DataNeeded).
//! 4. **No-search-backend → IndexChoice::NoChange** — when both
//!    `fts_enabled` and `tantivy_enabled` are false, the index
//!    choice is always `NoChange`.
//! 5. **SearchHeavy + Hybrid** — when both backends are
//!    registered AND the workload mix is SearchHeavy, the
//!    recommendation is `IndexChoice::Hybrid`.
//! 6. **Critical checkpoint lag → High priority** — when
//!    `checkpoint_lag_bytes > 64 MiB` AND the sample is above
//!    threshold, `migration_priority == High`.
//! 7. **AdvisorMetrics counter conservation** — after recording
//!    N reports, `total_recommendations + total_data_needed ==
//!    N`.
//! 8. **AdvisorMetrics::recommend_and_record returns same shape
//!    as advise_from_event_bus_metrics** — the convenience
//!    method is pure-equivalent to the bare classifier path.
//! 9. **last_priority round-trip** — runtime
//!    `AdvisorMetrics::last_priority` matches snapshot
//!    `last_priority`.

use std::sync::Once;

use frankenterm_core::events::MetricsSnapshot;
use frankenterm_core::storage_cardinality_sketch::StorageDistinctSketchSnapshot;
use frankenterm_core::storage_workload_advisor::{
    AdvisorMetrics, AdvisorReport, BackendChoice, HotTableSnapshot, IndexChoice, MigrationPriority,
    SearchBackendsInUse, TailLatencySnapshot, WorkloadMix, WorkloadProfile, advise_from_event_bus_metrics,
    classify,
};
use proptest::prelude::*;

/// Mirror of the substrate's private `MIN_SAMPLE_OPS` constant.
/// Documented in the substrate at
/// `storage_workload_advisor.rs::MIN_SAMPLE_OPS`.
const MIN_SAMPLE_OPS: u64 = 1_000;

/// Mirror of the substrate's checkpoint-lag cliff (64 MiB).
const CHECKPOINT_LAG_CLIFF: u64 = 64 * 1024 * 1024;

fn init_test_tracing_json() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_target(true)
            .with_test_writer()
            .try_init();
    });
}

fn arb_workload_profile() -> impl Strategy<Value = WorkloadProfile> {
    (
        0u64..=10_000, // total_writes
        0u64..=10_000, // total_reads
        0u64..=10_000, // total_searches
        any::<bool>(), // fts_enabled
        any::<bool>(), // tantivy_enabled
        0u64..=1_000,  // distinct_panes
        0u64..=100,    // distinct_sessions
        0u64..=200_000, // p99_write_us
        0u64..=200_000, // p99_read_us
        0u64..=200_000_000, // checkpoint_lag_bytes (up to 200 MiB)
    )
        .prop_map(
            |(
                writes,
                reads,
                searches,
                fts,
                tantivy,
                panes,
                sessions,
                p99_w,
                p99_r,
                lag,
            )| WorkloadProfile {
                total_writes: writes,
                total_reads: reads,
                total_searches: searches,
                fts_enabled: fts,
                tantivy_enabled: tantivy,
                estimated_distinct_panes: panes,
                estimated_distinct_sessions: sessions,
                hot_table: None,
                p99_write_latency_us: p99_w,
                p99_read_latency_us: p99_r,
                checkpoint_lag_bytes: lag,
            },
        )
}

fn arb_metrics_snapshot() -> impl Strategy<Value = MetricsSnapshot> {
    (0u64..=20_000, 0u64..=20_000).prop_map(|(published, delivered)| MetricsSnapshot {
        events_published: published,
        events_dropped_no_subscribers: 0,
        events_dropped_dedup: 0,
        events_delivered: delivered,
        active_subscribers: 1,
        subscriber_lag_events: 0,
        bus_lock_poisoned_count: 0,
        delta_dedup_full_count: 0,
    })
}

fn baseline_cardinality() -> StorageDistinctSketchSnapshot {
    StorageDistinctSketchSnapshot {
        estimated_distinct_panes: 32,
        estimated_distinct_sessions: 8,
        estimated_distinct_embedders: 1,
        standard_error: 0.0081,
        memory_bytes: 49_152,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// **classify is pure**: same input → same output.
    #[test]
    fn classify_is_pure(profile in arb_workload_profile()) {
        init_test_tracing_json();
        let r1 = classify(&profile);
        let r2 = classify(&profile);
        prop_assert_eq!(r1, r2);
    }

    /// **Sparse-sample gate**: total_ops < MIN_SAMPLE_OPS →
    /// DataNeeded. Uses a sparse-only generator (each field
    /// capped at 300 → max total < 1_000) so proptest doesn't
    /// reject every case.
    #[test]
    fn sparse_sample_returns_data_needed(
        writes in 0u64..=300,
        reads in 0u64..=300,
        searches in 0u64..=300,
        fts in any::<bool>(),
        tantivy in any::<bool>(),
    ) {
        init_test_tracing_json();
        let profile = WorkloadProfile {
            total_writes: writes,
            total_reads: reads,
            total_searches: searches,
            fts_enabled: fts,
            tantivy_enabled: tantivy,
            ..WorkloadProfile::default()
        };
        let report = classify(&profile);
        let is_data_needed = matches!(report, AdvisorReport::DataNeeded { .. });
        prop_assert!(is_data_needed, "sparse profile should DataNeeded");
    }

    /// **Above-threshold returns Recommendation**: total_ops ≥
    /// MIN_SAMPLE_OPS → Recommendation (never DataNeeded).
    #[test]
    fn above_threshold_returns_recommendation(
        profile in arb_workload_profile(),
    ) {
        init_test_tracing_json();
        let total = profile.total_writes
            .saturating_add(profile.total_reads)
            .saturating_add(profile.total_searches);
        prop_assume!(total >= MIN_SAMPLE_OPS);
        let report = classify(&profile);
        let is_recommendation = matches!(report, AdvisorReport::Recommendation(_));
        prop_assert!(is_recommendation, "above-threshold should Recommend");
    }

    /// **No-search-backend → NoChange index**: when both fts and
    /// tantivy are disabled, IndexChoice is NoChange (regardless
    /// of mix).
    #[test]
    fn no_search_backend_yields_no_change_index(
        profile in arb_workload_profile(),
    ) {
        init_test_tracing_json();
        let total = profile.total_writes
            .saturating_add(profile.total_reads)
            .saturating_add(profile.total_searches);
        prop_assume!(total >= MIN_SAMPLE_OPS);
        let mut p = profile;
        p.fts_enabled = false;
        p.tantivy_enabled = false;
        match classify(&p) {
            AdvisorReport::Recommendation(rec) => {
                prop_assert_eq!(rec.index, IndexChoice::NoChange);
            }
            other => prop_assert!(false, "expected Recommendation, got {:?}", other),
        }
    }

    /// **SearchHeavy + both backends → Hybrid**: when the mix is
    /// SearchHeavy AND both fts and tantivy are registered, the
    /// recommendation is `IndexChoice::Hybrid`.
    #[test]
    fn search_heavy_with_both_backends_is_hybrid(
        profile in arb_workload_profile(),
    ) {
        init_test_tracing_json();
        let total = profile.total_writes
            .saturating_add(profile.total_reads)
            .saturating_add(profile.total_searches);
        prop_assume!(total >= MIN_SAMPLE_OPS);
        prop_assume!(profile.mix() == WorkloadMix::SearchHeavy);
        let mut p = profile;
        p.fts_enabled = true;
        p.tantivy_enabled = true;
        match classify(&p) {
            AdvisorReport::Recommendation(rec) => {
                prop_assert_eq!(rec.index, IndexChoice::Hybrid);
            }
            other => prop_assert!(false, "expected Recommendation, got {:?}", other),
        }
    }

    /// **Backend choice is always Rusqlite (substrate's
    /// conservative default until ft-kcdqp lands)**.
    #[test]
    fn backend_choice_is_rusqlite_for_now(
        profile in arb_workload_profile(),
    ) {
        init_test_tracing_json();
        let total = profile.total_writes
            .saturating_add(profile.total_reads)
            .saturating_add(profile.total_searches);
        prop_assume!(total >= MIN_SAMPLE_OPS);
        if let AdvisorReport::Recommendation(rec) = classify(&profile) {
            prop_assert_eq!(rec.backend, BackendChoice::Rusqlite);
        }
    }

    /// **Critical checkpoint lag → High priority**: when
    /// `checkpoint_lag_bytes > 64 MiB` AND the sample is above
    /// threshold, `migration_priority == High`.
    #[test]
    fn critical_checkpoint_lag_yields_high_priority(
        profile in arb_workload_profile(),
    ) {
        init_test_tracing_json();
        let total = profile.total_writes
            .saturating_add(profile.total_reads)
            .saturating_add(profile.total_searches);
        prop_assume!(total >= MIN_SAMPLE_OPS);
        let mut p = profile;
        p.checkpoint_lag_bytes = CHECKPOINT_LAG_CLIFF + 1;
        match classify(&p) {
            AdvisorReport::Recommendation(rec) => {
                prop_assert_eq!(rec.migration_priority, MigrationPriority::High);
            }
            other => prop_assert!(false, "expected Recommendation, got {:?}", other),
        }
    }

    /// **AdvisorMetrics counter conservation**: after recording
    /// N reports, `total_recommendations + total_data_needed ==
    /// N`.
    #[test]
    fn advisor_metrics_counter_conservation(
        reports in prop::collection::vec(arb_workload_profile(), 1..=15),
    ) {
        init_test_tracing_json();
        let metrics = AdvisorMetrics::new();
        for profile in &reports {
            let report = classify(profile);
            metrics.record_report(&report);
        }
        let snap = metrics.snapshot();
        prop_assert_eq!(
            snap.total_recommendations + snap.total_data_needed,
            reports.len() as u64
        );
    }

    /// **recommend_and_record returns the same report as
    /// advise_from_event_bus_metrics**: the convenience method
    /// is pure-equivalent to the bare classifier path.
    #[test]
    fn recommend_and_record_matches_bare_advise(
        m in arb_metrics_snapshot(),
        total_searches in 0u64..=20_000,
        fts in any::<bool>(),
        tantivy in any::<bool>(),
        tail_w in 0u64..=200_000,
        tail_r in 0u64..=200_000,
        lag in 0u64..=200_000_000,
    ) {
        init_test_tracing_json();
        let cardinality = baseline_cardinality();
        let backends = SearchBackendsInUse {
            fts5: fts,
            tantivy,
        };
        let tail = TailLatencySnapshot::new(tail_w, tail_r);
        let hot: Option<HotTableSnapshot> = None;

        let bare = advise_from_event_bus_metrics(
            &m,
            &cardinality,
            total_searches,
            backends,
            hot.clone(),
            tail,
            lag,
        );
        let metrics = AdvisorMetrics::new();
        let convenient = metrics.recommend_and_record(
            &m,
            &cardinality,
            total_searches,
            backends,
            hot,
            tail,
            lag,
        );
        prop_assert_eq!(bare, convenient);
    }

    /// **AdvisorMetricsSnapshot::last_priority round-trip**: the
    /// snapshot's `last_priority` matches the runtime metric's
    /// `last_priority`.
    #[test]
    fn last_priority_round_trips_through_snapshot(
        profiles in prop::collection::vec(arb_workload_profile(), 1..=10),
    ) {
        init_test_tracing_json();
        let metrics = AdvisorMetrics::new();
        for p in &profiles {
            let r = classify(p);
            metrics.record_report(&r);
        }
        prop_assert_eq!(metrics.last_priority(), metrics.snapshot().last_priority());
    }

    /// **Determinism end-to-end**: the live-wire
    /// `advise_from_event_bus_metrics` is byte-deterministic for
    /// identical inputs (the substrate's documented purity
    /// contract pinned at the public-API boundary).
    #[test]
    fn advise_from_event_bus_metrics_is_deterministic(
        m in arb_metrics_snapshot(),
        total_searches in 0u64..=20_000,
        fts in any::<bool>(),
        tantivy in any::<bool>(),
        tail_w in 0u64..=200_000,
        tail_r in 0u64..=200_000,
        lag in 0u64..=200_000_000,
    ) {
        init_test_tracing_json();
        let cardinality = baseline_cardinality();
        let backends = SearchBackendsInUse {
            fts5: fts,
            tantivy,
        };
        let tail = TailLatencySnapshot::new(tail_w, tail_r);
        let r1 = advise_from_event_bus_metrics(
            &m,
            &cardinality,
            total_searches,
            backends,
            None,
            tail,
            lag,
        );
        let r2 = advise_from_event_bus_metrics(
            &m,
            &cardinality,
            total_searches,
            backends,
            None,
            tail,
            lag,
        );
        prop_assert_eq!(&r1, &r2);
        let j1 = serde_json::to_string(&r1).expect("serialize r1");
        let j2 = serde_json::to_string(&r2).expect("serialize r2");
        prop_assert_eq!(j1, j2);
    }
}
