//! Renderer for [`crate::pareto_frontier_planner::PlannerReport`]. [ft-1650n.13]
//!
//! Slice ships the operator-facing rendering layer on top of the
//! substrate at `pareto_frontier_planner.rs` (sibling-shipped at
//! 73dd441df). 5th iteration of the established renderer pattern:
//!
//! 1. capability_passport_doctor (ft-ykp2y) — sibling-shipped.
//! 2. handoff_capsule_inspect (ft-yk9lp slice 1) — mine.
//! 3. approval_impact_simulator_doctor (ft-1650n.14 slice) — mine.
//! 4. onboarding_stress_capsule_doctor (ft-1650n.17 slice) — mine.
//! 5. pareto_frontier_planner_doctor (this commit, ft-1650n.13) — mine.
//!
//! - `render_text(&report) -> String` — concise plain-text preview
//!   suitable for `ft tune doctor` output. Header line + frontier
//!   table + dominated-config explanations.
//! - `render(&report) -> PlannerReportRendering` — structured
//!   JSON-serializable rendering for machine consumption (audit-feed
//!   integration, dashboarding, automation harnesses, golden-report
//!   schema per the bead's acceptance criteria).
//!
//! # Privacy
//!
//! Pareto data is operational/numeric (knob configs, latency,
//! resource cost, quality scores, machine_shape strings, workload
//! IDs). No credential-bearing fields. The canary at the bottom of
//! this file plants a quasi-credential string in the free-form
//! `machine_shape` and `workload_id` fields and verifies the
//! renderer transports them faithfully (no Debug-format
//! amplification, text and JSON paths surface the same count). A
//! caller using these fields for IDs that happen to look like
//! tokens isn't penalized by the renderer, but operators are
//! reminded by docstring that machine_shape / workload_id are
//! audit-visible.

use serde::{Deserialize, Serialize};

use crate::pareto_frontier_planner::{
    DominatedExplanation, KnobConfig, LatencyMetrics, MeasurementPoint, PlannerReport,
    ResourceMetrics,
};

/// Stateless renderer for [`PlannerReport`].
pub struct PlannerReportRenderer;

impl PlannerReportRenderer {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Render `report` as a concise operator-facing plain-text
    /// preview.
    #[must_use]
    pub fn render_text(&self, report: &PlannerReport) -> String {
        match report {
            PlannerReport::Frontier {
                frontier,
                dominated,
            } => Self::render_frontier_text(frontier, dominated),
            PlannerReport::DataNeeded { reasons } => Self::render_data_needed_text(reasons),
        }
    }

    /// Render `report` as a structured JSON-serializable rendering.
    #[must_use]
    pub fn render(&self, report: &PlannerReport) -> PlannerReportRendering {
        match report {
            PlannerReport::Frontier {
                frontier,
                dominated,
            } => PlannerReportRendering::Frontier {
                frontier_size: frontier.len(),
                dominated_size: dominated.len(),
                frontier: frontier
                    .iter()
                    .map(MeasurementPointRendering::from_substrate)
                    .collect(),
                dominated: dominated
                    .iter()
                    .map(DominatedExplanationRendering::from_substrate)
                    .collect(),
            },
            PlannerReport::DataNeeded { reasons } => PlannerReportRendering::DataNeeded {
                reasons: reasons.clone(),
            },
        }
    }

    fn render_frontier_text(
        frontier: &[MeasurementPoint],
        dominated: &[DominatedExplanation],
    ) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "pareto-frontier: verdict=frontier frontier_size={} dominated_size={}\n",
            frontier.len(),
            dominated.len(),
        ));
        if frontier.is_empty() {
            out.push_str("  frontier: ∅\n");
        } else {
            out.push_str("  frontier:\n");
            for (i, p) in frontier.iter().enumerate() {
                out.push_str(&format!(
                    "    [{i}] {}\n      machine={} workload={}\n      latency: p50={}us p95={}us p99={}us\n      resources: mem={}B cpu={} write={} quality={}\n",
                    knob_config_label(&p.config),
                    p.machine_shape,
                    p.workload_id,
                    p.latency.p50_us,
                    p.latency.p95_us,
                    p.latency.p99_us,
                    p.resources.memory_bytes,
                    p.resources.cpu_percent_e3,
                    p.resources.storage_write_pressure_e3,
                    p.resources.token_output_quality_e3,
                ));
            }
        }
        if dominated.is_empty() {
            out.push_str("  dominated: ∅\n");
        } else {
            out.push_str("  dominated:\n");
            for (i, d) in dominated.iter().enumerate() {
                out.push_str(&format!(
                    "    [{i}] dominated={} → dominator={} winning={}\n",
                    knob_config_label(&d.dominated_config),
                    knob_config_label(&d.dominator_config),
                    if d.strict_dimensions.is_empty() {
                        "∅".to_string()
                    } else {
                        d.strict_dimensions.join(",")
                    },
                ));
            }
        }
        out
    }

    fn render_data_needed_text(reasons: &[String]) -> String {
        let mut out = String::new();
        out.push_str("pareto-frontier: verdict=data_needed\n");
        if reasons.is_empty() {
            out.push_str("  reasons: ∅ (substrate emitted DataNeeded with no reasons)\n");
        } else {
            out.push_str("  reasons:\n");
            for (i, r) in reasons.iter().enumerate() {
                out.push_str(&format!("    [{i}] {r}\n"));
            }
        }
        out
    }
}

impl Default for PlannerReportRenderer {
    fn default() -> Self {
        Self::new()
    }
}

/// Structured JSON-serializable rendering. Tagged enum mirroring the
/// substrate's [`PlannerReport`] discriminant so machine consumers
/// can branch on `variant`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "variant", rename_all = "snake_case")]
pub enum PlannerReportRendering {
    Frontier {
        frontier_size: usize,
        dominated_size: usize,
        frontier: Vec<MeasurementPointRendering>,
        dominated: Vec<DominatedExplanationRendering>,
    },
    DataNeeded {
        reasons: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasurementPointRendering {
    pub config: KnobConfig,
    pub latency: LatencyMetrics,
    pub resources: ResourceMetrics,
    pub machine_shape: String,
    pub workload_id: String,
}

impl MeasurementPointRendering {
    fn from_substrate(p: &MeasurementPoint) -> Self {
        Self {
            config: p.config,
            latency: p.latency,
            resources: p.resources,
            machine_shape: p.machine_shape.clone(),
            workload_id: p.workload_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DominatedExplanationRendering {
    pub dominated_config: KnobConfig,
    pub dominator_config: KnobConfig,
    pub strict_dimensions: Vec<String>,
}

impl DominatedExplanationRendering {
    fn from_substrate(d: &DominatedExplanation) -> Self {
        Self {
            dominated_config: d.dominated_config,
            dominator_config: d.dominator_config,
            strict_dimensions: d.strict_dimensions.clone(),
        }
    }
}

fn knob_config_label(c: &KnobConfig) -> String {
    format!(
        "cap={} batch={} compr={} hi={} lo={} tcap={}",
        c.capture_concurrency,
        c.search_batch_size,
        c.output_compression_level,
        c.backpressure_high_watermark,
        c.backpressure_low_watermark,
        c.telemetry_sample_cap,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pareto_frontier_planner::plan;

    fn point(
        capture: u32,
        batch: u32,
        compr: u32,
        hi: u32,
        lo: u32,
        tcap: u32,
        p50: u64,
        p95: u64,
        p99: u64,
        mem: u64,
        cpu_e3: u32,
        write_e3: u32,
        quality_e3: u32,
        shape: &str,
        workload: &str,
    ) -> MeasurementPoint {
        MeasurementPoint {
            config: KnobConfig {
                capture_concurrency: capture,
                search_batch_size: batch,
                output_compression_level: compr,
                backpressure_high_watermark: hi,
                backpressure_low_watermark: lo,
                telemetry_sample_cap: tcap,
            },
            latency: LatencyMetrics {
                p50_us: p50,
                p95_us: p95,
                p99_us: p99,
            },
            resources: ResourceMetrics {
                memory_bytes: mem,
                cpu_percent_e3: cpu_e3,
                storage_write_pressure_e3: write_e3,
                token_output_quality_e3: quality_e3,
            },
            machine_shape: shape.to_string(),
            workload_id: workload.to_string(),
        }
    }

    /// Three-point sweep where one config dominates another and a
    /// third is on the frontier alongside the dominator.
    fn three_point_sweep() -> Vec<MeasurementPoint> {
        vec![
            // Dominated: high latency + high resources + low quality.
            point(
                4,
                100,
                1,
                1000,
                500,
                1000,
                5000,
                10000,
                20000,
                1_000_000,
                50_000,
                30_000,
                800,
                "darwin-arm64-12c-32g",
                "replay-write-heavy-1k",
            ),
            // Dominator: lower latency + lower resources + higher quality.
            point(
                2,
                200,
                2,
                800,
                400,
                800,
                3000,
                7000,
                15000,
                800_000,
                40_000,
                25_000,
                950,
                "darwin-arm64-12c-32g",
                "replay-write-heavy-1k",
            ),
            // Frontier sibling: trades p50 for memory.
            point(
                8,
                400,
                0,
                2000,
                1000,
                1500,
                2000,
                9000,
                18000,
                1_500_000,
                60_000,
                35_000,
                900,
                "darwin-arm64-12c-32g",
                "replay-write-heavy-1k",
            ),
        ]
    }

    #[test]
    fn render_text_frontier_with_dominated_explanation() {
        let report = plan(&three_point_sweep());
        let text = PlannerReportRenderer::new().render_text(&report);
        assert!(text.contains("verdict=frontier"));
        assert!(text.contains("frontier:"));
        assert!(text.contains("dominated:"));
        // Frontier and dominated both have entries.
        assert!(text.contains("[0]"));
        // Knob config labels surface the canonical k=v form.
        assert!(text.contains("cap="));
        assert!(text.contains("batch="));
        // Latency + resource numbers appear.
        assert!(text.contains("p50="));
        assert!(text.contains("quality="));
        // Machine shape + workload appear.
        assert!(text.contains("darwin-arm64-12c-32g"));
        assert!(text.contains("replay-write-heavy-1k"));
        // Dominated row shows winning dimensions.
        assert!(text.contains("winning="));
    }

    #[test]
    fn render_text_data_needed_lists_reasons() {
        let report = plan(&[]);
        let text = PlannerReportRenderer::new().render_text(&report);
        assert!(text.contains("verdict=data_needed"));
        assert!(text.contains("reasons:"));
        assert!(text.contains("at least 3 measurement points"));
    }

    #[test]
    fn render_text_data_needed_empty_reasons_marker() {
        // Defensive: substrate currently always populates reasons,
        // but if a future contract change emits an empty list, the
        // renderer must surface that explicitly rather than print a
        // confusing blank section.
        let report = PlannerReport::DataNeeded {
            reasons: Vec::new(),
        };
        let text = PlannerReportRenderer::new().render_text(&report);
        assert!(text.contains("verdict=data_needed"));
        assert!(
            text.contains("substrate emitted DataNeeded with no reasons"),
            "renderer must surface the empty-reasons defensive path; got {text}"
        );
    }

    #[test]
    fn render_json_frontier_dispatches_to_frontier_variant() {
        let report = plan(&three_point_sweep());
        let rendering = PlannerReportRenderer::new().render(&report);
        match rendering {
            PlannerReportRendering::Frontier {
                frontier_size,
                dominated_size,
                frontier,
                dominated,
            } => {
                assert!(frontier_size >= 1);
                assert_eq!(frontier_size, frontier.len());
                assert_eq!(dominated_size, dominated.len());
                // Each frontier point preserves machine_shape + workload_id.
                for p in &frontier {
                    assert_eq!(p.machine_shape, "darwin-arm64-12c-32g");
                    assert_eq!(p.workload_id, "replay-write-heavy-1k");
                }
            }
            PlannerReportRendering::DataNeeded { .. } => panic!("expected Frontier variant"),
        }
    }

    #[test]
    fn render_json_data_needed_dispatches_to_data_needed_variant() {
        let report = plan(&[]);
        let rendering = PlannerReportRenderer::new().render(&report);
        match rendering {
            PlannerReportRendering::DataNeeded { reasons } => {
                assert!(!reasons.is_empty());
                assert!(reasons[0].contains("at least 3"));
            }
            PlannerReportRendering::Frontier { .. } => panic!("expected DataNeeded variant"),
        }
    }

    #[test]
    fn render_json_serde_roundtrip_for_frontier_variant() {
        let report = plan(&three_point_sweep());
        let rendering = PlannerReportRenderer::new().render(&report);
        let json = serde_json::to_string(&rendering).unwrap();
        assert!(json.contains("\"variant\":\"frontier\""));
        let parsed: PlannerReportRendering = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rendering);
    }

    #[test]
    fn render_json_serde_roundtrip_for_data_needed_variant() {
        let report = plan(&[]);
        let rendering = PlannerReportRenderer::new().render(&report);
        let json = serde_json::to_string(&rendering).unwrap();
        assert!(json.contains("\"variant\":\"data_needed\""));
        let parsed: PlannerReportRendering = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rendering);
    }

    #[test]
    fn render_text_dominated_section_shows_winning_dimension_names() {
        // Pin that the renderer surfaces the strict_dimensions list
        // — operators triage dominated configs by reading WHICH
        // axis they lost on. A dropped dimension would silently
        // hide the diagnosis.
        let report = plan(&three_point_sweep());
        let text = PlannerReportRenderer::new().render_text(&report);
        // At least one of the latency or resource dimension names
        // must appear in the dominated section.
        let has_dim = [
            "p50_us",
            "p95_us",
            "p99_us",
            "memory_bytes",
            "cpu_percent_e3",
        ]
        .iter()
        .any(|d| text.contains(d));
        assert!(
            has_dim,
            "dominated section must surface at least one strict winning dimension; got {text}"
        );
    }

    #[test]
    fn render_json_size_summaries_match_vector_lengths() {
        // Pin frontier_size == frontier.len() and dominated_size ==
        // dominated.len() invariant. Without this, dashboards reading
        // the summary scalar diverge from operators reading the lists.
        let report = plan(&three_point_sweep());
        let rendering = PlannerReportRenderer::new().render(&report);
        if let PlannerReportRendering::Frontier {
            frontier_size,
            dominated_size,
            frontier,
            dominated,
        } = rendering
        {
            assert_eq!(frontier_size, frontier.len());
            assert_eq!(dominated_size, dominated.len());
        } else {
            panic!("expected Frontier variant");
        }
    }

    /// Pareto data is numeric/operational, but `machine_shape` and
    /// `workload_id` are free-form caller-supplied strings. The
    /// renderer must transport these faithfully — neither dropping
    /// them nor amplifying via Debug-format. The canary plants a
    /// quasi-credential pattern in those fields and verifies the
    /// text and JSON paths surface the SAME number of occurrences
    /// (no Debug-amplification on either side).
    #[test]
    fn render_does_not_amplify_caller_supplied_strings_ft_1650n_13() {
        const PLANTED: &str = "PLANTED-1650n.13-canary-marker-1234567890ABCDEFGHIJ";
        let mut points = three_point_sweep();
        // Replace machine_shape + workload_id with the planted
        // string in every point so the renderer must transport
        // them through both display paths.
        for p in &mut points {
            p.machine_shape = PLANTED.to_string();
            p.workload_id = PLANTED.to_string();
        }
        let report = plan(&points);
        let text = PlannerReportRenderer::new().render_text(&report);
        let json = serde_json::to_string(&PlannerReportRenderer::new().render(&report)).unwrap();
        let text_count = text.matches(PLANTED).count();
        let json_count = json.matches(PLANTED).count();
        // Each point contributes 2 occurrences (machine_shape +
        // workload_id). 3 points × 2 = 6 occurrences max in BOTH
        // paths. The frontier may exclude one point as dominated
        // so the lower bound is 4 (2 frontier × 2 fields). Renderer
        // must surface the SAME count in text and JSON — neither
        // path can diverge.
        assert!(
            (4..=12).contains(&text_count),
            "text rendering should surface 4-12 occurrences (2 fields × frontier+dominated points); got {text_count}"
        );
        assert_eq!(
            text_count, json_count,
            "br-ft-1650n.13: text and JSON paths must surface the same number of caller-supplied strings — got text={text_count} json={json_count}"
        );
    }

    #[test]
    fn render_text_data_needed_includes_substrate_threshold_message() {
        // Pin that the substrate's MIN_POINTS_FOR_FRONTIER message
        // appears in the rendering. A future renaming of the
        // threshold message would trip this and force a
        // coordinated update.
        let report = plan(&[three_point_sweep()[0].clone()]);
        let text = PlannerReportRenderer::new().render_text(&report);
        assert!(text.contains("verdict=data_needed"));
        assert!(text.contains("got 1"));
    }
}
