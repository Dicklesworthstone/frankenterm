//! br-ft-1650n.15 e2e replay harness: fixture-driven integration tests
//! for [`storage_workload_advisor::advise_from_event_bus_metrics`].
//!
//! Closes the bead's "Replay/e2e with synthetic storage profiles for
//! write-heavy, search-heavy, and balanced workloads" acceptance
//! criterion, plus the "Detailed logging for every recommendation
//! input" requirement (each replay step emits a structured
//! tracing-json record so failures land a parseable trace).
//!
//! ## What the harness does
//!
//! Each fixture is a *trajectory*: a sequence of `MetricsSnapshot`s
//! representing how the EventBus counters evolve over a sampling
//! window. The harness:
//!
//! 1. Walks the trajectory step-by-step.
//! 2. At each step calls `advise_from_event_bus_metrics` with the
//!    corresponding cardinality / search-backend / latency snapshot.
//! 3. Emits a structured `tracing::info!` event with the step index,
//!    the input metric snapshot, and the advisor's verdict.
//! 4. Asserts the final verdict matches the fixture's expected
//!    `AdvisorReport` shape.
//!
//! ## Fixtures
//!
//! - `write_heavy_trajectory`: counters skew to writes; expect
//!   `WriteHeavy` mix once the sample crosses `MIN_SAMPLE_OPS`.
//! - `search_heavy_trajectory`: counters skew to searches; expect
//!   `IndexChoice::Hybrid` when both FTS5 and Tantivy are
//!   registered, `Tantivy` when only tantivy is registered.
//! - `balanced_trajectory`: roughly equal mix; expect
//!   `WorkloadMix::Balanced` and a confidence band that scales
//!   with sample size.
//! - `stability_replay`: re-feeding the same fixture twice produces
//!   byte-identical `AdvisorReport`s (the advisor is pure).
//! - `cliff_priority_trajectory`: checkpoint lag growth across the
//!   trajectory eventually pushes `MigrationPriority::High`.

use std::sync::Once;

use frankenterm_core::events::MetricsSnapshot;
use frankenterm_core::storage_cardinality_sketch::StorageDistinctSketchSnapshot;
use frankenterm_core::storage_workload_advisor::{
    AdvisorReport, BackendChoice, Confidence, HotTableSnapshot, IndexChoice, MigrationPriority,
    SearchBackendsInUse, TailLatencySnapshot, advise_from_event_bus_metrics,
};
use tracing::info;

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

/// One step in a replay trajectory. Holds the EventBus counter
/// snapshot at the step plus the cardinality / search-side state
/// the advisor still needs (the EventBus has no signal for those).
#[derive(Debug, Clone)]
struct ReplayStep {
    metrics: MetricsSnapshot,
    cardinality: StorageDistinctSketchSnapshot,
    total_searches: u64,
    search_backends: SearchBackendsInUse,
    hot_table: Option<HotTableSnapshot>,
    tail_latency: TailLatencySnapshot,
    checkpoint_lag_bytes: u64,
}

impl ReplayStep {
    fn run(&self) -> AdvisorReport {
        advise_from_event_bus_metrics(
            &self.metrics,
            &self.cardinality,
            self.total_searches,
            self.search_backends,
            self.hot_table.clone(),
            self.tail_latency,
            self.checkpoint_lag_bytes,
        )
    }
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

fn metrics(events_published: u64, events_delivered: u64) -> MetricsSnapshot {
    MetricsSnapshot {
        events_published,
        events_dropped_no_subscribers: 0,
        events_dropped_dedup: 0,
        events_delivered,
        active_subscribers: 4,
        subscriber_lag_events: 0,
        bus_lock_poisoned_count: 0,
        delta_dedup_full_count: 0,
    }
}

/// Walk a trajectory, log each step, and return the final
/// `AdvisorReport`. Each step is logged as a structured
/// tracing-json event so a failing case lands a parseable trace.
fn replay(fixture_name: &'static str, trajectory: &[ReplayStep]) -> AdvisorReport {
    init_test_tracing_json();
    let mut last = None;
    for (idx, step) in trajectory.iter().enumerate() {
        let report = step.run();
        info!(
            fixture = fixture_name,
            step = idx,
            events_published = step.metrics.events_published,
            events_delivered = step.metrics.events_delivered,
            total_searches = step.total_searches,
            fts5 = step.search_backends.fts5,
            tantivy = step.search_backends.tantivy,
            checkpoint_lag_bytes = step.checkpoint_lag_bytes,
            p99_write_us = step.tail_latency.p99_write_us,
            p99_read_us = step.tail_latency.p99_read_us,
            distinct_panes = step.cardinality.estimated_distinct_panes,
            verdict_kind = report_kind_label(&report),
            verdict_index = report_index_label(&report),
            verdict_priority = report_priority_label(&report),
            verdict_confidence = report_confidence_label(&report),
            "advisor replay step"
        );
        last = Some(report);
    }
    last.expect("trajectory must contain at least one step")
}

fn report_kind_label(report: &AdvisorReport) -> &'static str {
    match report {
        AdvisorReport::Recommendation(_) => "recommendation",
        AdvisorReport::DataNeeded { .. } => "data_needed",
    }
}

fn report_index_label(report: &AdvisorReport) -> &'static str {
    match report {
        AdvisorReport::Recommendation(rec) => match rec.index {
            IndexChoice::Fts5 => "fts5",
            IndexChoice::Tantivy => "tantivy",
            IndexChoice::Hybrid => "hybrid",
            IndexChoice::NoChange => "no_change",
        },
        AdvisorReport::DataNeeded { .. } => "n/a",
    }
}

fn report_priority_label(report: &AdvisorReport) -> &'static str {
    match report {
        AdvisorReport::Recommendation(rec) => match rec.migration_priority {
            MigrationPriority::None => "none",
            MigrationPriority::Low => "low",
            MigrationPriority::Medium => "medium",
            MigrationPriority::High => "high",
        },
        AdvisorReport::DataNeeded { .. } => "n/a",
    }
}

fn report_confidence_label(report: &AdvisorReport) -> &'static str {
    match report {
        AdvisorReport::Recommendation(rec) => match rec.confidence {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        },
        AdvisorReport::DataNeeded { .. } => "n/a",
    }
}

/// Write-heavy trajectory: counters skew toward `events_published`.
/// The first step is below `MIN_SAMPLE_OPS` (sparse → DataNeeded);
/// later steps cross the gate and the verdict transitions to a
/// concrete recommendation with the WriteHeavy mix reflected in
/// the rationale.
#[test]
fn replay_write_heavy_trajectory() {
    let card = baseline_cardinality();
    let backends = SearchBackendsInUse::fts5_only();

    let trajectory = vec![
        // step 0: tiny sample — DataNeeded.
        ReplayStep {
            metrics: metrics(50, 30),
            cardinality: card.clone(),
            total_searches: 5,
            search_backends: backends,
            hot_table: None,
            tail_latency: TailLatencySnapshot::default(),
            checkpoint_lag_bytes: 0,
        },
        // step 1: above gate, write share dominates.
        ReplayStep {
            metrics: metrics(2_000, 200),
            cardinality: card.clone(),
            total_searches: 100,
            search_backends: backends,
            hot_table: Some(HotTableSnapshot {
                name: "events".to_string(),
                row_count: 500_000,
            }),
            tail_latency: TailLatencySnapshot::new(20_000, 4_000),
            checkpoint_lag_bytes: 1024 * 1024,
        },
        // step 2: sample grows further; still write-heavy.
        ReplayStep {
            metrics: metrics(8_000, 1_000),
            cardinality: card.clone(),
            total_searches: 500,
            search_backends: backends,
            hot_table: Some(HotTableSnapshot {
                name: "events".to_string(),
                row_count: 2_000_000,
            }),
            tail_latency: TailLatencySnapshot::new(35_000, 6_000),
            checkpoint_lag_bytes: 8 * 1024 * 1024,
        },
    ];

    // Step 0 must be DataNeeded.
    let s0 = trajectory[0].run();
    assert!(
        matches!(s0, AdvisorReport::DataNeeded { .. }),
        "step 0 should be DataNeeded; got {s0:?}"
    );

    let final_report = replay("write_heavy_trajectory", &trajectory);
    match final_report {
        AdvisorReport::Recommendation(rec) => {
            assert_eq!(rec.backend, BackendChoice::Rusqlite);
            assert_eq!(rec.index, IndexChoice::Fts5);
            // Rationale string surfaces the WriteHeavy mix label.
            assert!(
                rec.rationale.contains("WriteHeavy"),
                "rationale missing WriteHeavy: {}",
                rec.rationale
            );
        }
        other => panic!("write-heavy final step should recommend; got {other:?}"),
    }
}

/// Search-heavy trajectory with both FTS5 and Tantivy registered:
/// expect `IndexChoice::Hybrid` once the sample crosses the gate.
#[test]
fn replay_search_heavy_trajectory_hybrid() {
    let card = baseline_cardinality();
    let backends = SearchBackendsInUse::both();

    let trajectory = vec![
        ReplayStep {
            metrics: metrics(100, 100),
            cardinality: card.clone(),
            total_searches: 50,
            search_backends: backends,
            hot_table: None,
            tail_latency: TailLatencySnapshot::default(),
            checkpoint_lag_bytes: 0,
        },
        ReplayStep {
            metrics: metrics(200, 200),
            cardinality: card.clone(),
            total_searches: 2_000,
            search_backends: backends,
            hot_table: Some(HotTableSnapshot {
                name: "output_segments".to_string(),
                row_count: 500_000,
            }),
            tail_latency: TailLatencySnapshot::new(30_000, 8_000),
            checkpoint_lag_bytes: 2 * 1024 * 1024,
        },
        ReplayStep {
            metrics: metrics(500, 500),
            cardinality: card.clone(),
            total_searches: 12_000,
            search_backends: backends,
            hot_table: Some(HotTableSnapshot {
                name: "output_segments".to_string(),
                row_count: 5_000_000,
            }),
            tail_latency: TailLatencySnapshot::new(35_000, 9_000),
            checkpoint_lag_bytes: 4 * 1024 * 1024,
        },
    ];

    let final_report = replay("search_heavy_hybrid_trajectory", &trajectory);
    match final_report {
        AdvisorReport::Recommendation(rec) => {
            assert_eq!(rec.index, IndexChoice::Hybrid);
            assert!(
                rec.rationale.contains("SearchHeavy"),
                "rationale missing SearchHeavy: {}",
                rec.rationale
            );
        }
        other => panic!("search-heavy/both should recommend Hybrid; got {other:?}"),
    }
}

/// Search-heavy trajectory with only Tantivy registered: expect
/// `IndexChoice::Tantivy`.
#[test]
fn replay_search_heavy_trajectory_tantivy_only() {
    let card = baseline_cardinality();
    let backends = SearchBackendsInUse::tantivy_only();

    let trajectory = vec![ReplayStep {
        metrics: metrics(300, 300),
        cardinality: card.clone(),
        total_searches: 5_000,
        search_backends: backends,
        hot_table: None,
        tail_latency: TailLatencySnapshot::new(15_000, 4_000),
        checkpoint_lag_bytes: 0,
    }];

    let report = replay("search_heavy_tantivy_only_trajectory", &trajectory);
    match report {
        AdvisorReport::Recommendation(rec) => assert_eq!(rec.index, IndexChoice::Tantivy),
        other => panic!("expected Tantivy recommendation; got {other:?}"),
    }
}

/// Balanced trajectory: equal-share writes/reads/searches. The
/// classifier should not flip to a non-balanced bucket; the index
/// recommendation reflects only which backends are registered.
#[test]
fn replay_balanced_trajectory() {
    let card = baseline_cardinality();
    let backends = SearchBackendsInUse::fts5_only();

    let trajectory = vec![
        ReplayStep {
            metrics: metrics(500, 500),
            cardinality: card.clone(),
            total_searches: 500,
            search_backends: backends,
            hot_table: None,
            tail_latency: TailLatencySnapshot::default(),
            checkpoint_lag_bytes: 0,
        },
        ReplayStep {
            metrics: metrics(2_000, 2_000),
            cardinality: card.clone(),
            total_searches: 2_000,
            search_backends: backends,
            hot_table: Some(HotTableSnapshot {
                name: "events".to_string(),
                row_count: 1_000_000,
            }),
            tail_latency: TailLatencySnapshot::new(15_000, 4_000),
            checkpoint_lag_bytes: 1024 * 1024,
        },
    ];

    let report = replay("balanced_trajectory", &trajectory);
    match report {
        AdvisorReport::Recommendation(rec) => {
            assert_eq!(rec.index, IndexChoice::Fts5);
            assert!(
                rec.rationale.contains("Balanced"),
                "rationale missing Balanced: {}",
                rec.rationale
            );
        }
        other => panic!("balanced trajectory should recommend; got {other:?}"),
    }
}

/// Stability: re-feeding the same trajectory produces a
/// byte-identical final report. The advisor is documented as a
/// pure function; this fixture pins that contract end-to-end.
#[test]
fn replay_stability_idempotent() {
    let card = baseline_cardinality();
    let backends = SearchBackendsInUse::both();

    let trajectory = vec![ReplayStep {
        metrics: metrics(4_000, 3_500),
        cardinality: card.clone(),
        total_searches: 8_000,
        search_backends: backends,
        hot_table: Some(HotTableSnapshot {
            name: "output_segments".to_string(),
            row_count: 5_000_000,
        }),
        tail_latency: TailLatencySnapshot::new(20_000, 5_000),
        checkpoint_lag_bytes: 4 * 1024 * 1024,
    }];

    let first = replay("stability_replay_first", &trajectory);
    let second = replay("stability_replay_second", &trajectory);
    assert_eq!(
        first, second,
        "advise_from_event_bus_metrics must be byte-identical across replays"
    );

    // Serde roundtrip parity: the JSON encoding is also stable.
    let j1 = serde_json::to_string(&first).expect("serialize first");
    let j2 = serde_json::to_string(&second).expect("serialize second");
    assert_eq!(j1, j2, "serde encoding must be stable across replays");
}

/// Cliff priority: checkpoint lag growth across the trajectory
/// eventually pushes `MigrationPriority::High`. The 64 MiB
/// threshold (`storage_workload_advisor::classify`) is the cliff
/// gate.
#[test]
fn replay_cliff_priority_trajectory() {
    let card = baseline_cardinality();
    let backends = SearchBackendsInUse::fts5_only();
    let base_metrics = metrics(3_000, 2_500);
    let hot = Some(HotTableSnapshot {
        name: "events".to_string(),
        row_count: 5_000_000,
    });

    let trajectory = vec![
        ReplayStep {
            metrics: base_metrics.clone(),
            cardinality: card.clone(),
            total_searches: 500,
            search_backends: backends,
            hot_table: hot.clone(),
            tail_latency: TailLatencySnapshot::new(10_000, 3_000),
            checkpoint_lag_bytes: 8 * 1024 * 1024,
        },
        ReplayStep {
            metrics: base_metrics.clone(),
            cardinality: card.clone(),
            total_searches: 500,
            search_backends: backends,
            hot_table: hot.clone(),
            tail_latency: TailLatencySnapshot::new(12_000, 3_500),
            checkpoint_lag_bytes: 32 * 1024 * 1024,
        },
        ReplayStep {
            metrics: base_metrics.clone(),
            cardinality: card.clone(),
            total_searches: 500,
            search_backends: backends,
            hot_table: hot,
            tail_latency: TailLatencySnapshot::new(15_000, 4_000),
            checkpoint_lag_bytes: 128 * 1024 * 1024,
        },
    ];

    // Step 0: lag well under the cliff → priority None.
    match trajectory[0].run() {
        AdvisorReport::Recommendation(rec) => {
            assert_eq!(rec.migration_priority, MigrationPriority::None);
        }
        other => panic!("step 0 should recommend; got {other:?}"),
    }

    // Final step crosses the 64 MiB cliff → High.
    let final_report = replay("cliff_priority_trajectory", &trajectory);
    match final_report {
        AdvisorReport::Recommendation(rec) => {
            assert_eq!(rec.migration_priority, MigrationPriority::High);
        }
        other => panic!("cliff trajectory final should recommend; got {other:?}"),
    }
}

/// No-search-backend fixture: even a large sample produces an
/// `IndexChoice::NoChange` because the advisor has no signal to
/// recommend FTS5 vs Tantivy when neither is registered.
#[test]
fn replay_no_search_backend_trajectory() {
    let card = baseline_cardinality();
    let backends = SearchBackendsInUse::neither();

    let trajectory = vec![ReplayStep {
        metrics: metrics(5_000, 4_500),
        cardinality: card.clone(),
        total_searches: 0,
        search_backends: backends,
        hot_table: None,
        tail_latency: TailLatencySnapshot::default(),
        checkpoint_lag_bytes: 0,
    }];

    match replay("no_search_backend_trajectory", &trajectory) {
        AdvisorReport::Recommendation(rec) => {
            assert_eq!(rec.index, IndexChoice::NoChange);
        }
        other => panic!("no-search-backend should still recommend; got {other:?}"),
    }
}
