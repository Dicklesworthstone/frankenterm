//! br-ft-1650n.13: Tail-latency Pareto Frontier Planner substrate.
//!
//! Pure-function substrate for an offline / `ft doctor` planner that
//! sweeps swarm resource knobs and reports Pareto frontiers across
//! tail-latency, memory, CPU, storage write pressure, and token
//! output quality. Operators read the frontier to pick evidence-
//! backed presets instead of folklore constants.
//!
//! ## Objectives
//!
//! All metrics MINIMIZE except `token_output_quality_e3`, which
//! MAXIMIZES. Concretely, point `b` weakly dominates point `a` iff
//! every minimization metric is `<=` and the quality metric is
//! `>=`. `b` strictly dominates `a` iff at least one inequality is
//! strict in `b`'s favor.
//!
//! See [`is_dominated_by`] for the canonical implementation.
//!
//! ## What ships in this slice
//!
//! - [`KnobConfig`] — a single point in the sweep space (capture
//!   concurrency, search batch size, output compression, two
//!   backpressure watermarks, telemetry sample cap).
//! - [`LatencyMetrics`] / [`ResourceMetrics`] — measured outcome
//!   per knob configuration.
//! - [`MeasurementPoint`] — `KnobConfig` + outcome metrics + the
//!   provenance fields (`machine_shape`, `workload_id`) operators
//!   need to reproduce a result.
//! - [`is_dominated_by`] — pure dominance predicate.
//! - [`compute_frontier`] — extracts non-dominated points and
//!   sorts them with deterministic tie-breaks.
//! - [`explain_dominated`] — builds operator-facing rationale
//!   strings naming the dimensions on which a point loses.
//! - [`plan`] — entry point; returns `PlannerReport::Frontier`
//!   when evidence is sufficient, `PlannerReport::DataNeeded`
//!   otherwise.
//!
//! ## What is deferred
//!
//! - Sweep harness: scheduling actual benchmark runs across the
//!   knob space is a `bench/e2e` concern. The substrate here
//!   accepts already-measured points.
//! - Machine shape derivation: `MachineShape` is a serializable
//!   string; deriving it from `/proc/cpuinfo`-style probes is a
//!   wired-pass concern.

use serde::{Deserialize, Serialize};

/// br-ft-1650n.13: a single point in the resource-knob sweep
/// space. Every field is a `u32` so the sweep harness can encode
/// a config as a fixed-shape JSON or TOML fragment without
/// floating-point drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KnobConfig {
    /// Capture concurrency — number of pane-output extractor
    /// workers running in parallel.
    pub capture_concurrency: u32,
    /// Search batch size — how many records the search backend
    /// flushes per write.
    pub search_batch_size: u32,
    /// Output compression level (0 = none, higher = stronger).
    pub output_compression_level: u32,
    /// Backpressure high watermark — when this many records are
    /// queued, the producer slows.
    pub backpressure_high_watermark: u32,
    /// Backpressure low watermark — when the queue drops below
    /// this, the producer resumes.
    pub backpressure_low_watermark: u32,
    /// Telemetry sample cap — maximum number of telemetry events
    /// retained per second.
    pub telemetry_sample_cap: u32,
}

impl KnobConfig {
    /// Stable tie-break ordering for two configs that produce
    /// identical metrics. Operators want the planner to pick the
    /// LOWEST-resource config among equivalents (lower
    /// concurrency, smaller batches, weaker compression, smaller
    /// watermarks, smaller sample cap), so `cmp` orders by every
    /// field in order. Documented as the canonical tie-break so a
    /// future sweep harness can rely on it.
    fn cmp_lex(&self, other: &Self) -> std::cmp::Ordering {
        self.capture_concurrency
            .cmp(&other.capture_concurrency)
            .then_with(|| self.search_batch_size.cmp(&other.search_batch_size))
            .then_with(|| {
                self.output_compression_level
                    .cmp(&other.output_compression_level)
            })
            .then_with(|| {
                self.backpressure_high_watermark
                    .cmp(&other.backpressure_high_watermark)
            })
            .then_with(|| {
                self.backpressure_low_watermark
                    .cmp(&other.backpressure_low_watermark)
            })
            .then_with(|| self.telemetry_sample_cap.cmp(&other.telemetry_sample_cap))
    }
}

/// Measured tail-latency for a knob configuration, in
/// microseconds. p50 ≤ p95 ≤ p99 is an invariant of the input
/// data — the planner does not enforce it (sweep harnesses pass
/// already-validated samples).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct LatencyMetrics {
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
}

/// Measured resource cost + quality outcome for a knob
/// configuration.
///
/// `*_e3` fields are integer-encoded fixed-point: divide by 1000
/// to get the floating-point value. This avoids `f64` Eq /
/// dominance issues without losing the precision a sweep harness
/// needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct ResourceMetrics {
    /// Working-set memory in bytes.
    pub memory_bytes: u64,
    /// CPU usage in millipercent (1000 = 1.00%).
    pub cpu_percent_e3: u32,
    /// Storage write pressure in millipercent of capacity.
    pub storage_write_pressure_e3: u32,
    /// Token-output quality score in millipercent of perfect
    /// (1000 = 1.00, the planner's MAXIMIZE objective).
    pub token_output_quality_e3: u32,
}

/// br-ft-1650n.13: a single sweep observation = config + outcome
/// + provenance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MeasurementPoint {
    pub config: KnobConfig,
    pub latency: LatencyMetrics,
    pub resources: ResourceMetrics,
    /// Free-form machine-shape string (e.g., `"darwin-arm64-12c-32g"`).
    /// Operators need to reproduce a result on the same machine
    /// class; the planner does not parse this field.
    pub machine_shape: String,
    /// Workload manifest identifier (e.g., `"replay-write-heavy-1k"`).
    pub workload_id: String,
}

/// br-ft-1650n.13: Pareto dominance predicate.
///
/// Returns `true` iff `b` strictly dominates `a` — i.e., `b` is
/// no worse than `a` on every objective AND strictly better on at
/// least one.
///
/// Objectives:
/// - p50_us, p95_us, p99_us — minimize.
/// - memory_bytes — minimize.
/// - cpu_percent_e3 — minimize.
/// - storage_write_pressure_e3 — minimize.
/// - token_output_quality_e3 — MAXIMIZE.
///
/// Properties (pinned by tests below):
/// - Irreflexive: `is_dominated_by(a, a) == false`.
/// - Asymmetric: `is_dominated_by(a, b) && is_dominated_by(b, a)`
///   never both true.
/// - Provenance-blind: the predicate ignores `machine_shape` and
///   `workload_id`; mixing those is a caller-side concern.
#[must_use]
pub fn is_dominated_by(a: &MeasurementPoint, b: &MeasurementPoint) -> bool {
    let weak = b.latency.p50_us <= a.latency.p50_us
        && b.latency.p95_us <= a.latency.p95_us
        && b.latency.p99_us <= a.latency.p99_us
        && b.resources.memory_bytes <= a.resources.memory_bytes
        && b.resources.cpu_percent_e3 <= a.resources.cpu_percent_e3
        && b.resources.storage_write_pressure_e3 <= a.resources.storage_write_pressure_e3
        && b.resources.token_output_quality_e3 >= a.resources.token_output_quality_e3;
    if !weak {
        return false;
    }
    b.latency.p50_us < a.latency.p50_us
        || b.latency.p95_us < a.latency.p95_us
        || b.latency.p99_us < a.latency.p99_us
        || b.resources.memory_bytes < a.resources.memory_bytes
        || b.resources.cpu_percent_e3 < a.resources.cpu_percent_e3
        || b.resources.storage_write_pressure_e3 < a.resources.storage_write_pressure_e3
        || b.resources.token_output_quality_e3 > a.resources.token_output_quality_e3
}

/// br-ft-1650n.13: extract the Pareto frontier from a set of
/// measurement points. Frontier points are sorted by
/// `KnobConfig::cmp_lex` so the output is deterministic across
/// runs.
///
/// Implementation is `O(n²)` — fine for the substrate; the
/// expected sweep size is dozens to a few hundred points. A
/// future optimization can switch to an `O(n log n)` 2-D
/// front-tracker once the planner is wired into a CLI.
#[must_use]
pub fn compute_frontier(points: &[MeasurementPoint]) -> Vec<MeasurementPoint> {
    let mut frontier: Vec<MeasurementPoint> = points
        .iter()
        .filter(|p| !points.iter().any(|q| is_dominated_by(p, q)))
        .cloned()
        .collect();
    frontier.sort_by(|a, b| a.config.cmp_lex(&b.config));
    // Deduplicate adjacent equal configs after sort. Two configs
    // can be byte-identical (same knob settings, identical
    // metrics) when the sweep harness re-ran a config — the
    // operator-facing frontier should list each config once.
    frontier.dedup_by(|a, b| a.config == b.config);
    frontier
}

/// br-ft-1650n.13: rationale entry for a dominated point. Lists
/// the dimensions on which the dominator strictly beats the
/// dominated point so an operator can read why a config was
/// excluded from the frontier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DominatedExplanation {
    /// The dominated configuration.
    pub dominated_config: KnobConfig,
    /// One representative dominator (the lex-smallest by config
    /// among all dominators, for determinism).
    pub dominator_config: KnobConfig,
    /// Names of the dimensions the dominator strictly improved.
    pub strict_dimensions: Vec<String>,
}

/// br-ft-1650n.13: build dominated-config explanations. For each
/// dominated point, picks the lex-smallest dominator (by knob
/// config) so the output is deterministic.
#[must_use]
pub fn explain_dominated(points: &[MeasurementPoint]) -> Vec<DominatedExplanation> {
    let mut out = Vec::new();
    for p in points {
        let mut dominators: Vec<&MeasurementPoint> =
            points.iter().filter(|q| is_dominated_by(p, q)).collect();
        if dominators.is_empty() {
            continue;
        }
        dominators.sort_by(|a, b| a.config.cmp_lex(&b.config));
        let dominator = dominators[0];
        out.push(DominatedExplanation {
            dominated_config: p.config,
            dominator_config: dominator.config,
            strict_dimensions: strict_winning_dimensions(p, dominator),
        });
    }
    out.sort_by(|a, b| a.dominated_config.cmp_lex(&b.dominated_config));
    out
}

fn strict_winning_dimensions(
    dominated: &MeasurementPoint,
    dominator: &MeasurementPoint,
) -> Vec<String> {
    let mut dims = Vec::new();
    if dominator.latency.p50_us < dominated.latency.p50_us {
        dims.push("p50_us".to_string());
    }
    if dominator.latency.p95_us < dominated.latency.p95_us {
        dims.push("p95_us".to_string());
    }
    if dominator.latency.p99_us < dominated.latency.p99_us {
        dims.push("p99_us".to_string());
    }
    if dominator.resources.memory_bytes < dominated.resources.memory_bytes {
        dims.push("memory_bytes".to_string());
    }
    if dominator.resources.cpu_percent_e3 < dominated.resources.cpu_percent_e3 {
        dims.push("cpu_percent_e3".to_string());
    }
    if dominator.resources.storage_write_pressure_e3 < dominated.resources.storage_write_pressure_e3
    {
        dims.push("storage_write_pressure_e3".to_string());
    }
    if dominator.resources.token_output_quality_e3 > dominated.resources.token_output_quality_e3 {
        dims.push("token_output_quality_e3".to_string());
    }
    dims
}

/// Minimum number of measurement points before the planner emits
/// a frontier. With fewer points the operator-actionable signal
/// is too thin and the planner returns `DataNeeded`.
pub const MIN_POINTS_FOR_FRONTIER: usize = 3;

/// br-ft-1650n.13: planner output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlannerReport {
    /// Sufficient evidence — frontier and dominated-config
    /// explanations are present.
    Frontier {
        frontier: Vec<MeasurementPoint>,
        dominated: Vec<DominatedExplanation>,
    },
    /// Insufficient evidence — caller should run more sweeps.
    /// The reasons list explains what's missing.
    DataNeeded { reasons: Vec<String> },
}

/// br-ft-1650n.13: planner entry point. Pure function over the
/// measurement-point set.
#[must_use]
pub fn plan(points: &[MeasurementPoint]) -> PlannerReport {
    if points.len() < MIN_POINTS_FOR_FRONTIER {
        return PlannerReport::DataNeeded {
            reasons: vec![format!(
                "need at least {MIN_POINTS_FOR_FRONTIER} measurement points, got {}",
                points.len()
            )],
        };
    }
    let frontier = compute_frontier(points);
    let dominated = explain_dominated(points);
    PlannerReport::Frontier {
        frontier,
        dominated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(c: u32, b: u32) -> KnobConfig {
        KnobConfig {
            capture_concurrency: c,
            search_batch_size: b,
            output_compression_level: 0,
            backpressure_high_watermark: 1000,
            backpressure_low_watermark: 200,
            telemetry_sample_cap: 100,
        }
    }

    fn point(
        c: u32,
        b: u32,
        p50: u64,
        p95: u64,
        p99: u64,
        mem: u64,
        cpu_e3: u32,
        quality_e3: u32,
    ) -> MeasurementPoint {
        MeasurementPoint {
            config: cfg(c, b),
            latency: LatencyMetrics {
                p50_us: p50,
                p95_us: p95,
                p99_us: p99,
            },
            resources: ResourceMetrics {
                memory_bytes: mem,
                cpu_percent_e3: cpu_e3,
                storage_write_pressure_e3: 0,
                token_output_quality_e3: quality_e3,
            },
            machine_shape: "test".to_string(),
            workload_id: "test".to_string(),
        }
    }

    /// Dominance is irreflexive: a point does not dominate itself.
    /// (No strict inequality is possible against self.)
    #[test]
    fn dominance_is_irreflexive() {
        let p = point(2, 64, 1000, 2000, 5000, 100, 1500, 800);
        assert!(!is_dominated_by(&p, &p));
    }

    /// Dominance is asymmetric: if a dominates b, b cannot
    /// dominate a.
    #[test]
    fn dominance_is_asymmetric() {
        // b is strictly better on every minimize dimension and
        // ties on quality.
        let a = point(2, 64, 2000, 4000, 8000, 200, 2000, 800);
        let b = point(2, 64, 1000, 2000, 4000, 100, 1500, 800);
        assert!(is_dominated_by(&a, &b));
        assert!(!is_dominated_by(&b, &a));
    }

    /// Higher token_output_quality_e3 is BETTER (the lone
    /// MAXIMIZE objective).
    #[test]
    fn quality_maximizes_not_minimizes() {
        // a and b tie on every minimize dimension; b has higher
        // quality → b dominates a.
        let a = point(2, 64, 1000, 2000, 4000, 100, 1500, 600);
        let b = point(2, 64, 1000, 2000, 4000, 100, 1500, 800);
        assert!(is_dominated_by(&a, &b));
        assert!(!is_dominated_by(&b, &a));
    }

    /// Two points each strictly better on a different dimension
    /// — neither dominates the other (both stay on the frontier).
    #[test]
    fn incomparable_points_neither_dominates() {
        // a wins on memory; b wins on p99.
        let a = point(2, 64, 2000, 4000, 8000, 50, 1500, 800);
        let b = point(2, 64, 1000, 2000, 4000, 200, 1500, 800);
        assert!(!is_dominated_by(&a, &b));
        assert!(!is_dominated_by(&b, &a));
    }

    /// Empty input → DataNeeded.
    #[test]
    fn empty_input_returns_data_needed() {
        let report = plan(&[]);
        assert!(matches!(report, PlannerReport::DataNeeded { .. }));
    }

    /// Below the threshold → DataNeeded.
    #[test]
    fn sparse_input_returns_data_needed() {
        let p1 = point(2, 64, 1000, 2000, 4000, 100, 1500, 800);
        let p2 = point(4, 128, 800, 1500, 3000, 200, 2500, 800);
        let report = plan(&[p1, p2]);
        match report {
            PlannerReport::DataNeeded { reasons } => assert!(!reasons.is_empty()),
            other @ PlannerReport::Frontier { .. } => {
                panic!("expected DataNeeded, got {other:?}")
            }
        }
    }

    /// Three Pareto-incomparable points all stay on the frontier.
    #[test]
    fn all_frontier_points_survive() {
        // Each wins on a different dimension.
        let p1 = point(2, 64, 500, 1000, 2000, 200, 2000, 800); // wins on p99
        let p2 = point(4, 128, 1000, 2000, 4000, 100, 2000, 800); // wins on memory
        let p3 = point(8, 256, 1000, 2000, 4000, 200, 1000, 800); // wins on cpu
        let report = plan(&[p1, p2, p3]);
        match report {
            PlannerReport::Frontier {
                frontier,
                dominated,
            } => {
                assert_eq!(frontier.len(), 3);
                assert!(dominated.is_empty());
            }
            other @ PlannerReport::DataNeeded { .. } => {
                panic!("expected Frontier, got {other:?}")
            }
        }
    }

    /// One dominated point is excluded from the frontier and
    /// reported in `dominated`.
    #[test]
    fn dominated_point_explained() {
        let dom1 = point(2, 64, 1000, 2000, 4000, 100, 1500, 800);
        let dom2 = point(4, 128, 800, 1500, 3000, 200, 2000, 800);
        // p3 is dominated by dom1 (worse on every dim).
        let p3 = point(8, 32, 2000, 4000, 8000, 200, 2500, 600);
        let report = plan(&[dom1, dom2, p3.clone()]);
        match report {
            PlannerReport::Frontier {
                frontier,
                dominated,
            } => {
                assert_eq!(frontier.len(), 2);
                assert_eq!(dominated.len(), 1);
                assert_eq!(dominated[0].dominated_config, p3.config);
                // strict_dimensions lists the dimensions the
                // dominator beat — at minimum p50_us, p95_us,
                // p99_us, cpu_percent_e3, token_output_quality_e3
                // (memory_bytes ties at 200).
                assert!(dominated[0].strict_dimensions.iter().any(|d| d == "p99_us"));
                assert!(
                    dominated[0]
                        .strict_dimensions
                        .iter()
                        .any(|d| d == "token_output_quality_e3")
                );
            }
            other @ PlannerReport::DataNeeded { .. } => {
                panic!("expected Frontier, got {other:?}")
            }
        }
    }

    /// Frontier output ordering is deterministic (lex by config).
    /// Pinned so a sweep harness can rely on byte-identical
    /// reports across runs.
    ///
    /// Three points with IDENTICAL metrics but DIFFERENT configs
    /// are pairwise incomparable (no strict inequality on any
    /// dimension), so all three stay on the frontier. Ordering is
    /// by lex `KnobConfig` → ascending `capture_concurrency`.
    #[test]
    fn frontier_ordering_is_deterministic() {
        let p1 = point(8, 256, 1000, 2000, 4000, 100, 2000, 800);
        let p2 = point(4, 128, 1000, 2000, 4000, 100, 2000, 800);
        let p3 = point(2, 64, 1000, 2000, 4000, 100, 2000, 800);
        let report = plan(&[p1.clone(), p2.clone(), p3.clone()]);
        if let PlannerReport::Frontier { frontier, .. } = report {
            assert_eq!(frontier.len(), 3);
            // Lex order on capture_concurrency: 2 < 4 < 8.
            assert_eq!(frontier[0].config, p3.config);
            assert_eq!(frontier[1].config, p2.config);
            assert_eq!(frontier[2].config, p1.config);
        } else {
            panic!("expected Frontier");
        }
    }

    /// Pinned: when two byte-identical configs (same KnobConfig
    /// AND same metrics) are present, the dedup_by reduces them
    /// to one frontier entry. Sweep harnesses sometimes re-run a
    /// config; the operator-facing frontier should list each
    /// config once.
    #[test]
    fn frontier_dedups_identical_configs() {
        let p = point(2, 64, 1000, 2000, 4000, 100, 2000, 800);
        let p_dup = p.clone();
        let other = point(4, 128, 800, 1500, 3000, 200, 1500, 900);
        let report = plan(&[p, p_dup, other]);
        if let PlannerReport::Frontier { frontier, .. } = report {
            // p + p_dup dedup to one; other is incomparable → 2.
            assert_eq!(frontier.len(), 2);
        } else {
            panic!("expected Frontier");
        }
    }

    /// Explanation list ordering is deterministic (sorted by
    /// dominated config).
    #[test]
    fn dominated_explanations_are_sorted_deterministically() {
        // Three frontier points + two dominated.
        let dom = point(2, 64, 500, 1000, 2000, 100, 1000, 1000);
        let pa = point(8, 256, 5000, 10_000, 20_000, 1000, 5000, 100); // dominated
        let pb = point(4, 128, 5000, 10_000, 20_000, 1000, 5000, 100); // dominated
        let report = plan(&[dom, pa.clone(), pb.clone()]);
        if let PlannerReport::Frontier { dominated, .. } = report {
            assert_eq!(dominated.len(), 2);
            // pb has lex-smaller config (capture_concurrency=4)
            // than pa (capture_concurrency=8) → pb first.
            assert_eq!(dominated[0].dominated_config, pb.config);
            assert_eq!(dominated[1].dominated_config, pa.config);
        } else {
            panic!("expected Frontier");
        }
    }

    /// Provenance-blind: dominance ignores machine_shape and
    /// workload_id. Two points with identical metrics + configs
    /// but different provenance still have one dominate the other
    /// only via the metric path.
    #[test]
    fn dominance_ignores_provenance_fields() {
        let a = MeasurementPoint {
            machine_shape: "darwin".to_string(),
            workload_id: "wa".to_string(),
            ..point(2, 64, 1000, 2000, 4000, 100, 1500, 800)
        };
        let b = MeasurementPoint {
            machine_shape: "linux".to_string(),
            workload_id: "wb".to_string(),
            ..point(2, 64, 500, 1000, 2000, 50, 1000, 800)
        };
        assert!(is_dominated_by(&a, &b));
    }

    /// PlannerReport serde roundtrip preserves both variants.
    #[test]
    fn planner_report_serde_roundtrip() {
        let report = PlannerReport::DataNeeded {
            reasons: vec!["sparse".to_string()],
        };
        let json = serde_json::to_string(&report).expect("serialize");
        let back: PlannerReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(report, back);

        let frontier_report = PlannerReport::Frontier {
            frontier: vec![point(2, 64, 1000, 2000, 4000, 100, 1500, 800)],
            dominated: vec![DominatedExplanation {
                dominated_config: cfg(8, 256),
                dominator_config: cfg(2, 64),
                strict_dimensions: vec!["p99_us".to_string()],
            }],
        };
        let json = serde_json::to_string(&frontier_report).expect("serialize");
        let back: PlannerReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(frontier_report, back);
    }
}
