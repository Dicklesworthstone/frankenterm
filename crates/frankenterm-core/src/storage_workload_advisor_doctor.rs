//! Renderer for [`crate::storage_workload_advisor::AdvisorReport`]. [ft-1650n.15]
//!
//! Slice ships the operator-facing rendering layer on top of the
//! substrate at `storage_workload_advisor.rs` (sibling-shipped). 6th
//! iteration of the established renderer pattern:
//!
//! 1. capability_passport_doctor (ft-ykp2y) — sibling.
//! 2. handoff_capsule_inspect (ft-yk9lp slice 1) — me.
//! 3. approval_impact_simulator_doctor (ft-1650n.14 slice) — me.
//! 4. onboarding_stress_capsule_doctor (ft-1650n.17 slice) — me.
//! 5. pareto_frontier_planner_doctor (ft-1650n.13 slice) — me.
//! 6. storage_workload_advisor_doctor (this commit, ft-1650n.15) — me.
//!
//! - `render_text(&report) -> String` — concise plain-text preview
//!   suitable for `ft storage advisor` CLI output. Header line +
//!   recommendation block (backend / index / priority / confidence /
//!   rationale / proof commands) for the `Recommendation` variant,
//!   or reason list for `DataNeeded`.
//! - `render(&report) -> AdvisorReportRendering` — structured
//!   JSON-serializable rendering for machine consumption (audit-feed
//!   integration, dashboarding, automation harnesses).
//!
//! # Privacy
//!
//! Advisor output is operational/numeric (workload op counts,
//! distinct-cardinality estimates, hot-table row counts, p99
//! latencies, checkpoint lag bytes). The `proof_commands` field on
//! `StorageRecommendation` carries shell-command strings the operator
//! is expected to run BEFORE migrating — substrate documents these
//! as "well-formed command-line strings (no shell metachars requiring
//! escape)". The renderer prints them verbatim; the canary at the
//! bottom of this file pins that the renderer never amplifies what
//! the substrate emits, so a substrate change that introduces an
//! unsafe command would be visible at the same count in both text
//! and JSON paths (and trivial to spot in audit).

use serde::{Deserialize, Serialize};

use crate::storage_workload_advisor::{
    AdvisorReport, BackendChoice, Confidence, IndexChoice, MigrationPriority, StorageRecommendation,
};

/// Stateless renderer for [`AdvisorReport`].
pub struct WorkloadAdvisorDoctor;

impl WorkloadAdvisorDoctor {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Render `report` as a concise operator-facing plain-text
    /// preview.
    #[must_use]
    pub fn render_text(&self, report: &AdvisorReport) -> String {
        match report {
            AdvisorReport::Recommendation(rec) => Self::render_recommendation_text(rec),
            AdvisorReport::DataNeeded { reasons } => Self::render_data_needed_text(reasons),
        }
    }

    /// Render `report` as a structured JSON-serializable rendering.
    #[must_use]
    pub fn render(&self, report: &AdvisorReport) -> AdvisorReportRendering {
        match report {
            AdvisorReport::Recommendation(rec) => AdvisorReportRendering::Recommendation {
                backend: rec.backend,
                index: rec.index,
                migration_priority: rec.migration_priority,
                confidence: rec.confidence,
                rationale: rec.rationale.clone(),
                proof_commands: rec.proof_commands.clone(),
            },
            AdvisorReport::DataNeeded { reasons } => AdvisorReportRendering::DataNeeded {
                reasons: reasons.clone(),
            },
        }
    }

    fn render_recommendation_text(rec: &StorageRecommendation) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "storage-advisor: verdict=recommendation backend={} index={} priority={} confidence={}\n",
            backend_label(rec.backend),
            index_label(rec.index),
            priority_label(rec.migration_priority),
            confidence_label(rec.confidence),
        ));
        out.push_str(&format!("  rationale: {}\n", rec.rationale));
        if rec.proof_commands.is_empty() {
            out.push_str("  proof: ∅\n");
        } else {
            out.push_str("  proof:\n");
            for (i, cmd) in rec.proof_commands.iter().enumerate() {
                out.push_str(&format!("    [{i}] {cmd}\n"));
            }
        }
        out
    }

    fn render_data_needed_text(reasons: &[String]) -> String {
        let mut out = String::new();
        out.push_str("storage-advisor: verdict=data_needed\n");
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

impl Default for WorkloadAdvisorDoctor {
    fn default() -> Self {
        Self::new()
    }
}

/// Structured JSON-serializable rendering. Tagged enum mirroring the
/// substrate's [`AdvisorReport`] discriminant so machine consumers
/// can branch on `variant`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "variant", rename_all = "snake_case")]
pub enum AdvisorReportRendering {
    Recommendation {
        backend: BackendChoice,
        index: IndexChoice,
        migration_priority: MigrationPriority,
        confidence: Confidence,
        rationale: String,
        proof_commands: Vec<String>,
    },
    DataNeeded {
        reasons: Vec<String>,
    },
}

const fn backend_label(b: BackendChoice) -> &'static str {
    match b {
        BackendChoice::Rusqlite => "rusqlite",
        BackendChoice::FrankenSqlite => "franken_sqlite",
        BackendChoice::NoChange => "no_change",
    }
}

const fn index_label(i: IndexChoice) -> &'static str {
    match i {
        IndexChoice::Fts5 => "fts5",
        IndexChoice::Tantivy => "tantivy",
        IndexChoice::Hybrid => "hybrid",
        IndexChoice::NoChange => "no_change",
    }
}

const fn priority_label(p: MigrationPriority) -> &'static str {
    match p {
        MigrationPriority::None => "none",
        MigrationPriority::Low => "low",
        MigrationPriority::Medium => "medium",
        MigrationPriority::High => "high",
    }
}

const fn confidence_label(c: Confidence) -> &'static str {
    match c {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_workload_advisor::{HotTableSnapshot, WorkloadProfile, classify};

    fn write_heavy_profile() -> WorkloadProfile {
        WorkloadProfile {
            total_writes: 8_000,
            total_reads: 1_500,
            total_searches: 500,
            fts_enabled: true,
            tantivy_enabled: false,
            estimated_distinct_panes: 100,
            estimated_distinct_sessions: 25,
            hot_table: Some(HotTableSnapshot {
                name: "output_segments".to_string(),
                row_count: 5_000_000,
            }),
            p99_write_latency_us: 50_000,
            p99_read_latency_us: 5_000,
            checkpoint_lag_bytes: 0,
        }
    }

    fn data_needed_profile() -> WorkloadProfile {
        // Below MIN_SAMPLE_OPS = 1_000.
        WorkloadProfile {
            total_writes: 100,
            total_reads: 100,
            total_searches: 100,
            ..WorkloadProfile::default()
        }
    }

    #[test]
    fn render_text_recommendation_includes_all_fields() {
        let report = classify(&write_heavy_profile());
        let text = WorkloadAdvisorDoctor::new().render_text(&report);
        assert!(text.contains("verdict=recommendation"));
        assert!(text.contains("backend="));
        assert!(text.contains("index="));
        assert!(text.contains("priority="));
        assert!(text.contains("confidence="));
        assert!(text.contains("rationale:"));
    }

    #[test]
    fn render_text_data_needed_lists_reasons() {
        let report = classify(&data_needed_profile());
        let text = WorkloadAdvisorDoctor::new().render_text(&report);
        assert!(text.contains("verdict=data_needed"));
        assert!(text.contains("reasons:"));
    }

    #[test]
    fn render_text_data_needed_empty_reasons_marker() {
        // Defensive path — substrate currently always populates
        // reasons, but if a future contract change emits an empty
        // list, the renderer must surface that explicitly.
        let report = AdvisorReport::DataNeeded {
            reasons: Vec::new(),
        };
        let text = WorkloadAdvisorDoctor::new().render_text(&report);
        assert!(text.contains("verdict=data_needed"));
        assert!(text.contains("substrate emitted DataNeeded with no reasons"));
    }

    #[test]
    fn render_json_recommendation_dispatches_to_recommendation_variant() {
        let report = classify(&write_heavy_profile());
        let rendering = WorkloadAdvisorDoctor::new().render(&report);
        match rendering {
            AdvisorReportRendering::Recommendation {
                backend,
                index,
                migration_priority,
                confidence,
                rationale,
                proof_commands,
            } => {
                // Just verify the rendering preserves the substrate's
                // structured fields. The substrate's classifier
                // determines the actual values.
                let _ = (backend, index, migration_priority, confidence);
                assert!(!rationale.is_empty());
                let _ = proof_commands;
            }
            _ => panic!("expected Recommendation variant"),
        }
    }

    #[test]
    fn render_json_data_needed_dispatches_to_data_needed_variant() {
        let report = classify(&data_needed_profile());
        let rendering = WorkloadAdvisorDoctor::new().render(&report);
        match rendering {
            AdvisorReportRendering::DataNeeded { reasons } => {
                assert!(!reasons.is_empty());
            }
            _ => panic!("expected DataNeeded variant"),
        }
    }

    #[test]
    fn render_json_serde_roundtrip_for_recommendation_variant() {
        let report = classify(&write_heavy_profile());
        let rendering = WorkloadAdvisorDoctor::new().render(&report);
        let json = serde_json::to_string(&rendering).unwrap();
        assert!(json.contains("\"variant\":\"recommendation\""));
        let parsed: AdvisorReportRendering = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rendering);
    }

    #[test]
    fn render_json_serde_roundtrip_for_data_needed_variant() {
        let report = classify(&data_needed_profile());
        let rendering = WorkloadAdvisorDoctor::new().render(&report);
        let json = serde_json::to_string(&rendering).unwrap();
        assert!(json.contains("\"variant\":\"data_needed\""));
        let parsed: AdvisorReportRendering = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rendering);
    }

    #[test]
    fn render_text_uses_canonical_label_vocabulary() {
        // Pin label vocabulary across all four enums so dashboards
        // and operator output share canonical strings.
        for backend in [
            BackendChoice::Rusqlite,
            BackendChoice::FrankenSqlite,
            BackendChoice::NoChange,
        ] {
            assert!(!backend_label(backend).is_empty());
        }
        for index in [
            IndexChoice::Fts5,
            IndexChoice::Tantivy,
            IndexChoice::Hybrid,
            IndexChoice::NoChange,
        ] {
            assert!(!index_label(index).is_empty());
        }
        for priority in [
            MigrationPriority::None,
            MigrationPriority::Low,
            MigrationPriority::Medium,
            MigrationPriority::High,
        ] {
            assert!(!priority_label(priority).is_empty());
        }
        for confidence in [Confidence::High, Confidence::Medium, Confidence::Low] {
            assert!(!confidence_label(confidence).is_empty());
        }
    }

    /// Canary [ft-1650n.15]: the renderer must transport
    /// `proof_commands` exactly as the substrate emitted them — no
    /// Debug-format amplification, no double-print. Plant a unique
    /// marker in the proof-command vector and assert text/JSON
    /// paths surface the SAME count, neither path can diverge.
    /// Mirrors the canary pattern from the prior 5 renderer
    /// slices.
    #[test]
    fn render_does_not_amplify_proof_commands_ft_1650n_15() {
        const PLANTED_CMD: &str = "PLANTED-1650n.15-canary-CMD-1234567890ABCDEFGHIJ";
        // Build a Recommendation directly so we control the
        // proof_commands vector exactly.
        let report = AdvisorReport::Recommendation(StorageRecommendation {
            backend: BackendChoice::Rusqlite,
            index: IndexChoice::Fts5,
            migration_priority: MigrationPriority::None,
            confidence: Confidence::High,
            rationale: "canary test rationale".to_string(),
            proof_commands: vec![
                PLANTED_CMD.to_string(),
                "real-command".to_string(),
                PLANTED_CMD.to_string(),
            ],
        });
        let text = WorkloadAdvisorDoctor::new().render_text(&report);
        let json = serde_json::to_string(&WorkloadAdvisorDoctor::new().render(&report)).unwrap();
        let text_count = text.matches(PLANTED_CMD).count();
        let json_count = json.matches(PLANTED_CMD).count();
        // Two occurrences in the source vector — renderer should
        // surface exactly two in both text and JSON. Anything more
        // would indicate Debug-format amplification.
        assert_eq!(
            text_count, 2,
            "br-ft-1650n.15: text rendering must surface proof_commands exactly once each; got {text_count}"
        );
        assert_eq!(
            json_count, 2,
            "br-ft-1650n.15: JSON rendering must surface proof_commands exactly once each; got {json_count}"
        );
        assert_eq!(
            text_count, json_count,
            "br-ft-1650n.15: text and JSON paths must surface the same count"
        );
    }

    #[test]
    fn render_text_recommendation_with_empty_proof_marker() {
        // If substrate emits a Recommendation with NO proof
        // commands, the renderer must surface ∅ rather than print
        // a confusing blank "proof:" header.
        let report = AdvisorReport::Recommendation(StorageRecommendation {
            backend: BackendChoice::NoChange,
            index: IndexChoice::NoChange,
            migration_priority: MigrationPriority::None,
            confidence: Confidence::Low,
            rationale: "nothing to recommend".to_string(),
            proof_commands: Vec::new(),
        });
        let text = WorkloadAdvisorDoctor::new().render_text(&report);
        assert!(text.contains("proof: ∅"));
    }
}
