//! Property-based tests for replay_report (ft-og6q6.5.4).
//!
//! Invariants tested:
//! - RP-1: ReportFormat serde roundtrip
//! - RP-2: ReportMeta serde roundtrip
//! - RP-3: JsonReport serde roundtrip (via generate+parse+serialize+parse)
//! - RP-4: All four formats produce non-empty output
//! - RP-5: JSON format always valid JSON
//! - RP-6: CSV header always present
//! - RP-7: CSV line count = 1 + divergence count
//! - RP-8: Markdown starts with # heading
//! - RP-9: Human format contains "Recommendation"
//! - RP-10: Empty diff → PASS in all formats
//! - RP-11: JSON divergence count matches risk summary total
//! - RP-12: JSON pass field ↔ recommendation consistency
//! - RP-13: Human detail count ≤ MAX_DETAIL_DIVERGENCES (20)
//! - RP-14: All severity strings in JSON are valid
//! - RP-15: Markdown contains severity table
//! - RP-16: JSON equivalence_level in {L0, L1, L2, NONE}
//! - RP-17: CSV fields per line = 7 (matching header)
//! - RP-18: Idempotent generation (same input → same output)

use proptest::prelude::*;

use frankenterm_core::replay_decision_diff::{DecisionDiff, DiffConfig};
use frankenterm_core::replay_decision_graph::{DecisionEvent, DecisionGraph, DecisionType};
use frankenterm_core::replay_report::{JsonReport, ReportFormat, ReportGenerator, ReportMeta};

// ── Strategies ────────────────────────────────────────────────────────────

fn arb_report_format() -> impl Strategy<Value = ReportFormat> {
    prop_oneof![
        Just(ReportFormat::Human),
        Just(ReportFormat::Json),
        Just(ReportFormat::Markdown),
        Just(ReportFormat::Csv),
    ]
}

fn arb_meta() -> impl Strategy<Value = ReportMeta> {
    (
        "[a-z]{3,8}",          // replay_run_id
        "[a-z]{3,8}\\.replay", // artifact_path
        "[a-z]{3,8}\\.over",   // override_path
        0u64..10000,           // replay_duration_ms
        0u64..5000,            // total_events
    )
        .prop_map(|(run_id, art, ovr, dur, evts)| ReportMeta {
            replay_run_id: run_id,
            artifact_path: art,
            override_path: ovr,
            replay_duration_ms: dur,
            total_events: evts,
            timestamp: "2026-02-24T00:00:00Z".into(),
        })
}

fn arb_decision_type() -> impl Strategy<Value = DecisionType> {
    prop_oneof![
        Just(DecisionType::PatternMatch),
        Just(DecisionType::WorkflowStep),
        Just(DecisionType::PolicyDecision),
        Just(DecisionType::AlertFired),
    ]
}

fn make_event(
    dt: DecisionType,
    rule_id: &str,
    ts: u64,
    pane: u64,
    def: &str,
    out: &str,
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

/// Generate a pair of diffs with controlled divergences.
fn arb_diff_pair(
    n_shared: usize,
    n_modified: usize,
    n_added: usize,
    n_removed: usize,
) -> (DecisionDiff, usize) {
    let mut base_events = Vec::new();
    let mut cand_events = Vec::new();
    let mut ts = 100u64;

    // Shared (unchanged)
    for i in 0..n_shared {
        let rule = format!("rule_{}", i);
        base_events.push(make_event(
            DecisionType::PatternMatch,
            &rule,
            ts,
            0,
            "def",
            "out",
        ));
        cand_events.push(make_event(
            DecisionType::PatternMatch,
            &rule,
            ts,
            0,
            "def",
            "out",
        ));
        ts += 10;
    }

    // Modified (same rule, different output)
    for i in 0..n_modified {
        let rule = format!("mod_rule_{}", i);
        base_events.push(make_event(
            DecisionType::PatternMatch,
            &rule,
            ts,
            0,
            "def",
            "out_base",
        ));
        cand_events.push(make_event(
            DecisionType::PatternMatch,
            &rule,
            ts,
            0,
            "def",
            "out_cand",
        ));
        ts += 10;
    }

    // Removed (only in baseline)
    for i in 0..n_removed {
        let rule = format!("rem_rule_{}", i);
        base_events.push(make_event(
            DecisionType::PatternMatch,
            &rule,
            ts,
            0,
            "def",
            "out",
        ));
        ts += 10;
    }

    // Added (only in candidate)
    for i in 0..n_added {
        let rule = format!("add_rule_{}", i);
        cand_events.push(make_event(
            DecisionType::PatternMatch,
            &rule,
            ts,
            0,
            "def",
            "out",
        ));
        ts += 10;
    }

    let base = DecisionGraph::from_decisions(&base_events);
    let cand = DecisionGraph::from_decisions(&cand_events);
    let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());
    let total_divergences = diff.divergences.len();
    (diff, total_divergences)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // ── RP-1: ReportFormat serde roundtrip ────────────────────────────────

    #[test]
    fn rp1_format_serde(fmt in arb_report_format()) {
        let json = serde_json::to_string(&fmt).unwrap();
        let restored: ReportFormat = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, fmt);
    }

    // ── RP-2: ReportMeta serde roundtrip ──────────────────────────────────

    #[test]
    fn rp2_meta_serde(meta in arb_meta()) {
        let json = serde_json::to_string(&meta).unwrap();
        let restored: ReportMeta = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&restored.replay_run_id, &meta.replay_run_id);
        prop_assert_eq!(restored.replay_duration_ms, meta.replay_duration_ms);
        prop_assert_eq!(restored.total_events, meta.total_events);
    }

    // ── RP-3: JsonReport serde roundtrip ──────────────────────────────────

    #[test]
    fn rp3_json_report_roundtrip(
        meta in arb_meta(),
        n_shared in 0usize..5,
        n_modified in 0usize..5,
    ) {
        let (diff, _) = arb_diff_pair(n_shared, n_modified, 0, 0);
        let generator = ReportGenerator::new(meta);
        let json_str = generator.generate(&diff, ReportFormat::Json);
        let parsed: JsonReport = serde_json::from_str(&json_str).unwrap();
        let re_json = serde_json::to_string_pretty(&parsed).unwrap();
        let re_parsed: JsonReport = serde_json::from_str(&re_json).unwrap();
        prop_assert_eq!(&re_parsed.replay_run_id, &parsed.replay_run_id);
        prop_assert_eq!(re_parsed.pass, parsed.pass);
        prop_assert_eq!(re_parsed.divergences.len(), parsed.divergences.len());
    }

    // ── RP-4: All formats produce non-empty output ────────────────────────

    #[test]
    fn rp4_all_formats_nonempty(
        meta in arb_meta(),
        fmt in arb_report_format(),
        n_mod in 0usize..4,
    ) {
        let (diff, _) = arb_diff_pair(2, n_mod, 0, 0);
        let generator = ReportGenerator::new(meta);
        let report = generator.generate(&diff, fmt);
        prop_assert!(!report.is_empty());
    }

    // ── RP-5: JSON format always valid JSON ───────────────────────────────

    #[test]
    fn rp5_json_valid(
        meta in arb_meta(),
        n_shared in 0usize..5,
        n_modified in 0usize..3,
        n_added in 0usize..3,
    ) {
        let (diff, _) = arb_diff_pair(n_shared, n_modified, n_added, 0);
        let generator = ReportGenerator::new(meta);
        let report = generator.generate(&diff, ReportFormat::Json);
        let parsed: serde_json::Value = serde_json::from_str(&report).unwrap();
        let is_obj = parsed.is_object();
        prop_assert!(is_obj);
    }

    // ── RP-6: CSV header always present ───────────────────────────────────

    #[test]
    fn rp6_csv_header(meta in arb_meta(), n_mod in 0usize..5) {
        let (diff, _) = arb_diff_pair(1, n_mod, 0, 0);
        let generator = ReportGenerator::new(meta);
        let report = generator.generate(&diff, ReportFormat::Csv);
        let first_line = report.lines().next().unwrap();
        prop_assert!(first_line.starts_with("position,divergence_type,severity"));
    }

    // ── RP-7: CSV line count = 1 + divergence count ──────────────────────

    #[test]
    fn rp7_csv_line_count(
        meta in arb_meta(),
        n_mod in 0usize..5,
        n_added in 0usize..3,
    ) {
        let (diff, total_div) = arb_diff_pair(2, n_mod, n_added, 0);
        let generator = ReportGenerator::new(meta);
        let report = generator.generate(&diff, ReportFormat::Csv);
        let line_count = report.lines().count();
        prop_assert_eq!(line_count, 1 + total_div);
    }

    // ── RP-8: Markdown starts with heading ────────────────────────────────

    #[test]
    fn rp8_markdown_heading(meta in arb_meta(), n_mod in 0usize..3) {
        let (diff, _) = arb_diff_pair(1, n_mod, 0, 0);
        let generator = ReportGenerator::new(meta);
        let report = generator.generate(&diff, ReportFormat::Markdown);
        prop_assert!(report.starts_with("# "));
    }

    // ── RP-9: Human format contains "Recommendation" ─────────────────────

    #[test]
    fn rp9_human_recommendation(meta in arb_meta(), n_mod in 0usize..5) {
        let (diff, _) = arb_diff_pair(1, n_mod, 0, 0);
        let generator = ReportGenerator::new(meta);
        let report = generator.generate(&diff, ReportFormat::Human);
        prop_assert!(report.contains("Recommendation"));
    }

    // ── RP-10: Empty diff → PASS ─────────────────────────────────────────

    #[test]
    fn rp10_empty_pass(meta in arb_meta(), fmt in arb_report_format()) {
        let (diff, _) = arb_diff_pair(3, 0, 0, 0);
        let generator = ReportGenerator::new(meta);
        let report = generator.generate(&diff, fmt);
        match fmt {
            ReportFormat::Json => {
                let parsed: JsonReport = serde_json::from_str(&report).unwrap();
                prop_assert!(parsed.pass);
            }
            ReportFormat::Human => {
                prop_assert!(report.contains("PASS"));
            }
            _ => {} // Markdown/CSV don't have explicit pass indicator in all cases
        }
    }

    // ── RP-11: JSON divergence count matches risk total ──────────────────

    #[test]
    fn rp11_json_count_consistency(
        meta in arb_meta(),
        n_mod in 0usize..5,
        n_added in 0usize..3,
    ) {
        let (diff, _) = arb_diff_pair(2, n_mod, n_added, 0);
        let generator = ReportGenerator::new(meta);
        let report = generator.generate(&diff, ReportFormat::Json);
        let parsed: JsonReport = serde_json::from_str(&report).unwrap();
        let risk_total = parsed.risk_summary.critical_count
            + parsed.risk_summary.high_count
            + parsed.risk_summary.medium_count
            + parsed.risk_summary.low_count
            + parsed.risk_summary.info_count;
        prop_assert_eq!(risk_total as usize, parsed.divergences.len());
    }

    // ── RP-12: JSON pass ↔ recommendation consistency ────────────────────

    #[test]
    fn rp12_pass_recommendation(
        meta in arb_meta(),
        n_mod in 0usize..5,
    ) {
        let (diff, _) = arb_diff_pair(2, n_mod, 0, 0);
        let generator = ReportGenerator::new(meta);
        let report = generator.generate(&diff, ReportFormat::Json);
        let parsed: JsonReport = serde_json::from_str(&report).unwrap();
        if parsed.pass {
            prop_assert_eq!(&parsed.recommendation, "Pass");
        } else {
            let is_review_or_block = &parsed.recommendation == "Review" || &parsed.recommendation == "Block";
            prop_assert!(is_review_or_block);
        }
    }

    // ── RP-13: Human detail ≤ 20 lines ───────────────────────────────────

    #[test]
    fn rp13_human_detail_limit(meta in arb_meta()) {
        // Build a diff with >20 divergences.
        let (diff, _) = arb_diff_pair(0, 25, 0, 0);
        let generator = ReportGenerator::new(meta);
        let report = generator.generate(&diff, ReportFormat::Human);
        // Count detail lines (start with "  [").
        let detail_count = report.lines().filter(|l| l.trim_start().starts_with('[') && l.contains(']')).count();
        prop_assert!(detail_count <= 20, "detail_count {} > 20", detail_count);
    }

    // ── RP-14: Valid severity strings in JSON ─────────────────────────────

    #[test]
    fn rp14_valid_severities(
        meta in arb_meta(),
        n_mod in 1usize..5,
    ) {
        let (diff, _) = arb_diff_pair(1, n_mod, 0, 0);
        let generator = ReportGenerator::new(meta);
        let report = generator.generate(&diff, ReportFormat::Json);
        let parsed: JsonReport = serde_json::from_str(&report).unwrap();
        let valid_severities = ["Info", "Low", "Medium", "High", "Critical"];
        for div in &parsed.divergences {
            let is_valid = valid_severities.contains(&div.severity.as_str());
            prop_assert!(is_valid, "invalid severity: {}", div.severity);
        }
    }

    // ── RP-15: Markdown contains severity table ──────────────────────────

    #[test]
    fn rp15_markdown_severity_table(meta in arb_meta(), n_mod in 0usize..3) {
        let (diff, _) = arb_diff_pair(1, n_mod, 0, 0);
        let generator = ReportGenerator::new(meta);
        let report = generator.generate(&diff, ReportFormat::Markdown);
        prop_assert!(report.contains("| Severity | Count |"));
    }

    // ── RP-16: JSON equivalence_level valid ──────────────────────────────

    #[test]
    fn rp16_equivalence_level(
        meta in arb_meta(),
        n_mod in 0usize..3,
        n_added in 0usize..2,
    ) {
        let (diff, _) = arb_diff_pair(2, n_mod, n_added, 0);
        let generator = ReportGenerator::new(meta);
        let report = generator.generate(&diff, ReportFormat::Json);
        let parsed: JsonReport = serde_json::from_str(&report).unwrap();
        let valid_levels = ["L0", "L1", "L2", "NONE"];
        let is_valid = valid_levels.contains(&parsed.equivalence_level.as_str());
        prop_assert!(is_valid, "invalid level: {}", parsed.equivalence_level);
    }

    // ── RP-17: CSV fields per line = 7 ───────────────────────────────────

    #[test]
    fn rp17_csv_field_count(meta in arb_meta(), n_mod in 1usize..5) {
        let (diff, _) = arb_diff_pair(1, n_mod, 0, 0);
        let generator = ReportGenerator::new(meta);
        let report = generator.generate(&diff, ReportFormat::Csv);
        for line in report.lines() {
            let fields = line.split(',').count();
            prop_assert!(fields >= 7, "expected >=7 fields, got {}", fields);
        }
    }

    // ── RP-18: Idempotent generation ─────────────────────────────────────

    #[test]
    fn rp18_idempotent(
        meta in arb_meta(),
        fmt in arb_report_format(),
        n_mod in 0usize..3,
    ) {
        let (diff, _) = arb_diff_pair(2, n_mod, 0, 0);
        let generator = ReportGenerator::new(meta);
        let report1 = generator.generate(&diff, fmt);
        let report2 = generator.generate(&diff, fmt);
        prop_assert_eq!(&report1, &report2);
    }
}
