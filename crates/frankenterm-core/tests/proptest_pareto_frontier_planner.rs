//! Property tests for [`pareto_frontier_planner`] (ft-1650n.13).
//!
//! Pins the dominance predicate's documented properties and the
//! frontier extractor's determinism / idempotence over arbitrary
//! measurement sets. Complements the unit tests in
//! `pareto_frontier_planner.rs::tests` and the e2e fixture suite
//! at `tests/pareto_frontier_planner_e2e.rs`.
//!
//! Properties pinned here:
//!
//! 1. **Dominance is irreflexive** — `is_dominated_by(p, p)` is
//!    always `false` for any point.
//! 2. **Dominance is asymmetric** — for any pair `(a, b)`, at
//!    most one of `is_dominated_by(a, b)` and
//!    `is_dominated_by(b, a)` is true.
//! 3. **Dominance is transitive** — if `is_dominated_by(a, b)`
//!    and `is_dominated_by(b, c)` then `is_dominated_by(a, c)`.
//!    A textbook property of Pareto dominance that the substrate
//!    relies on for `compute_frontier` correctness.
//! 4. **Frontier non-empty on non-empty input** — given ≥ 1
//!    distinct point, `compute_frontier` returns ≥ 1 entry.
//!    Pinned because a buggy dominance predicate could otherwise
//!    cull every point.
//! 5. **Frontier is fixed-point** — `compute_frontier(frontier)
//!    == frontier`. Re-applying the extractor yields the same
//!    set.
//! 6. **Frontier is permutation-stable** — shuffling the input
//!    produces an identical `frontier` (the substrate's
//!    documented determinism contract via `KnobConfig::cmp_lex`).
//! 7. **No frontier point dominates another frontier point** —
//!    pairwise check: for any two distinct frontier points,
//!    `is_dominated_by` is false in both directions.
//! 8. **`plan` purity** — same input always produces the same
//!    `PlannerReport`.

use std::sync::Once;

use frankenterm_core::pareto_frontier_planner::{
    KnobConfig, LatencyMetrics, MeasurementPoint, PlannerReport, ResourceMetrics,
    compute_frontier, is_dominated_by, plan,
};
use proptest::prelude::*;
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

/// Bounded knob-config strategy. Fields stay small so the
/// dominance comparisons exercise interesting boundary cases
/// without exploding the search space.
fn arb_knob_config() -> impl Strategy<Value = KnobConfig> {
    (0u32..=4, 0u32..=4, 0u32..=3, 0u32..=4, 0u32..=4, 0u32..=4).prop_map(
        |(c, b, comp, hi, lo, sample)| KnobConfig {
            capture_concurrency: c,
            search_batch_size: b,
            output_compression_level: comp,
            backpressure_high_watermark: hi,
            backpressure_low_watermark: lo,
            telemetry_sample_cap: sample,
        },
    )
}

fn arb_latency() -> impl Strategy<Value = LatencyMetrics> {
    (0u64..=10, 0u64..=10, 0u64..=10).prop_map(|(p50, p95, p99)| LatencyMetrics {
        p50_us: p50,
        p95_us: p95,
        p99_us: p99,
    })
}

fn arb_resources() -> impl Strategy<Value = ResourceMetrics> {
    (0u64..=10, 0u32..=10, 0u32..=10, 0u32..=10).prop_map(
        |(mem, cpu, storage, quality)| ResourceMetrics {
            memory_bytes: mem,
            cpu_percent_e3: cpu,
            storage_write_pressure_e3: storage,
            token_output_quality_e3: quality,
        },
    )
}

fn arb_measurement_point() -> impl Strategy<Value = MeasurementPoint> {
    (arb_knob_config(), arb_latency(), arb_resources()).prop_map(|(config, latency, resources)| {
        MeasurementPoint {
            config,
            latency,
            resources,
            machine_shape: "proptest".to_string(),
            workload_id: "proptest".to_string(),
        }
    })
}

fn arb_measurement_set() -> impl Strategy<Value = Vec<MeasurementPoint>> {
    prop::collection::vec(arb_measurement_point(), 1..=12)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// **Dominance is irreflexive**: a point never strictly
    /// dominates itself.
    #[test]
    fn dominance_irreflexive(p in arb_measurement_point()) {
        init_test_tracing_json();
        prop_assert!(!is_dominated_by(&p, &p));
    }

    /// **Dominance is asymmetric**: at most one of (a≺b, b≺a)
    /// is true. This is the canonical invariant of strict
    /// dominance — pinned over an arbitrary pair.
    #[test]
    fn dominance_asymmetric(a in arb_measurement_point(), b in arb_measurement_point()) {
        init_test_tracing_json();
        let a_dom_by_b = is_dominated_by(&a, &b);
        let b_dom_by_a = is_dominated_by(&b, &a);
        prop_assert!(!(a_dom_by_b && b_dom_by_a),
                     "dominance must be asymmetric: a≺b={} b≺a={}",
                     a_dom_by_b, b_dom_by_a);
    }

    /// **Dominance is transitive**: a≺b ∧ b≺c → a≺c. Textbook
    /// property of Pareto dominance — the substrate's
    /// `compute_frontier` correctness depends on it.
    #[test]
    fn dominance_transitive(
        a in arb_measurement_point(),
        b in arb_measurement_point(),
        c in arb_measurement_point(),
    ) {
        init_test_tracing_json();
        if is_dominated_by(&a, &b) && is_dominated_by(&b, &c) {
            prop_assert!(
                is_dominated_by(&a, &c),
                "transitivity violated: a≺b ∧ b≺c but a⊀c"
            );
        }
    }

    /// **Frontier non-empty on non-empty input**: given any
    /// non-empty measurement set, the Pareto frontier has at
    /// least one point. Buggy dominance would otherwise leave
    /// the frontier empty.
    #[test]
    fn frontier_non_empty_on_non_empty_input(points in arb_measurement_set()) {
        init_test_tracing_json();
        let frontier = compute_frontier(&points);
        info!(
            test = "frontier_non_empty_on_non_empty_input",
            input_count = points.len(),
            frontier_count = frontier.len(),
            "non-empty input case"
        );
        prop_assert!(!frontier.is_empty());
    }

    /// **Frontier is a fixed point of `compute_frontier`**:
    /// re-applying the extractor to its own output yields the
    /// same set (no further culling).
    #[test]
    fn frontier_is_fixed_point(points in arb_measurement_set()) {
        init_test_tracing_json();
        let frontier = compute_frontier(&points);
        let re = compute_frontier(&frontier);
        prop_assert_eq!(frontier, re);
    }

    /// **Permutation stability**: the frontier is invariant
    /// under input ordering. The substrate's documented
    /// determinism via `KnobConfig::cmp_lex` lex sort means the
    /// returned vector is byte-identical regardless of the input
    /// order.
    #[test]
    fn frontier_is_permutation_stable(
        mut points in arb_measurement_set(),
        seed in any::<u64>(),
    ) {
        init_test_tracing_json();
        let original_frontier = compute_frontier(&points);
        // Deterministic shuffle via a tiny linear-congruential
        // generator seeded from `seed`. Avoids pulling in `rand`
        // and keeps the proptest reproducible.
        let mut state = seed.wrapping_add(1);
        for i in (1..points.len()).rev() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let j = (state as usize) % (i + 1);
            points.swap(i, j);
        }
        let shuffled_frontier = compute_frontier(&points);
        prop_assert_eq!(original_frontier, shuffled_frontier);
    }

    /// **No frontier point dominates another frontier point**:
    /// for any two distinct frontier entries `(p, q)`,
    /// `is_dominated_by(p, q)` and `is_dominated_by(q, p)` are
    /// both false. The substrate's "non-dominated" definition
    /// applied pairwise.
    #[test]
    fn frontier_pairwise_incomparable(points in arb_measurement_set()) {
        init_test_tracing_json();
        let frontier = compute_frontier(&points);
        for i in 0..frontier.len() {
            for j in (i + 1)..frontier.len() {
                let p = &frontier[i];
                let q = &frontier[j];
                prop_assert!(
                    !is_dominated_by(p, q),
                    "frontier point {} dominated by frontier point {}",
                    i, j
                );
                prop_assert!(
                    !is_dominated_by(q, p),
                    "frontier point {} dominated by frontier point {}",
                    j, i
                );
            }
        }
    }

    /// **`plan` purity**: identical inputs produce identical
    /// `PlannerReport`s, including every byte of the JSON
    /// encoding. Pure-function contract pinned end-to-end.
    #[test]
    fn plan_is_pure(points in arb_measurement_set()) {
        init_test_tracing_json();
        let r1 = plan(&points);
        let r2 = plan(&points);
        prop_assert_eq!(&r1, &r2);
        let j1 = serde_json::to_string(&r1).expect("serialize r1");
        let j2 = serde_json::to_string(&r2).expect("serialize r2");
        prop_assert_eq!(j1, j2);
    }

    /// **DataNeeded gate**: any input with strictly fewer than 3
    /// points returns `DataNeeded` (the substrate's
    /// `MIN_POINTS_FOR_FRONTIER = 3` threshold).
    #[test]
    fn sparse_inputs_return_data_needed(
        points in prop::collection::vec(arb_measurement_point(), 0..=2),
    ) {
        init_test_tracing_json();
        let report = plan(&points);
        let is_data_needed = matches!(report, PlannerReport::DataNeeded { .. });
        prop_assert!(is_data_needed);
    }

    /// **Frontier ⊆ input**: every frontier point is one of the
    /// input points (the extractor doesn't synthesize new
    /// points). Pinned via the `KnobConfig` field — every
    /// frontier point's config must appear in the input.
    #[test]
    fn frontier_is_subset_of_input(points in arb_measurement_set()) {
        init_test_tracing_json();
        let frontier = compute_frontier(&points);
        for fp in &frontier {
            let found = points.iter().any(|p| p.config == fp.config
                && p.latency == fp.latency
                && p.resources == fp.resources);
            prop_assert!(found, "frontier point not in input set");
        }
    }
}
