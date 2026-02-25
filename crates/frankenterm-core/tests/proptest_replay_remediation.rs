//! Property-based tests for replay_remediation (ft-og6q6.5.7).
//!
//! Invariants tested:
//! - RM-1: SuggestionAction serde roundtrip
//! - RM-2: EffortEstimate serde roundtrip
//! - RM-3: EffortEstimate ordering (Low < Medium < High)
//! - RM-4: Suggestion serde roundtrip
//! - RM-5: Every divergence gets at least one suggestion
//! - RM-6: Info severity → NoActionNeeded
//! - RM-7: Timing shift → NoActionNeeded
//! - RM-8: Rule definition change → includes AddAnnotation
//! - RM-9: Input divergence → includes InvestigateUpstream
//! - RM-10: Confidence is in [0, 1]
//! - RM-11: Target is non-empty
//! - RM-12: Rationale is non-empty
//! - RM-13: suggest_all returns one entry per divergence
//! - RM-14: Cascade detection adds suggestion when shared upstream
//! - RM-15: New decision → AddAnnotation
//! - RM-16: Dropped decision → InvestigateUpstream

use proptest::prelude::*;

use frankenterm_core::replay_decision_diff::{
    Divergence, DivergenceNode, DivergenceType, RootCause,
};
use frankenterm_core::replay_remediation::{
    EffortEstimate, RemediationEngine, Suggestion, SuggestionAction,
};

// ── Strategies ────────────────────────────────────────────────────────────

fn arb_action() -> impl Strategy<Value = SuggestionAction> {
    prop_oneof![
        Just(SuggestionAction::UpdateRule),
        Just(SuggestionAction::AddAnnotation),
        Just(SuggestionAction::RevertChange),
        Just(SuggestionAction::InvestigateUpstream),
        Just(SuggestionAction::NoActionNeeded),
    ]
}

fn arb_effort() -> impl Strategy<Value = EffortEstimate> {
    prop_oneof![
        Just(EffortEstimate::Low),
        Just(EffortEstimate::Medium),
        Just(EffortEstimate::High),
    ]
}

fn make_node(rule_id: &str) -> DivergenceNode {
    DivergenceNode {
        node_id: 0,
        rule_id: rule_id.into(),
        definition_hash: "d".into(),
        output_hash: "o".into(),
        timestamp_ms: 100,
        pane_id: 1,
    }
}

fn arb_root_cause() -> impl Strategy<Value = RootCause> {
    prop_oneof![
        Just(RootCause::RuleDefinitionChange {
            rule_id: "r".into(),
            baseline_hash: "h1".into(),
            candidate_hash: "h2".into(),
        }),
        Just(RootCause::InputDivergence {
            upstream_rule_id: "u".into(),
            upstream_position: 0,
        }),
        Just(RootCause::OverrideApplied {
            rule_id: "r".into(),
            override_id: "ovr".into(),
        }),
        Just(RootCause::NewDecision {
            rule_id: "new_r".into(),
        }),
        Just(RootCause::DroppedDecision {
            rule_id: "dropped_r".into(),
        }),
        Just(RootCause::TimingShift {
            baseline_ms: 100,
            candidate_ms: 150,
            delta_ms: 50,
        }),
        Just(RootCause::Unknown),
    ]
}

fn _arb_divergence_type() -> impl Strategy<Value = DivergenceType> {
    prop_oneof![
        Just(DivergenceType::Added),
        Just(DivergenceType::Removed),
        Just(DivergenceType::Modified),
        Just(DivergenceType::Shifted),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // ── RM-1: SuggestionAction serde roundtrip ────────────────────────

    #[test]
    fn rm1_action_serde(action in arb_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let restored: SuggestionAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, action);
    }

    // ── RM-2: EffortEstimate serde roundtrip ──────────────────────────

    #[test]
    fn rm2_effort_serde(effort in arb_effort()) {
        let json = serde_json::to_string(&effort).unwrap();
        let restored: EffortEstimate = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, effort);
    }

    // ── RM-3: EffortEstimate ordering ─────────────────────────────────

    #[test]
    fn rm3_effort_ordering(_dummy in 0u8..1) {
        prop_assert!(EffortEstimate::Low < EffortEstimate::Medium);
        prop_assert!(EffortEstimate::Medium < EffortEstimate::High);
    }

    // ── RM-4: Suggestion serde roundtrip ──────────────────────────────

    #[test]
    fn rm4_suggestion_serde(
        action in arb_action(),
        effort in arb_effort(),
        conf in 0.0f64..1.0,
    ) {
        let s = Suggestion {
            action,
            target: "target_r".into(),
            rationale: "rationale".into(),
            confidence: conf,
            effort_estimate: effort,
        };
        let json = serde_json::to_string(&s).unwrap();
        let restored: Suggestion = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.action, action);
        prop_assert_eq!(restored.effort_estimate, effort);
    }

    // ── RM-5: Every divergence gets at least one suggestion ───────────

    #[test]
    fn rm5_at_least_one(root_cause in arb_root_cause()) {
        let engine = RemediationEngine::new();
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Modified,
            baseline_node: Some(make_node("rule_x")),
            candidate_node: Some(make_node("rule_x")),
            root_cause,
        };
        let suggestions = engine.suggest(&div);
        prop_assert!(!suggestions.is_empty());
    }

    // ── RM-6: Info severity → NoActionNeeded ──────────────────────────

    #[test]
    fn rm6_info_no_action(_dummy in 0u8..1) {
        let engine = RemediationEngine::new();
        // Shifted is always Info.
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Shifted,
            baseline_node: Some(make_node("r")),
            candidate_node: Some(make_node("r")),
            root_cause: RootCause::TimingShift {
                baseline_ms: 100,
                candidate_ms: 110,
                delta_ms: 10,
            },
        };
        let suggestions = engine.suggest(&div);
        prop_assert_eq!(suggestions[0].action, SuggestionAction::NoActionNeeded);
    }

    // ── RM-7: Timing shift → NoActionNeeded ───────────────────────────

    #[test]
    fn rm7_timing_no_action(delta in 1u64..1000) {
        let engine = RemediationEngine::new();
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Shifted,
            baseline_node: Some(make_node("r")),
            candidate_node: Some(make_node("r")),
            root_cause: RootCause::TimingShift {
                baseline_ms: 100,
                candidate_ms: 100 + delta,
                delta_ms: delta,
            },
        };
        let suggestions = engine.suggest(&div);
        let has_no_action = suggestions.iter().any(|s| s.action == SuggestionAction::NoActionNeeded);
        prop_assert!(has_no_action);
    }

    // ── RM-8: Rule def change → AddAnnotation ─────────────────────────

    #[test]
    fn rm8_rule_change_annotation(rule_id in "[a-z_]{3,10}") {
        let engine = RemediationEngine::new();
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Modified,
            baseline_node: Some(make_node(&rule_id)),
            candidate_node: Some(make_node(&rule_id)),
            root_cause: RootCause::RuleDefinitionChange {
                rule_id: rule_id.clone(),
                baseline_hash: "h1".into(),
                candidate_hash: "h2".into(),
            },
        };
        let suggestions = engine.suggest(&div);
        let has_annotation = suggestions.iter().any(|s| s.action == SuggestionAction::AddAnnotation);
        prop_assert!(has_annotation);
    }

    // ── RM-9: Input divergence → InvestigateUpstream ──────────────────

    #[test]
    fn rm9_input_investigate(upstream in "[a-z_]{3,10}") {
        let engine = RemediationEngine::new();
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Modified,
            baseline_node: Some(make_node("downstream")),
            candidate_node: Some(make_node("downstream")),
            root_cause: RootCause::InputDivergence {
                upstream_rule_id: upstream,
                upstream_position: 0,
            },
        };
        let suggestions = engine.suggest(&div);
        let has_investigate = suggestions.iter().any(|s| s.action == SuggestionAction::InvestigateUpstream);
        prop_assert!(has_investigate);
    }

    // ── RM-10: Confidence in [0, 1] ───────────────────────────────────

    #[test]
    fn rm10_confidence_bounded(root_cause in arb_root_cause()) {
        let engine = RemediationEngine::new();
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Modified,
            baseline_node: Some(make_node("r")),
            candidate_node: Some(make_node("r")),
            root_cause,
        };
        for s in engine.suggest(&div) {
            prop_assert!(s.confidence >= 0.0 && s.confidence <= 1.0,
                "confidence {} out of range", s.confidence);
        }
    }

    // ── RM-11: Target is non-empty ────────────────────────────────────

    #[test]
    fn rm11_target_nonempty(root_cause in arb_root_cause()) {
        let engine = RemediationEngine::new();
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Modified,
            baseline_node: Some(make_node("r")),
            candidate_node: Some(make_node("r")),
            root_cause,
        };
        for s in engine.suggest(&div) {
            prop_assert!(!s.target.is_empty());
        }
    }

    // ── RM-12: Rationale is non-empty ─────────────────────────────────

    #[test]
    fn rm12_rationale_nonempty(root_cause in arb_root_cause()) {
        let engine = RemediationEngine::new();
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Modified,
            baseline_node: Some(make_node("r")),
            candidate_node: Some(make_node("r")),
            root_cause,
        };
        for s in engine.suggest(&div) {
            prop_assert!(!s.rationale.is_empty());
        }
    }

    // ── RM-13: suggest_all returns one entry per divergence ───────────

    #[test]
    fn rm13_suggest_all_count(n in 1usize..8) {
        let engine = RemediationEngine::new();
        let divs: Vec<Divergence> = (0..n)
            .map(|i| Divergence {
                position: i as u64,
                divergence_type: DivergenceType::Modified,
                baseline_node: Some(make_node(&format!("r_{}", i))),
                candidate_node: Some(make_node(&format!("r_{}", i))),
                root_cause: RootCause::Unknown,
            })
            .collect();
        let results = engine.suggest_all(&divs);
        prop_assert_eq!(results.len(), n);
    }

    // ── RM-14: Cascade adds extra suggestion ──────────────────────────

    #[test]
    fn rm14_cascade_extra(_dummy in 0u8..1) {
        let engine = RemediationEngine::new();
        let divs = vec![
            Divergence {
                position: 0,
                divergence_type: DivergenceType::Modified,
                baseline_node: Some(make_node("root_r")),
                candidate_node: Some(make_node("root_r")),
                root_cause: RootCause::RuleDefinitionChange {
                    rule_id: "root_r".into(),
                    baseline_hash: "h1".into(),
                    candidate_hash: "h2".into(),
                },
            },
            Divergence {
                position: 1,
                divergence_type: DivergenceType::Modified,
                baseline_node: Some(make_node("child_r")),
                candidate_node: Some(make_node("child_r")),
                root_cause: RootCause::InputDivergence {
                    upstream_rule_id: "root_r".into(),
                    upstream_position: 0,
                },
            },
        ];
        let results = engine.suggest_all(&divs);
        let (_, root_suggestions) = &results[0];
        let has_cascade = root_suggestions.iter().any(|s| s.rationale.contains("downstream"));
        prop_assert!(has_cascade);
    }

    // ── RM-15: New decision → AddAnnotation ───────────────────────────

    #[test]
    fn rm15_new_decision(rule_id in "[a-z_]{3,10}") {
        let engine = RemediationEngine::new();
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Added,
            baseline_node: None,
            candidate_node: Some(make_node(&rule_id)),
            root_cause: RootCause::NewDecision { rule_id },
        };
        let suggestions = engine.suggest(&div);
        let has_annotation = suggestions.iter().any(|s| s.action == SuggestionAction::AddAnnotation);
        prop_assert!(has_annotation);
    }

    // ── RM-16: Dropped decision → InvestigateUpstream ─────────────────

    #[test]
    fn rm16_dropped_investigate(rule_id in "[a-z_]{3,10}") {
        let engine = RemediationEngine::new();
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Removed,
            baseline_node: Some(make_node(&rule_id)),
            candidate_node: None,
            root_cause: RootCause::DroppedDecision { rule_id },
        };
        let suggestions = engine.suggest(&div);
        let has_investigate = suggestions.iter().any(|s| s.action == SuggestionAction::InvestigateUpstream);
        prop_assert!(has_investigate);
    }
}
