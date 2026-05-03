//! br-ft-1650n.13 e2e harness: fixture-driven integration tests
//! for [`pareto_frontier_planner::plan`].
//!
//! Closes the bead's "Bench/e2e script with at least three
//! scenarios and logged machine shape" + "Golden report schema
//! with reproducible input manifest" acceptance criteria.
//!
//! ## What the harness does
//!
//! Each fixture is a *synthetic swarm profile*: a hand-rolled
//! sweep of `MeasurementPoint`s along one resource-knob axis with
//! a documented tradeoff envelope. The harness:
//!
//! 1. Builds the fixture's measurement set deterministically.
//! 2. Logs every measurement as a structured tracing-json event
//!    (input manifest = the input to `plan`, in line with the
//!    bead's "Detailed logging for every recommendation input"
//!    requirement).
//! 3. Runs `plan` and asserts the golden frontier shape — counts,
//!    ordering, and which configs survive vs. get explained.
//! 4. Asserts byte-identical re-plan stability (the planner is a
//!    pure function; this pins the contract end-to-end).
//!
//! ## Fixtures
//!
//! - `concurrency_sweep`: capture concurrency 1, 2, 4, 8. Higher
//!   concurrency cuts p99 latency but raises memory and CPU. The
//!   midrange (2/4) and the extremes (1/8) all stay on the
//!   frontier; nothing dominates.
//! - `compression_sweep`: output compression 0, 1, 2, 3. Higher
//!   compression saves storage write pressure but burns CPU. Each
//!   level dominates none of the others — pure tradeoff.
//! - `backpressure_sweep`: backpressure (high, low) watermarks
//!   (200,50), (1000,200), (5000,1000). Wider watermarks cut p99
//!   but balloon memory; narrow watermarks reverse it. Includes
//!   one dominated config to exercise the explanation path.

use std::sync::Once;

use frankenterm_core::pareto_frontier_planner::{
    KnobConfig, LatencyMetrics, MeasurementPoint, PlannerReport, ResourceMetrics, plan,
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

/// Documented machine shape for every fixture. The string is
/// opaque to the planner (provenance-blind dominance) but pinned
/// here so the e2e logs carry a reproducible manifest.
const MACHINE_SHAPE: &str = "darwin-arm64-12c-32g-test";

fn knob(
    capture_concurrency: u32,
    search_batch_size: u32,
    compression: u32,
    high_watermark: u32,
    low_watermark: u32,
) -> KnobConfig {
    KnobConfig {
        capture_concurrency,
        search_batch_size,
        output_compression_level: compression,
        backpressure_high_watermark: high_watermark,
        backpressure_low_watermark: low_watermark,
        telemetry_sample_cap: 1000,
    }
}

fn point(
    cfg: KnobConfig,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    memory_bytes: u64,
    cpu_e3: u32,
    storage_e3: u32,
    quality_e3: u32,
    workload_id: &str,
) -> MeasurementPoint {
    MeasurementPoint {
        config: cfg,
        latency: LatencyMetrics {
            p50_us,
            p95_us,
            p99_us,
        },
        resources: ResourceMetrics {
            memory_bytes,
            cpu_percent_e3: cpu_e3,
            storage_write_pressure_e3: storage_e3,
            token_output_quality_e3: quality_e3,
        },
        machine_shape: MACHINE_SHAPE.to_string(),
        workload_id: workload_id.to_string(),
    }
}

/// Walk the fixture, log each measurement, run `plan`, and
/// return the report. Each measurement lands a structured
/// tracing-json event so a failing case carries a parseable
/// manifest.
fn replay(fixture_name: &'static str, points: &[MeasurementPoint]) -> PlannerReport {
    init_test_tracing_json();
    for (idx, p) in points.iter().enumerate() {
        info!(
            fixture = fixture_name,
            measurement = idx,
            machine_shape = %p.machine_shape,
            workload_id = %p.workload_id,
            capture_concurrency = p.config.capture_concurrency,
            search_batch_size = p.config.search_batch_size,
            compression = p.config.output_compression_level,
            high_watermark = p.config.backpressure_high_watermark,
            low_watermark = p.config.backpressure_low_watermark,
            p50_us = p.latency.p50_us,
            p95_us = p.latency.p95_us,
            p99_us = p.latency.p99_us,
            memory_bytes = p.resources.memory_bytes,
            cpu_e3 = p.resources.cpu_percent_e3,
            storage_e3 = p.resources.storage_write_pressure_e3,
            quality_e3 = p.resources.token_output_quality_e3,
            "pareto fixture measurement"
        );
    }
    plan(points)
}

/// Concurrency sweep: 1 / 2 / 4 / 8 workers. Higher concurrency
/// cuts p99 latency but raises memory and CPU. Each config wins
/// on a different dimension — frontier preserves all four.
#[test]
fn e2e_concurrency_sweep_all_pareto_optimal() {
    let workload = "concurrency-sweep-1k";
    let points = vec![
        point(
            knob(1, 64, 1, 1000, 200),
            5000,
            10_000,
            20_000,
            64 * 1024 * 1024,
            500,
            300,
            850,
            workload,
        ),
        point(
            knob(2, 64, 1, 1000, 200),
            3000,
            6000,
            12_000,
            128 * 1024 * 1024,
            900,
            300,
            850,
            workload,
        ),
        point(
            knob(4, 64, 1, 1000, 200),
            2000,
            4000,
            8000,
            256 * 1024 * 1024,
            1700,
            300,
            850,
            workload,
        ),
        point(
            knob(8, 64, 1, 1000, 200),
            1000,
            2000,
            4000,
            512 * 1024 * 1024,
            3300,
            300,
            850,
            workload,
        ),
    ];
    let report = replay("concurrency_sweep", &points);
    match report {
        PlannerReport::Frontier {
            frontier,
            dominated,
        } => {
            // Every config wins on a different dimension → all 4
            // survive.
            assert_eq!(frontier.len(), 4, "every config must stay on frontier");
            assert!(dominated.is_empty(), "no dominated configs in this sweep");
            // Lex order: capture_concurrency 1, 2, 4, 8.
            assert_eq!(frontier[0].config.capture_concurrency, 1);
            assert_eq!(frontier[1].config.capture_concurrency, 2);
            assert_eq!(frontier[2].config.capture_concurrency, 4);
            assert_eq!(frontier[3].config.capture_concurrency, 8);
        }
        other => panic!("expected Frontier, got {other:?}"),
    }
}

/// Compression sweep: levels 0 / 1 / 2 / 3. Higher compression
/// saves storage write pressure but burns CPU. Pure tradeoff —
/// every config stays on the frontier.
#[test]
fn e2e_compression_sweep_all_pareto_optimal() {
    let workload = "compression-sweep-write-heavy";
    let cfg_at = |level| knob(2, 128, level, 2000, 400);
    let points = vec![
        point(
            cfg_at(0),
            2000,
            4000,
            8000,
            128 * 1024 * 1024,
            1500,
            900,
            800,
            workload,
        ),
        point(
            cfg_at(1),
            2100,
            4100,
            8100,
            128 * 1024 * 1024,
            2000,
            500,
            800,
            workload,
        ),
        point(
            cfg_at(2),
            2200,
            4200,
            8200,
            128 * 1024 * 1024,
            2700,
            300,
            800,
            workload,
        ),
        point(
            cfg_at(3),
            2300,
            4300,
            8300,
            128 * 1024 * 1024,
            3500,
            150,
            800,
            workload,
        ),
    ];
    let report = replay("compression_sweep", &points);
    match report {
        PlannerReport::Frontier {
            frontier,
            dominated,
        } => {
            assert_eq!(
                frontier.len(),
                4,
                "every compression level must stay on frontier"
            );
            assert!(dominated.is_empty());
            // Lex order: compression 0 → 3.
            for (idx, p) in frontier.iter().enumerate() {
                assert_eq!(p.config.output_compression_level as usize, idx);
            }
        }
        other => panic!("expected Frontier, got {other:?}"),
    }
}

/// Backpressure sweep with a known dominated config. (200,50)
/// narrow watermarks; (1000,200) midrange; (5000,1000) wide.
/// Plus one redundant config (2000,400) that's strictly worse
/// than (5000,1000) — same quality, higher memory + worse p99.
/// Exercises the dominated-explanation path.
#[test]
fn e2e_backpressure_sweep_with_dominated_config() {
    let workload = "backpressure-sweep";
    let narrow = knob(2, 64, 1, 200, 50);
    let mid = knob(2, 64, 1, 1000, 200);
    let wide = knob(2, 64, 1, 5000, 1000);
    let redundant = knob(2, 64, 1, 2000, 400);

    let points = vec![
        // narrow: low memory but slow p99 (queue too tight).
        point(
            narrow,
            3000,
            7000,
            18_000,
            32 * 1024 * 1024,
            1800,
            400,
            800,
            workload,
        ),
        // mid: balanced.
        point(
            mid,
            2000,
            4000,
            8000,
            128 * 1024 * 1024,
            1500,
            400,
            800,
            workload,
        ),
        // wide: low p99, high memory.
        point(
            wide,
            1500,
            3000,
            6000,
            512 * 1024 * 1024,
            1500,
            400,
            800,
            workload,
        ),
        // redundant: same quality, higher p99 AND higher memory
        // than `wide` → dominated by wide.
        point(
            redundant,
            2200,
            4200,
            9000,
            520 * 1024 * 1024,
            1500,
            400,
            800,
            workload,
        ),
    ];
    let report = replay("backpressure_sweep", &points);
    match report {
        PlannerReport::Frontier {
            frontier,
            dominated,
        } => {
            // 3 frontier survivors + 1 dominated.
            assert_eq!(frontier.len(), 3, "frontier should retain narrow/mid/wide");
            assert_eq!(dominated.len(), 1, "redundant config should be dominated");

            assert_eq!(dominated[0].dominated_config, redundant);
            // explain_dominated picks the lex-smallest dominator
            // by KnobConfig. `mid` (high_watermark=1000) is
            // lex-smaller than `wide` (high_watermark=5000), and
            // both dominate `redundant`, so `mid` is the
            // representative dominator.
            assert_eq!(dominated[0].dominator_config, mid);
            // The dominator beats the redundant config on
            // p50/p95/p99 and memory_bytes.
            let dims = &dominated[0].strict_dimensions;
            assert!(dims.iter().any(|d| d == "p99_us"));
            assert!(dims.iter().any(|d| d == "memory_bytes"));
        }
        other => panic!("expected Frontier, got {other:?}"),
    }
}

/// Stability: re-running every fixture produces a byte-identical
/// `PlannerReport`. The planner is documented as a pure
/// function; this pins the contract end-to-end including JSON
/// encoding parity (the bead's "Golden report schema with
/// reproducible input manifest" criterion).
#[test]
fn e2e_replay_stability_byte_identical() {
    let workload = "stability";
    let points = vec![
        point(
            knob(2, 64, 1, 1000, 200),
            2000,
            4000,
            8000,
            128 * 1024 * 1024,
            1500,
            400,
            800,
            workload,
        ),
        point(
            knob(4, 128, 2, 2000, 400),
            1500,
            3000,
            6000,
            256 * 1024 * 1024,
            2500,
            300,
            800,
            workload,
        ),
        point(
            knob(8, 256, 0, 5000, 1000),
            1000,
            2000,
            4000,
            512 * 1024 * 1024,
            3500,
            900,
            800,
            workload,
        ),
    ];
    let first = replay("stability_first", &points);
    let second = replay("stability_second", &points);
    assert_eq!(first, second, "plan must be byte-identical across runs");

    let j1 = serde_json::to_string(&first).expect("serialize first");
    let j2 = serde_json::to_string(&second).expect("serialize second");
    assert_eq!(j1, j2, "JSON encoding must be stable across runs");
}

/// Sparse evidence: even a single high-quality measurement
/// produces `DataNeeded` because the operator-actionable signal
/// requires ≥ 3 points per the substrate's `MIN_POINTS_FOR_FRONTIER`.
#[test]
fn e2e_sparse_evidence_requests_more_data() {
    let points = vec![point(
        knob(2, 64, 1, 1000, 200),
        2000,
        4000,
        8000,
        128 * 1024 * 1024,
        1500,
        400,
        800,
        "sparse",
    )];
    let report = replay("sparse_evidence", &points);
    match report {
        PlannerReport::DataNeeded { reasons } => {
            assert!(!reasons.is_empty());
            // The substrate's reason string mentions the threshold.
            assert!(
                reasons
                    .iter()
                    .any(|r| r.to_lowercase().contains("measurement points")
                        || r.to_lowercase().contains("min_sample")
                        || r.contains("3"))
            );
        }
        other => panic!("expected DataNeeded, got {other:?}"),
    }
}
