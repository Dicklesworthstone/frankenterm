// Property-based tests for output/renderers module.
//
// Covers: serde roundtrips for all publicly exported Serialize/Deserialize types
// from the output module: RuleListItem, RuleTestMatch, RuleDetail,
// AnalyticsSummaryData, HealthDiagnosticStatus.
#![allow(clippy::ignored_unit_patterns)]

use proptest::prelude::*;

use frankenterm_core::output::{
    AnalyticsSummaryData, HealthDiagnosticStatus, RuleDetail, RuleListItem, RuleTestMatch,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_rule_list_item() -> impl Strategy<Value = RuleListItem> {
    (
        "[a-z_.]{5,20}",
        prop_oneof![
            Just("codex".to_string()),
            Just("claude_code".to_string()),
            Just("gemini".to_string()),
            Just("wezterm".to_string()),
        ],
        "[a-z_.]{5,20}",
        prop_oneof![
            Just("info".to_string()),
            Just("warning".to_string()),
            Just("critical".to_string()),
        ],
        "[a-z ]{10,40}",
        prop::option::of("[a-z_]{5,20}"),
        0usize..10,
        any::<bool>(),
    )
        .prop_map(
            |(id, agent_type, event_type, severity, description, workflow, anchor_count, has_regex)| {
                RuleListItem {
                    id,
                    agent_type,
                    event_type,
                    severity,
                    description,
                    workflow,
                    anchor_count,
                    has_regex,
                }
            },
        )
}

fn arb_rule_test_match() -> impl Strategy<Value = RuleTestMatch> {
    (
        "[a-z_.]{5,20}",
        prop_oneof![
            Just("codex".to_string()),
            Just("claude_code".to_string()),
            Just("gemini".to_string()),
        ],
        "[a-z_.]{5,20}",
        prop_oneof![
            Just("info".to_string()),
            Just("warning".to_string()),
            Just("critical".to_string()),
        ],
        0.0f64..=1.0f64,
        "[a-z ]{5,30}",
        prop::option::of(Just(serde_json::json!({"key": "value"}))),
    )
        .prop_map(
            |(rule_id, agent_type, event_type, severity, confidence, matched_text, extracted)| {
                RuleTestMatch {
                    rule_id,
                    agent_type,
                    event_type,
                    severity,
                    confidence,
                    matched_text,
                    extracted,
                }
            },
        )
}

fn arb_rule_detail() -> impl Strategy<Value = RuleDetail> {
    (
        "[a-z_.]{5,20}",
        prop_oneof![
            Just("codex".to_string()),
            Just("claude_code".to_string()),
        ],
        "[a-z_.]{5,20}",
        prop_oneof![Just("info".to_string()), Just("warning".to_string())],
        "[a-z ]{10,40}",
        prop::collection::vec("[a-z ]{5,20}", 0..5),
        prop::option::of("[a-z.]+"),
        prop::option::of("[a-z_]{5,20}"),
        prop::option::of("[a-z ]{10,40}"),
        prop::option::of("[a-z ]{10,40}"),
        prop::option::of("[a-z:/]{10,30}"),
    )
        .prop_map(
            |(
                id,
                agent_type,
                event_type,
                severity,
                description,
                anchors,
                regex,
                workflow,
                remediation,
                manual_fix,
                learn_more_url,
            )| {
                RuleDetail {
                    id,
                    agent_type,
                    event_type,
                    severity,
                    description,
                    anchors,
                    regex,
                    workflow,
                    remediation,
                    manual_fix,
                    learn_more_url,
                }
            },
        )
}

fn arb_health_diagnostic_status() -> impl Strategy<Value = HealthDiagnosticStatus> {
    prop_oneof![
        Just(HealthDiagnosticStatus::Ok),
        Just(HealthDiagnosticStatus::Info),
        Just(HealthDiagnosticStatus::Warning),
        Just(HealthDiagnosticStatus::Error),
    ]
}

fn arb_analytics_summary_data() -> impl Strategy<Value = AnalyticsSummaryData> {
    (
        "[a-z 0-9]{5,20}",
        0i64..1_000_000,
        0.0f64..10_000.0f64,
        0i64..1_000,
        0i64..10_000,
    )
        .prop_map(
            |(period_label, total_tokens, total_cost, rate_limit_hits, workflow_runs)| {
                AnalyticsSummaryData {
                    period_label,
                    total_tokens,
                    total_cost,
                    rate_limit_hits,
                    workflow_runs,
                }
            },
        )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn rule_list_item_serde_roundtrip(val in arb_rule_list_item()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: RuleListItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val.id, &back.id);
        prop_assert_eq!(&val.agent_type, &back.agent_type);
        prop_assert_eq!(&val.event_type, &back.event_type);
        prop_assert_eq!(&val.severity, &back.severity);
        prop_assert_eq!(&val.description, &back.description);
        prop_assert_eq!(&val.workflow, &back.workflow);
        prop_assert_eq!(val.anchor_count, back.anchor_count);
        prop_assert_eq!(val.has_regex, back.has_regex);
    }

    #[test]
    fn rule_test_match_serde_roundtrip(val in arb_rule_test_match()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: RuleTestMatch = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val.rule_id, &back.rule_id);
        prop_assert_eq!(&val.agent_type, &back.agent_type);
        prop_assert_eq!(&val.matched_text, &back.matched_text);
        // f64 roundtrip: check within ULP tolerance
        prop_assert!((val.confidence - back.confidence).abs() < 1e-10);
    }

    #[test]
    fn rule_detail_serde_roundtrip(val in arb_rule_detail()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: RuleDetail = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val.id, &back.id);
        prop_assert_eq!(&val.agent_type, &back.agent_type);
        prop_assert_eq!(&val.description, &back.description);
        prop_assert_eq!(&val.anchors, &back.anchors);
        prop_assert_eq!(&val.regex, &back.regex);
        prop_assert_eq!(&val.workflow, &back.workflow);
    }

    #[test]
    fn health_diagnostic_status_serde_roundtrip(val in arb_health_diagnostic_status()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: HealthDiagnosticStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val, back);
    }

    #[test]
    fn analytics_summary_data_serde_roundtrip(val in arb_analytics_summary_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: AnalyticsSummaryData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val.period_label, &back.period_label);
        prop_assert_eq!(val.total_tokens, back.total_tokens);
        prop_assert_eq!(val.rate_limit_hits, back.rate_limit_hits);
        prop_assert_eq!(val.workflow_runs, back.workflow_runs);
        prop_assert!((val.total_cost - back.total_cost).abs() < 1e-10);
    }
}

// =============================================================================
// Structural invariant tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn health_diagnostic_status_as_str_nonempty(val in arb_health_diagnostic_status()) {
        prop_assert!(!val.as_str().is_empty());
    }

    #[test]
    fn health_diagnostic_status_as_str_matches_serde(val in arb_health_diagnostic_status()) {
        let json = serde_json::to_string(&val).unwrap();
        // serde uses rename_all = "snake_case", and as_str returns lowercase
        let expected = format!("\"{}\"", val.as_str());
        prop_assert_eq!(json, expected);
    }

    #[test]
    fn rule_list_item_has_nonempty_id(val in arb_rule_list_item()) {
        prop_assert!(!val.id.is_empty());
        prop_assert!(!val.agent_type.is_empty());
        prop_assert!(!val.event_type.is_empty());
    }

    #[test]
    fn rule_test_match_confidence_in_range(val in arb_rule_test_match()) {
        prop_assert!(val.confidence >= 0.0);
        prop_assert!(val.confidence <= 1.0);
    }

    #[test]
    fn rule_detail_has_nonempty_id(val in arb_rule_detail()) {
        prop_assert!(!val.id.is_empty());
        prop_assert!(!val.agent_type.is_empty());
        prop_assert!(!val.description.is_empty());
    }

    #[test]
    fn analytics_summary_data_tokens_nonnegative(val in arb_analytics_summary_data()) {
        prop_assert!(val.total_tokens >= 0);
        prop_assert!(val.total_cost >= 0.0);
        prop_assert!(val.rate_limit_hits >= 0);
        prop_assert!(val.workflow_runs >= 0);
    }
}
