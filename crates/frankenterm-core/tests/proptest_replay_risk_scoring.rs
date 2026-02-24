//! Property-based tests for replay_risk_scoring (ft-og6q6.5.3).
//!
//! Invariants tested:
//! - RS-1: Severity ordering: Info < Low < Medium < High < Critical
//! - RS-2: Severity scores are monotonically increasing
//! - RS-3: DivergenceSeverity serde roundtrip
//! - RS-4: Recommendation serde roundtrip
//! - RS-5: RiskScore serde roundtrip
//! - RS-6: AggregateRisk serde roundtrip
//! - RS-7: Empty scores → Pass recommendation
//! - RS-8: Any Critical → Block
//! - RS-9: Any High → Block
//! - RS-10: Medium only → Review
//! - RS-11: Info/Low only → Pass
//! - RS-12: max_severity is max of individual severities
//! - RS-13: total_risk_score is sum of individual scores
//! - RS-14: Count invariant: critical+high+medium+low+info = total scores
//! - RS-15: Shifted divergences always classify as Info
//! - RS-16: Custom rules override default classification
//! - RS-17: Confidence is in [0, 1]
//! - RS-18: SeverityConfig from TOML roundtrip

use proptest::prelude::*;

use frankenterm_core::replay_decision_diff::{
    Divergence, DivergenceNode, DivergenceType, RootCause,
};
use frankenterm_core::replay_risk_scoring::{
    AggregateRisk, DivergenceSeverity, Recommendation, RiskScore, RiskScorer, SeverityConfig,
    SeverityRule,
};

// ── Strategies ────────────────────────────────────────────────────────────

fn arb_severity() -> impl Strategy<Value = DivergenceSeverity> {
    prop_oneof![
        Just(DivergenceSeverity::Info),
        Just(DivergenceSeverity::Low),
        Just(DivergenceSeverity::Medium),
        Just(DivergenceSeverity::High),
        Just(DivergenceSeverity::Critical),
    ]
}

fn arb_recommendation() -> impl Strategy<Value = Recommendation> {
    prop_oneof![
        Just(Recommendation::Pass),
        Just(Recommendation::Review),
        Just(Recommendation::Block),
    ]
}

fn arb_risk_score() -> impl Strategy<Value = RiskScore> {
    (arb_severity(), 0u64..100, 0.0f64..1.0).prop_map(|(sev, ir, conf)| RiskScore {
        severity: sev,
        impact_radius: ir,
        confidence: conf,
        explanation: "test".into(),
    })
}

fn make_shifted_divergence() -> Divergence {
    Divergence {
        position: 0,
        divergence_type: DivergenceType::Shifted,
        baseline_node: Some(DivergenceNode {
            node_id: 0,
            rule_id: "r1".into(),
            definition_hash: "d".into(),
            output_hash: "o".into(),
            timestamp_ms: 100,
            pane_id: 1,
        }),
        candidate_node: Some(DivergenceNode {
            node_id: 0,
            rule_id: "r1".into(),
            definition_hash: "d".into(),
            output_hash: "o".into(),
            timestamp_ms: 150,
            pane_id: 1,
        }),
        root_cause: RootCause::TimingShift {
            baseline_ms: 100,
            candidate_ms: 150,
            delta_ms: 50,
        },
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // ── RS-1: Severity ordering ────────────────────────────────────────

    #[test]
    fn rs1_severity_ordering(_dummy in 0u8..1) {
        prop_assert!(DivergenceSeverity::Info < DivergenceSeverity::Low);
        prop_assert!(DivergenceSeverity::Low < DivergenceSeverity::Medium);
        prop_assert!(DivergenceSeverity::Medium < DivergenceSeverity::High);
        prop_assert!(DivergenceSeverity::High < DivergenceSeverity::Critical);
    }

    // ── RS-2: Severity scores monotonic ────────────────────────────────

    #[test]
    fn rs2_scores_monotonic(_dummy in 0u8..1) {
        let severities = [
            DivergenceSeverity::Info,
            DivergenceSeverity::Low,
            DivergenceSeverity::Medium,
            DivergenceSeverity::High,
            DivergenceSeverity::Critical,
        ];
        for window in severities.windows(2) {
            prop_assert!(window[0].score() < window[1].score());
        }
    }

    // ── RS-3: Severity serde ───────────────────────────────────────────

    #[test]
    fn rs3_severity_serde(sev in arb_severity()) {
        let json = serde_json::to_string(&sev).unwrap();
        let restored: DivergenceSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, sev);
    }

    // ── RS-4: Recommendation serde ─────────────────────────────────────

    #[test]
    fn rs4_recommendation_serde(rec in arb_recommendation()) {
        let json = serde_json::to_string(&rec).unwrap();
        let restored: Recommendation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, rec);
    }

    // ── RS-5: RiskScore serde ──────────────────────────────────────────

    #[test]
    fn rs5_risk_score_serde(rs in arb_risk_score()) {
        let json = serde_json::to_string(&rs).unwrap();
        let restored: RiskScore = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.severity, rs.severity);
        prop_assert_eq!(restored.impact_radius, rs.impact_radius);
    }

    // ── RS-6: AggregateRisk serde ──────────────────────────────────────

    #[test]
    fn rs6_aggregate_serde(scores in prop::collection::vec(arb_risk_score(), 0..10)) {
        let agg = AggregateRisk::from_scores(&scores);
        let json = serde_json::to_string(&agg).unwrap();
        let restored: AggregateRisk = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.recommendation, agg.recommendation);
        prop_assert_eq!(restored.total_risk_score, agg.total_risk_score);
    }

    // ── RS-7: Empty → Pass ─────────────────────────────────────────────

    #[test]
    fn rs7_empty_pass(_dummy in 0u8..1) {
        let agg = AggregateRisk::from_scores(&[]);
        prop_assert_eq!(agg.recommendation, Recommendation::Pass);
    }

    // ── RS-8: Any Critical → Block ─────────────────────────────────────

    #[test]
    fn rs8_critical_blocks(n in 1usize..5) {
        let mut scores: Vec<RiskScore> = (0..n)
            .map(|_| RiskScore {
                severity: DivergenceSeverity::Info,
                impact_radius: 0,
                confidence: 1.0,
                explanation: String::new(),
            })
            .collect();
        scores.push(RiskScore {
            severity: DivergenceSeverity::Critical,
            impact_radius: 0,
            confidence: 1.0,
            explanation: String::new(),
        });
        let agg = AggregateRisk::from_scores(&scores);
        prop_assert_eq!(agg.recommendation, Recommendation::Block);
    }

    // ── RS-9: Any High → Block ─────────────────────────────────────────

    #[test]
    fn rs9_high_blocks(n in 1usize..5) {
        let mut scores: Vec<RiskScore> = (0..n)
            .map(|_| RiskScore {
                severity: DivergenceSeverity::Low,
                impact_radius: 0,
                confidence: 1.0,
                explanation: String::new(),
            })
            .collect();
        scores.push(RiskScore {
            severity: DivergenceSeverity::High,
            impact_radius: 0,
            confidence: 1.0,
            explanation: String::new(),
        });
        let agg = AggregateRisk::from_scores(&scores);
        prop_assert_eq!(agg.recommendation, Recommendation::Block);
    }

    // ── RS-10: Medium only → Review ────────────────────────────────────

    #[test]
    fn rs10_medium_review(n in 1usize..5) {
        let scores: Vec<RiskScore> = (0..n)
            .map(|_| RiskScore {
                severity: DivergenceSeverity::Medium,
                impact_radius: 0,
                confidence: 1.0,
                explanation: String::new(),
            })
            .collect();
        let agg = AggregateRisk::from_scores(&scores);
        prop_assert_eq!(agg.recommendation, Recommendation::Review);
    }

    // ── RS-11: Info/Low only → Pass ────────────────────────────────────

    #[test]
    fn rs11_info_low_pass(n_info in 0usize..5, n_low in 0usize..5) {
        let total = n_info + n_low;
        prop_assume!(total > 0);
        let mut scores = Vec::new();
        for _ in 0..n_info {
            scores.push(RiskScore { severity: DivergenceSeverity::Info, impact_radius: 0, confidence: 1.0, explanation: String::new() });
        }
        for _ in 0..n_low {
            scores.push(RiskScore { severity: DivergenceSeverity::Low, impact_radius: 0, confidence: 1.0, explanation: String::new() });
        }
        let agg = AggregateRisk::from_scores(&scores);
        prop_assert_eq!(agg.recommendation, Recommendation::Pass);
    }

    // ── RS-12: max_severity is max of individuals ──────────────────────

    #[test]
    fn rs12_max_severity(scores in prop::collection::vec(arb_risk_score(), 1..10)) {
        let expected_max = scores.iter().map(|s| s.severity).max().unwrap();
        let agg = AggregateRisk::from_scores(&scores);
        prop_assert_eq!(agg.max_severity, expected_max);
    }

    // ── RS-13: total_risk_score is sum ─────────────────────────────────

    #[test]
    fn rs13_total_score_sum(scores in prop::collection::vec(arb_risk_score(), 0..10)) {
        let expected_sum: u64 = scores.iter().map(|s| s.severity.score() as u64).sum();
        let agg = AggregateRisk::from_scores(&scores);
        prop_assert_eq!(agg.total_risk_score, expected_sum);
    }

    // ── RS-14: Count invariant ─────────────────────────────────────────

    #[test]
    fn rs14_count_invariant(scores in prop::collection::vec(arb_risk_score(), 0..10)) {
        let total = scores.len() as u64;
        let agg = AggregateRisk::from_scores(&scores);
        let sum = agg.critical_count + agg.high_count + agg.medium_count + agg.low_count + agg.info_count;
        prop_assert_eq!(sum, total);
    }

    // ── RS-15: Shifted always Info ─────────────────────────────────────

    #[test]
    fn rs15_shifted_info(_dummy in 0u8..1) {
        let scorer = RiskScorer::new();
        let div = make_shifted_divergence();
        let score = scorer.score(&div, 0);
        prop_assert_eq!(score.severity, DivergenceSeverity::Info);
    }

    // ── RS-16: Custom rules override defaults ──────────────────────────

    #[test]
    fn rs16_custom_overrides(sev in arb_severity()) {
        let config = SeverityConfig {
            rules: vec![SeverityRule {
                decision_type: None,
                rule_id_pattern: Some("custom_*".into()),
                severity: sev,
            }],
        };
        let scorer = RiskScorer::with_config(config);
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Modified,
            baseline_node: Some(DivergenceNode {
                node_id: 0,
                rule_id: "custom_rule".into(),
                definition_hash: "d1".into(),
                output_hash: "o1".into(),
                timestamp_ms: 100,
                pane_id: 1,
            }),
            candidate_node: Some(DivergenceNode {
                node_id: 0,
                rule_id: "custom_rule".into(),
                definition_hash: "d2".into(),
                output_hash: "o2".into(),
                timestamp_ms: 100,
                pane_id: 1,
            }),
            root_cause: RootCause::RuleDefinitionChange {
                rule_id: "custom_rule".into(),
                baseline_hash: "d1".into(),
                candidate_hash: "d2".into(),
            },
        };
        let score = scorer.score(&div, 0);
        prop_assert_eq!(score.severity, sev);
    }

    // ── RS-17: Confidence in [0, 1] ────────────────────────────────────

    #[test]
    fn rs17_confidence_bounded(_dummy in 0u8..1) {
        let scorer = RiskScorer::new();
        let divergences = vec![
            make_shifted_divergence(),
            Divergence {
                position: 1,
                divergence_type: DivergenceType::Added,
                baseline_node: None,
                candidate_node: Some(DivergenceNode {
                    node_id: 1,
                    rule_id: "rule_a".into(),
                    definition_hash: "d".into(),
                    output_hash: "o".into(),
                    timestamp_ms: 200,
                    pane_id: 1,
                }),
                root_cause: RootCause::NewDecision { rule_id: "rule_a".into() },
            },
        ];
        for div in &divergences {
            let score = scorer.score(div, 0);
            prop_assert!(score.confidence >= 0.0 && score.confidence <= 1.0);
        }
    }

    // ── RS-18: SeverityConfig TOML roundtrip ───────────────────────────

    #[test]
    fn rs18_config_toml_roundtrip(sev in arb_severity()) {
        let config = SeverityConfig {
            rules: vec![SeverityRule {
                decision_type: None,
                rule_id_pattern: Some("test_*".into()),
                severity: sev,
            }],
        };
        let toml_str = toml::to_string(&config).unwrap();
        let restored = SeverityConfig::from_toml(&toml_str).unwrap();
        prop_assert_eq!(restored.rules.len(), 1);
        prop_assert_eq!(restored.rules[0].severity, sev);
    }
}
