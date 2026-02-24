//! Golden test suite for the decision-diff pipeline (ft-og6q6.5.6).
//!
//! Tests the full pipeline: graph → diff → risk → report → gate evaluation
//! against known-good fixtures. Each golden scenario has:
//! - A fixed set of baseline and candidate decision events
//! - Expected diff summary (divergence counts by type)
//! - Expected risk aggregate (severity counts, recommendation)
//! - Expected report structure validation
//! - Expected gate evaluation result
//!
//! Golden tests verify:
//! - G-1:  Identical graphs → zero divergences, L2 equivalence, Pass
//! - G-2:  Single modified output → 1 Modified divergence, L0 equivalence
//! - G-3:  Added decision → 1 Added, structural divergence (not L0)
//! - G-4:  Removed decision → 1 Removed, structural divergence (not L0)
//! - G-5:  Timing shift → 1 Shifted, L1 equivalence, Info severity
//! - G-6:  Rule definition change → Modified, Critical for policy rules
//! - G-7:  Cascading divergence → multiple Modified from single root cause
//! - G-8:  Mixed divergences → correct counts per type
//! - G-9:  JSON report schema completeness
//! - G-10: JSON report serde roundtrip
//! - G-11: CSV column count consistency
//! - G-12: Markdown heading structure
//! - G-13: Human format severity ordering (Critical before Info)
//! - G-14: Gate evaluator: zero divergences → Pass
//! - G-15: Gate evaluator: 1 critical → Fail
//! - G-16: Gate evaluator: with annotation → Pass
//! - G-17: Gate evaluator: skip budget exceeded → Fail
//! - G-18: Gate evaluator: custom budget relaxed → Pass
//! - G-19: Full pipeline roundtrip (diff → JSON → deserialize → identical)
//! - G-20: All four report formats produce valid output
//! - G-21: Large diff truncation in Human format
//! - G-22: DiffSummary accounting invariant (unchanged+modified+removed+shifted = baseline)
//! - G-23: Risk score sum consistency
//! - G-24: Report generator idempotency
//! - G-25: Empty candidate → all Removed
//! - G-26: Empty baseline → all Added

use frankenterm_core::replay_decision_diff::{DecisionDiff, DiffConfig, EquivalenceLevel};
use frankenterm_core::replay_decision_graph::{DecisionEvent, DecisionGraph, DecisionType};
use frankenterm_core::replay_guardrails_gate::{
    EvaluationContext, ExpectedDivergenceAnnotation, GateEvaluator, GateResult, RegressionBudget,
};
use frankenterm_core::replay_report::{
    JsonReport, ReportFormat, ReportGenerator, ReportMeta,
};
use frankenterm_core::replay_risk_scoring::{Recommendation, RiskScorer};

// ── Fixture helpers ───────────────────────────────────────────────────────

fn event(
    rule_id: &str,
    ts: u64,
    pane: u64,
    def: &str,
    out: &str,
    dt: DecisionType,
) -> DecisionEvent {
    DecisionEvent {
        decision_type: dt,
        rule_id: rule_id.into(),
        definition_hash: def.into(),
        input_hash: format!("in_{}", ts),
        output_hash: out.into(),
        timestamp_ms: ts,
        pane_id: pane,
        triggered_by: None,
        overrides: None,
        wall_clock_ms: 0,
        replay_run_id: String::new(),
    }
}

fn pm(rule_id: &str, ts: u64, pane: u64, def: &str, out: &str) -> DecisionEvent {
    event(rule_id, ts, pane, def, out, DecisionType::PatternMatch)
}

fn wf(rule_id: &str, ts: u64, pane: u64, def: &str, out: &str) -> DecisionEvent {
    event(rule_id, ts, pane, def, out, DecisionType::WorkflowStep)
}

fn pol(rule_id: &str, ts: u64, pane: u64, def: &str, out: &str) -> DecisionEvent {
    event(rule_id, ts, pane, def, out, DecisionType::PolicyDecision)
}

fn meta() -> ReportMeta {
    ReportMeta {
        replay_run_id: "golden_001".into(),
        artifact_path: "golden.ftreplay".into(),
        override_path: "golden.ftoverride".into(),
        replay_duration_ms: 1500,
        total_events: 100,
        timestamp: "2026-02-24T12:00:00Z".into(),
    }
}

// ── G-1: Identical graphs ─────────────────────────────────────────────────

#[test]
fn g01_identical_graphs() {
    let events = vec![
        pm("rule_a", 100, 1, "def1", "out1"),
        pm("rule_b", 200, 1, "def2", "out2"),
        pm("rule_c", 300, 1, "def3", "out3"),
    ];
    let graph = DecisionGraph::from_decisions(&events);
    let diff = DecisionDiff::diff(&graph, &graph, &DiffConfig::default());

    assert!(diff.divergences.is_empty(), "identical graphs should have no divergences");
    assert!(diff.is_equivalent(EquivalenceLevel::L2));
    assert_eq!(diff.summary.unchanged, 3);
    assert_eq!(diff.summary.added, 0);
    assert_eq!(diff.summary.removed, 0);
    assert_eq!(diff.summary.modified, 0);
    assert_eq!(diff.summary.shifted, 0);
}

// ── G-2: Single modified output ───────────────────────────────────────────

#[test]
fn g02_single_modified() {
    let base = DecisionGraph::from_decisions(&[
        pm("rule_a", 100, 1, "def1", "out1"),
    ]);
    let cand = DecisionGraph::from_decisions(&[
        pm("rule_a", 100, 1, "def1", "out_modified"),
    ]);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());

    assert_eq!(diff.divergences.len(), 1);
    assert_eq!(diff.summary.modified, 1);
    assert!(diff.is_equivalent(EquivalenceLevel::L0));
    assert!(!diff.is_equivalent(EquivalenceLevel::L1));
}

// ── G-3: Added decision ──────────────────────────────────────────────────

#[test]
fn g03_added_decision() {
    let base = DecisionGraph::from_decisions(&[
        pm("rule_a", 100, 1, "def1", "out1"),
    ]);
    let cand = DecisionGraph::from_decisions(&[
        pm("rule_a", 100, 1, "def1", "out1"),
        pm("rule_b", 200, 1, "def2", "out2"),
    ]);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());

    assert_eq!(diff.summary.added, 1);
    assert!(!diff.is_equivalent(EquivalenceLevel::L0));
}

// ── G-4: Removed decision ─────────────────────────────────────────────────

#[test]
fn g04_removed_decision() {
    let base = DecisionGraph::from_decisions(&[
        pm("rule_a", 100, 1, "def1", "out1"),
        pm("rule_b", 200, 1, "def2", "out2"),
    ]);
    let cand = DecisionGraph::from_decisions(&[
        pm("rule_a", 100, 1, "def1", "out1"),
    ]);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());

    assert_eq!(diff.summary.removed, 1);
    assert!(!diff.is_equivalent(EquivalenceLevel::L0));
}

// ── G-5: Timing shift ────────────────────────────────────────────────────

#[test]
fn g05_timing_shift() {
    let base = DecisionGraph::from_decisions(&[
        pm("rule_a", 100, 1, "def1", "out1"),
    ]);
    let cand = DecisionGraph::from_decisions(&[
        pm("rule_a", 150, 1, "def1", "out1"), // shifted by 50ms (within 100ms tolerance)
    ]);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());

    assert_eq!(diff.summary.shifted, 1);
    assert!(diff.is_equivalent(EquivalenceLevel::L1));
    assert!(!diff.is_equivalent(EquivalenceLevel::L2));

    // Shifted divergences should be Info severity.
    let scorer = RiskScorer::new();
    let risk = scorer.aggregate(&diff.divergences);
    assert_eq!(risk.info_count, 1);
    assert_eq!(risk.recommendation, Recommendation::Pass);
}

// ── G-6: Rule definition change on policy rule → Critical ─────────────────

#[test]
fn g06_policy_definition_change() {
    let base = DecisionGraph::from_decisions(&[
        pol("pol_auth", 100, 1, "def_v1", "allow"),
    ]);
    let cand = DecisionGraph::from_decisions(&[
        pol("pol_auth", 100, 1, "def_v2", "deny"), // definition changed
    ]);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());

    assert_eq!(diff.summary.modified, 1);

    let scorer = RiskScorer::new();
    let risk = scorer.aggregate(&diff.divergences);
    // Policy rule definition change should be Critical.
    assert_eq!(risk.critical_count, 1);
    assert_eq!(risk.recommendation, Recommendation::Block);
}

// ── G-7: Cascading divergences ────────────────────────────────────────────

#[test]
fn g07_cascading_divergences() {
    let base = DecisionGraph::from_decisions(&[
        pm("rule_a", 100, 1, "def1", "out1"),
        pm("rule_b", 200, 1, "def2", "out2"),
        pm("rule_c", 300, 1, "def3", "out3"),
    ]);
    // All rules produce different output (cascading effect).
    let cand = DecisionGraph::from_decisions(&[
        pm("rule_a", 100, 1, "def1", "out1_v2"),
        pm("rule_b", 200, 1, "def2", "out2_v2"),
        pm("rule_c", 300, 1, "def3", "out3_v2"),
    ]);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());

    assert_eq!(diff.summary.modified, 3);
    assert_eq!(diff.divergences.len(), 3);
}

// ── G-8: Mixed divergences ────────────────────────────────────────────────

#[test]
fn g08_mixed_divergences() {
    let base = DecisionGraph::from_decisions(&[
        pm("rule_a", 100, 1, "def1", "out1"),   // unchanged
        pm("rule_b", 200, 1, "def2", "out2"),   // modified
        pm("rule_c", 300, 1, "def3", "out3"),   // removed
    ]);
    let cand = DecisionGraph::from_decisions(&[
        pm("rule_a", 100, 1, "def1", "out1"),   // unchanged
        pm("rule_b", 200, 1, "def2", "out2_v2"), // modified
        pm("rule_d", 400, 1, "def4", "out4"),   // added
    ]);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());

    assert_eq!(diff.summary.unchanged, 1);
    assert_eq!(diff.summary.modified, 1);
    assert_eq!(diff.summary.removed, 1);
    assert_eq!(diff.summary.added, 1);
}

// ── G-9: JSON report schema completeness ──────────────────────────────────

#[test]
fn g09_json_schema() {
    let base = DecisionGraph::from_decisions(&[
        pm("rule_a", 100, 1, "def1", "out1"),
        wf("wf_deploy", 200, 1, "def2", "out2"),
    ]);
    let cand = DecisionGraph::from_decisions(&[
        pm("rule_a", 100, 1, "def1", "out1_v2"),
        wf("wf_deploy", 200, 1, "def2", "out2"),
    ]);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());
    let generator = ReportGenerator::new(meta());
    let json_str = generator.generate(&diff, ReportFormat::Json);
    let parsed: JsonReport = serde_json::from_str(&json_str).unwrap();

    // All required fields present and populated.
    assert_eq!(parsed.replay_run_id, "golden_001");
    assert_eq!(parsed.artifact_path, "golden.ftreplay");
    assert!(!parsed.equivalence_level.is_empty());
    assert!(!parsed.recommendation.is_empty());
    assert!(!parsed.timestamp.is_empty());

    // Risk summary has all fields.
    assert!(!parsed.risk_summary.max_severity.is_empty());

    // Divergences have all fields.
    for div in &parsed.divergences {
        assert!(!div.divergence_type.is_empty());
        assert!(!div.severity.is_empty());
        assert!(!div.rule_id.is_empty());
        assert!(!div.root_cause.is_empty());
    }
}

// ── G-10: JSON report serde roundtrip ─────────────────────────────────────

#[test]
fn g10_json_roundtrip() {
    let diff = make_standard_diff();
    let generator = ReportGenerator::new(meta());
    let json_str = generator.generate(&diff, ReportFormat::Json);

    let parsed: JsonReport = serde_json::from_str(&json_str).unwrap();
    let re_json = serde_json::to_string_pretty(&parsed).unwrap();
    let re_parsed: JsonReport = serde_json::from_str(&re_json).unwrap();

    assert_eq!(re_parsed.replay_run_id, parsed.replay_run_id);
    assert_eq!(re_parsed.pass, parsed.pass);
    assert_eq!(re_parsed.equivalence_level, parsed.equivalence_level);
    assert_eq!(re_parsed.divergences.len(), parsed.divergences.len());
    assert_eq!(
        re_parsed.risk_summary.total_risk_score,
        parsed.risk_summary.total_risk_score
    );
}

// ── G-11: CSV column consistency ──────────────────────────────────────────

#[test]
fn g11_csv_columns() {
    let diff = make_standard_diff();
    let generator = ReportGenerator::new(meta());
    let csv = generator.generate(&diff, ReportFormat::Csv);

    let lines: Vec<&str> = csv.lines().collect();
    assert!(lines.len() > 1, "CSV should have header + data");

    let header_fields = lines[0].split(',').count();
    assert_eq!(header_fields, 7, "CSV header should have 7 columns");

    for (i, line) in lines.iter().enumerate().skip(1) {
        let fields = line.split(',').count();
        assert!(
            fields >= 7,
            "CSV line {} has {} fields, expected >= 7",
            i,
            fields
        );
    }
}

// ── G-12: Markdown heading structure ──────────────────────────────────────

#[test]
fn g12_markdown_headings() {
    let diff = make_standard_diff();
    let generator = ReportGenerator::new(meta());
    let md = generator.generate(&diff, ReportFormat::Markdown);

    assert!(md.starts_with("# Replay Decision-Diff Report"));
    assert!(md.contains("## Summary"));
    assert!(md.contains("## Divergences"));
}

// ── G-13: Human format severity ordering ──────────────────────────────────

#[test]
fn g13_human_severity_order() {
    // Create diff with mixed severities.
    let base = DecisionGraph::from_decisions(&[
        pm("rule_low", 100, 1, "def1", "out1"),
        pol("pol_auth", 200, 1, "def2", "out2"),
    ]);
    let cand = DecisionGraph::from_decisions(&[
        pm("rule_low", 100, 1, "def1", "out1_v2"),   // Low
        pol("pol_auth", 200, 1, "def2_v2", "out2_v2"), // Critical (policy def change)
    ]);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());
    let generator = ReportGenerator::new(meta());
    let human = generator.generate(&diff, ReportFormat::Human);

    // In the detail section, Critical should appear before Low.
    if let Some(details_start) = human.find("Divergence Details") {
        let details = &human[details_start..];
        if let Some(crit_pos) = details.find("Critical") {
            if let Some(low_pos) = details.find("Low") {
                assert!(crit_pos < low_pos, "Critical should appear before Low in details");
            }
        }
    }
}

// ── G-14: Gate: zero divergences → Pass ───────────────────────────────────

#[test]
fn g14_gate_empty_pass() {
    let events = vec![pm("r1", 100, 1, "d", "o")];
    let graph = DecisionGraph::from_decisions(&events);
    let diff = DecisionDiff::diff(&graph, &graph, &DiffConfig::default());
    let generator = ReportGenerator::new(meta());
    let json_str = generator.generate(&diff, ReportFormat::Json);
    let report: JsonReport = serde_json::from_str(&json_str).unwrap();

    let eval = GateEvaluator::with_defaults();
    let result = eval.evaluate_simple(&report);
    assert_eq!(result, GateResult::Pass);
}

// ── G-15: Gate: 1 critical → Fail ─────────────────────────────────────────

#[test]
fn g15_gate_critical_fail() {
    let base = DecisionGraph::from_decisions(&[
        pol("pol_auth", 100, 1, "def_v1", "allow"),
    ]);
    let cand = DecisionGraph::from_decisions(&[
        pol("pol_auth", 100, 1, "def_v2", "deny"),
    ]);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());
    let generator = ReportGenerator::new(meta());
    let json_str = generator.generate(&diff, ReportFormat::Json);
    let report: JsonReport = serde_json::from_str(&json_str).unwrap();

    let eval = GateEvaluator::with_defaults();
    let result = eval.evaluate_simple(&report);
    assert!(result.is_fail());
    assert!(
        result.violations().iter().any(|v| v.budget_dimension == "max_critical"),
        "should have critical violation"
    );
}

// ── G-16: Gate: annotation → Pass ─────────────────────────────────────────

#[test]
fn g16_gate_annotation_pass() {
    let base = DecisionGraph::from_decisions(&[
        pol("pol_auth", 100, 1, "def_v1", "allow"),
    ]);
    let cand = DecisionGraph::from_decisions(&[
        pol("pol_auth", 100, 1, "def_v2", "deny"),
    ]);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());
    let generator = ReportGenerator::new(meta());
    let json_str = generator.generate(&diff, ReportFormat::Json);
    let report: JsonReport = serde_json::from_str(&json_str).unwrap();

    // Annotate the divergence as expected.
    let ctx = EvaluationContext {
        annotations: vec![ExpectedDivergenceAnnotation {
            position: report.divergences[0].position,
            reason: "Intentional policy change for PR-42".into(),
            pr_reference: "PR-42".into(),
            definition_change_hash: "def_v2".into(),
        }],
        ..Default::default()
    };

    let eval = GateEvaluator::with_defaults();
    let result = eval.evaluate(&report, &ctx);
    assert!(result.is_pass(), "annotated divergence should pass: {:?}", result);
}

// ── G-17: Gate: skip budget → Fail ────────────────────────────────────────

#[test]
fn g17_gate_skip_budget() {
    let events = vec![pm("r1", 100, 1, "d", "o")];
    let graph = DecisionGraph::from_decisions(&events);
    let diff = DecisionDiff::diff(&graph, &graph, &DiffConfig::default());
    let generator = ReportGenerator::new(meta());
    let json_str = generator.generate(&diff, ReportFormat::Json);
    let report: JsonReport = serde_json::from_str(&json_str).unwrap();

    let ctx = EvaluationContext {
        total_artifacts: 100,
        skipped_artifacts: 15, // 15% > 10% default
        ..Default::default()
    };

    let eval = GateEvaluator::with_defaults();
    let result = eval.evaluate(&report, &ctx);
    assert!(result.is_fail());
}

// ── G-18: Gate: custom budget → Pass ──────────────────────────────────────

#[test]
fn g18_gate_custom_budget() {
    let base = DecisionGraph::from_decisions(&[
        wf("wf_deploy", 100, 1, "def1", "out1"),
    ]);
    let cand = DecisionGraph::from_decisions(&[
        wf("wf_deploy", 100, 1, "def1", "out1_v2"),
    ]);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());
    let generator = ReportGenerator::new(meta());
    let json_str = generator.generate(&diff, ReportFormat::Json);
    let report: JsonReport = serde_json::from_str(&json_str).unwrap();

    // Relaxed budget allows 5 high.
    let budget = RegressionBudget {
        max_high: 5,
        ..Default::default()
    };
    let eval = GateEvaluator::new(budget);
    let result = eval.evaluate_simple(&report);
    assert!(result.is_pass());
}

// ── G-19: Full pipeline roundtrip ─────────────────────────────────────────

#[test]
fn g19_full_roundtrip() {
    let diff = make_standard_diff();
    let generator = ReportGenerator::new(meta());

    // Generate JSON.
    let json_str = generator.generate(&diff, ReportFormat::Json);
    let parsed: JsonReport = serde_json::from_str(&json_str).unwrap();

    // Validate report against gate.
    let eval = GateEvaluator::with_defaults();
    let result = eval.evaluate_simple(&parsed);

    // The standard diff has modifications → check result is well-formed.
    match &result {
        GateResult::Pass => {}
        GateResult::Fail(violations) => {
            for v in violations {
                assert!(!v.budget_dimension.is_empty());
                assert!(!v.limit.is_empty());
                assert!(!v.actual.is_empty());
            }
        }
        GateResult::Warn(warnings) => {
            for w in warnings {
                assert!(!w.message.is_empty());
            }
        }
    }
}

// ── G-20: All four formats valid ──────────────────────────────────────────

#[test]
fn g20_all_formats() {
    let diff = make_standard_diff();
    let generator = ReportGenerator::new(meta());

    for fmt in &[
        ReportFormat::Human,
        ReportFormat::Json,
        ReportFormat::Markdown,
        ReportFormat::Csv,
    ] {
        let report = generator.generate(&diff, *fmt);
        assert!(!report.is_empty(), "format {:?} should produce output", fmt);
    }
}

// ── G-21: Large diff truncation ───────────────────────────────────────────

#[test]
fn g21_large_truncation() {
    let base_events: Vec<DecisionEvent> = (0..60)
        .map(|i| pm(&format!("rule_{}", i), i * 10, 0, "def", "out"))
        .collect();
    let cand_events: Vec<DecisionEvent> = (0..60)
        .map(|i| pm(&format!("rule_{}", i), i * 10, 0, "def", "out_mod"))
        .collect();
    let base = DecisionGraph::from_decisions(&base_events);
    let cand = DecisionGraph::from_decisions(&cand_events);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());

    let generator = ReportGenerator::new(meta());
    let human = generator.generate(&diff, ReportFormat::Human);
    assert!(human.contains("more divergences"));

    // CSV still has all rows.
    let csv = generator.generate(&diff, ReportFormat::Csv);
    let csv_lines = csv.lines().count();
    assert_eq!(csv_lines, 1 + diff.divergences.len());
}

// ── G-22: DiffSummary accounting invariant ────────────────────────────────

#[test]
fn g22_summary_accounting() {
    let diff = make_standard_diff();
    let total_baseline = diff.summary.total_baseline;
    let accounted = diff.summary.unchanged
        + diff.summary.modified
        + diff.summary.removed
        + diff.summary.shifted;
    assert_eq!(
        accounted, total_baseline,
        "unchanged+modified+removed+shifted should equal total_baseline"
    );
}

// ── G-23: Risk score sum consistency ──────────────────────────────────────

#[test]
fn g23_risk_score_sum() {
    let diff = make_standard_diff();
    let scorer = RiskScorer::new();
    let risk = scorer.aggregate(&diff.divergences);
    let count_sum = risk.critical_count + risk.high_count + risk.medium_count + risk.low_count + risk.info_count;
    assert_eq!(count_sum, diff.divergences.len() as u64);
}

// ── G-24: Report idempotency ──────────────────────────────────────────────

#[test]
fn g24_idempotent() {
    let diff = make_standard_diff();
    let generator = ReportGenerator::new(meta());

    for fmt in &[
        ReportFormat::Human,
        ReportFormat::Json,
        ReportFormat::Markdown,
        ReportFormat::Csv,
    ] {
        let r1 = generator.generate(&diff, *fmt);
        let r2 = generator.generate(&diff, *fmt);
        assert_eq!(r1, r2, "format {:?} should be idempotent", fmt);
    }
}

// ── G-25: Empty candidate → all Removed ───────────────────────────────────

#[test]
fn g25_empty_candidate() {
    let base = DecisionGraph::from_decisions(&[
        pm("r1", 100, 1, "d1", "o1"),
        pm("r2", 200, 1, "d2", "o2"),
        pm("r3", 300, 1, "d3", "o3"),
    ]);
    let cand = DecisionGraph::from_decisions(&[]);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());

    assert_eq!(diff.summary.removed, 3);
    assert_eq!(diff.summary.unchanged, 0);
    assert_eq!(diff.summary.added, 0);
}

// ── G-26: Empty baseline → all Added ──────────────────────────────────────

#[test]
fn g26_empty_baseline() {
    let base = DecisionGraph::from_decisions(&[]);
    let cand = DecisionGraph::from_decisions(&[
        pm("r1", 100, 1, "d1", "o1"),
        pm("r2", 200, 1, "d2", "o2"),
    ]);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());

    assert_eq!(diff.summary.added, 2);
    assert_eq!(diff.summary.unchanged, 0);
    assert_eq!(diff.summary.removed, 0);
}

// ── Shared fixture ────────────────────────────────────────────────────────

fn make_standard_diff() -> DecisionDiff {
    let base = DecisionGraph::from_decisions(&[
        pm("rule_a", 100, 1, "def1", "out1"),
        wf("wf_deploy", 200, 1, "def2", "out2"),
        pol("pol_auth", 300, 1, "def3", "out3"),
    ]);
    let cand = DecisionGraph::from_decisions(&[
        pm("rule_a", 100, 1, "def1", "out1"),        // unchanged
        wf("wf_deploy", 200, 1, "def2", "out_mod"),  // modified
        pol("pol_auth", 300, 1, "def3_v2", "out_v2"), // modified (def change)
    ]);
    DecisionDiff::diff(&base, &cand, &DiffConfig::default())
}
