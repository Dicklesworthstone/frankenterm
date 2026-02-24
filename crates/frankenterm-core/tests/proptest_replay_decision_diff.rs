//! Property-based tests for replay_decision_diff (ft-og6q6.5.2).
//!
//! Invariants tested:
//! - DD-1: diff(A, A) produces empty divergences
//! - DD-2: diff(A, B) added == diff(B, A) removed and vice versa
//! - DD-3: Root causes always reference valid rule_ids
//! - DD-4: summary.total_baseline == baseline node count
//! - DD-5: summary.total_candidate == candidate node count
//! - DD-6: unchanged + modified + removed + shifted == total_baseline (approx)
//! - DD-7: DivergenceType serde roundtrip
//! - DD-8: RootCause serde roundtrip
//! - DD-9: EquivalenceLevel ordering L0 < L1 < L2
//! - DD-10: L2 equivalent implies L1 implies L0
//! - DD-11: DiffSummary serde roundtrip
//! - DD-12: DiffConfig serde roundtrip
//! - DD-13: DecisionDiff JSON roundtrip
//! - DD-14: total_divergences == added + removed + modified + shifted
//! - DD-15: Empty diff is_empty
//! - DD-16: Custom tolerance accepts wider shifts
//! - DD-17: Shifted divergences have TimingShift root cause
//! - DD-18: Modified divergences have non-NewDecision root cause

use proptest::prelude::*;

use frankenterm_core::replay_decision_diff::{
    DecisionDiff, DiffConfig, DiffSummary, DivergenceType, EquivalenceLevel, RootCause,
};
use frankenterm_core::replay_decision_graph::{DecisionEvent, DecisionGraph, DecisionType};

// ── Strategies ────────────────────────────────────────────────────────────

fn arb_decision_type() -> impl Strategy<Value = DecisionType> {
    prop_oneof![
        Just(DecisionType::PatternMatch),
        Just(DecisionType::WorkflowStep),
        Just(DecisionType::PolicyDecision),
        Just(DecisionType::AlertFired),
        Just(DecisionType::OverrideApplied),
        Just(DecisionType::BarrierDecision),
        Just(DecisionType::NoOp),
    ]
}

fn arb_event(index: usize) -> impl Strategy<Value = DecisionEvent> {
    (arb_decision_type(), "[a-z]{1,4}", 0u64..3)
        .prop_map(move |(dt, rule_id, pane_id)| DecisionEvent {
            decision_type: dt,
            rule_id,
            definition_hash: format!("def_{}", index),
            input_hash: format!("in_{}", index),
            output_hash: format!("out_{}", index),
            timestamp_ms: (index as u64) * 100,
            pane_id,
            triggered_by: None,
            overrides: None,
            wall_clock_ms: 0,
            replay_run_id: String::new(),
        })
}

fn arb_events(max_len: usize) -> impl Strategy<Value = Vec<DecisionEvent>> {
    (1..max_len).prop_flat_map(|n| {
        let strats: Vec<_> = (0..n).map(|i| arb_event(i).boxed()).collect();
        strats
    })
}

fn arb_divergence_type() -> impl Strategy<Value = DivergenceType> {
    prop_oneof![
        Just(DivergenceType::Added),
        Just(DivergenceType::Removed),
        Just(DivergenceType::Modified),
        Just(DivergenceType::Shifted),
    ]
}

fn arb_root_cause() -> impl Strategy<Value = RootCause> {
    prop_oneof![
        ("[a-z]{1,4}", "[a-f0-9]{8}", "[a-f0-9]{8}").prop_map(|(rule_id, bh, ch)| {
            RootCause::RuleDefinitionChange {
                rule_id,
                baseline_hash: bh,
                candidate_hash: ch,
            }
        }),
        ("[a-z]{1,4}", 0u64..100).prop_map(|(rule_id, pos)| {
            RootCause::InputDivergence {
                upstream_rule_id: rule_id,
                upstream_position: pos,
            }
        }),
        "[a-z]{1,4}".prop_map(|rule_id| RootCause::NewDecision { rule_id }),
        "[a-z]{1,4}".prop_map(|rule_id| RootCause::DroppedDecision { rule_id }),
        (0u64..10000, 0u64..10000).prop_map(|(b, c)| {
            let delta = if c >= b { c - b } else { b - c };
            RootCause::TimingShift {
                baseline_ms: b,
                candidate_ms: c,
                delta_ms: delta,
            }
        }),
        Just(RootCause::Unknown),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // ── DD-1: diff(A, A) produces empty divergences ────────────────────

    #[test]
    fn dd1_self_diff_empty(events in arb_events(10)) {
        let graph = DecisionGraph::from_decisions(&events);
        let diff = DecisionDiff::diff(&graph, &graph, &DiffConfig::default());
        prop_assert!(diff.divergences.is_empty(), "self-diff should be empty");
        prop_assert!(diff.summary.is_empty());
    }

    // ── DD-2: Added/removed swap when reversing ────────────────────────

    #[test]
    fn dd2_symmetry(
        base_events in arb_events(8),
        cand_events in arb_events(8),
    ) {
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let cfg = DiffConfig::default();
        let fwd = DecisionDiff::diff(&base, &cand, &cfg);
        let rev = DecisionDiff::diff(&cand, &base, &cfg);
        prop_assert_eq!(fwd.summary.added, rev.summary.removed);
        prop_assert_eq!(fwd.summary.removed, rev.summary.added);
    }

    // ── DD-3: Root causes reference valid rule_ids ─────────────────────

    #[test]
    fn dd3_root_cause_valid(events in arb_events(8)) {
        let mut modified_events = events.clone();
        for e in &mut modified_events {
            e.output_hash = format!("{}_mod", e.output_hash);
        }
        let base = DecisionGraph::from_decisions(&events);
        let cand = DecisionGraph::from_decisions(&modified_events);
        let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());
        for div in &diff.divergences {
            match &div.root_cause {
                RootCause::RuleDefinitionChange { rule_id, .. } => {
                    prop_assert!(!rule_id.is_empty(), "rule_id should not be empty");
                }
                RootCause::InputDivergence { upstream_rule_id, .. } => {
                    prop_assert!(!upstream_rule_id.is_empty());
                }
                RootCause::NewDecision { rule_id } => {
                    prop_assert!(!rule_id.is_empty());
                }
                RootCause::DroppedDecision { rule_id } => {
                    prop_assert!(!rule_id.is_empty());
                }
                _ => {}
            }
        }
    }

    // ── DD-4: total_baseline matches ───────────────────────────────────

    #[test]
    fn dd4_total_baseline(events in arb_events(10)) {
        let base = DecisionGraph::from_decisions(&events);
        let cand = DecisionGraph::from_decisions(&[]);
        let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());
        prop_assert_eq!(diff.summary.total_baseline, events.len() as u64);
    }

    // ── DD-5: total_candidate matches ──────────────────────────────────

    #[test]
    fn dd5_total_candidate(events in arb_events(10)) {
        let base = DecisionGraph::from_decisions(&[]);
        let cand = DecisionGraph::from_decisions(&events);
        let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());
        prop_assert_eq!(diff.summary.total_candidate, events.len() as u64);
    }

    // ── DD-6: Accounting: unchanged + modified + removed + shifted = total_baseline ─

    #[test]
    fn dd6_accounting(
        base_events in arb_events(8),
        cand_events in arb_events(8),
    ) {
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());
        let s = &diff.summary;
        // Every baseline node must be either unchanged, modified, removed, or shifted.
        let accounted = s.unchanged + s.modified + s.removed + s.shifted;
        prop_assert_eq!(
            accounted, s.total_baseline,
            "unchanged({}) + modified({}) + removed({}) + shifted({}) should equal total_baseline({})",
            s.unchanged, s.modified, s.removed, s.shifted, s.total_baseline
        );
    }

    // ── DD-7: DivergenceType serde ─────────────────────────────────────

    #[test]
    fn dd7_divergence_type_serde(dt in arb_divergence_type()) {
        let json = serde_json::to_string(&dt).unwrap();
        let restored: DivergenceType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, dt);
    }

    // ── DD-8: RootCause serde ──────────────────────────────────────────

    #[test]
    fn dd8_root_cause_serde(rc in arb_root_cause()) {
        let json = serde_json::to_string(&rc).unwrap();
        let restored: RootCause = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, rc);
    }

    // ── DD-9: EquivalenceLevel ordering ────────────────────────────────

    #[test]
    fn dd9_level_ordering(_dummy in 0u8..1) {
        prop_assert!(EquivalenceLevel::L0 < EquivalenceLevel::L1);
        prop_assert!(EquivalenceLevel::L1 < EquivalenceLevel::L2);
    }

    // ── DD-10: L2 implies L1 implies L0 ────────────────────────────────

    #[test]
    fn dd10_level_implication(
        base_events in arb_events(6),
        cand_events in arb_events(6),
    ) {
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());
        if diff.is_equivalent(EquivalenceLevel::L2) {
            prop_assert!(diff.is_equivalent(EquivalenceLevel::L1));
        }
        if diff.is_equivalent(EquivalenceLevel::L1) {
            prop_assert!(diff.is_equivalent(EquivalenceLevel::L0));
        }
    }

    // ── DD-11: DiffSummary serde ───────────────────────────────────────

    #[test]
    fn dd11_summary_serde(
        total_baseline in 0u64..100,
        total_candidate in 0u64..100,
        unchanged in 0u64..50,
        added in 0u64..50,
        removed in 0u64..50,
        modified in 0u64..50,
        shifted in 0u64..50,
    ) {
        let summary = DiffSummary {
            total_baseline,
            total_candidate,
            unchanged,
            added,
            removed,
            modified,
            shifted,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let restored: DiffSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, summary);
    }

    // ── DD-12: DiffConfig serde ────────────────────────────────────────

    #[test]
    fn dd12_config_serde(tolerance in 0u64..10000, attr in proptest::bool::ANY) {
        let cfg = DiffConfig {
            time_tolerance_ms: tolerance,
            attribute_root_causes: attr,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: DiffConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.time_tolerance_ms, tolerance);
        prop_assert_eq!(restored.attribute_root_causes, attr);
    }

    // ── DD-13: DecisionDiff JSON roundtrip ─────────────────────────────

    #[test]
    fn dd13_diff_json_roundtrip(events in arb_events(6)) {
        let mut mod_events = events.clone();
        for e in &mut mod_events {
            e.output_hash = format!("{}_x", e.output_hash);
        }
        let base = DecisionGraph::from_decisions(&events);
        let cand = DecisionGraph::from_decisions(&mod_events);
        let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());
        let json = diff.to_json();
        let restored: DecisionDiff = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.summary.modified, diff.summary.modified);
    }

    // ── DD-14: total_divergences sum ───────────────────────────────────

    #[test]
    fn dd14_total_divergences(
        added in 0u64..50,
        removed in 0u64..50,
        modified in 0u64..50,
        shifted in 0u64..50,
    ) {
        let summary = DiffSummary {
            total_baseline: 100,
            total_candidate: 100,
            unchanged: 0,
            added,
            removed,
            modified,
            shifted,
        };
        prop_assert_eq!(summary.total_divergences(), added + removed + modified + shifted);
    }

    // ── DD-15: Empty diff is_empty ─────────────────────────────────────

    #[test]
    fn dd15_empty_is_empty(_dummy in 0u8..1) {
        let summary = DiffSummary::default();
        prop_assert!(summary.is_empty());
    }

    // ── DD-16: Wider tolerance accepts more shifts ─────────────────────

    #[test]
    fn dd16_wider_tolerance(delta in 1u64..200) {
        // Create events with a timing shift of `delta` ms.
        let base_events = vec![DecisionEvent {
            decision_type: DecisionType::PatternMatch,
            rule_id: "r1".into(),
            definition_hash: "d".into(),
            input_hash: "i".into(),
            output_hash: "o".into(),
            timestamp_ms: 1000,
            pane_id: 0,
            triggered_by: None,
            overrides: None,
            wall_clock_ms: 0,
            replay_run_id: String::new(),
        }];
        let cand_events = vec![DecisionEvent {
            decision_type: DecisionType::PatternMatch,
            rule_id: "r1".into(),
            definition_hash: "d".into(),
            input_hash: "i".into(),
            output_hash: "o".into(),
            timestamp_ms: 1000 + delta,
            pane_id: 0,
            triggered_by: None,
            overrides: None,
            wall_clock_ms: 0,
            replay_run_id: String::new(),
        }];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);

        // Narrow tolerance: 0ms → never shifted.
        let narrow = DiffConfig { time_tolerance_ms: 0, attribute_root_causes: false };
        let diff_narrow = DecisionDiff::diff(&base, &cand, &narrow);

        // Wide tolerance: delta + 1 → always shifted.
        let wide = DiffConfig { time_tolerance_ms: delta + 1, attribute_root_causes: false };
        let diff_wide = DecisionDiff::diff(&base, &cand, &wide);

        prop_assert!(diff_wide.summary.shifted >= diff_narrow.summary.shifted);
    }

    // ── DD-17: Shifted divergences have TimingShift root cause ──────────

    #[test]
    fn dd17_shifted_has_timing_root(events in arb_events(6)) {
        // Create candidate with small timing shifts.
        let mut shifted_events = events.clone();
        for e in &mut shifted_events {
            e.timestamp_ms += 50; // Within default 100ms tolerance.
        }
        let base = DecisionGraph::from_decisions(&events);
        let cand = DecisionGraph::from_decisions(&shifted_events);
        let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());
        for div in &diff.divergences {
            if div.divergence_type == DivergenceType::Shifted {
                let is_timing = matches!(&div.root_cause, RootCause::TimingShift { .. });
                prop_assert!(is_timing, "shifted divergence should have TimingShift root cause");
            }
        }
    }

    // ── DD-18: Modified divergences don't have NewDecision root cause ───

    #[test]
    fn dd18_modified_not_new(events in arb_events(6)) {
        let mut mod_events = events.clone();
        for e in &mut mod_events {
            e.output_hash = format!("{}_changed", e.output_hash);
        }
        let base = DecisionGraph::from_decisions(&events);
        let cand = DecisionGraph::from_decisions(&mod_events);
        let diff = DecisionDiff::diff(&base, &cand, &DiffConfig::default());
        for div in &diff.divergences {
            if div.divergence_type == DivergenceType::Modified {
                let is_new = matches!(&div.root_cause, RootCause::NewDecision { .. });
                prop_assert!(!is_new, "modified divergence should not have NewDecision root cause");
            }
        }
    }
}
