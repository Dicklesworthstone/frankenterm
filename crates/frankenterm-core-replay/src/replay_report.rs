//! Decision-diff report generation for humans and robot-mode consumers (ft-og6q6.5.4).
//!
//! Provides:
//! - [`ReportFormat`] — Human, Json, Markdown, Csv.
//! - [`ReportGenerator`] — Generates reports from [`DecisionDiff`] + [`AggregateRisk`].
//! - [`ReportMeta`] — Metadata included in every report.

use serde::{Deserialize, Serialize};

use crate::replay_decision_diff::{DecisionDiff, Divergence, DivergenceType};
use crate::replay_risk_scoring::{AggregateRisk, DivergenceSeverity, Recommendation, RiskScorer};

// ============================================================================
// ReportFormat — output format selector
// ============================================================================

/// Report output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReportFormat {
    /// Colored terminal output for humans.
    Human,
    /// Machine-readable JSON for CI/robot consumption.
    Json,
    /// Markdown for PR comments.
    Markdown,
    /// CSV for spreadsheet analysis.
    Csv,
}

// ============================================================================
// ReportMeta — report metadata
// ============================================================================

/// Metadata included in every report.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReportMeta {
    /// Replay run ID.
    #[serde(default)]
    pub replay_run_id: String,
    /// Artifact path.
    #[serde(default)]
    pub artifact_path: String,
    /// Override path (if any).
    #[serde(default)]
    pub override_path: String,
    /// Replay duration in ms.
    #[serde(default)]
    pub replay_duration_ms: u64,
    /// Total events processed.
    #[serde(default)]
    pub total_events: u64,
    /// ISO-8601 timestamp.
    #[serde(default)]
    pub timestamp: String,
}

// ============================================================================
// JsonReport — structured report for CI gate
// ============================================================================

/// Structured JSON report matching gate-report schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonReport {
    pub replay_run_id: String,
    pub artifact_path: String,
    pub override_path: String,
    pub equivalence_level: String,
    pub pass: bool,
    pub recommendation: String,
    pub divergences: Vec<JsonDivergence>,
    pub risk_summary: JsonRiskSummary,
    pub timestamp: String,
}

/// Single divergence in JSON report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonDivergence {
    pub position: u64,
    pub divergence_type: String,
    pub severity: String,
    pub rule_id: String,
    pub root_cause: String,
    pub baseline_output: String,
    pub candidate_output: String,
}

/// Risk summary in JSON report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRiskSummary {
    pub max_severity: String,
    pub total_risk_score: u64,
    pub critical_count: u64,
    pub high_count: u64,
    pub medium_count: u64,
    pub low_count: u64,
    pub info_count: u64,
}

// ============================================================================
// ReportGenerator — generates reports in all formats
// ============================================================================

/// Maximum divergences to show in detail for Human and Markdown formats.
const MAX_DETAIL_DIVERGENCES: usize = 20;

/// Maximum total divergences before truncation.
const TRUNCATION_THRESHOLD: usize = 50;

/// Generates reports from decision diffs.
pub struct ReportGenerator {
    meta: ReportMeta,
    scorer: RiskScorer,
}

impl ReportGenerator {
    /// Create a generator with metadata and default scorer.
    #[must_use]
    pub fn new(meta: ReportMeta) -> Self {
        Self {
            meta,
            scorer: RiskScorer::new(),
        }
    }

    /// Create a generator with custom scorer.
    #[must_use]
    pub fn with_scorer(meta: ReportMeta, scorer: RiskScorer) -> Self {
        Self { meta, scorer }
    }

    /// Generate a report in the specified format.
    #[must_use]
    pub fn generate(&self, diff: &DecisionDiff, format: ReportFormat) -> String {
        let risk = self.scorer.aggregate(&diff.divergences);
        match format {
            ReportFormat::Human => self.render_human(diff, &risk),
            ReportFormat::Json => self.render_json(diff, &risk),
            ReportFormat::Markdown => self.render_markdown(diff, &risk),
            ReportFormat::Csv => self.render_csv(diff, &risk),
        }
    }

    // ── Human format ───────────────────────────────────────────────────

    fn render_human(&self, diff: &DecisionDiff, risk: &AggregateRisk) -> String {
        let mut out = String::new();

        // Header.
        out.push_str("=== Replay Decision-Diff Report ===\n\n");
        if !self.meta.artifact_path.is_empty() {
            out.push_str(&format!("Artifact:  {}\n", self.meta.artifact_path));
        }
        if !self.meta.override_path.is_empty() {
            out.push_str(&format!("Override:  {}\n", self.meta.override_path));
        }
        if self.meta.replay_duration_ms > 0 {
            out.push_str(&format!("Duration:  {}ms\n", self.meta.replay_duration_ms));
        }
        if self.meta.total_events > 0 {
            out.push_str(&format!("Events:    {}\n", self.meta.total_events));
        }
        out.push('\n');

        // Summary table.
        out.push_str("--- Divergence Summary ---\n");
        out.push_str(&format!("Critical: {}\n", risk.critical_count));
        out.push_str(&format!("High:     {}\n", risk.high_count));
        out.push_str(&format!("Medium:   {}\n", risk.medium_count));
        out.push_str(&format!("Low:      {}\n", risk.low_count));
        out.push_str(&format!("Info:     {}\n", risk.info_count));
        out.push_str(&format!(
            "Total:    {} ({} unchanged)\n",
            diff.summary.total_divergences(),
            diff.summary.unchanged
        ));
        out.push('\n');

        // Divergence details (sorted by severity, truncated).
        let sorted = self.sort_by_severity(diff);
        let show_count = sorted.len().min(MAX_DETAIL_DIVERGENCES);

        if !sorted.is_empty() {
            out.push_str("--- Divergence Details ---\n");
            for (i, (div, sev)) in sorted.iter().take(show_count).enumerate() {
                let rule_id = extract_rule_id(div);
                out.push_str(&format!(
                    "  [{:>2}] [{:?}] {} — {} (pos {})\n",
                    i + 1,
                    sev,
                    format_divergence_type(div.divergence_type),
                    rule_id,
                    div.position,
                ));
                out.push_str(&format!("       Root cause: {}\n", format_root_cause(div)));
            }
            if sorted.len() > MAX_DETAIL_DIVERGENCES {
                out.push_str(&format!(
                    "  ... and {} more divergences\n",
                    sorted.len() - MAX_DETAIL_DIVERGENCES
                ));
            }
            out.push('\n');
        }

        // Recommendation.
        out.push_str("--- Recommendation ---\n");
        out.push_str(&format!("{}\n", format_recommendation(risk.recommendation)));
        out
    }

    // ── JSON format ────────────────────────────────────────────────────

    fn render_json(&self, diff: &DecisionDiff, risk: &AggregateRisk) -> String {
        let sorted = self.sort_by_severity(diff);
        let divergences: Vec<JsonDivergence> = sorted
            .iter()
            .map(|(div, sev)| JsonDivergence {
                position: div.position,
                divergence_type: format_divergence_type(div.divergence_type),
                severity: format!("{:?}", sev),
                rule_id: extract_rule_id(div),
                root_cause: format_root_cause(div),
                baseline_output: div
                    .baseline_node
                    .as_ref()
                    .map(|n| n.output_hash.clone())
                    .unwrap_or_default(),
                candidate_output: div
                    .candidate_node
                    .as_ref()
                    .map(|n| n.output_hash.clone())
                    .unwrap_or_default(),
            })
            .collect();

        let report = JsonReport {
            replay_run_id: self.meta.replay_run_id.clone(),
            artifact_path: self.meta.artifact_path.clone(),
            override_path: self.meta.override_path.clone(),
            equivalence_level: if diff.summary.is_empty() {
                "L2".into()
            } else if diff.summary.added == 0
                && diff.summary.removed == 0
                && diff.summary.modified == 0
            {
                "L1".into()
            } else if diff.summary.added == 0 && diff.summary.removed == 0 {
                "L0".into()
            } else {
                "NONE".into()
            },
            pass: risk.recommendation == Recommendation::Pass,
            recommendation: format!("{:?}", risk.recommendation),
            divergences,
            risk_summary: JsonRiskSummary {
                max_severity: format!("{:?}", risk.max_severity),
                total_risk_score: risk.total_risk_score,
                critical_count: risk.critical_count,
                high_count: risk.high_count,
                medium_count: risk.medium_count,
                low_count: risk.low_count,
                info_count: risk.info_count,
            },
            timestamp: self.meta.timestamp.clone(),
        };

        serde_json::to_string_pretty(&report).unwrap_or_default()
    }

    // ── Markdown format ────────────────────────────────────────────────

    fn render_markdown(&self, diff: &DecisionDiff, risk: &AggregateRisk) -> String {
        let mut out = String::new();

        out.push_str("# Replay Decision-Diff Report\n\n");

        // Metadata table.
        out.push_str("| Field | Value |\n|---|---|\n");
        if !self.meta.artifact_path.is_empty() {
            out.push_str(&format!("| Artifact | `{}` |\n", self.meta.artifact_path));
        }
        if !self.meta.override_path.is_empty() {
            out.push_str(&format!("| Override | `{}` |\n", self.meta.override_path));
        }
        if self.meta.replay_duration_ms > 0 {
            out.push_str(&format!(
                "| Duration | {}ms |\n",
                self.meta.replay_duration_ms
            ));
        }
        out.push('\n');

        // Summary.
        out.push_str("## Summary\n\n");
        out.push_str(&format!(
            "**Recommendation: {}**\n\n",
            format_recommendation(risk.recommendation)
        ));
        out.push_str("| Severity | Count |\n|---|---|\n");
        out.push_str(&format!("| Critical | {} |\n", risk.critical_count));
        out.push_str(&format!("| High | {} |\n", risk.high_count));
        out.push_str(&format!("| Medium | {} |\n", risk.medium_count));
        out.push_str(&format!("| Low | {} |\n", risk.low_count));
        out.push_str(&format!("| Info | {} |\n", risk.info_count));
        out.push('\n');

        // Divergences.
        let sorted = self.sort_by_severity(diff);
        if !sorted.is_empty() {
            out.push_str("## Divergences\n\n");

            let show_count = sorted.len().min(MAX_DETAIL_DIVERGENCES);
            out.push_str("| # | Severity | Type | Rule | Root Cause |\n");
            out.push_str("|---|---|---|---|---|\n");
            for (i, (div, sev)) in sorted.iter().take(show_count).enumerate() {
                out.push_str(&format!(
                    "| {} | {:?} | {} | `{}` | {} |\n",
                    i + 1,
                    sev,
                    format_divergence_type(div.divergence_type),
                    extract_rule_id(div),
                    format_root_cause(div),
                ));
            }

            if sorted.len() > TRUNCATION_THRESHOLD {
                out.push_str(&format!(
                    "\n<details><summary>... and {} more divergences</summary>\n\n",
                    sorted.len() - MAX_DETAIL_DIVERGENCES
                ));
                out.push_str("See full JSON report for complete divergence list.\n");
                out.push_str("</details>\n");
            }
        } else {
            out.push_str("## Divergences\n\nNo divergences found.\n");
        }

        out
    }

    // ── CSV format ─────────────────────────────────────────────────────

    fn render_csv(&self, diff: &DecisionDiff, _risk: &AggregateRisk) -> String {
        let mut out = String::new();
        out.push_str("position,divergence_type,severity,rule_id,root_cause,baseline_output,candidate_output\n");

        let sorted = self.sort_by_severity(diff);
        for (div, sev) in &sorted {
            let baseline_output = div
                .baseline_node
                .as_ref()
                .map(|n| n.output_hash.clone())
                .unwrap_or_default();
            let candidate_output = div
                .candidate_node
                .as_ref()
                .map(|n| n.output_hash.clone())
                .unwrap_or_default();
            out.push_str(&format!(
                "{},{},{:?},{},{},{},{}\n",
                div.position,
                format_divergence_type(div.divergence_type),
                sev,
                extract_rule_id(div),
                format_root_cause(div),
                baseline_output,
                candidate_output,
            ));
        }
        out
    }

    // ── Helpers ────────────────────────────────────────────────────────

    fn sort_by_severity<'a>(
        &self,
        diff: &'a DecisionDiff,
    ) -> Vec<(&'a Divergence, DivergenceSeverity)> {
        let mut scored: Vec<(&Divergence, DivergenceSeverity)> = diff
            .divergences
            .iter()
            .map(|d| {
                let score = self.scorer.score(d, 0);
                (d, score.severity)
            })
            .collect();
        // Sort by severity descending, then position ascending.
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.position.cmp(&b.0.position)));
        scored
    }
}

// ── Formatting helpers ─────────────────────────────────────────────────

fn format_divergence_type(dt: DivergenceType) -> String {
    match dt {
        DivergenceType::Added => "Added".into(),
        DivergenceType::Removed => "Removed".into(),
        DivergenceType::Modified => "Modified".into(),
        DivergenceType::Shifted => "Shifted".into(),
    }
}

fn extract_rule_id(div: &Divergence) -> String {
    div.baseline_node
        .as_ref()
        .or(div.candidate_node.as_ref())
        .map(|n| n.rule_id.clone())
        .unwrap_or_else(|| "unknown".into())
}

fn format_root_cause(div: &Divergence) -> String {
    use crate::replay_decision_diff::RootCause;
    match &div.root_cause {
        RootCause::RuleDefinitionChange { rule_id, .. } => {
            format!("Rule definition changed: {}", rule_id)
        }
        RootCause::InputDivergence {
            upstream_rule_id, ..
        } => {
            format!("Input diverged from: {}", upstream_rule_id)
        }
        RootCause::OverrideApplied {
            rule_id,
            override_id,
        } => {
            format!("Override applied: {} -> {}", override_id, rule_id)
        }
        RootCause::NewDecision { rule_id } => format!("New decision: {}", rule_id),
        RootCause::DroppedDecision { rule_id } => format!("Decision dropped: {}", rule_id),
        RootCause::TimingShift { delta_ms, .. } => format!("Timing shift: {}ms", delta_ms),
        RootCause::Unknown => "Unknown".into(),
    }
}

fn format_recommendation(rec: Recommendation) -> String {
    match rec {
        Recommendation::Pass => "PASS — No actionable divergences".into(),
        Recommendation::Review => "REVIEW — Medium divergences require human review".into(),
        Recommendation::Block => "BLOCK — Critical/High divergences prevent merge".into(),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_decision_diff::{DecisionDiff, DiffConfig};
    use crate::replay_decision_graph::{DecisionEvent, DecisionGraph, DecisionType};

    fn make_event(rule_id: &str, ts: u64, pane: u64, def: &str, out: &str) -> DecisionEvent {
        let input = format!("rule={rule_id};ts={ts};pane={pane}");
        DecisionEvent::new(
            DecisionType::PatternMatch,
            pane,
            rule_id,
            def,
            &input,
            serde_json::Value::String(out.into()),
            None,
            Some(1.0),
            ts,
        )
    }

    fn sample_meta() -> ReportMeta {
        ReportMeta {
            replay_run_id: "run_001".into(),
            artifact_path: "trace_a.ftreplay".into(),
            override_path: "strict.ftoverride".into(),
            replay_duration_ms: 5000,
            total_events: 1000,
            timestamp: "2026-02-24T12:00:00Z".into(),
        }
    }

    fn sample_diff() -> DecisionDiff {
        let base_events = vec![
            make_event("rule_a", 100, 1, "def1", "out1"),
            make_event("wf_deploy", 200, 1, "def2", "out2"),
            make_event("pol_auth", 300, 1, "def3", "out3"),
        ];
        let cand_events = vec![
            make_event("rule_a", 100, 1, "def1", "out1"), // unchanged
            make_event("wf_deploy", 200, 1, "def2", "out_mod"), // modified
            make_event("pol_auth", 300, 1, "def3_v2", "out_mod2"), // modified (def change)
        ];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        DecisionDiff::diff(&base, &cand, &DiffConfig::default())
    }

    fn empty_diff() -> DecisionDiff {
        let events = vec![make_event("r1", 100, 1, "d", "o")];
        let graph = DecisionGraph::from_decisions(&events);
        DecisionDiff::diff(&graph, &graph, &DiffConfig::default())
    }

    // ── Human format ───────────────────────────────────────────────────

    #[test]
    fn human_format_contains_header() {
        let generator = ReportGenerator::new(sample_meta());
        let report = generator.generate(&sample_diff(), ReportFormat::Human);
        assert!(report.contains("Replay Decision-Diff Report"));
        assert!(report.contains("trace_a.ftreplay"));
    }

    #[test]
    fn human_format_contains_summary() {
        let generator = ReportGenerator::new(sample_meta());
        let report = generator.generate(&sample_diff(), ReportFormat::Human);
        assert!(report.contains("Divergence Summary"));
    }

    #[test]
    fn human_format_contains_recommendation() {
        let generator = ReportGenerator::new(sample_meta());
        let report = generator.generate(&sample_diff(), ReportFormat::Human);
        assert!(report.contains("Recommendation"));
    }

    #[test]
    fn human_format_empty_diff() {
        let generator = ReportGenerator::new(sample_meta());
        let report = generator.generate(&empty_diff(), ReportFormat::Human);
        assert!(report.contains("PASS"));
    }

    // ── JSON format ────────────────────────────────────────────────────

    #[test]
    fn json_format_valid() {
        let generator = ReportGenerator::new(sample_meta());
        let report = generator.generate(&sample_diff(), ReportFormat::Json);
        let parsed: JsonReport = serde_json::from_str(&report).unwrap();
        assert_eq!(parsed.replay_run_id, "run_001");
    }

    #[test]
    fn json_format_gate_fields() {
        let generator = ReportGenerator::new(sample_meta());
        let report = generator.generate(&sample_diff(), ReportFormat::Json);
        let parsed: JsonReport = serde_json::from_str(&report).unwrap();
        assert!(!parsed.equivalence_level.is_empty());
        assert!(!parsed.recommendation.is_empty());
        assert!(!parsed.divergences.is_empty());
    }

    #[test]
    fn json_format_empty_diff_pass() {
        let generator = ReportGenerator::new(sample_meta());
        let report = generator.generate(&empty_diff(), ReportFormat::Json);
        let parsed: JsonReport = serde_json::from_str(&report).unwrap();
        assert!(parsed.pass);
        assert_eq!(parsed.equivalence_level, "L2");
    }

    #[test]
    fn json_format_risk_summary() {
        let generator = ReportGenerator::new(sample_meta());
        let report = generator.generate(&sample_diff(), ReportFormat::Json);
        let parsed: JsonReport = serde_json::from_str(&report).unwrap();
        let total = parsed.risk_summary.critical_count
            + parsed.risk_summary.high_count
            + parsed.risk_summary.medium_count
            + parsed.risk_summary.low_count
            + parsed.risk_summary.info_count;
        assert_eq!(total as usize, parsed.divergences.len());
    }

    // ── Markdown format ────────────────────────────────────────────────

    #[test]
    fn markdown_format_valid() {
        let generator = ReportGenerator::new(sample_meta());
        let report = generator.generate(&sample_diff(), ReportFormat::Markdown);
        assert!(report.starts_with("# Replay Decision-Diff Report"));
    }

    #[test]
    fn markdown_format_tables() {
        let generator = ReportGenerator::new(sample_meta());
        let report = generator.generate(&sample_diff(), ReportFormat::Markdown);
        assert!(report.contains("| Severity | Count |"));
    }

    #[test]
    fn markdown_format_empty_diff() {
        let generator = ReportGenerator::new(sample_meta());
        let report = generator.generate(&empty_diff(), ReportFormat::Markdown);
        assert!(report.contains("No divergences found"));
    }

    // ── CSV format ─────────────────────────────────────────────────────

    #[test]
    fn csv_format_headers() {
        let generator = ReportGenerator::new(sample_meta());
        let report = generator.generate(&sample_diff(), ReportFormat::Csv);
        assert!(report.starts_with(
            "position,divergence_type,severity,rule_id,root_cause,baseline_output,candidate_output"
        ));
    }

    #[test]
    fn csv_format_line_count() {
        let generator = ReportGenerator::new(sample_meta());
        let diff = sample_diff();
        let report = generator.generate(&diff, ReportFormat::Csv);
        // Header + one line per divergence.
        assert_eq!(report.lines().count(), 1 + diff.divergences.len());
    }

    #[test]
    fn csv_format_empty_diff() {
        let generator = ReportGenerator::new(sample_meta());
        let report = generator.generate(&empty_diff(), ReportFormat::Csv);
        assert_eq!(report.lines().count(), 1); // Header only.
    }

    // ── All formats produce non-empty output ───────────────────────────

    #[test]
    fn all_formats_nonempty() {
        let generator = ReportGenerator::new(sample_meta());
        let diff = sample_diff();
        for format in &[
            ReportFormat::Human,
            ReportFormat::Json,
            ReportFormat::Markdown,
            ReportFormat::Csv,
        ] {
            let report = generator.generate(&diff, *format);
            assert!(
                !report.is_empty(),
                "format {:?} should produce non-empty output",
                format
            );
        }
    }

    // ── Serde roundtrips ───────────────────────────────────────────────

    #[test]
    fn report_format_serde() {
        let f = ReportFormat::Markdown;
        let json = serde_json::to_string(&f).unwrap();
        let restored: ReportFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, f);
    }

    #[test]
    fn report_meta_serde() {
        let meta = sample_meta();
        let json = serde_json::to_string(&meta).unwrap();
        let restored: ReportMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.replay_run_id, "run_001");
    }

    #[test]
    fn json_report_serde() {
        let generator = ReportGenerator::new(sample_meta());
        let report_str = generator.generate(&sample_diff(), ReportFormat::Json);
        let parsed: JsonReport = serde_json::from_str(&report_str).unwrap();
        let re_json = serde_json::to_string_pretty(&parsed).unwrap();
        let re_parsed: JsonReport = serde_json::from_str(&re_json).unwrap();
        assert_eq!(re_parsed.replay_run_id, parsed.replay_run_id);
    }

    // ── Human report: Critical first ───────────────────────────────────

    #[test]
    fn human_critical_first() {
        let generator = ReportGenerator::new(sample_meta());
        let report = generator.generate(&sample_diff(), ReportFormat::Human);
        // Find positions of "Critical" and other severities in the detail section.
        if let Some(details_start) = report.find("Divergence Details") {
            let details = &report[details_start..];
            if let Some(crit_pos) = details.find("Critical") {
                if let Some(medium_pos) = details.find("Medium") {
                    assert!(
                        crit_pos < medium_pos,
                        "Critical should appear before Medium"
                    );
                }
            }
        }
    }

    // ── Large diff truncation ──────────────────────────────────────────

    #[test]
    fn large_diff_truncation() {
        // Build a diff with >50 divergences.
        let base_events: Vec<DecisionEvent> = (0..60)
            .map(|i| make_event(&format!("rule_{}", i), i * 10, 0, "def", "out"))
            .collect();
        let cand_events: Vec<DecisionEvent> = (0..60)
            .map(|i| make_event(&format!("rule_{}", i), i * 10, 0, "def", "out_mod"))
            .collect();
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());

        let generator = ReportGenerator::new(sample_meta());
        let report = generator.generate(&diff, ReportFormat::Human);
        assert!(report.contains("and "));
        assert!(report.contains("more divergences"));
    }

    // ── Default meta ───────────────────────────────────────────────────

    #[test]
    fn default_meta() {
        let meta = ReportMeta::default();
        assert!(meta.replay_run_id.is_empty());
        assert!(meta.artifact_path.is_empty());
    }

    // ── Custom scorer ──────────────────────────────────────────────────

    #[test]
    fn custom_scorer() {
        use crate::replay_risk_scoring::{SeverityConfig, SeverityRule};
        let config = SeverityConfig {
            rules: vec![SeverityRule {
                decision_type: None,
                rule_id_pattern: Some("*".into()),
                severity: DivergenceSeverity::Info,
            }],
        };
        let scorer = RiskScorer::with_config(config);
        let generator = ReportGenerator::with_scorer(sample_meta(), scorer);
        let report = generator.generate(&sample_diff(), ReportFormat::Json);
        let parsed: JsonReport = serde_json::from_str(&report).unwrap();
        assert!(parsed.pass); // All Info → Pass.
    }
}
